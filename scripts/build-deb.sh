#!/usr/bin/env bash
# Builds the Linux package: the frontend, the three release executables, then
# a .deb laid out from packaging/linux.
#
#   scripts/build-deb.sh                 # target/dist/own-backup_<version>_amd64.deb
#   scripts/build-deb.sh --skip-build    # package binaries already in target/release
#
# Build on the OLDEST supported system — Ubuntu 24.04 (plan §6, decision 2):
# the binaries take the glibc and WebKitGTK versions they are linked against
# as their minimum, and dpkg-shlibdeps writes exactly those into Depends.
# Debian 13 and newer Ubuntu then install the same package.
#
# Build dependencies (once):
#   sudo apt install build-essential pkg-config protobuf-compiler nodejs npm \
#       libwebkit2gtk-4.1-dev libsoup-3.0-dev libayatana-appindicator3-dev \
#       librsvg2-dev libgtk-layer-shell-dev dpkg-dev desktop-file-utils lintian
#
# OUTPUT names another file for the result. Nothing here talks to GitHub
# Actions: Linux packages are built locally (AGENTS.md).
set -euo pipefail

root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$root"

skip_build=0
if [[ "${1:-}" == "--skip-build" ]]; then
    skip_build=1
fi

for tool in dpkg-deb dpkg-shlibdeps python3 npm cargo strip; do
    if ! command -v "$tool" >/dev/null; then
        echo "$tool not found; see the build dependencies at the top of $0" >&2
        exit 1
    fi
done

arch="$(dpkg --print-architecture)"
if [[ "$arch" != amd64 ]]; then
    echo "only amd64 is supported (this machine: $arch)" >&2
    exit 1
fi

version="$(sed -n 's/^version[[:space:]]*=[[:space:]]*"\(.*\)"/\1/p' Cargo.toml | head -1)"
if [[ -z "$version" ]]; then
    echo 'cannot read the version from Cargo.toml' >&2
    exit 1
fi
# Debian orders "0.1.5~beta" before "0.1.5", as semver orders "0.1.5-beta";
# a hyphen would be read as the Debian revision. "-1" is the package revision
# of a local test build; DEB_REVISION gives another one (DEB_REVISION=2
# upgrades an installed -1). DEB_REVISION=none is a release: no revision
# (own-backup_0.1.8~beta), and the release's medium is named after it, so the
# installed program finds it (media_linux::release_asset_name). Test builds of
# the same version, -1 and up, sort after the release and install over it.
revision="${DEB_REVISION:-1}"
if [[ "$revision" == none ]]; then
    deb_version="$(printf '%s' "$version" | sed 's/-/~/')"
elif [[ "$revision" =~ ^[0-9][0-9A-Za-z.+~]*$ ]]; then
    deb_version="$(printf '%s' "$version" | sed 's/-/~/')-$revision"
else
    echo "DEB_REVISION must be none or start with a digit and use only 0-9A-Za-z.+~: $revision" >&2
    exit 1
fi

target_dir="${CARGO_TARGET_DIR:-$root/target}"
bin_dir="$target_dir/release"
pkg="$root/packaging/linux"

if [[ "$skip_build" -eq 0 ]]; then
    # The window's pages are compiled into own-backup-gui, so they come first.
    (
        cd crates/gui/frontend
        [[ -d node_modules ]] || "$root/scripts/npm-ci.sh"
        npm run build
    )
    cargo build --locked --release -p own-backup-gui -p own-backup-service -p own-backup-cli \
        -p own-backup-shell
fi

for name in own-backup-gui own-backup-service own-backup-cli own-backup-shell; do
    if [[ ! -x "$bin_dir/$name" ]]; then
        echo "executable not found: $bin_dir/$name" >&2
        exit 1
    fi
done

stage="$target_dir/deb/own-backup_${deb_version}_$arch"
rm -rf "$stage"
mkdir -p "$stage/DEBIAN"

# --- programs -----------------------------------------------------------------
# The window and the CLI are what a person runs; the engine is started by
# systemd only, so it lives in libexec, off PATH.
install -D -m755 "$bin_dir/own-backup-gui" "$stage/usr/bin/own-backup-gui"
install -D -m755 "$bin_dir/own-backup-cli" "$stage/usr/bin/own-backup-cli"
install -D -m755 "$bin_dir/own-backup-service" "$stage/usr/libexec/own-backup/own-backup-service"
# The boot medium's strip (labwc's autostart there runs it); an installed
# system never starts it. Its one library of its own, libgtk-layer-shell0, is
# small and in both Ubuntu 24.04 and Debian 13, so it is an honest Depends
# rather than a second package.
install -D -m755 "$bin_dir/own-backup-shell" "$stage/usr/libexec/own-backup/own-backup-shell"
# Debian ships stripped binaries; the symbols would triple the package.
strip --strip-unneeded --remove-section=.comment --remove-section=.note \
    "$stage/usr/bin/own-backup-gui" "$stage/usr/bin/own-backup-cli" \
    "$stage/usr/libexec/own-backup/own-backup-service" \
    "$stage/usr/libexec/own-backup/own-backup-shell"

