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
#   verify guard clash disk plan volume mount fs stack change mbr
# (default: all but mbr; mbr belongs to a `linux-vm.py up --bios` run). `fs`
# lays its own partitions out on the target (format and grow ext4/XFS/btrfs);
# `stack` changes identities inside LUKS, LVM and md on the target; `change`
# restores the source again with its identities changed and its
# references fixed, and leaves the target that way for the host to boot. Each
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
#   (after `change`: the restored system, with new identities, boots; with
#   `--with data-1.qcow2` the source is attached beside it, as on the machine
#   the copy came from)
# BIOS/MBR: the same with `up --bios … ` and the stage `mbr`, then
#   linux-restore-boot.py boot data-1.qcow2 --bios
set -uo pipefail

RPC=$(readlink -f "${1:?RPC tool}")
CLI=$(readlink -f "${2:?CLI}")
shift 2
STAGES=("$@")
[[ ${#STAGES[@]} -eq 0 ]] && STAGES=(verify guard clash disk plan volume mount fs stack change)

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
    [[ -n ${STACK_LOOP:-} ]] && stack_down
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
copy_source() { # NAME: a whole-disk copy of the source into $DISK_IMG
    local boot=()
    [[ $UEFI -eq 1 ]] && boot=(--bootable)
    rm -f "$DISK_IMG"
    run "$1" "$CLI" backup-disk --device "$SOURCE" -o "$DISK_IMG" --format qcow2 "${boot[@]}"
    check "$1" "$(verdict $?)" "whole-disk copy of the source system ($SRC_PARTS)"
}
stage_verify() {
    copy_source v-copy
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
# fs: create and grow ext4/XFS/btrfs (and NTFS when ntfs-3g is there) in
# restore plans. The target gets a layout of its own: small partitions
# formatted by plans, copied, and put back into larger ones with the grow
# step.
# ---------------------------------------------------------------------------
fs_bytes() { # node: the filesystem's own size in bytes
    local node=$1
    case $(blkid -p -o value -s TYPE "$node") in
        ext*) dumpe2fs -h "$node" 2>/dev/null | awk -F: '/^Block count/{c=$2}/^Block size/{s=$2}END{print c*s}' ;;
        xfs) xfs_db -r -c 'sb 0' -c 'print dblocks blocksize' "$node" | awk '/dblocks/{d=$3}/blocksize/{b=$3}END{print d*b}' ;;
        btrfs) btrfs inspect-internal dump-super "$node" | awk '/^dev_item.total_bytes/{print $2}' ;;
        ntfs) ntfsresize --info --force --no-progress-bar "$node" 2>/dev/null | awk '/Current volume size/{print $4}' ;;
        *) echo 0 ;;
    esac
}
fills() { # node: the filesystem spans (nearly) its whole partition
    local fs dev
    fs=$(fs_bytes "$1")
    dev=$(blockdev --getsize64 "$1")
    echo "$1: filesystem $fs of $dev bytes" >>"$EV/fs-sizes.txt"
    [[ -n $fs && $fs -gt 0 ]] && ((fs * 100 >= dev * 97))
}
stage_fs() {
    [[ -f $DISK_IMG ]] || stage_verify
    local types=(ext4 xfs btrfs) n t p img rc mnt opts
    command -v mkntfs >/dev/null && types+=(ntfs)
    wipefs -aq "$TARGET"
    local args=(-o) i=1
    # 4 + 4 partitions on the stand's 4 GiB disk; XFS needs 300 MiB at least.
    for t in "${types[@]}"; do args+=(-n "$i:0:+320M"); i=$((i + 1)); done
    for t in "${types[@]}"; do args+=(-n "$i:0:+640M"); i=$((i + 1)); done
    sgdisk "${args[@]}" "$TARGET" >"$EV/fs-sgdisk.out" 2>&1
    reread "$TARGET"
    n=1
    for t in "${types[@]}"; do
        run "fs-plan-$t" "$RPC" plan-format "$DISK_IMG" "$TARGET" "$n" "$t" "$WORK/plan-fs-$t.json"
        run "fs-format-$t" "$RPC" apply "$WORK/plan-fs-$t.json"
        rc=$?
        p=$(part "$TARGET" "$n")
        [[ $rc -eq 0 && $(blkid -p -o value -s TYPE "$p") == "$t" ]]
        check "fs-format-$t" "$(verdict $?)" "a plan's Format step made $t on partition $n"
        n=$((n + 1))
    done
    # Something to find again after the grow.
    n=1
    for t in "${types[@]}"; do
        p=$(part "$TARGET" "$n")
        mnt=$WORK/m-fs-$t
        mkdir -p "$mnt" && mount "$p" "$mnt" && head -c 4M /dev/urandom >"$mnt/marker" \
            && sha256sum <"$mnt/marker" >"$WORK/marker-$t.sha" && umount "$mnt"
        img=$COPIES/fs-$t.qcow2
        rm -f "$img"
        run "fs-copy-$t" "$CLI" backup "$p" -o "$img" --format qcow2
        check "fs-copy-$t" "$(verdict $?)" "volume copy of the $t partition"
        n=$((n + 1))
    done
    local big=$n
    for t in "${types[@]}"; do
        img=$COPIES/fs-$t.qcow2
        run "fs-plan-grow-$t" "$RPC" plan-volume "$img" "$TARGET" "$big" "$WORK/plan-grow-$t.json" --extend
        # The copy duplicates the small partition it came from. Kept, so the
        # grow meets a duplicate: XFS mounts with nouuid, a btrfs clone needs
        # the kernel's temporary fsid (6.7+).
        run "fs-grow-$t" "$RPC" apply "$WORK/plan-grow-$t.json" --keep
        rc=$?
        p=$(part "$TARGET" "$big")
        if [[ $t == btrfs && $rc -ne 0 ]] && grep -q '6\.7' "$EV/fs-grow-$t.out"; then
            check "fs-grow-$t" SKIP "kernel older than 6.7: a btrfs clone is not grown (see the evidence)"
        else
            [[ $rc -eq 0 ]] && fills "$p"
            check "fs-grow-$t" "$(verdict $?)" "restored into a twice larger partition and grown to fill it"
        fi
        mnt=$WORK/m-fs-$t
        opts=ro
        [[ $t == xfs ]] && opts=ro,nouuid
        if mkdir -p "$mnt" && mount -o "$opts" "$p" "$mnt"; then
            sha256sum <"$mnt/marker" | cmp -s - "$WORK/marker-$t.sha"
            check "fs-grow-data-$t" "$(verdict $?)" "the grown $t still holds its file"
            umount "$mnt"
        else
            check "fs-grow-data-$t" FAIL "the grown $t does not mount"
        fi
        big=$((big + 1))
    done
    e2fsck -fn "$(part "$TARGET" "$n")" >"$EV/fs-fsck.out" 2>&1
    check fs-fsck "$(verdict $?)" "the grown ext4 checks clean"
    xfs_repair -n "$(part "$TARGET" $((n + 1)))" >"$EV/fs-xfs-repair.out" 2>&1
    check fs-xfs-repair "$(verdict $?)" "the grown XFS checks clean"
    btrfs check --readonly "$(part "$TARGET" $((n + 2)))" >"$EV/fs-btrfs-check.out" 2>&1
    check fs-btrfs-check "$(verdict $?)" "the grown btrfs checks clean"
}

