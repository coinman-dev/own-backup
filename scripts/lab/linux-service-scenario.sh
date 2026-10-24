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
# limit so it lasts long enough to be cut. Then the source disk becomes two
# LVM PV partitions of one volume group (a volume on each, one on both, two of
# them mounted and one written during the copy) copied whole by a service job:
# one suspend for all, the second PV held by the first one's layers, every
# layer released, no metadata written. Last, a whole disk with an ESP and a
# btrfs whose excluded files are partly kept by name: the run says how many,
# and the image's btrfs lists exactly that many. Each check prints
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
check remote-session-refused "$(verdict $?)" "lab from an SSH session is refused (rc $rc): $(grep -m1 -oE '(Отказано|Access denied|Denied)[^.]*' "$EV/ssh-user.out")"

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
finished() { last_run "$job" | python3 -c 'import json,sys; r=json.load(sys.stdin); sys.exit(0 if r.get("id","") not in ("",sys.argv[1]) and r.get("status")!="running" else 1)' "$before"; }
wait_for 600 finished
last_run "$job" >"$EV/run-3.json"
python3 -c 'import json,sys; sys.exit(0 if json.load(open(sys.argv[1])).get("status")=="success" else 1)' "$EV/run-3.json"
check copy-after-kill "$(verdict $?)" "a full copy succeeds afterwards"
find "$IMG" -maxdepth 2 -printf '%p %s\n' >"$EV/dest-final.txt"
no_pending
check final-no-private-image "$(verdict $?)" "only published images in the destination"

cli jobs delete "$job" >/dev/null 2>&1

# --- a disk whose volume group lies on two of its partitions, through the service
# One VG on PV1 and PV2 of the source disk: `a` (PV1, mounted, written during
# the copy), `span` (on both, mounted), `b` (PV2 only, active, unmounted). The
# service keeps one snapshot provider for the whole disk (2f2a46b; before,
# each partition had its own and the layers were put back by id from the
# kernel's state): all three volumes must be held in one suspend, the second
# PV taken as held by the first one's layers, and every layer gone afterwards.
PVG=obksvcpv
PV_MNT=/mnt/obk-svc-pv
pv_cleanup() {
    pkill -f obk-svc-churn 2>/dev/null
    for m in "$PV_MNT"-a "$PV_MNT"-span; do
        mountpoint -q "$m" && { umount "$m" || umount -l "$m"; }
    done
    vgs "$PVG" >/dev/null 2>&1 && { vgchange -an "$PVG" >/dev/null 2>&1; vgremove -ff -y "$PVG" >/dev/null 2>&1; }
    wipefs -a -q "$D_SRC"?* 2>/dev/null
    pvremove -ff -y "$D_SRC"?* >/dev/null 2>&1
    wipefs -a -q "$D_SRC" 2>/dev/null
}
trap 'pv_cleanup; cleanup' EXIT
pv_tables() { for lv in a span b; do dmsetup table "$PVG-$lv" 2>&1; done | sha256sum | cut -d' ' -f1; }

pv_cleanup
parted -s "$D_SRC" mklabel gpt mkpart pv1 1MiB 50% mkpart pv2 50% 95% set 1 lvm on set 2 lvm on &&
    udevadm settle -t 10
PV1=$(lsblk -lnpo NAME "$D_SRC" | sed -n 2p)
PV2=$(lsblk -lnpo NAME "$D_SRC" | sed -n 3p)
(
    set -e
    pvcreate -ff -y "$PV1" "$PV2"
    vgcreate "$PVG" "$PV1" "$PV2"
    lvcreate -y -Wy -Zy -L 300M -n a "$PVG" "$PV1"
    lvcreate -y -Wy -Zy -L 200M -n span "$PVG" "$PV1"
    lvextend -y -L +200M "$PVG/span" "$PV2"
    lvcreate -y -Wy -Zy -L 100M -n b "$PVG" "$PV2"
    for lv in a span b; do mkfs.ext4 -q -F -L "obk-svc-$lv" "/dev/$PVG/$lv"; done
    mkdir -p "$PV_MNT"-a "$PV_MNT"-span
    mount "/dev/$PVG/a" "$PV_MNT"-a
    mount "/dev/$PVG/span" "$PV_MNT"-span
    head -c 150M /dev/urandom >"$PV_MNT"-a/a.bin
    head -c 300M /dev/urandom >"$PV_MNT"-span/span.bin
    sync
    lvs -a -o lv_name,lv_size,devices,seg_pe_ranges "$PVG"
) >"$EV/pv-setup.out" 2>&1
check pv-setup "$(verdict $?)" "VG $PVG on $PV1 and $PV2: a (PV1), span (both), b (PV2)"
(cd "$PV_MNT"-a && sha256sum a.bin) >"$EV/pv-a.sha256"
(cd "$PV_MNT"-span && sha256sum span.bin) >"$EV/pv-span.sha256"
tables=$(pv_tables)
seqno=$(vgs --noheadings -o vg_seqno "$PVG" | tr -d ' ')

