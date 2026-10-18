# Agent instructions

## Local environment & workflow rules (owner's machine)

### Language & communication
- **Always reply in Russian** to the user (all answers, status updates, summaries). Code, identifiers, paths and commit-style terms stay as they are.
- **Commit messages in this repo are in Russian** (see `git log`).
- UI text exists in Russian and English (`common::tr!`, `Paraglide`), default is Russian.

### Two checkouts and synchronization (since 2026-10-18)
The owner now works on a native Linux machine (Ubuntu 26.04, KDE Plasma on Wayland). The project lives in two folders that must be kept in sync:
1. `/home/modulator/Development/own-backup` — **the working checkout.** Edit, build, test and commit here.
2. `/run/media/modulator/WorkDisk-1/Development/own-backup` — a copy on the WorkDisk (BitLocker NTFS; under Windows this is `D:\Development\own-backup`). Keep it identical: after committing, bring its `dev` to the same commit (`git pull` there; it must stay clean) and copy the git-ignored working files (`.ai/`, `backup/`, `design/`, `tmp/`) with `rsync` without `--delete`. Do not build in the copy.
- **Sync is two-way:** another agent or a Windows session may have worked in either folder. **Before syncing**, check `git status` and `git log` on both sides so foreign work is never overwritten.
- **Line endings:** the repo is strictly LF (`.gitattributes`). Preserve LF when copying text files written under Windows (strip `\r` if needed).
- **Temporary and scratch files:** put ALL temporary files into the repo's `tmp/` (git-ignored), never in `/tmp`, `~`, or outside the project (opening files outside the project causes Zed/editor freezes on replay).

### Building and artifacts
- **Build on the Linux machine, in the working checkout:**
  - Linux checks and unit tests: `cargo test` (root-gated tests stay ignored without root).
  - Tools that are not installed system-wide (no sudo) were unpacked from Ubuntu packages into `tmp/tools/`: `. tmp/tools/env.sh` sets `PROTOC`, puts clang-cl/lld-link (for `cargo xwin`) and QEMU 10.2 with OVMF/SeaBIOS on `PATH`, and points `PKG_CONFIG_PATH` at the WebKitGTK 4.1 development `.pc` files. That is enough for `cargo check -p own-backup-gui` on Linux; linking and running the Linux GUI need the real packages (`libwebkit2gtk-4.1-dev`), which are not installed.
  - KVM is usable without sudo (`/dev/kvm` ACL). The Linux stand (`scripts/lab/linux-vm.py`) runs guests where root is available for snapshot, guard, systemd and package checks.
  - Windows cross-compilation: `cargo xwin build --target x86_64-pc-windows-msvc --release`
  - Full installer build: `scripts/build-installer.sh` (produces `target/dist/own-backup-install.exe`)
- **Built Windows `.exe` files go to** `win/` (`own-backup-gui.exe`, `own-backup-service.exe`, `own-backup-cli.exe`, `own-backup-shell.exe`, `own-backup-install.exe`).
- **Antivirus (Avast):**
  - Avoid running `cmd.exe`, `npm`, `npx`, `.bat` shims directly on Windows — build frontends in WSL.
  - If a test `.exe` needs to be run on Windows directly, run it from a subfolder of `D:\OwnBackup\` (e.g. `D:\OwnBackup\theme-test\`) — this path is in Avast exclusions. Never run newly built `.exe` from `tmp\` or `%TEMP%`.
  - Installed app: `C:\Program Files\OwnBackup\` (service runs under LocalSystem as `OwnBackup`).
  - Runtime data, jobs, activity, logs: `C:\ProgramData\OwnBackup\`.

### Machine specifics (diagnostics & logs)
- **Drive layout:**
  - Disk 2 (Samsung SSD 970 PRO 1TB): EFI, C: (Win11), Recovery, D: (Development).
  - Disk 0 (ST2000DM008 2TB HDD, SMR): F: — backup target (`F:\Backup\Disk_C_Win11`). SMR writes are slow after sustained loads; do not blame code performance without measuring on SSD.
- **When diagnosing backup failures / large increments, check event logs first:**
  - **Acronis True Image** is installed and also backs up C: (`.tibx` on F:). Simultaneous VSS snapshots can cause shadow storage exhaustion (`volsnap` event 33 → `ERROR_NOT_READY` 0x80070015).
  - **Windows Automatic Maintenance** runs defrag on the C: SSD at night, rewriting blocks and causing subsequent incrementals to balloon.
  - **Shutdowns / power events:** check System log for Event 1074 (`User32`) and Event 109 (`Kernel-Power`) before assuming a backup crash.

### Project architectural decisions (from CLAUDE.md)
- **Status & plan:** start at `.ai/PLAN.md`; the only other active status files are `.ai/DONE.md` and `.ai/CHECKS.md`.
- **True removal:** excluded files must be truly unlinked/freed from images, not just zero-filled ghosts.
- **MFT scan:** exclusions resolved before copy via `$MFT` reading (`ntfs_scan.rs`). The in-image mount-and-delete is only a fallback.
- **No detach stalls:** backup engine does NOT attach VHDX/qcow2 images during backup — images are written directly in userspace via `VhdxWriter` / userspace writers. Never re-introduce `AttachVirtualDisk` into the backup pipeline.
- **iSCSI target:** Arsenal Image Mounter (AGPL) was replaced by our in-house iSCSI target (`iscsi/`, `iscsi_win.rs`).
- **Gating:** Windows-specific code behind `#[cfg(target_os = "windows")]`; mock implementations exist for all traits on Linux.

