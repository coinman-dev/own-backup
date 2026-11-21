#!/usr/bin/env bash
# The .deb built inside an Ubuntu 24.04 (noble) tree, from this machine of any
# newer distribution: the package takes the glibc and WebKitGTK of the system
# it is built on as its minimum (scripts/build-deb.sh), and 24.04 is the
# oldest supported one. Releases are built the same way by GitHub
# (.github/workflows/release.yml, ubuntu-24.04); this is for a test package.
#
#   scripts/build-deb-noble.sh       # target/dist/own-backup_<version>_amd64.deb
#
# Needs sudo (debootstrap, mount, chroot). The tree is made once in
# tmp/deb-noble and its cargo cache is tmp/deb-noble-target; both are
# re-created after scripts/clean-artifacts.sh --force, at the cost of the
# download. While it runs, this repository is bind-mounted at
# tmp/deb-noble/src — the cleanup script leaves a tree with anything mounted
# in it alone.
set -eu
R="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
SRC=${SRC:-$R}
C=$R/tmp/deb-noble
S() { sudo "$@"; }
mkdir -p "$R/tmp"
if [ ! -x "$C/usr/bin/apt-get" ]; then
  S debootstrap --variant=minbase --components=main,universe noble "$C" http://archive.ubuntu.com/ubuntu
fi
S cp /etc/resolv.conf "$C/etc/resolv.conf"
for m in proc sys dev; do mountpoint -q "$C/$m" || S mount --bind "/$m" "$C/$m"; done
S mkdir -p "$C/src"; mountpoint -q "$C/src" || S mount --bind "$SRC" "$C/src"
trap 'for m in src dev sys proc; do S umount -l "$C/$m" 2>/dev/null || true; done' EXIT
S chroot "$C" bash -c 'echo "deb http://archive.ubuntu.com/ubuntu noble-updates main universe" > /etc/apt/sources.list.d/updates.list; apt-get update -qq && DEBIAN_FRONTEND=noninteractive apt-get install -y -qq build-essential pkg-config protobuf-compiler nodejs npm libwebkit2gtk-4.1-dev libsoup-3.0-dev libayatana-appindicator3-dev librsvg2-dev libgtk-layer-shell-dev dpkg-dev desktop-file-utils curl ca-certificates python3 file >/dev/null; id b >/dev/null 2>&1 || useradd -u '"$(id -u)"' -m b'
S chroot --userspec="$(id -u):$(id -g)" "$C" env HOME=/home/b bash -c 'test -x ~/.cargo/bin/cargo || curl -sSf https://sh.rustup.rs | sh -s -- -y -q --default-toolchain none --profile minimal'
S chroot --userspec="$(id -u):$(id -g)" "$C" env HOME=/home/b PATH=/home/b/.cargo/bin:/usr/bin:/bin CARGO_TARGET_DIR=/src/tmp/deb-noble-target DEB_REVISION="${DEB_REVISION:-1}" SOURCE_DATE_EPOCH="${SOURCE_DATE_EPOCH:-}" CARGO_INCREMENTAL=0 NODE_OPTIONS=--experimental-global-webcrypto bash -c 'cd /src && scripts/build-deb.sh'
