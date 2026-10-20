#!/usr/bin/env bash
# Linux acceptance of restore and verify THROUGH THE SERVICE (plan §6, stage
# 3): run as root inside a stand guest (`scripts/lab/linux-vm.py`) with the
# package installed and its service running. The host half — making the
# source system and booting what was restored — is
# `scripts/lab/linux-restore-boot.py`.
#
#   sudo scripts/lab/linux-restore-scenario.sh RPC CLI [STAGE...]
#
# RPC is the stand's driver of the service RPCs
# (`cargo build -p own-backup-client --example linux_restore_rpc`,
# target/debug/examples/linux_restore_rpc), CLI the package's
# /usr/bin/own-backup-cli or a build. STAGE is any of
#   verify guard clash disk plan volume mount mbr
# (default: all but mbr; mbr belongs to a `linux-vm.py up --bios` run). Each
# check prints `CHECK <id> PASS|FAIL|SKIP <text>`; the exit code is the
# number of FAILs. Evidence stays in $WORK/evidence.
#
# Disks (the run's data disks, by their stable names):
#   SOURCE  virtio-ob-data-1  a bootable system (`linux-restore-boot.py source`
#                             before `up`: an overlay of the Ubuntu base image)
#   TARGET  virtio-ob-data-2  blank; overwritten by the restores
#   STORE   virtio-ob-data-3  formatted here, holds the copies
#
# Order on the host, UEFI:
#   linux-vm.py up --disks 3 --size 6 ; linux-vm.py down
#   linux-restore-boot.py source ; linux-vm.py up
#   linux-vm.py put … (package, RPC tool, this script) ; install the package
#   linux-vm.py ssh -- sudo …/linux-restore-scenario.sh RPC /usr/bin/own-backup-cli
#   linux-vm.py down ; linux-restore-boot.py boot data-2.qcow2
# BIOS/MBR: the same with `up --bios … ` and the stage `mbr`, then
#   linux-restore-boot.py boot data-1.qcow2 --bios
set -uo pipefail

RPC=$(readlink -f "${1:?RPC tool}")
CLI=$(readlink -f "${2:?CLI}")
shift 2
STAGES=("$@")
[[ ${#STAGES[@]} -eq 0 ]] && STAGES=(verify guard clash disk plan volume mount)

SOURCE=${SOURCE:-/dev/disk/by-id/virtio-ob-data-1}
TARGET=${TARGET:-/dev/disk/by-id/virtio-ob-data-2}
STORE=${STORE:-/dev/disk/by-id/virtio-ob-data-3}
WORK=${WORK:-/var/tmp/lab-restore}
EV=$WORK/evidence
COPIES=$WORK/store
FAILS=0
MOUNTS=()

if [[ $(id -u) -ne 0 ]]; then
    echo "run as root (raw disks, mount)" >&2
    exit 2
fi
for x in "$RPC" "$CLI"; do [[ -x $x ]] || { echo "not executable: $x" >&2; exit 2; }; done
for d in "$SOURCE" "$TARGET" "$STORE"; do [[ -b $d ]] || { echo "no disk $d" >&2; exit 2; }; done

check() { # id verdict text...
    local id=$1 verdict=$2
    shift 2
    [[ $verdict == FAIL ]] && FAILS=$((FAILS + 1))
    printf 'CHECK %-34s %-4s %s\n' "$id" "$verdict" "$*" | tee -a "$EV/summary.txt"
}
verdict() { if [[ $1 -eq 0 ]]; then echo PASS; else echo FAIL; fi; }
run() { # NAME CMD...: stdout+stderr to $EV/NAME.out, exit code returned
    local name=$1
    shift
    "$@" >"$EV/$name.out" 2>&1
}
# The JSON reply of the last `run NAME` (jq-free: python is in every guest).
field() { python3 -c "import json,sys;d=json.load(open('$EV/$1.out'));print(eval('d'+sys.argv[1]))" "$2"; }
part() { # disk number -> its node
    local name
    name=$(basename "$(readlink -f "$1")")
    for p in /sys/class/block/"$name"/"$name"*; do
        [[ $(cat "$p/partition" 2>/dev/null) == "$2" ]] && { echo "/dev/$(basename "$p")"; return; }
    done
    return 1
}
same_partition() { # disk-a disk-b number [esp-may-gain-fallback]: the same data
    # Byte-identical, or — for a filesystem the copy took by its used blocks,
    # whose free space the restore wipes — the same files and a clean check.
    local a b fs ma=$WORK/cmp-a mb=$WORK/cmp-b rc=1 extra=()
    a=$(part "$1" "$3") && b=$(part "$2" "$3") || return 1
    cmp -s "$a" "$b" && return 0
    fs=$(blkid -o value -s TYPE "$a")
    case $fs in
        ext2|ext3|ext4) e2fsck -fn "$b" >/dev/null 2>&1 || return 1 ;;
        vfat) fsck.fat -n "$b" >/dev/null 2>&1 || return 1 ;;
        *) return 1 ;;
    esac
    [[ ${4:-} == esp ]] && extra=(-x BOOTX64.EFI -x BOOT)
    mkdir -p "$ma" "$mb"
    local opts=ro
    [[ $fs == ext* ]] && opts=ro,noload
    if mount -o "$opts" "$a" "$ma"; then
        if mount -o "$opts" "$b" "$mb"; then
            diff -r "${extra[@]}" "$ma" "$mb" >>"$EV/same-$3.diff" 2>&1 && rc=0
            umount "$mb"
        fi
        umount "$ma"
    fi
    [[ $rc -eq 0 ]] && echo "partition $3: free space differs, files identical, $fs check clean" >>"$EV/same.txt"
    return $rc
}
cleanup() {
    local m
    for m in "${MOUNTS[@]}"; do mountpoint -q "$m" && umount "$m"; done
}
trap cleanup EXIT

