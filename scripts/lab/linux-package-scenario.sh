#!/usr/bin/env bash
# Linux acceptance scenario for the .deb (plan §6, stages 4 and 7): install,
# what the package sets up, the logind locks, an upgrade DURING a copy, the
# after-backup poweroff, remove and purge.
#
#   scripts/build-deb.sh                                  # on the host
#   python3 -I scripts/lab/linux-vm.py up --disks 2 --size 7
#   scripts/lab/linux-package-scenario.sh [path/to/own-backup_*.deb]
#
# The host part copies the package (default: the newest in target/dist) and
# this file into the guest and runs `this-file guest <deb>` there as root.
# The guest makes an "upgrade" package from the same one (version + ".1") to
# install over a running copy. Only the data disks with the serials
# ob-data-1..2 are touched; the package is purged at the end. Evidence:
# ~/lab/linux/run/evidence/package-<UTC>/ (guest/summary.txt holds the
# verdicts). Exit 0 only when every check passed.
#
# Not covered here: suspend and the RTC wake-up (the guest would have to be
# resumed from the host), and the polkit password dialog (needs a desktop
# session with an agent) — those are checked by hand on a desktop machine.
set -uo pipefail

# ---------------------------------------------------------------------------
# Guest part (root, inside the VM)
# ---------------------------------------------------------------------------

EV=/var/tmp/obk-evidence
FAILS=0
CHECKS=0
SRC_MNT=/mnt/obk-pkg-src
IMG=/mnt/obk-pkg-images
SVC=/usr/libexec/own-backup/own-backup-service
LEGACY=/etc/systemd/system/own-backup.service

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
    echo "### $name: $*" >"$EV/$name.out"
    "$@" >>"$EV/$name.out" 2>&1
    local rc=$?
    echo "### exit $rc" >>"$EV/$name.out"
    sed 's/^/    /' "$EV/$name.out"
    return "$rc"
}

part_of() { # disk-link partition-number -> canonical partition device
    local link="$1-part$2"
    for _ in $(seq 1 50); do
        [[ -b $link ]] && { readlink -f "$link"; return 0; }
        udevadm settle -t 2 2>/dev/null
        sleep 0.2
    done
    return 1
}

cli() { /usr/bin/own-backup-cli "$@"; }

# The job's latest run from the engine's own history (a JSON list, root-only).
last_run() { # job-id
    python3 -c 'import json,sys
try: runs=json.load(open("/var/lib/own-backup/activity.json"))
except OSError: runs=[]
r=[a for a in runs if a.get("job_id")==sys.argv[1] and a.get("operation","backup") in ("","backup")]
print(json.dumps(max(r,key=lambda a:a.get("started_at","")) if r else {}))' "$1"
}

job_id_named() { # name
    python3 -c 'import json,sys
print(next((j["id"] for j in json.load(open("/var/lib/own-backup/jobs.json")) if j.get("name")==sys.argv[1]),""))' "$1"
}

wait_for() { # seconds command...
    local deadline=$((SECONDS + $1))
    shift
    until "$@"; do
        [[ $SECONDS -ge $deadline ]] && return 1
        sleep 1
    done
}

inhibited_by_us() { # mode
    systemd-inhibit --list --no-pager --no-legend --mode="$1" 2>/dev/null | grep -q 'Own Backup'
}

guest_cleanup() {
    # Never leave the stand counting down to a poweroff.
    shutdown -c 2>/dev/null
    dpkg -s own-backup >/dev/null 2>&1 && apt-get purge -y -qq own-backup >/dev/null 2>&1
    rm -f "$LEGACY"
    systemctl daemon-reload 2>/dev/null
    for m in "$SRC_MNT" "$IMG"; do
        mountpoint -q "$m" && { umount "$m" || umount -l "$m"; }
        [[ -d $m ]] && rmdir "$m"
    done
    for d in "${D1:-}" "${D2:-}"; do
        [[ -b $d ]] || continue
        wipefs -a -q "$d"-part* "$d" 2>/dev/null
    done
    id lab2 >/dev/null 2>&1 && userdel lab2
    udevadm settle -t 10 2>/dev/null
}

# An "upgrade" of the same package: version + ".1", nothing else changed.
make_upgrade() { # deb out
    local work=/tmp/obk-pkg-upgrade
    rm -rf "$work" && mkdir -p "$work"
    dpkg-deb -R "$1" "$work/root" || return 1
    sed -i 's/^Version: \(.*\)$/Version: \1.1/' "$work/root/DEBIAN/control"
    dpkg-deb --build --root-owner-group "$work/root" "$2" >/dev/null
}

