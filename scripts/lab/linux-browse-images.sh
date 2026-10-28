#!/usr/bin/env bash
# Test images for the Total Commander plugin (crates/tc-plugin) and the
# service's LinuxBrowse, made by the real tools: an encrypted Ubuntu-like
# partition — LUKS2 (argon2id) holding an LVM volume group `vgobk` with `root`
# (ext4) and `swap_1`; btrfs straight inside LUKS2 with Ubuntu's subvolumes
# `@` and `@home`, zstd-compressed; and a plain ext4 partition. Run as root
# (loop devices, device-mapper):
#
#   sudo scripts/lab/linux-browse-images.sh DIR [PASSPHRASE]
#
# Then offer them to a development build of the service and drive the
# plugin with crates/tc-plugin/examples/wfx_harness.rs:
#
#   OBK_LINUX_BROWSE_IMAGES="DIR/luks-lvm.img;DIR/luks-btrfs.img;DIR/plain.img" \
#       target/debug/own-backup-service --foreground
#
# /home/user/big.bin holds i % 251 for 5 MiB + 123 bytes, which the harness
# checks byte for byte.
set -euo pipefail
dir=${1:?usage: $0 DIR [PASSPHRASE]}
pw=${2:-correct horse battery}
mkdir -p "$dir"
cd "$dir"
# No udev in a container or WSL: LVM must not wait for it.
lvm_conf='activation { udev_sync = 0 udev_rules = 0 } devices { obtain_device_list_from_udev = 0 }'

loop="" mapped=""
cleanup() {
    set +e
    mountpoint -q mnt && umount mnt
    vgchange -an --config "$lvm_conf" vgobk >/dev/null 2>&1
    [ -n "$mapped" ] && cryptsetup close obk-browse-test
    [ -n "$loop" ] && losetup -d "$loop"
}
trap cleanup EXIT

fill() { # the files every image holds
    local root=$1
    mkdir -p "$root/home/user/Документы" "$root/etc"
    printf 'привет\n' >"$root/home/user/Документы/отчёт.txt"
    python3 -c "import sys; sys.stdout.buffer.write(bytes(i % 251 for i in range(5*1024*1024+123)))" \
        >"$root/home/user/big.bin"
    printf 'colon' >"$root/home/user/a:b.txt"
    ln -sf user/big.bin "$root/home/link.bin"
    printf 'own-backup\n' >"$root/etc/hostname"
}

rm -f luks-lvm.img luks-btrfs.img plain.img
truncate -s 160M luks-lvm.img
loop=$(losetup -f --show luks-lvm.img)
printf '%s' "$pw" | cryptsetup luksFormat -q --type luks2 --pbkdf argon2id --key-file - "$loop"
printf '%s' "$pw" | cryptsetup open --key-file - "$loop" obk-browse-test
mapped=1
pvcreate -q --config "$lvm_conf" /dev/mapper/obk-browse-test
vgcreate -q --config "$lvm_conf" vgobk /dev/mapper/obk-browse-test
lvcreate -q -y --config "$lvm_conf" -L 96M -n root vgobk
lvcreate -q -y --config "$lvm_conf" -l 100%FREE -n swap_1 vgobk
vgmknodes --config "$lvm_conf" vgobk >/dev/null 2>&1 || true
mkfs.ext4 -q -L obk-root /dev/vgobk/root
mkswap -q /dev/vgobk/swap_1
mkdir -p mnt
mount /dev/vgobk/root mnt
fill mnt
umount mnt
cleanup
loop="" mapped=""
trap - EXIT

# btrfs in LUKS, the files put in by mkfs.btrfs itself — homes in @home,
# and @/etc/fstab mounting @ at / and @home at /home, as Ubuntu's does.
fsid=0f1e2d3c-4b5a-6978-8796-a5b4c3d2e1f0
src=$(mktemp -d)
mkdir -p "$src/@"
fill "$src/@"
mv "$src/@/home" "$src/@home"
mkdir "$src/@/home"
printf 'home subvolume\n' >"$src/@home/user/note.txt"
printf 'UUID=%s /     btrfs defaults,subvol=@ 0 1\nUUID=%s /home btrfs defaults,subvol=@home 0 2\n' \
    "$fsid" "$fsid" >"$src/@/etc/fstab"
truncate -s 160M luks-btrfs.img
loop=$(losetup -f --show luks-btrfs.img)
trap cleanup EXIT
printf '%s' "$pw" | cryptsetup luksFormat -q --type luks2 --pbkdf argon2id --key-file - "$loop"
printf '%s' "$pw" | cryptsetup open --key-file - "$loop" obk-browse-test
mapped=1
mkfs.btrfs -q -f -L obk-btrfs -U "$fsid" --rootdir "$src" --subvol rw:@ --subvol rw:@home \
    --compress zstd /dev/mapper/obk-browse-test
cleanup
loop="" mapped=""
trap - EXIT
rm -rf "$src"

src=$(mktemp -d)
fill "$src"
truncate -s 32M plain.img
mkfs.ext4 -q -L obk-plain -d "$src" plain.img
rm -rf "$src" mnt
chmod 0644 luks-lvm.img luks-btrfs.img plain.img
echo "made $dir/luks-lvm.img, $dir/luks-btrfs.img (passphrase: $pw) and $dir/plain.img"
