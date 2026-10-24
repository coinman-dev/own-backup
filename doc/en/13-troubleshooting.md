# Troubleshooting and FAQ

## Where to look first

- **Activity** lists every run with its result, errors and notes.
- The **service log** says in detail what happened — see
  [Files, folders and logs](12-files-and-logs.md). With **Settings → Log
  level → Verbose** it records every step.
- **Settings → Service status** shows whether the service runs and which
  version it is.

## The window says the service is not running

- Windows: open *Services* and start **Own Backup**, or reinstall. The service
  restarts by itself after a crash (in 10 s, then 1 and 5 minutes).
- Linux: `systemctl status own-backup`. If the window says **you are not in the
  `own-backup` group**, add yourself (`sudo own-backup-service --grant-access
  $USER`) and sign in again. The window does not work over SSH: access is given
  to a local session only.

## A scheduled backup did not run

- The computer was off or asleep: with **Run missed operations at system
  startup** the backup runs after the next start (after the chosen delay). With
  **Wake the computer from sleep** the computer wakes up for it.
- The job copies an encrypted volume (BitLocker, LUKS) and its password is not
  saved: the run is skipped and **Activity** says so. Save the password in the
  job's **Source** step.
- The destination disk was not connected: the run fails and is retried
  (**Advanced → Retry a failed backup**). An event job **When the destination
  disk is plugged in** runs as soon as it appears.
- The job's key can no longer be opened by the service (the system was
  reinstalled, the backups were moved to another computer): choose **Authorize
  automatic backups again…** in the job's menu and enter the password or the
  recovery key.

## The backup does not move

The job page says when nothing has moved for several minutes.

- **While copying:** usually the backup disk — failing, disconnected, or an
  SMR hard disk that has slowed down after a long write. Cancel the run if the
  disk is gone.
- **At the end ("releasing the volume snapshot", "opening the finished
  backup"):** these steps wait for Windows' device and snapshot services. A
  hung device — a failing USB stick is the usual one — can hold them; the
  Device Manager then also hangs on "Searching for Plug and Play devices".
  Unplug the device; the snapshot step gives up by itself after 10 minutes.
  If the computer stays stuck, restart it: the backup being written is not
  published, earlier versions are never touched.

## Excluded files remain in the backup's catalog

Some excluded files could not be removed from the file system inside the image
(files protected by antivirus software, NTFS entries the Windows driver refuses
to delete, btrfs files whose removal would break a tree leaf). Their contents
are still left out — the entry stays with zeroed data. The result names how
many; the service log names which.

## Notes in an old language

Notes in the run history (Activity, job page) are stored in the language the
program used at the time of the run. After switching the language, older runs
keep their old wording; new runs use the new language.

## Restoring the system disk

The running system cannot overwrite itself: restore protection refuses the
disk Windows (or Linux) runs from and its EFI partition. Boot the computer
from the [bootable media](10-bootable-media.md) and restore from there.

## The clock is three hours off after switching between Windows and Linux

Not a backup problem, but it confuses logs: Linux keeps the hardware clock in
UTC, Windows in local time. Either run `timedatectl set-local-rtc 1
--adjust-system-clock` on Linux, or set
`HKLM\SYSTEM\CurrentControlSet\Control\TimeZoneInformation\RealTimeIsUniversal`
(DWORD 1) on Windows.

## Questions

**Can I restore a backup made on Windows on Linux, and the other way round?**
Yes — the formats are the same, VHDX chains included.

**Can I open a backup without Own Backup?**
VHDX attaches in Windows Disk Management; qcow2 opens with QEMU tools (an
incremental needs its whole chain). OBK needs Own Backup — or the boot medium.

**Is a whole-disk backup bootable after restore?**
Yes, when the backup was made as a **Bootable disk backup** (all partitions
the system needs, with the partition table and boot code). After restoring on
another machine, check its boot entries on the **Boot menu** page.

**What does "Recoverable data" mean?**
The size of the data a restore of the latest version brings back — the used
blocks of the copied partitions, without the excluded files.
