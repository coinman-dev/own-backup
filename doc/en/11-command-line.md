# Command line

`own-backup-cli` does from a terminal what the window does. On Windows it is in
`C:\Program Files\OwnBackup` (commands that touch disks ask for administrator
rights themselves); on Linux it is `/usr/bin/own-backup-cli`.

```
own-backup-cli --help
own-backup-cli <command> --help
```

## Jobs (through the running service)

| Command | What it does |
|---|---|
| `jobs list` | The configured jobs, with their ids. |
| `jobs run <id>` | Queues a job, like **Back up now**. |
| `jobs cancel` | Cancels the running backup. |
| `jobs queue` | The running job and the queue. |
| `jobs activity [--limit N]` | Recent run history (10 entries by default). |
| `jobs status` | Service version and uptime. |
| `jobs watch` | Follows the live events (Ctrl+C stops). |
| `jobs versions <id>` | The job's backups on disk, by source and chain, with sizes and what each depends on. |
| `jobs show <id>` | Prints a job as JSON. |
| `jobs template` | A new job's JSON with the defaults the window uses. |
| `jobs save <file\|->` | Creates or replaces a job from JSON. |
| `jobs new …` / `jobs set <id> …` | Creates a job, or changes its policy, with flags instead of JSON. |
| `jobs delete <id> [--delete-files]` | Deletes a job, and with `--delete-files` its backups. |

## Disks and images

| Command | What it does |
|---|---|
| `list-disks` | Disks and partitions as the program sees them. |
| `list-backups` | Backup jobs and sets. |
| `verify <image>` | Validates an image (with its chain) against its checksums. |
| `mount <image>` / `unmount <image>` | Attaches an image as a disk / detaches it. |
| `backup …` | Backs up a volume. |
| `backup-disk …` | Backs up a whole disk into one bootable image, as a job does. |
| `restore …` | Restores a volume. |
| `restore-disk …` | Restores a whole disk — destructive. |
| `boot` | The UEFI boot entries: what the firmware offers and where it leads. |
| `driver` | Whether mounting works: the iSCSI initiator (Windows), NBD and NTFS (Linux). |

## Boot media

| Command | What it does |
|---|---|
| `media build …` | Builds the WinPE medium (Windows), as the window does. |
| `media build-linux …` | Builds the Linux medium from the installed program into a folder (Linux). |
| `media sticks` | USB sticks the medium can be written to (Linux). |
| `media write-stick …` | Erases a stick and writes the medium onto it (Linux). |
| `media cancel` | Stops a medium build or stick write in progress. |

## The service

On Windows `own-backup-service.exe` itself takes `/install`, `/uninstall`,
`/start`, `/stop` and `/status` (the installer uses these); `own-backup-cli
service status` reports the same. On Linux the service is a systemd unit:
`systemctl status own-backup`, `journalctl -u own-backup`.

## Examples

```powershell
own-backup-cli jobs list
own-backup-cli jobs run 3f2a…
own-backup-cli verify "F:\Backup\Disk_C_Win11\Disk_C_Win11_disk4_inc_20261024-185102.obk"
own-backup-service.exe /status
```

The command line is in Russian and English only in parts: its messages are
mostly in English, some in Russian.
