#!/usr/bin/env bash
# Linux acceptance: a whole-disk restore puts back what the copy holds and
# leaves every other partition of the disk as it is (owner, 24.10: his system
# disk holds ESP, /boot and the LUKS root — the copy — and right after them a
# 347 GB BitLocker partition the copy leaves out). Run as root inside a stand
# guest with the package installed and its service running:
#
#   sudo scripts/lab/linux-keep-partitions-scenario.sh RPC [DISK STORE]
#
# RPC is `cargo build -p own-backup-client --example linux_restore_rpc`
# (target/debug/examples/linux_restore_rpc). DISK (default the stand's
# virtio-ob-data-2) is wiped and laid out like the owner's disk: an ESP from
# LBA 34 (not on a 1 MiB boundary), /boot, a root, and a "neighbour" right
# after them, filled with random bytes and with GPT attributes of its own.
# STORE (default virtio-ob-data-3) holds the copy. A service job copies
# partitions 1–3 of DISK; then the disk is restored three times:
#   cli   — the CLI's restore-disk (no plan): the copy's partitions back, the
#           neighbour kept, entry and all;
#   lost  — the copy's own table as a plan (where the window's designer
#           starts): refused, the neighbour named;
#   plan  — the same plan keeping the neighbour's entry: applied, the
#           neighbour kept.
# After each restore: the neighbour's bytes (sha256 of all of it) and its GPT
# entry (start, size, type, GUID, name, attributes) are those of before; the
# restored partitions pass their checkers and hold the files of the copy, not
# what was written after it. Each check prints `CHECK <id> PASS|FAIL <text>`;
# the exit code is the number of FAILs. Evidence: /var/tmp/obk-keep-evidence.
set -uo pipefail

RPC=$(readlink -f "${1:?RPC tool}")
D=$(readlink -f "${2:-/dev/disk/by-id/virtio-ob-data-2}")
S=$(readlink -f "${3:-/dev/disk/by-id/virtio-ob-data-3}")
EV=/var/tmp/obk-keep-evidence
MNT=/mnt/obk-keep
IMG=/mnt/obk-keep-img
FAILS=0

[[ $(id -u) -eq 0 ]] || { echo "run as root" >&2; exit 2; }
for x in "$D" "$S"; do [[ -b $x ]] || { echo "no disk $x" >&2; exit 2; }; done
[[ -x $RPC ]] || { echo "no RPC tool $RPC" >&2; exit 2; }
systemctl is-active -q own-backup || { echo "the service is not running" >&2; exit 2; }
rm -rf "$EV" && mkdir -p "$EV" "$MNT" "$IMG"

check() { # id verdict text...
    local id=$1 verdict=$2
    shift 2
    [[ $verdict == FAIL ]] && FAILS=$((FAILS + 1))
    printf 'CHECK %-30s %-4s %s\n' "$id" "$verdict" "$*" | tee -a "$EV/summary.txt"
}
verdict() { if [[ $1 -eq 0 ]]; then echo PASS; else echo FAIL; fi; }
cli() { /usr/bin/own-backup-cli "$@"; }
part() { lsblk -lnpo NAME "$D" | sed -n "$(($1 + 1))p"; }
cleanup() {
    for m in "$MNT" "$IMG"; do mountpoint -q "$m" && { umount "$m" || umount -l "$m"; }; done
}
trap cleanup EXIT

# --- the disk, laid out like the owner's ------------------------------------
wipefs -a -q "$D"* "$S"* 2>/dev/null
# ESP from LBA 34 to the 64 MiB boundary, /boot 256 MiB, root 1.5 GiB, the
# neighbour from right after the root to the end.
printf '%s\n' 'label: gpt' 'first-lba: 34' \
    'start=34, size=131038, type=C12A7328-F81F-11D2-BA4B-00A0C93EC93B, name="EFI system partition"' \
    'start=131072, size=524288, type=0FC63DAF-8483-4772-8E79-3D69D8477DE4' \
    'start=655360, size=3145728, type=0FC63DAF-8483-4772-8E79-3D69D8477DE4' \
    'start=3801088, type=EBD0A0A2-B9E5-4433-87C0-68B6B72699C7, name="Neighbour data"' |
    sfdisk -q "$D" >"$EV/sfdisk.out" 2>&1
