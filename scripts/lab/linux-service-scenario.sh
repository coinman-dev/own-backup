#!/usr/bin/env bash
# Linux acceptance of the service as a daemon (CHECKS 11): one engine at a
# time, the session check on the socket, a stop in the middle of a copy and a
# killed service in the middle of a copy. Run as root inside a stand guest
# (`scripts/lab/linux-vm.py`) with the package installed and its service
# running:
#
#   sudo scripts/lab/linux-service-scenario.sh [SRC_DISK DEST_DISK]
#
# SRC_DISK and DEST_DISK (default: the stand's virtio-ob-data-2 and
# virtio-ob-data-3) are wiped: the source gets an ext4 with incompressible
# data, the destination an ext4 for the images. The copy is held to a speed
# limit so it lasts long enough to be cut. Each check prints
# `CHECK <id> PASS|FAIL <text>`; the exit code is the number of FAILs.
# Evidence: /var/tmp/obk-service-evidence.
set -uo pipefail

D_SRC=$(readlink -f "${1:-/dev/disk/by-id/virtio-ob-data-2}")
D_DST=$(readlink -f "${2:-/dev/disk/by-id/virtio-ob-data-3}")
EV=/var/tmp/obk-service-evidence
SRC_MNT=/mnt/obk-svc-src
IMG=/mnt/obk-svc-img
SVC=/usr/libexec/own-backup/own-backup-service
DATA=/var/lib/own-backup
FAILS=0

if [[ $(id -u) -ne 0 ]]; then
    echo "run as root" >&2
    exit 2
fi
for d in "$D_SRC" "$D_DST"; do [[ -b $d ]] || { echo "no disk $d" >&2; exit 2; }; done
systemctl is-active -q own-backup || { echo "the service is not running" >&2; exit 2; }
rm -rf "$EV" && mkdir -p "$EV"

check() { # id verdict text...
    local id=$1 verdict=$2
    shift 2
    [[ $verdict == FAIL ]] && FAILS=$((FAILS + 1))
    printf 'CHECK %-34s %-4s %s\n' "$id" "$verdict" "$*" | tee -a "$EV/summary.txt"
}
verdict() { if [[ $1 -eq 0 ]]; then echo PASS; else echo FAIL; fi; }
run() { # NAME CMD...: output to $EV/NAME.out, exit code returned
    local name=$1
    shift
    "$@" >"$EV/$name.out" 2>&1
}
wait_for() { # seconds command...
    local deadline=$((SECONDS + $1))
    shift
    until "$@"; do
        [[ $SECONDS -ge $deadline ]] && return 1
        sleep 1
    done
}
cli() { /usr/bin/own-backup-cli "$@"; }
job_id_named() {
    python3 -c 'import json,sys
print(next((j["id"] for j in json.load(open("/var/lib/own-backup/jobs.json")) if j.get("name")==sys.argv[1]),""))' "$1"
}
last_run() { # job-id -> the latest backup run of the job, as JSON
    python3 -c 'import json,sys
try: runs=json.load(open("/var/lib/own-backup/activity.json"))
except OSError: runs=[]
r=[a for a in runs if a.get("job_id")==sys.argv[1] and a.get("operation","backup") in ("","backup")]
print(json.dumps(max(r,key=lambda a:a.get("started_at","")) if r else {}))' "$1"
}
pending() { find "$IMG" -name '.own-backup-new-*' 2>/dev/null | grep -q .; }
no_pending() { ! pending; }
our_dm() { dmsetup ls 2>/dev/null | grep -q '^obk-'; }
main_pid() { systemctl show -P MainPID own-backup; }

cleanup() {
    for m in "$SRC_MNT" "$IMG"; do
        mountpoint -q "$m" && { umount "$m" || umount -l "$m"; }
    done
}
trap cleanup EXIT

# --- disks --------------------------------------------------------------------
wipefs -a -q "$D_SRC"* "$D_DST"* 2>/dev/null
parted -s "$D_SRC" mklabel gpt mkpart src ext4 1MiB 100% &&
    parted -s "$D_DST" mklabel gpt mkpart img ext4 1MiB 100% || exit 2
udevadm settle -t 10
SRC=$(lsblk -lnpo NAME "$D_SRC" | sed -n 2p)
DST=$(lsblk -lnpo NAME "$D_DST" | sed -n 2p)
mkfs.ext4 -q -F "$SRC" && mkfs.ext4 -q -F "$DST" || exit 2
mkdir -p "$SRC_MNT" "$IMG" && mount "$SRC" "$SRC_MNT" && mount "$DST" "$IMG" || exit 2
head -c 1536M /dev/urandom >"$SRC_MNT/random.bin" && sync
umount "$SRC_MNT"

# --- one engine at a time -------------------------------------------------------
pid_before=$(main_pid)
timeout 30 "$SVC" --run >"$EV/second.out" 2>&1
rc=$?
[[ $rc -ne 0 && $rc -ne 124 ]]
check second-instance-refused "$(verdict $?)" "a second engine exits at once (rc $rc): $(tail -1 "$EV/second.out")"
sleep 1
[[ $(main_pid) == "$pid_before" ]] && run status-after-second cli jobs status
check first-instance-untouched "$(verdict $?)" "the running service keeps its socket and answers"

# --- the session check ------------------------------------------------------------
"$SVC" --grant-access lab >/dev/null 2>&1
runuser -u lab -g own-backup -- /usr/bin/own-backup-cli jobs status >"$EV/ssh-user.out" 2>&1
rc=$?
[[ $rc -ne 0 ]]
check remote-session-refused "$(verdict $?)" "lab from an SSH session is refused (rc $rc): $(grep -m1 -oE '(Отказано|Denied)[^.]*' "$EV/ssh-user.out")"

