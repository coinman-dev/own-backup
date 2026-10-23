#!/usr/bin/env bash
# First Linux acceptance scenario: a mounted logical volume is copied through an
# LVM snapshot (classic LV/ext4 -> qcow2, thin LV/xfs -> VHDX), each copy is
# restored onto a fresh LV and compared by sha256, and the refusals hold: a busy
# plain partition that cannot be held still for its copy (here: the copy would
# go onto it — a short one that can is linux-shortfreeze-scenario.sh's) is not
# copied and nothing is frozen, and restore will not write over a mounted LV,
# the root partition or the ESP the guest booted from, while an idle partition
# is accepted.
#
# Stage `pv` (whole disk with an LVM physical volume in use): data disk 1 gets
# a GPT with an ESP (vfat, mounted) and a PV whose VG holds `root` (ext4,
# mounted, written with fsync throughout the copy), `data` (xfs, mounted), an
# inactive `cold` (ext4) and free extents in the middle and at the end; the
# VG also lies on a second PV partition of the same disk, with `data` grown
# onto it and `tail` (ext4, active, unmounted) only there — the two PVs are
# held at one point in time, the second through the first one's snapshots. The
# disk is copied with `backup-disk` — every LV through its own device-mapper
# snapshot, all in one suspend, no `lvcreate` (the VG's seqno must not move) —
# verified, then an increment; the increment is restored onto data disk 3,
# whose clone of the VG is renamed with vgimportclone, and every filesystem is
# checked by its checker and its files by sha256. Also: a volume copy of the
# PV partition is refused, an LV created during the copy makes the copy fail
# and publish nothing, and a SIGKILL mid-copy is undone by cleanup-snapshots.
# Stage `pvroot` (opt-in): the guest's own system disk, when its root is on
# LVM (Ubuntu Server's default layout), copied whole onto data disk 2.
#
#   python3 -I scripts/lab/linux-vm.py up --disks 3 [--size 2]
#   scripts/lab/linux-snapshot-scenario.sh [lvm|pv|pvroot|all]   (default: all = lvm pv)
#
# The host part builds the release CLI, copies it and this file into the guest
# and runs `this-file guest <cli>` there as root. Only the data disks with the
# serials ob-data-1..3 are touched, and the guest is left as it was found.
# Evidence: ~/lab/linux/run/evidence/snapshot-<UTC>/ (guest/ holds the raw
# outputs, guest/summary.txt the verdicts). Exit 0 only when every check passed.
set -uo pipefail

# ---------------------------------------------------------------------------
# Guest part (root, inside the VM)
# ---------------------------------------------------------------------------

VG=obklab
# Stage pv: the VG on data disk 1's PV partition, and its restored clone.
PVG=obkpvlab
PV_RST=obkpvrst
LVM_TYPE=E6D6D379-F507-44C2-A23C-238F2A3DF928
# Stage pvroot with PVROOT_RESTORE=1 leaves its restored copy on data disk 3.
KEEP_D3=0
EV=/var/tmp/obk-evidence
FAILS=0
CHECKS=0

check() { # id verdict text...
    local id=$1 verdict=$2
    shift 2
    CHECKS=$((CHECKS + 1))
    [[ $verdict == FAIL ]] && FAILS=$((FAILS + 1))
    printf 'CHECK %-34s %-4s %s\n' "$id" "$verdict" "$*" | tee -a "$EV/summary.txt"
}

verdict() { # condition-exit-code -> PASS/FAIL
    if [[ $1 -eq 0 ]]; then echo PASS; else echo FAIL; fi
}

# The CLI colours its log lines even into a file; evidence is kept plain.
plain() { sed -i 's/\x1b\[[0-9;]*m//g' "$1"; }

# run NAME CMD...: output to $EV/NAME.out (and the console), exit code to NAME.rc.
run() {
    local name=$1
    shift
    echo "### $name: $*" >"$EV/$name.out"
    "$@" >>"$EV/$name.out" 2>&1
    local rc=$?
    echo "### exit $rc" >>"$EV/$name.out"
    plain "$EV/$name.out"
    sed 's/^/    /' "$EV/$name.out"
    echo "$rc" >"$EV/$name.rc"
    return "$rc"
}

part_of() { # disk-link partition-number -> canonical partition device
    local link="$1-part$2" i
    for i in $(seq 1 50); do
        [[ -b $link ]] && { readlink -f "$link"; return 0; }
        udevadm settle -t 2 2>/dev/null
        sleep 0.2
    done
    return 1
}

populate() { # dir: known files whose hashes the restore must reproduce
    local d=$1/data i
    mkdir -p "$d/nested/deeper" "$d/many" || return 1
    head -c 3M /dev/urandom >"$d/random-3m.bin" || return 1
    head -c 700K /dev/urandom >"$d/nested/random-700k.bin" || return 1
    seq 1 200000 >"$d/nested/deeper/seq.txt" || return 1
    printf 'Own Backup Linux lab — %s\n' "$1" >"$d/hello.txt" || return 1
    dd if=/dev/urandom of="$d/sparse.bin" bs=64K count=4 seek=64 status=none || return 1
    for i in $(seq 1 120); do head -c $((1024 + i * 37)) /dev/urandom >"$d/many/f$i"; done
    sync -f "$d/hello.txt"
}

hashes() { # mountpoint manifest
    (cd "$1" && find data -type f -print0 | sort -z | xargs -0 sha256sum) >"$2"
}

compare() { # mountpoint manifest
    (cd "$1" && sha256sum -c --quiet "$2")
}

guest_cleanup() {
    local m
    pkill -f 'obk-churn' 2>/dev/null
    # A run killed here leaves our snapshot layers (and a remounted ESP):
    # put every volume back before anything is unmounted or removed.
    if dmsetup ls 2>/dev/null | grep -qE '^obk-(real|snap)-' || [[ -n $(ls /run/own-backup/frozen 2>/dev/null) ]]; then
        [[ -x ${CLI:-} ]] && "$CLI" cleanup-snapshots >/dev/null 2>&1
    fi
    for m in /mnt/obk-rst-thin /mnt/obk-rst-classic /mnt/obk-idle /mnt/obk-busy \
        /mnt/obk-thin /mnt/obk-classic /mnt/obk-pv-rst /mnt/obk-pv-esp /mnt/obk-pv-root \
        /mnt/obk-pv-cold /mnt/obk-pv-data /mnt/obk-pv-tail /mnt/obk-images; do
        mountpoint -q "$m" && { umount "$m" || umount -l "$m"; }
        [[ -d $m ]] && rmdir "$m"
    done
    for g in "$PV_RST" "$PVG"; do
        if vgs "$g" >/dev/null 2>&1; then
            vgchange -an "$g" >/dev/null 2>&1
            vgremove -ff -y "$g" >/dev/null 2>&1
        fi
    done
    if [[ -n ${ESP_MOUNT:-} ]] && ! mountpoint -q "$ESP_MOUNT"; then
        mount "$ESP_MOUNT" || echo "WARNING: could not remount $ESP_MOUNT"
    fi
    if vgs "$VG" >/dev/null 2>&1; then
        vgchange -an "$VG" >/dev/null 2>&1
        vgremove -ff -y "$VG" >/dev/null 2>&1
    fi
    for d in "${D1:-}" "${D2:-}" "${D3:-}"; do
        [[ -b $d ]] || continue
        [[ $KEEP_D3 -eq 1 && $d == "${D3:-}" ]] && continue
        pvremove -ff -y "$d" "$d"-part* >/dev/null 2>&1
        wipefs -a -q "$d"-part* "$d" 2>/dev/null
    done
    udevadm settle -t 10 2>/dev/null
}

