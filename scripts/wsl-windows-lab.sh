#!/usr/bin/env bash
# Disposable Windows VM for the E2 cold-start probe, driven from WSL.
#
# Why here and not on the developer's Windows: the probe is an unsigned test
# executable, the check needs a real power cycle, and neither belongs on the
# machine doing the work. QEMU/KVM inside WSL gives a throwaway Windows that
# nothing else shares.
#
# It needs, once, from a shell with root:
#     sudo apt-get install -y qemu-system-x86 xorriso
#     sudo usermod -aG kvm "$USER"     # then restart WSL: wsl --terminate <distro>
#
# Phases:
#   prepare   build the answer CD and a blank disk (wipes the VM)
#   media     rebuild only the answer CD, keeping the installed VM
#   install   boot the Windows ISO, install unattended, run stage 1, power off
#   coldstart boot the installed disk with nobody logged on, run stage 2
#   shot      screenshot the running VM (PNG), for when a phase does not finish
#   key/reset press a key in the guest, or reset it
#   wait      block until the guest powers itself off
#   status    what the VM has said so far
#   clean     delete the VM, keeping the logs
#
# Nothing here touches the host, the installed service, or any real backup.
set -euo pipefail

LAB="${LAB_DIR:-$HOME/lab}"
REPO="${REPO_DIR:-$HOME/Development/own-backup}"
WINDOWS_ISO="${WINDOWS_ISO:-$LAB/win10-ltsc.iso}"
PROBE="${PROBE_EXE:-$REPO/target/x86_64-pc-windows-msvc/debug/examples/seal_store_service.exe}"
DISK="$LAB/win-lab.qcow2"
ANSWER="$LAB/answer.iso"
MONITOR="$LAB/monitor.sock"
PIDFILE="$LAB/qemu.pid"
DISK_GB="${DISK_GB:-40}"
MEMORY_MB="${MEMORY_MB:-4096}"
CPUS="${CPUS:-4}"

say() { printf '[lab] %s\n' "$*"; }
die() { printf '[lab] %s\n' "$*" >&2; exit 1; }

require_tools() {
    command -v qemu-system-x86_64 >/dev/null ||
        die "qemu-system-x86_64 is missing: sudo apt-get install -y qemu-system-x86 xorriso"
    command -v xorriso >/dev/null ||
        die "xorriso is missing: sudo apt-get install -y xorriso"
    [ -r /dev/kvm ] && [ -w /dev/kvm ] ||
        die "/dev/kvm is not accessible: sudo usermod -aG kvm $USER, then restart WSL"
}

build_media() {
    require_tools
    [ -f "$WINDOWS_ISO" ] || die "Windows ISO not found: $WINDOWS_ISO"
    [ -f "$PROBE" ] || die "probe executable not found: $PROBE (build it with cargo xwin)"
    mkdir -p "$LAB"

    # The answer CD: what Windows Setup reads, and what the guest runs afterwards.
    local stage="$LAB/answer"
    rm -rf "$stage"
    mkdir -p "$stage/lab"
    cp "$REPO/scripts/lab/autounattend.xml" "$stage/autounattend.xml"
    cp "$REPO/scripts/lab/lab-stage1.ps1" "$REPO/scripts/lab/lab-stage2.ps1" "$stage/lab/"
    cp "$REPO/scripts/test-seal-service.ps1" "$stage/lab/"
    cp "$PROBE" "$stage/lab/seal_store_service.exe"
    echo 'OwnBackup E2 disposable laboratory media' > "$stage/lab/lab-marker.txt"
    xorriso -as mkisofs -quiet -J -R -V OBKLAB -o "$ANSWER" "$stage"
    say "answer CD: $ANSWER ($(du -h "$ANSWER" | cut -f1))"

}

create_disk() {
    rm -f "$DISK"
    qemu-img create -f qcow2 "$DISK" "${DISK_GB}G" >/dev/null
    say "blank disk: $DISK (${DISK_GB} GiB)"
}

