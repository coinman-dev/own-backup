#!/usr/bin/env bash
# Linux acceptance scenario for decision 10: a SHORT plain partition in use —
# /boot, the ESP — is held still for the length of its own copy. A 512 MiB ext4
# partition mounted read-write with a writer loop (fsync) running is frozen
# (FIFREEZE) and thawed right after its copy; a 256 MiB FAT32 partition with no
# open files is remounted read-only and back. Each copy is restored onto a
# spare partition and compared by sha256 and its filesystem's checker; the
# writer stalls only while its partition is held; afterwards both are writable
# again and no marker is left in /run/own-backup/frozen. Refusals: FAT with a
# file open for writing, a destination on the partition itself, `/`, a 6 GiB
# ext4 partition. Kill test: the CLI is killed with SIGKILL while ext4 is
# frozen; the filesystem stays frozen (its writer waits) until `own-backup-cli
# cleanup-snapshots` thaws it, and the writer then resumes. Finally the
# guest's own /boot (ext4) and /boot/efi (vfat) are copied while it runs.
#
#   python3 -I scripts/lab/linux-vm.py up --disks 2 --size 7
#   scripts/lab/linux-shortfreeze-scenario.sh
#
# The host part builds the release CLI, copies it and this file into the guest
# and runs `this-file guest <cli>` there as root. Only the data disks with the
# serials ob-data-1..2 are touched, and the guest is left as it was found.
# Evidence: ~/lab/linux/run/evidence/shortfreeze-<UTC>/ (guest/ holds the raw
# outputs, guest/summary.txt the verdicts). Exit 0 only when every check passed.
set -uo pipefail

# ---------------------------------------------------------------------------
# Guest part (root, inside the VM)
# ---------------------------------------------------------------------------

EV=/var/tmp/obk-evidence
FAILS=0
CHECKS=0
IMG=/mnt/obk-images
BOOT=/mnt/boot-like
ESP=/mnt/esp-like
BIG=/mnt/obk-big
MARKERS=/run/own-backup/frozen

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
    printf 'Own Backup short freeze lab — %s\n' "$1" >"$d/hello.txt" || return 1
    for i in $(seq 1 120); do head -c $((1024 + i * 37)) /dev/urandom >"$d/many/f$i"; done
    sync -f "$d/hello.txt"
}

hashes() { (cd "$1" && find data -type f -print0 | sort -z | xargs -0 sha256sum) >"$2"; }
compare() { (cd "$1" && sha256sum -c --quiet "$2"); }

markers() { ls -A "$MARKERS" 2>/dev/null; }

# Epoch seconds of the first CLI log line matching a pattern (its UTC stamp).
stamp_of() { # file pattern
    local ts
    ts=$(grep -m1 -E "$2" "$1" | grep -oE '^[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9:.]+Z')
    [[ -n $ts ]] && date -u -d "$ts" +%s.%N
}

# Keep writing with fsync; one line per write: index, start, seconds, exit.
churn() { # mountpoint out-file
    mkdir -p "$1/churn"
    exec -a obk-churn bash -c '
        dir=$1; out=$2; i=0
        while :; do
            t0=$(date +%s.%N)
            dd if=/dev/urandom of="$dir/c$((i % 20))" bs=64K count=4 conv=fsync status=none
            rc=$?
            t1=$(date +%s.%N)
            awk -v i="$i" -v a="$t0" -v b="$t1" -v rc="$rc" \
                "BEGIN { printf \"%d %s %.3f %d\n\", i, a, b - a, rc }" >>"$out"
            i=$((i + 1))
            sleep 0.05
        done' obk-churn "$1/churn" "$2"
}

stop_churn() {
    pkill -f obk-churn 2>/dev/null
    # A writer that waits on a frozen filesystem goes only once it is thawed.
    for _ in $(seq 1 50); do pgrep -f obk-churn >/dev/null || return 0; sleep 0.2; done
}

