# Own Backup

Own Backup creates block-level backups of Windows volumes and whole disks. It
supports full, incremental and differential chains in VHDX and qcow2. Encrypted
archives use the current `.obk` container. A Windows service performs privileged
disk operations; the GUI and CLI connect to it locally. The source is available
under the PolyForm Noncommercial 1.0.0 license; commercial use requires a
separate license (see [LICENSE](LICENSE) and
[COMMERCIAL-LICENSE.md](COMMERCIAL-LICENSE.md)).

The product is **Windows-first**. Linux code and mock providers are used for
development and tests, but the Linux backup workflow is still deferred. Disk
restore and boot-media changes must be checked on a Windows or WinPE test
machine; a cross-compile cannot exercise VSS, DPAPI, iSCSI or raw disk I/O.

## Build and checks

Rust 1.98.1 is pinned in `rust-toolchain.toml`. The GUI frontend uses Node 24.
The Rust build scripts need `protoc`. Tauri needs the system packages in the
[Tauri prerequisites](https://v2.tauri.app/start/prerequisites/) on Linux.

```bash
cd crates/gui/frontend
npm ci
npm run build
cd ../../..
cargo test --workspace --locked
rustup target add x86_64-pc-windows-msvc
cargo xwin check --workspace --locked --target x86_64-pc-windows-msvc
```

The last command requires `cargo-xwin` and the Windows target. On a Windows
machine, `cargo check --workspace --locked --target x86_64-pc-windows-msvc`
uses the local MSVC toolchain. CI runs the Linux tests, a Windows check and the
frontend build.

## Current boundaries

- NTFS/ReFS and FAT32/exFAT exclusions have live acceptance runs. ext2/3/4
  and btrfs metadata removal have filesystem checks and mount/write tests.
- XFS true removal is incomplete for some directory/tree shapes; some excluded
  names can remain as zero-filled entries. Do not assume complete XFS removal.
- Exclusions inside an encrypted ReFS source may be skipped with an explicit
  warning. See the in-app result before relying on that rule.
- `cargo test` skips tests that need root, loop mounts, real Windows images or
  external lab fixtures. See each ignored test's description.

Verify important backups with a restore on a spare disk or VM. Keep at least
one independent copy before changing retention rules or consolidating chains.
The [Windows acceptance checklist](docs/WINDOWS-TESTS-AND-FIXES-2026-09-26.md)
records the remaining checks after the September 2026 audit fixes.
