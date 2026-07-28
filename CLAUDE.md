# My Own Backup

Source-available block-level backup system (Rust). Analogue of Acronis True Image.
**Windows-first**: Linux support is deferred until the Windows version works end to end.

**License (decision 2026-07-08): PolyForm Noncommercial 1.0.0** — free for
noncommercial use (forks too, staying noncommercial); commercial use needs a
separate license (`COMMERCIAL-LICENSE.md`). NOT OSI "open source" — say
"source-available / free for noncommercial use". `aim_win.rs`/`devio.rs` are
AGPL-derived (Arsenal Image Mounter) → **test-only, never distributed**; being
replaced by wnbd (LGPL-2.1) before release. See `THIRD-PARTY-NOTICES.md`.

Current roadmap: `.ai/implementation_plan_v2_2026-07-01.md` (phases 0–8, decisions log).
qcow2 second format + mounting: `.ai/qcow2-second-format-plan_2026-07-07.md` (Q0–Q5).

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
- **True removal is a hard product requirement (decision 2026-07-28):** excluded
  files must be REALLY removed from the image's filesystem on every supported
  FS — zero-filled ghost entries are a fallback, not the product. Status:
  NTFS/ReFS (driver mount-&-delete), FAT32/exFAT (direct patch), ext2/3/4
  (Stage 2) — done; btrfs Stage 2 v2 per этап C of
  `.ai/exclusions-full-repair-plan_2026-07-28.md`; xfs — этап D there, pending.
  Any change that edits foreign FS metadata must pass BOTH fsck-clean
  (`btrfs check --check-data-csum` / `e2fsck -fn` / `xfs_repair -n`) AND a
  mount-and-write acceptance before shipping — static checks alone once let a
  first-write corruption through.
- The legacy `.obk` format (zstd + AES-256-GCM + Argon2id) was **deleted**
  (decision 2026-07-26, D8 in `.ai/restore-designer-plan_2026-07-26.md`): it had
  no writer left in any live path and its dead reader confused the restore work.
  `ImageManifest` (`common/src/image.rs`) survived it and is now the sidecar's
  manifest. Do not resurrect the container format.

## Architecture

Cargo workspace with crates:
- `common` — shared types, error, image/manifest structs, config
- `inventory` — disk enumeration (WMI via wmic/PowerShell on Windows, mock for tests)
- `snapshot` — snapshot providers (VSS on Windows — stub pending phase 2, mock for tests)
- `backup` — block pipeline, used-sector maps, VHDX/qcow2 handling, checksums,
  exclusion scanners, LUKS/BitLocker
- `restore` — restore-target guards (system-disk protection)
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
- gRPC production transport: DACL-restricted **named pipe** `\\.\pipe\OwnBackup`
  on Windows, **UDS** on Unix — no listening TCP port. Foreground/dev uses
  loopback TCP:50051. Bearer token still required (defence in depth); no TLS
- SQLite with `bundled` feature for cross-compilation
- No email notifications (decision 2026-07-01); local notifications only