guest_cleanup() {
    local m
    # Nothing may stay frozen: a frozen filesystem cannot be unmounted cleanly.
    [[ -x ${CLI:-} ]] && "$CLI" cleanup-snapshots >/dev/null 2>&1
    for m in "$BOOT" "$ESP"; do
        mountpoint -q "$m" && { fsfreeze -u "$m" 2>/dev/null; mount -o remount,rw "$m" 2>/dev/null; }
    done
    stop_churn
    pkill -f obk-holder 2>/dev/null
    for m in /mnt/obk-rst-boot /mnt/obk-rst-esp "$BOOT" "$ESP" "$BIG" "$IMG"; do
        mountpoint -q "$m" && { umount "$m" || umount -l "$m"; }
        [[ -d $m ]] && rmdir "$m"
    done
    for d in "${D1:-}" "${D2:-}"; do
        [[ -b $d ]] || continue
        wipefs -a -q "$d"-part* "$d" 2>/dev/null
    done
    udevadm settle -t 10 2>/dev/null
}

# writer_report NAME CHURN-FILE CLI-OUT: the writer kept going, and its one
# long wait fell inside the hold the CLI logged.
writer_report() {
    local name=$1 churn=$2 out=$3
    local writes failed slowest h0 h1
    writes=$(wc -l <"$churn")
    failed=$(awk '$4 != 0' "$churn" | wc -l)
    slowest=$(awk '{ if ($3 > m) { m = $3; l = $0 } } END { print l }' "$churn")
    h0=$(stamp_of "$out" 'partition held still for its copy')
    h1=$(stamp_of "$out" 'partition released after its copy')
    echo "hold window: $h0 .. $h1; slowest write: $slowest" >"$EV/$name.writer.txt"
    [[ $writes -gt 0 && $failed -eq 0 ]]
    check "$name-writer-kept-writing" "$(verdict $?)" "$writes fsync'd 256 KiB writes, $failed failed"
    # Every write that waited more than 0.5 s overlapped the hold, and none
    # waited longer than the hold lasted (plus 0.5 s of slack).
    local verdict_rc=1 held=? longest=?
    if [[ -n $h0 && -n $h1 ]]; then
        held=$(awk -v a="$h0" -v b="$h1" 'BEGIN { printf "%.3f", b - a }')
        longest=$(awk '{ if ($3 > m) m = $3 } END { printf "%.3f", m }' "$churn")
        awk -v a="$h0" -v b="$h1" '
            $3 > 0.5 { if ($2 > b || $2 + $3 < a || $3 > b - a + 0.5) bad++ }
            END { exit bad ? 1 : 0 }' "$churn"
        verdict_rc=$?
        awk -v a="$h0" -v b="$h1" '$3 > 0.5 { printf "long write: start %.3f (%.3f s after the hold began) for %.3f s\n", $2, $2 - a, $3 }' \
            "$churn" >>"$EV/$name.writer.txt"
    fi
    check "$name-writer-stalled-only-while-held" "$(verdict $verdict_rc)" \
        "held ${held} s; longest write ${longest} s; writes over 0.5 s: $(awk '$3 > 0.5' "$churn" | wc -l)"
    echo "$name longest=$longest held=$held" >>"$EV/stalls.txt"
}

