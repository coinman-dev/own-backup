[English](CHANGELOG.md) | [Русский](CHANGELOG.ru_RU.md)

# Changelog

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