backup_lv() { # name lv mountpoint image [format args...]
    local name=$1 lv=$2 mnt=$3 image=$4
    shift 4
    local out=$EV/backup-$name.out

    # Watch the volume group while the copy runs, and keep writing to the
    # mounted source: it must stay writable (no freeze for the copy).
    (while :; do date +%T.%N; lvs --noheadings -o lv_name,lv_attr,origin,pool_lv "$VG"; sleep 0.2; done) \
        >"$EV/backup-$name.lvs-during.txt" 2>&1 &
    local watch=$!
    (for i in $(seq 1 8); do
        dd if=/dev/urandom of="$mnt/churn-$i.bin" bs=1M count=1 conv=fsync status=none || exit 1
    done) &
    local churn=$!

    local t0=$SECONDS
    RUST_LOG=info timeout 600 "$CLI" backup "$lv" -o "$image" "$@" >"$out" 2>&1
    local rc=$?
    local churn_rc
    wait "$churn"
    churn_rc=$?
    kill "$watch" 2>/dev/null
    wait "$watch" 2>/dev/null
    echo "### exit $rc after $((SECONDS - t0)) s; churn writer exit $churn_rc" >>"$out"
    plain "$out"
    sed 's/^/    /' "$out"

    check "backup-$name-exit" "$(verdict $rc)" "backup $lv -> $image $* (exit $rc)"
    grep -q 'LVM snapshot created' "$out"
    check "backup-$name-snapshot-taken" "$(verdict $?)" \
        "$(grep -o 'snapshot=[^ ]*' "$out" | head -1) $(grep -o 'thin=[a-z]*' "$out" | head -1)"
    local left
    left=$(lvs --noheadings -o vg_name,lv_name 2>/dev/null | grep -c 'obk-snap-')
    grep -q 'LVM snapshot removed' "$out" && [[ $left -eq 0 ]]
    check "backup-$name-snapshot-released" "$(verdict $?)" "obk-snap-* left in lvs: $left"
    grep -q '^Verify: OK' "$out" && [[ -s $image ]]
    check "backup-$name-verified" "$(verdict $?)" "$(grep '^Done:\|^Verify:' "$out" | tr '\n' ' ')"
    # Not a verdict: whether the copy knew which blocks the filesystem uses.
    check "backup-$name-used-map" INFO \
        "$(grep -o 'Using [a-z0-9]* used-block map\|no used-block map available[^—]*' "$out" | head -1)"
    findmnt -no OPTIONS "$mnt" | grep -qw rw && [[ $churn_rc -eq 0 ]]
    check "backup-$name-source-stayed-rw" "$(verdict $?)" \
        "source mounted rw throughout, 8 x 1 MiB fsync'd writes during the copy (exit $churn_rc)"
}

restore_lv() { # name image target mountpoint manifest fs mount-options
    local name=$1 image=$2 target=$3 mnt=$4 manifest=$5 fs=$6 opts=$7
    run "restore-$name" timeout 600 "$CLI" restore "$image" --target "$target" --yes
    local rc=$?
    check "restore-$name-exit" "$(verdict $rc)" "restore $image -> $target (exit $rc)"
    mkdir -p "$mnt"
    if mount -o "$opts" "$target" "$mnt" 2>"$EV/restore-$name.mount.err"; then
        compare "$mnt" "$manifest" >"$EV/restore-$name.compare.txt" 2>&1
        check "restore-$name-hashes" "$(verdict $?)" \
            "$(wc -l <"$manifest") files compared by sha256 ($(wc -l <"$EV/restore-$name.compare.txt") mismatch lines)"
        umount "$mnt"
    else
        check "restore-$name-hashes" FAIL "mount failed: $(cat "$EV/restore-$name.mount.err")"
    fi
    if [[ $fs == ext4 ]]; then
        run "restore-$name-fsck" e2fsck -fn "$target"
    else
        run "restore-$name-fsck" xfs_repair -n "$target"
    fi
    check "restore-$name-fsck-clean" "$(verdict $?)" "$fs read-only check of $target after mount/umount"
}

# refuse NAME TARGET IMAGE: restore must be refused by the guard. Run without
# --yes and with "no" on stdin, so even a guard that wrongly agreed would stop
# at the confirmation and write nothing.
refuse_restore() {
    local name=$1 target=$2 image=$3 pattern=$4
    local out=$EV/refuse-$name.out
    echo "### restore $image --target $target (no --yes; stdin: no)" >"$out"
    printf 'no\n' | timeout 60 "$CLI" restore "$image" --target "$target" >>"$out" 2>&1
    local rc=$?
    echo "### exit $rc" >>"$out"
    plain "$out"
    sed 's/^/    /' "$out"
    [[ $rc -ne 0 ]] && grep -Eq "$pattern" "$out" && ! grep -q 'Type YES' "$out"
    check "refuse-$name" "$(verdict $?)" "$target: $(grep -m1 '^Error:' "$out" | cut -c1-160)"
}

# ---------------------------------------------------------------------------
# Stage pv: a whole disk whose PV is in use
# ---------------------------------------------------------------------------

# Our layers, the loop devices of their stores, the stores themselves.
obk_leftovers() {
    dmsetup ls 2>/dev/null | grep -E '^obk-(real|snap)-' || true
    losetup -a 2>/dev/null | grep 'own-backup-cow-' || true
    find /mnt/obk-images -maxdepth 1 -name '.own-backup-cow-*' 2>/dev/null || true
}

# The live tables of the active LVs (no keys in an LV's table).
lv_tables() {
    local lv
    for lv in root data tail; do dmsetup table "$PVG-$lv" 2>&1; done | sha256sum | cut -d' ' -f1
}

vg_seqno() { vgs --noheadings -o vg_seqno "$PVG" 2>/dev/null | tr -d ' '; }

# Keep writing with fsync; one line per write: index, start, seconds, exit.
pv_churn() { # dir out-file
    mkdir -p "$1"
    exec -a obk-churn bash -c '
        dir=$1; out=$2; i=0
        while :; do
            t0=$(date +%s.%N)
            dd if=/dev/urandom of="$dir/c$((i % 40))" bs=64K count=4 conv=fsync status=none
            rc=$?
            t1=$(date +%s.%N)
            awk -v i="$i" -v a="$t0" -v b="$t1" -v rc="$rc" \
                "BEGIN { printf \"%d %s %.3f %d\n\", i, a, b - a, rc }" >>"$out"
            i=$((i + 1))
            sleep 0.05
        done' obk-churn "$1" "$2"
}

