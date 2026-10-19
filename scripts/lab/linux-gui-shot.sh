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
#   scripts/lab/linux-gui-shot.sh gui [lab|root]     # окно (unit ob-gui), DISPLAY=:99
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
        xvfb xauth xdotool imagemagick x11-utils dbus-x11 fonts-dejavu-core >/tmp/apt.log 2>&1 \
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
       npm ci --no-audit --no-fund >/tmp/npm-ci.log 2>&1 && npm run build >/tmp/npm-build.log 2>&1 &&
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
    g 'systemctl --user stop ob-gui 2>/dev/null; sudo systemctl stop ob-gui 2>/dev/null; true'
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
