#!/usr/bin/env bash
# Builds the Linux boot medium: a Debian 13 "trixie" live ISO with the engine
# and the window in a cage kiosk (packaging/linux/media, PLAN §6 stage 6,
# decision 11). UEFI with Secure Boot through Debian's signed shim/GRUB/kernel,
# BIOS through ISOLINUX, a USB stick through the isohybrid MBR.
#
#   scripts/build-linux-media.sh                  # builds the .deb first
#   scripts/build-linux-media.sh --deb PATH.deb   # uses that package
#   scripts/build-linux-media.sh --clean          # drops the build tree (keeps caches)
#
# Result: target/dist/own-backup-media_<version>_amd64.iso and its .sha256
# (OUTPUT names another file). Write it onto a stick from the window (Tools →
# boot medium → Linux) or `own-backup-cli media write-stick`; boot it in the
# stand with scripts/lab/linux-media-scenario.sh.
#
# How: the build runs inside a minimal Debian trixie tree made by debootstrap
# (target/linux-media/builder), with Debian's own live-build. The host's
# live-build is not used: Ubuntu ships an old fork (3.0~a57) that cannot build
# a trixie medium with Secure Boot. The tree and the package cache are kept
# between runs, so a rebuild after changing only the program downloads
# nothing but updates.
#
# Needs: root through sudo (debootstrap, chroot, mounts), debootstrap and
# debian-archive-keyring on the host (`sudo apt install debootstrap
# debian-archive-keyring`), network to the Debian mirror, and the space checked
# below (AGENTS.md: at least 30 GiB left free after the build's peak).
#
# The package must install on trixie: binaries linked against a newer glibc
# than Debian 13's (a .deb built on Ubuntu 26.04) are refused before the long
# part starts, with apt's own reason. Build the .deb on the Ubuntu 24.04
# baseline (plan §6, decision 2) — the stand's Ubuntu 24.04 guest, for one.
#
# Mirror: OBK_MEDIA_MIRROR (default http://deb.debian.org/debian/) and
# OBK_MEDIA_SECURITY_MIRROR. Nothing here talks to GitHub Actions.
set -euo pipefail

root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$root"

deb=""
clean=0
while [[ $# -gt 0 ]]; do
    case "$1" in
        --deb) deb="$(realpath "$2")"; shift 2 ;;
        --clean) clean=1; shift ;;
        -h|--help) sed -n '2,36p' "$0"; exit 0 ;;
        *) echo "unknown argument: $1 (see --help)" >&2; exit 2 ;;
    esac
done

target_dir="${CARGO_TARGET_DIR:-$root/target}"
work="$target_dir/linux-media"
builder="$work/builder"
mirror="${OBK_MEDIA_MIRROR:-http://deb.debian.org/debian/}"
security="${OBK_MEDIA_SECURITY_MIRROR:-http://security.debian.org/debian-security/}"
suite=trixie

sudo_() {
    if [[ "$(id -u)" -eq 0 ]]; then "$@"; else sudo "$@"; fi
}

say() {
    echo "[linux-media] $*"
}