## GitHub Actions budget policy

The owner requested minimal necessary cloud work on 2026-10-06 after an Actions-minute alert. This applies to public and private repositories; public standard runners may be free, but redundant work is still unwanted.

- Develop and run focused checks locally on `dev`. Do not restore expensive automatic builds on every `dev` push.
- `main` runs cheap checks where useful. Full validation runs on a ready (non-draft) PR, explicit manual dispatch, or as a release gate.
- Before a direct behavior change in `main` that bypasses PR validation, run the appropriate full validation manually; documentation and policy-only edits do not need a full build. Cheap checks do not prove the application passes its full tests.
- Build/install/package Windows, macOS, Android release variants and architecture matrices only for `v*` prerelease/release tags or an explicit manual request. A tag containing `-` is a prerelease; a stable tag has no suffix.
- Preserve security, race/regression, signing, license and installer checks required for release quality. Do not make required checks nonblocking to save minutes.
- Cancel obsolete validation runs by workflow/ref. Serialize publication for the same tag without cancelling an in-progress release. Set bounded job timeouts.
- Reuse Rust/Go/Gradle/npm/Docker caches. Do not add daily cache deletion; it wastes compilation time. Artifact expiry and cache eviction are different from consumed execution minutes.
- Keep short artifact retention (normally 3 days; release assets remain attached to Releases). Keep only schedules with a concrete purpose, such as fresh vulnerability data or live-provider contracts.
- Do not trigger full builds, push test tags, publish releases or rerun the entire matrix merely to check workflow edits. Use actionlint, YAML validation and event/condition review first.
- Maintain this policy when adding workflows. If a required check is introduced in branch protection, account for draft PRs and path filters so it cannot remain pending forever.

## This repository

- Main: frontend + vendor checks; full cached Windows validation on ready PR or manual dispatch.
- Release: Windows tests, dependency audits and installer checks gate publication. Do not run Linux Rust tests in GitHub Actions (owner request, 2026-10-10).
- Linux checks (tests, `cargo check`, clippy, `.deb` builds) run only locally; do not add them to GitHub Actions (owner, 2026-10-18). The free Actions minutes are exhausted until the end of October 2026: do not trigger any workflow (no push to `main`, no PR to `main`, no tags, no manual dispatch) without a new instruction. Pushing to `dev` triggers nothing.

## Keep the plan readable (owner request, 2026-10-08)

- Keep exactly three active files in `.ai/`: `PLAN.md` (remaining implementation and decisions), `DONE.md` (brief accepted results), `CHECKS.md` (unverified behavior, why, and acceptance steps). Historical reports and evidence are in `.ai/archive/`. The entire `.ai/` directory is local-only and git-ignored; keep the two local copies synchronized explicitly, without commits or pushes.
- Update existing items in place. Do not append session diaries, hashes, commands, build inventories, or repeated reports to these files. Do not create extra plans or handoffs in `.ai/` or `docs/`.
- Accepted work stays closed unless relevant code changes or a concrete regression appears. Partial coverage is not full acceptance; user-skipped tests stay skipped.
- Historical reports/evidence live in `.ai/archive/`; old local plans/logs are in ignored `backup/documentation-local-before-2026-10-08.tar.gz`. Neither is an active task list. New raw evidence and a necessary compact checkpoint belong in ignored `tmp/`, not the user-facing plan.
- Retain useful test scripts, source, vendored dependencies, design assets and current binaries. Do not call VM bases or backing images disposable without checking dependencies.

## Stand resource limits

- Before builds or VM writes, measure both WSL free space and free space on the actual Windows volume containing this distribution's `ext4.vhdx`; keep at least 30 GiB free on both after expected peak growth.
- Lab plus task work directories: at most 65 GiB; one active run including its media: 20 GiB; the single project `target/`: 30 GiB. Do not silently increase these budgets. Monitor every 25–30 seconds during long writes, and stop new stages if a limit is reached.
- One running VM. Use read-only accepted backing bases; check the entire backing chain before cleanup. Preserve unfinished failures until their evidence is extracted and verified; clean obsolete disposable attempts after that, including failed attempts.
- Do not export an entire Windows disk or NTFS partition to RAW just to read logs. Keep one source ISO and the needed current WinPE, avoid duplicate build caches and media.
- Before repeating a test, identify its still-open acceptance criterion. Finish with verified evidence, VM shutdown, temporary-data cleanup and a space measurement.
- Dense TiB testing was explicitly skipped by the owner. Do not resume without a new instruction. Never zero-fill WSL free space to compact it; use TRIM where appropriate, and do not stop WSL unexpectedly.
