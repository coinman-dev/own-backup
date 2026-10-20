#!/usr/bin/env bash
# Linux acceptance scenario for the boot medium (plan §6, stage 6): the ISO
# boots under UEFI with Secure Boot (Microsoft's keys, Debian's signed shim)
# and under SeaBIOS, as a CD and as a stick; the kiosk window comes up and
# draws, the engine answers in media mode; the stick writer in an installed
# system writes a stick that boots and keeps the session's logs.
#
#   scripts/build-linux-media.sh --deb target/dist/own-backup_*.deb   # the ISO
#   scripts/lab/linux-media-scenario.sh boot  ISO          # 4 boots of the ISO
#   scripts/lab/linux-media-scenario.sh stick ISO DEB      # writer + 2 stick boots
#
# `boot` needs no Ubuntu guest: each boot is a media run of its own
# (`linux-vm.py media`, cleaned afterwards). `stick` starts the stand's Ubuntu
# guest with an empty 4 GiB USB stick (`up --stick 4`), installs the package,
# writes the ISO onto the stick with `own-backup-cli media write-stick`, then
# boots that stick under both firmwares, powers off (the journal goes onto the
# stick then), and reads the log partition from the host with mtools. The run
# is left for inspection; `linux-vm.py clean` archives it.
#
# What a boot must show (the medium's self-test, serial port; see
# packaging/linux/media/.../selftest): own-backup.media on the command line;
# the firmware it was booted with, and Secure Boot on under uefi-sb; the
# engine answering (`media sticks` over its socket) with its state on the RAM
# disk (media mode); the kiosk active and the page drawn; and on the screen
# shot more than a handful of colours, none covering 90 % of it.
#
# Evidence: ~/lab/linux/run/evidence/ (media-<label>.txt and .png per boot)
# and, for `boot`, ~/lab/linux/evidence/<run-id>/ after each clean. Exit 0 only
# when every check passed.
set -uo pipefail

here="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
vm=(python3 -I "$here/linux-vm.py")
RUN="$HOME/lab/linux/run"
FAILS=0
CHECKS=0

check() { # id verdict text...
    local id=$1 verdict=$2
    shift 2
    CHECKS=$((CHECKS + 1))
    [[ $verdict == FAIL ]] && FAILS=$((FAILS + 1))
    printf 'CHECK %-34s %-4s %s\n' "$id" "$verdict" "$*"
}

expect() { # id file regex text
    if grep -Eq "$3" "$2"; then check "$1" PASS "$4"; else check "$1" FAIL "$4 (want /$3/)"; fi
}

# The self-test's verdicts on one boot. $1 label, $2 firmware, $3 log place
# (memory | stick).
judge() {
    local label=$1 fw=$2 logs=$3 txt="$RUN/evidence/media-$1.txt"
    if [[ ! -f $txt ]]; then
        check "$label" FAIL "no self-test report"
        return
    fi
    sed 's/^/    /' "$txt"
    expect "$label:cmdline" "$txt" '^OBK-SELFTEST cmdline=1$' "own-backup.media on the command line"
    case "$fw" in
        uefi-sb) expect "$label:secureboot" "$txt" 'firmware=uefi secureboot=1$' "UEFI, Secure Boot on" ;;
        uefi) expect "$label:firmware" "$txt" 'firmware=uefi ' "UEFI" ;;
        bios) expect "$label:firmware" "$txt" 'firmware=bios ' "BIOS" ;;
    esac
    expect "$label:service" "$txt" '^OBK-SELFTEST service=ok$' "the engine answers on its socket"
    expect "$label:media-mode" "$txt" '^OBK-SELFTEST service_media=1$' "the engine is in media mode"
    expect "$label:window" "$txt" 'kiosk=active window=drawn$' "the kiosk runs and the page drew"
    if [[ $logs == stick ]]; then
        expect "$label:logs" "$txt" '^OBK-SELFTEST logs=/run/own-backup-media-log/own-backup-logs/' \
            "logs go to the stick's OBK-LOGS"
    else
        expect "$label:logs" "$txt" '^OBK-SELFTEST logs=memory$' "no log partition: logs in memory"
    fi
    expect "$label:failed-units" "$txt" '^OBK-SELFTEST failed_units=$' "no failed systemd unit"
    local colours dominant
    colours="$(sed -n 's/^SCREEN .* colours=\([0-9]*\) .*/\1/p' "$txt")"
    dominant="$(sed -n 's/^SCREEN .* dominant=\([0-9]*\)$/\1/p' "$txt")"
    if [[ -n $colours && $colours -ge 16 && ${dominant:-100} -lt 90 ]]; then
        check "$label:screen" PASS "the screen shows a page ($colours colours, $dominant % one)"
    else
        check "$label:screen" FAIL "the screen looks empty (${colours:-?} colours, ${dominant:-?} % one)"
    fi
}

boot_once() { # label firmware media-args...
    local label=$1 fw=$2
    shift 2
    echo "=== boot $label"
    if ! "${vm[@]}" media --firmware "$fw" --label "$label" "$@"; then
        check "$label:boot" FAIL "no self-test (see $RUN/serial.log and evidence/media-$label.png)"
    fi
}

