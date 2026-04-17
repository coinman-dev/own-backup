# My Own Backup

Open-source block-level backup system (Rust). Analogue of Acronis True Image.

## Build

```bash
cargo build          # build all crates (Linux, cross-platform only)
cargo test           # run all tests
cargo xwin build --target x86_64-pc-windows-msvc --release  # Windows cross-compile
```

## Architecture

Cargo workspace with crates:
- `common` — shared types, error, image format (.obk), config
- `inventory` — disk enumeration (WMI on Windows, mock for tests)
- `snapshot` — snapshot providers (VSS on Windows, mock for tests)
- `backup` — compression (zstd), encryption (AES-256-GCM), block pipeline, image writer/reader
- `restore` — block writer, partition table recreation, validation
- `metadata` — SQLite DB for backup jobs, sets, retention
- `scheduler` — cron-based job scheduling
- `service` — Windows Service + gRPC server (tonic)
- `cli` — command-line interface (clap)

## Key patterns

- Windows-specific code gated with `#[cfg(target_os = "windows")]`
- All traits have mock implementations for Linux testing
- Image format `.obk` is cross-platform
- gRPC on localhost:50051, no TLS
- SQLite with `bundled` feature for cross-compilation
