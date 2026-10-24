//! Guard: every RAW attach of an image goes through the one disciplined helper.
//!
//! The ~180 s `DetachVirtualDisk` stall came back three times, and every time
//! the hole was the same shape: a NEW place that attached an image without the
//! offline discipline (SAN policy → attach → dismount whatever surfaced →
//! offline while holding the locks). The third recurrence was a read-only
//! attach of the parent image, which only an incremental performs — invisible
//! to any test that only ever ran a full backup.
//!
//! `engine_win.rs` is Windows-only code, so nothing here can run it. What CAN
//! be checked anywhere is the invariant that made the discipline reachable in
//! the first place: RawWrite is constructed in exactly one function
//! (`attach_for_raw_write`), and images are READ without attaching at all.
//! A new raw attach elsewhere fails this test instead of costing three minutes
//! per backup and another week of bisecting.
//!
//! The engine is not one file any more: its platform-independent half (image
//! targets among it) lives in `engine_core/`, and its Windows platform layer,
//! once there is one, in `engine_platform_win.rs`. All of it is read here, so
//! moving code out of `engine_win.rs` cannot move it out of this guard's sight.

use std::path::{Path, PathBuf};

/// Every source file of the backup engine, concatenated.
fn engine_source() -> String {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files = vec![src.join("engine_win.rs")];
    let platform = src.join("engine_platform_win.rs");
    if platform.exists() {
        files.push(platform);
    }
    let core = rust_files_under(&src.join("engine_core"));
    assert!(
        !core.is_empty(),
        "no sources under src/engine_core — the guard would not see the shared engine"
    );
    files.extend(core);
    files
        .iter()
        .map(|f| std::fs::read_to_string(f).unwrap())
        .collect::<Vec<_>>()
        .join("\n")
}

fn rust_files_under(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return out;
    };
    for entry in entries {
        let path = entry.unwrap().path();
        if path.is_dir() {
            out.extend(rust_files_under(&path));
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
    out
}

/// Occurrences of `needle` outside comment lines (the doc comments discuss
/// these modes on purpose).
fn code_occurrences(haystack: &str, needle: &str) -> usize {
    haystack
        .lines()
        .filter(|l| {
            let t = l.trim_start();
            !t.starts_with("//") && !t.starts_with("*")
        })
        .map(|l| l.matches(needle).count())
        .sum()
}

#[test]
fn backup_engine_never_attaches_images_for_writing() {
    // Backup creation (both full VHDX and differencing AVHDX) is written 100%
    // in userspace using VhdxWriter, exactly like qcow2 and Linux VHDX.
    // Attaching images to the kernel during backup creation was the root cause
    // of the 180 s PnP detach stalls.
    assert_eq!(
        code_occurrences(&engine_source(), "AttachMode::RawWrite"),
        0,
        "the backup engine must NEVER attach images for writing — write with VhdxWriter in userspace"
    );
}

#[test]
fn reading_an_image_never_attaches_it() {
    // There is no raw read-only attach mode any more, and no read path may
    // reintroduce one: `NO_DRIVE_LETTER` does not stop Windows from mounting
    // the image, and a read attach has no writable handle to take the disk
    // offline with, so the volume stack survives and the detach waits out the
    // ~180 s PnP veto (measured: verify finished 15:43:20, the job "finished"
    // 15:46:20). Images are read in userspace, exactly as qcow2 always was.
    let vhdx =
        std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/src/vhdx.rs")).unwrap();
    assert!(
        !vhdx.contains("RawRead"),
        "AttachMode::RawRead is gone on purpose — read images with \
         vhdx_bat::VhdxGuestReader instead of attaching them."
    );
    assert_eq!(
        code_occurrences(&engine_source(), "AttachMode::RawRead"),
        0,
        "a read path started attaching images again"
    );
}

#[test]
fn image_target_uses_userspace_vhdx_writer() {
    // ImageTarget::Vhdx must hold VhdxWriter directly, with zero AttachedVhdx or WinRawDevice.
    assert!(
        engine_source().contains("writer: crate::vhdx_format::VhdxWriter"),
        "ImageTarget::Vhdx must use user-space VhdxWriter"
    );
}