guest_steps() {
    local deb=$1 up=/tmp/obk-pkg-upgrade.deb src dst job

    # --- install over a unit an older --install left in /etc ---------------
    # As the old --install left it: written into /etc and enabled.
    printf '[Unit]\nDescription=Block-level backup engine for Own Backup: runs scheduled backups and serves the GUI/CLI.\n\n[Service]\nType=simple\nExecStart=/usr/local/bin/own-backup-service --run\n\n[Install]\nWantedBy=multi-user.target\n' >"$LEGACY"
    systemctl daemon-reload && systemctl enable own-backup 2>/dev/null
    run install env SUDO_USER=lab DEBIAN_FRONTEND=noninteractive apt-get install -y "$deb"
    check install "$(verdict $?)" "apt-get install of $(basename "$deb")"
    [[ ! -e $LEGACY && $(readlink /etc/systemd/system/multi-user.target.wants/own-backup.service) == /usr/lib/systemd/system/own-backup.service ]]
    check legacy-unit-removed "$(verdict $?)" "the /etc unit of an older --install is gone; the packaged one is enabled"
    run unit systemctl show own-backup -p Type -p ActiveState -p FragmentPath -p ExecStart
    [[ $(systemctl show -P Type own-backup) == notify && $(systemctl show -P ActiveState own-backup) == active &&
        $(systemctl show -P FragmentPath own-backup) == /usr/lib/systemd/system/own-backup.service ]]
    check unit-active-notify "$(verdict $?)" "packaged unit, Type=notify, active"
    systemctl is-enabled -q own-backup
    check unit-enabled "$(verdict $?)" "enabled at boot"
    getent group own-backup >/dev/null && id -nG lab | tr ' ' '\n' | grep -qx own-backup
    check group-and-sudo-user "$(verdict $?)" "group own-backup exists; SUDO_USER lab is a member"
    run perms stat -c '%n %a %U %G' /var/lib/own-backup /var/log/own-backup /run/own-backup \
        /run/own-backup/service.sock /run/own-backup/service.token
    [[ $(stat -c '%a %U %G' /var/lib/own-backup) == '750 root own-backup' &&
        $(stat -c '%a %U %G' /run/own-backup) == '750 root own-backup' &&
        $(stat -c '%a %U %G' /run/own-backup/service.sock) == '660 root own-backup' &&
        $(stat -c '%a %U %G' /run/own-backup/service.token) == '640 root own-backup' ]]
    check directories-socket-token "$(verdict $?)" "0750/0660/0640, root:own-backup"
    run polkit pkaction --action-id com.ownbackup.delete-backups --verbose
    check polkit-actions "$(verdict $?)" "com.ownbackup.* known to polkit"
    run docs ls -l /usr/share/doc/own-backup
    local f ok=0
    for f in LICENSE NOTICE COMMERCIAL-LICENSE.md THIRD-PARTY-NOTICES.md THIRD-PARTY-LICENSES.txt.gz copyright; do
        [[ -s /usr/share/doc/own-backup/$f ]] || ok=1
    done
    check licences-shipped "$(verdict $ok)" "licence, notices and third-party texts in /usr/share/doc"
    run inhibit-delay systemd-inhibit --list --no-pager --mode=delay
    inhibited_by_us delay
    check shutdown-delay-lock "$(verdict $?)" "the engine holds a delay lock on shutdown"
    run cli-status cli jobs status
    check cli-reaches-service "$(verdict $?)" "own-backup-cli talks to the packaged service"

    # --- grant-access --------------------------------------------------------
    useradd -M -s /usr/sbin/nologin lab2 2>/dev/null
    run grant "$SVC" --grant-access lab2
    id -nG lab2 | tr ' ' '\n' | grep -qx own-backup
    check grant-access "$(verdict $?)" "--grant-access lab2 adds lab2 to own-backup"

    # --- a copy, and an upgrade while it runs ---------------------------------
    parted -s "$D1" mklabel gpt mkpart src ext4 1MiB 100% &&
        parted -s "$D2" mklabel gpt mkpart img ext4 1MiB 100% || return 1
    src=$(part_of "$D1" 1) && dst=$(part_of "$D2" 1) || return 1
    mkfs.ext4 -q -F "$src" && mkfs.ext4 -q -F "$dst" || return 1
    mkdir -p "$SRC_MNT" "$IMG" && mount "$dst" "$IMG" && mount "$src" "$SRC_MNT" || return 1
    # Incompressible data, so the copy lasts long enough to be interrupted.
    head -c 4G /dev/urandom >"$SRC_MNT/random.bin" && sync
    umount "$SRC_MNT"
    run job-new cli jobs new --name pkg-scenario --source "$src" --dest "$IMG" \
        --method full --no-exclusions --schedule none
    job=$(job_id_named pkg-scenario)
    [[ -n $job ]]
    check job-created "$(verdict $?)" "job $job"
    # Held to 100 MB/s by the job's speed limit (carried out on Linux too),
    # the 4 GiB copy lasts about 40 s whatever the disk: long enough for the
    # upgrade below to cut it. Unpaced, a fast disk finishes first.
    cli jobs show "$job" >"$EV/job-paced.json" 2>/dev/null
    python3 - "$EV/job-paced.json" <<'PY'
import json, sys
p = sys.argv[1]
j = json.load(open(p))
j.setdefault("advanced", {})["speed_limit_mbps"] = 100
json.dump(j, open(p, "w"))
PY
    run job-paced cli jobs save --file "$EV/job-paced.json"
    check job-paced "$(verdict $?)" "speed limit 100 MB/s saved"

    make_upgrade "$deb" "$up"
    check upgrade-package-made "$(verdict $?)" "$(dpkg-deb -f "$up" Version)"

    run run-1 cli jobs run "$job"
    wait_for 60 bash -c "ls $IMG/*/* $IMG/* 2>/dev/null | grep -q ."
    sleep 5
    run inhibit-block systemd-inhibit --list --no-pager --mode=block
    inhibited_by_us block
    check sleep-blocked-while-copying "$(verdict $?)" "a block lock on sleep while the copy runs"

    local t0=$SECONDS
    run upgrade env DEBIAN_FRONTEND=noninteractive apt-get install -y "$up"
    local rc=$?
    check upgrade-during-copy "$(verdict $rc)" "apt-get install over a running copy took $((SECONDS - t0)) s"
    [[ $(dpkg-query -W -f '${Version}' own-backup) == $(dpkg-deb -f "$up" Version) &&
        $(systemctl show -P ActiveState own-backup) == active ]]
    check upgraded-and-running "$(verdict $?)" "new version installed, service active again"
    last_run "$job" >"$EV/run-1-record.json"
    cat "$EV/run-1-record.json"
    python3 -c 'import json,sys; r=json.load(open(sys.argv[1])); sys.exit(0 if r.get("status") in ("cancelled","error") else 1)' "$EV/run-1-record.json"
    check interrupted-run-recorded "$(verdict $?)" "the copy cut by the upgrade is recorded as not successful"
    run dest-after-upgrade find "$IMG" -maxdepth 3 -printf '%p %s\n'
    ! inhibited_by_us block
    check no-lock-left "$(verdict $?)" "no sleep lock left by the stopped engine"

    # --- after-backup poweroff -------------------------------------------------
    # A fast copy can be past its last byte when the upgrade stops the engine
    # (then only the check after it is cut, and the run says so): its image is
    # published and leaves no room for a second full copy on the small
    # destination disk. It is not needed any more.
    find "$IMG" -maxdepth 1 -type f -name 'pkg-scenario_*' -delete
    cli jobs show "$job" >"$EV/job.json" 2>/dev/null
    python3 - "$EV/job.json" <<'PY'
import json, sys
p = sys.argv[1]
j = json.load(open(p))
j.setdefault("advanced", {})["after_backup"] = "shutdown"
j["advanced"]["speed_limit_mbps"] = 0
json.dump(j, open(p, "w"))
PY
    run job-shutdown cli jobs save --file "$EV/job.json"
    run run-2 cli jobs run "$job"
    local before
    before=$(last_run "$job" | python3 -c 'import json,sys; print(json.load(sys.stdin).get("id",""))')
    run_finished() {
        last_run "$job" | python3 -c 'import json,sys; r=json.load(sys.stdin); sys.exit(0 if r.get("id") not in ("",sys.argv[1]) else 1)' "$before"
    }
    wait_for 900 run_finished
    last_run "$job" >"$EV/run-2-record.json"
    python3 -c 'import json,sys; sys.exit(0 if json.load(open(sys.argv[1])).get("status")=="success" else 1)' "$EV/run-2-record.json"
    check copy-after-upgrade "$(verdict $?)" "a full copy succeeds with the upgraded engine"
    sleep 5
    run scheduled busctl get-property org.freedesktop.login1 /org/freedesktop/login1 \
        org.freedesktop.login1.Manager ScheduledShutdown
    grep -q '"poweroff"' "$EV/scheduled.out"
    check poweroff-scheduled-after-copy "$(verdict $?)" "logind has a poweroff scheduled after the copy"
    shutdown -c
    run scheduled-cancelled busctl get-property org.freedesktop.login1 /org/freedesktop/login1 \
        org.freedesktop.login1.Manager ScheduledShutdown
    # Nothing scheduled any more: an empty kind (the time is 0 or, in newer
    # systemd, USEC_INFINITY).
    grep -qE '^\(st\) "" ' "$EV/scheduled-cancelled.out"
    check poweroff-cancellable "$(verdict $?)" "shutdown -c cancels it"

    # --- remove, then purge ------------------------------------------------------
    run remove env DEBIAN_FRONTEND=noninteractive apt-get remove -y own-backup
    check remove "$(verdict $?)" "apt-get remove"
    [[ $(systemctl show -P ActiveState own-backup) != active && ! -e /usr/lib/systemd/system/own-backup.service &&
        -d /var/lib/own-backup ]] && getent group own-backup >/dev/null
    check remove-keeps-data "$(verdict $?)" "stopped, unit gone, /var/lib/own-backup and the group kept"
    run purge env DEBIAN_FRONTEND=noninteractive apt-get purge -y own-backup
    check purge "$(verdict $?)" "apt-get purge"
    [[ ! -e /var/lib/own-backup && ! -e /var/log/own-backup ]] && getent group own-backup >/dev/null
    check purge-removes-data "$(verdict $?)" "/var/lib and /var/log gone, group kept"
    run images-after-purge find "$IMG" -maxdepth 3 -printf '%p %s\n'
    find "$IMG" -mindepth 1 -maxdepth 3 -type f | grep -q .
    check backups-survive-purge "$(verdict $?)" "the backups in the destination are untouched"
}

