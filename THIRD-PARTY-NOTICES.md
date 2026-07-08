# Third-party notices

Own Backup is licensed under the PolyForm Noncommercial License 1.0.0
(see `LICENSE`). It uses third-party components with their own licenses.

## Rust dependencies

Standard crates from crates.io (zstd, aes-gcm, argon2, xxhash-rust, windows,
tokio, serde, tonic, …) under permissive licenses (MIT / Apache-2.0 / …).
Their license terms are reproduced in each crate; run `cargo about` or
`cargo license` for the full list.

The window's page is built with Svelte and Paraglide JS (MIT) and the Tauri
JavaScript API (MIT / Apache-2.0).

### Local GTK3 compatibility fixes

`vendor/glib` is glib 0.18.5 from gtk-rs, under its original MIT license
(`vendor/glib/LICENSE`, `vendor/glib/COPYRIGHT`). Its API remains compatible
with GTK3. The only source change backports the upstream fix for
RUSTSEC-2024-0429: a mutable output pointer in `VariantStrIter::impl_get`.
The original registry checksums and exact changed-file hashes are recorded
in `vendor/glib/OWN-BACKUP-PATCH.json`; `scripts/check-vendor.py` validates
the copy and patch. It is a local backport, not a newer upstream release.

`vendor/glib-macros` 0.18.5 and `vendor/gtk3-macros` 0.18.2 retain their
original MIT licenses and GTK3 macro APIs. They replace unmaintained
`proc-macro-error` with `proc-macro-error3` 3.1.1, with the import/dependency
changes recorded in each `OWN-BACKUP-PATCH.json` and validated by the same
script. These local compatibility copies must be reviewed when the GUI
stack can move to newer upstream GTK bindings.

## Windows: mounting a backup image needs no third-party component

An image is shown to Windows as a disk by serving it from this program as an
**iSCSI target** (`crates/backup/src/iscsi/`) and letting Windows' own
initiator — the `MSiSCSI` service, part of every Windows and of WinPE — log in
to it (`crates/backup/src/iscsi_win.rs`). Both sides ship with the operating
system or are ours; nothing is downloaded, installed or redistributed.

The protocol implemented is RFC 7143 (iSCSI) and the SCSI command set (SPC/SBC),
written from the published standards.