# $1 = phase name, $2... = extra qemu arguments
boot() {
    local phase="$1"; shift
    require_tools
    [ -f "$DISK" ] || die "no VM disk — run: $0 prepare"
    if [ -e "$PIDFILE" ] && kill -0 "$(cat "$PIDFILE")" 2>/dev/null; then
        die "a VM is already running (pid $(cat "$PIDFILE")); use: $0 stop"
    fi
    rm -f "$MONITOR" "$PIDFILE"
    local serial="$LAB/serial-$phase.log"
    : > "$serial"
    say "booting phase '$phase'; the guest speaks into $serial"
    setsid qemu-system-x86_64 \
        -machine pc,accel=kvm -cpu host -smp "$CPUS" -m "$MEMORY_MB" \
        -drive "file=$DISK,format=qcow2,if=ide,index=0,media=disk" \
        -netdev user,id=n0 -device e1000,netdev=n0 \
        -serial "file:$serial" \
        -monitor "unix:$MONITOR,server,nowait" \
        -display none -no-user-config -pidfile "$PIDFILE" \
        "$@" </dev/null >"$LAB/qemu-$phase.out" 2>&1 &
    sleep 2
    [ -e "$PIDFILE" ] || die "QEMU did not start; see $LAB/qemu-$phase.out"
    say "pid $(cat "$PIDFILE")"
}

case "${1:-}" in
    prepare)
        build_media
        create_disk
        ;;
    media)
        # Rebuild only the answer CD: a fixed cold-start script reaches the
        # installed VM through it, without reinstalling Windows.
        build_media
        ;;
    install)
        # CD first: the Windows installer boots, and later boots fall through to
        # the disk because nobody presses a key.
        boot install \
            -drive "file=$WINDOWS_ISO,format=raw,if=ide,index=1,media=cdrom" \
            -drive "file=$ANSWER,format=raw,if=ide,index=2,media=cdrom" \
            -boot order=dc
        # "Press any key to boot from CD" wants exactly that, and there is
        # nobody at this console: press it for the first 25 seconds.
        ( for _ in $(seq 50); do
              python3 "$REPO/scripts/lab/qemu-monitor.py" "$MONITOR" "sendkey ret" >/dev/null 2>&1 || true
              sleep 0.5
          done ) &
        say 'pressing a key so the installer boots from CD'
        ;;
    key)
        python3 "$REPO/scripts/lab/qemu-monitor.py" "$MONITOR" "sendkey ${2:-ret}"
        ;;
    reset)
        python3 "$REPO/scripts/lab/qemu-monitor.py" "$MONITOR" "system_reset"
        ;;
    coldstart)
        # No installer media, nobody logged on: this is the cold start itself.
        boot coldstart \
            -drive "file=$ANSWER,format=raw,if=ide,index=2,media=cdrom" \
            -boot order=c
        ;;
    shot)
        [ -S "$MONITOR" ] || die "no running VM to photograph"
        python3 "$REPO/scripts/lab/qemu-screenshot.py" "$MONITOR" "${2:-$LAB/screen.png}"
        ;;
    wait)
        # A phase ends when the guest powers itself off; block until it does.
        deadline=$(( $(date +%s) + ${2:-3600} ))
        while [ -e "$PIDFILE" ] && kill -0 "$(cat "$PIDFILE")" 2>/dev/null; do
            if [ "$(date +%s)" -ge "$deadline" ]; then
                say "the VM is still running after ${2:-3600}s"
                exit 2
            fi
            sleep 10
        done
        say 'the VM powered itself off'
        ;;
    status)
        if [ -e "$PIDFILE" ] && kill -0 "$(cat "$PIDFILE")" 2>/dev/null; then
            say "VM running, pid $(cat "$PIDFILE")"
        else
            say "no VM running"
        fi
        for log in "$LAB"/serial-*.log; do
            [ -e "$log" ] || continue
            say "--- $(basename "$log") ---"
            tr -d '\r' < "$log" | tail -n "${2:-40}"
        done
        ;;
    stop)
        if [ -e "$PIDFILE" ]; then
            kill "$(cat "$PIDFILE")" 2>/dev/null || true
            rm -f "$PIDFILE"
            say 'VM stopped'
        fi
        ;;
    clean)
        rm -f "$DISK" "$ANSWER" "$MONITOR" "$PIDFILE"
        rm -rf "$LAB/answer"
        say 'VM removed; serial logs kept'
        ;;
    *)
        sed -n '2,30p' "$0"
        exit 1
        ;;
esac
