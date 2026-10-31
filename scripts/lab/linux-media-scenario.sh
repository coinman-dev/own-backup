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
#   scripts/lab/linux-media-scenario.sh build DEB [MEDIA_DEB]  # the service builds it
#
# `boot` needs no Ubuntu guest: each boot is a media run of its own
# (`linux-vm.py media`, cleaned afterwards). `stick` starts the stand's Ubuntu
# guest with an empty 4 GiB USB stick (`up --stick 4`), installs the package,
# writes the ISO onto the stick with `own-backup-cli media write-stick`, then
# boots that stick under both firmwares, powers off (the journal goes onto the
# stick then), and reads the log partition from the host with mtools. The run
# is left for inspection; `linux-vm.py clean` archives it.
#
# `build` is the window's way: the Ubuntu guest (16 GiB root for the build's
# cache, a 2 GiB stick, network through QEMU's user mode) installs DEB and has
# the SERVICE build the medium with `own-backup-cli media build-linux` —
# first refused without debootstrap, then into ~lab/Downloads (the ISO must be
# lab's, as the folder is), then again with `--stick` (a new name beside the
# first, the stick written and verified), then a third build cancelled
# halfway (nothing mounted or half-built left, no partial ISO, the builder
# kept). The ISO and the stick then boot (UEFI+SB from the ISO, BIOS from the
# stick). The medium carries the INSTALLED package, which must install on
# Debian 13: DEB built on the Ubuntu 24.04 baseline. With a DEB built on
# 26.04 that apt refuses there, give a baseline MEDIA_DEB: the refusal must
# carry apt's reason, and the builds go on with `--deb MEDIA_DEB` (22.10: the
# 26.04 package did install on trixie). Needs
# the internet on the host (deb.debian.org); about 15 minutes for the first
# build, a few for the next ones.
#
# What a boot must show (the medium's self-test, serial port; see
# packaging/linux/media/.../selftest): own-backup.media on the command line;
# the firmware it was booted with, and Secure Boot on under uefi-sb; the
# engine answering (`media sticks` over its socket) with its state on the RAM
# disk (media mode); the kiosk active and the page drawn; labwc with our strip
# (its window list names the window and a terminal opened and closed beside
# it, a killed strip comes back); and on the screen shot more than a handful
# of colours, none covering 90 % of it.
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
    expect "$label:desktop" "$txt" '^OBK-SELFTEST desktop=labwc shell=running$' \
        "labwc with our strip (not the cage fallback)"
    expect "$label:ntfs3" "$txt" '^OBK-SELFTEST ntfs3_mount=yes$' \
        "the kernel's NTFS driver mounts an NTFS volume read-only"
    expect "$label:vm-screen" "$txt" '^OBK-SELFTEST vm_screen=1440x900$' \
        "a virtual machine gets 1440x900"
    expect "$label:strip-lists" "$txt" '^OBK-SELFTEST shell_windows=[1-9][0-9]* front=.' \
        "the strip has a button for the window, one of them in front"
    expect "$label:strip-terminal" "$txt" '^OBK-SELFTEST terminal_listed=yes terminal_gone=yes$' \
        "a terminal gets a button and loses it when closed"
    expect "$label:strip-restart" "$txt" '^OBK-SELFTEST shell_restart=yes$' "a killed strip comes back"
    expect "$label:layout" "$txt" '^OBK-SELFTEST layout=EN layout_at=[0-9]+,[0-9]+$' \
        "the strip shows the layout (EN at boot) and where its indicator is"
    expect "$label:layout-keys" "$txt" '^OBK-SELFTEST typed_after_keys=d0b0 layout=RU$' \
        "Alt+Shift on the keyboard: labwc types Cyrillic and the indicator says RU"
    expect "$label:layout-click" "$txt" '^OBK-SELFTEST typed_after_click=66 layout=EN$' \
        "a click on the indicator: labwc types Latin again and the indicator says EN"
    expect "$label:layout-restart" "$txt" '^OBK-SELFTEST layout_after_restart=RU$' \
        "a restarted strip keeps the layout labwc has"
    expect "$label:layout-restart-typed" "$txt" '^OBK-SELFTEST typed_after_restart=d0b0 layout=RU$' \
        "and labwc still types Cyrillic: the strip's new keyboard reset nothing"
    if grep -q '^OBK-SELFTEST tool_' "$txt"; then
        expect "$label:tool-krusader" "$txt" '^OBK-SELFTEST tool_krusader=window caption=Krusader' \
            "Krusader starts as the Start menu starts it, straight to its main window (no first-run dialogs)"
        expect "$label:tool-featherpad" "$txt" '^OBK-SELFTEST tool_featherpad=window ' \
            "FeatherPad starts as the Start menu starts it and shows a window"
    fi
    expect "$label:fallback" "$txt" '^OBK-SELFTEST fallback=cage,active,cage=1,window=1$' \
        "a labwc that cannot start: the window comes up in cage"
    expect "$label:fallback-back" "$txt" '^OBK-SELFTEST fallback_back=labwc,active,cage=0,window=1 shell=running$' \
        "labwc back: the desktop and the strip return"
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