run pv-job-new cli jobs new --name svc-pv-disk --source "$PV1" --dest "$IMG" \
    --method full --no-exclusions --schedule none --validate-after
pvjob=$(job_id_named svc-pv-disk)
cli jobs show "$pvjob" >"$EV/pv-job.json" 2>/dev/null
python3 - "$EV/pv-job.json" "$D_SRC" "$PV1" "$PV2" <<'PY'
import json, sys
p, disk, pv1, pv2 = sys.argv[1:5]
j = json.load(open(p))
j["sources"] = []
j["disk_source"] = {
    "disk_number": 0,
    "device": disk,
    "disk_model": "",
    "parts": [
        {"part_index": 1, "device": pv1, "filesystem": "LVM2_member"},
        {"part_index": 2, "device": pv2, "filesystem": "LVM2_member"},
    ],
    "bootable": False,
}
json.dump(j, open(p, "w"))
PY
run pv-job-save cli jobs save --file "$EV/pv-job.json"
[[ -n $pvjob ]]
check pv-job-created "$(verdict $?)" "whole-disk job $pvjob over $D_SRC (partitions 1 and 2)"

# Writes to `a` throughout the copy.
(exec -a obk-svc-churn bash -c 'i=0; while :; do dd if=/dev/urandom of="$0/c$((i % 20))" bs=64K count=4 conv=fsync status=none || exit 1; i=$((i + 1)); sleep 0.05; done' "$PV_MNT"-a) &
churn=$!
since=$(date '+%Y-%m-%d %H:%M:%S')
before=$(last_run "$pvjob" | python3 -c 'import json,sys; print(json.load(sys.stdin).get("id",""))')
run pv-run cli jobs run "$pvjob"
pv_finished() { last_run "$pvjob" | python3 -c 'import json,sys; r=json.load(sys.stdin); sys.exit(0 if r.get("id","") not in ("",sys.argv[1]) and r.get("status")!="running" else 1)' "$before"; }
wait_for 900 pv_finished
kill "$churn" 2>/dev/null
wait "$churn"
churn_rc=$?
pkill -f obk-svc-churn 2>/dev/null
last_run "$pvjob" >"$EV/pv-run.json"
journalctl -u own-backup --since "$since" -o cat >"$EV/pv-journal.txt" 2>&1
python3 -c 'import json,sys; sys.exit(0 if json.load(open(sys.argv[1])).get("status")=="success" else 1)' "$EV/pv-run.json"
check pv-run-success "$(verdict $?)" "$(python3 -c 'import json,sys; r=json.load(open(sys.argv[1])); print(r.get("status"), r.get("image_path"), (r.get("error") or "")[:160], r.get("notes"))' "$EV/pv-run.json")"
[[ $(grep -c 'device-mapper snapshot created' "$EV/pv-journal.txt") -eq 3 ]]
check pv-one-suspend "$(verdict $?)" "a, span and b held by device-mapper snapshots taken together ($(grep -c 'device-mapper snapshot created' "$EV/pv-journal.txt") created)"
! grep -q 'снимок прерванного копирования\|snapshot of an interrupted backup' "$EV/pv-journal.txt" "$EV/pv-run.json"
check pv-second-pv-held "$(verdict $?)" "the second PV is taken as held by the first one's layers, not refused"
released=$(($(grep -c 'device-mapper snapshot removed' "$EV/pv-journal.txt") + $(grep -c 'volume put back on its own table' "$EV/pv-journal.txt")))
[[ $released -ge 3 ]]
check pv-released "$(verdict $?)" "every snapshot taken is released again ($released of 3)"
! our_dm && [[ $(pv_tables) == "$tables" ]]
check pv-nothing-held "$(verdict $?)" "no obk-* device left, a/span/b on exactly their own tables"
[[ $(vgs --noheadings -o vg_seqno "$PVG" | tr -d ' ') == "$seqno" ]]
check pv-no-metadata-written "$(verdict $?)" "VG seqno $seqno unchanged"
python3 -c 'import json,sys; sys.exit(0 if not json.load(open(sys.argv[1])).get("notes") else 1)' "$EV/pv-run.json"
check pv-no-false-note "$(verdict $?)" "both PVs in the copy: no note about PVs outside it"
[[ $churn_rc -eq 0 || $churn_rc -eq 143 ]] && findmnt -no OPTIONS "$PV_MNT"-a | grep -qw rw
check pv-source-stayed-rw "$(verdict $?)" "a was written throughout the copy and stays rw (writer exit $churn_rc)"
(cd "$PV_MNT"-a && sha256sum -c --quiet "$EV/pv-a.sha256") && (cd "$PV_MNT"-span && sha256sum -c --quiet "$EV/pv-span.sha256")
check pv-sources-intact "$(verdict $?)" "the files on a and span still match"
cli jobs delete "$pvjob" >/dev/null 2>&1

