# Installation

Download the files from the
[release page](https://github.com/coinman-dev/own-backup/releases). The file
`SHA256SUMS.txt` there holds the SHA-256 checksum of each file.

| File | For |
|---|---|
| `own-backup-install_<version>.exe` | Windows 10/11, x64 |
| `own-backup_<version>_amd64.deb` | Ubuntu 24.04+, Debian 13+, amd64 |
| `OwnBackup-Linux-<version>_amd64.iso` | Linux boot medium (see [Bootable media](10-bootable-media.md)) |

## Windows

### Requirements

- Windows 10 or 11, 64-bit.
- Administrator rights for the installation (the program itself then runs
  without them).
- The Microsoft Edge **WebView2 Runtime** for the window. Windows 11 has it;
  on Windows 10 the installer says so if it is missing and offers the download
  page.

### Install

1. Run `own-backup-install_0.1.9-beta.exe` and confirm the administrator prompt.
2. Choose the language of the wizard (English or Russian) and follow it.

The installer puts the program into `C:\Program Files\OwnBackup` for all users
of the computer, registers and starts the **Own Backup** service, adds the
program to the Start menu, and starts the tray icon for the current user.

**Updating** is the same: run the new installer over the old version. Settings,
jobs and history stay. An update may be installed while a backup is running —
the installer stops the service first, and the interrupted backup is retried
by its schedule.

### Silent install

```
own-backup-install_0.1.9-beta.exe /S
own-backup-install_0.1.9-beta.exe /S /D=D:\Programs\OwnBackup
```

`/S` installs without questions. `/D=<folder>`, which must be the last
argument, chooses the folder of a first installation.

### Remove

Use **Settings → Apps** (or *Programs and Features*). The uninstaller asks
whether to keep the settings, jobs and history; a silent removal always keeps
them. **Backups are never deleted** by uninstalling.

## Linux

### Requirements

- Ubuntu 24.04 or newer, or Debian 13 or newer; amd64.
- systemd (the service is a systemd unit) and polkit.
- The package brings the libraries the window needs (WebKitGTK and the
  others). Recommended packages — `lvm2`, `cryptsetup-bin`, `ntfs-3g`,
  `debootstrap` and others — are installed by default by `apt` and are needed
  for LVM snapshots, LUKS, removing NTFS names and building the boot medium.

### Install

```bash
sudo apt install ./own-backup_0.1.9.beta_amd64.deb
```

The package installs the window and the command line into `/usr/bin`, the
service into `/usr/libexec/own-backup`, enables and starts the `own-backup`
systemd unit, and adds the window to the applications menu.

**Access.** Only members of the `own-backup` group may use the service, and
only from a local session (not over SSH). A first installation from a terminal
adds the user who ran `sudo apt install` to the group. **Sign out and in again**
for the membership to take effect. To give access to another user:

```bash
sudo own-backup-service --grant-access USER
```

The window offers the same (through a password prompt) when it finds that you
are not in the group.

Some actions also ask polkit for an administrator password: restoring onto a
disk, deleting backups, managing keys, building boot media, changing boot
entries.

### Update

Install the new package the same way. A running backup is stopped cleanly and
retried by its schedule.

### Remove

```bash
sudo apt remove own-backup     # the program; settings, jobs and keys stay
sudo apt purge own-backup      # also /var/lib/own-backup (settings, jobs, keys)
```

A purge does not delete the data while the service is still running. Backups
are never deleted.

## After installation

Open **Own Backup** from the Start menu (Windows) or the applications menu
(Linux). The window opens on the **Backup** page; with no jobs yet it offers
to create the first one — see [Quick start](04-quick-start.md).

The window has a **tray icon** (notification area). It starts with your
session; closing the window leaves the icon, and the service keeps running the
schedule whether the window is open or not.
