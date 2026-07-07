# My Own Backup

Open-source block-level backup system (Rust). Analogue of Acronis True Image.
**Windows-first**: Linux support is deferred until the Windows version works end to end.

Current roadmap: `.ai/implementation_plan_v2_2026-07-01.md` (phases 0–8, decisions log).

## Build

```bash
cargo build          # build all crates (Linux, mocks only)
cargo test           # run all tests (76+, run on mocks)
cargo xwin check --target x86_64-pc-windows-msvc -p own-backup-backup ...  # fast Windows check
cargo xwin build --target x86_64-pc-windows-msvc --release  # Windows cross-compile
```

Real Windows behaviour (VSS, VHDX attach, FSCTL, raw device I/O) can only be
tested on a Windows host — cross-compilation validates compilation only.

## Backup format (decided 2026-07-01)

- **Native format: dynamic VHDX** + sidecar manifest `.obkm` (disk metadata,
  xxHash64 block checksums). VHDX mounts natively on Windows (AttachVirtualDisk,
  read-only by default) and on Linux via qemu-nbd/guestmount.
- Only used NTFS clusters are backed up (`FSCTL_GET_VOLUME_BITMAP`); file/path
  exclusions via `FSCTL_GET_RETRIEVAL_POINTERS` (clusters zeroed) **plus true
  removal** (decision 2026-07-07): after the copy, a temporary differencing
  child of the image is mounted and the Windows FS driver itself deletes the
  excluded entries; the child is merged down (`MergeVirtualDisk`) and only the
  touched blocks are re-checksummed (`true_removal_win.rs`, `vhdx_bat.rs`).
  Works for NTFS and ReFS; on failure before the merge the backup gracefully
  keeps zero-filled ghosts. FAT32/exFAT true removal patches the FS metadata
  directly (`fat_exclusions.rs`).
- Incremental/differential = differencing VHDX (AVHDX chains); consolidation via
  `MergeVirtualDisk`.
- The legacy `.obk` format (zstd + AES-256-GCM + Argon2id) is kept as a future
  **archival** export format (phase 8) — do not delete that code.
- No kernel drivers, no WinFsp/Dokan, no NTFS parser: mounting and FS access go
  through native Windows facilities only. (Confirmed 2026-07-07: this rule is
  NOT an obstacle to Acronis-style full exclusion removal — the mount-&-delete
  pass achieves it via the OS driver without parsing NTFS/ReFS.)

## Architecture

Cargo workspace with crates:
- `common` — shared types, error, image/manifest structs, config
- `inventory` — disk enumeration (WMI via wmic/PowerShell on Windows, mock for tests)
- `snapshot` — snapshot providers (VSS on Windows — stub pending phase 2, mock for tests)
- `backup` — block pipeline, used-sector maps, VHDX handling, checksums; plus
  archival zstd/AES code (frozen)
- `restore` — block writer, restore pipeline, validation
- `metadata` — SQLite DB for backup jobs, sets, retention, activity log
- `scheduler` — cron-based job scheduling (wired into service in phase 4)
- `service` — Windows Service + gRPC server (tonic)
- `cli` — command-line interface (clap)
- `gui` — Tauri v2 + Svelte 5 (frontend being rebuilt per `design/` mockup:
  4 sidebar sections — Backup, Tools, Settings, Help; ru/en, light/dark themes)

## Key patterns

- Windows-specific code gated with `#[cfg(target_os = "windows")]`
- All traits (`BlockReader`, `BlockWriter`, `UsedSectorMap`, `SnapshotProvider`,
  `DiskInventory`) have mock implementations for Linux testing
- Unimplemented gRPC methods must return `Status::unimplemented` — never fake success
- gRPC on localhost:50051 (TCP + bearer token) / UDS on Unix; no TLS
- SQLite with `bundled` feature for cross-compilation
- No email notifications (decision 2026-07-01); local notifications only
