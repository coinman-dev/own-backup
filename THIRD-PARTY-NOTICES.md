# Third-party notices

Own Backup is licensed under the PolyForm Noncommercial License 1.0.0
(see `LICENSE`). It uses third-party components with their own licenses.

## Rust dependencies

Standard crates from crates.io (zstd, aes-gcm, argon2, xxhash-rust, windows,
tokio, serde, tonic, …) under permissive licenses (MIT / Apache-2.0 / …), and
a few under MPL-2.0, BSL-1.0, Unicode-3.0 and Zlib. Their full license texts —
each crate's own files, with its name, version and source address — are in
`THIRD-PARTY-LICENSES.txt` beside the program, generated for every build by
`scripts/third-party-licenses.py`. The MPL-2.0 components are unmodified; their
source is at the addresses listed there.

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

### Local binrw compatibility fix

`vendor/binrw` is binrw 0.11.3 (MIT), which `ntfs` 0.4.0 — its latest
release — needs. Two lines are changed so it keeps compiling on future Rust
releases: the `default_reader` helper is written out with the signature its
`#[binrw::parser]` attribute generates, and `extern crate self as binrw` is
made `pub`. Its own warnings are silenced as in the GTK copies. The registry
checksum and the changed-file hashes are in `vendor/binrw/OWN-BACKUP-PATCH.json`,
checked by `scripts/check-vendor.py`.

## Linux: system libraries and the boot medium

The `.deb` links against libraries of the system it is installed on — GTK 3,
WebKitGTK, gtk-layer-shell (the boot medium's strip), libayatana-appindicator
and others named in its `Depends` — which the package manager installs under
their own licenses; the package does not carry them.

The Linux boot medium is a Debian 13 live system built from unmodified Debian
packages (the kernel, systemd, labwc, cage, foot, Mesa, firmware and the rest
of its package list), each under its own license, with its copyright file in
`/usr/share/doc/<package>/copyright` on the medium and its source in the
Debian archive.

## Windows: mounting a backup image needs no third-party component

An image is shown to Windows as a disk by serving it from this program as an
**iSCSI target** (`crates/backup/src/iscsi/`) and letting Windows' own
initiator — the `MSiSCSI` service, part of every Windows and of WinPE — log in
to it (`crates/backup/src/iscsi_win.rs`). Both sides ship with the operating
system or are ours; nothing is downloaded, installed or redistributed.

The protocol implemented is RFC 7143 (iSCSI) and the SCSI command set (SPC/SBC),
written from the published standards.
