#!/usr/bin/env bash
# Turn the folder make-winpe-media.cmd produces into a bootable VMDK for VMware.
#
#   scripts/make-media-vmdk.sh [media_folder] [output.vmdk]
#
# Defaults are the medium built on drive D and the VM's existing disk:
#   win-files/disk-d/OwnBackupMedia/media -> win-files/VMware/OwnBackupMedia.vmdk
#
# The layout is a GPT disk with a single EFI System Partition (FAT32) holding
# the media files; VMware's EFI firmware finds \EFI\BOOT\BOOTX64.EFI there, and
# the BCD store next to it boots \sources\boot.wim off a ramdisk.
#
# Nothing is mounted: the filesystem is built in a plain file with mkfs.vfat and
# filled with mtools, so no root and no loop device are needed. Needs qemu-img,
# mkfs.vfat, mtools and sgdisk.
set -euo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
MEDIA="${1:-$REPO/win-files/disk-d/OwnBackupMedia/media}"
OUT="${2:-$REPO/win-files/VMware/OwnBackupMedia.vmdk}"
WORK="$REPO/tmp/vmdk-build"

for tool in qemu-img mkfs.vfat mcopy mtype sgdisk; do
  command -v "$tool" >/dev/null || { echo "[!] $tool is missing"; exit 1; }
done
for f in EFI/BOOT/BOOTX64.EFI EFI/Microsoft/Boot/BCD boot/boot.sdi sources/boot.wim; do
  [ -f "$MEDIA/$f" ] || { echo "[!] $MEDIA/$f not found - build the medium first"; exit 1; }
done

# Disk size: the payload plus a quarter for slack, at least 1 GiB, rounded up to
# whole MiB. Growing the medium (a bigger boot.wim) then needs no edit here.
PAYLOAD=$(du -sb "$MEDIA" | cut -f1)
MIB=$(( (PAYLOAD * 5 / 4) / 1048576 + 16 ))
[ "$MIB" -lt 1024 ] && MIB=1024

echo "=== media   : $MEDIA ($((PAYLOAD / 1048576)) MiB)"
echo "=== disk    : ${MIB} MiB"
echo "=== output  : $OUT"

rm -rf "$WORK"; mkdir -p "$WORK"
trap 'rm -rf "$WORK"' EXIT

# 1. GPT with one ESP, starting at the conventional 1 MiB and stopping short of
#    the secondary GPT (the last 34 sectors).
truncate -s "${MIB}M" "$WORK/disk.raw"
sgdisk -Z "$WORK/disk.raw" >/dev/null 2>&1 || true
sgdisk -n 1:2048:-34 -t 1:ef00 -c 1:OWNBACKUP "$WORK/disk.raw" >/dev/null

read -r PSTART PEND < <(sgdisk -i 1 "$WORK/disk.raw" |
  awk '/First sector/{s=$3} /Last sector/{e=$3} END{print s, e}')
PSECT=$(( PEND - PSTART + 1 ))

# 2. The filesystem, built beside the disk and written into the partition. The
#    BPB's hidden-sector count has to match where the partition actually starts,
#    or firmware that trusts it reads the volume at the wrong offset.
truncate -s $(( PSECT * 512 )) "$WORK/esp.img"
mkfs.vfat -F 32 -S 512 -h "$PSTART" -n OWNBACKUP "$WORK/esp.img" >/dev/null
mcopy -i "$WORK/esp.img" -s "$MEDIA"/* ::
dd if="$WORK/esp.img" of="$WORK/disk.raw" bs=512 seek="$PSTART" conv=notrunc status=none
rm -f "$WORK/esp.img"

# 3. boot.wim is the one file big enough for a silent truncation to go unnoticed
#    until the machine fails to boot, so it is read back out of the assembled
#    disk and compared.
echo "=== verifying boot.wim inside the image..."
WANT=$(sha256sum "$MEDIA/sources/boot.wim" | cut -d' ' -f1)
GOT=$(mtype -i "$WORK/disk.raw@@$(( PSTART * 512 ))" ::/sources/boot.wim | sha256sum | cut -d' ' -f1)
[ "$WANT" = "$GOT" ] || { echo "[!] boot.wim does not match the source"; exit 1; }

# 4. The VMDK. It is built under its final name because monolithicSparse records
#    the extent's file name inside the file itself - renaming it afterwards
#    leaves the descriptor pointing at a name that no longer exists.
NAME="$(basename "$OUT")"
qemu-img convert -f raw -O vmdk -o adapter_type=lsilogic,subformat=monolithicSparse \
  "$WORK/disk.raw" "$WORK/$NAME"
qemu-img compare "$WORK/disk.raw" "$WORK/$NAME"

mkdir -p "$(dirname "$OUT")"
cp "$WORK/$NAME" "$OUT"
echo "=== done: $OUT ($(du -h "$OUT" | cut -f1))"
echo "    The VM must be powered off; attach it as the boot disk with firmware=efi."
