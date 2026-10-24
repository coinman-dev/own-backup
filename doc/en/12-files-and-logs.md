# Files, folders and logs

## Windows

| What | Where |
|---|---|
| The program | `C:\Program Files\OwnBackup` |
| Settings, jobs, run history, remembered volume passwords | `C:\ProgramData\OwnBackup` (`config.toml`, `jobs.json`, `activity.json`, `metadata.db`) — readable by SYSTEM and administrators only |
| Service log, one file a day, thirty days kept | `C:\Program Files\OwnBackup\Logs\own-backup-service-<date>.log` |
| Window and tray logs | `C:\ProgramData\OwnBackup\Logs\` |
| Copies of boot entries made before every change | `C:\ProgramData\OwnBackup\boot` |
| What the window remembers (theme, language choice) | `%LOCALAPPDATA%\com.ownbackup.gui` |

## Linux

| What | Where |
|---|---|
| The window and the command line | `/usr/bin/own-backup-gui`, `/usr/bin/own-backup-cli` |
| The service | `/usr/libexec/own-backup/own-backup-service`, unit `own-backup.service` |
| Settings, jobs, history, keys | `/var/lib/own-backup` |
| Service log | journald (`journalctl -u own-backup`) and `/var/log/own-backup` |
| Copies of boot entries | `/var/lib/own-backup/boot` |

## Backups

Backups are written only where a job says; the program keeps none of its own.
A version is one file named

```
<job>_<source>_<full|inc|diff>_<YYYYMMDD-HHMMSS>.<extension>
```

where `<source>` is `disk<N>` for a whole disk or the volume's name (its
letter, for one), and the extension is `.vhdx` for a full VHDX version and
`.avhdx` for its incremental and differential versions (the Windows
convention), `.qcow2` for qcow2, and `.obk` for every version of an encrypted
job. For example `Disk_C_Win11_disk4_inc_20261024-185102.obk`. The time is UTC.
Each image carries its own metadata (what was copied, the checksums of every
block, the partition layout), so a backup folder can be moved or copied
elsewhere and still be opened, validated and restored.

While a backup is being written, the file has a temporary hidden name
(`.own-backup-obk-….tmp.obk` and the like) and becomes visible under its real
name only when it is complete. A file left behind by an interrupted run is
removed the next time the service starts or the job runs.

## Settings that live in the service

**Settings → General** holds the log level, the default compression level and
a folder to **mirror the log** to (a USB stick, another disk) — useful when
the system disk itself is the problem. The service writes the settings file,
so the window does not need administrator rights to change them.
