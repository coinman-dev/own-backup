[English](CHANGELOG.md) | [Русский](CHANGELOG.ru_RU.md)

# Changelog

## Unreleased

### Added

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
