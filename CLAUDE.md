# My Own Backup

Source-available block-level backup system (Rust). Analogue of Acronis True Image.
**Windows-first**: Linux support is deferred until the Windows version works end to end.

**License (decision 2026-07-08): PolyForm Noncommercial 1.0.0** — free for
noncommercial use (forks too, staying noncommercial); commercial use needs a
separate license (`COMMERCIAL-LICENSE.md`). NOT OSI "open source" — say
"source-available / free for noncommercial use". `aim_win.rs`/`devio.rs` are
AGPL-derived (Arsenal Image Mounter) → **test-only, never distributed**; being
replaced by wnbd (LGPL-2.1) before release. See `THIRD-PARTY-NOTICES.md`.

**There is one plan: `.ai/PLAN.md`** (consolidated 2026-08-20 — done / done but
unverified / not done, plus the decisions that must not be reopened). The
twenty-three plans it replaced are in `.ai/archive-plans-until-2026-08-20.tar.gz`
— `.ai` is not under git, so that tarball is their only copy. Unpack it for a
decision log, never as a to-do list: several of those files listed closed work
as open. Check the code before trusting any of them.

**BitLocker:** the FVE parser (`backup/src/bitlocker.rs`) opens a real volume —
confirmed on Windows 10 / XTS-AES 128 on 2026-08-05 via `own-backup-cli
bitlocker --unlock`, password protector only. It reads the ciphertext off
`\\.\PhysicalDriveN` at the partition's offset, because a locked volume
refuses reads through its own device (`FVE_E_LOCKED_VOLUME`). **Wired into
backup** (`engine_win`: `open_bitlocker_source` + `unlock_and_probe` — a locked
volume is backed up DECRYPTED through a window onto its disk; recovery-key
protector too; a half-encrypted volume is refused). **The whole path was run on
the boot medium on 2026-08-19 and works**: a 128 GiB locked system volume opened
by its 48-digit recovery key, copied decrypted (XTS-AES 128), and the finished
image mounted — the in-image exclusion pass walked 703 317 entries in it and
deleted 23 499 files. That run also settled the open question: the recovery key
is hashed with **one** SHA-256, not two.

**Three things that must never be conflated again (2026-08-20, found by one
live run, each hiding the next):** (1) "is there anything a person can type"
is NOT "does our parser know the cipher" — Windows encrypts REMOVABLE drives
in compatible mode (AES-CBC) by default, so conflating them refused every
ordinary flash drive; (2) "wrong password" means exactly one thing, an AES-CCM
tag mismatch — everything else must go to the Windows-unlock road instead;
(3) a container holds more than NTFS, and the "did it decrypt sensibly" check
must accept FAT32/exFAT/ReFS. Also: the sector size in the BPB is ZERO for
exFAT and ReFS — take it from the device geometry, never guess 512.

Still open — the negative cases (wrong password, TPM-only volume, AES-CBC
refusal), restoring such a copy, and the conversion-state field:
`.ai/PLAN.md` §2–3.

## Branches (decision 2026-08-06)

Work happens on **`dev`** — commit and push there. **`main` is the release
branch**: it is fast-forwarded from `dev` when something is worth publishing,
not committed to directly. (`winpe-boot-media` was renamed to `dev` once its
work landed in `main`; there is no feature-branch-per-task habit here.)

## Build

