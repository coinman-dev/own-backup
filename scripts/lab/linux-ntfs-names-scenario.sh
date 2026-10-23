#!/usr/bin/env bash
# Linux acceptance of the NTFS name removal (plan §6 stage 2, decision 8) on
# loop devices, through the CLI and the real kernel path: our NBD server over
# the scratch overlay, ntfs-3g mounting it, the fold into the image. No VM is
# needed — root, loop devices, nbd.ko, FUSE and the NTFS tools are — and it
# runs the same inside a stand guest (`scripts/lab/linux-vm.py`).
#
#   sudo scripts/lab/linux-ntfs-names-scenario.sh [CLI] [STAGE...]
#
# CLI defaults to target/debug/own-backup-cli (build it first, as yourself).
# STAGE is any of n1 n2 n3 n4 (default: all). Each check prints one line
# `CHECK <id> PASS|FAIL|SKIP <text>`; the exit code is the number of FAILs.
# Evidence stays in $WORK/evidence until the next run; the images Windows has
# to judge (chkdsk, see the end of this header) are left in $WORK/for-windows.
#
#   n1  qcow2 and VHDX copies of a populated NTFS volume with exclusions: the
#       names leave the image, nothing excluded is left in its bytes (named
#       streams too: in clusters, inside the record, and behind an attribute
#       list), the restored volume is clean (ntfsfix -n), mounts with ntfs3
#       and ntfs-3g, keeps every other file and stream bit for bit, takes
#       writes, stays clean.
#   n2  a qcow2 chain: the source moves on, the increment is judged the same
#       way, and the parent is not changed by the child's pass.
#   n3  a whole disk with two NTFS partitions, one image, and an increment.
#   n4  a volume Windows left hibernated: ntfs-3g refuses to write it, the
#       copy is still good and the result says the names were kept — and an
#       excluded file's named stream is not in it all the same (the MFT scan
#       alone keeps it out, no deletion pass runs).
#
# Needs: ntfs-3g (mkntfs, ntfsfix), nbd.ko, /dev/fuse, qemu-img, sfdisk.
#
# What only the Windows stand can accept (CLAUDE.md: Linux checks do not
# establish real Windows behaviour). Take $WORK/for-windows/*.vhdx there and,
# for each, attach it read-only and then read-write (Disk Management or
# `Mount-DiskImage`), and on its volume:
#   1. `chkdsk X:` (no /f): "Windows has scanned the file system and found no
#      problems"; `chkdsk X: /scan` the same; `fsutil dirty query X:` — not
#      dirty.
#   2. The excluded names (cache\, Users\u\AppData\Local\Temp\*, pagefile.sys,
#      *.dmp, the contents of System Volume Information) are not listed; the
#      kept tree is (`dir /s /a`), and `Get-FileHash` of the kept files matches
#      $WORK/for-windows/<name>.sums (sha256, paths relative to the volume).
#   3. The named streams survive: `Get-Content keep\ads.txt -Stream Zone.Identifier`,
#      and `-Stream keepbig` is 20000 × "K".
#   4. Write: copy a few hundred MiB onto the volume, delete them, `chkdsk X:`
#      again — no problems.
#   5. The qcow2 copies (n1-q.qcow2, n2-inc.qcow2, n3-disk-inc.qcow2) the same
#      way after `own-backup-cli mount` on Windows, or restored onto a disk.
set -uo pipefail