# Wait until both active LVs run on their snapshot-origin table, neither is
# suspended any more and both snapshots are live: the copy is past its switch.
pv_wait_switched() { # pid of the copy: no use waiting once it is gone
    local i lv ok
    for i in $(seq 1 6000); do
        kill -0 "$1" 2>/dev/null || return 1
        ok=1
        for lv in root data; do
            dmsetup table "$PVG-$lv" 2>/dev/null | grep -q ' snapshot-origin ' || ok=0
            dmsetup info -c --noheadings -o attr "$PVG-$lv" 2>/dev/null | grep -qv s || ok=0
        done
        [[ $ok -eq 1 && $(dmsetup status 2>/dev/null | grep -cE '^obk-snap-[0-9a-f]+: .* snapshot [0-9]+/') -ge 2 ]] &&
            return 0
        sleep 0.01
    done
    return 1
}

pv_make_disks() ( # make the PV disk's table and the images partition
    set -e
    local size pv_mib
    size=$(blockdev --getsize64 "$D1")
    # Room after the PV: the image adds its metadata partition past the
    # last partition, and the restore target is no larger than the source.
    pv_mib=$((size / 1048576 - 1 - 128 - 160 - 192))
    printf 'label: gpt\nsize=128MiB, type=U\nsize=%sMiB, type=%s\nsize=160MiB, type=%s\n' \
        "$pv_mib" "$LVM_TYPE" "$LVM_TYPE" | sfdisk -q "$D1"
    printf 'label: gpt\n,,L\n' | sfdisk -q "$D2"
    udevadm settle -t 10
)

pv_make_volumes() ( # esp pv images pv2: the VG, its LVs and filesystems, mounted
    set -e
    mkfs.vfat -F 32 -n OBKPVESP "$1"
    pvcreate -ff -y "$2" "$4"
    vgcreate "$PVG" "$2"
    lvcreate -y -Wy -Zy -L 600M -n root "$PVG"
    lvcreate -y -Wy -Zy -L 100M -n cold "$PVG"
    # A hole of free extents between cold and data.
    lvcreate -y -Wy -Zy -L 64M -n hole "$PVG"
    lvcreate -y -Wy -Zy -L 300M -n data "$PVG"
    lvremove -y "$PVG/hole"
    # The second PV: data grows onto it, tail lives only there.
    vgextend "$PVG" "$4"
    lvextend -y -L +64M "$PVG/data" "$4"
    lvcreate -y -Wy -Zy -L 48M -n tail "$PVG" "$4"
    mkfs.ext4 -q -L obk-pv-tail "/dev/$PVG/tail"
    mkfs.ext4 -q -L obk-pv-root "/dev/$PVG/root"
    mkfs.ext4 -q -L obk-pv-cold "/dev/$PVG/cold"
    mkfs.xfs -q -f -L obk-pv-data "/dev/$PVG/data"
    mkfs.ext4 -q -L obk-images "$3"
    mkdir -p /mnt/obk-pv-esp /mnt/obk-pv-root /mnt/obk-pv-cold /mnt/obk-pv-data /mnt/obk-images
    mount "$1" /mnt/obk-pv-esp
    mount "/dev/$PVG/root" /mnt/obk-pv-root
    mount "/dev/$PVG/cold" /mnt/obk-pv-cold
    mount "/dev/$PVG/data" /mnt/obk-pv-data
    mount "$3" /mnt/obk-images
    xfs_growfs /mnt/obk-pv-data
    lvs -a -o lv_name,lv_size,lv_active,seg_pe_ranges "$PVG"
    pvs -o pv_name,pv_size,pv_free,pe_start "$2" "$4"
)

pv_populate() ( # the known files, hashed; cold unmounted and deactivated
    set -e
    populate /mnt/obk-pv-root
    hashes /mnt/obk-pv-root "$EV/pv-root.sha256"
    populate /mnt/obk-pv-data
    hashes /mnt/obk-pv-data "$EV/pv-data.sha256"
    populate /mnt/obk-pv-cold
    hashes /mnt/obk-pv-cold "$EV/pv-cold.sha256"
    mkdir -p /mnt/obk-pv-esp/data/EFI/obk
    head -c 2M /dev/urandom >/mnt/obk-pv-esp/data/EFI/obk/grubx64.efi
    printf 'Own Backup PV lab ESP\n' >/mnt/obk-pv-esp/data/hello.txt
    hashes /mnt/obk-pv-esp "$EV/pv-esp.sha256"
    sync
    umount /mnt/obk-pv-cold
    lvchange -an "$PVG/cold"
    mkdir -p /mnt/obk-pv-tail
    mount "/dev/$PVG/tail" /mnt/obk-pv-tail
    populate /mnt/obk-pv-tail
    hashes /mnt/obk-pv-tail "$EV/pv-tail.sha256"
    umount /mnt/obk-pv-tail
    rmdir /mnt/obk-pv-tail
    lvs -a -o lv_name,lv_active "$PVG"
)

# pv_backup NAME IMAGE [cli args...]: the whole disk, with the root LV written
# to throughout. Leaves the verdicts that belong to every successful copy.
pv_backup() {
    local name=$1 image=$2
    shift 2
    local out=$EV/$name.out tables seqno
    tables=$(lv_tables)
    seqno=$(vg_seqno)
    : >"$EV/$name.churn.txt"
    pv_churn /mnt/obk-pv-root/churn "$EV/$name.churn.txt" &
    local churn_pid=$!
    sleep 1
    local t0=$SECONDS
    RUST_LOG=info timeout 1200 "$CLI" backup-disk --device "$D1" -o "$image" --format qcow2 --verify "$@" \
        >"$out" 2>&1
    local rc=$?
    sleep 1
    kill "$churn_pid" 2>/dev/null
    wait "$churn_pid" 2>/dev/null
    pkill -f obk-churn 2>/dev/null
    echo "### exit $rc after $((SECONDS - t0)) s" >>"$out"
    plain "$out"
    sed 's/^/    /' "$out"

    check "$name-exit" "$(verdict $rc)" "backup-disk $D1 -> $image $* (exit $rc)"
    [[ $(grep -c 'device-mapper snapshot created' "$out") -eq 3 ]] &&
        grep -q 'every logical volume of the physical volume is held by its snapshot' "$out"
    check "$name-pv-snapshots" "$(verdict $?)" \
        "root, data and tail (both PVs) held by device-mapper snapshots in one suspend: $(grep -o 'snapshot=obk-snap-[0-9a-f]*' "$out" | tr '\n' ' ')"
    ! grep -q 'LVM snapshot created' "$out" && [[ $(vg_seqno) == "$seqno" ]]
    check "$name-no-lvm-metadata-written" "$(verdict $?)" "no lvcreate; VG seqno $seqno before, $(vg_seqno) after"
    grep -q 'LVM physical volume: free extents are not copied' "$out"
    check "$name-free-extents-skipped" "$(verdict $?)" \
        "$(grep -o 'LVM physical volume prepared.*' "$out" | grep -o 'free_bytes=[0-9]*' | head -1)"
    grep -q "every partition's snapshot held to the end of the copy" "$out"
    check "$name-checked-before-publish" "$(verdict $?)" "every hold, the PV's included, checked at publication"
    grep -q '^Verify: OK' "$out" && [[ -s $image ]]
    check "$name-verified" "$(verdict $?)" "$(grep '^Done:\|^Verify:\|^Excluded\|^Exclusions' "$out" | tr '\n' ' ')"
    local left
    left=$(obk_leftovers)
    [[ $(grep -c 'device-mapper snapshot removed' "$out") -eq 3 && -z $left ]]
    check "$name-nothing-left" "$(verdict $?)" "all three snapshots removed; obk-*, store loops, stores: ${left:-none}"
    [[ $(lv_tables) == "$tables" ]]
    check "$name-tables-restored" "$(verdict $?)" "root, data and tail run on exactly their own tables again"
    ! grep -q 'snapshot of an interrupted backup\|снимок прерванного копирования' "$out"
    check "$name-second-pv-not-a-leftover" "$(verdict $?)" "the second PV, held by the first one's layers, is not refused"
    findmnt -no OPTIONS /mnt/obk-pv-esp | grep -qw rw
    check "$name-esp-rw-again" "$(verdict $?)" "the ESP, held read-only for its copy, is read-write again"
    local writes failed slowest
    writes=$(wc -l <"$EV/$name.churn.txt")
    failed=$(awk '$4 != 0' "$EV/$name.churn.txt" | wc -l)
    slowest=$(sort -k3 -g "$EV/$name.churn.txt" | tail -1 | awk '{print $3}')
    findmnt -no OPTIONS /mnt/obk-pv-root | grep -qw rw && [[ $writes -gt 0 && $failed -eq 0 ]]
    check "$name-root-stayed-rw" "$(verdict $?)" \
        "$writes fsync'd 256 KiB writes to root during the copy, $failed failed, slowest ${slowest:-?} s"
    return "$rc"
}

