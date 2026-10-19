#!/usr/bin/env bash
# Linux acceptance scenario for the device-mapper snapshot of an opened LUKS
# container (no LVM): btrfs, ext4 and XFS, each on its own LUKS2 mapping,
# mounted read-write and written to with fsync throughout the copy, are copied
# through obk-snap/obk-real built by the CLI itself; afterwards no obk-* device,
# loop device or store is left and the mapping has its own table back; each copy
# is restored and compared by sha256 and checked by its filesystem's checker.
# Refusals: no password for a kernel-only key, a store folder on the volume
# itself, a busy plain partition that cannot be held still (the copy would go
# onto it). Kill test: the CLI is killed with SIGKILL
# while the snapshot is in place; the volume must stay writable, and
# `own-backup-cli cleanup-snapshots` must put the mapping back.
#
#   python3 -I scripts/lab/linux-vm.py up --disks 3 [--size 4]
#   scripts/lab/linux-dm-snapshot-scenario.sh
#
# The host part builds the release CLI, copies it and this file into the guest
# and runs `this-file guest <cli>` there as root. Only the data disks with the
# serials ob-data-1..3 are touched, and the guest is left as it was found.
# Evidence: ~/lab/linux/run/evidence/dm-snapshot-<UTC>/ (guest/ holds the raw
# outputs, guest/summary.txt the verdicts). Keys never enter the evidence: a
# table with its key is only ever hashed. Exit 0 only when every check passed.
set -uo pipefail

# ---------------------------------------------------------------------------
# Guest part (root, inside the VM)
# ---------------------------------------------------------------------------

EV=/var/tmp/obk-evidence
FAILS=0
CHECKS=0
PASS=lab-luks-pass
IMG=/mnt/obk-images
MAPS=(obk-lab-btrfs obk-lab-ext4 obk-lab-xfs)
RST_MAP=obk-rst-btrfs

check() { # id verdict text...
    local id=$1 verdict=$2
    shift 2
    CHECKS=$((CHECKS + 1))
    [[ $verdict == FAIL ]] && FAILS=$((FAILS + 1))
    printf 'CHECK %-36s %-4s %s\n' "$id" "$verdict" "$*" | tee -a "$EV/summary.txt"
}

verdict() { if [[ $1 -eq 0 ]]; then echo PASS; else echo FAIL; fi; }

plain() { sed -i 's/\x1b\[[0-9;]*m//g' "$1"; }

