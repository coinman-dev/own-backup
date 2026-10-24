# What Own Backup is

Own Backup makes **image backups** of disks and partitions: a backup holds the
partition (or the whole disk) as it was, block by block, so it can be put back
exactly — including the disk the system boots from. Unlike file-by-file backup
programs it does not copy files one at a time; it reads the used blocks of the
file system and writes them into one image file.

It runs on **Windows 10/11 (x64)** and on **Linux (Ubuntu 24.04 or newer,
Debian 13 or newer, amd64)**. Backups made on one system open, validate and
restore on the other.

## What it can do

- **Back up** volumes and whole disks: full, incremental (only what changed
  since the last version) and differential (what changed since the last full)
  backups, on a schedule, on an event or by hand.
- **Read only what is used.** On NTFS, ReFS, FAT32, exFAT, ext2/3/4, XFS and
  btrfs only the clusters that hold data go into the backup; free space is
  skipped.
- **Back up a running system.** On Windows the copy is taken from a VSS
  snapshot; on Linux from an LVM or device-mapper snapshot. Files that are open
  are copied in a consistent state.
- **Exclude files for real.** Excluded files are not read at all, and their
  names are removed from the file system inside the backup — the restored
  volume does not have them, and their space is free.
- **Encrypt.** The OBK format is encrypted with AES-256-GCM and opened with a
  password or a recovery key. BitLocker and LUKS volumes are copied decrypted
  with their password.
- **Restore** a partition, a whole disk (with its partition table and boot
  code), or individual files — by mounting the backup as a disk and copying
  from it.
- **Validate.** Every block carries a checksum; a backup can be checked after
  it is made, on a schedule, or at any time.
- **Boot media.** A bootable USB stick or ISO, built from this computer, with
  the same program on it: it restores the system disk and backs up a computer
  that does not start.
- **Boot menu.** View and edit the computer's UEFI boot entries.

## What it does not do

- It does not back up to the cloud or over the network on its own: a backup
  goes to a folder — a local disk, a USB disk or a mounted network share.
- It does not synchronise files or keep file versions one by one. To get one
  file back, mount the backup and copy the file.
- It does not send anything anywhere: no advertising, no telemetry, no update
  checks.

## How it is built

Own Backup is three programs:

| Part | What it does |
|---|---|
| **Service** | Does all the work that needs administrator rights: reads disks, makes snapshots, writes and restores images, runs the schedule. On Windows it is the `OwnBackup` service (LocalSystem); on Linux the `own-backup` systemd unit. |
| **Window** | What you see: creates jobs, shows progress and history, starts restores. It runs without administrator rights and asks the service for everything. |
| **Command line** | `own-backup-cli` — the same operations from a terminal or a script. |

The window and the service talk over a local channel that only this computer
can reach (a named pipe on Windows, a Unix socket on Linux), with an access
token. No network port is opened.

## A first look

![The Backup page with a job selected](../images/en/backup-dashboard.png)

The left column is the menu: **Backup**, **Restore**, **Boot menu**, **Tools**,
**Activity**, **Settings**, **Help**. The next column on the **Backup** page
lists the backup jobs; the right part shows the selected job: what it backs up,
where to, when it last ran and what the backup holds by file type.

## Prerelease status

0.1.6-beta is a prerelease, and the first with a Linux version. Before relying
on a backup, check it: restore it to a spare disk or a virtual machine, or at
least mount it and open a few files. On Linux especially — that version is new.

## License

Own Backup is distributed under the **PolyForm Noncommercial License 1.0.0**:
free to use for noncommercial purposes — at home, in education, in research.
The source code is available to study and modify. Commercial use needs a
separate license (see `COMMERCIAL-LICENSE.md` next to the program). This is a
source-available license, not an "open source" license in the OSI sense.
Third-party components keep their own licenses, listed in
`THIRD-PARTY-NOTICES.txt`.
