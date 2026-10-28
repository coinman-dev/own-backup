[English](CHANGELOG.md) | [Русский](CHANGELOG.ru_RU.md)

# Changelog

## Unreleased

### Added

- **Windows: disk cleanup.** A new section, **Cleanup**, before Settings:
  temporary files, Windows error reports, crash dumps, setup logs, the WinINet
  and shader caches, thumbnails, Prefetch and the Recycle Bin, for the system
  and the chosen users. **Analyse** shows how much each item takes, **Clean
  up** removes it — plainly and fast, not into the Recycle Bin. **Files**
  lists what an item will remove (search, largest first), and the cleanup
  removes exactly that list. Only the
  folders of the items are touched, never a mask over a whole disk; files
  younger than a day in the temporary folders, files in use, links and
  junctions stay. The result goes into Activity.
- **Cleanup: browsers.** The caches and crash reports of Chrome, Edge,
  Vivaldi, Brave, Yandex, Opera, Firefox and Thunderbird in every profile;
  history, cookies and sessions — open tabs (off by default; cookies and
  sessions are separate items, sign-ins live in the cookies). A browser that is open is left and the item says to
  close it; passwords, bookmarks and autofill are never touched, nor
  Firefox's history, which shares a database with the bookmarks.
  Browser history can keep its last day, week or month: only older visits
  are removed from the history database, edited on a copy, checked and then
  put in place.
- **Cleanup: programs.** Microsoft Office recent documents (the places
  pinned to the lists stay), the recent files of Paint, WordPad, Windows
  Media Player, MPC-HC, 7-Zip, WinRAR, Adobe Acrobat Reader; the caches of
  XnView MP, Discord, Visual Studio Code, Steam, Telegram Desktop, npm, pip,
  Yarn and NuGet; the logs of qBittorrent, Zed, DaVinci Resolve, OBS Studio;
  NVIDIA driver installers. The programs' settings and data are not touched;
  a program that is open is left.
- **Cleanup: Windows traces** (off by default): recent documents (shortcuts
  and the registry's lists, the Open and Save As dialogs' too), jump lists
  (the folders pinned to Quick access stay), the Run box's, Explorer's
  address bar's and search history, Remote Desktop connections (saved
  passwords stay). A user who is not signed in has their registry loaded by
  the service for itself alone and only for the cleanup; the **Files** list
  shows what each registry entry holds.

### Changed

- **The chain number in backup file names.** A new full starts the next
  number and its versions carry it: `Disk_C_Win11_disk4_3-full_….obk`,
  `Disk_C_Win11_disk4_3-inc_….obk`, so it is plain which full an increment
  belongs to, and sorted by name a folder lists each chain together. Backups
  made before keep their names (a version finds its parent by that name), and
  versions added to such a chain stay unnumbered; the next full is numbered
  past the chains already in the folder.

## 0.1.8-beta — 2026-10-28

### Added

- **Windows: Linux partitions in Total Commander.** A file system plugin
  shows the files of this computer's Linux partitions — ext2/3/4 and btrfs,
  also inside LVM and LUKS encryption (an Ubuntu installed with disk
  encryption) — read-only, in place: nothing is attached, and the other
  partitions of the same disk stay in use. Total Commander asks for the
  encryption passphrase. btrfs is shown the way the system sees it after
  boot, by its `/etc/fstab` (`@` is the root, `@home` is `\home`);
  compressed files (zstd, zlib, LZO) are read as they are. The installer
  puts the plugin's archive into
  `C:\Program Files\OwnBackup\TotalCommander\OwnBackup-TC-plugin.zip`:
  opened in Total Commander, it installs the plugin. The service does the
  reading, so it must be running.
- **Linux boot medium:** the keyboard layout indicator on the taskbar. When
  the medium is built in the Linux window, the "Tools just in case" box, as
  for the Windows medium, adds the Krusader file manager and the FeatherPad
  editor (the ready ISO from GitHub has neither).

### Fixed

- The service no longer waits forever, when it is stopped, for a client
  that keeps its connection open without answering (Total Commander with
  the plugin): the installer hung on "Stopping the Own Backup service".
  Open connections now get 3 seconds to close.

### Changed

- Release files carry the version in their names:
  `own-backup-install_0.1.8-beta.exe`, `own-backup_0.1.8.beta_amd64.deb`,
  `OwnBackup-Linux-0.1.8.beta_amd64.iso`. The release package has no Debian
  revision (`0.1.8~beta`); the window still finds the media of earlier
  releases, named with one.

## 0.1.7-beta — 2026-10-26

