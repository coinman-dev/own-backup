#!/usr/bin/env bash
# Окно Own Backup в Linux-госте стенда (scripts/lab/linux-vm.py): сборка,
# безголовый X-дисплей, запуск окна, снимки экрана и клики.
#
#   scripts/lab/linux-gui-shot.sh deps               # пакеты сборки и дисплея
#   scripts/lab/linux-gui-shot.sh build <archive>    # исходники (git archive) → сборка
#   scripts/lab/linux-gui-shot.sh install            # служба как из пакета (scripts/install-linux-dev.sh)
#   scripts/lab/linux-gui-shot.sh seat               # автологин на tty1: сеанс seat0
#   scripts/lab/linux-gui-shot.sh relogin            # новый вход lab: группа own-backup
#   scripts/lab/linux-gui-shot.sh display [WxH]      # Xvfb :99 (user-unit ob-xvfb)
#   scripts/lab/linux-gui-shot.sh gui [lab|root] [auto] # окно (unit ob-gui), DISPLAY=:99;
#                                                    #   auto — способ рисования решает окно
#   scripts/lab/linux-gui-shot.sh again              # второй запуск: окно одно, второй выходит
#   scripts/lab/linux-gui-shot.sh byid               # by-id дисков: udev и инвентарь службы
#   scripts/lab/linux-gui-shot.sh hotplug            # loop-диск появился/ушёл → окно перечитало
#   scripts/lab/linux-gui-shot.sh tray [label]       # значок: SNI-наблюдатель, меню, щелчок
#   scripts/lab/linux-gui-shot.sh lang <locale>      # язык при первом запуске (LANG), без выбора
#   scripts/lab/linux-gui-shot.sh stop               # остановить окно
#   scripts/lab/linux-gui-shot.sh shot <name>        # снимок → $OUT/<name>.png
#   scripts/lab/linux-gui-shot.sh click <x> <y>      # левый клик в точке экрана
#   scripts/lab/linux-gui-shot.sh move <x> <y>       # указатель (клавиатура идёт окну под ним)
#   scripts/lab/linux-gui-shot.sh scroll <x> <y> <n> # n щелчков колеса вниз в точке
#   scripts/lab/linux-gui-shot.sh key <keysym...>    # xdotool key
#   scripts/lab/linux-gui-shot.sh type <text>        # xdotool type
#   scripts/lab/linux-gui-shot.sh logs               # журналы окна и службы → $OUT/
#
# OUT — папка улик на хосте (по умолчанию ~/lab/linux/evidence/gui-<UTC>,
# имя запоминается в ~/lab/linux/run/gui-out). Окно пользователя `lab`
# запускается через `systemd-run --user`, т. е. вне SSH-сеанса — как
# приложения рабочего стола KDE/GNOME; служба судит такого клиента по сеансам
# пользователя (peer_access.rs), и проходит он, только пока у `lab` есть
# активный локальный сеанс на seat0 (команда `seat`). Окно, запущенное прямо из
# SSH, служба отвергает. Оконного менеджера нет: клавиатура достаётся окну под
# указателем, поэтому перед `key`/`type` в диалоге нужен `move` в него.
# Порядок: deps → build → install → seat → relogin → display → gui → shot/click…
set -euo pipefail

HERE=$(cd "$(dirname "$0")" && pwd)
VM=(python3 -I "$HERE/linux-vm.py")
RUN=$HOME/lab/linux/run
SRC=/home/lab/src

# Папка улик: одна на прогон стенда. Имя хранится в папке прогона, только
# пока она есть, — без config.json рядом `linux-vm.py up` её не примет.
out_dir() {
    if [[ -z "${OUT:-}" ]]; then
        if [[ -s $RUN/gui-out ]]; then
            OUT=$(cat "$RUN/gui-out")
        else
            OUT=$HOME/lab/linux/evidence/gui-$(date -u +%Y%m%dT%H%M%SZ)
            [[ -f $RUN/config.json ]] && echo "$OUT" > "$RUN/gui-out"
        fi
    fi
    mkdir -p "$OUT"
}

g() { "${VM[@]}" ssh -- "$1"; }