# --- excluded files a btrfs keeps by name, a whole disk through the service ------
# An ESP and an idle btrfs with a swap file and 600 small files, every third
# one excluded: the swap file stays listed (its header is kept), and Stage 2
# may spare more for the leaves' sake. The run must say how many, and that
# number must be the excluded names the image's btrfs still lists (CHECKS 21).
KN_MNT=/mnt/obk-svc-kn
KN_OUT=/var/tmp/obk-svc-kn-restored
kn_cleanup() {
    mountpoint -q "$KN_MNT" && umount "$KN_MNT"
    qemu-nbd -d /dev/nbd0 >/dev/null 2>&1
    rm -rf "$KN_OUT"
}
trap 'kn_cleanup; pv_cleanup; cleanup' EXIT
pv_cleanup
parted -s "$D_SRC" mklabel gpt mkpart esp fat32 1MiB 65MiB set 1 esp on \
    mkpart root btrfs 65MiB 1100MiB && udevadm settle -t 10
KP1=$(lsblk -lnpo NAME "$D_SRC" | sed -n 2p)
KP2=$(lsblk -lnpo NAME "$D_SRC" | sed -n 3p)
(
    set -e
    mkfs.vfat -F 32 "$KP1"
    mkfs.btrfs -q -f "$KP2"
    mkdir -p "$KN_MNT"
    mount "$KP2" "$KN_MNT"
    mkdir -p "$KN_MNT/data/logs" "$KN_MNT/data/cache"
    for i in $(seq 0 599); do
        ext=keep
        ((i % 3 == 0)) && ext=log
        head -c 8192 /dev/urandom >"$KN_MNT/data/logs/f$(printf %04d "$i").$ext"
    done
    head -c 4M /dev/urandom >"$KN_MNT/data/cache/junk.bin"
    btrfs filesystem mkswapfile --size 64m "$KN_MNT/swapfile"
    sync
    umount "$KN_MNT"
) >"$EV/kn-setup.out" 2>&1
check kn-setup "$(verdict $?)" "ESP $KP1, btrfs $KP2: 600 files (200 *.log), data/cache, a swap file"

run kn-job-new cli jobs new --name svc-kn-disk --source "$KP2" --dest "$IMG" \
    --method full --schedule none --validate-after
knjob=$(job_id_named svc-kn-disk)
cli jobs show "$knjob" >"$EV/kn-job.json" 2>/dev/null
python3 - "$EV/kn-job.json" "$D_SRC" "$KP1" "$KP2" <<'PY'
import json, sys
p, disk, p1, p2 = sys.argv[1:5]
j = json.load(open(p))
j["sources"] = []
j["disk_source"] = {
    "disk_number": 0,
    "device": disk,
    "disk_model": "",
    "parts": [
        {"part_index": 1, "device": p1, "filesystem": "vfat"},
        {"part_index": 2, "device": p2, "filesystem": "btrfs"},
    ],
    "bootable": False,
}
j["exclusions"]["enabled"] = True
j["exclusions"]["patterns_linux"] = ["*.log", "data/cache/**", "swapfile"]
json.dump(j, open(p, "w"))
PY
run kn-job-save cli jobs save --file "$EV/kn-job.json"
[[ -n $knjob ]]
check kn-job-created "$(verdict $?)" "whole-disk job $knjob over $D_SRC with Linux exclusions"