stage_build() {
    local deb=$1 media_deb=${2:-}
    local out=/home/lab/Downloads
    "${vm[@]}" up --disks 0 --root-size 16 --stick 2 --mem 4096 --cpus 4 \
        || { check build:up FAIL "the Ubuntu guest did not come up"; return; }
    "${vm[@]}" put "$deb" /tmp/own-backup.deb
    # Without the recommended debootstrap first: the build must say what to install.
    if "${vm[@]}" ssh -- 'sudo apt-get update -qq >/tmp/install.log 2>&1;
            sudo apt-get install -y --no-install-recommends /tmp/own-backup.deb >>/tmp/install.log 2>&1'; then
        check build:install PASS "the package installs"
    else
        check build:install FAIL "the package does not install (guest /tmp/install.log)"
        "${vm[@]}" down
        return
    fi
    "${vm[@]}" ssh -- "mkdir -p $out"
    "${vm[@]}" ssh -- "sudo own-backup-cli media build-linux --out $out" \
        >"$RUN/evidence/build-no-tools.txt" 2>&1
    expect build:refused-tools "$RUN/evidence/build-no-tools.txt" \
        'sudo apt install debootstrap debian-archive-keyring' "without debootstrap: refused, naming the apt command"
    "${vm[@]}" ssh -- "sudo own-backup-cli media build-linux --out /nonexistent/dir" \
        >"$RUN/evidence/build-no-folder.txt" 2>&1
    expect build:refused-folder "$RUN/evidence/build-no-folder.txt" '/nonexistent/dir' \
        "a folder that does not exist is refused"
    "${vm[@]}" ssh -- 'sudo apt-get install -y debootstrap debian-archive-keyring >>/tmp/install.log 2>&1' \
        || check build:tools FAIL "debootstrap does not install (guest /tmp/install.log)"

    # Build 1 from the installed package. A package built on a system newer
    # than Debian 13 may not install there: then apt's reason must be in the
    # refusal, and the builds go on with MEDIA_DEB (`--deb`).
    local given=()
    echo "=== build 1: into $out"
    "${vm[@]}" ssh -- "sudo own-backup-cli media build-linux --out $out" 2>&1 \
        | tee "$RUN/evidence/build-1.txt"
    if grep -q '^Готово: ' "$RUN/evidence/build-1.txt"; then
        expect build:repacked "$RUN/evidence/build-1.txt" '\(установленный пакет\)$' \
            "the medium carries the installed package"
    elif [[ -n $media_deb ]] && grep -Eq 'Debian 13.*(Depends|E: )' "$RUN/evidence/build-1.txt"; then
        check build:installed-refused PASS "the installed package does not install on trixie; apt's reason is shown"
        "${vm[@]}" put "$media_deb" /home/lab/media.deb
        given=(--deb /home/lab/media.deb)
        echo "=== build 1 again, with the given baseline package"
        "${vm[@]}" ssh -- "sudo own-backup-cli media build-linux --out $out ${given[*]}" 2>&1 \
            | tee "$RUN/evidence/build-1.txt"
    else
        check build:repacked FAIL "the build from the installed package failed (evidence/build-1.txt)"
    fi
    expect build:done "$RUN/evidence/build-1.txt" "^Готово: $out/OwnBackup-Linux-[^/]*\\.iso\$" "the ISO is saved in the folder"
    local stage
    for stage in 'Подготовка сборщика' 'Загрузка пакетов Debian' 'Сборка системы' 'Создание ISO'; do
        expect "build:stage:$stage" "$RUN/evidence/build-1.txt" "^  $stage\$" "stage shown: $stage"
    done
    local iso
    iso="$(sed -n 's/^Готово: //p' "$RUN/evidence/build-1.txt" | tail -1)"
    "${vm[@]}" ssh -- "stat -c '%U %G %s' '$iso'" >"$RUN/evidence/build-owner.txt" 2>&1
    expect build:owner "$RUN/evidence/build-owner.txt" '^lab lab [0-9]{8,}$' "the ISO is lab's, as the folder is"

    local dev
    dev="$("${vm[@]}" ssh -- 'readlink -f /dev/disk/by-id/usb-*ob-stick*-0:0 2>/dev/null | head -1')"
    echo "=== build 2: into $out again, and onto the stick $dev"
    "${vm[@]}" ssh -- "sudo own-backup-cli media build-linux --out $out --stick $dev ${given[*]}" \
        | tee "$RUN/evidence/build-2.txt"
    expect build:second-name "$RUN/evidence/build-2.txt" "^Готово: $out/OwnBackup-Linux-.* \(2\)\.iso\$" \
        "the second ISO gets a name of its own; the first is not replaced"
    expect build:stick-written "$RUN/evidence/build-2.txt" '^Записано и проверено' "the stick is written and read back"
    expect build:stick-logs "$RUN/evidence/build-2.txt" "^Раздел для журналов: ${dev}3\$" "the log partition is ${dev}3"
    "${vm[@]}" ssh -- "ls -l $out; sudo own-backup-cli media linux-cache;
        echo mounts=\$(grep -c -e ' /var/tmp/own-backup-media' -e ' /var/cache/own-backup' /proc/self/mountinfo);
        echo work=\$(sudo find /var/tmp /var/lib/own-backup -maxdepth 1 -name 'own-backup-media.*' | wc -l);
        echo unit=\$(systemctl is-active own-backup-media-build.service)" \
        | tee "$RUN/evidence/build-after.txt"
    expect build:no-mounts "$RUN/evidence/build-after.txt" '^mounts=0$' "nothing is left mounted"
    expect build:work-removed "$RUN/evidence/build-after.txt" '^work=0$' "the temporary build folder is removed after a build"
    expect build:unit-gone "$RUN/evidence/build-after.txt" '^unit=(inactive|unknown)$' "the transient unit is gone"
    expect build:nothing-left "$RUN/evidence/build-after.txt" '^Остатки прошлых сборок: 0 МиБ$' "nothing of the build stays"

    echo "=== build 3: cancelled halfway"
    # The client's pid, not pgrep: this very shell's command line names the build too.
    "${vm[@]}" ssh -- "nohup sudo own-backup-cli media build-linux --out $out ${given[*]} >/tmp/build-3.txt 2>&1 &
        pid=\$!
        for i in \$(seq 1 300); do grep -q '^  Сборка системы' /tmp/build-3.txt && break; sleep 2; done
        sleep 20; sudo own-backup-cli media cancel
        for i in \$(seq 1 150); do [ -d /proc/\$pid ] || break; sleep 2; done
        cat /tmp/build-3.txt; ls -la $out;
        echo mounts=\$(grep -c -e ' /var/tmp/own-backup-media' -e ' /var/cache/own-backup' /proc/self/mountinfo);
        echo work=\$(sudo find /var/tmp /var/lib/own-backup -maxdepth 1 -name 'own-backup-media.*' | wc -l);
        echo unit=\$(systemctl is-active own-backup-media-build.service)" \
        | tee "$RUN/evidence/build-3.txt"
    expect build:cancelled "$RUN/evidence/build-3.txt" 'сборка носителя отменена' "the build is cancelled"
    if grep -q '\.part' "$RUN/evidence/build-3.txt" || grep -q '(3)\.iso' "$RUN/evidence/build-3.txt"; then
        check build:cancel-no-iso FAIL "a partial or third ISO is in the folder"
    else
        check build:cancel-no-iso PASS "no partial ISO in the folder"
    fi
    expect build:cancel-mounts "$RUN/evidence/build-3.txt" '^mounts=0$' "nothing left mounted after the cancel"
    expect build:cancel-work "$RUN/evidence/build-3.txt" '^work=0$' "the temporary build folder is removed after the cancel"
    expect build:cancel-unit "$RUN/evidence/build-3.txt" '^unit=(inactive|unknown)$' "the transient unit is gone"

    "${vm[@]}" get "$iso" "$RUN/built.iso"
    "${vm[@]}" down
    boot_once built-iso-uefi-sb uefi-sb --iso "$RUN/built.iso" --attach cdrom
    judge built-iso-uefi-sb uefi-sb memory
    "${vm[@]}" down
    boot_once built-stick-bios bios --stick
    judge built-stick-bios bios stick
    "${vm[@]}" down
    rm -f "$RUN/built.iso"
}

case "${1:-}" in
    build)
        [[ -f ${2:-} ]] || { echo "usage: $0 build DEB [MEDIA_DEB]" >&2; exit 2; }
        [[ -z ${3:-} || -f ${3:-} ]] || { echo "no such file: $3" >&2; exit 2; }
        stage_build "$(realpath "$2")" "${3:+$(realpath "$3")}"
        ;;
    boot)
        [[ -f ${2:-} ]] || { echo "usage: $0 boot ISO" >&2; exit 2; }
        stage_boot "$(realpath "$2")"
        ;;
    stick)
        [[ -f ${2:-} && -f ${3:-} ]] || { echo "usage: $0 stick ISO DEB" >&2; exit 2; }
        stage_stick "$(realpath "$2")" "$(realpath "$3")"
        ;;
    *)
        sed -n '2,45p' "$0"
        exit 2
        ;;
esac
echo "=== $CHECKS checks, $FAILS failed"
[[ $FAILS -eq 0 ]]
