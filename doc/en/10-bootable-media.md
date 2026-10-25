# Bootable media

The boot medium is a small operating system with Own Backup on it, started
from a USB stick, a CD/DVD or an ISO attached to a virtual machine. Use it to:

- restore the **system disk** — the running system cannot overwrite itself;
- back up a computer that **does not start**, or a disk while the system on it
  is shut down (no snapshot is needed then);
- restore onto a **new, empty disk** or another computer.

Build it **now**, while the computer works, and check that the computer starts
from it.

There are two kinds. Both run the same window: back up, restore, mount,
validate, boot menu.

## Based on Windows Recovery Environment (Windows)

Built on Windows from what is already on the machine: the Windows Recovery
Environment (`Winre.wim`), the program with its window, the WebView2 runtime,
and optionally this computer's drivers. It reads NTFS and ReFS natively.

**Tools → Bootable media → Based on Microsoft Windows Recovery Environment
(x64) → Click to set up and build →** opens the build dialog:

- **Where to save the ISO.**
- **Compress WebView2 into the image** — the ISO is about 280 MB smaller, but
  at boot the runtime takes about 260 MB of RAM (WinPE keeps the whole image in
  memory). Off: the ISO is bigger, memory stays free.
- **Add this computer's drivers** — the build polls the disks and network
  adapters and offers the drivers that are not built into Windows. Without them
  the medium may not see NVMe or RAID disks.
- **Just-in-case tools** — programs from this computer (for example a file
  manager) copied onto the medium with their settings.
- **Screen resolution on the media** — a hint about the firmware's screen mode.

**Start** builds the ISO. The report shows the file, its size, the base build
and the drivers added. Write the ISO to a USB stick with Rufus or a similar tool,
or attach it to a virtual machine.

If the recovery image is not found, enable it with `reagentc /enable` or choose
a `Winre.wim` by hand (**Choose Winre.wim…**), for example one taken from a
Windows installation image. The card warns when the image was built for a
different Windows build than the one running.

### Working from it

The medium starts straight into the program. At the bottom of the screen is a
strip with a **Start** menu (the program, a command prompt, the tools you
added, **Restart**, **Shut down**), buttons of the open windows, the keyboard
layout and a clock. Closing the program's window does not end the session —
open it again from **Start**.

## Based on Linux (Debian 13)

A Debian 13 live system with the program on a small desktop (labwc) with its
own taskbar: a **Start** menu (**Backup**, **Terminal**, **Restart**, **Shut
down**), buttons of the open windows and a clock. It boots with **UEFI —
Secure Boot does not need to be turned off** (Debian's signed boot loader) —
and with legacy **BIOS**. It reads ext4, XFS, btrfs, LVM and LUKS directly, and
restores Windows backups as well (NTFS, BitLocker, boot configuration).

### Getting it

- **From the release page:** download `OwnBackup-Linux-<version>_amd64.iso` and
  check it against `SHA256SUMS.txt`.
- **On Linux, from the window:** **Tools → Bootable media → Based on Linux
  (Debian) x64 → Click to build the medium →**. The dialog asks **How do you want
  to get the medium?**
  - **Download the ready ISO from GitHub** — the medium of the installed
    version, checked against the release's checksums; a few minutes, no
    administrator password.
  - **Build on this computer** — about 15 minutes; about 1 GB of Debian
    packages is downloaded and about 10 GiB of temporary space is used; needs
    an administrator password.

  With **Write onto a USB stick** the ISO goes onto the chosen stick right away.
- **On Windows, from the window:** **Tools → Bootable media → Based on Linux
  (Debian) x64 → Click to download →**. The medium is built on Linux only, so
  Windows downloads the ready ISO of the installed program's version from
  GitHub and checks it against the release's checksum; writing it to a stick
  is not offered there yet.

### Writing it to a USB stick

On Linux the window writes it: **Write a ready ISO onto a stick…** — choose the
ISO and the stick (USB sticks, memory cards and removable disks are offered,
internal disks never). **Everything on the stick is erased.** The image is read
back to verify it, and the stick gets an **OBK-LOGS** partition where the medium
keeps its logs. On Windows use Rufus, balenaEtcher or a similar tool (the logs
then stay in memory).

### Working from it

Choose the stick in the computer's boot menu (usually F8, F11 or F12 at
power-on). The desktop opens with the program. The other consoles
(Ctrl+Alt+F2…) have a login prompt for repairs by hand. The logs of the
session go to the OBK-LOGS partition of the stick, so they survive a restart.

## Notes

- On the medium the service runs as a process; nothing is installed on the
  computer's disks.
- The medium does not keep jobs: add the backup with **Restore → Add backup**
  (a file or a folder) and restore it. Encrypted backups ask for their password
  or recovery key.
- New OBK images can only be opened by a medium of this version or newer:
  rebuild the medium after updating the program.