# --- the boot medium's builder ---------------------------------------------------
# The window builds the Linux boot medium through the service (Tools → boot
# medium → Linux): the shared build script and the live-build configuration it
# copies, exactly as scripts/build-linux-media.sh uses them from the checkout.
install -D -m755 "$pkg/media/build-media.sh" "$stage/usr/libexec/own-backup/build-linux-media"
install -d -m755 "$stage/usr/share/own-backup/media"
cp -r --no-preserve=ownership "$pkg/media/auto" "$pkg/media/config" "$stage/usr/share/own-backup/media/"
# Modes as committed (the hooks and auto/ scripts are executable), nothing
# writable by others whatever the checkout's umask was.
chmod -R go-w "$stage/usr/share/own-backup"

# --- system integration -------------------------------------------------------
install -D -m644 "$pkg/own-backup.service" "$stage/usr/lib/systemd/system/own-backup.service"
install -D -m644 "$pkg/own-backup.sysusers" "$stage/usr/lib/sysusers.d/own-backup.conf"
install -D -m644 "$pkg/own-backup.tmpfiles" "$stage/usr/lib/tmpfiles.d/own-backup.conf"
install -D -m644 "$pkg/com.ownbackup.policy" "$stage/usr/share/polkit-1/actions/com.ownbackup.policy"
install -D -m644 "$pkg/own-backup.desktop" "$stage/usr/share/applications/own-backup.desktop"
install -D -m644 "$pkg/own-backup-tray.desktop" "$stage/etc/xdg/autostart/own-backup-tray.desktop"
for size in 32x32:32x32.png 128x128:128x128.png 256x256:128x128@2x.png 512x512:icon.png; do
    install -D -m644 "crates/gui/icons/${size#*:}" \
        "$stage/usr/share/icons/hicolor/${size%%:*}/apps/own-backup.png"
done

# --- documents ------------------------------------------------------------------
# The licence, the notices and the full texts of every third-party component
# the binaries carry (scripts/third-party-licenses.py, Linux target). A
# component without a text stops the build rather than shipping a gap.
doc="$stage/usr/share/doc/own-backup"
install -d -m755 "$doc"
install -m644 LICENSE "$doc/LICENSE"
install -m644 NOTICE "$doc/NOTICE"
install -m644 COMMERCIAL-LICENSE.md "$doc/COMMERCIAL-LICENSE.md"
install -m644 THIRD-PARTY-NOTICES.md "$doc/THIRD-PARTY-NOTICES.md"
python3 scripts/third-party-licenses.py --target linux "$target_dir/THIRD-PARTY-LICENSES-linux.txt"
gzip -9n -c "$target_dir/THIRD-PARTY-LICENSES-linux.txt" > "$doc/THIRD-PARTY-LICENSES.txt.gz"
# Debian's machine-readable copyright file names the licence; the text is in
# LICENSE beside it, as it is not one of /usr/share/common-licenses. The holder
# is the one NOTICE requires to be named.
holder="$(sed -n 's/^Required Notice: Copyright (c) //p' NOTICE | head -1)"
if [[ -z "$holder" ]]; then
    echo 'NOTICE has no "Required Notice: Copyright (c) ..." line' >&2
    exit 1
fi
cat > "$doc/copyright" <<EOF
Format: https://www.debian.org/doc/packaging-manuals/copyright-format/1.0/
Upstream-Name: Own Backup
Source: https://github.com/coinman-dev/own-backup

Files: *
Copyright: $holder
License: PolyForm-Noncommercial-1.0.0
 Source-available, not open source: commercial use needs a separate licence
 (COMMERCIAL-LICENSE.md). The full text is in /usr/share/doc/own-backup/LICENSE.
 Third-party components and their licences: THIRD-PARTY-NOTICES.md and
 THIRD-PARTY-LICENSES.txt.gz in the same folder. System libraries the
 programs use (glibc, GTK, WebKitGTK and others) are not part of this package;
 they come from the distribution under their own licences.