```bash
cargo build          # build all crates (Linux, mocks only)
cargo test           # run all tests (556 pass, 16 #[ignore] need root)
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
- **Sliding increment window (2026-08-21):** a job may keep at most N
  increments — the oldest is folded into the base, the next repointed at it,
  the folded file deleted LAST so a crash is finishable (`consolidate.rs`,
  marker `<base>.consolidating.json`). Nothing is re-encoded and no checksum is
  recomputed: the increment's sidecar already describes the whole state, so the
  base inherits it. **Differential chains are never consolidated** — their
  children are siblings on the base, and moving the base would silently rebase
  them onto a moment they were never computed against. qcow2 is done and
  validated by `qemu-img`. Consolidating deletes files, so anything counting a
  chain's versions must use `Chain::increments_ever()`, never `images.len()`.
  **The VHDX half RAN AND WAS VERIFIED on 2026-08-21** (before that it had only
  ever compiled): `full ← inc1 ← inc2 ← inc3` became `full ← inc2 ← inc3 ←
  inc4` — oldest folded, next repointed, folded file deleted, marker cleared.
  Verified against the FILES, not the log: the surviving increment reads
  byte-for-byte identically through the old and the new chain (0 differing
  blocks of 2338 across the whole 2.4 GiB virtual disk — that is the acceptance
  criterion, since consolidation must change nothing observable); the victim's
  block is in the base; the base inherited the folded increment's sidecar (its
  `backup_id` and `created_at`) promoted to `full`, with
  `consolidated_increments=1` and `chain_started_at` still holding the ORIGINAL
  full's timestamp; the repointed increment's `parent_backup_id` and
  `parent_image_file_name` follow the base's new identity.
  That run also DISPROVED something this module relied on: `MergeVirtualDisk`
  did NOT change the base's `DataWriteGuid` — the whole chain shares one and it
  survived the merge, because userspace writing means nothing bumps it any
  more. The crash-resume cross-check "the child's parent linkage went stale ⇒
  the fold already happened" could therefore never fire, and a crash between a
  successful merge and the marker write would have left the chain in its one
  unreadable state (base folded, next not yet repointed) with the resume
  refusing to finish it. It now asks the DATA instead
  (`consolidate::merge_already_applied`: does the base already carry every
  block the victim holds?), keeping the linkage only as a bonus signal.
- **True removal is a hard product requirement (decision 2026-07-28):** excluded
  files must be REALLY removed from the image's filesystem on every supported
  FS — zero-filled ghost entries are a fallback, not the product. Status:
  NTFS/ReFS (driver mount-&-delete), FAT32/exFAT (direct patch — **verified
  2026-08-20 inside a LOCKED BitLocker container too**, the last step of the
  exclusions plan: the volume stays locked, Windows never mounts it, and the
  used map is built over our own decrypted window), ext2/3/4
  (Stage 2) and btrfs (Stage 2 v2, enabled in the engine) — done;
  xfs — Stage 1 only, D0/D1 done and the writer missing (`.ai/PLAN.md` §3.2;
  the kernel's reference delta says a single unlink touches seven structure
  classes — RMAPBT and FINOBT included, both on by default in modern mkfs).
  **An encrypted source honours the rules too (2026-08-07).** A locked BitLocker
  volume is not mounted by Windows, so nothing can be asked which clusters a
  file holds before the copy — the exclusions used to be dropped entirely (12
  configured rules, `excluded_files=0`, a warning in the log and nothing on
  screen). They now run AFTER the copy, inside the image, which holds that NTFS
  in the clear: mount a differencing child → walk it with the same matcher live
  volumes get → the FS driver deletes the matches → dismount → zero the clusters
  they held, **but only those `$Bitmap` still calls free**, so a cluster the
  filesystem took back for its own metadata is never overwritten → merge and
  re-checksum (`true_removal_win::exclude_inside_image`). Without that zero pass
  it would be a rename, not a removal: the data would sit in the image's free
  space, out of the catalog and fully recoverable. Both formats: VHDX through a
  differencing child, qcow2 through an AIM RW mount over a CoW overlay
  (`qcow2_removal_win::exclude_inside_qcow2`) — the scan/delete/zero half is
  shared (`scan_delete_zero_on_disk`), only the attach differs. **NTFS inside
  only.** ReFS is not a mounting problem (AIM and the VHDX driver mount it
  fine) but a free-map one: the zero pass must know which clusters the FS still
  calls free, and ReFS's allocation state is not readable from the raw image.
  The route in is `FSCTL_GET_VOLUME_BITMAP`, which ReFS does answer and
  `used_sectors_win` already uses — it just has to be asked while the volume is
  LOCKED and before it is dismounted, so nothing writes between the answer and
  the zeroing. Planned in `.ai/PLAN.md` §3.3
  (R1 opens that window inside `flush_lock_dismount_volume` — the
  function that fought the ~180 s PnP removal veto, so it lands and gets
  verified on its own). Until then ReFS says out loud that it did not apply the
  rules.
  **This pass is slow on a spinning disk, and that is measured, not assumed
  (2026-08-08).** On the boot medium, 12 937 excluded files (1.9 GiB) out of an
  84 GiB BitLocker volume: qcow2 81 s, VHDX 420 s. The VHDX half goes to the
  merge (~95 s) and the re-checksum of 3467 blocks (~260 s). Four shapes of that
  re-checksum were measured on the same machine and image — one read per block
  through the attached disk (255 s), 16 MiB runs still attached (245 s), runs
  read from the file (268 s), runs through a reader that parses the BAT once
  (260 s). It is therefore neither the read count, nor the attach, nor the
  per-block BAT re-parse; ~75 ms per megabyte stayed put through all of them.
  The remaining suspect is the destination drive: an ST2000DM008 is **SMR**, and
  this ran right after it took 84 GiB of writes. The same run's merge moved
  7 GiB at 72 MB/s, so the disk is not slow in general — only afterwards. **Do
  not re-optimise this without first measuring on a CMR disk or an SSD**; three
  of those four rows are the cost of assuming the code was at fault. qcow2 is
  faster for structural reasons that will not change: zstd means fewer physical
  bytes, and its 64 KiB CoW clusters amplify scattered MFT writes 16× less than
  the VHDX child's 1 MiB blocks (delete: 0.7 s vs 9 s).
  **DONE 2026-08-20 — the in-image pass is now the FALLBACK, not the main road**
  (`.ai/PLAN.md` §1). Exclusions are
  resolved BEFORE the copy by reading `$MFT` (`backup/src/ntfs_scan.rs`, crate
  `ntfs` 0.4): matched extents are subtracted from the used map
  (`ntfs_map::build_ntfs_used_map_excl`), so excluded data is never read, never
  written, and there is nothing to zero afterwards; only the catalog entries are
  deleted (`remove_excluded_from_image`, the same pass unlocked volumes use).
  This is what Acronis does — its file filters stop working when the MFT/run
  list/bitmap is corrupt (kb.acronis.com/content/36993) — except we also delete
  the entries, so no ghosts.
  **The same reader serves every source**: a mounted volume (or its VSS snapshot
  device) and a locked BitLocker volume through its decrypted window. The
  directory walk stays as the fallback and picks itself when the MFT cannot be
  trusted: ReFS (no MFT), a dirty volume (`IS_DIRTY`), an unparsed record, or a
  source that is NOT frozen — no VSS snapshot and not media mode, because raw
  metadata of a volume being written cannot be parsed safely.
  Measured, on the same data: tree walk 45–52 s on a live SSD volume and 959 s
  inside an image on SMR, versus 15–28 s for the MFT read. Verified against
  Windows itself (`FSCTL_GET_RETRIEVAL_POINTERS` via `own-backup-cli
  scan-exclusions --method both`): **byte-for-byte on every common file**, twice
  — 17 889/17 890 on the image (the one diff was a live database) and
  17 958/17 958 on a live volume. The MFT road also excludes what the walk
  cannot: `pagefile.sys`, `swapfile.sys` and `DumpStack.log.tmp` refuse to open
  on a live volume ("file in use") and the walk left 176 MiB of them in the copy.
  Acceptance of an exclusion run is by CLUSTERS, never by grepping the image for
  a marker string: on a real run both the excluded file and the control file
  turned out NTFS-COMPRESSED, so the plaintext was in neither, and a
  grep-based test would have passed for the wrong reason. Use
  `scripts/ntfs-mft-probe.py IMAGE --check NAME` — it finds the file in the
  image's own MFT and reports whether the record is live and how many of its
  clusters carry data (excluded: deleted / 0; control: live / all).
  Any change that edits foreign FS metadata must pass BOTH fsck-clean
  (`btrfs check --check-data-csum` / `e2fsck -fn` / `xfs_repair -n`) AND a
  mount-and-write acceptance before shipping — static checks alone once let a
  first-write corruption through. **No new scanner or stage merges without both
  steps.** The second one is a single command: `bash scripts/verify-mounted.sh`
  (run as yourself; it takes sudo only for the run — ext/xfs/btrfs, mount → read
  everything → write to ENOSPC → read again → unmount → static recheck). Its
  fixture is the shared corpus in `backup/src/exclusion_corpus.rs`, built by the
  kernel through a mount (dense and nested-dense dirs, hard links across the
  exclusion, device nodes/FIFO/sockets, fast+slow symlinks, shared and private
  xattr blocks, reflinks; btrfs adds a subvolume, a snapshot, DUP metadata and a
  name-hash collision) — extend THAT, not a copy in one test. **fsck-clean is not proof:** `btrfs check`
  reported "no error found" on a copy whose csum-tree parent keys were stale
  (kernel: `tree first key mismatch` → EIO → init took SIGBUS). Hence our own
  structural invariants in the test harness (parent key == child's first key,
  csums only over allocated extents) and fixtures big enough to have
  MULTI-LEVEL trees — a flat image has no parent node to get wrong.
- The legacy `.obk` format (zstd + AES-256-GCM + Argon2id) was **deleted**
  (decision 2026-07-26): it had
  no writer left in any live path and its dead reader confused the restore work.
  `ImageManifest` (`common/src/image.rs`) survived it and is now the sidecar's
  manifest. Do not resurrect the container format.

## Architecture

Cargo workspace with crates:
- `common` — shared types, error, image/manifest structs, config
- `inventory` — disk enumeration (native CfgMgr + IOCTL on Windows; no
  PowerShell/wmic — WinPE has neither. Mock for tests)
- `snapshot` — snapshot providers (VSS on Windows, `vss_raw.rs`/`vss_sync.rs`;
  mock for tests)
- `backup` — block pipeline, used-sector maps, VHDX/qcow2 handling, checksums,
  exclusion scanners, LUKS/BitLocker
- `restore` — restore-target guards (system-disk protection)
- `metadata` — SQLite DB for backup jobs, sets, retention, activity log
- `scheduler` — cron-based job scheduling (run by the service,
  `service/src/scheduler.rs`; OS events too — logon/logoff/startup)
- `service` — Windows Service + gRPC server (tonic)
- `cli` — command-line interface (clap)
- `gui` — Tauri v2 + Svelte 5 (frontend being rebuilt per `design/` mockup:
  4 sidebar sections — Backup, Tools, Settings, Help; ru/en, light/dark themes)

## Key patterns

- **The ~180 s detach stall is GONE because the backup engine no longer
  attaches images AT ALL (2026-08-21, after four failed attempts to attach
  them "carefully").** A volume surfacing from an attached image makes
  `DetachVirtualDisk` wait out a fixed ~180 s PnP veto, and
  `NO_DRIVE_LETTER` withholds the letter, NOT the mount — so every attach
  mounts the image's filesystem and Defender/Search/VSS grab PnP handles
  before anything can be done about it. Four fixes tried to win that race
  (SAN policy, offline-attribute persist order, force-dismount, offline while
  holding locks); none could, because the race is not ours to win.
  Now: the file is CREATED by `CreateVirtualDisk` (which does not attach) and
  then written byte by byte in userspace by `vhdx_format::VhdxWriter`
  (`open_existing` → `write_at` → `finish` writes the BAT), and read by
  `vhdx_bat::VhdxGuestReader`. `AttachVirtualDisk`/`DetachVirtualDisk` are gone
  from the engine — exactly how qcow2 always worked, which is why qcow2 never
  had any of this. `tests/attach_choke_point.rs` fails the build if any path
  starts attaching images again.
  **What userspace writing takes ON, and must never lose:** the writer now owns
  the answer to "what does an untouched block read as", and it differs by
  image kind — zeros for a parentless image, THE PARENT'S DATA for a
  differencing child. Two shortcuts that are right for the first are silent
  data loss for the second: (1) skipping an all-zero write (the child would
  keep showing the parent's bytes — a region emptied on the source comes back
  from the dead on restore); (2) allocating a block for a PARTIAL write without
  seeding it (the block becomes FULLY_PRESENT, so everything not written reads
  as zeros instead of the parent). `VhdxWriter` tracks `has_parent` from the
  FileParameters flag and guards both; `tests/vhdx_partial_block.rs` fails
  without either guard. The engine's own writes are block-aligned today, so (2)
  was latent — do not rely on that staying true.
  **Do not "verify" a teardown with `all_volume_roots_on_disk`:** it filters by
  `volume_extents`, which FAILS for the volumes of an offline disk — the check
  reports "no volumes" precisely when the disk is in the state we put it in.
  That false negative is what made the second fix look correct while the stall
  continued. Use `volume_win::partition_devices_on_disk` (probes
  `\Device\HarddiskN\PartitionM`, survives offline) and log the disk's REAL
  attributes (`is_offline_read_only`) — "we set offline" is not "the disk is
  offline"; the transition is async and can be vetoed.
  Tripwires: `MOUNT-LEAK` (something surfaced, names it), `DETACH-STALL`
  (detach > 5 s), and an `image device state before detach` line every run.
  After touching anything near attach/GPT/finalize, run one INCREMENTAL (a full
  cannot reproduce this) and check `detach_ms` < 1000.
  **A volume that surfaced from an image is force-dismounted, never flushed or
  locked** (`volume_win::dismount_volume_forced`). `flush_lock_dismount_volume`
  is for REAL volumes, where discarding cached writes would be data loss; on an
  image it does the opposite of what is wanted — the first version of this fix
  used it and pushed the FAT32 driver's dirty boot sector (LBA 34/35/66) into a
  freshly written incremental, so the copy no longer matched the checksums the
  backup had just computed. Nothing Windows thought while it briefly held the
  volume belongs in the image.
- **`PAYLOAD_BLOCK_PARTIALLY_PRESENT` (BAT state 7) is per-SECTOR, and our
  reader used to get it wrong (found 2026-08-21).** State 7 means only some
  sectors of the block live in this file and the chunk's sector bitmap says
  which; the rest still come from the parent. Both readers in `vhdx_bat.rs`
  treated it as "fully present" and returned the child's zeros for the parent's
  sectors. Symptom: an image whose block 0 had been touched by a transient
  mount lost its GPT header (LBA 1) and every later read failed with
  `embedded sidecar not found (disk has no GPT)` — while the image itself was
  perfectly fine when read through Windows. Windows writes state 7; we never
  do, so it only appears after something outside the backup wrote to an image.
  `tests/vhdx_partial_block.rs` builds a synthetic parent/child pair byte by
  byte (the only way to produce state 7 on Linux) and fails if either reader
  regresses. The write path refuses a partial block outright rather than
  writing bytes that would read back from the parent.
- Windows-specific code gated with `#[cfg(target_os = "windows")]`
- All traits (`BlockReader`, `BlockWriter`, `UsedSectorMap`, `SnapshotProvider`,
  `DiskInventory`) have mock implementations for Linux testing
- Unimplemented gRPC methods must return `Status::unimplemented` — never fake success
- gRPC production transport: DACL-restricted **named pipe** `\\.\pipe\OwnBackup`
  on Windows, **UDS** on Unix — no listening TCP port. Foreground/dev uses
  loopback TCP:50051. Bearer token still required (defence in depth); no TLS
- SQLite with `bundled` feature for cross-compilation
- No email notifications (decision 2026-07-01); local notifications only