# backup_held NAME DEVICE MOUNTPOINT IMAGE MODE CHURN(yes|no) [cli args...]
backup_held() {
    local name=$1 dev=$2 mnt=$3 image=$4 mode=$5 with_churn=$6
    shift 6
    local out=$EV/backup-$name.out churn_pid= sampler_pid= options_before
    options_before=$(findmnt -no OPTIONS "$mnt")
    : >"$EV/backup-$name.churn.txt"
    if [[ $with_churn == yes ]]; then
        churn "$mnt" "$EV/backup-$name.churn.txt" &
        churn_pid=$!
    fi
    # The mount's options every 20 ms: a read-only remount shows as `ro`.
    (while :; do echo "$(date +%s.%N) $(findmnt -no OPTIONS "$mnt" | cut -d, -f1)"; sleep 0.02; done) \
        >"$EV/backup-$name.options.txt" 2>&1 &
    sampler_pid=$!
    sleep 1

    local t0=$SECONDS
    RUST_LOG=info timeout 600 "$CLI" backup "$dev" -o "$image" "$@" >"$out" 2>&1
    local rc=$?
    sleep 1
    kill "$sampler_pid" 2>/dev/null
    wait "$sampler_pid" 2>/dev/null
    if [[ -n $churn_pid ]]; then
        stop_churn
        wait "$churn_pid" 2>/dev/null
    fi
    echo "### exit $rc after $((SECONDS - t0)) s" >>"$out"
    plain "$out"
    sed 's/^/    /' "$out"

    grep -q '^Verify: OK' "$out" && [[ $rc -eq 0 && -s $image ]]
    check "backup-$name-verified" "$(verdict $?)" "exit $rc: $(grep '^Done:\|^Verify:' "$out" | tr '\n' ' ')"
    grep 'partition held still for its copy' "$out" | grep -q "mode=$mode"
    check "backup-$name-mode-$mode" "$(verdict $?)" \
        "$(grep -o 'partition held still.*' "$out" | head -1 | cut -c1-40) $(grep -o "mode=[a-z-]*" "$out" | head -1) $(grep -o 'mount_point=[^ ]*' "$out" | head -1)"
    grep -q 'partition released after its copy' "$out"
    check "backup-$name-released" "$(verdict $?)" "$(grep -o 'held_ms=[0-9]*' "$out" | head -1)"
    # Released before the verification read the image.
    local rel ver
    rel=$(grep -n 'partition released after its copy' "$out" | head -1 | cut -d: -f1)
    ver=$(grep -n '^Verify:' "$out" | head -1 | cut -d: -f1)
    [[ -n $rel && -n $ver && $rel -lt $ver ]]
    check "backup-$name-released-before-verify" "$(verdict $?)" "release on line $rel, verification on line $ver"
    local left
    left=$(markers)
    [[ -z $left ]]
    check "backup-$name-no-marker" "$(verdict $?)" "$MARKERS: ${left:-empty}"
    findmnt -no OPTIONS "$mnt" | grep -qE '^rw' &&
        timeout 10 bash -c "echo after >$mnt/after-$name.txt && sync -f $mnt/after-$name.txt"
    check "backup-$name-writable-after" "$(verdict $?)" \
        "$(findmnt -no OPTIONS "$mnt" | cut -d, -f1), a file written and synced within 10 s"
    rm -f "$mnt/after-$name.txt"
    [[ $(findmnt -no OPTIONS "$mnt") == "$options_before" ]]
    check "backup-$name-options-kept" "$(verdict $?)" "$(findmnt -no OPTIONS "$mnt")"
    if [[ $mode == readonly-remount ]]; then
        local ro
        ro=$(awk '$2 == "ro"' "$EV/backup-$name.options.txt" | wc -l)
        [[ $ro -gt 0 ]] && tail -1 "$EV/backup-$name.options.txt" | grep -q ' rw$'
        check "backup-$name-was-readonly-meanwhile" "$(verdict $?)" \
            "$ro of $(wc -l <"$EV/backup-$name.options.txt") samples (20 ms) saw ro; rw again at the end"
    fi
    [[ $with_churn == yes ]] && writer_report "backup-$name" "$EV/backup-$name.churn.txt" "$out"
    return 0
}

restore_and_compare() { # name image target mountpoint manifest fs
    local name=$1 image=$2 target=$3 mnt=$4 manifest=$5 fs=$6
    run "restore-$name" timeout 600 "$CLI" restore "$image" --target "$target" --yes
    local rc=$?
    check "restore-$name-exit" "$(verdict $rc)" "restore $image -> $target (exit $rc)"
    case $fs in
        ext4) run "restore-$name-fsck" e2fsck -fn "$target" ;;
        vfat) run "restore-$name-fsck" fsck.vfat -n "$target" ;;
    esac
    rc=$?
    check "restore-$name-fsck-clean" "$(verdict $rc)" "$fs read-only check of $target before any mount"
    mkdir -p "$mnt"
    if mount "$target" "$mnt" 2>"$EV/restore-$name.mount.err"; then
        compare "$mnt" "$manifest" >"$EV/restore-$name.compare.txt" 2>&1
        check "restore-$name-hashes" "$(verdict $?)" \
            "$(wc -l <"$manifest") files compared by sha256 ($(wc -l <"$EV/restore-$name.compare.txt") mismatch lines)"
        echo after-restore >"$mnt/after-restore.txt" && sync -f "$mnt/after-restore.txt"
        check "restore-$name-writable" "$(verdict $?)" "a file written and synced on the restored $fs"
        umount "$mnt"
    else
        check "restore-$name-hashes" FAIL "mount failed: $(cat "$EV/restore-$name.mount.err")"
    fi
}

