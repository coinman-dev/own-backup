# Linux boot medium

A Debian 13 "trixie" live system that boots straight into the Own Backup
window: the engine (`own-backup.service` from our `.deb`) and the window in a
`cage` kiosk on tty1, no desktop. Plan §6, stage 6, decision 11.

- **Build in the window:** Tools → boot medium → Linux (or
  `own-backup-cli media build-linux --out DIR [--stick /dev/sdX]`). The
  service packs the INSTALLED package (`dpkg -L own-backup`, checked against
  dpkg's checksums; a changed program file refuses the build), runs
  `build-media.sh` — installed as `/usr/libexec/own-backup/build-linux-media`,
  with `auto/` and `config/` in `/usr/share/own-backup/media` — as root in the
  transient unit `own-backup-media-build.service`, and saves
  `OwnBackup-Linux-<version>.iso` into the caller's own folder (never over an
  existing file). Needs `debootstrap` and `debian-archive-keyring` (the
  package recommends them), the network, about 10 GiB in
  `/var/cache/own-backup/media`. The builder and the package cache stay there
  between builds (about 2 GiB); the dialog's «Очистить кэш» or
  `own-backup-cli media linux-cache --clear` removes them, and so does a purge
  of the package.
- **Build from the checkout:** `scripts/build-linux-media.sh` (root through
  sudo) runs the same `build-media.sh` with the repository's paths. Result:
  `target/dist/own-backup-media_<version>_amd64.iso`.
- **Boot:** UEFI with Secure Boot (Debian's signed shim, GRUB and kernel),
  UEFI without it, BIOS (ISOLINUX), from a CD or a USB stick (isohybrid).
- **Media mode:** every boot entry puts `own-backup.media` on the kernel
  command line; the engine, the window and the CLI read it there
  (`crates/common/src/media_mode.rs`). State lives in `/run/own-backup-media`.
- **Stick:** the window (Tools → boot medium → Linux) or
  `own-backup-cli media write-stick --iso … --device /dev/sdX` writes the ISO
  byte for byte, reads it back and adds a FAT32 partition `OBK-LOGS` behind
  it (`crates/backup/src/media_stick*.rs`). `dd` works too, without the log
  partition.
- **Logs:** at boot `own-backup-media-log` mounts the FAT partition
  `OBK-LOGS` of the disk the medium booted from (never one of another disk:
  those are the machines being rescued) and points the engine's and the
  window's log copy at `own-backup-logs/<date>_<boot id>/`; the session's
  journal is written there at power-off. Without such a partition (`dd`, a
  CD, `toram`) the logs stay in memory, and the journal says why.
- **Network:** optional; DHCP on wired ports through systemd-networkd, nothing
  waits for it.
- **Stand:** `scripts/lab/linux-media-scenario.sh` (boots under OVMF with
  Secure Boot and SeaBIOS, checks the window, the engine and the logs through
  the medium's self-test and a screenshot).

Layout (live-build): `auto/` (the `lb config` options), `config/package-lists/`,
`config/includes.chroot/` (units, PAM, scripts), `config/hooks/live/`;
`build-media.sh` is the build itself. The build copies `auto/` and `config/`
and adds `config/packages.chroot/own-backup_*.deb`; nothing is generated here.