# --- a job held to 25 MB/s (about a minute for 1.5 GiB) ---------------------------
run job-new cli jobs new --name svc-scenario --source "$SRC" --dest "$IMG" \
    --method full --no-exclusions --schedule none
job=$(job_id_named svc-scenario)
cli jobs show "$job" >"$EV/job.json" 2>/dev/null
python3 - "$EV/job.json" <<'PY'
import json, sys
p = sys.argv[1]
j = json.load(open(p))
j.setdefault("advanced", {})["speed_limit_mbps"] = 25
json.dump(j, open(p, "w"))
PY
run job-save cli jobs save --file "$EV/job.json"
[[ -n $job ]]
check job-created "$(verdict $?)" "job $job, 25 MB/s"

# --- systemctl stop in the middle of the copy -------------------------------------
run run-1 cli jobs run "$job"
wait_for 60 pending
check stop-copy-running "$(verdict $?)" "the copy writes its private image"
sleep 5
t0=$SECONDS
systemctl stop own-backup
rc=$?
took=$((SECONDS - t0))
[[ $rc -eq 0 && $(systemctl show -P ActiveState own-backup) == inactive ]]
check stop-graceful "$(verdict $?)" "systemctl stop returned after $took s (TimeoutStopSec 330)"
[[ $(systemctl show -P Result own-backup) == success ]]
check stop-clean-exit "$(verdict $?)" "the unit stopped cleanly (Result=$(systemctl show -P Result own-backup))"
last_run "$job" >"$EV/run-1.json"
python3 -c 'import json,sys; r=json.load(open(sys.argv[1])); sys.exit(0 if r.get("status")=="cancelled" and r.get("interrupted") else 1)' "$EV/run-1.json"
check stop-run-interrupted "$(verdict $?)" "the run is recorded as interrupted: $(cat "$EV/run-1.json" | head -c 300)"
no_pending
check stop-no-private-image "$(verdict $?)" "no private image left in the destination"
! our_dm
check stop-nothing-held "$(verdict $?)" "no obk-* device left"

systemctl start own-backup && wait_for 30 cli jobs status >/dev/null 2>&1
check restart "$(verdict $?)" "the service starts again"

# --- kill -9 in the middle of the copy -------------------------------------------
run run-2 cli jobs run "$job"
wait_for 60 pending
check kill-copy-running "$(verdict $?)" "the copy writes its private image"
sleep 5
ls -la "$IMG" "$DATA/pending-images" >"$EV/before-kill.txt" 2>&1
old=$(main_pid)
systemctl kill -s KILL own-backup
wait_for 60 bash -c "[[ \$(systemctl show -P MainPID own-backup) != 0 && \$(systemctl show -P MainPID own-backup) != $old ]] && systemctl is-active -q own-backup"
check kill-restarted "$(verdict $?)" "systemd restarted the killed service (Restart=on-failure)"
wait_for 60 no_pending
check kill-private-image-reclaimed "$(verdict $?)" "the killed run's private image is deleted at the next start"
ls -la "$IMG" "$DATA/pending-images" >"$EV/after-kill.txt" 2>&1
[[ -z $(find "$DATA/pending-images" -type f 2>/dev/null) ]]
check kill-ledger-empty "$(verdict $?)" "the ledger keeps no record of it"
! our_dm
check kill-no-dm "$(verdict $?)" "no obk-* device left"
# The killed run has a record of its own after the restart: interrupted,
# newer than the stopped run-1 (JobStore::record_killed_runs).
last_run "$job" >"$EV/run-2.json"
python3 -c 'import json,sys
r=json.load(open(sys.argv[1])); one=json.load(open(sys.argv[2]))
sys.exit(0 if r.get("id")!=one.get("id") and r.get("status")=="cancelled" and r.get("interrupted") else 1)' "$EV/run-2.json" "$EV/run-1.json"
check kill-run-recorded "$(verdict $?)" "the killed run is in the history as interrupted: $(python3 -c 'import json,sys; r=json.load(open(sys.argv[1])); print(r.get("started_at"), r.get("status"), (r.get("error") or "")[:90])' "$EV/run-2.json")"
[[ -z $(find /var/lib/own-backup/running -type f 2>/dev/null) ]]
check kill-note-cleared "$(verdict $?)" "no run-in-progress note is left"

# --- and a copy after all that ----------------------------------------------------
cli jobs show "$job" >"$EV/job.json" 2>/dev/null
python3 - "$EV/job.json" <<'PY'
import json, sys
p = sys.argv[1]
j = json.load(open(p))
j["advanced"]["speed_limit_mbps"] = 0
json.dump(j, open(p, "w"))
PY
run job-unpaced cli jobs save --file "$EV/job.json"
before=$(last_run "$job" | python3 -c 'import json,sys; print(json.load(sys.stdin).get("id",""))')
run run-3 cli jobs run "$job"
finished() { last_run "$job" | python3 -c 'import json,sys; r=json.load(sys.stdin); sys.exit(0 if r.get("id") not in ("",sys.argv[1]) and r.get("status")!="running" else 1)' "$before"; }
wait_for 600 finished
last_run "$job" >"$EV/run-3.json"
python3 -c 'import json,sys; sys.exit(0 if json.load(open(sys.argv[1])).get("status")=="success" else 1)' "$EV/run-3.json"
check copy-after-kill "$(verdict $?)" "a full copy succeeds afterwards"
find "$IMG" -maxdepth 2 -printf '%p %s\n' >"$EV/dest-final.txt"
no_pending
check final-no-private-image "$(verdict $?)" "only published images in the destination"

cli jobs delete "$job" >/dev/null 2>&1
echo "FAILS=$FAILS (evidence: $EV)"
exit "$FAILS"