# --- the mounts of the builder tree, always undone ----------------------------
mounted=()
unmount_all() {
    local i
    for ((i = ${#mounted[@]} - 1; i >= 0; i--)); do
        sudo_ umount -l "${mounted[$i]}" 2>/dev/null || true
    done
    mounted=()
}
trap unmount_all EXIT INT TERM

bind() {
    sudo_ mkdir -p "$2"
    sudo_ mount --bind "$1" "$2"
    mounted+=("$2")
}

if [[ "$clean" -eq 1 ]]; then
    if [[ -d "$builder/build" ]]; then
        say "removing the build tree (the builder and the package cache stay)"
        sudo_ rm -rf --one-file-system "$builder/build"
    fi
    exit 0
fi

for tool in debootstrap realpath df dpkg-deb; do
    if ! command -v "$tool" >/dev/null; then
        echo "$tool not found; see the requirements at the top of $0" >&2
        exit 1
    fi
done
keyring=/usr/share/keyrings/debian-archive-keyring.gpg
if [[ ! -r "$keyring" ]]; then
    echo "$keyring not found: sudo apt install debian-archive-keyring" >&2
    exit 1
fi
if [[ "$(dpkg --print-architecture)" != amd64 ]]; then
    echo "only amd64 is supported" >&2
    exit 1
fi

# --- space (AGENTS.md "Stand resource limits") ---------------------------------
# Peak: the builder (~0.5 GiB), live-build's chroot and binary trees (~4 GiB),
# the package cache (~1.5 GiB), the ISO twice (~2.5 GiB) — 10 GiB is generous.
mkdir -p "$work"
peak_gib=10
free_gib=$(( $(df --output=avail -B1 "$work" | tail -1) >> 30 ))
if (( free_gib - peak_gib < 30 )); then
    echo "only ${free_gib} GiB free on $(df --output=target "$work" | tail -1); the build needs" \
        "${peak_gib} GiB and 30 GiB must stay free (AGENTS.md)" >&2
    exit 1
fi
say "space: ${free_gib} GiB free, peak ${peak_gib} GiB"

# --- our package ------------------------------------------------------------------
if [[ -z "$deb" ]]; then
    say "building the package (scripts/build-deb.sh)"
    out_deb="$target_dir/dist/own-backup-media-input.deb"
    OUTPUT="$out_deb" scripts/build-deb.sh
    deb="$out_deb"
fi
if [[ ! -f "$deb" ]]; then
    echo "package not found: $deb" >&2
    exit 1
fi
pkg_version="$(dpkg-deb -f "$deb" Version)"
say "package: $(basename "$deb") ($pkg_version)"

# --- the builder: Debian trixie with Debian's live-build --------------------------
if [[ ! -x "$builder/usr/bin/lb" ]]; then
    say "making the builder (debootstrap $suite from $mirror)"
    sudo_ rm -rf --one-file-system "$builder"
    sudo_ debootstrap --variant=minbase --keyring="$keyring" \
        --include=live-build,debootstrap,debian-archive-keyring,xorriso,squashfs-tools,mtools,dosfstools,ca-certificates,cpio,wget \
        "$suite" "$builder" "$mirror"
fi
sudo_ tee "$builder/etc/apt/sources.list" >/dev/null <<EOF
deb $mirror $suite main non-free-firmware
deb $mirror $suite-updates main non-free-firmware
deb $security $suite-security main non-free-firmware
EOF
for fs in proc sys dev dev/pts; do
    bind "/$fs" "$builder/$fs"
done
sudo_ cp /etc/resolv.conf "$builder/etc/resolv.conf"
in_builder() {
    sudo_ chroot "$builder" /usr/bin/env -i PATH=/usr/sbin:/usr/bin:/sbin:/bin \
        LANG=C.UTF-8 HOME=/root OBK_MEDIA_MIRROR="$mirror" \
        OBK_MEDIA_SECURITY_MIRROR="$security" "$@"
}
say "updating the builder"
in_builder apt-get -qq update
in_builder apt-get -qq -y full-upgrade
# shellcheck disable=SC2016 # ${Version} is dpkg-query's, not the shell's
say "live-build $(in_builder dpkg-query -W -f='${Version}' live-build)"

# --- does our package install on trixie? ------------------------------------------
sudo_ install -D -m644 "$deb" "$builder/tmp/own-backup.deb"
if ! in_builder apt-get -s -qq install /tmp/own-backup.deb >"$work/apt-check.log" 2>&1; then
    cat "$work/apt-check.log" >&2
    echo "the package does not install on Debian $suite (above: apt's reason). A .deb" \
        "built on a newer system than the Ubuntu 24.04 baseline needs a newer glibc" \
        "or WebKitGTK than Debian 13 has." >&2
    exit 1
fi
say "the package installs on $suite"

# --- the build ---------------------------------------------------------------------
# A fresh copy of the configuration each time; live-build's cache/ stays.
build="$builder/build"
sudo_ mkdir -p "$build"
sudo_ find "$build" -mindepth 1 -maxdepth 1 ! -name cache -exec rm -rf --one-file-system {} +
sudo_ cp -a packaging/linux/media/auto packaging/linux/media/config "$build/"
sudo_ install -D -m644 "$deb" "$build/config/packages.chroot/own-backup_${pkg_version}_amd64.deb"
say "lb config"
in_builder sh -c 'cd /build && lb config'
say "lb build (long; the log is $build/build.log)"
if ! in_builder sh -c 'cd /build && lb build'; then
    tail -n 40 "$build/build.log" >&2 || true
    echo "the medium was not built; the whole log: $build/build.log" >&2
    exit 1
fi

iso="$(find "$build" -maxdepth 1 -name '*.hybrid.iso' -print -quit)"
if [[ -z "$iso" ]]; then
    echo "lb build finished without an ISO (see $build/build.log)" >&2
    exit 1
fi
version="${pkg_version%-*}"
output="${OUTPUT:-$target_dir/dist/own-backup-media_${version}_amd64.iso}"
mkdir -p "$(dirname "$output")"
sudo_ cp "$iso" "$output"
sudo_ chown "$(id -u):$(id -g)" "$output"
(cd "$(dirname "$output")" && sha256sum "$(basename "$output")" >"$(basename "$output").sha256")
say "done: $output ($(( $(stat -c %s "$output") >> 20 )) MiB)"
