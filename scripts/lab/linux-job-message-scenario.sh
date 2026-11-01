#!/usr/bin/env bash
# Linux acceptance of the message about a started job and of the moving icon (CHECKS
# 57): the real service runs real copies, the real `own-backup-gui --tray` of a person
# with a local session watches them, and notifications-mock.py and sni-watcher.py stand
# in for the desktop. Run as root inside a stand guest (`scripts/lab/linux-vm.py`)
# prepared by `linux-gui-shot.sh` (install, seat, relogin, display):
#
#   sudo scripts/lab/linux-job-message-scenario.sh MOCK_DIR [SRC_DISK DEST_DISK]
#
# MOCK_DIR holds notifications-mock.py and sni-watcher.py. SRC_DISK and DEST_DISK
# (default: the stand's virtio-ob-data-1 and virtio-ob-data-2) are wiped. Each check
# prints `CHECK <id> PASS|FAIL <text>`; the exit code is the number of FAILs.
# Evidence: /var/tmp/obk-message-evidence.
set -uo pipefail

MOCKS=${1:?the folder of notifications-mock.py and sni-watcher.py}
D_SRC=$(readlink -f "${2:-/dev/disk/by-id/virtio-ob-data-1}")
D_DST=$(readlink -f "${3:-/dev/disk/by-id/virtio-ob-data-2}")
EV=/var/tmp/obk-message-evidence
IMG=/mnt/obk-msg-img
NAME='Диск для проверки'
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
    printf 'CHECK %-30s %-4s %s\n' "$id" "$verdict" "$*" | tee -a "$EV/summary.txt"
}
verdict() { if [[ $1 -eq 0 ]]; then echo PASS; else echo FAIL; fi; }
wait_for() { # seconds command...
    local deadline=$((SECONDS + $1))
    shift
    until "$@"; do
        [[ $SECONDS -ge $deadline ]] && return 1
        sleep 1
    done
}
cli() { /usr/bin/own-backup-cli "$@"; }
# A command in the session of `lab`, where the window and the icon live.
lab_uid=$(id -u lab)
as_lab() {
    runuser -u lab -- env XDG_RUNTIME_DIR="/run/user/$lab_uid" \
        DBUS_SESSION_BUS_ADDRESS="unix:path=/run/user/$lab_uid/bus" sh -c "$1"
}
messages() { as_lab "journalctl --user -u ob-notify --no-pager -o cat" | grep -c '^{"app"'; }
last_message() { as_lab "journalctl --user -u ob-notify --no-pager -o cat" | grep '^{"app"' | tail -1; }
frames() { as_lab "grep -c 'member=NewIcon' /tmp/ob-newicon.log 2>/dev/null" || true; }
has_messages() { [[ $(messages) -ge $1 ]]; }
pending() { find "$IMG" -name '.own-backup-new-*' 2>/dev/null | grep -q .; }
no_pending() { ! pending; }

start_tray() {
    as_lab "systemctl --user stop ob-tray 2>/dev/null; systemctl --user reset-failed ob-tray 2>/dev/null;
            systemd-run --user --unit=ob-tray --quiet --setenv=DISPLAY=:99 /usr/bin/own-backup-gui --tray"
}

cleanup() {
    as_lab 'systemctl --user stop ob-tray ob-notify ob-sni ob-newicon 2>/dev/null' 2>/dev/null
    mountpoint -q "$IMG" && { umount "$IMG" || umount -l "$IMG"; }
}
trap cleanup EXIT

# --- the desktop's stand-ins and the icon -------------------------------------------
as_lab "systemctl --user stop ob-tray ob-notify ob-sni ob-newicon 2>/dev/null;
        systemctl --user reset-failed ob-tray ob-notify ob-sni ob-newicon 2>/dev/null;
        rm -f /tmp/ob-newicon.log;
        systemd-run --user --unit=ob-notify --quiet python3 -u $MOCKS/notifications-mock.py;
        systemd-run --user --unit=ob-sni --quiet python3 -u $MOCKS/sni-watcher.py;
        systemd-run --user --unit=ob-newicon --quiet sh -c \"exec dbus-monitor --session \\\"type='signal',member='NewIcon'\\\" >/tmp/ob-newicon.log\""
sleep 2
start_tray
sleep 8
as_lab "journalctl --user -u ob-sni --no-pager -o cat" >"$EV/sni.txt"
grep -q '^registered' "$EV/sni.txt"
check icon-registered "$(verdict $?)" "the icon is in the notification area protocol"
[[ $(messages) -eq 0 ]]
check no-message-without-a-job "$(verdict $?)" "$(messages) messages while the queue is empty"

