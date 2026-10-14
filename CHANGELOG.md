[English](CHANGELOG.md) | [Русский](CHANGELOG.ru_RU.md)

# Changelog

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