# ---------------------------------------------------------------------------
# stack: identities inside containers — LUKS, an LVM volume group, an md
# RAID1 — restored onto the same machine with identity_choice change. The
# "machine's own" stack is a loop disk in the store, kept open and active the
# whole time (as a running system keeps its own); its whole-disk copy goes
# onto the target.
# ---------------------------------------------------------------------------
STACK_PW=obk-stack-pw
STACK_LOOP=
stack_down() { # leave nothing of the stack behind (also on failure)
    local vg
    for vg in obkstack obkstack_restored; do vgchange -an "$vg" >/dev/null 2>&1; done
    for md in /dev/md/obkstack /dev/md12[0-7]; do [[ -b $md ]] && mdadm --stop "$md" >/dev/null 2>&1; done
    [[ -b /dev/mapper/obkstack ]] && cryptsetup close obkstack
    [[ -n $STACK_LOOP ]] && losetup -d "$STACK_LOOP" 2>/dev/null
    STACK_LOOP=
}
stage_stack() {
    local need=()
    command -v cryptsetup >/dev/null || need+=(cryptsetup)
    command -v vgimportclone >/dev/null || need+=(lvm2)
    command -v mdadm >/dev/null || need+=(mdadm)
    if [[ ${#need[@]} -gt 0 ]]; then
        DEBIAN_FRONTEND=noninteractive apt-get install -y "${need[@]}" >"$EV/st-apt.out" 2>&1 \
            || { check stack SKIP "${need[*]} not installable"; return; }
    fi
    local img=$COPIES/stack.raw copy=$COPIES/stack-disk.qcow2 p n
    rm -f "$img" "$copy"
    truncate -s 1200M "$img"
    STACK_LOOP=$(losetup -fP --show "$img")
    sgdisk -o -n1:0:+256M -n2:0:+256M -t2:8e00 -n3:0:+256M -t3:fd00 -n4:0:+256M -t4:fd00 \
        "$STACK_LOOP" >"$EV/st-sgdisk.out" 2>&1
    reread "$STACK_LOOP"
    local l1 l2 l3 l4
    l1=$(part "$STACK_LOOP" 1) && l2=$(part "$STACK_LOOP" 2) && l3=$(part "$STACK_LOOP" 3) && l4=$(part "$STACK_LOOP" 4)
    {
        printf '%s' "$STACK_PW" | cryptsetup luksFormat -q --type luks2 --pbkdf pbkdf2 \
            --pbkdf-force-iterations 1000 --key-file=- "$l1" \
            && printf '%s' "$STACK_PW" | cryptsetup open --key-file=- "$l1" obkstack \
            && mkfs.ext4 -q -F -L st-luks /dev/mapper/obkstack \
            && pvcreate -q -y "$l2" && vgcreate -q obkstack "$l2" \
            && lvcreate -q -y -n data -L 128M obkstack && mkfs.ext4 -q -F -L st-lv /dev/obkstack/data \
            && mdadm --create /dev/md/obkstack --run --level=1 --raid-devices=2 --metadata=1.2 "$l3" "$l4" \
            && mkfs.ext4 -q -F -L st-md /dev/md/obkstack
    } >"$EV/st-make.out" 2>&1
    local rc=$?
    udevadm settle
    check st-make "$(verdict $rc)" "the machine's own LUKS→ext4, VG obkstack/data→ext4, RAID1→ext4 (open, active)"
    [[ $rc -eq 0 ]] || { stack_down; return; }
    local luks_old lv_old md_old mdfs_old pv_old
    luks_old=$(cryptsetup luksUUID "$l1")
    lv_old=$(blkid -p -o value -s UUID /dev/obkstack/data)
    pv_old=$(pvs --noheadings -o pv_uuid "$l2" | tr -d ' ')
    md_old=$(mdadm --examine --export "$l3" | sed -n 's/^MD_UUID=//p')
    mdfs_old=$(blkid -p -o value -s UUID /dev/md/obkstack)
    # A disk whose containers are open is refused (nothing holds it still):
    # copy it closed, as from the boot medium, then open it again — the
    # restore below must meet the machine's own LUKS, VG and array active.
    umount /dev/mapper/obkstack /dev/obkstack/data /dev/md/obkstack 2>/dev/null
    cryptsetup close obkstack && vgchange -q -an obkstack >/dev/null && mdadm --stop /dev/md/obkstack >/dev/null 2>&1
    udevadm settle
    run st-copy "$CLI" backup-disk --device "$STACK_LOOP" -o "$copy" --format qcow2
    check st-copy "$(verdict $?)" "whole-disk copy of the stack's disk (containers as they are on disk)"
    {
        printf '%s' "$STACK_PW" | cryptsetup open --key-file=- "$l1" obkstack \
            && vgchange -q -ay obkstack && mdadm --assemble /dev/md/obkstack "$l3" "$l4"
    } >>"$EV/st-make.out" 2>&1
    udevadm settle

    wipefs -aq "$TARGET"
    reread "$TARGET"
    run st-ask "$RPC" restore-disk "$copy" "$TARGET"
    [[ $? -eq 3 ]] && grep -q '"kind": "luks"' "$EV/st-ask.out" && grep -q '"kind": "lvm_' "$EV/st-ask.out" \
        && grep -q '"kind": "md"' "$EV/st-ask.out"
    check st-ask "$(verdict $?)" "the duplicates named: LUKS, LVM, md"
    run st-change "$RPC" restore-disk "$copy" "$TARGET" --change
    check st-change "$(verdict $?)" "RestoreDisk with identity_choice change"
    udevadm settle
    local t1 t2 t3 t4
    t1=$(part "$TARGET" 1) && t2=$(part "$TARGET" 2) && t3=$(part "$TARGET" 3) && t4=$(part "$TARGET" 4)
    [[ -n $t1 && $(cryptsetup luksUUID "$t1") != "$luks_old" ]]
    check st-luks "$(verdict $?)" "the restored LUKS container has a new UUID"
    grep -q 'luks_inside_unchecked\|не проверено\|was not checked' "$EV/st-change.out"
    check st-luks-note "$(verdict $?)" "…and the reply says what inside it could not be checked (no passphrase)"
    [[ -n $t2 && $(pvs --noheadings -o pv_uuid "$t2" 2>/dev/null | tr -d ' ') != "$pv_old" ]] \
        && [[ $(pvs --noheadings -o vg_name "$t2" 2>/dev/null | tr -d ' ') == obkstack_restored ]]
    check st-vg "$(verdict $?)" "the restored PV is new and its VG is obkstack_restored"
    vgchange -ay obkstack_restored >"$EV/st-vgchange.out" 2>&1
    udevadm settle
    [[ -b /dev/obkstack_restored/data && $(blkid -p -o value -s UUID /dev/obkstack_restored/data) != "$lv_old" ]]
    check st-lv-fs "$(verdict $?)" "the filesystem inside the restored LV has a new UUID"
    vgchange -an obkstack_restored >/dev/null 2>&1
    [[ -n $t3 && $(mdadm --examine --export "$t3" | sed -n 's/^MD_UUID=//p') != "$md_old" ]] \
        && [[ $(mdadm --examine --export "$t3" | sed -n 's/^MD_UUID=//p') == $(mdadm --examine --export "$t4" | sed -n 's/^MD_UUID=//p') ]]
    check st-md "$(verdict $?)" "both restored RAID members share a new array UUID"
    # The restored array keeps the md NAME (…:obkstack) — only its UUID is
    # new — so it cannot be told from the machine's own one by name, and
    # md127 is the machine's own. udev may have assembled the restored
    # members already once the restore's hold went away: take whatever holds
    # them, else assemble them under a name of our own.
    python3 -c "import json,sys;d=json.load(open(sys.argv[1]));sys.exit(0 if any(c.get('kind')=='ext' and '/dev/md' in c.get('device','') for c in d.get('identity_changes',[])) else 1)" "$EV/st-change.out"
    check st-md-fs-reply "$(verdict $?)" "the reply lists the filesystem inside the restored array as changed"
    local arr= holder
    holder=$(ls "/sys/class/block/$(basename "$t3")/holders" 2>/dev/null | head -1)
    if [[ -n $holder ]]; then
        arr=/dev/$holder
    elif mdadm --assemble /dev/md100 --run "$t3" "$t4" >"$EV/st-md-assemble.out" 2>&1; then
        arr=/dev/md100
    fi
    udevadm settle
    echo "array: $arr" >>"$EV/st-md-assemble.out"
    [[ -n $arr ]] && blkid -p "$arr" >>"$EV/st-md-assemble.out" 2>&1
    [[ -n $arr && -n $(blkid -p -o value -s UUID "$arr") && $(blkid -p -o value -s UUID "$arr") != "$mdfs_old" ]]
    check st-md-fs "$(verdict $?)" "the filesystem inside the restored array has a new UUID"
    [[ -n $arr ]] && mdadm --stop "$arr" >/dev/null 2>&1
    [[ -n $(cryptsetup luksUUID "$l1") && $(blkid -p -o value -s UUID /dev/obkstack/data) == "$lv_old" \
        && $(blkid -p -o value -s UUID /dev/md/obkstack) == "$mdfs_old" ]]
    check st-own "$(verdict $?)" "the machine's own stack is untouched and still open"
    [[ -z $(ls /run/udev/rules.d/59-own-backup-hold-* 2>/dev/null) && -z $(ls /dev/mapper/obk-reid-* 2>/dev/null) ]]
    check st-hold "$(verdict $?)" "no udev hold and no temporary device is left"
    stack_down
}

# ---------------------------------------------------------------------------
# change: the source restored again with identity_choice change. First
# RestoreDisk without fixing references (the system's files stay as they
# were), then the plan with fix_references — that one stays on the target for
# the host to boot (`linux-restore-boot.py boot data-2.qcow2`). Before the
# copy, the source's initramfs is made to keep its root's UUID (a resume
# device, as for a swap file on /), so the plan must also have the restored
# system's own tool rebuild that image (owner decision 13).
# ---------------------------------------------------------------------------
# What the disk holds, read from the kernel and the devices themselves (no
# udev database needed).
part_numbers() { # disk: its partition numbers
    local name p
    name=$(basename "$(readlink -f "$1")")
    for p in /sys/class/block/"$name"/"$name"*; do
        [[ -f $p/partition ]] && cat "$p/partition"
    done | sort -n
}
reread() { # disk: the kernel shows the table just written
    blockdev --rereadpt "$1" 2>/dev/null
    # A kernel without a parser for the table (a sandbox) learns it from
    # userspace instead.
    [[ -n $(part_numbers "$1") ]] || partx -a "$1" 2>/dev/null
    udevadm settle 2>/dev/null
    true
}
uuids_of() { # disk: "<partition> <TYPE> <UUID>" for every filesystem/LUKS on it
    local n p
    for n in $(part_numbers "$1"); do
        p=$(part "$1" "$n") || continue
        printf '%s %s %s\n' "$n" "$(blkid -p -o value -s TYPE "$p")" "$(blkid -p -o value -s UUID "$p")"
    done
}
partuuids_of() { # disk: "<partition> <PARTUUID>"
    local n p
    for n in $(part_numbers "$1"); do
        p=$(part "$1" "$n") || continue
        printf '%s %s\n' "$n" "$(blkid -p -o value -s PART_ENTRY_UUID "$p")"
    done
}
label_part() { # disk label: the number of its partition with that filesystem label
    local n p
    for n in $(part_numbers "$1"); do
        p=$(part "$1" "$n") || continue
        [[ $(blkid -p -o value -s LABEL "$p") == "$2" ]] && { echo "$n"; return; }
    done
    return 1
}
initrd_confs() { # image out: the configuration files an initramfs carries
    # (dracut's etc/cmdline.d, initramfs-tools' conf/conf.d), concatenated
    local img=$1 out=$2 f x=$WORK/initrd-x
    : >"$out"
    if command -v lsinitrd >/dev/null; then
        for f in $(lsinitrd "$img" 2>/dev/null | awk '{print $NF}' | grep -E '^(etc/cmdline\.d/|conf/conf\.d/)'); do
            lsinitrd -f "$f" "$img" >>"$out" 2>/dev/null
        done
    else
        rm -rf "$x" && mkdir -p "$x"
        unmkinitramfs "$img" "$x" >/dev/null 2>&1
        find "$x" -type f \( -path '*/conf/conf.d/*' -o -path '*/etc/cmdline.d/*' \) -exec cat {} + >>"$out"
        rm -rf "$x"
    fi
}
# The source's root and /boot partitions (the Ubuntu cloud image names them
# by label), set by source_resume.
SRC_ROOT_N=
SRC_BOOT_N=
source_resume() { # 0: the source was changed (copy it again); 2: it already was; else failed
    local root boot uuid mnt=$WORK/m-src rc conf=etc/dracut.conf.d/90-obk-lab-resume.conf d
    SRC_ROOT_N=$(label_part "$SOURCE" cloudimg-rootfs) && SRC_BOOT_N=$(label_part "$SOURCE" BOOT) || {
        check ch-src-resume FAIL "the source's root/BOOT partitions not found by label"
        return 1
    }
    root=$(part "$SOURCE" "$SRC_ROOT_N")
    boot=$(part "$SOURCE" "$SRC_BOOT_N")
    uuid=$(blkid -p -o value -s UUID "$root")
    mkdir -p "$mnt" && mount "$root" "$mnt" || return 1
    if grep -qs "resume=UUID=$uuid" "$mnt/$conf"; then
        umount "$mnt"
        check ch-src-resume PASS "the source's initramfs already keeps its root UUID (resume=UUID=$uuid)"
        return 2
    fi
    mount "$boot" "$mnt/boot" || { umount "$mnt"; return 1; }
    # dracut (Ubuntu 26.04) copies kernel_cmdline into the image;
    # initramfs-tools its resume configuration.
    printf 'kernel_cmdline="resume=UUID=%s"\n' "$uuid" >"$mnt/$conf"
    mkdir -p "$mnt/etc/initramfs-tools/conf.d"
    printf 'RESUME=UUID=%s\n' "$uuid" >"$mnt/etc/initramfs-tools/conf.d/resume"
    for d in dev proc sys; do mount --bind /$d "$mnt/$d"; done
    chroot "$mnt" update-initramfs -u -k all >"$EV/ch-src-initramfs.out" 2>&1
    rc=$?
    for d in sys proc dev; do umount "$mnt/$d"; done
    umount "$mnt/boot"
    umount "$mnt"
    check ch-src-resume "$(verdict $rc)" "the source's initramfs keeps its root UUID (resume=UUID=$uuid)"
    return $rc
}
initramfs_rebuilt() { # the restored image differs from the source's and names the new root UUID, not the old
    local tm=$WORK/m-tboot sm=$WORK/m-sboot img name new old n=0 bad=0
    new=$(awk -v n="$SRC_ROOT_N" '$1==n{print $3}' "$WORK/uuids.target")
    old=$(awk -v n="$SRC_ROOT_N" '$1==n{print $3}' "$WORK/uuids.source")
    [[ -n $new && -n $old && $new != "$old" ]] || return 1
    mkdir -p "$tm" "$sm"
    mount -o ro,noload "$(part "$TARGET" "$SRC_BOOT_N")" "$tm" || return 1
    mount -o ro,noload "$(part "$SOURCE" "$SRC_BOOT_N")" "$sm" || { umount "$tm"; return 1; }
    for img in "$tm"/initrd.img-* "$tm"/initramfs-*.img; do
        [[ -f $img && ! -L $img ]] || continue
        name=$(basename "$img")
        n=$((n + 1))
        cmp -s "$img" "$sm/$name" && { bad=1; echo "$name: the same as the source's" >>"$EV/ch-initramfs.txt"; }
        initrd_confs "$img" "$WORK/initrd.target.conf"
        cp "$WORK/initrd.target.conf" "$EV/ch-initramfs-$name.conf"
        grep -qi -- "$new" "$WORK/initrd.target.conf" || { bad=1; echo "$name: no $new" >>"$EV/ch-initramfs.txt"; }
        grep -qi -- "$old" "$WORK/initrd.target.conf" && { bad=1; echo "$name: still $old" >>"$EV/ch-initramfs.txt"; }
    done
    umount "$sm"
    umount "$tm"
    [[ $n -gt 0 && $bad -eq 0 ]]
}
initramfs_kernel_names() { # no rebuilt image on the restored /boot names this machine's block devices
    # by kernel name (/dev/vdb13, \x2fdev\x2fvdb13, dev-vdb13.device, /dev/block/252:29) where
    # it makes the boot wait or mount — the restored machine has other names
    # (dracut: "devexists-\x2fdev\x2fvdb13.sh" timeouts, emergency shell) —
    # and the rebuild left no copy of a previous image behind
    local tm=$WORK/m-tboot-k x=$WORK/initrd-k img name n=0 bad=0
    mkdir -p "$tm"
    mount -o ro,noload "$(part "$TARGET" "$SRC_BOOT_N")" "$tm" || return 1
    : >"$EV/ch-initramfs-names.txt"
    if compgen -G "$tm/*.obk-prev" >/dev/null; then
        bad=1
        ls -l "$tm"/*.obk-prev >>"$EV/ch-initramfs-names.txt"
    fi
    for img in "$tm"/initrd.img-* "$tm"/initramfs-*.img; do
        [[ -f $img && ! -L $img && $img != *.obk-prev ]] || continue
        name=$(basename "$img")
        n=$((n + 1))
        rm -rf "$x" && mkdir -p "$x"
        if command -v lsinitrd >/dev/null; then
            (cd "$x" && lsinitrd --unpack "$img") >/dev/null 2>&1
        else
            unmkinitramfs "$img" "$x" >/dev/null 2>&1
        fi
        [[ -n $(ls -A "$x") ]] || { bad=1; echo "$name: could not be unpacked" >>"$EV/ch-initramfs-names.txt"; continue; }
        python3 - "$x" "$name" >>"$EV/ch-initramfs-names.txt" <<'PY' || bad=1
import os, re, sys
root, image = sys.argv[1], sys.argv[2]
if os.path.isdir(os.path.join(root, "main")):  # unmkinitramfs: early/, main/
    root = os.path.join(root, "main")
devs = []
for n in os.listdir("/sys/class/block"):
    devs.append("/dev/" + n)
    try:
        devs.append("/dev/block/" + open("/sys/class/block/%s/dev" % n).read().strip())
    except OSError:
        pass
def unit(p):  # systemd-escape -p
    out = []
    for i, c in enumerate(p.strip("/")):
        if c == "/":
            out.append("-")
        elif c.isascii() and (c.isalnum() or c in ":_.") and not (c == "." and i == 0):
            out.append(c)
        else:
            out.append("".join("\\x%02x" % b for b in c.encode()))
    return "".join(out)
pats = []
for d in devs:
    for s, pre in ((d, ""), (d.replace("/", "\\x2f"), ""), (unit(d), r"(?<![A-Za-z0-9\\-])")):
        pats.append((d, re.compile(pre + re.escape(s) + r"(?![A-Za-z0-9])")))
def relevant(rel):
    return ("dracut/hooks/" in rel
            or rel.startswith(("etc/cmdline.d/", "etc/systemd/system/", "conf/", "cryptroot/"))
            or rel in ("etc/cmdline", "etc/fstab", "etc/crypttab", "etc/block_uuid.map"))
found = set()
for dp, dirs, files in os.walk(root):
    for f in files + [d for d in dirs if os.path.islink(os.path.join(dp, d))]:
        p = os.path.join(dp, f)
        rel = os.path.relpath(p, root)
        if not relevant(rel):
            continue
        if os.path.islink(p):
            text = os.readlink(p)
        else:
            with open(p, "rb") as fh:
                text = fh.read(1 << 20).decode("latin-1")
        found.update((rel, d) for d, rx in pats if rx.search(rel) or rx.search(text))
for rel, d in sorted(found):
    print("%s: %s names %s" % (image, rel, d))
sys.exit(1 if found else 0)
PY
    done
    rm -rf "$x"
    umount "$tm"
    [[ $n -gt 0 && $bad -eq 0 ]]
}
reply_counts() { # NAME: "<identity_changes> <edited_files>" of a JSON reply
    python3 -c "import json,sys;d=json.load(open(sys.argv[1]));print(len(d.get('identity_changes',[])),len(d.get('edited_files',[])))" "$EV/$1.out"
}
stage_change() {
    source_resume
    case $? in
        0) copy_source ch-copy ;;
        2) [[ -f $DISK_IMG ]] || copy_source ch-copy ;;
        *) [[ -f $DISK_IMG ]] || stage_verify ;;
    esac
    uuids_of "$SOURCE" >"$WORK/uuids.source"
    partuuids_of "$SOURCE" >"$WORK/partuuids.source"
    local changed edited p n mnt=$WORK/m-ch old bad=0 refs=0 f root

    wipefs -aq "$TARGET"
    reread "$TARGET"
    run ch-disk "$RPC" restore-disk "$DISK_IMG" "$TARGET" --change
    check ch-disk "$(verdict $?)" "RestoreDisk with identity_choice change"
    udevadm settle
    read -r changed edited < <(reply_counts ch-disk)
    [[ ${changed:-0} -gt 0 && ${edited:-1} -eq 0 ]]
    check ch-disk-report "$(verdict $?)" "the reply lists ${changed:-0} changed identities and no edited file (not asked)"
    ! grep -q 'пересобран\|was rebuilt' "$EV/ch-disk.out"
    check ch-disk-no-initramfs "$(verdict $?)" "no initramfs rebuild without fixed references"
    uuids_of "$TARGET" >"$WORK/uuids.disk"
    ! grep -Fxf <(awk '$3!=""' "$WORK/uuids.source") "$WORK/uuids.disk" >"$EV/ch-disk-same.txt"
    check ch-disk-new "$(verdict $?)" "no restored filesystem carries the source's UUID"

    wipefs -aq "$TARGET"
    reread "$TARGET"
    run ch-plan-make "$RPC" plan-disk "$DISK_IMG" "$TARGET" "$WORK/plan-change.json"
    run ch-apply "$RPC" apply "$WORK/plan-change.json" --change --fix-references
    check ch-apply "$(verdict $?)" "plan with change + fix_references completes"
    grep -q '"kind": "change_identities"' "$EV/ch-apply.out" && ! grep -q '"status": "failed"' "$EV/ch-apply.out"
    check ch-step "$(verdict $?)" "the plan ran its change_identities step"
    udevadm settle
    uuids_of "$TARGET" >"$WORK/uuids.target"
    ! grep -Fxf <(awk '$3!=""' "$WORK/uuids.source") "$WORK/uuids.target" >"$EV/ch-same.txt"
    check ch-uuids "$(verdict $?)" "every filesystem UUID (and the ESP serial) is new"
    [[ -s $WORK/partuuids.source && $(partuuids_of "$TARGET") == "$(cat "$WORK/partuuids.source")" ]]
    check ch-partuuid "$(verdict $?)" "PARTUUIDs are the copy's — never changed silently"
    for n in $(part_numbers "$TARGET"); do
        p=$(part "$TARGET" "$n")
        case $(blkid -p -o value -s TYPE "$p") in
            ext4 | vfat | xfs | btrfs) ;;
            *) continue ;;
        esac
        mkdir -p "$mnt" && mount -o ro "$p" "$mnt" 2>/dev/null || continue
        for f in "$mnt"/etc/fstab "$mnt"/etc/crypttab "$mnt"/boot/grub/grub.cfg "$mnt"/grub/grub.cfg "$mnt"/EFI/*/grub.cfg; do
            [[ -f $f ]] || continue
            refs=$((refs + 1))
            for old in $(awk '{print $3}' "$WORK/uuids.source"); do
                grep -qi -- "$old" "$f" && { bad=1; echo "$f still names $old" >>"$EV/ch-refs.txt"; }
            done
        done
        umount "$mnt"
    done
    [[ $bad -eq 0 && $refs -gt 0 ]]
    check ch-refs "$(verdict $?)" "fstab/crypttab/grub.cfg/ESP stubs ($refs files) name none of the old UUIDs"
    read -r changed edited < <(reply_counts ch-apply)
    [[ ${edited:-0} -gt 0 ]]
    check ch-edited "$(verdict $?)" "the run reports the ${edited:-0} files it edited"
    root=$(part "$TARGET" "$(awk '$2=="ext4"{print $1; exit}' "$WORK/uuids.target")")
    e2fsck -fn "$root" >"$EV/ch-fsck.out" 2>&1
    check ch-fsck "$(verdict $?)" "the restored root checks clean after its UUID change and edits"
    if [[ -n $SRC_ROOT_N && -n $SRC_BOOT_N ]]; then
        grep -q 'initramfs восстановленной системы пересобран (update-initramfs)\|initramfs was rebuilt (update-initramfs)' "$EV/ch-apply.out"
        check ch-initramfs-note "$(verdict $?)" "the plan step reports the restored system's initramfs rebuilt"
        initramfs_rebuilt
        check ch-initramfs "$(verdict $?)" "the restored /boot's initramfs is new and names the new root UUID, not the old"
        initramfs_kernel_names
        check ch-initramfs-names "$(verdict $?)" "the rebuilt image names no device by this machine's kernel name (it waits by UUID), no previous-image copy is left"
    else
        check ch-initramfs SKIP "the source's initramfs could not be prepared"
    fi
    [[ -z $(ls /run/udev/rules.d/59-own-backup-hold-* 2>/dev/null) && -z $(ls /dev/mapper/obk-reid-* 2>/dev/null) \
        && -z $(ls -d /run/own-backup/initramfs-* 2>/dev/null) ]] && ! grep -q ' /run/own-backup/' /proc/self/mounts
    check ch-hold "$(verdict $?)" "no udev hold, temporary mapping or rebuild mount is left"
    check ch-boot SKIP "host: linux-vm.py down; linux-restore-boot.py boot data-2.qcow2 [--with data-1.qcow2]"
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
        verify | guard | clash | disk | plan | volume | mount | fs | stack | change | mbr) "stage_$s" ;;
        *) echo "unknown stage $s" >&2; FAILS=$((FAILS + 1)) ;;
    esac
done
cp /var/log/own-backup/*.log "$EV/" 2>/dev/null
echo "FAILS=$FAILS (evidence: $EV)"
exit "$FAILS"
