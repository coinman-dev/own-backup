[English](README.md) | [Русский](README.ru_RU.md)

<p align="center">
  <img src="crates/gui/frontend/src/assets/logo.png" alt="Own Backup" width="160">
</p>

<h1 align="center">Own Backup</h1>
<p align="center">Block-level backup of Windows disks and volumes, with a boot medium to restore from.</p>

[![Release](https://img.shields.io/badge/release-0.1.3--beta-orange)](https://github.com/coinman-dev/own-backup/releases/tag/v0.1.3-beta)
[![Build](https://github.com/coinman-dev/own-backup/actions/workflows/release.yml/badge.svg)](https://github.com/coinman-dev/own-backup/actions/workflows/release.yml)
[![Windows](https://img.shields.io/badge/Windows-10%20%2F%2011%20x64-blue)](#download)
[![Rust](https://img.shields.io/badge/Rust-1.98-orange)](#building)
[![License](https://img.shields.io/badge/license-PolyForm%20Noncommercial-blue)](LICENSE)

**Own Backup** copies whole disks and volumes block by block: only used
clusters go into a backup, and excluded files do not go in at all. A backup can
be encrypted with a password. From a backup you can restore a partition, a disk
or individual files — and, from the boot medium, the disk Windows itself boots
from.

> **0.1.3-beta is a Windows prerelease.** Check an important backup by
> restoring it to a spare disk or a virtual machine before you rely on it.
> Linux code exists for development and tests; there is no Linux version yet.

## Download

| Edition | Download | Usage |
|---|---|---|
| Installer | [own-backup-install.exe](https://github.com/coinman-dev/own-backup/releases/download/v0.1.3-beta/own-backup-install.exe) | Installs the program for all users of the computer and starts its backup service. Needs administrator rights. |

Windows 10/11, x64. The window needs the Microsoft Edge WebView2 Runtime — part
of Windows 11, and the installer says so if a Windows 10 does not have it. No
advertising, no telemetry, no update checks.
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
- Only used clusters are read (NTFS, ReFS, FAT32, exFAT), from a VSS snapshot
  when the system is running.
- Three formats: **VHDX** (Windows mounts it by itself), **qcow2** (zstd
  compression) and **OBK** (zstd and AES-256-GCM, opened with a password or a
  recovery key).
- Exclusions that really exclude: matched files are not read and their entries
  are removed from the filesystem inside the image, not left as empty ghosts.
- BitLocker volumes, locked ones included, are copied decrypted — with the
  password or the 48-digit recovery key.
- Per-block xxHash64 checksums and verification after a backup or on demand.
- Restore of a partition or a whole bootable disk, UEFI boot entries included,
  with a guard against overwriting the disk the system runs from.
- A boot medium built from the computer's own Windows recovery environment,
  with the same interface on it.
- A backup mounted as a disk through Windows' own iSCSI initiator: no
  third-party driver is installed.
- Schedules and events (logon, startup, a destination disk plugged in),
  retries, wake from sleep, an optional shutdown afterwards.
- A window, a command line and a service: the service runs as LocalSystem and
  answers over a named pipe with restricted access.

## Current boundaries

- NTFS/ReFS and FAT32/exFAT exclusions have live acceptance runs. ext2/3/4
  and btrfs metadata removal have filesystem checks and mount/write tests.
- XFS removal is incomplete for some directory shapes; some excluded names can
  remain as zero-filled entries.
- Exclusions inside an encrypted ReFS source may be skipped, with an explicit
  warning in the result.
- New `.obk` images use two header generations. A boot medium built by an
  older version cannot open them: rebuild the medium with this version.
- TPM-only BitLocker volumes are not opened by the program itself.
- The [Windows acceptance checklist](docs/WINDOWS-TESTS-AND-FIXES-2026-09-26.md)
  records what was checked live and what was not.

## Settings and data

| What | Where |
|---|---|
| The program | `C:\Program Files\OwnBackup` |
| Settings, jobs, run history, remembered volume keys | `C:\ProgramData\OwnBackup` |
| Service log, one file a day, thirty days kept | `Logs\own-backup-service-<date>.log` in the program folder |
| What the window remembers: theme, language | `%LOCALAPPDATA%\com.ownbackup.gui` |
| Backups | wherever each job says; the program keeps none of its own |

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

`own-backup-install.exe /S` installs silently; `/D=<folder>`, as the last
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
```

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

Pushing a version tag such as `v0.1.3-beta` starts the
[release workflow](.github/workflows/release.yml). The tag must match the
version in `Cargo.toml`. GitHub builds the Windows x64 executables, packs the
installer, installs and removes it on the runner, computes SHA-256 checksums
and publishes the release only after the files are ready. A version with a
suffix is marked as a prerelease. The workflow can also be run by hand for an
existing tag, or as a dry run that publishes nothing.

## License

Original project code is licensed under the
**[PolyForm Noncommercial License 1.0.0](LICENSE)**: free for noncommercial
use, forks included. It is a source-available license, not an OSI "open source"
one. Commercial use needs a separate license — see
[COMMERCIAL-LICENSE.md](COMMERCIAL-LICENSE.md). See [NOTICE](NOTICE) for the
required notice.

Third-party libraries retain their own licenses; see
[THIRD-PARTY-NOTICES.md](THIRD-PARTY-NOTICES.md).
