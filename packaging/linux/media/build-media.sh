#!/usr/bin/env bash
# Builds the Linux boot medium's ISO: a Debian 13 "trixie" live system with
# the engine and the window in a cage kiosk (this directory's auto/ and
# config/, PLAN §6 stage 6, decision 11). UEFI with Secure Boot through
# Debian's signed shim/GRUB/kernel, BIOS through ISOLINUX, a USB stick through
# the isohybrid MBR.
#
# One script, two callers:
#   - the service, installed as /usr/libexec/own-backup/build-linux-media with
#     this directory's auto/ and config/ in /usr/share/own-backup/media, run as
#     root in a transient unit (crates/backup/src/media_linux_build.rs);
#   - scripts/build-linux-media.sh in the repository, through sudo, with the
#     repository's paths.
#
#   build-media.sh build --config DIR --deb FILE --work DIR
#   build-media.sh clean --work DIR   # the build tree goes; builder and package cache stay
#   build-media.sh purge --work DIR   # everything under DIR goes
#
# How: the build runs inside a minimal Debian trixie tree made by debootstrap
# (DIR/builder), with Debian's own live-build. The host's live-build is not
# used: Ubuntu ships an old fork (3.0~a57) that cannot build a trixie medium
# with Secure Boot. The tree and live-build's package cache (builder/build/
# cache) are kept between runs, so a rebuild downloads nothing but updates.
#
# Needs: root, debootstrap and debian-archive-keyring on the host, network to
# the Debian mirror (OBK_MEDIA_MIRROR, default http://deb.debian.org/debian/;
# OBK_MEDIA_SECURITY_MIRROR). The caller checks the space (about 10 GiB).
#
# Output is the tools' own lines (stderr folded into stdout) and, for the
# caller to act on, lines of its own:
#   OBK-STAGE builder | download   a stage begins; the later ones (system, iso)
#                                  follow from live-build's "[time] lb <step>"
#   OBK-VERSION <version>          the package's version
#   OBK-APT <line>                 apt's reason when the package does not install
#   OBK-ERROR <code> [detail]      why it stopped: not-root, usage,
#                                  missing-tool NAME, missing-keyring PATH,
#                                  arch NAME, network HOST, bad-deb, apt-check,
#                                  no-iso
#   OBK-ISO <path>                 the finished image, inside DIR
# Signals (TERM, INT, HUP, and PIPE when the reader is gone) stop it: the
# builder's mounts are undone and the half-built tree is removed; the builder
# and the package cache stay.
set -euo pipefail
exec 2>&1

usage() {
    echo "OBK-ERROR usage"
    sed -n '15,17p' "$0"
    exit 2
}

cmd="${1:-}"
[[ -n $cmd ]] || usage
shift
config="" deb="" work=""
while [[ $# -gt 0 ]]; do
    case "$1" in
        --config) config="${2:-}"; shift 2 ;;
        --deb) deb="${2:-}"; shift 2 ;;
        --work) work="${2:-}"; shift 2 ;;
        *) usage ;;
    esac
done
[[ -n $work ]] || usage
case "$cmd" in
    build) [[ -n $config && -n $deb ]] || usage ;;
    clean | purge) ;;
    *) usage ;;
esac

if [[ "$(id -u)" -ne 0 ]]; then
    echo "OBK-ERROR not-root"
    exit 1
fi

builder="$work/builder"
build="$builder/build"
mirror="${OBK_MEDIA_MIRROR:-http://deb.debian.org/debian/}"
security="${OBK_MEDIA_SECURITY_MIRROR:-http://security.debian.org/debian-security/}"
suite=trixie

say() {
    echo "[linux-media] $*"
}

# Every mount point at or below $1, deepest first, lazily unmounted: the
# builder's binds and whatever live-build left mounted in its chroot when it
# was stopped. Field 5 of mountinfo is the mount point, spaces as \040.
unmount_under() {
    local mp
    awk -v base="$1/" '{ mp = $5; gsub(/\\040/, " ", mp); if (index(mp "/", base) == 1) print mp }' \
        /proc/self/mountinfo | sort -r | while IFS= read -r mp; do
        umount -l "$mp" 2>/dev/null || true
    done
}

# The build tree without live-build's package cache.
drop_build() {
    if [[ -d $build ]]; then
        find "$build" -mindepth 1 -maxdepth 1 ! -name cache -exec rm -rf --one-file-system {} +
    fi
}

state=running
# The clean-up ignores the stop signal, and so do the tools it runs (an
# ignored signal stays ignored across exec): systemd's stop signals the unit's
# processes until no new ones appear, and would kill the umount and rm this
# forks — the stand's first cancel left the half-built tree mounted (22.10).
# TimeoutStopSec of the transient unit bounds it.
#
# A reader that went away (the service killed or upgraded mid-build: the unit
# outlives it, its output pipe does not) must not kill the script before this
# runs: SIGPIPE's default action ends bash without its EXIT trap, leaving the
# builder's binds mounted. So it is caught, not ignored — an ignored signal
# would be inherited by every tool, whose own pipelines rely on it — and
# treated as a stop; the clean-up's messages then just fail to be written.
on_exit() {
    trap '' TERM INT HUP PIPE
    unmount_under "$builder"
    if [[ $state == stopped ]]; then
        drop_build
        say "stopped; the half-built tree was removed" || true
    fi
}
trap on_exit EXIT
trap "trap '' TERM INT HUP PIPE; state=stopped; exit 143" TERM INT HUP
trap "trap '' TERM INT HUP PIPE; state=stopped; exit 141" PIPE

