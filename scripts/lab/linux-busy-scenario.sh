#!/usr/bin/env bash
# Linux acceptance: which service operations may run together (b19a780) and
# a volume group on two disks (CHECKS 18). Run as root inside a stand guest
# with the package installed and its service running:
#
#   sudo scripts/lab/linux-busy-scenario.sh RPC [STORE SRC_A SRC_B]
#
# RPC is `cargo build -p own-backup-client --example linux_restore_rpc`.
# The disks (default the stand's virtio-ob-data-1..3) are wiped: STORE holds
# the copies (one folder per job), SRC_A gets 1.5 GiB of random data, SRC_B a
# small ext4. While job A copies at 25 MB/s (about a minute):
#   - job B's copy is verified — another chain: it runs beside the copy;
#   - job A's copies are verified — the same chain: refused, the copy named;
#   - job B's image is restored onto SRC_B — a restore takes the machine:
#     refused, the copy named;
#   - the boot-medium cache is cleared — another resource: beside it.
# Then a volume group across SRC_A and SRC_B, copied as the whole of SRC_A:
# the run succeeds and its summary says the group has a physical volume
# outside the copy. Each check prints `CHECK <id> PASS|FAIL <text>`; the exit
# code is the number of FAILs. Evidence: /var/tmp/obk-busy-evidence.
set -uo pipefail

RPC=$(readlink -f "${1:?RPC tool}")
STORE=$(readlink -f "${2:-/dev/disk/by-id/virtio-ob-data-1}")
A=$(readlink -f "${3:-/dev/disk/by-id/virtio-ob-data-2}")
B=$(readlink -f "${4:-/dev/disk/by-id/virtio-ob-data-3}")
EV=/var/tmp/obk-busy-evidence
IMG=/mnt/obk-busy-img
MNT=/mnt/obk-busy
FAILS=0
VG=obkbusyvg

[[ $(id -u) -eq 0 ]] || { echo "run as root" >&2; exit 2; }
for x in "$STORE" "$A" "$B"; do [[ -b $x ]] || { echo "no disk $x" >&2; exit 2; }; done
[[ -x $RPC ]] || { echo "no RPC tool $RPC" >&2; exit 2; }
systemctl is-active -q own-backup || { echo "the service is not running" >&2; exit 2; }
rm -rf "$EV" && mkdir -p "$EV" "$IMG" "$MNT"

check() { # id verdict text...
    local id=$1 verdict=$2
    shift 2
    [[ $verdict == FAIL ]] && FAILS=$((FAILS + 1))
    printf 'CHECK %-30s %-4s %s\n' "$id" "$verdict" "$*" | tee -a "$EV/summary.txt"
}
verdict() { if [[ $1 -eq 0 ]]; then echo PASS; else echo FAIL; fi; }
cli() { /usr/bin/own-backup-cli "$@"; }
part() { lsblk -lnpo NAME "$1" | sed -n 2p; }
job_id() {
    python3 -c 'import json,sys
print(next((j["id"] for j in json.load(open("/var/lib/own-backup/jobs.json")) if j.get("name")==sys.argv[1]),""))' "$1"
}
last_run() { # job-id -> JSON of its newest backup run
    python3 -c 'import json,sys
try: runs=json.load(open("/var/lib/own-backup/activity.json"))
except OSError: runs=[]
r=[a for a in runs if a.get("job_id")==sys.argv[1] and a.get("operation","") in ("","backup")]
print(json.dumps(max(r,key=lambda a:a.get("started_at","")) if r else {}))' "$1"
}
status_of() { last_run "$1" | python3 -c 'import json,sys; print(json.load(sys.stdin).get("status",""))'; }
wait_done() { # job-id: until its newest run is finished
    for _ in $(seq 1 300); do
        local s
        s=$(status_of "$1")
        [[ -n $s && $s != running ]] && return 0
        sleep 2
    done
    return 1
}
# Is a copy running now? (`jobs queue`: «Выполняется: —» when none.) Only one
# job runs at a time here, so any is the one asked about.
running() { cli jobs queue 2>/dev/null | grep '^Выполняется:' | grep -qv -- '—'; }
cleanup() {
    for m in "$MNT" "$IMG"; do mountpoint -q "$m" && { umount "$m" || umount -l "$m"; }; done
    vgs "$VG" >/dev/null 2>&1 && { vgchange -an "$VG" >/dev/null 2>&1; vgremove -ff -y "$VG" >/dev/null 2>&1; }
}
trap cleanup EXIT

# --- disks ------------------------------------------------------------------
wipefs -a -q "$STORE"* "$A"* "$B"* 2>/dev/null
(
    set -e
    for d in "$STORE" "$A" "$B"; do parted -s "$d" mklabel gpt mkpart p ext4 1MiB 100%; done
    udevadm settle -t 10
    mkfs.ext4 -q -F "$(part "$STORE")"
    mount "$(part "$STORE")" "$IMG"
    mkdir -p "$IMG/a" "$IMG/b"
    mkfs.ext4 -q -F "$(part "$A")"
    mkfs.ext4 -q -F "$(part "$B")"
    mount "$(part "$A")" "$MNT" && head -c 1536M /dev/urandom >"$MNT/random.bin" && umount "$MNT"
    mount "$(part "$B")" "$MNT" && seq 1 200000 >"$MNT/seq.txt" && umount "$MNT"
) >"$EV/setup.out" 2>&1
check setup "$(verdict $?)" "store $STORE, A $(part "$A") (1.5 GiB), B $(part "$B")"
PA=$(part "$A") PB=$(part "$B")

