# Tools

![The Tools page](../images/en/tools.png)

## Mount image as a disk

Attaches a backup image as a disk so its contents can be opened and files
copied out — the way to restore individual files.

1. Type the path of an image or click **Browse…** (`.vhdx`, `.avhdx`, `.qcow2`,
   `.obk`). An incremental version is mounted together with its chain.
2. Leave **Allow writing** off unless you need it: a read-only mount cannot
   change the backup.
3. Click **Mount**. An encrypted (OBK) backup asks for its password or recovery
   key first.

On **Windows** the image is attached through Windows' own iSCSI initiator (no
third-party driver is installed); its volumes get drive letters and open in
File Explorer. On **Linux** the image is served through the kernel's NBD
device, read-only, and its volume is mounted into a folder.

The list below shows what is mounted; **Unmount** detaches the selected image.

The same is reachable from a job: **⌄ → Mount as a disk…** lets you choose the
version.

## Validate image

Checks every block of an image against the checksums stored in it. Choose the
image and click **Validate**; an incremental version is checked with its
chain. The result is either *Integrity confirmed: N blocks in S s* or *The
backup is damaged: checksums do not match for N of M blocks*. The progress
bar and the result stay if you leave the page and come back.

## Letters for hidden partitions

Windows gives no drive letter to the EFI system partition, the Windows recovery
environment or vendor partitions. Here you can **Assign** a letter so the
partition shows up in Explorer, and **Remove letter** when done. Explorer opens
the EFI partition only as an administrator. Do not keep the letters for long:
Windows and its updates expect these partitions to stay hidden. (Windows only.)

## Bootable media

![Bootable media](../images/en/tools-bootable-media.png)

Builds the boot medium — a system that starts from a USB stick or a CD with the
program on it, to restore the system disk or back up a computer that does not
start. Two kinds:

- **Based on Microsoft Windows Recovery Environment (x64)** — built on Windows
  from what is already on the machine: the Windows Recovery Environment, the
  program, the WebView2 runtime and, optionally, this computer's drivers.
- **Based on Linux (Debian) x64** — a small Debian system with the program; it
  reads ext4, btrfs, XFS and LUKS directly and restores Windows backups too.
  On Linux it is built from the installed package or downloaded ready from the
  release; on Windows, for now, only a description is shown (download the
  ISO from the release page).

Details, writing to a USB stick and working from the medium are in
[Bootable media](10-bootable-media.md).