stage_boot() {
    local iso=$1 fw attach label
    for fw in uefi-sb bios; do
        for attach in cdrom usb; do
            label="iso-$fw-$attach"
            "${vm[@]}" status >/dev/null 2>&1 && { echo "a run is up; stopping it"; "${vm[@]}" down; }
            boot_once "$label" "$fw" --iso "$iso" --attach "$attach"
            judge "$label" "$fw" memory
            "${vm[@]}" down
            "${vm[@]}" clean
        done
    done
}

stage_stick() {
    local iso=$1 deb=$2
    "${vm[@]}" up --disks 0 --stick 4 || { check stick:up FAIL "the Ubuntu guest did not come up"; return; }
    "${vm[@]}" put "$deb" /tmp/own-backup.deb
    "${vm[@]}" put "$iso" /tmp/own-backup-media.iso
    if "${vm[@]}" ssh -- 'sudo apt-get install -y /tmp/own-backup.deb >/tmp/install.log 2>&1'; then
        check stick:install PASS "the package installs"
    else
        check stick:install FAIL "the package does not install (guest /tmp/install.log)"
    fi
    # The stick: the USB disk with serial ob-stick.
    local dev
    dev="$("${vm[@]}" ssh -- 'readlink -f /dev/disk/by-id/usb-*ob-stick*-0:0 2>/dev/null | head -1')"
    if [[ $dev != /dev/sd* ]]; then
        check stick:device FAIL "the guest shows no USB stick (got '$dev')"
        "${vm[@]}" down
        return
    fi
    "${vm[@]}" ssh -- sudo own-backup-cli media sticks | tee "$RUN/evidence/sticks.txt"
    expect stick:offered "$RUN/evidence/sticks.txt" "^$dev .* usb " "the stick is offered ($dev, usb)"
    "${vm[@]}" ssh -- sudo own-backup-cli media write-stick --iso /tmp/own-backup-media.iso \
        --device "$dev" | tee "$RUN/evidence/write-stick.txt"
    expect stick:written "$RUN/evidence/write-stick.txt" '^Записано и проверено' \
        "written and read back"
    expect stick:log-partition "$RUN/evidence/write-stick.txt" "^Раздел для журналов: ${dev}3$" \
        "the log partition is ${dev}3"
    # An internal disk is refused even when asked for by name.
    if ! "${vm[@]}" ssh -- sudo own-backup-cli media write-stick --iso /tmp/own-backup-media.iso \
        --device /dev/vda >"$RUN/evidence/write-internal.txt" 2>&1 \
        && grep -q 'не флешка\|not a stick\|нет в списке флешек\|not in the stick list' "$RUN/evidence/write-internal.txt"; then
        check stick:internal-refused PASS "the system disk is refused"
    else
        check stick:internal-refused FAIL "the system disk was not refused"
    fi
    "${vm[@]}" down

    local fw label
    for fw in uefi-sb bios; do
        label="stick-$fw"
        boot_once "$label" "$fw" --stick
        judge "$label" "$fw" stick
        # ACPI power-off: the journal goes onto the stick on the way down.
        "${vm[@]}" down
    done

    # The log partition, read on the host: partition 3 of the stick.
    local start
    start="$(python3 -I - "$RUN/stick.img" <<'EOF'
import struct, sys
with open(sys.argv[1], 'rb') as f:
    lba0 = f.read(512)
first = struct.unpack('<I', lba0[446 + 32 + 8:446 + 32 + 12])[0]
print(first * 512)
EOF
)"
    MTOOLS_SKIP_CHECK=1 mdir -/ -b -i "$RUN/stick.img@@$start" ::/own-backup-logs \
        >"$RUN/evidence/stick-logs.txt" 2>&1
    sed 's/^/    /' "$RUN/evidence/stick-logs.txt"
    local sessions
    sessions="$(grep -c 'own-backup-logs/[^/]*/session.txt$' "$RUN/evidence/stick-logs.txt")"
    if [[ $sessions -ge 2 ]]; then
        check stick:sessions PASS "$sessions session folders on the stick"
    else
        check stick:sessions FAIL "$sessions session folders on the stick (want 2)"
    fi
    expect stick:service-log "$RUN/evidence/stick-logs.txt" 'own-backup-service.log$' \
        "the engine's log is on the stick"
    expect stick:journal "$RUN/evidence/stick-logs.txt" 'journal.txt$' \
        "the session's journal was saved at power-off"
}

case "${1:-}" in
    boot)
        [[ -f ${2:-} ]] || { echo "usage: $0 boot ISO" >&2; exit 2; }
        stage_boot "$(realpath "$2")"
        ;;
    stick)
        [[ -f ${2:-} && -f ${3:-} ]] || { echo "usage: $0 stick ISO DEB" >&2; exit 2; }
        stage_stick "$(realpath "$2")" "$(realpath "$3")"
        ;;
    *)
        sed -n '2,30p' "$0"
        exit 2
        ;;
esac
echo "=== $CHECKS checks, $FAILS failed"
[[ $FAILS -eq 0 ]]