cd "$(dirname "$0")/../.."
CLI=$(readlink -f "${1:-target/debug/own-backup-cli}")
shift || true
STAGES=("$@")
[[ ${#STAGES[@]} -eq 0 ]] && STAGES=(n1 n2 n3 n4)

WORK=${WORK:-$PWD/tmp/lab-ntfs-names}
EV=$WORK/evidence
WIN=$WORK/for-windows
FAILS=0
CHECKS=0
LOOPS=()
MOUNTS=()
MARK=OBK-EXCLUDED-CONTENT-MARKER

if [[ $(id -u) -ne 0 ]]; then
    echo "run as root (loop devices, nbd, FUSE mounts)" >&2
    exit 2
fi
[[ -x $CLI ]] || { echo "no CLI at $CLI — cargo build -p own-backup-cli" >&2; exit 2; }
for t in mkntfs ntfs-3g ntfsfix qemu-img sfdisk; do
    command -v "$t" >/dev/null || { echo "missing tool: $t" >&2; exit 2; }
done
modprobe nbd 2>/dev/null
[[ -e /dev/nbd0 ]] || { echo "no /dev/nbd0 — nbd.ko is needed" >&2; exit 2; }

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

loopdev() { # VAR name MiB: a loop device over a new file, its path in VAR
    truncate -s "${3}M" "$WORK/$2.img" || return 1
    local d
    d=$(losetup -f --show "$WORK/$2.img") || return 1
    LOOPS+=("$d")
    printf -v "$1" '%s' "$d"
}

# ntfs-3g mounts stay in the foreground of nothing here: `umount` returns once
# the volume is written back for a block device (fuseblk), which a loop is.
mnt3g() { mkdir -p "$2" && ntfs-3g -o "${3:-rw}" "$1" "$2" && MOUNTS+=("$2"); }
mnt3() { mkdir -p "$2" && mount -t ntfs3 -o "${3:-ro}" "$1" "$2" && MOUNTS+=("$2"); }
umnt() { mountpoint -q "$1" && umount "$1"; sync; }
has_ntfs3() { grep -qw ntfs3 /proc/filesystems || modprobe ntfs3 2>/dev/null; grep -qw ntfs3 /proc/filesystems; }

# The excluded half carries $MARK in every file and stream, so a byte of it
# left anywhere in an image is found by grep.
populate() { # dir
    local d=$1 i
    mkdir -p "$d/keep/deep/one" "$d/cache/one" "$d/cache/two" \
        "$d/Users/u/AppData/Local/Temp/a/b" "$d/Users/u/Documents" \
        "$d/System Volume Information" || return 1
    for i in $(seq 1 300); do head -c $((4096 + i * 97)) /dev/urandom >"$d/keep/k$i.bin"; done
    for i in $(seq 1 400); do head -c 3000 /dev/urandom >"$d/keep/deep/one/d$i.bin"; done
    head -c 20M /dev/urandom >"$d/keep/big.bin"
    for i in $(seq 1 600); do
        printf '%s %s\n' "$MARK" "$(head -c 4000 /dev/zero | tr '\0' x)" \
            >"$d/cache/$(printf 'x%.0s' $(seq 1 150))-$i.tmp"
    done
    for sub in one two; do
        for i in $(seq 1 300); do printf '%s %s\n' "$MARK" "$sub$i" >"$d/cache/$sub/n$i.tmp"; done
    done
    # A hard link across the boundary: the kept name keeps the file.
    head -c 70000 /dev/urandom >"$d/cache/hard_in.bin" && ln "$d/cache/hard_in.bin" "$d/keep/hard_out.bin"
    ln -s ../keep/k1.bin "$d/cache/link" && ln -s k1.bin "$d/keep/link"
    printf '%s temp\n' "$MARK" >"$d/Users/u/AppData/Local/Temp/x.tmp"
    printf '%s deeper\n' "$MARK" >"$d/Users/u/AppData/Local/Temp/a/b/y.tmp"
    head -c 300000 /dev/urandom >"$d/Users/u/Documents/report.docx"
    printf '%s %s\n' "$MARK" "$(head -c 3000000 /dev/zero | tr '\0' p)" >"$d/pagefile.sys"
    printf '%s dump\n' "$MARK" >"$d/MEMORY.DMP"
    printf '%s tracking\n' "$MARK" >"$d/System Volume Information/tracking.log"
    # Named streams (ntfs-3g's user.* xattrs): kept on a kept file, and a
    # marked one on an excluded file — the deletion pass must erase it.
    echo "kept" >"$d/keep/ads.txt"
    python3 -c 'import os,sys; os.setxattr(sys.argv[1], "user.Zone.Identifier", b"[ZoneTransfer]\r\nZoneId=3")' \
        "$d/keep/ads.txt"
    python3 -c 'import os,sys; os.setxattr(sys.argv[1], "user.hidden", (sys.argv[2] * 400).encode())' \
        "$d/Users/u/AppData/Local/Temp/x.tmp" "$MARK"
    # A short (resident) marked stream on an excluded file, a kept file with a
    # stream in clusters of its own, and an excluded file with so many streams
    # that its record overflows into extension records behind an attribute
    # list (its name moves there too) — the MFT scan must follow it.
    python3 -c 'import os,sys; os.setxattr(sys.argv[1], "user.Zone.Identifier", ("[ZoneTransfer]\r\nZoneId=3 " + sys.argv[2]).encode())' \
        "$d/MEMORY.DMP" "$MARK"
    python3 -c 'import os,sys; os.setxattr(sys.argv[1], "user.keepbig", b"K" * 20000)' "$d/keep/ads.txt"
    printf '%s many\n' "$MARK" >"$d/cache/ads_many.tmp"
    python3 -c '
import os, sys
p, m = sys.argv[1], sys.argv[2]
for i in range(30):
    os.setxattr(p, "user.s%02d" % i, ("%s list %02d " % (m, i)).encode().ljust(100, b"q"))
os.setxattr(p, "user.bigx", (m * 1200).encode())' "$d/cache/ads_many.tmp" "$MARK"
    sync
}

RULES=(--exclude cache --exclude 'Users\*\AppData\Local\Temp\**' --exclude pagefile.sys
    --exclude '*.dmp' --exclude 'System Volume Information')

stream_of() { python3 -c 'import os,sys; print(os.getxattr(sys.argv[1], "user.Zone.Identifier").decode(), end="")' "$1"; }
keepbig_ok() { python3 -c 'import os,sys; sys.exit(os.getxattr(sys.argv[1], "user.keepbig") != b"K" * 20000)' "$1"; }

hashes() { (cd "$1" && find keep Users/u/Documents -type f -print0 | sort -z | xargs -0 sha256sum) >"$2"; }
compare() { (cd "$1" && sha256sum -c --quiet "$2") >/dev/null 2>&1; }

# The names that must be gone, under a mount.
names_gone() { # dir
    [[ ! -e $1/cache && ! -e $1/pagefile.sys && ! -e $1/MEMORY.DMP \
        && -d $1/Users/u/AppData/Local/Temp && -z $(ls -A "$1/Users/u/AppData/Local/Temp") \
        && -d "$1/System Volume Information" && -z $(ls -A "$1/System Volume Information") ]]
}

# judge ID DEVICE SUMS: everything a restored NTFS volume must pass.
judge() {
    local id=$1 dev=$2 sums=$3
    ! grep -qa "$MARK" "$dev"
    check "$id-no-bytes" "$(verdict $?)" "no byte of excluded content (data or stream) in the volume"
    run "$id-ntfsfix" ntfsfix -n "$dev"
    check "$id-ntfsfix" "$(verdict $?)" "ntfsfix -n: clean"
    if has_ntfs3; then
        mnt3 "$dev" "$WORK/rst" ro && names_gone "$WORK/rst" && compare "$WORK/rst" "$sums"
        check "$id-ntfs3" "$(verdict $?)" "kernel ntfs3: excluded names gone, kept files intact"
        umnt "$WORK/rst"
    else
        check "$id-ntfs3" SKIP "no ntfs3 in this kernel"
    fi
    mnt3g "$dev" "$WORK/rst" && names_gone "$WORK/rst" && compare "$WORK/rst" "$sums" \
        && [[ $(stream_of "$WORK/rst/keep/ads.txt") == *ZoneId=3* ]] && keepbig_ok "$WORK/rst/keep/ads.txt"
    check "$id-ntfs3g" "$(verdict $?)" "ntfs-3g: excluded names gone, kept files and streams intact"
    local i=0
    while head -c 1M /dev/urandom >"$WORK/rst/fill$i.bin" 2>/dev/null && ((i < 4000)); do i=$((i + 1)); done
    sync
    compare "$WORK/rst" "$sums" && ((i > 0))
    check "$id-write" "$(verdict $?)" "filled the free space ($i MiB), kept files still intact"
    rm -f "$WORK"/rst/fill*.bin
    umnt "$WORK/rst"
    run "$id-ntfsfix2" ntfsfix -n "$dev"
    check "$id-ntfsfix2" "$(verdict $?)" "ntfsfix -n: clean after writing"
}

# The pass ran through NBD and left nothing behind.
no_leftovers() { # id
    ! grep -l . /sys/block/nbd*/pid >/dev/null 2>&1 \
        && [[ -z $(ls -A /run/own-backup/ntfs-names 2>/dev/null) ]] \
        && [[ -z $(find "$WORK/out" -name '*.excl.scratch') ]]
    check "$1-clean" "$(verdict $?)" "no NBD device, mount or scratch left behind"
}

cleanup() {
    local m d
    for m in "${MOUNTS[@]}"; do mountpoint -q "$m" && { umount "$m" || umount -l "$m"; }; done
    for d in "${LOOPS[@]}"; do losetup -d "$d" 2>/dev/null; done
}
trap cleanup EXIT

rm -rf "$WORK"
mkdir -p "$EV" "$WIN" "$WORK/out"

stage_n1() {
    local src dst out=$WORK/out
    loopdev src n1-src 512 && loopdev dst n1-dst 512 || return
    mkntfs -F -Q -q "$src" && mnt3g "$src" "$WORK/m-n1" && populate "$WORK/m-n1" \
        && hashes "$WORK/m-n1" "$WORK/n1.sums" && umnt "$WORK/m-n1"
    check n1-source "$(verdict $?)" "NTFS volume populated with the corpus"
    ntfsinfo -F /cache/ads_many.tmp "$src" >"$EV/n1-ads-many.txt" 2>&1 && grep -q 'ATTRIBUTE_LIST' "$EV/n1-ads-many.txt"
    check n1-attr-list "$(verdict $?)" "the many-streams file really has an attribute list"
    local fmt
    for fmt in qcow2 vhdx; do
        run "n1-$fmt" "$CLI" backup "$src" -o "$out/n1.$fmt" --format "$fmt" "${RULES[@]}" --verify
        check "n1-$fmt-copy" "$(verdict $?)" "copied to $fmt with exclusions and verified"
        grep -q '^Excluded: ' "$EV/n1-$fmt.out" && ! grep -q 'Excluded names kept' "$EV/n1-$fmt.out"
        check "n1-$fmt-said" "$(verdict $?)" "the result says the names were removed, not kept"
        ! grep -qa "$MARK" "$out/n1.$fmt"
        check "n1-$fmt-image" "$(verdict $?)" "no excluded byte in the image file itself"
        no_leftovers "n1-$fmt"
        run "n1-$fmt-restore" "$CLI" restore "$out/n1.$fmt" -t "$dst" --yes
        check "n1-$fmt-restore" "$(verdict $?)" "restored onto $dst"
        judge "n1-$fmt" "$dst" "$WORK/n1.sums"
    done
    cp "$out/n1.vhdx" "$WIN/n1.vhdx" && cp "$WORK/n1.sums" "$WIN/n1.sums"
    cp "$out/n1.qcow2" "$WIN/n1-q.qcow2"
}

stage_n2() {
    local src dst out=$WORK/out
    loopdev src n2-src 512 && loopdev dst n2-dst 512 || return
    mkntfs -F -Q -q "$src" && mnt3g "$src" "$WORK/m-n2" && populate "$WORK/m-n2" && umnt "$WORK/m-n2"
    run n2-full "$CLI" backup "$src" -o "$out/n2-full.qcow2" --format qcow2 "${RULES[@]}"
    check n2-full "$(verdict $?)" "version 1: full qcow2"
    mnt3g "$src" "$WORK/m-n2" && head -c 2M /dev/urandom >"$WORK/m-n2/keep/added.bin" \
        && head -c 5000 /dev/urandom >"$WORK/m-n2/keep/k7.bin" \
        && printf '%s later\n' "$MARK" >"$WORK/m-n2/cache/later.tmp" \
        && printf '%s later\n' "$MARK" >"$WORK/m-n2/Users/u/AppData/Local/Temp/later.tmp" \
        && hashes "$WORK/m-n2" "$WORK/n2.sums" && umnt "$WORK/m-n2"
    local before
    before=$(sha256sum <"$out/n2-full.qcow2")
    run n2-inc "$CLI" backup "$src" -o "$out/n2-inc.qcow2" --format qcow2 --kind incremental \
        --parent-auto "${RULES[@]}" --verify
    check n2-inc "$(verdict $?)" "version 2: incremental, verified"
    grep -q 'Incremental: .* blocks written' "$EV/n2-inc.out" && ! grep -q 'Excluded names kept' "$EV/n2-inc.out"
    check n2-inc-child "$(verdict $?)" "a real child, names removed"
    [[ $(sha256sum <"$out/n2-full.qcow2") == "$before" ]]
    check n2-parent "$(verdict $?)" "the parent is byte-identical after the child's pass"
    run n2-check qemu-img check "$out/n2-inc.qcow2"
    check n2-qemu-check "$(verdict $?)" "qemu-img check: no errors in the child"
    no_leftovers n2
    run n2-restore "$CLI" restore "$out/n2-inc.qcow2" -t "$dst" --yes
    check n2-restore "$(verdict $?)" "the chain restored"
    judge n2 "$dst" "$WORK/n2.sums"
    cp "$out/n2-full.qcow2" "$out/n2-inc.qcow2" "$WIN/" && cp "$WORK/n2.sums" "$WIN/n2.sums"
}

stage_n3() {
    local disk out=$WORK/out back
    truncate -s 900M "$WORK/n3-disk.img" || return
    printf 'label: gpt\nsize=500MiB, type=EBD0A0A2-B9E5-4433-87C0-68B6B72699C7\nsize=300MiB, type=EBD0A0A2-B9E5-4433-87C0-68B6B72699C7\n' \
        | sfdisk -q "$WORK/n3-disk.img" || return
    disk=$(losetup -f --show -P "$WORK/n3-disk.img") && LOOPS+=("$disk")
    udevadm settle -t 5 2>/dev/null
    mkntfs -F -Q -q "${disk}p1" && mkntfs -F -Q -q "${disk}p2" \
        && mnt3g "${disk}p1" "$WORK/m-n3" && populate "$WORK/m-n3" && hashes "$WORK/m-n3" "$WORK/n3.sums" \
        && umnt "$WORK/m-n3" && mnt3g "${disk}p2" "$WORK/m-n3" && mkdir -p "$WORK/m-n3/data/cache" \
        && printf '%s p2\n' "$MARK" >"$WORK/m-n3/data/cache/p2.tmp" && echo second >"$WORK/m-n3/data/keep.txt" \
        && umnt "$WORK/m-n3"
    check n3-source "$(verdict $?)" "a GPT disk with two populated NTFS partitions"
    run n3-full "$CLI" backup-disk --device "$disk" -o "$out/n3-disk.qcow2" --format qcow2 "${RULES[@]}" --verify
    check n3-full "$(verdict $?)" "the whole disk into one qcow2, verified"
    grep -q '^Done: 2 partitions' "$EV/n3-full.out" && ! grep -q 'Excluded names kept' "$EV/n3-full.out"
    check n3-said "$(verdict $?)" "both partitions, names removed"
    mnt3g "${disk}p2" "$WORK/m-n3" && echo later >"$WORK/m-n3/data/later.txt" && umnt "$WORK/m-n3"
    run n3-inc "$CLI" backup-disk --device "$disk" -o "$out/n3-disk-inc.qcow2" --format qcow2 \
        --kind incremental --parent-auto "${RULES[@]}" --verify
    check n3-inc "$(verdict $?)" "an increment of the disk"
    grep -q 'Incremental' "$EV/n3-inc.out" && ! grep -q 'Excluded names kept' "$EV/n3-inc.out"
    check n3-inc-said "$(verdict $?)" "a real child, names removed"
    no_leftovers n3
    run n3-convert qemu-img convert -O raw "$out/n3-disk-inc.qcow2" "$WORK/n3-back.img"
    check n3-convert "$(verdict $?)" "qemu-img reads the whole-disk chain"
    back=$(losetup -f --show -P "$WORK/n3-back.img") && LOOPS+=("$back")
    udevadm settle -t 5 2>/dev/null
    judge n3-p1 "${back}p1" "$WORK/n3.sums"
    ! grep -qa "$MARK" "${back}p2" && run n3-p2-fix ntfsfix -n "${back}p2" && mnt3g "${back}p2" "$WORK/rst" \
        && [[ ! -e $WORK/rst/data/cache && $(cat "$WORK/rst/data/later.txt") == later ]]
    check n3-p2 "$(verdict $?)" "second partition: clean, its excluded name gone, its change there"
    umnt "$WORK/rst"
    cp "$out/n3-disk-inc.qcow2" "$out/n3-disk.qcow2" "$WIN/"
}

stage_n4() {
    local src dst out=$WORK/out
    local smark=OBK-EXCLUDED-STREAM-MARKER
    loopdev src n4-src 256 && loopdev dst n4-dst 256 || return
    # The names stay here (nothing deletes them), so whatever is inside the
    # records stays too; a stream in clusters of its own must not — only the
    # MFT scan keeps it out, with no deletion pass behind it.
    mkntfs -F -Q -q "$src" && mnt3g "$src" "$WORK/m-n4" && mkdir -p "$WORK/m-n4/cache" \
        && printf '%s\n' "$MARK" >"$WORK/m-n4/cache/a.tmp" \
        && python3 -c 'import os,sys; os.setxattr(sys.argv[1], "user.big", (sys.argv[2] * 800).encode())' \
            "$WORK/m-n4/cache/a.tmp" "$smark" \
        && { printf 'HIBR'; head -c 65532 /dev/zero; } >"$WORK/m-n4/hiberfil.sys" && umnt "$WORK/m-n4"
    run n4 "$CLI" backup "$src" -o "$out/n4.qcow2" --format qcow2 --exclude cache --verify
    check n4-copy "$(verdict $?)" "a hibernated volume is still copied and verified"
    grep -q 'Excluded names kept' "$EV/n4.out"
    check n4-said "$(verdict $?)" "the result says the names were kept"
    ! grep -qa "$MARK" "$out/n4.qcow2"
    check n4-no-bytes "$(verdict $?)" "the excluded data is still left out"
    run n4-restore "$CLI" restore "$out/n4.qcow2" -t "$dst" --yes && ! grep -qa "$smark" "$dst"
    check n4-no-stream "$(verdict $?)" "an excluded file's named stream is not in the restored volume"
    no_leftovers n4
}

for stage in "${STAGES[@]}"; do
    if declare -F "stage_$stage" >/dev/null; then
        echo "== $stage"
        "stage_$stage"
    else
        check "$stage" SKIP "no such stage in this version of the scenario"
    fi
done

echo "== $CHECKS checks, $FAILS failed (evidence: $EV; for the Windows stand: $WIN)"
exit "$FAILS"
