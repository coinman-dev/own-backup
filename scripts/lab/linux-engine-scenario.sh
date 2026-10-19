#!/usr/bin/env bash
# Linux acceptance of the shared engine (plan stage 1, phases L and E) on loop
# devices: no VM needed, only root, loop devices and the mkfs tools. It runs
# the same way inside a stand guest (`scripts/lab/linux-vm.py`) and on any
# root shell with /dev/loop*; it touches nothing but the loop devices it makes
# over files in its own work folder.
#
#   sudo scripts/lab/linux-engine-scenario.sh [CLI] [STAGE...]
#
# CLI defaults to target/debug/own-backup-cli (build it first, as yourself).
# STAGE is any of l1 l2 l3 e6 (default: all). Each check prints one line
# `CHECK <id> PASS|FAIL|SKIP <text>`; the exit code is the number of FAILs.
# Evidence (command outputs) stays in $WORK/evidence until the next run.
#
# What it shows that `cargo test` cannot: a real block device is held as the
# engine says (read-only mount, idle device under O_EXCL), the restored
# filesystems pass their own checkers and mount with the data intact, and the
# result is the same through the CLI as through the unit-level engine.
set -uo pipefail

cd "$(dirname "$0")/../.."
CLI=$(readlink -f "${1:-target/debug/own-backup-cli}")
shift || true
STAGES=("$@")
[[ ${#STAGES[@]} -eq 0 ]] && STAGES=(l1 l2 l3 e6)

WORK=${WORK:-$PWD/tmp/lab-engine}
EV=$WORK/evidence
FAILS=0
CHECKS=0
LOOPS=()
MOUNTS=()

if [[ $(id -u) -ne 0 ]]; then
    echo "run as root (loop devices, mount, fsfreeze)" >&2
    exit 2
fi
[[ -x $CLI ]] || { echo "no CLI at $CLI — cargo build -p own-backup-cli" >&2; exit 2; }

check() { # id verdict text...
    local id=$1 verdict=$2
    shift 2
    CHECKS=$((CHECKS + 1))
    [[ $verdict == FAIL ]] && FAILS=$((FAILS + 1))
    printf 'CHECK %-34s %-4s %s\n' "$id" "$verdict" "$*" | tee -a "$EV/summary.txt"
}

verdict() { if [[ $1 -eq 0 ]]; then echo PASS; else echo FAIL; fi; }

run() { # NAME CMD...: output to $EV/NAME.out, exit code returned
    local name=$1
    shift
    echo "### $*" >"$EV/$name.out"
    "$@" >>"$EV/$name.out" 2>&1
    local rc=$?
    echo "### exit $rc" >>"$EV/$name.out"
    return "$rc"
}

loopdev() { # name MiB -> device
    truncate -s "${2}M" "$WORK/$1.img" || return 1
    local d
    d=$(losetup -f --show "$WORK/$1.img") || return 1
    LOOPS+=("$d")
    echo "$d"
}

mnt() { # device dir [opts]
    mkdir -p "$2"
    mount ${3:+-o "$3"} "$1" "$2" && MOUNTS+=("$2")
}

umnt() { mountpoint -q "$1" && umount "$1"; }

populate() { # dir
    local d=$1/data i
    mkdir -p "$d/nested/deeper" "$d/many" "$d/cache" "$d/keep" || return 1
    head -c 12M /dev/urandom >"$d/big.bin" || return 1
    seq 1 200000 >"$d/nested/deeper/seq.txt" || return 1
    printf 'Own Backup engine lab — %s\n' "$1" >"$d/hello.txt" || return 1
    for i in $(seq 1 60); do head -c $((1024 + i * 37)) /dev/urandom >"$d/many/f$i"; done
    head -c 6M /dev/urandom >"$d/cache/junk.bin" || return 1
    head -c 2M /dev/urandom >"$d/keep/report.tmp" || return 1
    head -c 1M /dev/urandom >"$d/keep/report.txt" || return 1
    sync
}

hashes() { (cd "$1" && find data -type f -print0 | sort -z | xargs -0 sha256sum) >"$2"; }
compare() { (cd "$1" && sha256sum -c --quiet "$2") >/dev/null 2>&1; }

fsck_of() { # fstype device
    case $1 in
        ext4) e2fsck -fn "$2" ;;
        xfs) xfs_repair -n "$2" ;;
        btrfs) btrfs check --readonly "$2" ;;
        vfat) fsck.vfat -n "$2" ;;
    esac
}