# В X-дисплей ходит сам lab; XAUTHORITY не нужен (Xvfb -ac).
X="export DISPLAY=:99;"

case "${1:-}" in
deps)
    # Ровно то, с чем окно собралось и запустилось в Ubuntu 26.04
    # (--no-install-recommends; npm из архива тянет свои node-* пакеты).
    g 'sudo DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
        build-essential pkg-config libwebkit2gtk-4.1-dev libayatana-appindicator3-dev \
        librsvg2-dev libssl-dev protobuf-compiler curl ca-certificates git nodejs npm \
        xvfb xauth xdotool imagemagick x11-utils dbus-x11 fonts-dejavu-core \
        python3-dbus python3-gi libayatana-appindicator3-1 >/tmp/apt.log 2>&1 \
        || { tail -20 /tmp/apt.log; exit 1; }
       command -v rustup >/dev/null || [ -x ~/.cargo/bin/rustup ] || {
         curl -sSf https://sh.rustup.rs -o /tmp/rustup.sh &&
         sh /tmp/rustup.sh -y --profile minimal --default-toolchain none >/tmp/rustup.log 2>&1; }'
    ;;
build)
    archive=${2:?archive from git archive}
    "${VM[@]}" put "$archive" /home/lab/src.tar.gz
    # Отладочная сборка без отладочной информации: в госте 16 ГиБ корня.
    g "rm -rf $SRC/crates $SRC/vendor; mkdir -p $SRC && tar -xzf ~/src.tar.gz -C $SRC &&
       . ~/.cargo/env && cd $SRC/crates/gui/frontend &&
       $SRC/scripts/npm-ci.sh --no-audit --no-fund >/tmp/npm-ci.log 2>&1 && npm run build >/tmp/npm-build.log 2>&1 &&
       cd $SRC && CARGO_PROFILE_DEV_DEBUG=0 CARGO_INCREMENTAL=0 cargo build --locked \
         -p own-backup-service -p own-backup-cli -p own-backup-gui >/tmp/cargo-build.log 2>&1;
       rc=\$?; tail -5 /tmp/cargo-build.log; exit \$rc"
    ;;
install)
    # Laid out as the .deb does it (unit, sysusers, tmpfiles, polkit); lab joins
    # the access group.
    g "sudo $SRC/scripts/install-linux-dev.sh $SRC/target/debug lab"
    ;;
seat)
    # Автологин на tty1 даёт сеанс SEAT=seat0 TTY=tty1 REMOTE=0 ACTIVE=1 —
    # единственный в госте без экрана, который служба признаёт локальным.
    g 'sudo mkdir -p /etc/systemd/system/getty@tty1.service.d &&
       printf "[Service]\nExecStart=\nExecStart=-/sbin/agetty --autologin lab --noclear %%I \$TERM\n" |
         sudo tee /etc/systemd/system/getty@tty1.service.d/autologin.conf >/dev/null &&
       sudo systemctl daemon-reload && sudo systemctl restart getty@tty1 && sleep 2 &&
       loginctl list-sessions --no-legend'
    ;;
relogin)
    # `--grant-access` добавляет lab в own-backup, но user@1000 (откуда запускается
    # окно) и сеанс tty1 получают группу только при новом входе.
    g 'sudo systemd-run --quiet --no-block sh -c "sleep 1; loginctl terminate-user lab"' || true
    sleep 6
    for _ in $(seq 60); do
        g 'loginctl list-sessions --no-legend | grep -q tty1' 2>/dev/null && break
        sleep 2
    done
    g 'id; loginctl list-sessions --no-legend'
    ;;
display)
    size=${2:-1280x900}
    g "systemctl --user stop ob-xvfb 2>/dev/null; systemctl --user reset-failed ob-xvfb 2>/dev/null;
       systemd-run --user --unit=ob-xvfb --quiet Xvfb :99 -screen 0 ${size}x24 -ac -nolisten tcp &&
       sleep 1 && systemctl --user is-active ob-xvfb"
    ;;
