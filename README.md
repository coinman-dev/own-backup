[English](README.md) | [Русский](README.ru_RU.md)

<p align="center">
  <img src="crates/gui/frontend/src/assets/logo.png" alt="Own Backup" width="160">
</p>

<h1 align="center">Own Backup</h1>
<p align="center">Block-level backup of Windows and Linux disks and volumes, with boot media to restore from.</p>

[![Release](https://img.shields.io/badge/release-0.1.8--beta-orange)](https://github.com/coinman-dev/own-backup/releases/tag/v0.1.8-beta)
[![Build](https://github.com/coinman-dev/own-backup/actions/workflows/release.yml/badge.svg)](https://github.com/coinman-dev/own-backup/actions/workflows/release.yml)
[![Windows](https://img.shields.io/badge/Windows-10%20%2F%2011%20x64-blue)](#download)
[![Linux](https://img.shields.io/badge/Linux-Ubuntu%2024.04%2B%20%2F%20Debian%2013%2B-blue)](#download)
[![Rust](https://img.shields.io/badge/Rust-1.98-orange)](#building)
[![License](https://img.shields.io/badge/license-PolyForm%20Noncommercial-blue)](LICENSE)

**Own Backup** copies whole disks and volumes block by block: only used
clusters go into a backup, and excluded files do not go in at all. A backup can
be encrypted with a password. From a backup you can restore a partition, a disk
or individual files — and, from the boot medium, the disk Windows itself boots
from.

<p align="center">
  <img src="doc/images/en/backup-dashboard.png" alt="Own Backup: the Backup page with jobs, the last backup of each and its versions">
</p>

> **0.1.8-beta is a prerelease; the Linux version is new since 0.1.6-beta.**
> Check an important backup by restoring it to a spare disk or a virtual
> machine before you rely on it — on Linux especially.

## Download

| Edition | Download | Usage |
|---|---|---|
| Windows installer | [own-backup-install_0.1.8-beta.exe](https://github.com/coinman-dev/own-backup/releases/download/v0.1.8-beta/own-backup-install_0.1.8-beta.exe) | Installs the program for all users of the computer and starts its backup service. Needs administrator rights. |
| Linux package | [own-backup_0.1.8.beta_amd64.deb](https://github.com/coinman-dev/own-backup/releases/download/v0.1.8-beta/own-backup_0.1.8.beta_amd64.deb) | `sudo apt install ./own-backup_0.1.8.beta_amd64.deb` installs the program, starts its service and adds you to the `own-backup` group (sign in again to open the window). |
| Linux boot medium | [OwnBackup-Linux-0.1.8.beta_amd64.iso](https://github.com/coinman-dev/own-backup/releases/download/v0.1.8-beta/OwnBackup-Linux-0.1.8.beta_amd64.iso) | Boots a computer from a CD or a USB stick (UEFI with Secure Boot, or BIOS) to back up or restore its disks, Windows ones included. The window writes it onto a stick. |

Windows 10/11, x64. The window needs the Microsoft Edge WebView2 Runtime — part
of Windows 11, and the installer says so if a Windows 10 does not have it.
Linux: Ubuntu 24.04 or newer, Debian 13 or newer, amd64; the package brings
WebKitGTK and the rest of what the window needs. No advertising, no
telemetry, no update checks.
[All releases](https://github.com/coinman-dev/own-backup/releases) ·
[Changelog](CHANGELOG.md).

## Quick start

1. Run the installer. It registers the **Own Backup** service, which does the
   work that needs administrator rights; the window itself runs without them.
2. Open **Own Backup** from the Start menu and create a job under **Backup**:
   what to copy, where to, in which format and on what schedule.
3. Run the job, then check the result under **Tools → Validate image** or by
   mounting the backup as a disk.
4. Build the boot medium under **Tools → Bootable media** while the computer
   still works — it is what restores a system disk.

The interface is in English and Russian, with light, dark and system themes.

## Features

- Full, incremental and differential backups of volumes and whole disks; a job
  can keep a sliding window of increments, the oldest folded into the base.
- Only used clusters are read (NTFS, ReFS, FAT32, exFAT, ext2/3/4, XFS,
  btrfs). A running system is copied from a VSS snapshot on Windows and from an
  LVM or device-mapper snapshot on Linux.
- Windows and Linux: a backup made on one opens, validates and restores on the
  other.
- Three formats: **VHDX** (Windows mounts it by itself), **qcow2** (zstd
  compression) and **OBK** (zstd and AES-256-GCM, opened with a password or a
  recovery key).
- Exclusions that really exclude: matched files are not read and their entries
  are removed from the filesystem inside the image, not left as empty ghosts.
- BitLocker volumes, locked ones included, are copied decrypted — with the
  password or the 48-digit recovery key; LUKS volumes with their passphrase.
- Per-block xxHash64 checksums and verification after a backup or on demand.
- Restore of a partition or a whole bootable disk, UEFI boot entries included,
  with a guard against overwriting the disk the system runs from.
- Two boot media with the same interface on them: one built from the
  computer's own Windows recovery environment, and a Linux one (Debian 13
  live, UEFI with Secure Boot or BIOS) — built by the Linux window, or
  downloaded ready by the Windows one and checked against the release's
  checksums.
- A backup mounted as a disk through Windows' own iSCSI initiator: no
  third-party driver is installed.
- **Linux partitions in Total Commander** (Windows): a plugin shows the files
  of this computer's ext2/3/4 and btrfs partitions, also inside LVM and LUKS,
  read-only and in place — the other partitions of that disk stay in use.
- Schedules and events (logon, startup, a destination disk plugged in),
  retries, wake from sleep, an optional shutdown afterwards. An action whose
  disks or folder are busy waits in a queue instead of being refused.
- A window, a command line and a service: on Windows the service runs as
  LocalSystem and answers over a named pipe with restricted access; on Linux
  it is a systemd unit behind a Unix socket open to the `own-backup` group.

## Current boundaries

- NTFS/ReFS and FAT32/exFAT exclusions have live acceptance runs. ext2/3/4
  and btrfs metadata removal have filesystem checks and mount/write tests.
- On ext4, XFS and btrfs a file that cannot be removed safely stays as a
  zero-filled entry; the result of the run says how many.
- Linux: a running volume is copied from an LVM or device-mapper snapshot. A
  busy plain partition (an ext4 root without LVM or LUKS) cannot be held
  still while it runs — copy it from the boot medium. A volume group spread
  over several disks is copied only with a warning.
- Exclusions inside an encrypted ReFS source may be skipped, with an explicit
  warning in the result.
- New `.obk` images use two header generations. A boot medium built by an
  older version cannot open them: rebuild the medium with this version.
- TPM-only BitLocker volumes are not opened by the program itself.
- On the developer's machine, current plans and historical evidence are kept
  locally in `.ai/`; this directory is excluded from the repository.

## Settings and data

| What | Where |
|---|---|
| The program | `C:\Program Files\OwnBackup` |
| Settings, jobs, run history, remembered volume keys | `C:\ProgramData\OwnBackup` |
| Service log, one file a day, thirty days kept | `Logs\own-backup-service-<date>.log` in the program folder |
| What the window remembers: theme, language | `%LOCALAPPDATA%\com.ownbackup.gui` |
| Backups | wherever each job says; the program keeps none of its own |

On Linux the program is in `/usr/bin` and `/usr/libexec/own-backup`, settings,
jobs and keys in `/var/lib/own-backup`, and the service log in journald
(`journalctl -u own-backup`) and `/var/log/own-backup`.

Running the installer again updates the program where it is. The uninstaller
keeps the settings unless you tell it otherwise, a silent removal always keeps
them, and backups are never touched.

## Command line

```powershell
own-backup-cli.exe --help
own-backup-cli.exe list-disks
own-backup-cli.exe jobs status
own-backup-cli.exe verify D:\Backups\disk.vhdx
own-backup-service.exe /status
```

`own-backup-install_<version>.exe /S` installs silently; `/D=<folder>`, as the last
argument, chooses the folder of a first installation.

## Building

Rust 1.98.1 is pinned in `rust-toolchain.toml`; the window's pages need Node 24
and the build scripts need `protoc`. On Linux, Tauri needs the packages listed
in its [prerequisites](https://v2.tauri.app/start/prerequisites/).

```bash
cd crates/gui/frontend && npm ci && npm run build && cd ../../..
cargo test --workspace --locked
cargo xwin check --workspace --locked --target x86_64-pc-windows-msvc

sudo apt install nsis           # once, for makensis
scripts/build-installer.sh      # target/dist/own-backup-install.exe
scripts/build-deb.sh            # target/dist/own-backup_<version>_amd64.deb
scripts/build-linux-media.sh    # target/dist/own-backup-media_<version>_amd64.iso
```

Build the `.deb` on Ubuntu 24.04, the oldest supported system: the package
requires the glibc and WebKitGTK it was linked against. The boot medium needs
root, `debootstrap` and the Debian mirror.

The last script builds the frontend and the four release executables with
`cargo xwin` and packs them with NSIS
([packaging/windows/own-backup.nsi](packaging/windows/own-backup.nsi));
`--skip-build` packs what is already built. On Windows, build with
`cargo build --release --target x86_64-pc-windows-msvc` and run the same script
from Git Bash with `--skip-build`. VSS, DPAPI, iSCSI and raw disk I/O cannot be
exercised by a cross-compile: `scripts/lab/run-installer.py` runs the installer
in a throwaway Windows VM, and disk restore has to be checked on a Windows or
WinPE machine.

## Automated releases

Pushing a version tag such as `v0.1.7-beta` starts the
[release workflow](.github/workflows/release.yml). The tag must match the
version in `Cargo.toml`. GitHub builds the Windows x64 executables, packs the
installer, installs and removes it on the runner, computes SHA-256 checksums
and publishes the release only after the files are ready. A version with a
suffix is marked as a prerelease. The workflow can also be run by hand for an
existing tag, or as a dry run that publishes nothing. The Linux package and
boot medium are built locally and added to the release.

## License

Original project code is licensed under the
**[PolyForm Noncommercial License 1.0.0](LICENSE)**: free for noncommercial
use, forks included. It is a source-available license, not an OSI "open source"
one. Commercial use needs a separate license — see
[COMMERCIAL-LICENSE.md](COMMERCIAL-LICENSE.md). See [NOTICE](NOTICE) for the
required notice.

Third-party libraries retain their own licenses; see
[THIRD-PARTY-NOTICES.md](THIRD-PARTY-NOTICES.md).