mkdir -p "$EV" "$COPIES"
: >"$EV/summary.txt"

# The store: a filesystem of its own, so a copy never lands on a disk it is about.
if ! mountpoint -q "$COPIES"; then
    blkid "$STORE" >/dev/null 2>&1 || mkfs.ext4 -q -F -L obk-store "$STORE"
    mount "$STORE" "$COPIES" && MOUNTS+=("$COPIES")
fi
UEFI=0
[[ -d /sys/firmware/efi ]] && UEFI=1
SRC_PARTS=$(lsblk -nro PARTN "$SOURCE" | sed '/^$/d' | tr '\n' ' ')
ROOT_SRC=$(findmnt -no SOURCE /)
ROOT_DISK=/dev/$(lsblk -no PKNAME "$ROOT_SRC" | head -1)
DISK_IMG=$COPIES/source-disk.qcow2

# ---------------------------------------------------------------------------
# verify: VerifyImage through the service; a damaged copy is said to be one.
# ---------------------------------------------------------------------------
stage_verify() {
    local boot=()
    [[ $UEFI -eq 1 ]] && boot=(--bootable)
    rm -f "$DISK_IMG"
    run v-copy "$CLI" backup-disk --device "$SOURCE" -o "$DISK_IMG" --format qcow2 "${boot[@]}"
    check v-copy "$(verdict $?)" "whole-disk copy of the source system ($SRC_PARTS)"
    run v-ok "$RPC" verify-image "$DISK_IMG"
    [[ $? -eq 0 && $(field v-ok "['valid']") == True ]]
    check v-ok "$(verdict $?)" "VerifyImage: the copy is whole"
    cp "$DISK_IMG" "$WORK/damaged.qcow2"
    python3 - "$WORK/damaged.qcow2" <<'EOF'
import os, sys
with open(sys.argv[1], 'r+b') as f:
    f.seek(os.path.getsize(sys.argv[1]) // 2)
    f.write(os.urandom(65536))
EOF
    run v-bad "$RPC" verify-image "$WORK/damaged.qcow2"
    local rc=$?
    [[ $rc -ne 0 ]] || [[ $(field v-bad "['valid']") == False ]]
    check v-bad "$(verdict $?)" "VerifyImage: a damaged copy is not called whole"
    rm -f "$WORK/damaged.qcow2"
}

# ---------------------------------------------------------------------------
# guard: the running system's own volume, ESP and disk are refused.
# ---------------------------------------------------------------------------
stage_guard() {
    [[ -f $DISK_IMG ]] || stage_verify
    run g-root "$RPC" restore-volume "$DISK_IMG" "$ROOT_SRC" --keep
    [[ $? -eq 3 ]]
    check g-root "$(verdict $?)" "RestoreVolume onto the mounted root $ROOT_SRC is refused"
    local esp
    esp=$(findmnt -no SOURCE /boot/efi 2>/dev/null)
    if [[ -n $esp ]]; then
        run g-esp "$RPC" restore-volume "$DISK_IMG" "$esp" --keep
        [[ $? -eq 3 ]]
        check g-esp "$(verdict $?)" "RestoreVolume onto the boot ESP $esp is refused"
    else
        check g-esp SKIP "no /boot/efi mounted (BIOS run)"
    fi
    run g-disk "$RPC" restore-disk "$DISK_IMG" "$ROOT_DISK" --keep
    [[ $? -eq 3 ]]
    check g-disk "$(verdict $?)" "RestoreDisk onto the running system's disk $ROOT_DISK is refused"
    run g-plan-make "$RPC" plan-disk "$DISK_IMG" "$ROOT_DISK" "$WORK/plan-live.json"
    run g-plan "$RPC" dry-run "$WORK/plan-live.json"
    grep -Eq '"(target_is_live_system_partition|live_system_disk_layout)"' "$EV/g-plan.out"
    check g-plan "$(verdict $?)" "a plan over the running system's disk is refused in the preview"
    run g-apply "$RPC" apply "$WORK/plan-live.json" --keep
    [[ $? -eq 3 ]]
    check g-apply "$(verdict $?)" "…and by ApplyRestorePlan, nothing written"
}

# ---------------------------------------------------------------------------
# clash: the source is still attached, so its identities are on this machine.
# ---------------------------------------------------------------------------
stage_clash() {
    [[ -f $DISK_IMG ]] || stage_verify
    wipefs -aq "$TARGET"
    run c-ask "$RPC" restore-disk "$DISK_IMG" "$TARGET"
    local rc=$?
    [[ $rc -eq 3 ]] && grep -q '"kind": "partuuid"' "$EV/c-ask.out"
    check c-ask "$(verdict $?)" "duplicate identities need a choice; the list names PARTUUIDs"
    grep -Eq '"kind": "(ext|btrfs|xfs)"' "$EV/c-ask.out"
    check c-ask-fs "$(verdict $?)" "…and the filesystem UUIDs"
    [[ -z $(blkid -p "$TARGET" 2>/dev/null) ]]
    check c-ask-nothing "$(verdict $?)" "asking wrote nothing"
}

# ---------------------------------------------------------------------------
# disk: RestoreDisk with the duplicates kept.
# ---------------------------------------------------------------------------
stage_disk() {
    [[ -f $DISK_IMG ]] || stage_verify
    run d-keep "$RPC" restore-disk "$DISK_IMG" "$TARGET" --keep
    check d-keep "$(verdict $?)" "RestoreDisk with identity_choice keep"
    udevadm settle
    sgdisk -v "$TARGET" >"$EV/d-sgdisk.out" 2>&1
    check d-gpt "$(verdict $?)" "the written GPT is valid (backup header at the true end)"
    [[ $(sgdisk -p "$SOURCE" | grep 'Disk identifier') == $(sgdisk -p "$TARGET" | grep 'Disk identifier') ]]
    check d-guid "$(verdict $?)" "the disk GUID is the copy's"
    local n bad=0
    for n in $SRC_PARTS; do
        same_partition "$SOURCE" "$TARGET" "$n" || { bad=1; echo "partition $n differs" >>"$EV/d-cmp.out"; }
    done
    check d-data "$(verdict $bad)" "every partition holds the source's data (bytes, or files + fsck)"
    [[ -z $(ls /run/udev/rules.d/59-own-backup-hold-* 2>/dev/null) ]]
    check d-hold "$(verdict $?)" "the temporary udev hold is gone"
}

# ---------------------------------------------------------------------------
# plan: the same through DryRun/ApplyRestorePlan, with the boot steps.
# ---------------------------------------------------------------------------
stage_plan() {
    [[ -f $DISK_IMG ]] || stage_verify
    wipefs -aq "$TARGET"
    local entries=()
    [[ $UEFI -eq 1 ]] && entries=(--entries)
    run p-make "$RPC" plan-disk "$DISK_IMG" "$TARGET" "$WORK/plan.json" "${entries[@]}"
    check p-make "$(verdict $?)" "plan: table + every partition + fallback loader"
    run p-dry "$RPC" dry-run "$WORK/plan.json"
    [[ $? -eq 0 ]] && ! grep -q '"level": "error"' "$EV/p-dry.out"
    check p-dry "$(verdict $?)" "the preview finds nothing to refuse"
    grep -q '"identity_clash"' "$EV/p-dry.out"
    check p-dry-clash "$(verdict $?)" "the preview warns about the duplicates"
    run p-ask "$RPC" apply "$WORK/plan.json"
    [[ $? -eq 3 ]]
    check p-ask "$(verdict $?)" "ApplyRestorePlan without a choice is refused"
    run p-apply "$RPC" apply "$WORK/plan.json" --keep
    check p-apply "$(verdict $?)" "ApplyRestorePlan with keep completes"
    grep -q '"kind": "verify_sectors"' "$EV/p-apply.out" && ! grep -q '"status": "failed"' "$EV/p-apply.out"
    check p-verified "$(verdict $?)" "every restored partition was read back and matched"
    local n bad=0
    for n in $SRC_PARTS; do same_partition "$SOURCE" "$TARGET" "$n" esp || bad=1; done
    # The ESP may differ by the fallback loader the plan put there.
    check p-data "$(verdict $bad)" "partitions identical to the source (an ESP may gain BOOTX64.EFI)"
    if [[ $UEFI -eq 1 ]]; then
        local esp mnt=$WORK/m-esp
        esp=$(part "$TARGET" "$(lsblk -nro PARTN,PARTTYPE "$TARGET" | awk 'tolower($2)=="c12a7328-f81f-11d2-ba4b-00a0c93ec93b"{print $1}')")
        mkdir -p "$mnt" && mount -o ro "$esp" "$mnt" && MOUNTS+=("$mnt")
        [[ -f $mnt/EFI/BOOT/BOOTX64.EFI ]]
        check p-fallback "$(verdict $?)" "the restored ESP has \\EFI\\BOOT\\BOOTX64.EFI"
        umount "$mnt"
        ls /var/lib/own-backup/boot/boot-*.json >/dev/null 2>&1
        check p-nvram-copy "$(verdict $?)" "a safety copy of the boot entries was taken first"
    else
        check p-fallback SKIP "BIOS run"
    fi
}

# ---------------------------------------------------------------------------
# volume: RestoreVolume of one partition's copy into the restored disk.
# ---------------------------------------------------------------------------
stage_volume() {
    local src_root tgt_root img=$COPIES/root-volume.qcow2
    src_root=$(lsblk -nrpo NAME,FSTYPE "$SOURCE" | awk '$2=="ext4"{print $1; exit}')
    tgt_root=$(part "$TARGET" "$(lsblk -nro PARTN "$src_root")")
    if [[ -z $src_root || -z $tgt_root ]]; then
        check v2-volume SKIP "no ext4 partition on the source, or the target has no table yet (run disk)"
        return
    fi
    rm -f "$img"
    run vol-copy "$CLI" backup "$src_root" -o "$img" --format qcow2
    check vol-copy "$(verdict $?)" "volume copy of $src_root"
    dd if=/dev/zero of="$tgt_root" bs=1M count=8 conv=fsync status=none
    run vol-ask "$RPC" restore-volume "$img" "$tgt_root"
    [[ $? -eq 3 ]] && grep -q '"kind": "ext"' "$EV/vol-ask.out"
    check vol-ask "$(verdict $?)" "the ext4 UUID duplicate needs a choice"
    run vol-keep "$RPC" restore-volume "$img" "$tgt_root" --keep
    check vol-keep "$(verdict $?)" "RestoreVolume with keep"
    e2fsck -fn "$tgt_root" >"$EV/vol-fsck.out" 2>&1
    check vol-fsck "$(verdict $?)" "e2fsck clean"
    cmp -s "$src_root" "$tgt_root"
    check vol-data "$(verdict $?)" "byte-identical to the source volume"
}

# ---------------------------------------------------------------------------
# mount: the same identities on a mounted copy.
# ---------------------------------------------------------------------------
stage_mount() {
    local img=$COPIES/root-volume.qcow2
    [[ -f $img ]] || { check m-ask SKIP "no volume copy (run volume)"; return; }
    run m-ask "$RPC" mount "$img"
    [[ $? -eq 3 ]] && grep -q '"kind": "ext"' "$EV/m-ask.out"
    check m-ask "$(verdict $?)" "MountImage asks about the duplicate UUID"
    run m-keep "$RPC" mount "$img" --keep
    local rc=$?
    check m-keep "$(verdict $rc)" "MountImage with keep"
    if [[ $rc -eq 0 ]]; then
        local at
        at=$(field m-keep "['mount_point']")
        [[ -f $at/etc/os-release ]]
        check m-data "$(verdict $?)" "the copy's files are there ($at)"
        run m-un "$RPC" unmount "$img"
        check m-un "$(verdict $?)" "unmounted"
    fi
}

# ---------------------------------------------------------------------------
# mbr (BIOS run): an MBR system restored onto its own disk keeps the 440
# bytes of boot code in LBA 0, and still boots (host: boot data-1 --bios).
# ---------------------------------------------------------------------------
stage_mbr() {
    if [[ $UEFI -eq 1 ]]; then
        check mbr SKIP "a BIOS run (linux-vm.py up --bios) is needed"
        return
    fi
    if ! command -v grub-install >/dev/null || [[ ! -d /usr/lib/grub/i386-pc ]]; then
        DEBIAN_FRONTEND=noninteractive apt-get install -y grub-pc-bin >"$EV/mbr-apt.out" 2>&1 \
            || { check mbr SKIP "grub-pc-bin not installable"; return; }
    fi
    local p1 mnt=$WORK/m-mbr img=$COPIES/mbr-root.qcow2
    wipefs -aq "$SOURCE"
    printf 'label: dos\nstart=2048, type=83, bootable\n' | sfdisk -q "$SOURCE"
    udevadm settle
    p1=$(part "$SOURCE" 1)
    mkfs.ext4 -q -F -L mbr-root "$p1"
    mkdir -p "$mnt" && mount "$p1" "$mnt" && MOUNTS+=("$mnt")
    rsync -aHAXx --exclude='/var/tmp/*' / "$mnt"/ >"$EV/mbr-rsync.out" 2>&1
    # /boot is a partition of its own in the cloud image: -x left it out, and
    # the system had no kernel (GRUB then listed only os-prober's entries).
    rsync -aHAXx --exclude='/efi/*' /boot/ "$mnt"/boot/ >>"$EV/mbr-rsync.out" 2>&1
    printf 'GRUB_DISABLE_OS_PROBER=true\nGRUB_TIMEOUT=3\nGRUB_TIMEOUT_STYLE=menu\n' \
        >"$mnt/etc/default/grub.d/99-obk-lab.cfg"
    local uuid
    uuid=$(blkid -s UUID -o value "$p1")
    printf 'UUID=%s / ext4 defaults 0 1\n' "$uuid" >"$mnt/etc/fstab"
    for d in dev proc sys; do mount --bind /$d "$mnt/$d"; done
    chroot "$mnt" grub-install --target=i386-pc "$(readlink -f "$SOURCE")" >"$EV/mbr-grub.out" 2>&1 \
        && chroot "$mnt" update-grub >>"$EV/mbr-grub.out" 2>&1
    local rc=$?
    for d in sys proc dev; do umount "$mnt/$d"; done
    umount "$mnt"
    check mbr-system "$(verdict $rc)" "an MBR BIOS system on the source disk"
    dd if="$SOURCE" bs=440 count=1 status=none | sha256sum >"$WORK/lba0.before"
    run mbr-copy "$CLI" backup "$p1" -o "$img" --format qcow2
    check mbr-copy "$(verdict $?)" "volume copy of its root"
    # Lose the filesystem, keep LBA 0 and the gap where GRUB's core lives.
    dd if=/dev/zero of="$p1" bs=1M count=16 conv=fsync status=none
    run mbr-plan "$RPC" plan-volume "$img" "$SOURCE" 1 "$WORK/plan-mbr.json"
    run mbr-apply "$RPC" apply "$WORK/plan-mbr.json"
    check mbr-apply "$(verdict $?)" "plan: MBR table + the root back into partition 1"
    dd if="$SOURCE" bs=440 count=1 status=none | sha256sum | cmp -s - "$WORK/lba0.before"
    check mbr-bootcode "$(verdict $?)" "the 440 bytes of boot code in LBA 0 survived the table write"
    e2fsck -fn "$p1" >"$EV/mbr-fsck.out" 2>&1
    check mbr-fsck "$(verdict $?)" "e2fsck clean; host: linux-restore-boot.py boot data-1.qcow2 --bios"
}

for s in "${STAGES[@]}"; do
    case $s in
        verify | guard | clash | disk | plan | volume | mount | mbr) "stage_$s" ;;
        *) echo "unknown stage $s" >&2; FAILS=$((FAILS + 1)) ;;
    esac
done
cp /var/log/own-backup/*.log "$EV/" 2>/dev/null
echo "FAILS=$FAILS (evidence: $EV)"
exit "$FAILS"
