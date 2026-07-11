#!/usr/bin/env bash
# Regenerates xfs_reflink.img.zst — the reflink fixture for
# xfs_exclusions::stage1_zeroes_private_keeps_shared_repair_clean. Needs root
# (loop mount) + xfsprogs, and xfs in the kernel. Run on any such Linux:
#
#   sudo bash gen_xfs.sh
#
# Layout (v5, crc, reflink): keep.txt (0xCD ×64K, kept), secret.log (0xAB ×128K)
# reflink-copied to shared_clone.bin — so secret.log's extent has refcount 2 and
# must NOT be zeroed; cache/big.bin (0xAB ×64K, PRIVATE) — zeroed when excluded.
set -euo pipefail
cd "$(dirname "$0")"
T=$(mktemp -d)
trap 'umount "$T/mnt" 2>/dev/null || true; rm -rf "$T"' EXIT

truncate -s 400M "$T/fs.img"          # xfs minimum device size is ~300 MB
mkfs.xfs -q -m reflink=1,crc=1 "$T/fs.img"
mkdir -p "$T/mnt"
mount -o loop "$T/fs.img" "$T/mnt"
python3 -c "open('$T/mnt/keep.txt','wb').write(b'\xcd'*65536)"
python3 -c "open('$T/mnt/secret.log','wb').write(b'\xab'*131072)"
cp --reflink=always "$T/mnt/secret.log" "$T/mnt/shared_clone.bin"
mkdir "$T/mnt/cache"
python3 -c "open('$T/mnt/cache/big.bin','wb').write(b'\xab'*65536)"
sync
umount "$T/mnt"
xfs_repair -n "$T/fs.img" >/dev/null
zstd -19 -q -f "$T/fs.img" -o xfs_reflink.img.zst
echo "xfs_reflink.img.zst regenerated"