run() { # NAME CMD...: output to $EV/NAME.out, exit code returned
    local name=$1
    shift
    echo "### $name: $*" >"$EV/$name.out"
    "$@" >>"$EV/$name.out" 2>&1
    local rc=$?
    echo "### exit $rc" >>"$EV/$name.out"
    plain "$EV/$name.out"
    sed 's/^/    /' "$EV/$name.out"
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

populate() { # dir big-MiB
    local d=$1/data i
    mkdir -p "$d/nested/deeper" "$d/many" || return 1
    head -c "${2}M" /dev/urandom >"$d/big.bin" || return 1
    head -c 3M /dev/urandom >"$d/random-3m.bin" || return 1
    seq 1 200000 >"$d/nested/deeper/seq.txt" || return 1
    printf 'Own Backup dm lab — %s\n' "$1" >"$d/hello.txt" || return 1
    dd if=/dev/urandom of="$d/sparse.bin" bs=64K count=4 seek=64 status=none || return 1
    for i in $(seq 1 120); do head -c $((1024 + i * 37)) /dev/urandom >"$d/many/f$i"; done
    sync -f "$d/hello.txt"
}

hashes() { (cd "$1" && find data -type f -print0 | sort -z | xargs -0 sha256sum) >"$2"; }
compare() { (cd "$1" && sha256sum -c --quiet "$2"); }

# The live table with its key, hashed — never printed.
table_hash() { dmsetup table --showkeys "$1" | sha256sum | cut -d' ' -f1; }

leftovers() { # what our runs may leave: obk-* devices, loops on stores, stores
    dmsetup ls 2>/dev/null | grep -E '^obk-(real|snap)-' || true
    losetup -a 2>/dev/null | grep 'own-backup-cow-' || true
    find "$IMG" -maxdepth 1 -name '.own-backup-cow-*' 2>/dev/null || true
}

guest_cleanup() {
    local m
    pkill -f 'obk-churn' 2>/dev/null
    for m in /mnt/obk-rst-btrfs /mnt/obk-rst-ext4 /mnt/obk-rst-xfs /mnt/obk-src-btrfs \
        /mnt/obk-src-ext4 /mnt/obk-src-xfs /mnt/obk-busy "$IMG"; do
        mountpoint -q "$m" && { umount "$m" || umount -l "$m"; }
        [[ -d $m ]] && rmdir "$m"
    done
    # A run killed here leaves our layers: put the mapping back before closing it.
    if dmsetup ls 2>/dev/null | grep -qE '^obk-(real|snap)-'; then
        [[ -x ${CLI:-} ]] && "$CLI" cleanup-snapshots >/dev/null 2>&1
    fi
    for m in "$RST_MAP" "${MAPS[@]}"; do
        [[ -e /dev/mapper/$m ]] && cryptsetup close "$m"
    done
    for d in "${D1:-}" "${D2:-}" "${D3:-}"; do
        [[ -b $d ]] || continue
        wipefs -a -q "$d"-part* "$d" 2>/dev/null
    done
    udevadm settle -t 10 2>/dev/null
}

# During a copy: what device-mapper, udev and btrfs see, every 0.3 s.
watch_stack() { # out-file fsid-or-empty
    local out=$1 fsid=$2
    while :; do
        {
            echo "=== $(date +%T.%N)"
            dmsetup ls 2>&1 | sort
            dmsetup status 2>&1 | grep -E '^obk-|snapshot' | sort
            dmsetup table 2>&1 | grep -E 'snapshot' | sort
            lsblk -rno NAME,KNAME,FSTYPE,UUID 2>&1 | grep -E 'obk-'
            losetup -a 2>&1 | grep 'own-backup-cow-'
            [[ -n $fsid ]] && echo "btrfs devices: $(ls /sys/fs/btrfs/"$fsid"/devices/ 2>&1 | tr '\n' ' ')"
        } >>"$out" 2>&1
        sleep 0.3
    done
}

# Keep writing with fsync; one line per write: index, start, seconds, exit.
churn() { # mountpoint out-file
    mkdir -p "$1/churn"
    exec -a obk-churn bash -c '
        dir=$1; out=$2; i=0
        while :; do
            t0=$(date +%s.%N)
            dd if=/dev/urandom of="$dir/c$((i % 50))" bs=64K count=4 conv=fsync status=none
            rc=$?
            t1=$(date +%s.%N)
            awk -v i="$i" -v a="$t0" -v b="$t1" -v rc="$rc" \
                "BEGIN { printf \"%d %s %.3f %d\n\", i, a, b - a, rc }" >>"$out"
            i=$((i + 1))
            sleep 0.05
        done' obk-churn "$1/churn" "$2"
}

# backup_dm NAME MAP MOUNTPOINT IMAGE FSID [cli args...]: copy a busy mapping
# through the device-mapper snapshot, with churn and the watcher running.
backup_dm() {
    local name=$1 map=$2 mnt=$3 image=$4 fsid=$5
    shift 5
    local out=$EV/backup-$name.out before
    before=$(table_hash "$map")
    : >"$EV/backup-$name.churn.txt"
    churn "$mnt" "$EV/backup-$name.churn.txt" &
    local churn_pid=$!
    watch_stack "$EV/backup-$name.during.txt" "$fsid" &
    local watch_pid=$!
    sleep 1

    local t0=$SECONDS
    RUST_LOG=info timeout 900 "$CLI" backup "/dev/mapper/$map" -o "$image" "$@" >"$out" 2>&1
    local rc=$?
    sleep 1
    kill "$churn_pid" "$watch_pid" 2>/dev/null
    wait "$churn_pid" "$watch_pid" 2>/dev/null
    pkill -f obk-churn 2>/dev/null
    echo "### exit $rc after $((SECONDS - t0)) s" >>"$out"
    plain "$out"
    sed 's/^/    /' "$out"

    check "backup-$name-exit" "$(verdict $rc)" "backup /dev/mapper/$map -> $image $* (exit $rc)"
    grep -q 'device-mapper snapshot created' "$out"
    check "backup-$name-dm-snapshot" "$(verdict $?)" \
        "$(grep -o 'snapshot=obk-snap-[0-9a-f]*' "$out" | head -1) $(grep -o 'device=/dev/dm-[0-9]*' "$out" | head -1)"
    grep -q '^Verify: OK' "$out" && [[ -s $image ]]
    check "backup-$name-verified" "$(verdict $?)" "$(grep '^Done:\|^Verify:' "$out" | tr '\n' ' ')"
    check "backup-$name-used-map" INFO \
        "$(grep -o 'Using [a-z0-9]* used-block map\|no used-block map available[^—]*' "$out" | head -1); $(grep -o '[0-9.]* MiB copied' "$out" | head -1)"
    local left
    left=$(leftovers)
    grep -q 'device-mapper snapshot removed' "$out" && [[ -z $left ]]
    check "backup-$name-nothing-left" "$(verdict $?)" "no obk-* device, store loop or store file: ${left:-none}"
    [[ $(table_hash "$map") == "$before" ]]
    check "backup-$name-table-restored" "$(verdict $?)" \
        "/dev/mapper/$map has exactly its own table again (sha256 of the table with key)"
    cryptsetup status "$map" >"$EV/backup-$name.status-after.txt" 2>&1
    # Churn: every write succeeded; the longest one shows the freeze.
    local writes failed slowest
    writes=$(wc -l <"$EV/backup-$name.churn.txt")
    failed=$(awk '$4 != 0' "$EV/backup-$name.churn.txt" | wc -l)
    slowest=$(sort -k3 -g "$EV/backup-$name.churn.txt" | tail -1 | awk '{print $3}')
    findmnt -no OPTIONS "$mnt" | grep -qw rw && [[ $writes -gt 0 && $failed -eq 0 ]]
    check "backup-$name-source-stayed-rw" "$(verdict $?)" \
        "$writes fsync'd 256 KiB writes during the copy, $failed failed, slowest ${slowest:-?} s"
    # udev never probed our layers: no filesystem type on any obk-* device.
    ! grep -E '^obk-(real|snap)-[0-9a-f]+ dm-[0-9]+ [a-z]' "$EV/backup-$name.during.txt" >/dev/null
    check "backup-$name-hidden-from-udev" "$(verdict $?)" \
        "lsblk during the copy: obk-* devices without FSTYPE ($(grep -c '^obk-snap' "$EV/backup-$name.during.txt") samples)"
}

restore_and_compare() { # name image target mountpoint manifest fs mount-options
    local name=$1 image=$2 target=$3 mnt=$4 manifest=$5 fs=$6 opts=$7
    run "restore-$name" timeout 900 "$CLI" restore "$image" --target "$target" --yes
    local rc=$?
    check "restore-$name-exit" "$(verdict $rc)" "restore $image -> $target (exit $rc)"
    # Before any mount: btrfs and ext4 copied from a frozen filesystem are
    # clean as they are (a mounted ext4 has needs_recovery set on disk; only
    # the freeze clears it).
    case $fs in
        btrfs) run "restore-$name-fsck" btrfs check --readonly "$target" ;;
        ext4) run "restore-$name-fsck" e2fsck -fn "$target" ;;
        xfs) run "restore-$name-fsck" xfs_repair -n "$target" ;;
    esac
    local rc=$?
    if [[ $fs == xfs ]]; then
        # A frozen XFS keeps a covered but not unmounted log: its copy needs a
        # log replay on mount — so does a plain copy made under xfs_freeze.
        # What counts is the check after the first mount.
        check "restore-$name-before-mount" INFO \
            "xfs_repair -n before the first mount: exit $rc$(grep -q 'valuable metadata changes' "$EV/restore-$name-fsck.out" && echo ', log to replay')"
    else
        check "restore-$name-fsck-clean" "$(verdict $rc)" "$fs read-only check of $target before any mount"
    fi
    mkdir -p "$mnt"
    if mount -o "$opts" "$target" "$mnt" 2>"$EV/restore-$name.mount.err"; then
        compare "$mnt" "$manifest" >"$EV/restore-$name.compare.txt" 2>&1
        check "restore-$name-hashes" "$(verdict $?)" \
            "$(wc -l <"$manifest") files compared by sha256 ($(wc -l <"$EV/restore-$name.compare.txt") mismatch lines)"
        # The same filesystem must also be writable after the restore.
        echo after-restore >"$mnt/after-restore.txt" && sync -f "$mnt/after-restore.txt"
        check "restore-$name-writable" "$(verdict $?)" "a file written and synced on the restored $fs"
        umount "$mnt"
    else
        check "restore-$name-hashes" FAIL "mount -o $opts failed: $(cat "$EV/restore-$name.mount.err")"
    fi
    if [[ $fs == xfs ]]; then
        run "restore-$name-fsck-after-mount" xfs_repair -n "$target"
        check "restore-$name-fsck-clean" "$(verdict $?)" "xfs read-only check of $target after mount/umount"
    fi
}

