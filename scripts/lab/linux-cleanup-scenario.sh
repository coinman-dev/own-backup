#!/usr/bin/env bash
# Linux acceptance of the disk cleanup's removal (CHECKS 61): the real engine, as root, on a
# real system's apt, journal and logs and a person's folders. Run as root inside a stand
# guest (`scripts/lab/linux-vm.py`):
#
#   sudo scripts/lab/linux-cleanup-scenario.sh <cleanup_probe>
#
# The guest is disposable: it is cleaned for real. Each check prints
# `CHECK <id> PASS|FAIL <text>`; the exit code is the number of FAILs.
set -uo pipefail

PROBE=${1:?the cleanup_probe program}
U=lab
H=$(getent passwd "$U" | cut -d: -f6)
FAILS=0
[[ $(id -u) -eq 0 ]] || { echo "run as root" >&2; exit 2; }

check() { # id verdict text...
    local id=$1 verdict=$2
    shift 2
    [[ $verdict == FAIL ]] && FAILS=$((FAILS + 1))
    printf 'CHECK %-30s %-4s %s\n' "$id" "$verdict" "$*"
}
verdict() { if [[ $1 -eq 0 ]]; then echo PASS; else echo FAIL; fi; }
as_lab() { runuser -u "$U" -- sh -c "$1"; }
mib() { echo $(($(du -sxb "$@" 2>/dev/null | awk '{s+=$1} END {print s+0}') / 1048576)); }
clean() { # ids...: analysis, then the removal of what it listed
    "$PROBE" analyze "$@" | grep -E '^[a-z_]+ +[0-9]+ files'
    "$PROBE" clean "$@" | grep -E '^[a-z_]+ +[0-9]+ files'
}

# --- the system's -------------------------------------------------------------------------
export DEBIAN_FRONTEND=noninteractive
apt-get update -qq >/dev/null 2>&1
apt-get install -y --download-only -qq --reinstall nano >/dev/null 2>&1
apt_before=$(ls /var/cache/apt/archives/*.deb 2>/dev/null | wc -l)
clean apt_cache
apt_after=$(ls /var/cache/apt/archives/*.deb 2>/dev/null | wc -l)
[[ $apt_before -gt 0 && $apt_after -eq 0 ]]
check apt-cache "$(verdict $?)" "$apt_before packages, then $apt_after"
apt-get install -y -qq --reinstall nano >/dev/null 2>&1
check apt-still-works "$(verdict $?)" "apt installs after the cleanup"

mkdir -p /etc/systemd/journald.conf.d
printf '[Journal]\nRateLimitBurst=0\nCompress=no\nSystemMaxUse=2G\n' >/etc/systemd/journald.conf.d/ob.conf
systemctl restart systemd-journald
sleep 1
for _ in $(seq 1 3); do
    head -c 150M /dev/urandom | base64 -w 40000 | systemd-cat -t ob-fill
done
sleep 2
j_before=$(mib /var/log/journal /run/log/journal)
clean journal
sleep 1
j_after=$(mib /var/log/journal /run/log/journal)
[[ $j_before -gt 300 && $j_after -lt $((j_before - 100)) && $j_after -le 260 ]]
check journal-shrinks "$(verdict $?)" "$j_before MiB, then $j_after MiB (kept: 200)"
logger -t ob-after "still written"
sleep 1
journalctl -t ob-after -n 1 --no-pager | grep -q "still written"
check journal-still-works "$(verdict $?)" "the journal is written after the cleanup"

# A file's age counts from the later of its change and its creation, and `touch` cannot
# move a creation: the guest's clock moves instead.
echo old >/var/log/ob-test.log.1
echo old >/var/log/ob-test.log.2.gz
echo live >/var/log/ob-test.log
timedatectl set-ntp false 2>/dev/null
systemctl stop chrony systemd-timesyncd 2>/dev/null
date -s '+3 days' >/dev/null
echo fresh >/var/log/ob-fresh.log.1
clean rotated_logs
[[ ! -e /var/log/ob-test.log.1 && ! -e /var/log/ob-test.log.2.gz && -e /var/log/ob-test.log && -e /var/log/ob-fresh.log.1 ]]
check rotated-logs "$(verdict $?)" "old copies go; the live log and a fresh copy stay"
date -s '-3 days' >/dev/null

# --- a person's -------------------------------------------------------------------------
as_lab "mkdir -p $H/documents $H/.cache/pip/http $H/.cache/thumbnails/large $H/.cache/fontconfig \
    $H/.local/share/Trash/files/project/sub $H/.local/share/Trash/info \
    $H/.config/google-chrome/Default/Cache $H/.config/google-chrome/Default/GPUCache \
    $H/.cache/google-chrome/Default/Cache $H/.config/Code/Cache $H/.config/Code/User"
as_lab "echo work >$H/documents/work.odt; echo x >$H/.cache/pip/http/a; echo x >$H/.cache/thumbnails/large/a.png; \
    echo x >$H/.local/share/Trash/files/project/sub/a; echo x >$H/.local/share/Trash/info/project.trashinfo; \
    ln -s $H/documents $H/.local/share/Trash/files/project/link; \
    echo x >$H/.cache/google-chrome/Default/Cache/data_0; echo x >$H/.config/google-chrome/Default/GPUCache/d; \
    echo keep >'$H/.config/google-chrome/Default/Login Data'; echo keep >$H/.config/google-chrome/Default/Bookmarks; \
    echo x >$H/.config/Code/Cache/a; echo keep >$H/.config/Code/User/settings.json"
# A link where a cache should be: what it points to is not the cleaner's.
as_lab "mkdir -p $H/secret; echo s >$H/secret/s; rmdir $H/.cache/fontconfig; ln -s $H/secret $H/.cache/fontconfig"
# Someone else's file in the person's folder: their rights do not reach it.
echo root >"$H/.cache/pip/http/root-owned"
chown root:root "$H/.cache/pip/http/root-owned"
chmod 755 "$H/.cache/pip/http"
chown root:root "$H/.cache/pip/http"

clean trash thumbs font_cache dev_caches_linux browser_cache app_caches_linux

[[ ! -e $H/.local/share/Trash/files/project ]]
check trash-with-a-link "$(verdict $?)" "the folder with a link in it left the Trash"
[[ -f $H/documents/work.odt ]]
check link-target-stays "$(verdict $?)" "what the link pointed to is whole"
[[ ! -e $H/.cache/thumbnails/large/a.png ]]
check thumbnails "$(verdict $?)" "thumbnails go"
[[ -f $H/secret/s ]]
check cache-as-a-link "$(verdict $?)" "a link in a cache's place is not followed"
[[ -f $H/.cache/pip/http/root-owned ]]
check other-owner-stays "$(verdict $?)" "root's file stays: the person's rights do not reach it"
[[ ! -e $H/.cache/google-chrome/Default/Cache/data_0 && ! -e $H/.config/google-chrome/Default/GPUCache/d ]]
check browser-cache "$(verdict $?)" "the browser's caches go"
[[ -f "$H/.config/google-chrome/Default/Login Data" && -f $H/.config/google-chrome/Default/Bookmarks ]]
check browser-data-stays "$(verdict $?)" "passwords and bookmarks stay"
[[ ! -e $H/.config/Code/Cache/a && -f $H/.config/Code/User/settings.json ]]
check program-cache "$(verdict $?)" "a program's cache goes, its settings stay"
owner=$(stat -c %U "$H/.cache/pip")
[[ $owner == "$U" ]]
check still-theirs "$(verdict $?)" "the folders left are still the person's ($owner)"

echo "FAILS=$FAILS"
exit "$FAILS"