# A copy the PV changed under: an LV is created while the copy is stopped
# right after its switch. The copy must fail and publish nothing.
pv_changed_during_copy() {
    local out=$EV/pv-changed.out image=/mnt/obk-images/changed.qcow2 tables
    tables=$(lv_tables)
    RUST_LOG=info "$CLI" backup-disk --device "$D1" -o "$image" --format qcow2 >"$out" 2>&1 &
    local cli=$! seen=1
    pv_wait_switched "$cli" && { kill -STOP "$cli"; seen=0; }
    lvcreate -y -Wy -Zy -L 8M -n intruder "$PVG" >"$EV/pv-changed.lvcreate.out" 2>&1
    local made=$?
    kill -CONT "$cli" 2>/dev/null
    wait "$cli"
    local rc=$?
    echo "### exit $rc" >>"$out"
    plain "$out"
    sed 's/^/    /' "$out"
    [[ $seen -eq 0 && $made -eq 0 ]]
    check pv-changed-setup "$(verdict $?)" "copy stopped after its switch, intruder LV created (exit $made)"
    [[ $rc -ne 0 ]] && grep -Eq 'изменился во время копирования|changed during the copy' "$out" &&
        [[ ! -e $image ]] && ! ls -a /mnt/obk-images | grep -q '^\.own-backup-new-'
    check pv-changed-refused "$(verdict $?)" "exit $rc: $(grep -m1 -Eo '(Error|Ошибка).{0,160}' "$out")"
    local left
    left=$(obk_leftovers)
    [[ -z $left && $(lv_tables) == "$tables" ]]
    check pv-changed-released "$(verdict $?)" "snapshots released, tables restored: ${left:-nothing left}"
    lvremove -y "$PVG/intruder" >>"$EV/pv-changed.lvcreate.out" 2>&1
}

# SIGKILL mid-copy: the volumes stay writable, cleanup-snapshots undoes it all.
pv_kill() {
    local tables
    tables=$(lv_tables)
    : >"$EV/pv-kill.churn.txt"
    pv_churn /mnt/obk-pv-root/churn "$EV/pv-kill.churn.txt" &
    local churn_pid=$!
    RUST_LOG=info "$CLI" backup-disk --device "$D1" -o /mnt/obk-images/killed.qcow2 --format qcow2 \
        >"$EV/pv-kill-backup.out" 2>&1 &
    local cli=$! seen=1
    pv_wait_switched "$cli" && { kill -STOP "$cli"; seen=0; }
    {
        echo "=== CLI stopped after the switch (then SIGKILL)"
        dmsetup ls | sort
        dmsetup table | grep -E 'snapshot' | sort
        losetup -a | grep own-backup-cow-
        findmnt -no OPTIONS /mnt/obk-pv-esp
        ls -la /run/own-backup/frozen 2>&1
    } >"$EV/pv-kill-state.txt" 2>&1
    kill -9 "$cli" 2>/dev/null
    wait "$cli" 2>/dev/null
    plain "$EV/pv-kill-backup.out"
    [[ $seen -eq 0 && $(grep -cE '^obk-snap-' "$EV/pv-kill-state.txt") -ge 3 ]]
    check pv-kill-snapshots-in-place "$(verdict $?)" \
        "killed with $(grep -cE '^obk-(real|snap)-' "$EV/pv-kill-state.txt") obk-* devices in place"
    timeout 20 bash -c 'dd if=/dev/urandom of=/mnt/obk-pv-root/after-kill.bin bs=1M count=4 conv=fsync status=none &&
        dd if=/dev/urandom of=/mnt/obk-pv-data/after-kill.bin bs=1M count=4 conv=fsync status=none'
    check pv-kill-volumes-writable "$(verdict $?)" "root and data take 4 MiB fsync'd writes within 20 s of the SIGKILL"
    kill "$churn_pid" 2>/dev/null
    wait "$churn_pid" 2>/dev/null
    pkill -f obk-churn 2>/dev/null
    run pv-kill-cleanup env RUST_LOG=info timeout 120 "$CLI" cleanup-snapshots
    local rc=$? left
    left=$(obk_leftovers)
    [[ $rc -eq 0 && -z $left && $(lv_tables) == "$tables" ]]
    check pv-kill-cleanup "$(verdict $?)" \
        "exit $rc, $(grep -o 'removed: [0-9]*' "$EV/pv-kill-cleanup.out"); tables restored; left: ${left:-none}"
    findmnt -no OPTIONS /mnt/obk-pv-esp | grep -qw rw
    check pv-kill-esp-rw "$(verdict $?)" "the ESP the killed copy held read-only is read-write again"
    rm -f /mnt/obk-images/.own-backup-new-* /mnt/obk-pv-root/after-kill.bin /mnt/obk-pv-data/after-kill.bin
}