refuse() { # name pattern device image [cli args]: refused, nothing written, nothing held
    local name=$1 pattern=$2 dev=$3 image=$4
    shift 4
    local out=$EV/refuse-$name.out
    timeout 120 "$CLI" backup "$dev" -o "$image" "$@" >"$out" 2>&1
    local rc=$?
    plain "$out"
    sed 's/^/    /' "$out"
    [[ $rc -ne 0 && $rc -ne 124 ]] && grep -Eq "$pattern" "$out" && [[ ! -e $image && -z $(markers) ]] &&
        ! grep -q 'partition held still' "$out"
    check "refuse-$name" "$(verdict $?)" "exit $rc: $(grep -m1 -i 'error' "$out" | cut -c1-220)"
}

guest_steps() {
    run info-system bash -c 'uname -a; grep PRETTY /etc/os-release; lsblk -o NAME,SIZE,TYPE,FSTYPE,MOUNTPOINTS; for m in / /boot /boot/efi; do findmnt -rno SOURCE,TARGET,FSTYPE,OPTIONS $m; done'
    run info-cli "$CLI" --version

    # --- setup: boot-like ext4, esp-like FAT32 and a 6 GiB ext4 on data disk 1;
    # images and the restore targets on data disk 2.
    run setup-disks bash -c "set -e
        printf 'label: gpt\n,512M,L\n,256M,U\n,6G,L\n' | sfdisk -q $D1
        printf 'label: gpt\n,3G,L\n,512M,L\n,256M,L\n' | sfdisk -q $D2
        udevadm settle -t 10"
    local rc=$?
    local P_BOOT P_ESP P_BIG P_IMG R_BOOT R_ESP
    P_BOOT=$(part_of "$D1" 1) && P_ESP=$(part_of "$D1" 2) && P_BIG=$(part_of "$D1" 3) &&
        P_IMG=$(part_of "$D2" 1) && R_BOOT=$(part_of "$D2" 2) && R_ESP=$(part_of "$D2" 3) || rc=1
    if [[ $rc -eq 0 ]]; then
        run setup-fs bash -c "set -e
            mkfs.ext4 -q -L obk-bootlike $P_BOOT
            mkfs.vfat -F 32 -n OBKESP $P_ESP
            mkfs.ext4 -q -L obk-big $P_BIG
            mkfs.ext4 -q -L obk-images $P_IMG
            mkdir -p $BOOT $ESP $BIG $IMG
            mount $P_BOOT $BOOT
            mount -o nosuid,nodev,fmask=0077,dmask=0077 $P_ESP $ESP
            mount $P_BIG $BIG
            mount $P_IMG $IMG
            lsblk -o NAME,SIZE,FSTYPE,MOUNTPOINTS $D1 $D2
            for m in $BOOT $ESP $BIG $IMG; do findmnt -no TARGET,SOURCE,FSTYPE,OPTIONS \$m; done"
        rc=$?
    fi
    if [[ $rc -eq 0 ]]; then
        populate "$BOOT" 300 && hashes "$BOOT" "$EV/boot.sha256" &&
            populate "$ESP" 80 && hashes "$ESP" "$EV/esp.sha256" && sync
        rc=$?
    fi
    check setup "$(verdict $rc)" "ext4 512M at $BOOT, FAT32 256M at $ESP, ext4 6G at $BIG (all rw), images on ob-data-2"
    [[ $rc -eq 0 ]] || return
    echo "boot $P_BOOT esp $P_ESP big $P_BIG images $P_IMG restore $R_BOOT $R_ESP" >"$EV/devices.txt"

    # --- a. ext4 frozen while a writer runs; FAT32 remounted read-only
    backup_held boot "$P_BOOT" "$BOOT" "$IMG/boot.vhdx" freeze yes
    backup_held esp "$P_ESP" "$ESP" "$IMG/esp.qcow2" readonly-remount no --format qcow2

    # --- b. each copy restored onto a spare partition and compared
    restore_and_compare boot "$IMG/boot.vhdx" "$R_BOOT" /mnt/obk-rst-boot "$EV/boot.sha256" ext4
    restore_and_compare esp "$IMG/esp.qcow2" "$R_ESP" /mnt/obk-rst-esp "$EV/esp.sha256" vfat
    rm -f "$IMG/boot.vhdx" "$IMG/esp.qcow2" "$IMG"/*.json 2>/dev/null

    # --- c. refusals
    # FAT with a file open for writing cannot be remounted read-only.
    (exec -a obk-holder bash -c "exec 7>>$ESP/held-open.txt; sleep 300") &
    local holder=$!
    sleep 0.5
    refuse esp-open-for-write 'открыты файлы для записи|open for writing' "$P_ESP" "$IMG/esp-busy.vhdx"
    findmnt -no OPTIONS "$ESP" | grep -qE '^rw'
    check refuse-esp-open-left-rw "$(verdict $?)" "$(findmnt -no OPTIONS "$ESP" | cut -d, -f1) after the refusal"
    kill "$holder" 2>/dev/null
    pkill -f obk-holder 2>/dev/null
    wait "$holder" 2>/dev/null
    # The destination on the partition itself: freezing it would stop the copy.
    refuse dest-on-partition 'куда программа пишет|which the program writes to' "$P_BOOT" "$BOOT/self.vhdx"
    timeout 10 bash -c "echo alive >$BOOT/after-refusal.txt && sync -f $BOOT/after-refusal.txt"
    check refuse-dest-left-writable "$(verdict $?)" "$BOOT writable within 10 s after the refusal"
    local ROOT
    ROOT=$(findmnt -no SOURCE /)
    refuse root 'корень системы|system root' "$ROOT" "$IMG/root.vhdx"
    refuse big-partition 'больше|larger than' "$P_BIG" "$IMG/big.vhdx"
    timeout 10 bash -c "echo alive >$BIG/after-refusal.txt && sync -f $BIG/after-refusal.txt && touch /var/tmp/obk-alive"
    check refuse-left-everything-writable "$(verdict $?)" "$BIG and / writable within 10 s"

    # --- d. kill test: SIGKILL while ext4 is frozen
    : >"$EV/kill.churn.txt"
    churn "$BOOT" "$EV/kill.churn.txt" &
    local churn_pid=$!
    sleep 1
    # Every sector, compressed: a copy long enough to be stopped in the middle.
    RUST_LOG=info "$CLI" backup "$P_BOOT" -o "$IMG/killed.qcow2" --format qcow2 --all-sectors \
        >"$EV/kill-backup.out" 2>&1 &
    local cli_pid=$! seen=1
    for _ in $(seq 1 3000); do
        if grep -q 'partition held still for its copy' "$EV/kill-backup.out" 2>/dev/null; then
            kill -STOP "$cli_pid"
            seen=0
            break
        fi
        sleep 0.01
    done
    {
        echo "=== CLI stopped while the partition is held (then SIGKILL)"
        date -u +%FT%T.%NZ
        ls -la "$MARKERS"
        cat "$MARKERS"/*.marker
        findmnt -o TARGET,OPTIONS "$BOOT"
    } >"$EV/kill-state.txt" 2>&1
    kill -9 "$cli_pid" 2>/dev/null
    wait "$cli_pid" 2>/dev/null
    local killed_at
    killed_at=$(date +%s.%N)
    plain "$EV/kill-backup.out"
    ! grep -q 'partition released after its copy\|^Done:' "$EV/kill-backup.out" &&
        [[ $(grep -c 'method=freeze' "$EV/kill-state.txt") -eq 1 ]]
    check kill-landed-while-frozen "$(verdict $((seen + $?)))" \
        "killed with the freeze in place; marker: $(grep -o 'mount_point=.*' "$EV/kill-state.txt" | head -1)"
    # A writer started now waits: the filesystem is still frozen.
    rm -f /var/tmp/obk-probe.done
    (exec -a obk-probe bash -c "dd if=/dev/urandom of=$BOOT/probe.bin bs=64K count=4 conv=fsync status=none && date +%s.%N >/var/tmp/obk-probe.done") &
    local probe=$!
    sleep 5
    local after_kill
    after_kill=$(awk -v k="$killed_at" '$2 + $3 > k + 0.2' "$EV/kill.churn.txt" | wc -l)
    [[ ! -e /var/tmp/obk-probe.done && -n $(markers) && $after_kill -eq 0 ]]
    check kill-stays-frozen "$(verdict $?)" \
        "5 s after SIGKILL: the probe write has not completed, the churn finished $after_kill writes, marker $(markers)"
    local cleaned_at
    cleaned_at=$(date +%s.%N)
    run kill-cleanup env RUST_LOG=info timeout 120 "$CLI" cleanup-snapshots
    rc=$?
    [[ $rc -eq 0 && -z $(markers) ]] && grep -q 'removed: 1' "$EV/kill-cleanup.out" &&
        grep -q 'held still by an interrupted run is released' "$EV/kill-cleanup.out"
    check kill-cleanup-thawed "$(verdict $?)" "exit $rc; $(grep -o 'removed: [0-9]*' "$EV/kill-cleanup.out"); markers left: $(markers)"
    for _ in $(seq 1 100); do [[ -e /var/tmp/obk-probe.done ]] && break; sleep 0.1; done
    wait "$probe" 2>/dev/null
    local probe_done resumed
    probe_done=$(cat /var/tmp/obk-probe.done 2>/dev/null)
    sleep 2
    resumed=$(awk -v c="$cleaned_at" '$2 > c && $4 == 0' "$EV/kill.churn.txt" | wc -l)
    [[ -n $probe_done && $resumed -gt 0 ]]
    check kill-writer-resumed "$(verdict $?)" \
        "probe write done $(awk -v a="$cleaned_at" -v b="${probe_done:-0}" 'BEGIN { printf "%.2f", b - a }') s after the cleanup started; $resumed churn writes since"
    stop_churn
    wait "$churn_pid" 2>/dev/null
    local longest
    longest=$(awk '{ if ($3 > m) m = $3 } END { printf "%.3f", m }' "$EV/kill.churn.txt")
    echo "kill-test longest=$longest (frozen from the kill until the cleanup)" >>"$EV/stalls.txt"
    compare "$BOOT" "$EV/boot.sha256" >/dev/null 2>&1 &&
        timeout 10 bash -c "echo after-cleanup >$BOOT/after-cleanup.txt && sync -f $BOOT/after-cleanup.txt"
    check kill-partition-intact "$(verdict $?)" "data still matches its hashes, the partition is writable"
    ls -a "$IMG" >"$EV/kill-images-after.txt"
    check kill-unfinished-image INFO "$(grep -c '^\.own-backup-new-' "$EV/kill-images-after.txt") private image(s) of the killed run left (no service ledger in a CLI run)"
    rm -f "$IMG"/.own-backup-new-* /var/tmp/obk-probe.done

    # --- e. the guest's own /boot and ESP, while it runs
    local SYS_BOOT SYS_ESP
    SYS_BOOT=$(findmnt -no SOURCE /boot 2>/dev/null)
    SYS_ESP=$(findmnt -no SOURCE /boot/efi 2>/dev/null)
    if [[ -n $SYS_BOOT ]]; then
        backup_held sys-boot "$SYS_BOOT" /boot "$IMG/sys-boot.vhdx" freeze no
        run sys-boot-image bash -c "ls -la $IMG"
    else
        check sys-boot SKIP "no separate /boot"
    fi
    if [[ -n $SYS_ESP ]]; then
        backup_held sys-esp "$SYS_ESP" /boot/efi "$IMG/sys-esp.qcow2" readonly-remount no --format qcow2
    else
        check sys-esp SKIP "no ESP at /boot/efi"
    fi
    rm -f "$IMG"/sys-*

    [[ -z $(markers) ]] && findmnt -no OPTIONS "$BOOT" | grep -qE '^rw' &&
        findmnt -no OPTIONS "$ESP" | grep -qE '^rw'
    check nothing-held-at-end "$(verdict $?)" "no marker; $BOOT and $ESP read-write"
}

guest_main() {
    CLI=$1
    [[ $EUID -eq 0 ]] || { echo 'guest part needs root'; exit 2; }
    D1=/dev/disk/by-id/virtio-ob-data-1
    D2=/dev/disk/by-id/virtio-ob-data-2
    for d in "$D1" "$D2"; do
        [[ -b $d ]] || { echo "missing $d: start the VM with --disks 2 --size 7"; exit 2; }
    done
    [[ $(blockdev --getsize64 "$D1") -ge $((7 << 30)) ]] ||
        { echo "ob-data-1 is smaller than 7 GiB: start the VM with --size 7"; exit 2; }
    chmod 0755 "$CLI"
    rm -rf "$EV"
    mkdir -p "$EV"
    : >"$EV/summary.txt"
    : >"$EV/stalls.txt"
    trap 'guest_cleanup; chmod -R a+rX "$EV"' EXIT
    trap 'exit 130' INT TERM
    guest_cleanup

    guest_steps

    guest_cleanup
    run after-cleanup bash -c "lsblk -o NAME,SIZE,TYPE,FSTYPE,MOUNTPOINTS $D1 $D2; for m in /boot /boot/efi; do findmnt -no TARGET,OPTIONS \$m; done; ls -la $MARKERS"
    ! lsblk -rno NAME "$D1" "$D2" | grep -q '[0-9]$' && [[ -z $(markers) ]] &&
        findmnt -no OPTIONS /boot/efi | grep -qE '^rw'
    check guest-cleaned-up "$(verdict $?)" "no partitions on the data disks, no marker, /boot/efi read-write"
    mkdir -p "$EV/cli-logs"
    cp -a /var/log/own-backup/. "$EV/cli-logs/" 2>/dev/null
    dmesg >"$EV/dmesg.txt" 2>&1
    echo "--- writer stalls" | tee -a "$EV/summary.txt"
    tee -a "$EV/summary.txt" <"$EV/stalls.txt"
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
HOST_EV=$RUN_DIR/evidence/shortfreeze-$STAMP

die() {
    echo "[scenario] ERROR: $*" >&2
    exit 1
}

"${VM[@]}" status || die 'the VM is not running: python3 -I scripts/lab/linux-vm.py up --disks 2 --size 7'
mkdir -p "$HOST_EV"

COMMON=$(git -C "$REPO" rev-parse --path-format=absolute --git-common-dir 2>/dev/null)
for env_sh in "$REPO/tmp/tools/env.sh" "${COMMON%/.git}/tmp/tools/env.sh"; do
    if [[ -f $env_sh ]]; then
        # shellcheck source=/dev/null
        . "$env_sh"
        break
    fi
done

"${VM[@]}" budget --step shortfreeze-build || die 'space budget before the build'
(cd "$REPO" && exec cargo build --release -p own-backup-cli) >"$HOST_EV/build.log" 2>&1 &
build=$!
while kill -0 "$build" 2>/dev/null; do
    for _ in $(seq 1 28); do kill -0 "$build" 2>/dev/null || break; sleep 1; done
    if kill -0 "$build" 2>/dev/null && ! "${VM[@]}" budget --step shortfreeze-build-watch; then
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
"${VM[@]}" budget --step shortfreeze-done

echo
echo "[scenario] summary ($HOST_EV/guest/summary.txt):"
cat "$HOST_EV/guest/summary.txt" 2>/dev/null || echo '(no summary copied)'
echo "[scenario] guest exit $rc; evidence in $HOST_EV"
exit "$rc"