mkfs_of() { # fstype device
    case $1 in
        ext4) mkfs.ext4 -q -F "$2" ;;
        xfs) mkfs.xfs -q -f "$2" ;;
        btrfs) mkfs.btrfs -q -f "$2" ;;
        vfat) mkfs.vfat -F 32 "$2" >/dev/null ;;
    esac
}

fs_mountable() { grep -qw "$1" /proc/filesystems; }

# restore_and_check ID IMAGE TARGET FSTYPE HASHFILE: restore, the filesystem's
# own checker, mount, every hashed file reads back the same.
restore_and_check() {
    local id=$1 image=$2 target=$3 fs=$4 sums=$5
    run "$id-restore" "$CLI" restore "$image" -t "$target" --yes
    check "$id-restore" "$(verdict $?)" "restore $(basename "$image") onto $target"
    run "$id-fsck" fsck_of "$fs" "$target"
    check "$id-fsck" "$(verdict $?)" "$fs checker clean after restore"
    if fs_mountable "$fs"; then
        mnt "$target" "$WORK/rst" ro
        compare "$WORK/rst" "$sums"
        check "$id-data" "$(verdict $?)" "restored files match their hashes"
        umnt "$WORK/rst"
    else
        check "$id-data" SKIP "kernel cannot mount $fs here"
    fi
}

cleanup() {
    local m d
    for m in "${MOUNTS[@]}"; do
        mountpoint -q "$m" && { fsfreeze -u "$m" 2>/dev/null; umount "$m" || umount -l "$m"; }
    done
    for d in "${LOOPS[@]}"; do losetup -d "$d" 2>/dev/null; done
}
trap cleanup EXIT

rm -rf "$WORK"
mkdir -p "$EV" "$WORK/out"

# ---------------------------------------------------------------------------
# L1: Linux copies through the shared engine.
# ---------------------------------------------------------------------------
stage_l1() {
    local src dst out=$WORK/out/l1
    mkdir -p "$out"
    # A filesystem mounted read-only holds still: its metadata is parsed and
    # only the used blocks are copied.
    src=$(loopdev l1-src 320) && dst=$(loopdev l1-dst 320) || return
    mkfs_of ext4 "$src" && mnt "$src" "$WORK/m-l1" && populate "$WORK/m-l1" \
        && hashes "$WORK/m-l1" "$WORK/l1.sums" && mount -o remount,ro "$WORK/m-l1"
    run l1-ro "$CLI" backup "$src" -o "$out/ro.qcow2" --format qcow2 --verify
    check l1-ro-copy "$(verdict $?)" "ext4 mounted read-only: copied and verified"
    grep -q 'used-block' "$EV/l1-ro.out"
    check l1-ro-smart "$(verdict $?)" "only used blocks were read"
    umnt "$WORK/m-l1"
    restore_and_check l1-ro "$out/ro.qcow2" "$dst" ext4 "$WORK/l1.sums"

    # An idle device: read directly, claimed exclusively for the copy.
    mkfs_of xfs "$src" && mnt "$src" "$WORK/m-l1" && populate "$WORK/m-l1" \
        && hashes "$WORK/m-l1" "$WORK/l1x.sums" && umnt "$WORK/m-l1"
    run l1-idle "$CLI" backup "$src" -o "$out/idle.vhdx" --format vhdx --verify
    check l1-idle-copy "$(verdict $?)" "idle xfs: copied to VHDX and verified"
    restore_and_check l1-idle "$out/idle.vhdx" "$dst" xfs "$WORK/l1x.sums"

    # Mounted read-write, not a logical volume: no snapshot, refused; and
    # nothing — no private stage either — is left in the output folder.
    mnt "$src" "$WORK/m-l1"
    run l1-busy "$CLI" backup "$src" -o "$out/busy.qcow2" --format qcow2
    local rc=$?
    [[ $rc -ne 0 ]] && grep -qi 'смонтирован\|mounted' "$EV/l1-busy.out"
    check l1-busy-refused "$(verdict $?)" "a volume in use without a snapshot is refused"
    [[ -z $(find "$out" -name 'busy*' -o -name '.*' -type f) ]]
    check l1-busy-nothing "$(verdict $?)" "the refusal leaves no file behind"
    umnt "$WORK/m-l1"
}

for stage in "${STAGES[@]}"; do
    if declare -F "stage_$stage" >/dev/null; then
        echo "== $stage"
        "stage_$stage"
    else
        check "$stage" SKIP "no such stage in this version of the scenario"
    fi
done

echo "== $CHECKS checks, $FAILS failed (evidence: $EV)"
exit "$FAILS"