### Added

- **Windows: the ready Linux boot medium.** The "Based on Linux (Debian)
  x64" card is no longer "in development": the window downloads the boot
  medium ISO of its own version from the GitHub release, checks it against
  SHA256SUMS.txt and saves it without replacing an existing file (to
  Downloads by default). Writing it onto a USB stick is not in the Windows
  window yet — use Rufus in DD mode.
- **Queued instead of refused.** An action that needs only its own
  resources — verifying, exporting or deleting backups, building the boot
  medium, writing a stick — now waits its turn when they are busy (for
  example, verifying a backup while another job writes into the same
  folder). The window shows "Queued: … — starts when … ends" with a Cancel
  button. An action on the whole machine (restore, a drive letter) is still
  refused with the reason.
- **History in the window's language.** The notes of a run (why files were
  not listed, why excluded entries stayed in the image, what moved in the
  partition layout) are stored in both languages, and the window shows them
  in its current language. Runs recorded earlier are shown as recorded.
- User documentation in English and Russian (`doc/`).

### Fixed

- The end of a backup no longer hangs on a stuck device: attaching and
  detaching the image to delete excluded entries now gives up after 5
  minutes each. The image then keeps the excluded entries zeroed, and the
  result says so.
- When Windows cannot give memory for the disk read buffer, the service
  says that the system ran out of memory and how much was asked for, instead
  of "aligned buffer allocation failed".
- After a language switch the window asks the service again for the disks,
  the jobs and the history, so partition labels and run notes no longer stay
  in the old language until the window is reopened.
- Linux: a backup read from an LVM or device-mapper snapshot is no longer
  called a "VSS snapshot" (the window, the Activity page, the command line).
- The boot medium card on Windows 11 compared the medium with Windows build
  9200: the real build is now read (RtlGetVersion).
- Run notes in Activity wrap between words, not inside them.
- Windows: when the log mirror on a USB stick (or another disk) grew past
  4 MB, the previous log was moved into the service's own folder
  (System32) instead of staying beside the new one as `.log.old`.
- A release has one checksum list, SHA256SUMS.txt, for all its files.
- The zstd compression library (qcow2 and OBK) is updated to 0.14.3, which
  fixes the security advisories RUSTSEC-2026-0391, -0398, -0403 and -0410
  and the unsound APIs reported with them.

## 0.1.6-beta — 2026-10-24

### Added

- **Linux version.** A `.deb` package for Ubuntu 24.04+ and Debian 13+
  (amd64) with the same service, window and command line as on Windows. It
  backs up volumes and whole disks with ext4, XFS, btrfs, NTFS, FAT32 and
  exFAT, including LVM (every logical volume of a physical volume in one
  snapshot) and open LUKS containers (read through a device-mapper
  snapshot, the container header kept in the copy). Full, incremental and
  differential chains in VHDX, qcow2 and encrypted OBK; exclusions that
  really remove the files on ext4, XFS, btrfs and FAT, and on NTFS through
  ntfs-3g. The service key store uses systemd-creds (with the TPM 2.0 when
  there is one). A volume or a whole disk is restored through the service,
  with the choice to give the restored file systems, LUKS containers and
  volume groups new identities (UUIDs, labels, initramfs rebuilt) when the
  originals are still on the machine. The **Boot** page edits UEFI boot
  entries; the window has a tray icon and starts with the session. The
  service runs under systemd, asks polkit before destructive actions and
  logs to journald and a file.
- Backups are interchangeable between Windows and Linux, VHDX chains
  included: a copy made on one system opens, validates and restores on the
  other.