# Restore the image onto data disk 3, rename the clone's VG, check it all.
pv_restore() { # image
    local image=$1
    run pv-restore timeout 1200 "$CLI" restore "$image" --target "$D3" --yes
    local rc=$?
    check pv-restore-exit "$(verdict $rc)" "restore $image -> $D3 (exit $rc)"
    blockdev --rereadpt "$D3" 2>/dev/null
    udevadm settle -t 10
    local R_ESP R_PV R_PV2
    R_ESP=$(part_of "$D3" 1) && R_PV=$(part_of "$D3" 2) && R_PV2=$(part_of "$D3" 3)
    check pv-restore-partitions "$(verdict $?)" "ESP ${R_ESP:-?}, PVs ${R_PV:-?} ${R_PV2:-?} on the restored disk"
    [[ -n ${R_PV2:-} ]] || return
    # The clone carries the source's PV and VG UUIDs: renamed, it can be
    # activated beside the source.
    local import=(vgimportclone --basevgname "$PV_RST")
    if [[ $(lvmconfig --typeconfig full devices/use_devicesfile 2>/dev/null) == *1 ]]; then
        import+=(--importdevices)
    fi
    run pv-import "${import[@]}" "$R_PV" "$R_PV2"
    run pv-activate vgchange -ay "$PV_RST"
    rc=$?
    run pv-restored-lvs lvs -a -o lv_name,lv_size,seg_pe_ranges --units b "$PV_RST"
    lvs --noheadings -o lv_name,lv_size --units b "$PVG" | sort >"$EV/pv-lvs-source.txt"
    lvs --noheadings -o lv_name,lv_size --units b "$PV_RST" | sort >"$EV/pv-lvs-restored.txt"
    [[ $rc -eq 0 ]] && diff -q "$EV/pv-lvs-source.txt" "$EV/pv-lvs-restored.txt" >/dev/null
    check pv-restore-vg "$(verdict $?)" "the restored VG activates with the source's LVs and sizes"

    run pv-fsck-root e2fsck -fn "/dev/$PV_RST/root"
    check pv-fsck-root "$(verdict $?)" "ext4 root (copied while written) clean before any mount"
    run pv-fsck-cold e2fsck -fn "/dev/$PV_RST/cold"
    check pv-fsck-cold "$(verdict $?)" "ext4 cold (inactive, read from the partition) clean"
    run pv-fsck-tail e2fsck -fn "/dev/$PV_RST/tail"
    check pv-fsck-tail "$(verdict $?)" "ext4 tail (only on the second PV) clean"
    run pv-fsck-esp fsck.vfat -n "$R_ESP"
    check pv-fsck-esp "$(verdict $?)" "the ESP (held read-only for its copy) clean"
    run pv-fsck-data-before xfs_repair -n "/dev/$PV_RST/data"
    check pv-fsck-data-before INFO "xfs_repair -n before the first mount: exit $?"

    local lv mnt=/mnt/obk-pv-rst opts
    mkdir -p "$mnt"
    for lv in root data cold tail esp; do
        local dev=/dev/$PV_RST/$lv
        opts=ro
        [[ $lv == esp ]] && dev=$R_ESP
        # The source's XFS is mounted with the same UUID; its log replays on mount.
        [[ $lv == data ]] && opts=rw,nouuid
        if mount -o "$opts" "$dev" "$mnt" 2>"$EV/pv-mount-$lv.err"; then
            compare "$mnt" "$EV/pv-$lv.sha256" >"$EV/pv-compare-$lv.txt" 2>&1
            check "pv-restored-$lv-files" "$(verdict $?)" \
                "$(wc -l <"$EV/pv-$lv.sha256") files by sha256 ($(wc -l <"$EV/pv-compare-$lv.txt") mismatch lines)"
            if [[ $lv == root ]]; then
                [[ -f $mnt/data/after-full.txt ]]
                check pv-restored-root-increment "$(verdict $?)" "the file written between the full and the increment is there"
                check pv-restored-root-churn INFO "$(ls "$mnt/churn" 2>/dev/null | wc -l) churn files in the copy"
            fi
            umount "$mnt"
        else
            check "pv-restored-$lv-files" FAIL "mount -o $opts $dev: $(cat "$EV/pv-mount-$lv.err")"
        fi
    done
    run pv-fsck-data xfs_repair -n "/dev/$PV_RST/data"
    check pv-fsck-data "$(verdict $?)" "xfs data clean after mount/umount"
    vgchange -an "$PV_RST" >/dev/null 2>&1
    vgremove -ff -y "$PV_RST" >/dev/null 2>&1
}

pv_steps() {
    run pv-setup-disks pv_make_disks
    local rc=$? ESP1 PV1 PV2 P2
    ESP1=$(part_of "$D1" 1) && PV1=$(part_of "$D1" 2) && PV2=$(part_of "$D1" 3) &&
        P2=$(part_of "$D2" 1) || rc=1
    [[ $rc -eq 0 ]] && { run pv-setup-volumes pv_make_volumes "$ESP1" "$PV1" "$P2" "$PV2" || rc=1; }
    [[ $rc -eq 0 ]] && { run pv-setup-files pv_populate || rc=1; }
    check pv-setup "$(verdict $rc)" \
        "GPT on ob-data-1: ESP $ESP1 (vfat, mounted) + PVs $PV1 $PV2: root ext4 + data xfs (on both) mounted, cold inactive, tail on the second, free extents; images on $P2"
    [[ $rc -eq 0 ]] || return
    local IMG=/mnt/obk-images

    # A volume copy of the PV partition: refused, its LVs named.
    run pv-volume-refused timeout 120 "$CLI" backup "$PV1" -o "$IMG/pv-alone.qcow2" --format qcow2
    rc=$?
    [[ $rc -ne 0 ]] && grep -Eq 'физический том LVM|LVM physical volume' "$EV/pv-volume-refused.out" &&
        grep -q "$PVG/root" "$EV/pv-volume-refused.out" && [[ -z $(obk_leftovers) && ! -e $IMG/pv-alone.qcow2 ]]
    check pv-volume-copy-refused "$(verdict $?)" "$(grep -m1 -Eo '(Error|Ошибка).{0,160}' "$EV/pv-volume-refused.out")"

    pv_backup pv-full "$IMG/pvdisk.qcow2"
    local full_rc=$?
    run pv-full-image qemu-img info "$IMG/pvdisk.qcow2"

    # An increment of the disk: one more file on root.
    printf 'written between the full copy and the increment\n' >/mnt/obk-pv-root/data/after-full.txt
    sync -f /mnt/obk-pv-root/data/after-full.txt
    local restore_from=$IMG/pvdisk.qcow2
    if [[ $full_rc -eq 0 ]]; then
        pv_backup pv-inc "$IMG/pvdisk-inc.qcow2" --kind incremental --parent "$IMG/pvdisk.qcow2"
        if [[ $? -eq 0 ]]; then
            grep -q 'Incremental' "$EV/pv-inc.out"
            check pv-inc-is-a-child "$(verdict $?)" "$(grep '^Done:' "$EV/pv-inc.out")"
            restore_from=$IMG/pvdisk-inc.qcow2
        fi
    fi

    pv_changed_during_copy
    pv_kill
    if [[ $full_rc -eq 0 ]]; then
        pv_restore "$restore_from"
    else
        check pv-restore-exit SKIP "no image to restore"
    fi

    compare /mnt/obk-pv-root "$EV/pv-root.sha256" >/dev/null 2>&1 &&
        compare /mnt/obk-pv-data "$EV/pv-data.sha256" >/dev/null 2>&1
    check pv-sources-intact "$(verdict $?)" "root and data still match their hashes"
}

