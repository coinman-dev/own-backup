# Own Backup — developer guidance

Read `AGENTS.md` for the local workflow, language, two-checkout synchronization,
build rules, cloud budget and stand limits.

## Current work

The only current local status is `.ai/PLAN.md`, `.ai/DONE.md`, `.ai/CHECKS.md`.
Update those in place; do not create another plan or append a session diary.
Historical local reports are in `.ai/archive/`, including
`DEVELOPMENT-NOTES-before-2026-10-08.md` for old implementation decisions.
Historical open/closed labels are not current status.

## Product and release

- Current focus (owner decision 2026-10-18): full Linux parity with the Windows
  version, shipped as a `.deb` for Ubuntu 24.04+ and Debian 13+ (amd64). Work
  happens on the owner's Linux machine. The Windows version must not regress;
  Linux trait mocks do not establish real Windows behavior (VSS, raw devices,
  filesystem drivers, DPAPI). Linux plan: `.ai/PLAN.md` section 6.
- Source-available under PolyForm Noncommercial 1.0.0; commercial use needs a
  separate license. Do not describe it as OSI open source. Preserve LICENSE,
  NOTICE, COMMERCIAL-LICENSE.md and THIRD-PARTY-NOTICES.md in distribution.
- Work, commit and push on `dev`; commit messages are Russian. `main`, tags,
  releases and installed-owner-app updates require the corresponding task.
- Build in the working checkout on the Linux machine (see AGENTS.md). Full
  Windows installer: `scripts/build-installer.sh`; Windows artifacts go to `win/`. Installer source
  `packaging/windows/own-backup.nsi` must retain its UTF-8 BOM.
- Installer controls the service through its own `/install`, `/stop`, `/start`,
  `/uninstall` commands. Do not parse localized `sc`/`taskkill` output or expose
  child-process output as installer UI. Install all-users into protected paths.
- For an authorized release, update workspace version/lock, both changelogs,
  both README versions/links and use the existing release workflow. Preserve
  release gates and the Actions budget policy in AGENTS.md.

## Storage invariants

- Supported containers: VHDX, qcow2, encrypted qcow2-based OBK. The legacy OBK
  removed in July 2026 is unrelated; do not resurrect it. New dual-header OBK
  requires a current boot medium; older readers cannot open it.
- Backup writes images directly in userspace. Never reintroduce image attach
  into the backup engine. iSCSI is our own implementation, not Arsenal/AIM.
- Exclusions mean actual unlinking/freeing plus erasure of excluded content.
  Zero-filled named entries are an incomplete fallback and must be reported.
- Read MFT only from a frozen source. Follow real data runs; never assume MFT
  records are physically contiguous. Keep the directory-walk fallback.
- Filesystem metadata changes require BOTH static checks and mount/read/write
  acceptance (`scripts/verify-mounted.sh`, shared `exclusion_corpus.rs`). Check
  multi-level tree invariants; fsck-clean alone does not prove usability.
- A differencing image inherits untouched parent data. Do not skip zero writes
  to a child or allocate partially written blocks without seeding parent bytes.
  VHDX BAT state 7 is sector-specific, not a fully present block.
- Consolidate incremental chains only, never differential siblings. Persist a
  resumable marker, repoint survivors and delete the folded image last. Version
  counts include consolidated increments. Do not infer a completed merge solely
  from DataWriteGuid: userspace writes may leave it unchanged.
- New OBK is private until the whole pipeline has finished and synced. Publish
  without replacing an existing destination. Cleanup needs authenticated ownership
  and a dead owner; unknown public orphans must not be deleted by guesswork.
- Source encryption and archive encryption have distinct secrets. BitLocker
  conversion state is a metadata flag, not encrypted_size. Unsupported ciphers
  are not wrong passwords; password errors require an authentication-tag failure.

## Windows device and access invariants

- Restore protection covers the running OS and its actual ESP, including an ESP
  on another disk. CLI flags/environment must not bypass it on an installed OS.
- Identify an iSCSI disk by device type AND exact image size; an early reported
  disk number 0 can be an uninitialized value. Logout before stopping the target
  on every failure. Release an attached disk exactly once.
- For real volumes, preserve flush/lock/dismount ordering. For unintended image
  mounts, do not flush driver cache into an image we just wrote. See historical
  notes for the detach veto and why enumerating offline volume roots is unreliable.
- Production RPC uses restricted named pipes on Windows and UDS on Unix, plus
  a token. Loopback TCP is for foreground development. Keep console-user checks,
  protected token creation, SYSTEM-only secrets and the shared destructive-operation slot.
- Jobs/activity are owned by core JSON stores; SQLite stores other metadata and
  remembered volume secrets. Do not create a second source of truth. Preserve
  atomic writes, revision checks, queue cancellation and graceful RPC shutdown.
- Reauthorization preserves the portable archive identity; refresh a queued job
  after it acquires the shared slot. Remove keys only when no jobs reference them.
- The tray is `own-backup-gui.exe --tray` in the user session, not in LocalSystem.
  Its preference is in HKCU; do not start it on the boot medium.

## UI and code conventions

- Tauri v2 + Svelte 5. Theme colours use tokens in
  `crates/gui/frontend/src/styles/variables.css`; theme is `data-theme` on html.
- All product UI strings need Russian and English. Frontend uses Paraglide;
  preserve `messages/{ru,en}.json` consistency and reactive language changes.
  Rust calls `own_backup_common::tr!`, `%{name}` parameters; default is Russian.
- Decide behavior from structured fields, never translated messages. CLI/log
  localization is not part of the current GUI language work.
- Windows code is gated by `#[cfg(target_os = "windows")]`; traits have Linux
  mocks. Unimplemented RPC returns `Status::unimplemented`, never fake success.
- Preserve dependency security fixes in `vendor/` and run focused meaningful
  checks for changed behavior. Build/compilation is not native acceptance.