case "$cmd" in
    clean)
        say "removing the build tree (the builder and the package cache stay)"
        # Mounts first: rm --one-file-system fails on a mount point it skips.
        unmount_under "$builder"
        drop_build
        exit 0
        ;;
    purge)
        say "removing $work"
        unmount_under "$work"
        if [[ -d $work ]]; then
            find "$work" -mindepth 1 -maxdepth 1 -exec rm -rf --one-file-system {} +
        fi
        exit 0
        ;;
esac

# --- what the host must have -----------------------------------------------------
for tool in debootstrap chroot mount umount dpkg-deb getent; do
    if ! command -v "$tool" >/dev/null; then
        echo "OBK-ERROR missing-tool $tool"
        exit 1
    fi
done
keyring=/usr/share/keyrings/debian-archive-keyring.gpg
if [[ ! -r $keyring ]]; then
    echo "OBK-ERROR missing-keyring $keyring"
    exit 1
fi
arch="$(dpkg --print-architecture)"
if [[ $arch != amd64 ]]; then
    echo "OBK-ERROR arch $arch"
    exit 1
fi
config="$(realpath "$config")"
deb="$(realpath "$deb")"
if ! pkg_version="$(dpkg-deb -f "$deb" Version 2>/dev/null)" || [[ -z $pkg_version ]] \
    || [[ "$(dpkg-deb -f "$deb" Package 2>/dev/null)" != own-backup ]]; then
    echo "OBK-ERROR bad-deb"
    exit 1
fi
echo "OBK-VERSION $pkg_version"
say "package: $(basename "$deb") ($pkg_version)"

# The mirror's name has to resolve before anything is downloaded: "no network"
# is then said as that, not as a debootstrap error three screens later.
host="${mirror#*://}"
host="${host%%/*}"
host="${host%%:*}"
if ! getent hosts "$host" >/dev/null; then
    echo "OBK-ERROR network $host"
    exit 1
fi

# --- the builder: Debian trixie with Debian's live-build --------------------------
echo "OBK-STAGE builder"
mkdir -p "$work"
unmount_under "$builder"
if [[ ! -x $builder/usr/bin/lb ]]; then
    say "making the builder (debootstrap $suite from $mirror)"
    rm -rf --one-file-system "$builder"
    debootstrap --variant=minbase --keyring="$keyring" \
        --include=live-build,debootstrap,debian-archive-keyring,xorriso,squashfs-tools,mtools,dosfstools,ca-certificates,cpio,wget \
        "$suite" "$builder" "$mirror"
fi
cat >"$builder/etc/apt/sources.list" <<EOF
deb $mirror $suite main non-free-firmware
deb $mirror $suite-updates main non-free-firmware
deb $security $suite-security main non-free-firmware
EOF
for fs in proc sys dev dev/pts; do
    mkdir -p "$builder/$fs"
    mount --bind "/$fs" "$builder/$fs"
done
cp /etc/resolv.conf "$builder/etc/resolv.conf"
in_builder() {
    chroot "$builder" /usr/bin/env -i PATH=/usr/sbin:/usr/bin:/sbin:/bin \
        LANG=C.UTF-8 HOME=/root OBK_MEDIA_MIRROR="$mirror" \
        OBK_MEDIA_SECURITY_MIRROR="$security" "$@"
}
say "updating the builder"
in_builder apt-get --error-on=any -qq update
in_builder apt-get -qq -y full-upgrade
# shellcheck disable=SC2016 # ${Version} is dpkg-query's, not the shell's
say "live-build $(in_builder dpkg-query -W -f='${Version}' live-build)"

# --- does our package install on trixie? ------------------------------------------
install -D -m644 "$deb" "$builder/tmp/own-backup.deb"
if ! in_builder apt-get -s -qq install /tmp/own-backup.deb >"$work/apt-check.log" 2>&1; then
    sed 's/^/OBK-APT /' "$work/apt-check.log"
    echo "OBK-ERROR apt-check"
    exit 1
fi
say "the package installs on $suite"

# --- the build ---------------------------------------------------------------------
# A fresh copy of the configuration each time; live-build's cache/ stays.
echo "OBK-STAGE download"
mkdir -p "$build"
drop_build
cp -a "$config/auto" "$config/config" "$build/"
install -D -m644 "$deb" "$build/config/packages.chroot/own-backup_${pkg_version}_amd64.deb"
say "lb config"
in_builder sh -c 'cd /build && lb config'
say "lb build (long; the log is $build/build.log)"
if ! in_builder sh -c 'cd /build && lb build'; then
    say "the medium was not built; the whole log: $build/build.log"
    exit 1
fi

iso="$(find "$build" -maxdepth 1 -name '*.hybrid.iso' -print -quit)"
if [[ -z $iso ]]; then
    echo "OBK-ERROR no-iso"
    exit 1
fi
state=done
echo "OBK-ISO $iso"
