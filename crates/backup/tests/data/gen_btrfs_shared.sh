#!/usr/bin/env bash
# Regenerates btrfs_shared.img.zst — the shared-extent (reflink + snapshot)
# fixture for btrfs_exclusions::shared_extents_are_never_zeroed. Needs root
# (loop mount) and btrfs-progs; run on any Linux with btrfs in the kernel:
#
#   sudo bash gen_btrfs_shared.sh
#
# Layout: keep.txt (0xCD ×64K), orig.bin (0xAB ×256K), clone.bin (reflink of
# orig — shares its extents), snap/ (snapshot of the whole volume — shares
# everything). Excluding orig.bin must zero NOTHING: every extent is shared.
set -euo pipefail
cd "$(dirname "$0")"
T=$(mktemp -d)
trap 'umount "$T/mnt" 2>/dev/null || true; rm -rf "$T"' EXIT

truncate -s 128M "$T/shared.img"
mkfs.btrfs -q "$T/shared.img"
mkdir -p "$T/mnt"
mount -o loop "$T/shared.img" "$T/mnt"
python3 -c "open('$T/mnt/keep.txt','wb').write(b'\xcd'*65536)"
python3 -c "open('$T/mnt/orig.bin','wb').write(b'\xab'*262144)"
sync "$T/mnt/orig.bin"
cp --reflink=always "$T/mnt/orig.bin" "$T/mnt/clone.bin"
btrfs subvolume snapshot "$T/mnt" "$T/mnt/snap" >/dev/null
sync
umount "$T/mnt"
btrfs check "$T/shared.img" >/dev/null
zstd -19 -q -f "$T/shared.img" -o btrfs_shared.img.zst
echo "btrfs_shared.img.zst regenerated"