new_job() { # name source dest
    cli jobs new --name "$1" --source "$2" --dest "$3" --method full --schedule none \
        --no-exclusions >"$EV/new-$1.out" 2>&1
    job_id "$1"
}
jb=$(new_job busy-b "$PB" "$IMG/b")
cli jobs run "$jb" >/dev/null 2>&1
wait_done "$jb" && [[ $(status_of "$jb") == success ]]
check b-copy "$(verdict $?)" "job B copied: $(last_run "$jb" | python3 -c 'import json,sys; print(json.load(sys.stdin).get("image_path",""))')"
img_b=$(last_run "$jb" | python3 -c 'import json,sys; print(json.load(sys.stdin).get("image_path",""))')

# --- while job A copies -------------------------------------------------------
# A first copy at full speed, so A has copies to be verified; then the slow one.
ja=$(new_job busy-a "$PA" "$IMG/a")
cli jobs run "$ja" >/dev/null 2>&1
wait_done "$ja" && [[ $(status_of "$ja") == success ]]
check a-first-copy "$(verdict $?)" "job A's first copy, at full speed"
cli jobs show "$ja" >"$EV/job-a.json"
python3 - "$EV/job-a.json" <<'PY'
import json, sys
p = sys.argv[1]
j = json.load(open(p))
j.setdefault("advanced", {})["speed_limit_mbps"] = 25
json.dump(j, open(p, "w"))
PY
cli jobs save --file "$EV/job-a.json" >/dev/null 2>&1
before=$(last_run "$ja" | python3 -c 'import json,sys; print(json.load(sys.stdin).get("id",""))')
cli jobs run "$ja" >/dev/null 2>&1
for _ in $(seq 1 30); do running "$ja" && break; sleep 1; done
sleep 5
running "$ja"
check a-running "$(verdict $?)" "job A copies again (25 MB/s)"

cli jobs verify "$jb" >"$EV/verify-b.out" 2>&1
check beside-verify-other "$(verdict $?)" "B's copy verified while A copies: $(tail -1 "$EV/verify-b.out" | cut -c1-120)"
cli jobs verify "$ja" >"$EV/verify-a.out" 2>&1
rc=$?
[[ $rc -ne 0 ]] && grep -q 'busy-a' "$EV/verify-a.out"
check same-chain-refused "$(verdict $?)" "A's own copies are not verified meanwhile (exit $rc): $(grep -o 'Сейчас[^.]*\|Now[^.]*' "$EV/verify-a.out" | head -1)"
"$RPC" restore-volume "$img_b" "$PB" --keep >"$EV/restore-b.out" 2>&1
rc=$?
[[ $rc -ne 0 ]] && grep -q 'busy-a' "$EV/restore-b.out"
check restore-refused "$(verdict $?)" "a restore waits for the machine (exit $rc): $(grep -o '"message": "[^"]*' "$EV/restore-b.out" | cut -c13-140)"
cli media linux-cache --clear >"$EV/media-cache.out" 2>&1
check beside-media-cache "$(verdict $?)" "the medium builder's cache cleared while A copies"
running "$ja"
check a-still-running "$(verdict $?)" "A was not disturbed by any of it"
for _ in $(seq 1 300); do running "$ja" || break; sleep 2; done
after=$(last_run "$ja" | python3 -c 'import json,sys; print(json.load(sys.stdin).get("id",""))')
[[ $after != "$before" && $(status_of "$ja") == success ]]
check a-copy "$(verdict $?)" "job A's second copy finished: $(status_of "$ja")"

# --- a volume group on both disks, the whole of A copied ------------------------
(
    set -e
    wipefs -a -q "$PA" "$PB"
    pvcreate -ff -y "$PA" "$PB"
    vgcreate "$VG" "$PA" "$PB"
    lvcreate -y -Wy -Zy -l 100%FREE -n wide "$VG"
    mkfs.ext4 -q -F "/dev/$VG/wide"
    vgchange -an "$VG"
) >"$EV/vg.out" 2>&1
check vg-setup "$(verdict $?)" "VG $VG on $PA and $PB, one LV across both, inactive"
jv=$(new_job busy-vg "$PA" "$IMG/a")
cli jobs show "$jv" >"$EV/job-vg.json"
python3 - "$EV/job-vg.json" "$A" "$PA" <<'PY'
import json, sys
p, disk, pa = sys.argv[1:4]
j = json.load(open(p))
j["sources"] = []
j["disk_source"] = {"disk_number": 0, "device": disk, "disk_model": "", "bootable": False,
                    "parts": [{"part_index": 1, "device": pa, "filesystem": "LVM2_member"}]}
json.dump(j, open(p, "w"))
PY
cli jobs save --file "$EV/job-vg.json" >/dev/null 2>&1
cli jobs run "$jv" >/dev/null 2>&1
wait_done "$jv"
last_run "$jv" >"$EV/vg-run.json"
python3 -c 'import json,sys; r=json.load(open(sys.argv[1])); sys.exit(0 if r.get("status")=="success" else 1)' "$EV/vg-run.json"
check vg-copy "$(verdict $?)" "the whole of A copied: $(python3 -c 'import json,sys; r=json.load(open(sys.argv[1])); print(r.get("status"), (r.get("error") or "")[:120])' "$EV/vg-run.json")"
python3 -c 'import json,sys; n=" ".join(json.load(open(sys.argv[1])).get("notes") or []); print(n); sys.exit(0 if sys.argv[2] in n else 1)' "$EV/vg-run.json" "$VG" >"$EV/vg-note.out"
check vg-note "$(verdict $?)" "the summary names the group's PV outside the copy: $(cut -c1-160 "$EV/vg-note.out")"

for j in "$ja" "$jb" "$jv"; do cli jobs delete "$j" >/dev/null 2>&1; done
echo "FAILS=$FAILS (evidence: $EV)"
exit "$FAILS"
