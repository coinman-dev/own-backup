# Concepts

## Job

A **job** (shown as a *backup* in the list on the **Backup** page) is a saved
description of a backup: what to copy, where to, in which format, on what
schedule, what to leave out and what to do afterwards. Every run of a job adds
a **version** to the job's folder.

A job copies either **partitions (volumes)** or a **whole disk**. A whole-disk
job keeps the partition table, the boot code and every selected partition, so
the disk can be restored in one go and boot again.

## Versions and chains

| Kind | What it holds | File name part |
|---|---|---|
| **Full** | every used block | `_1-full_` |
| **Incremental** | blocks changed since the previous version (full or incremental) | `_1-inc_` |
| **Differential** | blocks changed since the last full version | `_1-diff_` |

A full version and the incremental versions after it form a **chain**. The
number before the kind is the chain's: each new full starts the next number,
and its versions carry the same one, so `2-inc` belongs to `2-full`. Sorted by
name, a folder lists every chain together — its full, then its versions by
time. Backups made before 0.1.8-beta have no number (`_full_`, `_inc_`), and
the versions added to such a chain stay without one. To
restore an incremental version the program reads the whole chain under it, so
a chain is only as good as its oldest file. The job's **retention** settings
decide how long chains grow and which versions are removed:

- **Standard — chains roll over:** after a set number of versions a new full
  version starts a new chain; old chains are deleted as a whole when they are
  no longer needed.
- **Consolidation — a single chain:** the oldest increment is folded into the
  full version, so there is always one full version and a sliding window of
  increments.

A chain never grows past 60 images: the next run then starts a new full
version.

## Image formats

| Format | Compression | Encryption | Notes |
|---|---|---|---|
| **VHDX** | no | no | Windows' own virtual disk format: Windows can attach it by itself. |
| **qcow2** | zstd | no | Smaller files; the virtual disk format of QEMU/KVM. |
| **OBK** | zstd | AES-256-GCM | Own Backup's encrypted format, opened with the password or the recovery key. |

The compression level (1–19, default 5) trades size for speed; above 9 a backup
gets noticeably slower for little gain.

Every block of an image carries an **xxHash64 checksum** in the image's own
metadata. Validation reads the blocks and compares them; nothing outside the
image is needed.

## Encryption and keys

An OBK job is encrypted with a **password** you choose when you create it, and
the program shows a **recovery key** once — write it down and keep it apart
from the backups. Either opens the backups.

To run scheduled backups without asking you, the service keeps the job's key
**protected by the computer** (DPAPI on Windows, systemd-creds and the TPM on
Linux). If that protection is lost — a reinstalled system, another computer —
the backups still open with the password or the recovery key, and the job can
be **authorized again** from its menu.

This is separate from **encrypted sources**: a BitLocker or LUKS volume is
copied *decrypted*, with its own password or recovery key. Without OBK
encryption the backup then holds that volume's data in the clear — the job
page says so.

## Snapshots

A running system changes while it is copied. The program first takes a
**snapshot**, a frozen view of the volume, and copies from it:

- **Windows:** a VSS shadow copy. It is released when the backup is finished.
- **Linux:** an LVM snapshot for logical volumes, a device-mapper snapshot for
  open LUKS volumes; a short partition (ESP, `/boot`) is held still for the
  time of the copy. A busy plain partition (for example an ext4 root without
  LVM or LUKS) cannot be held still while it runs — back it up from the boot
  medium.

## Exclusions

Exclusion rules are masks: `pagefile.sys`, `*.tmp`, `Temp\*.log`, `**\Temp\**`.
A mask without a path applies anywhere on the volume; a mask with a path
applies from the volume's root. Windows and Linux file systems have separate
lists (Windows masks for NTFS, ReFS and FAT; Linux masks for ext, XFS and
btrfs).

Excluded files are **not read**, and after the backup their **names are
removed** from the file system inside the image, so the restored volume does
not have them and their space is free. When a name cannot be removed safely,
the file stays in the image as an empty (zeroed) entry, and the result of the
run says how many such files there are. Exclusions do not apply in
sector-by-sector mode.

## Used sectors and sector-by-sector

Normally only the blocks the file system uses are read. **Sector-by-sector**
mode (job options → *Advanced*) copies every sector, used or not — slower and
bigger, for file systems the program cannot read or for forensic copies.
Exclusions do not apply then.

## The service

The service does the work. It keeps running when the window is closed and runs
scheduled backups, retries, scheduled validations and events (logon, startup,
a destination disk being plugged in). It can wake the computer for a
scheduled backup and put it to sleep or shut it down afterwards.

Only one operation that writes a disk runs at a time per disk or backup
folder: a restore is exclusive; copies, checks and mounts of different backup
folders run side by side; a scheduled check gives way to an action you start
on the same folder.