guest_steps() {
    run info-system bash -c 'uname -a; grep PRETTY /etc/os-release; cryptsetup --version; dmsetup version; btrfs --version; lsblk -o NAME,SIZE,TYPE,FSTYPE,MOUNTPOINTS'
    run info-cli "$CLI" --version
    run info-backup-help "$CLI" backup --help
    run info-cleanup-help "$CLI" cleanup-snapshots --help

    # --- setup: three LUKS2 mappings on data disk 1, images on 2, restore targets on 3
    run setup-disks bash -c "set -e
        for d in $D1 $D3; do printf 'label: gpt\n,1600M,L\n,1000M,L\n,,L\n' | sfdisk -q \$d; done
        printf 'label: gpt\n,,L\n' | sfdisk -q $D2
        udevadm settle -t 10"
    local rc=$?
    local i S=() R=()
    for i in 1 2 3; do
        S+=("$(part_of "$D1" "$i")") || rc=1
        R+=("$(part_of "$D3" "$i")") || rc=1
    done
    local P2
    P2=$(part_of "$D2" 1) || rc=1
    if [[ $rc -eq 0 ]]; then
        for i in 0 1 2; do
            printf '%s' "$PASS" | cryptsetup luksFormat --type luks2 --batch-mode --pbkdf pbkdf2 \
                --pbkdf-force-iterations 1000 "${S[$i]}" - >>"$EV/setup-luks.out" 2>&1 &&
                printf '%s' "$PASS" | cryptsetup open "${S[$i]}" "${MAPS[$i]}" - >>"$EV/setup-luks.out" 2>&1 || rc=1
        done
    fi
    if [[ $rc -eq 0 ]]; then
        run setup-fs bash -c "set -e
            mkfs.btrfs -q -f -L obk-src-btrfs /dev/mapper/${MAPS[0]}
            mkfs.ext4 -q -L obk-src-ext4 /dev/mapper/${MAPS[1]}
            mkfs.xfs -q -f -L obk-src-xfs /dev/mapper/${MAPS[2]}
            mkfs.ext4 -q -L obk-images $P2
            mkdir -p /mnt/obk-src-btrfs /mnt/obk-src-ext4 /mnt/obk-src-xfs $IMG
            mount /dev/mapper/${MAPS[0]} /mnt/obk-src-btrfs
            mount /dev/mapper/${MAPS[1]} /mnt/obk-src-ext4
            mount /dev/mapper/${MAPS[2]} /mnt/obk-src-xfs
            mount $P2 $IMG"
        rc=$?
    fi
    if [[ $rc -eq 0 ]]; then
        populate /mnt/obk-src-btrfs 300 && hashes /mnt/obk-src-btrfs "$EV/btrfs.sha256" &&
            populate /mnt/obk-src-ext4 20 && hashes /mnt/obk-src-ext4 "$EV/ext4.sha256" &&
            populate /mnt/obk-src-xfs 20 && hashes /mnt/obk-src-xfs "$EV/xfs.sha256" && sync
        rc=$?
    fi
    for i in 0 1 2; do
        dmsetup table "${MAPS[$i]}" >>"$EV/tables-before.txt" 2>&1 # dmsetup masks keys
        cryptsetup status "${MAPS[$i]}" >>"$EV/status-before.txt" 2>&1
    done
    check setup "$(verdict $rc)" "LUKS2 btrfs/ext4/xfs on ob-data-1 mounted rw, images ext4 on ob-data-2, files hashed"
    [[ $rc -eq 0 ]] || return
    grep -q 'key location: keyring' "$EV/status-before.txt"
    check setup-key-in-kernel-only "$(verdict $?)" \
        "cryptsetup keeps the volume key in the kernel keyring (the case that needs the password)"
    local FSID
    FSID=$(btrfs filesystem show /mnt/obk-src-btrfs 2>/dev/null | sed -n 's/.*uuid: *//p' | head -1)
    local SRC_DM
    SRC_DM=$(basename "$(readlink -f /dev/mapper/${MAPS[0]})")
    echo "btrfs fsid $FSID on $SRC_DM" >"$EV/btrfs-identity.txt"

    # --- a. refusals that must leave nothing behind and change nothing
    local IN_KEY='хранится только в ядре|held only by the kernel'
    local before out
    before=$(table_hash "${MAPS[0]}")
    out=$EV/refuse-no-password.out
    timeout 120 "$CLI" backup "/dev/mapper/${MAPS[0]}" -o "$IMG/nopass.vhdx" >"$out" 2>&1
    rc=$?
    plain "$out"
    sed 's/^/    /' "$out"
    [[ $rc -ne 0 && $rc -ne 124 ]] && grep -Eq "$IN_KEY" "$out" && [[ -z $(leftovers) ]] &&
        [[ $(table_hash "${MAPS[0]}") == "$before" ]] && [[ ! -e $IMG/nopass.vhdx ]]
    check refuse-key-without-password "$(verdict $?)" "exit $rc: $(grep -m1 'Error' "$out" | cut -c1-150)"
    timeout 10 bash -c "echo alive >/mnt/obk-src-btrfs/after-refusal.txt && sync -f /mnt/obk-src-btrfs/after-refusal.txt"
    check refuse-key-volume-writable "$(verdict $?)" "the volume is writable within 10 s after the refusal"

    out=$EV/refuse-store-on-source.out
    timeout 120 "$CLI" backup "/dev/mapper/${MAPS[0]}" -o /mnt/obk-src-btrfs/self.vhdx --password "$PASS" >"$out" 2>&1
    rc=$?
    plain "$out"
    sed 's/^/    /' "$out"
    [[ $rc -ne 0 ]] && grep -Eq 'находится на копируемом томе|is on the volume being copied' "$out" &&
        [[ -z $(dmsetup ls | grep -E '^obk-(real|snap)-') ]] && ! ls -a /mnt/obk-src-btrfs | grep -q 'own-backup-cow-'
    check refuse-store-on-source "$(verdict $?)" "exit $rc: $(grep -m1 'Error' "$out" | cut -c1-150)"

    run busy-partition-mount bash -c "mkdir -p /mnt/obk-busy && mount ${R[1]} /mnt/obk-busy 2>/dev/null || (mkfs.ext4 -q ${R[1]} && mount ${R[1]} /mnt/obk-busy)"
    out=$EV/refuse-busy-partition.out
    # Short enough to be held still for its copy, were the copy not going onto it.
    timeout 120 "$CLI" backup "${R[1]}" -o /mnt/obk-busy/busy.vhdx >"$out" 2>&1
    rc=$?
    plain "$out"
    [[ $rc -ne 0 ]] && grep -Eq 'сейчас используется|is in use' "$out" && [[ -z $(dmsetup ls | grep -E '^obk-(real|snap)-') ]]
    check refuse-busy-plain-partition "$(verdict $?)" "exit $rc: $(grep -m1 'Error' "$out" | cut -c1-150)"
    umount /mnt/obk-busy

    # --- b. btrfs on LUKS through the snapshot, restored onto another LUKS mapping
    backup_dm btrfs "${MAPS[0]}" /mnt/obk-src-btrfs "$IMG/btrfs.vhdx" "$FSID" --password "$PASS"
    local devs
    devs=$(grep -o 'btrfs devices: .*' "$EV/backup-btrfs.during.txt" | sort -u | tr '\n' ';')
    [[ -n $devs ]] && ! grep 'btrfs devices:' "$EV/backup-btrfs.during.txt" | grep -qv "btrfs devices: $SRC_DM *$"
    check btrfs-keeps-the-origin "$(verdict $?)" "the mounted btrfs used only $SRC_DM throughout: $devs"
    findmnt -no SOURCE /mnt/obk-src-btrfs >"$EV/btrfs-source-after.txt"
    grep -q "/dev/mapper/${MAPS[0]}" "$EV/btrfs-source-after.txt"
    check btrfs-mount-source-unchanged "$(verdict $?)" "findmnt: $(cat "$EV/btrfs-source-after.txt")"

    run setup-restore-btrfs bash -c "set -e
        printf '%s' '$PASS' | cryptsetup luksFormat --type luks2 --batch-mode --pbkdf pbkdf2 --pbkdf-force-iterations 1000 ${R[0]} -
        printf '%s' '$PASS' | cryptsetup open ${R[0]} $RST_MAP -"
    # The copy keeps the source's btrfs fsid; kernel 6.7+ mounts such a
    # single-device clone under a temporary fsid.
    restore_and_compare btrfs "$IMG/btrfs.vhdx" "/dev/mapper/$RST_MAP" /mnt/obk-rst-btrfs \
        "$EV/btrfs.sha256" btrfs rw
    cryptsetup close "$RST_MAP"

    # --- c. ext4 and XFS on LUKS
    backup_dm ext4 "${MAPS[1]}" /mnt/obk-src-ext4 "$IMG/ext4.qcow2" "" --password "$PASS" --format qcow2
    restore_and_compare ext4 "$IMG/ext4.qcow2" "${R[1]}" /mnt/obk-rst-ext4 "$EV/ext4.sha256" ext4 rw
    backup_dm xfs "${MAPS[2]}" /mnt/obk-src-xfs "$IMG/xfs.qcow2" "" --password "$PASS" --format qcow2
    restore_and_compare xfs "$IMG/xfs.qcow2" "${R[2]}" /mnt/obk-rst-xfs "$EV/xfs.sha256" xfs rw,nouuid
    rm -f "$IMG/ext4.qcow2" "$IMG/xfs.qcow2"

    # --- d. kill test: SIGKILL while the snapshot is in place
    local map=${MAPS[0]} mnt=/mnt/obk-src-btrfs
    before=$(table_hash "$map")
    : >"$EV/kill.churn.txt"
    churn "$mnt" "$EV/kill.churn.txt" &
    local churn_pid=$!
    RUST_LOG=info "$CLI" backup "/dev/mapper/$map" -o "$IMG/killed.vhdx" --password "$PASS" >"$EV/kill-backup.out" 2>&1 &
    local cli_pid=$! seen=1
    # Stop it the moment the switch is complete — the volume runs on
    # snapshot-origin again (not suspended) and obk-snap is active — so the
    # SIGKILL lands mid-copy, never inside the suspend window.
    for i in $(seq 1 3000); do
        if dmsetup table "$map" 2>/dev/null | grep -q ' snapshot-origin ' &&
            dmsetup info -c --noheadings -o attr "$map" 2>/dev/null | grep -qv s &&
            dmsetup status 2>/dev/null | grep -qE '^obk-snap-[0-9a-f]+: .* snapshot [0-9]+/'; then
            kill -STOP "$cli_pid"
            seen=0
            break
        fi
        sleep 0.01
    done
    {
        echo "=== CLI stopped mid-copy (then SIGKILL)"
        dmsetup ls | sort
        dmsetup info -c "$map"
        dmsetup status | grep -E '^obk-|snapshot' | sort
        dmsetup table | grep -E 'snapshot' | sort
        losetup -a | grep own-backup-cow-
        ls -la "$IMG"
    } >"$EV/kill-state.txt" 2>&1
    kill -9 "$cli_pid" 2>/dev/null
    wait "$cli_pid" 2>/dev/null
    plain "$EV/kill-backup.out"
    grep -q 'device-mapper snapshot created' "$EV/kill-backup.out" &&
        ! grep -q 'snapshot removed\|^Done:' "$EV/kill-backup.out" &&
        [[ $(grep -cE '^obk-(real|snap)-' "$EV/kill-state.txt") -ge 2 ]]
    check kill-snapshot-was-in-place "$(verdict $((seen + $?)))" \
        "killed mid-copy with the snapshot switched in: $(grep -cE '^obk-(real|snap)-[0-9a-f]+\s' "$EV/kill-state.txt") obk-* devices, $(grep -c own-backup-cow- "$EV/kill-state.txt") store lines"
    timeout 20 bash -c "dd if=/dev/urandom of=$mnt/after-kill.bin bs=1M count=4 conv=fsync status=none && sync -f $mnt/after-kill.bin"
    rc=$?
    check kill-volume-still-writable "$(verdict $rc)" \
        "4 MiB written with fsync to the volume within 20 s after SIGKILL (exit $rc)"
    sleep 1
    kill "$churn_pid" 2>/dev/null
    wait "$churn_pid" 2>/dev/null
    pkill -f obk-churn 2>/dev/null
    local failed
    failed=$(awk '$4 != 0' "$EV/kill.churn.txt" | wc -l)
    check kill-churn-kept-writing "$(verdict "$failed")" \
        "$(wc -l <"$EV/kill.churn.txt") writes around the kill, $failed failed"

    run kill-cleanup env RUST_LOG=info timeout 120 "$CLI" cleanup-snapshots
    rc=$?
    local left
    left=$(leftovers)
    [[ $rc -eq 0 && -z $left ]] && grep -q 'removed: 1' "$EV/kill-cleanup.out"
    check kill-cleanup-removed-everything "$(verdict $?)" "exit $rc; left: ${left:-none}; $(grep -o 'removed: [0-9]*' "$EV/kill-cleanup.out")"
    # The mapping runs on a crypt table of the same container again: the key
    # is now given inline (the run recovered it from the password; the keyring
    # key it named is unreachable), everything else is the original.
    local now orig_rest now_rest
    now=$(dmsetup table --showkeys "$map")
    if [[ $(table_hash "$map") == "$before" ]]; then
        check kill-cleanup-table-restored PASS "exactly the original table"
    else
        orig_rest=$(grep 'crypt' "$EV/tables-before.txt" | head -1 | cut -d' ' -f1-4,6-)
        now_rest=$(echo "$now" | cut -d' ' -f1-4,6-)
        printf '%s' "$PASS" | cryptsetup luksDump --dump-volume-key --batch-mode \
            --volume-key-file /run/obk-vk.bin "${S[0]}" --key-file - >/dev/null 2>&1
        local vk_hash key_hash
        vk_hash=$(od -An -tx1 -v /run/obk-vk.bin | tr -d ' \n' | sha256sum | cut -d' ' -f1)
        key_hash=$(echo "$now" | cut -d' ' -f5 | tr -d '\n' | sha256sum | cut -d' ' -f1)
        shred -u /run/obk-vk.bin 2>/dev/null || rm -f /run/obk-vk.bin
        [[ $(echo "$now" | cut -d' ' -f3) == crypt && $orig_rest == "$now_rest" && $vk_hash == "$key_hash" ]]
        check kill-cleanup-table-restored "$(verdict $?)" \
            "crypt again, same cipher/offsets/device as before; the key inline and equal to the container's volume key"
    fi
    cryptsetup status "$map" >"$EV/kill-status-after.txt" 2>&1
    check kill-cleanup-key-location INFO "$(grep 'key location' "$EV/kill-status-after.txt")"
    compare "$mnt" "$EV/btrfs.sha256" >/dev/null 2>&1 &&
        timeout 10 bash -c "echo after-cleanup >$mnt/after-cleanup.txt && sync -f $mnt/after-cleanup.txt"
    check kill-volume-intact-after-cleanup "$(verdict $?)" "data still matches its hashes, the volume is writable"
    ls -a "$IMG" >"$EV/kill-images-after.txt"
    check kill-unfinished-image INFO "$(grep -c '^\.own-backup-new-' "$EV/kill-images-after.txt") private image(s) of the killed run left (no service ledger in a CLI run)"
    rm -f "$IMG"/.own-backup-new-*

    # A store left by a run killed before device-mapper held it is swept by
    # the next run in the folder.
    : >"$IMG/.own-backup-cow-0123456789abcdef0123456789abcdef.tmp"
    backup_dm btrfs-after-kill "$map" "$mnt" "$IMG/btrfs-2.vhdx" "$FSID" --password "$PASS"
    [[ ! -e $IMG/.own-backup-cow-0123456789abcdef0123456789abcdef.tmp ]]
    check stale-store-swept "$(verdict $?)" "an unheld .own-backup-cow-* in the folder was deleted by the next run"

    compare /mnt/obk-src-btrfs "$EV/btrfs.sha256" >/dev/null 2>&1 &&
        compare /mnt/obk-src-ext4 "$EV/ext4.sha256" >/dev/null 2>&1 &&
        compare /mnt/obk-src-xfs "$EV/xfs.sha256" >/dev/null 2>&1
    check sources-intact-at-end "$(verdict $?)" "all three mounted sources still match their hashes"
}

