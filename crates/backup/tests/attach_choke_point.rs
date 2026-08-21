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

const ENGINE: &str = include_str!("../src/engine_win.rs");

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
fn raw_write_attach_has_exactly_one_call_site() {
    let n = code_occurrences(ENGINE, "AttachMode::RawWrite");
    assert_eq!(
        n, 1,
        "AttachMode::RawWrite must be used ONLY inside attach_for_raw_write(), which owns \
         the offline discipline (SAN policy, dismount-what-surfaced, offline while holding \
         the locks). Found {n} uses. Call attach_for_raw_write() instead of attaching by hand."
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
    let vhdx = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/src/vhdx.rs")).unwrap();
    assert!(
        !vhdx.contains("RawRead"),
        "AttachMode::RawRead is gone on purpose — read images with \
         vhdx_bat::VhdxGuestReader instead of attaching them."
    );
    assert_eq!(
        code_occurrences(ENGINE, "AttachMode::RawRead"),
        0,
        "a read path started attaching images again"
    );
}

#[test]
fn the_write_target_closes_its_handle_before_detaching() {
    // Struct fields drop in declaration order, so the device handle must be
    // named BEFORE the attachment: an error anywhere in the copy drops the
    // target implicitly, and detaching a disk we still hold open turns a clean
    // teardown into a vetoed one.
    let dev = ENGINE.find("writer: WinRawDevice").expect("no writer field");
    let att = ENGINE.find("attached: crate::vhdx::AttachedVhdx").expect("no attached field");
    assert!(
        dev < att,
        "ImageTarget::Vhdx: the device handle must be declared before the attachment so it \
         drops first (~180 s detach otherwise)."
    );
}
