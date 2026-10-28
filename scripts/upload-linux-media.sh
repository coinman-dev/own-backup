#!/usr/bin/env bash
# Puts a built Linux boot medium into a GitHub release, under the name the
# window looks for (crates/backup/src/media_linux.rs, release_asset_name):
# OwnBackup-Linux-<package version, "~+:" as dots, no epoch>_amd64.iso, and
# its line into the release's one checksum list, SHA256SUMS.txt (the window
# refuses an ISO it does not name). Uses `gh` and the API only: no workflows.
#
#   scripts/upload-linux-media.sh TAG ISO VERSION
#   scripts/upload-linux-media.sh v0.1.8-beta target/dist/own-backup-media_0.1.8~beta_amd64.iso 0.1.8~beta
#
# VERSION is the .deb's Version exactly as installed (`dpkg-query -W
# own-backup`): the window downloads only the medium of the installed build.
# The release must be public (the window downloads without a login).
set -euo pipefail

if [[ $# -ne 3 ]]; then
    sed -n '7,8p' "$0"
    exit 2
fi
tag="$1" iso="$2" version="$3"
[[ -f $iso ]] || { echo "no such ISO: $iso" >&2; exit 1; }

v="${version#*:}"
v="$(printf '%s' "$v" | tr -c 'A-Za-z0-9._-' '.')"
name="OwnBackup-Linux-${v}_amd64.iso"

root="$(cd "$(dirname "$0")/.." && pwd)"
work="$(mktemp -d "$root/tmp/upload-linux-media.XXXXXX")"
trap 'rm -rf "$work"' EXIT

ln -s "$(realpath "$iso")" "$work/$name"
sum="$(sha256sum "$iso" | cut -d' ' -f1)"

# The release's list — the installer's and the .deb's lines stay — without
# an older line for this name. (Releases up to 0.1.6-beta also carry a
# separate SHA256SUMS; the window still reads it there, nothing writes it.)
sums=SHA256SUMS.txt
if gh release download "$tag" --pattern "$sums" --dir "$work" 2>/dev/null; then
    grep -v "[ *]$name\$" "$work/$sums" >"$work/$sums.new" || true
else
    : >"$work/$sums.new"
fi
printf '%s  %s\n' "$sum" "$name" >>"$work/$sums.new"
mv "$work/$sums.new" "$work/$sums"

echo "uploading $name ($(du -h "$iso" | cut -f1)) into $tag"
gh release upload "$tag" "$work/$name" "$work/$sums" --clobber
echo "done: $name, sha256 $sum"