gui)
    who=${2:-lab}
    env="DISPLAY=:99 GDK_BACKEND=x11 WEBKIT_DISABLE_DMABUF_RENDERER=1 LIBGL_ALWAYS_SOFTWARE=1 NO_AT_BRIDGE=1"
    # Without the switch the window decides itself (linux_display.rs): the
    # guest has no render node, so its log must say render=NoRenderNode.
    [[ ${3:-} == auto ]] && env="DISPLAY=:99 GDK_BACKEND=x11 LIBGL_ALWAYS_SOFTWARE=1 NO_AT_BRIDGE=1"
    exe=$SRC/target/debug/own-backup-gui
    if [[ $who == root ]]; then
        g "sudo systemctl stop ob-gui 2>/dev/null; sudo systemctl reset-failed ob-gui 2>/dev/null;
           sudo systemd-run --unit=ob-gui --quiet $(printf -- '--setenv=%s ' $env) $exe &&
           sleep 3 && systemctl is-active ob-gui"
    else
        g "systemctl --user stop ob-gui 2>/dev/null; systemctl --user reset-failed ob-gui 2>/dev/null;
           systemd-run --user --unit=ob-gui --quiet $(printf -- '--setenv=%s ' $env) $exe &&
           sleep 3 && systemctl --user is-active ob-gui"
    fi
    ;;
stop)
    g 'systemctl --user stop ob-gui ob-tray ob-sni 2>/dev/null; sudo systemctl stop ob-gui 2>/dev/null; true'
    ;;
again)
    # A second start hands «show» to the first over $XDG_RUNTIME_DIR/own-backup
    # and leaves at once (instance_linux.rs).
    g "$X export LIBGL_ALWAYS_SOFTWARE=1 NO_AT_BRIDGE=1;
       before=\$(pgrep -cxf '$SRC/target/debug/own-backup-gui');
       systemd-run --user --wait --quiet --collect --setenv=DISPLAY=:99 $SRC/target/debug/own-backup-gui; rc=\$?;
       after=\$(pgrep -cxf '$SRC/target/debug/own-backup-gui');
       ls -la \$XDG_RUNTIME_DIR/own-backup;
       echo \"windows before=\$before after=\$after second exit=\$rc\";
       [ \"\$before\" = 1 ] && [ \"\$after\" = 1 ] && [ \$rc = 0 ] && echo PASS again || { echo FAIL again; exit 1; }"
    ;;
byid)
    # The by-id name each disk gets (DiskInfo.stable_device) against udev's
    # own links: a whole-disk job keeps it in disk_source.device.
    g "ls -l /dev/disk/by-id/ | grep -v -- '-part[0-9]*\$';
       sudo $SRC/target/debug/own-backup-cli list-disks --json |
         python3 -c 'import json,sys
for d in json.load(sys.stdin): print(d[\"device_id\"], \"->\", d.get(\"stable_device\") or \"(none)\")'"
    ;;
hotplug)
    # Every refresh of the window's disk list logs «list_disks ok». A loop
    # disk coming and going must cause one each (device_watch.rs); before
    # this the signature was always 0 and nothing happened.
    g 'log() { find ~/.local/state/own-backup -name "own-backup-gui-*.log" 2>/dev/null | xargs cat 2>/dev/null | grep -c "list_disks ok"; }
       truncate -s 64M /tmp/hotplug.img && printf "label: gpt
,,L
" | sfdisk -q /tmp/hotplug.img;
       n0=$(log); dev=$(sudo losetup -fP --show /tmp/hotplug.img); sleep 8; n1=$(log);
       sudo losetup -d "$dev"; sleep 8; n2=$(log); rm -f /tmp/hotplug.img;
       echo "refreshes: before=$n0 after-attach=$n1 after-detach=$n2 ($dev)";
       [ "$n1" -gt "$n0" ] && [ "$n2" -gt "$n1" ] && echo PASS hotplug || { echo FAIL hotplug; exit 1; }'
    ;;
tray)
    # The guest has no desktop: sni-watcher.py takes the watcher's bus name,
    # prints the item that registers and its menu, and clicks «label» (by
    # default the Russian «Открыть Own Backup», so the session must speak
    # Russian or pass the English label). The click must start the window.
    label=${2:-Открыть Own Backup}
    out_dir
    "${VM[@]}" put "$HERE/sni-watcher.py" /home/lab/sni-watcher.py
    g "systemctl --user stop ob-gui ob-tray ob-sni 2>/dev/null; systemctl --user reset-failed ob-gui ob-tray ob-sni 2>/dev/null;
       systemd-run --user --unit=ob-sni --quiet python3 /home/lab/sni-watcher.py --click $(printf '%q' "$label") && sleep 1 &&
       systemd-run --user --unit=ob-tray --quiet --setenv=DISPLAY=:99 $SRC/target/debug/own-backup-gui --tray && sleep 8;
       journalctl --user -u ob-sni --no-pager -o cat | grep -v '^Started';
       find ~/.local/state/own-backup -name 'own-backup-tray-*.log' | xargs tail -n 5;
       pgrep -xf '$SRC/target/debug/own-backup-gui' >/dev/null && echo 'window started from the menu';
       journalctl --user -u ob-sni --no-pager -o cat | grep -q '^registered' &&
       journalctl --user -u ob-sni --no-pager -o cat | grep -q '^clicked' &&
       pgrep -xf '$SRC/target/debug/own-backup-gui' >/dev/null && echo PASS tray || { echo FAIL tray; exit 1; }"
    ;;