- **Linux boot medium.** A Debian 13 live ISO with the program on a labwc
  desktop with our own taskbar; it boots with UEFI (Secure Boot through
  Debian's signed shim) and BIOS, from a CD or a USB stick, and restores
  Windows copies as well as Linux ones. The window builds it from the
  installed package or downloads the ready one of the same version from the
  release, and writes it onto a USB stick.
- The boot medium card lets you choose a Winre.wim by hand (for example one
  taken from a Windows installation image). The service reads it first and
  refuses an installation image, a split or non-x64 image with the reason;
  a build given such a file is refused the same way.

### Fixed

- An unfinished VHDX or qcow2 image of a run whose service was killed is now
  deleted when the service starts again (or at the next run, if the backup
  disk was not connected). The service records each such file in its
  protected folder before writing it, so only files it has proved its own
  are deleted. Leftovers of earlier versions and of runs outside the service
  stay where they are.
- The FAT boot image inside the medium's ISO keeps long names
  (`EFI\Microsoft`, `boot.stl`) instead of cutting them to 8.3.
- The empty part of the compression slider follows the theme.
- A job or history save that fails (for example on a full disk) no longer
  leaves a half-written temporary file holding the space.
- The window and the service exchange messages up to 64 MB: a job list
  past 4 MB (very long exclusion lists) no longer breaks every request.
- A source volume keeps its chain when it is named differently: by a new
  drive letter, its volume GUID path or a mount point. It used to start a new
  full copy, because the chain was matched by the name in the file name.
- A mount point (`C:\Mount\Data`) works as a source: it is read as the
  volume mounted there, its images are named by that volume, and a
  destination elsewhere on C: is no longer refused as "the same volume".
- A ReFS volume inside BitLocker is copied with its used-block map and its
  exclusions applied, locked or unlocked. It used to fail: under BitLocker
  ReFS ends before the device does, the cluster size could not be derived,
  and the sector-by-sector fallback hit the unreadable tail.
- A job remembers the folders it wrote into before its destination was
  changed, and deleting the job deletes its copies there too — even after
  the activity journal (200 runs) has forgotten that folder.
- A chain never grows past 60 images: the next run starts a new full copy
  whatever the scheme. A job of "only increments" without consolidation
  (saved before the modes existed, or made outside the window) used to grow
  past what the VHDX reader follows (64), after which every new copy was
  unreadable and retention stopped.
- XFS: every excluded file is now truly removed (1403 of 1403 on the test
  corpus, was 1369). A directory data block that would be left empty is taken
  out of the directory and returned to free space, as the kernel does, and
  is zeroed so the deleted names do not stay behind.
- The service restarts by itself after a crash (in 10 s, then 1 and 5 min).
  It used to stay stopped until the next reboot, and scheduled copies with it.
- A whole-disk restore keeps the partitions that are not in the backup, on
  Windows and on Linux. It used to rewrite the partition table from the
  backup alone.
- Restoring a disk keeps the boot code in LBA 0 (MBR) when the partition
  table is rewritten; on Linux the sectors after it (GRUB's core.img) stay
  too.
- NTFS exclusions cover named streams (Zone.Identifier and others) and
  content stored inside the MFT record. A file with many names or streams,
  whose record continues in extension records, no longer drops out of the
  exclusions entirely.
- qcow2: after consolidation the old bytes of replaced clusters are wiped,
  so excluded content does not survive in dead clusters.
- A backup interrupted because the service was stopped or killed is
  recorded as interrupted, and the schedule runs it again.
- The service no longer does one thing at a time for the whole machine:
  a restore is still exclusive, but copies, checks and mounts of different
  backup folders and the boot medium build run side by side; a scheduled
  check gives way to an action of a person on the same folder.
- A scheduled whole-disk backup with an encrypted partition needs the saved
  password only for that partition (ESP and `/boot` next to LUKS used to
  make every run skip), and a skipped run is shown under **Activity**.
- The end of a backup no longer waits forever behind a hung device: each
  VSS step has a time limit, and the window says "releasing the volume
  snapshot" or "opening the finished backup" instead of "flushing the
  cache".
- The result says how many excluded files an ext4, XFS or btrfs volume
  kept under their names with their contents zeroed.
- Validating an image under **Tools** keeps its progress bar and result
  when you leave the page and come back.
- On Windows the service writes the settings and the service data folder
  is open only to SYSTEM and administrators.
- Error messages no longer start with an English prefix.
- Dependencies: tokio past an I/O-safety flaw (RUSTSEC-2026-0386), Vite past
  advisories of its development server, and zbus 6 instead of the
  unmaintained zvariant crates; the Linux taskbar calls libgtk-layer-shell
  directly instead of its archived Rust bindings.

## 0.1.5-beta — 2026-10-17

### Added

- Tools assign and remove drive letters of hidden system partitions (EFI,
  recovery) and show the disk model.
- When writing to the backup disk stops moving — a failing or disconnected
  disk that answers with errors — the job panel says so after two minutes.
  Cancelling such a run works and frees the queue even while the write
  itself hangs; the unfinished file publishes nothing.
- The job panel says that the source is an encrypted volume and that the
  copy holds it decrypted, with a warning when the copy is not encrypted.
- The full license texts of all third-party components are installed next
  to the program (THIRD-PARTY-LICENSES.txt).

### Changed

- The boot medium's taskbar speaks the language of Windows from the start.
- The EFI partition scan tells Acronis loaders apart and recognises the
  program's own boot environment.

### Fixed

- VHDX and qcow2 are written under a private name and get their real one only
  when finished. A service stopped while excluded files were being deleted
  used to leave a half-made image that looked like a finished version and
  became the parent of the next increment, and after such a crash old copies
  of that job stopped being cleaned up.
- After an interrupted exclusion pass in an OBK, the next run also removes
  its scratch file.
- A run that could not be written into the program's history is marked on
  the job panel and written before the next run.
- Exclusions work again on XFS made by mkfs.xfs 6.x (nrext64) — such volumes
  used to be copied without them. True removal on XFS no longer backs off from
  the reverse-mapping tree: on the test set it deletes 1369 of 1403 files
  instead of 172.
- Restoring from a new boot medium brings back the boot loader of an older
  Windows too.
- Restoring on the medium no longer says the disk is not connected before the
  password is entered, nor warns about another machine because of the WinPE
  machine name.
- Without WebView2, the installed program advises installing the runtime
  rather than rebuilding the boot medium.

## 0.1.4-beta — 2026-10-14

### Changed

- New VHDX and qcow2 disk chains reserve metadata space for all addressable
  source blocks, so filling a disk does not prematurely start another full
  backup. OBK increments reserve their own metadata space as needed.

### Fixed

- A disk backup job follows partitions that Windows Update shrank, moved or
  recreated, such as the recovery partition. Before each run the job is
  matched against the disk as it is now and updated; the next copy is a full
  one, and the result says why. Such jobs used to fail with "file not found".
  A data partition that disappeared is never replaced by a guess.
- An incremental disk copy is never written over a parent copy with a
  different partition layout, which would have restored data into the wrong
  places.
- A copy that had to become a full one is named and recorded as a full copy,
  not with the name of the planned increment.
- Stopping an iSCSI target on Windows checks cancellation during socket waits,
  so an idle session no longer keeps the image open until its read timeout.
- Encrypted jobs can regain automatic access after their locally stored
  authorization is damaged, using the recovery key.
- Completed OBK images are published only after their data is durable; a
  restart removes unfinished images without exposing them as restore points.
- A backup that could not remove every excluded file reports the incomplete
  result instead of silently claiming full exclusion.
- Scheduled verification and the recovery of interrupted backup operations
  have been tightened.

## 0.1.3-beta — 2026-10-06

### Added

- A menu beside each backup job, also available with a right click: rename,
  check the latest version or all versions, clean up selected versions, clone
  settings, mount a version as a disk, open the backup folder, save a version
  as a standalone VHDX, and delete the job.
- Version cleanup shows chains and their dependencies. Selecting a copy also
  selects the versions built on it; deletion requires confirmation and refuses
  to leave a version without its parent.
- Saving as VHDX follows the selected version's whole chain. The result opens
  with Windows alone, is independent of the job, and never replaces an existing
  file. Encrypted copies require a password or recovery key and are exported
  without encryption, with a warning before saving.

### Changed

- The bottom strips of the menu, of the list and of the action bar are the same
  height, so their top lines meet in one line across the window. The buttons of
  the action bar sit as far from the bottom of the window as from that line;
  there used to be more room below them than above.

### Fixed

- Checking an image from the command line no longer asks for administrator
  rights: verification reads image files directly.
- The installer no longer unpacks a copy of the service program into the
  temporary folder to stop the running service. An antivirus held that copy,
  `service-control.exe`, for analysis, and the installation stood still. The
  service is now stopped by the program that is already installed.

## 0.1.2-beta — 2026-10-06

The first published build. Earlier versions were handed over as separate
executables and have no entries here.

### Added

- **Installer** `own-backup-install.exe`, in Russian and English. It installs
  the program for all users into `C:\Program Files\OwnBackup`, registers and
  starts the backup service, and updates an earlier installation where it is.
  `/S` runs it without questions.
- The uninstaller keeps settings, jobs and history unless asked to delete them;
  backups are never touched. Deleting the settings also removes the remembered
  volume keys and what the window remembered: the theme and the language.
- The large mark in the side strip has a white rim and lets a tenth of the
  strip show through.
- Releases are built by GitHub from a version tag.

### Changed

- `own-backup-service.exe /install` on a computer that already has the service
  points it at the copy it was run from, instead of failing with "the service
  already exists".
- `LICENSE` holds the PolyForm Noncommercial License 1.0.0 as published; the
  required notice moved to `NOTICE`.

### Fixed

- The theme switch at the bottom of the side strip no longer turns **Same as
  Windows** into a fixed theme when the press changes nothing on screen.