# --- disks and a job held to 20 MB/s (about 40 s for 800 MiB) -----------------------
wipefs -a -q "$D_SRC"* "$D_DST"* 2>/dev/null
parted -s "$D_SRC" mklabel gpt mkpart src ext4 1MiB 100% &&
    parted -s "$D_DST" mklabel gpt mkpart img ext4 1MiB 100% || exit 2
udevadm settle -t 10
SRC=$(lsblk -lnpo NAME "$D_SRC" | sed -n 2p)
DST=$(lsblk -lnpo NAME "$D_DST" | sed -n 2p)
mkfs.ext4 -q -F "$SRC" && mkfs.ext4 -q -F "$DST" || exit 2
mkdir -p /mnt/obk-msg-src "$IMG" && mount "$SRC" /mnt/obk-msg-src && mount "$DST" "$IMG" || exit 2
head -c 800M /dev/urandom >/mnt/obk-msg-src/random.bin && sync
umount /mnt/obk-msg-src

cli jobs new --name "$NAME" --source "$SRC" --dest "$IMG" \
    --method full --no-exclusions --schedule none >"$EV/job-new.out" 2>&1
job=$(python3 -c 'import json,sys
print(next((j["id"] for j in json.load(open("/var/lib/own-backup/jobs.json")) if j.get("name")==sys.argv[1]),""))' "$NAME")
cli jobs show "$job" >"$EV/job.json" 2>/dev/null
python3 - "$EV/job.json" <<'PY'
import json, sys
p = sys.argv[1]
j = json.load(open(p))
j.setdefault("advanced", {})["speed_limit_mbps"] = 20
json.dump(j, open(p, "w"))
PY
cli jobs save --file "$EV/job.json" >"$EV/job-save.out" 2>&1
[[ -n $job ]]
check job-created "$(verdict $?)" "job $job"

# --- the first copy -----------------------------------------------------------------
frames_before=$(frames)
cli jobs run "$job" >"$EV/run-1.out" 2>&1
wait_for 30 pending
check copy-running "$(verdict $?)" "the copy writes its private image"
wait_for 15 has_messages 1
check message-when-it-starts "$(verdict $?)" "$(last_message)"
last_message | grep -qF "$NAME"
check message-names-the-job "$(verdict $?)" "the job's name is in the message"
# The language is the system's, and a guest's is usually English.
last_message | grep -qE '"body": "(Началось резервное копирование|Backup started): '
check message-text "$(verdict $?)" "the text from the catalogue"
last_message | grep -q '"desktop_entry": "own-backup"'
check message-says-whose "$(verdict $?)" "the desktop entry is given"

sleep 6
frames_moving=$(frames)
[[ $((frames_moving - frames_before)) -ge 5 ]]
check icon-moves "$(verdict $?)" "$((frames_moving - frames_before)) new pictures since the job started"
[[ $(messages) -eq 1 ]]
check message-only-once "$(verdict $?)" "$(messages) message after several answers of the service"

# --- an icon started in the middle of a copy has nothing to announce ----------------
start_tray
sleep 8
pending
check still-copying "$(verdict $?)" "the copy is still running"
[[ $(messages) -eq 1 ]]
check no-message-for-a-running-job "$(verdict $?)" "$(messages) message after the icon restarted during the copy"

wait_for 180 no_pending
check copy-finished "$(verdict $?)" "the copy published its image"
sleep 5
frames_idle=$(frames)
sleep 6
[[ $(frames) -eq $frames_idle ]]
check icon-stops "$(verdict $?)" "$(( $(frames) - frames_idle )) new pictures with no job running"

# --- the same job again is news again -----------------------------------------------
cli jobs run "$job" >"$EV/run-2.out" 2>&1
wait_for 30 pending
wait_for 15 has_messages 2
check message-for-the-next-run "$(verdict $?)" "$(messages) messages after the second start"
wait_for 180 no_pending

as_lab "journalctl --user -u ob-notify --no-pager -o cat" >"$EV/messages.txt"
as_lab "find ~/.local/state/own-backup -name 'own-backup-tray-*.log' | xargs cat" >"$EV/tray.log" 2>&1
grep -q 'was not shown\|no session bus' "$EV/tray.log"
[[ $? -ne 0 ]]
check tray-log-clean "$(verdict $?)" "the icon's log has no complaint about the messages"

echo "FAILS=$FAILS"
exit "$FAILS"