# Stage pvroot: the guest's own system disk, when its root is an LV.
pvroot_steps() {
    local root_src disk
    root_src=$(findmnt -no SOURCE /)
    disk=$(lsblk -nrso NAME,TYPE "$root_src" 2>/dev/null | awk '$2 == "disk" { print $1 }' | head -1)
    if [[ $(lsblk -no TYPE "$root_src" 2>/dev/null) != lvm || -z $disk ]]; then
        check pvroot SKIP "the root ($root_src) is not on LVM: nothing to show here"
        return
    fi
    run pvroot-layout lsblk -o NAME,SIZE,TYPE,FSTYPE,MOUNTPOINTS "/dev/$disk"
    run pvroot-images bash -c "set -e
        printf 'label: gpt\n,,L\n' | sfdisk -q $D2
        udevadm settle -t 10"
    local P2
    P2=$(part_of "$D2" 1) || { check pvroot-setup FAIL "no images partition"; return; }
    mkfs.ext4 -q -L obk-images "$P2" && mkdir -p /mnt/obk-images && mount "$P2" /mnt/obk-images
    check pvroot-setup "$(verdict $?)" "images on $P2; system disk /dev/$disk, root $root_src"
    local tables
    tables=$(dmsetup table 2>/dev/null | grep -v '^obk-' | sha256sum)
    run pvroot-backup env RUST_LOG=info timeout 3600 "$CLI" backup-disk --device "/dev/$disk" \
        -o /mnt/obk-images/system.qcow2 --format qcow2 --bootable --verify
    local rc=$?
    check pvroot-backup "$(verdict $rc)" "the running system's disk, root LV suspended for its snapshot (exit $rc)"
    grep -q '^Verify: OK' "$EV/pvroot-backup.out"
    check pvroot-verified "$(verdict $?)" "$(grep '^Done:\|^Verify:' "$EV/pvroot-backup.out" | tr '\n' ' ')"
    [[ -z $(obk_leftovers) && $(dmsetup table 2>/dev/null | grep -v '^obk-' | sha256sum) == "$tables" ]]
    check pvroot-released "$(verdict $?)" "no obk-* left, every table as before"
    timeout 10 touch /var/tmp/obk-alive && timeout 10 sync
    check pvroot-guest-responsive "$(verdict $?)" "root writable within 10 s after the copy"
    if [[ ${PVROOT_RESTORE:-0} == 1 ]]; then
        # Leaves a duplicate of the system's VG on data disk 3: shut the stand
        # down right after, and boot the copy alone
        # (linux-restore-boot.py boot data-3.qcow2).
        run pvroot-restore timeout 3600 "$CLI" restore /mnt/obk-images/system.qcow2 --target "$D3" --yes
        check pvroot-restore "$(verdict $?)" "the copy restored onto $D3 for a boot test"
        KEEP_D3=1
    fi
}