sfdisk -q --part-attrs "$D" 4 "GUID:63" >>"$EV/sfdisk.out" 2>&1
udevadm settle -t 10
P1=$(part 1) P2=$(part 2) P3=$(part 3) P4=$(part 4)
(
    set -e
    mkfs.vfat -F 32 "$P1"
    mkfs.ext4 -q -F -L obk-boot "$P2"
    mkfs.ext4 -q -F -L obk-root "$P3"
    for p in "$P1" "$P2" "$P3"; do
        mount "$p" "$MNT"
        mkdir -p "$MNT/data"
        head -c 3M /dev/urandom >"$MNT/data/a.bin"
        seq 1 50000 >"$MNT/data/seq.txt"
        umount "$MNT"
    done
    # The neighbour: random bytes all through, its first MiB too (where a
    # BitLocker volume keeps its header).
    dd if=/dev/urandom of="$P4" bs=4M status=none conv=fsync || true
    mkfs.ext4 -q -F "$S"
    mount "$S" "$IMG"
) >"$EV/setup.out" 2>&1
check setup "$(verdict $?)" "$D: ESP@34 $P1, $P2, $P3, neighbour $P4 ($(blockdev --getsize64 "$P4") bytes)"
[[ $(cat "/sys/class/block/$(basename "$P1")/start") -eq 34 ]]
check esp-unaligned "$(verdict $?)" "the ESP starts at LBA 34, as on the owner's disk"

neighbour() { # the neighbour as it is: its bytes and its GPT entry
    echo "sha256 $(sha256sum "$P4" | cut -d' ' -f1)"
    echo "start $(cat "/sys/class/block/$(basename "$P4")/start")"
    echo "size $(cat "/sys/class/block/$(basename "$P4")/size")"
    echo "type $(sfdisk --part-type "$D" 4)"
    echo "uuid $(sfdisk --part-uuid "$D" 4)"
    echo "name $(sfdisk --part-label "$D" 4)"
    echo "attrs $(sfdisk --part-attrs "$D" 4)"
}
files_of() { # partition -> hashes of its files
    mount -o ro "$1" "$MNT" && (cd "$MNT" && find . -type f -print0 | sort -z | xargs -0 sha256sum)
    umount "$MNT"
}
neighbour >"$EV/neighbour.before"
for i in 1 2 3; do files_of "$(part $i)" >"$EV/files-$i.before"; done
cat "$EV/neighbour.before"

# --- the copy: partitions 1–3 through a service job -------------------------
cli jobs new --name keep-disk --source "$P2" --dest "$IMG" --method full \
    --schedule none --no-exclusions >"$EV/job-new.out" 2>&1
