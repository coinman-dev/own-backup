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
#   python3 -I scripts/lab/linux-vm.py up --disks 3 [--size 2]
#   scripts/lab/linux-snapshot-scenario.sh
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
    for m in /mnt/obk-rst-thin /mnt/obk-rst-classic /mnt/obk-idle /mnt/obk-busy \
        /mnt/obk-thin /mnt/obk-classic /mnt/obk-images; do
        mountpoint -q "$m" && { umount "$m" || umount -l "$m"; }
        [[ -d $m ]] && rmdir "$m"
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
        pvremove -ff -y "$d" >/dev/null 2>&1
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

    guest_steps

    guest_cleanup
    run after-cleanup bash -c "lsblk -o NAME,SIZE,TYPE,FSTYPE,MOUNTPOINTS $D1 $D2 $D3; vgs; lvs -a; findmnt /boot/efi"
    ! vgs "$VG" >/dev/null 2>&1 && ! lsblk -rno NAME "$D1" "$D2" "$D3" | grep -q '[0-9]$' &&
        ! lvs --noheadings 2>/dev/null | grep -q obk-snap- &&
        { [[ -z ${ESP_MOUNT:-} ]] || mountpoint -q "$ESP_MOUNT"; }
    check guest-cleaned-up "$(verdict $?)" "no VG, no partitions on the data disks, ESP mounted as before"
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
HOST_EV=$RUN_DIR/evidence/snapshot-$STAMP

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
"${VM[@]}" ssh -- 'sudo bash /tmp/obk-scenario/scenario.sh guest /tmp/obk-scenario/own-backup-cli' \
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
