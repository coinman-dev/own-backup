#!/usr/bin/env bash
# Builds the Linux boot medium: a Debian 13 "trixie" live ISO with the engine
# and the window on a labwc desktop (packaging/linux/media, PLAN §6 stage 6,
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
# How: packaging/linux/media/build-media.sh does the build — the same script
# the service runs when the window builds the medium (installed as
# /usr/libexec/own-backup/build-linux-media). This one adds what a developer
# needs around it: the package built from this checkout, the space rule of
# AGENTS.md, and the result in target/dist. The builder tree
# (target/linux-media/builder) and its package cache are kept between runs, so
# a rebuild after changing only the program downloads nothing but updates.
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
shared="$root/packaging/linux/media/build-media.sh"

sudo_() {
    if [[ "$(id -u)" -eq 0 ]]; then "$@"; else sudo "$@"; fi
}

say() {
    echo "[linux-media] $*"
}

# The mirror settings go through sudo, which drops the environment.
shared_() {
    sudo_ env OBK_MEDIA_MIRROR="${OBK_MEDIA_MIRROR:-}" \
        OBK_MEDIA_SECURITY_MIRROR="${OBK_MEDIA_SECURITY_MIRROR:-}" "$shared" "$@"
}

if [[ "$clean" -eq 1 ]]; then
    shared_ clean --work "$work"
    exit 0
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

# --- the build (the shared script) -------------------------------------------------
# Its own lines (OBK-*) are for the service; here they are shown as they come,
# and OBK-ISO names the result.
log="$work/build-output.log"
shared_ build --config "$root/packaging/linux/media" --deb "$deb" --work "$work" | tee "$log"
iso="$(sed -n 's/^OBK-ISO //p' "$log" | tail -1)"
if [[ -z "$iso" ]]; then
    echo "the medium was not built (see above; OBK-ERROR names the reason)" >&2
    exit 1
fi
pkg_version="$(dpkg-deb -f "$deb" Version)"
version="${pkg_version%-*}"
output="${OUTPUT:-$target_dir/dist/own-backup-media_${version}_amd64.iso}"
mkdir -p "$(dirname "$output")"
sudo_ cp "$iso" "$output"
sudo_ chown "$(id -u):$(id -g)" "$output"
(cd "$(dirname "$output")" && sha256sum "$(basename "$output")" >"$(basename "$output").sha256")
say "done: $output ($(( $(stat -c %s "$output") >> 20 )) MiB)"