guest_main() {
    local deb=$1
    [[ $EUID -eq 0 ]] || { echo 'guest part needs root'; exit 2; }
    D1=/dev/disk/by-id/virtio-ob-data-1
    D2=/dev/disk/by-id/virtio-ob-data-2
    for d in "$D1" "$D2"; do
        [[ -b $d ]] || { echo "missing $d: start the VM with --disks 2 --size 7"; exit 2; }
    done
    rm -rf "$EV"
    mkdir -p "$EV"
    : >"$EV/summary.txt"
    trap 'cp -a /var/log/own-backup "$EV/service-logs" 2>/dev/null; journalctl -u own-backup --no-pager >"$EV/journal.txt" 2>&1; guest_cleanup; chmod -R a+rX "$EV"' EXIT
    trap 'exit 130' INT TERM
    guest_cleanup
    command -v parted >/dev/null || apt-get install -y -qq parted >/dev/null

    guest_steps "$deb"

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
HOST_EV=$RUN_DIR/evidence/package-$STAMP

die() {
    echo "[scenario] ERROR: $*" >&2
    exit 1
}

DEB=${1:-$(ls -t "${CARGO_TARGET_DIR:-$REPO/target}"/dist/own-backup_*_amd64.deb 2>/dev/null | head -1)}
[[ -f $DEB ]] || die 'no package: run scripts/build-deb.sh, or name the .deb'
"${VM[@]}" status || die 'the VM is not running: python3 -I scripts/lab/linux-vm.py up --disks 2 --size 7'
"${VM[@]}" budget --step package-start || die 'space budget'
mkdir -p "$HOST_EV"
sha256sum "$DEB" | tee "$HOST_EV/deb.sha256"
git -C "$REPO" log -1 --format='%H %s' >"$HOST_EV/commit.txt"

"${VM[@]}" ssh -- 'rm -rf /tmp/obk-scenario && mkdir -p /tmp/obk-scenario' || die 'guest unreachable'
"${VM[@]}" put "$DEB" /tmp/obk-scenario/own-backup.deb || die 'copy of the package failed'
"${VM[@]}" put "${BASH_SOURCE[0]}" /tmp/obk-scenario/scenario.sh || die 'copy of the scenario failed'

echo "[scenario] guest part starts; evidence: $HOST_EV"
"${VM[@]}" ssh -- 'sudo bash /tmp/obk-scenario/scenario.sh guest /tmp/obk-scenario/own-backup.deb' \
    2>&1 | tee "$HOST_EV/guest-console.log"
rc=${PIPESTATUS[0]}

"${VM[@]}" get /var/tmp/obk-evidence "$HOST_EV/guest" || echo '[scenario] WARNING: evidence copy failed'
"${VM[@]}" ssh -- 'sudo rm -rf /var/tmp/obk-evidence /tmp/obk-scenario /tmp/obk-pkg-upgrade /tmp/obk-pkg-upgrade.deb'
"${VM[@]}" budget --step package-done

echo
echo "[scenario] summary ($HOST_EV/guest/summary.txt):"
cat "$HOST_EV/guest/summary.txt" 2>/dev/null || echo '(no summary copied)'
echo "[scenario] guest exit $rc; evidence in $HOST_EV"
exit "$rc"