guest_steps() {
    local IN_USE='сейчас используется|is in use'

    run info-system bash -c 'uname -a; cat /etc/os-release; echo; lsblk -o NAME,SIZE,TYPE,FSTYPE,MOUNTPOINTS,PARTUUID,SERIAL; echo; findmnt -rno SOURCE,TARGET,FSTYPE,OPTIONS; echo; ls -l /dev/disk/by-id/; echo; [ -d /sys/firmware/efi ] && echo UEFI || echo BIOS; command -v efibootmgr >/dev/null && efibootmgr -v'
    run info-cli "$CLI" --version
    run info-backup-help "$CLI" backup --help
    run info-restore-help "$CLI" restore --help

    # --- a. LVM: classic LV (ext4) and a thin LV (xfs), both mounted, known files
    run setup-lvm bash -c "set -e
        pvcreate -ff -y $D1
        vgcreate $VG $D1
        lvcreate -y -Wy -Zy -L 256M -n classic $VG
        lvcreate -y -Wy -Zy -L 1G -T $VG/pool
        lvcreate -y -V 512M -T $VG/pool -n thin
        mkfs.ext4 -q -L obk-classic /dev/$VG/classic
        mkfs.xfs -q -f -L obk-thin /dev/$VG/thin
        mkdir -p /mnt/obk-classic /mnt/obk-thin
        mount /dev/$VG/classic /mnt/obk-classic
        mount /dev/$VG/thin /mnt/obk-thin
        lvs -a -o lv_name,lv_size,segtype,pool_lv,lv_attr $VG"
    local rc=$?
    if [[ $rc -eq 0 ]]; then
        populate /mnt/obk-classic && hashes /mnt/obk-classic "$EV/classic.sha256" &&
            populate /mnt/obk-thin && hashes /mnt/obk-thin "$EV/thin.sha256" && sync
        rc=$?
    fi
    check setup-lvm "$(verdict $rc)" "VG $VG on ob-data-1: classic 256M ext4 + thin 512M xfs in a 1G pool, mounted rw, files hashed"
    [[ $rc -eq 0 ]] || return

    # Images go to a filesystem on data disk 2; data disk 3 gets a busy and an idle partition.
    run setup-disks bash -c "set -e
        printf 'label: gpt\n,,L\n' | sfdisk -q $D2
        printf 'label: gpt\n,400M,L\n,,L\n' | sfdisk -q $D3
        udevadm settle -t 10"
    rc=$?
    local P2 BUSY IDLE
    P2=$(part_of "$D2" 1) && BUSY=$(part_of "$D3" 1) && IDLE=$(part_of "$D3" 2) || rc=1
    if [[ $rc -eq 0 ]]; then
        run setup-fs bash -c "set -e
            mkfs.ext4 -q -L obk-images $P2
            mkfs.ext4 -q -L obk-busy $BUSY
            mkfs.ext4 -q -L obk-idle $IDLE
            mkdir -p /mnt/obk-images /mnt/obk-busy /mnt/obk-idle
            mount $P2 /mnt/obk-images
            mount $BUSY /mnt/obk-busy
            echo busy >/mnt/obk-busy/marker.txt
            mount $IDLE /mnt/obk-idle"
        rc=$?
    fi
    if [[ $rc -eq 0 ]]; then
        populate /mnt/obk-idle && hashes /mnt/obk-idle "$EV/idle.sha256" && umount /mnt/obk-idle
        rc=$?
    fi
    check setup-disks "$(verdict $rc)" "images fs $P2, busy $BUSY (mounted rw), idle $IDLE (unmounted)"
    [[ $rc -eq 0 ]] || return
    local IMG=/mnt/obk-images

    # --- b. backups of the mounted LVs, through LVM snapshots
    backup_lv classic-qcow2 "/dev/$VG/classic" /mnt/obk-classic "$IMG/classic.qcow2" --format qcow2
    backup_lv thin-vhdx "/dev/$VG/thin" /mnt/obk-thin "$IMG/thin.vhdx"
    run images-after-backup bash -c "ls -la $IMG; for f in $IMG/classic.qcow2 $IMG/thin.vhdx; do printf '%s: ' \$f; head -c 8 \$f | od -An -c; done"
    [[ $(head -c 4 "$IMG/classic.qcow2" 2>/dev/null | od -An -tx1 | tr -d ' ') == 514649fb ]]
    check format-qcow2 "$(verdict $?)" "classic.qcow2 starts with QFI\\xfb"
    [[ $(head -c 8 "$IMG/thin.vhdx" 2>/dev/null) == vhdxfile ]]
    check format-vhdx "$(verdict $?)" "thin.vhdx starts with vhdxfile (the default format)"
    ! ls -a "$IMG" | grep -q '^\.own-backup-new-'
    check no-private-leftovers "$(verdict $?)" "no .own-backup-new-* names left in $IMG"

    # --- c. restore each copy onto a fresh, unmounted LV and compare
    run setup-restore-targets bash -c "set -e
        lvcreate -y -Wy -Zy -L 256M -n rst-classic $VG
        lvcreate -y -V 512M -T $VG/pool -n rst-thin"
    if [[ $? -eq 0 ]]; then
        restore_lv classic "$IMG/classic.qcow2" "/dev/$VG/rst-classic" /mnt/obk-rst-classic \
            "$EV/classic.sha256" ext4 rw
        # The origin is still mounted with the same XFS UUID.
        restore_lv thin "$IMG/thin.vhdx" "/dev/$VG/rst-thin" /mnt/obk-rst-thin \
            "$EV/thin.sha256" xfs rw,nouuid
    else
        check restore-targets FAIL "could not create the fresh LVs"
    fi

    # --- d. refusals
    # A mounted plain partition: no snapshot exists for it, and a short one is
    # held still for its copy only when nothing the copy writes is on it.
    local out=$EV/backup-busy.out t0=$SECONDS
    echo "### backup $BUSY (mounted rw at /mnt/obk-busy) onto itself" >"$out"
    timeout 120 "$CLI" backup "$BUSY" -o /mnt/obk-busy/busy.vhdx >>"$out" 2>&1
    rc=$?
    local took=$((SECONDS - t0))
    echo "### exit $rc after $took s" >>"$out"
    plain "$out"
    sed 's/^/    /' "$out"
    [[ $rc -ne 0 && $rc -ne 124 ]] && grep -Eq "$IN_USE" "$out" && grep -q '/mnt/obk-busy' "$out"
    check refuse-backup-busy-partition "$(verdict $?)" "exit $rc in $took s: $(grep -m1 '^Error:' "$out" | cut -c1-160)"
    ! ls -a "$IMG" /mnt/obk-busy | grep -q 'busy\.vhdx\|^\.own-backup-new-'
    check refuse-backup-left-nothing "$(verdict $?)" "no busy.vhdx and no private image in $IMG or /mnt/obk-busy"
    timeout 10 bash -c 'echo after >/mnt/obk-busy/after-refusal.txt && sync -f /mnt/obk-busy/after-refusal.txt' &&
        timeout 10 touch /var/tmp/obk-alive && timeout 10 sync
    check guest-responsive-after-refusal "$(verdict $?)" "busy fs and root writable within 10 s each after the refusal"

    refuse_restore mounted-lv "/dev/$VG/classic" "$IMG/classic.qcow2" "$IN_USE"
    compare /mnt/obk-classic "$EV/classic.sha256" >"$EV/classic-after-refusal.compare.txt" 2>&1
    check mounted-lv-untouched "$(verdict $?)" "the mounted classic LV still matches its hashes"

    local ROOT
    ROOT=$(findmnt -no SOURCE /)
    refuse_restore root-partition "$ROOT" "$IMG/classic.qcow2" "$IN_USE"

    local ESP=''
    ESP_MOUNT=''
    if mountpoint -q /boot/efi; then
        ESP_MOUNT=/boot/efi
        ESP=$(findmnt -no SOURCE /boot/efi)
    fi
    if [[ -z $ESP ]]; then
        check refuse-esp-mounted SKIP "no ESP mounted at /boot/efi"
    else
        refuse_restore esp-mounted "$ESP" "$IMG/classic.qcow2" "$IN_USE"
        if [[ -d /sys/firmware/efi ]]; then
            # Unmounted it looks idle; only the firmware's BootCurrent says it
            # is what the machine started from. Which refusal is right depends
            # on that entry: OVMF's own "UEFI Misc Device" (first boot of a
            # fresh VARS) names no partition, so the guard cannot tell and must
            # protect every ESP; shim's "Ubuntu" entry (later boots) carries
            # HD(...,GPT,<partuuid>) and must be recognised as this ESP.
            umount /boot/efi
            local KNOWN='системный раздел EFI, с которого загрузился|EFI System Partition this computer booted from'
            local UNSURE='Не удалось определить, с какого системного раздела EFI|Could not determine which EFI System Partition'
            refuse_restore esp-unmounted "$ESP" "$IMG/classic.qcow2" "$KNOWN|$UNSURE"
            local current entry
            current=$(efibootmgr 2>/dev/null | sed -n 's/^BootCurrent: *//p')
            entry=$(efibootmgr -v 2>/dev/null | grep -m1 "^Boot$current")
            echo "BootCurrent $current: $entry" >"$EV/bootcurrent.txt"
            if [[ -z $current ]]; then
                check esp-refusal-matches-bootcurrent SKIP "efibootmgr cannot read BootCurrent"
            elif [[ $entry == *"HD("*"$(blkid -s PARTUUID -o value "$ESP")"* ]]; then
                grep -Eq "$KNOWN" "$EV/refuse-esp-unmounted.out"
                check esp-refusal-matches-bootcurrent "$(verdict $?)" \
                    "Boot$current points at this ESP: the refusal must name it as the booted ESP"
            else
                grep -Eq "$UNSURE" "$EV/refuse-esp-unmounted.out"
                check esp-refusal-matches-bootcurrent "$(verdict $?)" \
                    "Boot$current names no partition (${entry:0:60}): the refusal must be the cannot-tell one"
            fi
            mount /boot/efi
            mountpoint -q /boot/efi
            check esp-remounted "$(verdict $?)" "/boot/efi mounted again"
        else
            check refuse-esp-unmounted SKIP "BIOS boot: no booted ESP to protect (by design)"
        fi
    fi

    # An unmounted, idle partition is a fair target (a throwaway image of itself).
    run backup-idle env RUST_LOG=info timeout 600 "$CLI" backup "$IDLE" -o "$IMG/idle.vhdx"
    rc=$?
    [[ $rc -eq 0 ]] && ! grep -q 'LVM snapshot created' "$EV/backup-idle.out"
    check backup-idle-partition "$(verdict $?)" "idle $IDLE copied directly, no snapshot (exit $rc)"
    mount "$IDLE" /mnt/obk-idle && echo 'written after the copy' >/mnt/obk-idle/data/after-backup.txt &&
        rm -f /mnt/obk-idle/data/hello.txt && umount /mnt/obk-idle
    run restore-idle timeout 600 "$CLI" restore "$IMG/idle.vhdx" --target "$IDLE" --yes
    rc=$?
    grep -q 'Restore complete' "$EV/restore-idle.out"
    check accept-restore-idle-partition "$(verdict $((rc + $?)))" "restore onto unmounted $IDLE accepted (exit $rc)"
    if mount "$IDLE" /mnt/obk-idle; then
        compare /mnt/obk-idle "$EV/idle.sha256" >"$EV/restore-idle.compare.txt" 2>&1 &&
            [[ ! -e /mnt/obk-idle/data/after-backup.txt ]]
        check restore-idle-content "$(verdict $?)" "files as at the copy; the later change is gone"
        umount /mnt/obk-idle
        run restore-idle-fsck e2fsck -fn "$IDLE"
        check restore-idle-fsck-clean "$(verdict $?)" "ext4 read-only check of $IDLE"
    else
        check restore-idle-content FAIL "restored $IDLE does not mount"
    fi

    # The sources were never disturbed.
    compare /mnt/obk-classic "$EV/classic.sha256" >/dev/null 2>&1 &&
        compare /mnt/obk-thin "$EV/thin.sha256" >/dev/null 2>&1
    check sources-intact-at-end "$(verdict $?)" "both mounted LVs still match their hashes"
}