job=$(python3 -c 'import json,sys
print(next((j["id"] for j in json.load(open("/var/lib/own-backup/jobs.json")) if j.get("name")==sys.argv[1]),""))' keep-disk)
cli jobs show "$job" >"$EV/job.json"
python3 - "$EV/job.json" "$D" "$P1" "$P2" "$P3" <<'PY'
import json, sys
p, disk, p1, p2, p3 = sys.argv[1:6]
j = json.load(open(p))
j["sources"] = []
j["disk_source"] = {
    "disk_number": 0, "device": disk, "disk_model": "", "bootable": True,
    "parts": [
        {"part_index": 1, "device": p1, "filesystem": "vfat"},
        {"part_index": 2, "device": p2, "filesystem": "ext4"},
        {"part_index": 3, "device": p3, "filesystem": "ext4"},
    ],
}
json.dump(j, open(p, "w"))
PY
cli jobs save --file "$EV/job.json" >"$EV/job-save.out" 2>&1
check job "$(verdict $?)" "whole-disk job $job: partitions 1–3 of $D, not the neighbour"
cli jobs run "$job" >"$EV/run.out" 2>&1
for _ in $(seq 1 300); do
    st=$(python3 -c 'import json,sys
r=[a for a in json.load(open("/var/lib/own-backup/activity.json")) if a.get("job_id")==sys.argv[1]]
print(r[0].get("status","") if r else "")' "$job" 2>/dev/null)
    [[ -n $st && $st != running ]] && break
    sleep 2
done
image=$(python3 -c 'import json,sys
r=[a for a in json.load(open("/var/lib/own-backup/activity.json")) if a.get("job_id")==sys.argv[1]]
print(r[0].get("image_path",""))' "$job")
[[ $st == success && -f $image ]]
check copy "$(verdict $?)" "the copy: $st $image"

changed() { # write something after the copy, so a restore must undo it
    for i in 1 2 3; do
        mount "$(part $i)" "$MNT" && echo "after the copy" >"$MNT/after.txt" && umount "$MNT"
    done
}
restored() { # id: the copy's partitions are back, the neighbour untouched
    local id=$1 i
    udevadm settle -t 10
    neighbour >"$EV/neighbour.$id"
    diff "$EV/neighbour.before" "$EV/neighbour.$id" >"$EV/neighbour.$id.diff"
    check "$id-neighbour" "$(verdict $?)" "the neighbour: same bytes and GPT entry ($(wc -l <"$EV/neighbour.$id.diff") lines differ)"
    for i in 1 2 3; do
        local p
        p=$(part $i)
        if [[ $i -eq 1 ]]; then fsck.vfat -n "$p" >"$EV/$id-fsck-$i.out" 2>&1; else e2fsck -fn "$p" >"$EV/$id-fsck-$i.out" 2>&1; fi
        check "$id-fsck-$i" "$(verdict $?)" "partition $i clean after the restore"
        files_of "$p" >"$EV/files-$i.$id"
        diff "$EV/files-$i.before" "$EV/files-$i.$id" >/dev/null
        check "$id-files-$i" "$(verdict $?)" "partition $i holds the copy's files, not what came after"
    done
}

# --- cli: restore-disk, no plan ----------------------------------------------
changed
"$RPC" restore-disk "$image" "$D" --keep >"$EV/cli.out" 2>&1
check cli-restore "$(verdict $?)" "restore-disk $(basename "$image") -> $D"
grep -q 'Neighbour data\|оставлены как были\|left as they were' "$EV/cli.out"
check cli-said-kept "$(verdict $?)" "the result says the neighbour was left as it was"
restored cli

# --- lost: the copy's own table as a plan --------------------------------------
changed
"$RPC" plan-disk "$image" "$D" "$EV/plan-copy.json" >"$EV/plan-copy.out" 2>&1
"$RPC" dry-run "$EV/plan-copy.json" >"$EV/lost.out" 2>&1
grep -q 'existing_partition_lost' "$EV/lost.out" && grep -q 'Neighbour data\|4 (' "$EV/lost.out"
check lost-refused "$(verdict $?)" "the copy's table alone is refused, the neighbour named: $(grep -o '"message": "[^"]*Neighbour[^"]*' "$EV/lost.out" | head -1 | cut -c1-160)"
"$RPC" apply "$EV/plan-copy.json" >"$EV/lost-apply.out" 2>&1
rc=$?
neighbour >"$EV/neighbour.lost"
[[ $rc -ne 0 ]] && diff -q "$EV/neighbour.before" "$EV/neighbour.lost" >/dev/null
check lost-not-applied "$(verdict $?)" "applying it anyway is refused (exit $rc) and the disk is as it was"

# --- plan: the same plan, the neighbour's entry kept ---------------------------
python3 - "$EV/plan-copy.json" "$EV/plan-keep.json" \
    "$(cat "/sys/class/block/$(basename "$P4")/start")" \
    "$(cat "/sys/class/block/$(basename "$P4")/size")" <<'PY'
import json, sys
src, dst, start, size = sys.argv[1], sys.argv[2], int(sys.argv[3]), int(sys.argv[4])
plan = json.load(open(src))
for op in plan["ops"]:
    if op["op"] == "write_partition_table":
        # As the window's designer would send it: the volume's own facts,
        # no GUID, no attributes, no GPT name — the service takes those
        # from the disk.
        op["table"]["entries"].append({
            "index": 4, "first_lba": start, "last_lba": start + size - 1,
            "kind": {"scheme": "gpt", "type_guid": "EBD0A0A2-B9E5-4433-87C0-68B6B72699C7",
                     "part_guid": "", "attributes": 0, "name": ""},
        })
json.dump(plan, open(dst, "w"))
PY
"$RPC" dry-run "$EV/plan-keep.json" >"$EV/plan-dry.out" 2>&1
! grep -q 'existing_partition_lost' "$EV/plan-dry.out"
check plan-dry-run "$(verdict $?)" "keeping the neighbour's entry, nothing is said to be lost"
"$RPC" apply "$EV/plan-keep.json" >"$EV/plan-apply.out" 2>&1
check plan-apply "$(verdict $?)" "the plan is applied"
restored plan

cli jobs delete "$job" >/dev/null 2>&1
echo "FAILS=$FAILS (evidence: $EV)"
exit "$FAILS"