lang)
    # «Как в системе» on a first start: no choice saved in the WebView's
    # storage, the session's locale decides — the window's page and the
    # process's own messages alike.
    loc=${2:?locale, e.g. en_US.UTF-8}
    g "systemctl --user stop ob-gui 2>/dev/null; systemctl --user reset-failed ob-gui 2>/dev/null;
       rm -rf ~/.local/share/own-backup-gui ~/.local/share/com.ownbackup.* 2>/dev/null;
       systemd-run --user --unit=ob-gui --quiet --setenv=DISPLAY=:99 --setenv=LANG=$loc \
         --setenv=LIBGL_ALWAYS_SOFTWARE=1 --setenv=NO_AT_BRIDGE=1 $SRC/target/debug/own-backup-gui &&
       sleep 5 && systemctl --user is-active ob-gui"
    ;;
shot)
    name=${2:?name}
    out_dir
    g "$X mkdir -p /tmp/shots && import -window root /tmp/shots/$name.png"
    "${VM[@]}" get "/tmp/shots/$name.png" "$OUT/$name.png"
    echo "$OUT/$name.png"
    ;;
click)
    g "$X xdotool mousemove ${2:?x} ${3:?y} click 1"
    ;;
move)
    g "$X xdotool mousemove ${2:?x} ${3:?y}"
    ;;
scroll)
    g "$X xdotool mousemove ${2:?x} ${3:?y} click --repeat ${4:-5} 5"
    ;;
key)
    shift
    g "$X xdotool key --delay 80 $*"
    ;;
type)
    shift
    g "$X xdotool type --delay 40 $(printf '%q' "$*")"
    ;;
logs)
    out_dir
    g 'rm -rf /tmp/oblogs && mkdir -p /tmp/oblogs &&
       journalctl --user -u ob-gui --no-pager >/tmp/oblogs/ob-gui.user-journal.txt 2>&1;
       sudo journalctl -u ob-gui --no-pager >/tmp/oblogs/ob-gui.root-journal.txt 2>&1;
       sudo journalctl -u own-backup --no-pager >/tmp/oblogs/own-backup.journal.txt 2>&1;
       for d in ~/.local/state/own-backup /var/log/own-backup /var/lib/own-backup/Logs /root/.local/state/own-backup; do
         sudo test -d "$d" && sudo cp -r "$d" /tmp/oblogs/"$(echo "$d" | tr / _)"; done;
       sudo cp /etc/systemd/system/own-backup.service /tmp/oblogs/ 2>/dev/null;
       sudo ls -la /run/own-backup /var/lib/own-backup >/tmp/oblogs/runtime-perms.txt 2>&1;
       sudo chown -R lab /tmp/oblogs; ls -R /tmp/oblogs'
    "${VM[@]}" get /tmp/oblogs "$OUT/logs"
    echo "$OUT/logs"
    ;;
*)
    sed -n '2,28p' "$0"
    exit 2
    ;;
esac