guest_main() {
    CLI=$1
    local stages=${2:-all} stage
    [[ $stages == all ]] && stages='lvm pv'
    [[ $EUID -eq 0 ]] || { echo 'guest part needs root'; exit 2; }
    D1=/dev/disk/by-id/virtio-ob-data-1
    D2=/dev/disk/by-id/virtio-ob-data-2
    D3=/dev/disk/by-id/virtio-ob-data-3
    for d in "$D1" "$D2" "$D3"; do
        [[ -b $d ]] || { echo "missing $d: start the VM with --disks 3"; exit 2; }
    done
    chmod 0755 "$CLI"
    rm -rf "$EV"
    mkdir -p "$EV"
    : >"$EV/summary.txt"
    trap 'guest_cleanup; chmod -R a+rX "$EV"' EXIT
    trap 'exit 130' INT TERM
    guest_cleanup # leftovers of an interrupted earlier run

    for stage in $stages; do
        echo "== stage $stage" | tee -a "$EV/summary.txt"
        case $stage in
            lvm) guest_steps ;;
            pv) pv_steps ;;
            pvroot) pvroot_steps ;;
            *) check "stage-$stage" FAIL "no such stage (lvm, pv, pvroot, all)" ;;
        esac
        guest_cleanup
    done

    local wiped=("$D1" "$D2") kept=''
    if [[ $KEEP_D3 -eq 1 ]]; then kept=' (data disk 3 keeps the restored system)'; else wiped+=("$D3"); fi
    run after-cleanup bash -c "lsblk -o NAME,SIZE,TYPE,FSTYPE,MOUNTPOINTS $D1 $D2 $D3; vgs; lvs -a; dmsetup ls; findmnt /boot/efi"
    ! vgs "$VG" >/dev/null 2>&1 && ! vgs "$PVG" >/dev/null 2>&1 && ! vgs "$PV_RST" >/dev/null 2>&1 &&
        ! lsblk -rno NAME "${wiped[@]}" | grep -q '[0-9]$' &&
        ! lvs --noheadings 2>/dev/null | grep -q obk-snap- && [[ -z $(dmsetup ls 2>/dev/null | grep -E '^obk-') ]] &&
        { [[ -z ${ESP_MOUNT:-} ]] || mountpoint -q "$ESP_MOUNT"; }
    check guest-cleaned-up "$(verdict $?)" "no VG, no obk-* device, no partitions on the data disks$kept, ESP mounted as before"
    mkdir -p "$EV/cli-logs"
    cp -a /var/log/own-backup/. "$EV/cli-logs/" 2>/dev/null
    dmesg >"$EV/dmesg.txt" 2>&1
    if [[ $FAILS -eq 0 ]]; then
        echo "SCENARIO PASS ($CHECKS checks)" | tee -a "$EV/summary.txt"
    else
        echo "SCENARIO FAIL ($FAILS of $CHECKS checks failed)" | tee -a "$EV/summary.txt"
    fi
    [[ $FAILS -eq 0 ]]
}

if [[ ${1:-} == guest ]]; then
    guest_main "$2" "${3:-all}"
    exit $?
fi

# ---------------------------------------------------------------------------
# Host part
# ---------------------------------------------------------------------------

HERE=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
REPO=$(cd "$HERE/../.." && pwd)
VM=(python3 -I "$HERE/linux-vm.py")
RUN_DIR=$HOME/lab/linux/run
STAMP=$(date -u +%Y%m%dT%H%M%SZ)
HOST_EV=$RUN_DIR/evidence/snapshot-$STAMP
STAGES=${1:-all}
case $STAGES in
    lvm | pv | pvroot | all) ;;
    *) echo "usage: $0 [lvm|pv|pvroot|all]" >&2; exit 2 ;;
esac

die() {
    echo "[scenario] ERROR: $*" >&2
    exit 1
}

"${VM[@]}" status || die 'the VM is not running: python3 -I scripts/lab/linux-vm.py up --disks 3'
mkdir -p "$HOST_EV"

# Build tools that are not installed system-wide (PROTOC); a worktree uses the
# main checkout's ignored tmp/tools.
COMMON=$(git -C "$REPO" rev-parse --path-format=absolute --git-common-dir 2>/dev/null)
for env_sh in "$REPO/tmp/tools/env.sh" "${COMMON%/.git}/tmp/tools/env.sh"; do
    if [[ -f $env_sh ]]; then
        # shellcheck source=/dev/null
        . "$env_sh"
        break
    fi
done

"${VM[@]}" budget --step scenario-build || die 'space budget before the build'
# Watch the space while cargo writes (AGENTS.md: every 25-30 s).
(cd "$REPO" && exec cargo build --release -p own-backup-cli) >"$HOST_EV/build.log" 2>&1 &
build=$!
while kill -0 "$build" 2>/dev/null; do
    for _ in $(seq 1 28); do kill -0 "$build" 2>/dev/null || break; sleep 1; done
    if kill -0 "$build" 2>/dev/null && ! "${VM[@]}" budget --step scenario-build-watch; then
        kill "$build"
        die 'space budget exceeded during the build'
    fi
done
wait "$build" || { tail -30 "$HOST_EV/build.log"; die 'cargo build failed'; }
tail -2 "$HOST_EV/build.log"
CLI=${CARGO_TARGET_DIR:-$REPO/target}/release/own-backup-cli
[[ -x $CLI ]] || die "no $CLI"
sha256sum "$CLI" | tee "$HOST_EV/cli.sha256"
git -C "$REPO" log -1 --format='%H %s' >"$HOST_EV/commit.txt"

"${VM[@]}" ssh -- 'rm -rf /tmp/obk-scenario && mkdir -p /tmp/obk-scenario' || die 'guest unreachable'
"${VM[@]}" put "$CLI" /tmp/obk-scenario/own-backup-cli || die 'copy of the CLI failed'
"${VM[@]}" put "${BASH_SOURCE[0]}" /tmp/obk-scenario/scenario.sh || die 'copy of the scenario failed'

echo "[scenario] guest part starts; evidence: $HOST_EV"
"${VM[@]}" ssh -- "sudo env PVROOT_RESTORE=${PVROOT_RESTORE:-0} bash /tmp/obk-scenario/scenario.sh guest /tmp/obk-scenario/own-backup-cli $STAGES" \
    2>&1 | tee "$HOST_EV/guest-console.log"
rc=${PIPESTATUS[0]}

"${VM[@]}" get /var/tmp/obk-evidence "$HOST_EV/guest" || echo '[scenario] WARNING: evidence copy failed'
"${VM[@]}" ssh -- 'sudo rm -rf /var/tmp/obk-evidence /tmp/obk-scenario /var/tmp/obk-alive'
"${VM[@]}" budget --step scenario-done

echo
echo "[scenario] summary ($HOST_EV/guest/summary.txt):"
cat "$HOST_EV/guest/summary.txt" 2>/dev/null || echo '(no summary copied)'
echo "[scenario] guest exit $rc; evidence in $HOST_EV"
exit "$rc"