since=$(date '+%Y-%m-%d %H:%M:%S')
before=$(last_run "$knjob" | python3 -c 'import json,sys; print(json.load(sys.stdin).get("id",""))')
run kn-run cli jobs run "$knjob"
kn_finished() { last_run "$knjob" | python3 -c 'import json,sys; r=json.load(sys.stdin); sys.exit(0 if r.get("id","") not in ("",sys.argv[1]) and r.get("status")!="running" else 1)' "$before"; }
wait_for 600 kn_finished
last_run "$knjob" >"$EV/kn-run.json"
journalctl -u own-backup --since "$since" -o cat >"$EV/kn-journal.txt" 2>&1
python3 -c 'import json,sys; sys.exit(0 if json.load(open(sys.argv[1])).get("status")=="success" else 1)' "$EV/kn-run.json"
check kn-run-success "$(verdict $?)" "$(python3 -c 'import json,sys; r=json.load(open(sys.argv[1])); print(r.get("status"), r.get("image_path"), r.get("excluded_files"), (r.get("error") or "")[:160])' "$EV/kn-run.json")"
# "N из M исключённых файлов (btrfs) …" / "N of M excluded files (btrfs) …"
read -r said_kept said_total said_fs < <(python3 - "$EV/kn-run.json" <<'PY'
import json, re, sys
r = json.load(open(sys.argv[1]))
m = re.match(r"(\d+) (?:из|of) (\d+) \S+ \S+ \(([^)]*)\)", r.get("excluded_entries_kept") or "")
print(*(m.groups() if m else ("-", "-", "-")))
PY
)
run_excluded=$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1])).get("excluded_files"))' "$EV/kn-run.json")
[[ $said_kept =~ ^[0-9]+$ && $said_kept -ge 1 && $said_total == "$run_excluded" && $said_fs == btrfs ]]
check kn-said "$(verdict $?)" "the run says $said_kept of $said_total ($said_fs) kept by name; excluded $run_excluded: $(python3 -c 'import json,sys; print(json.load(open(sys.argv[1])).get("excluded_entries_kept"))' "$EV/kn-run.json")"

# What the image's btrfs really lists: read through qemu-nbd, unmounted (the
# source's fsid is already known to the kernel), with btrfs's own tools.
image=$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1])).get("image_path",""))' "$EV/kn-run.json")
modprobe nbd max_part=8 2>/dev/null
rm -rf "$KN_OUT" && mkdir -p "$KN_OUT"
(
    set -e
    qemu-nbd -r -c /dev/nbd0 "$image"
    udevadm settle -t 10
    btrfs check --readonly /dev/nbd0p2
    btrfs restore /dev/nbd0p2 "$KN_OUT"
) >"$EV/kn-image.out" 2>&1
check kn-image-clean "$(verdict $?)" "btrfs check clean on the image's partition 2, files read back"
listed=$(find "$KN_OUT" -type f \( -name '*.log' -o -path '*/data/cache/*' -o -name swapfile \) | wc -l)
# The swap file keeps its 4 KiB header (as on ext): only the rest is zeroed.
nonzero=$(find "$KN_OUT" -type f \( -name '*.log' -o -path '*/data/cache/*' \) \
    -exec sh -c 'tr -d "\000" <"$1" | head -c1 | grep -q .' _ {} \; -print | wc -l)
swap=$(find "$KN_OUT" -type f -name swapfile | head -1)
if [[ -n $swap ]] && tail -c +4097 "$swap" | tr -d '\000' | head -c1 | grep -q .; then
    nonzero=$((nonzero + 1))
fi
[[ $listed == "$said_kept" ]]
check kn-count-true "$(verdict $?)" "excluded names still in the image: $listed, the run said $said_kept"
[[ $nonzero -eq 0 ]]
check kn-kept-zeroed "$(verdict $?)" "every one of them holds only zeros ($nonzero with data)"
kept_ok=$(find "$KN_OUT" -type f -name '*.keep' | wc -l)
[[ $kept_ok -eq 400 ]]
check kn-others-kept "$(verdict $?)" "all 400 *.keep files are in the image ($kept_ok)"
kn_cleanup
cli jobs delete "$knjob" >/dev/null 2>&1

echo "FAILS=$FAILS (evidence: $EV)"
exit "$FAILS"
