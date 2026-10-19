#!/usr/bin/env bash
# Installs freshly built Linux binaries the way the .deb does, without
# building a package: for the Linux stand and for development machines.
#
#   sudo scripts/install-linux-dev.sh [BIN_DIR] [USER]
#
# BIN_DIR holds own-backup-service and, if built, own-backup-cli and
# own-backup-gui (default target/debug). The service goes to
# /usr/libexec/own-backup, the others to /usr/bin; the unit, sysusers.d,
# tmpfiles.d and polkit files from packaging/linux go where the package puts
# them, then the service enables itself (`--install`). With USER, that user is
# added to the access group (`--grant-access`). A real installation is
# `scripts/build-deb.sh` and `apt install ./own-backup_*.deb`; dpkg does not
# know about files put here, so remove them by installing and purging the
# package or by hand.
set -euo pipefail

root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
bin="${1:-$root/target/debug}"
user="${2:-}"
pkg="$root/packaging/linux"

if [[ $EUID -ne 0 ]]; then
    echo 'run as root (sudo)' >&2
    exit 1
fi
[[ -x "$bin/own-backup-service" ]] || { echo "no $bin/own-backup-service" >&2; exit 1; }

install -d -m755 /usr/libexec/own-backup
install -m755 "$bin/own-backup-service" /usr/libexec/own-backup/own-backup-service
for name in own-backup-cli own-backup-gui; do
    if [[ -x "$bin/$name" ]]; then
        install -m755 "$bin/$name" "/usr/bin/$name"
    fi
done
install -D -m644 "$pkg/own-backup.service" /usr/lib/systemd/system/own-backup.service
install -D -m644 "$pkg/own-backup.sysusers" /usr/lib/sysusers.d/own-backup.conf
install -D -m644 "$pkg/own-backup.tmpfiles" /usr/lib/tmpfiles.d/own-backup.conf
install -D -m644 "$pkg/com.ownbackup.policy" /usr/share/polkit-1/actions/com.ownbackup.policy

systemd-sysusers /usr/lib/sysusers.d/own-backup.conf
systemd-tmpfiles --create /usr/lib/tmpfiles.d/own-backup.conf
/usr/libexec/own-backup/own-backup-service --install
if [[ -n "$user" ]]; then
    /usr/libexec/own-backup/own-backup-service --grant-access "$user"
fi
systemctl is-active own-backup