EOF
changelog_date="$(date -R -d "@${SOURCE_DATE_EPOCH:-$(git log -1 --format=%ct)}")"
{
    echo "own-backup ($deb_version) unstable; urgency=medium"
    echo
    echo "  * Release $version. See CHANGELOG.md in the source for the changes."
    echo
    echo " -- $holder  $changelog_date"
} | gzip -9n > "$doc/changelog.Debian.gz"

# --- control --------------------------------------------------------------------
install -m755 "$pkg/debian/postinst" "$pkg/debian/prerm" "$pkg/debian/postrm" "$stage/DEBIAN/"
# Files under /etc are the administrator's once installed: an edited
# autostart entry survives an upgrade.
find "$stage/etc" -type f -printf '/etc/%P\n' | sort > "$stage/DEBIAN/conffiles"

# Shared-library dependencies as linked: libwebkit2gtk-4.1-0, libgtk-3-0,
# libc6 with the glibc version of THIS system, and so on. dpkg-shlibdeps
# wants a debian/control to sit in; a minimal one is made beside the stage.
shlib_dir="$target_dir/deb/shlibdeps"
rm -rf "$shlib_dir"
mkdir -p "$shlib_dir/debian"
printf 'Source: own-backup\n\nPackage: own-backup\nArchitecture: %s\n' "$arch" > "$shlib_dir/debian/control"
shlibs="$(
    cd "$shlib_dir"
    dpkg-shlibdeps -O -e"$stage/usr/bin/own-backup-gui" -e"$stage/usr/bin/own-backup-cli" \
        -e"$stage/usr/libexec/own-backup/own-backup-service" \
        -e"$stage/usr/libexec/own-backup/own-backup-shell" 2>/dev/null |
        sed -n 's/^shlibs:Depends=//p'
)"
if [[ -z "$shlibs" ]]; then
    echo 'dpkg-shlibdeps found no library dependencies' >&2
    exit 1
fi

installed_size="$(du -sk --exclude=DEBIAN "$stage" | cut -f1)"
# libtss2-rc: without it systemd-creds sees the TPM only "partially" and
# seals the machine key with the host key alone (stand, 20.10).
cat > "$stage/DEBIAN/control" <<EOF
Package: own-backup
Version: $deb_version
Architecture: $arch
Maintainer: $holder
Installed-Size: $installed_size
Depends: $shlibs, libayatana-appindicator3-1, systemd, dbus, polkitd | policykit-1, mount, kmod, curl
Recommends: lvm2, cryptsetup-bin, pkexec, ntfs-3g, libtss2-rc0t64 | libtss2-rc0,
 debootstrap, debian-archive-keyring
Section: admin
Priority: optional
Homepage: https://github.com/coinman-dev/own-backup
Description: block-level backup of disks and partitions
 Own Backup copies whole disks and partitions into VHDX, qcow2 or encrypted
 OBK images, on a schedule or on demand, and restores them. A service
 (systemd unit own-backup) does the work as root; the window and the CLI
 talk to it over a local socket from an active local session.
 .
 Source-available under the PolyForm Noncommercial License 1.0.0; commercial
 use needs a separate licence. It is not open-source software.
EOF
# debootstrap and debian-archive-keyring: building the Linux boot medium
# (both are in Ubuntu's universe and in Debian main); without them the build is
# refused with the apt command that installs them.
# libayatana-appindicator3-1 is loaded with dlopen by the window's tray
# support, so dpkg-shlibdeps cannot see it.

(
    cd "$stage"
    find . -type f ! -path './DEBIAN/*' -printf '%P\0' | sort -z | xargs -0 md5sum > DEBIAN/md5sums
)
chmod 644 "$stage/DEBIAN/control" "$stage/DEBIAN/conffiles" "$stage/DEBIAN/md5sums"

output="${OUTPUT:-$target_dir/dist/own-backup_${deb_version}_$arch.deb}"
mkdir -p "$(dirname -- "$output")"
# root:root ownership inside the archive without building as root.
dpkg-deb --build -Zxz --root-owner-group "$stage" "$output" >/dev/null

echo "packed from $bin_dir:"
for name in own-backup-gui own-backup-service own-backup-cli; do
    printf '  %s  %s\n' "$(date -r "$bin_dir/$name" '+%Y-%m-%d %H:%M')" "$name"
done
echo "package $deb_version: $output"
ls -l "$output"
if command -v lintian >/dev/null; then
    # Reported, not fatal. Known and accepted: no-manual-page (none written
    # yet) and initial-upload-closes-no-bugs (not in the Debian archive).
    # Anything else should be read.
    lintian --tag-display-limit 0 "$output" || true
fi
(
    cd "$(dirname -- "$output")"
    sha256sum "$(basename -- "$output")" > SHA256SUMS-deb.txt
    cat SHA256SUMS-deb.txt
)