guest_main() {
    CLI=$1
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
    guest_cleanup

    guest_steps

    guest_cleanup
    run after-cleanup bash -c "lsblk -o NAME,SIZE,TYPE,FSTYPE,MOUNTPOINTS $D1 $D2 $D3; dmsetup ls; losetup -a"
    [[ -z $(dmsetup ls 2>/dev/null | grep -E '^obk-(real|snap)-') ]] && ! lsblk -rno NAME "$D1" "$D2" "$D3" | grep -q '[0-9]$' &&
        [[ -z $(losetup -a | grep own-backup-cow-) ]]
    check guest-cleaned-up "$(verdict $?)" "no obk-* device, no partitions on the data disks, no store loop"
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
    guest_main "$2"
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
HOST_EV=$RUN_DIR/evidence/dm-snapshot-$STAMP

die() {
    echo "[scenario] ERROR: $*" >&2
    exit 1
}

"${VM[@]}" status || die 'the VM is not running: python3 -I scripts/lab/linux-vm.py up --disks 3'
mkdir -p "$HOST_EV"

COMMON=$(git -C "$REPO" rev-parse --path-format=absolute --git-common-dir 2>/dev/null)
for env_sh in "$REPO/tmp/tools/env.sh" "${COMMON%/.git}/tmp/tools/env.sh"; do
    if [[ -f $env_sh ]]; then
        # shellcheck source=/dev/null
        . "$env_sh"
        break
    fi
done

"${VM[@]}" budget --step dm-scenario-build || die 'space budget before the build'
(cd "$REPO" && exec cargo build --release -p own-backup-cli) >"$HOST_EV/build.log" 2>&1 &
build=$!
while kill -0 "$build" 2>/dev/null; do
    for _ in $(seq 1 28); do kill -0 "$build" 2>/dev/null || break; sleep 1; done
    if kill -0 "$build" 2>/dev/null && ! "${VM[@]}" budget --step dm-scenario-build-watch; then
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
git -C "$REPO" status --short >"$HOST_EV/worktree-status.txt"

"${VM[@]}" ssh -- 'rm -rf /tmp/obk-scenario && mkdir -p /tmp/obk-scenario' || die 'guest unreachable'
"${VM[@]}" put "$CLI" /tmp/obk-scenario/own-backup-cli || die 'copy of the CLI failed'
"${VM[@]}" put "${BASH_SOURCE[0]}" /tmp/obk-scenario/scenario.sh || die 'copy of the scenario failed'

echo "[scenario] guest part starts; evidence: $HOST_EV"
"${VM[@]}" ssh -- 'sudo bash /tmp/obk-scenario/scenario.sh guest /tmp/obk-scenario/own-backup-cli' \
    2>&1 | tee "$HOST_EV/guest-console.log"
rc=${PIPESTATUS[0]}

"${VM[@]}" get /var/tmp/obk-evidence "$HOST_EV/guest" || echo '[scenario] WARNING: evidence copy failed'
"${VM[@]}" ssh -- 'sudo rm -rf /var/tmp/obk-evidence /tmp/obk-scenario'
"${VM[@]}" budget --step dm-scenario-done

echo
echo "[scenario] summary ($HOST_EV/guest/summary.txt):"
cat "$HOST_EV/guest/summary.txt" 2>/dev/null || echo '(no summary copied)'
echo "[scenario] guest exit $rc; evidence in $HOST_EV"
exit "$rc"
