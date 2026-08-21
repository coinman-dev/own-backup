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
//! (`attach_for_raw_write`) and RawRead in exactly one (`ImageReadSource::open`).
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
fn raw_read_attach_has_exactly_one_call_site() {
    let n = code_occurrences(ENGINE, "AttachMode::RawRead");
    assert_eq!(
        n, 1,
        "AttachMode::RawRead must be used ONLY inside ImageReadSource::open(), which applies \
         the same offline discipline. A read attach mounts the image's filesystem just like a \
         write attach does — that is what made every INCREMENTAL pay a ~180 s detach. Found {n}."
    );
}

#[test]
fn image_handles_close_before_the_disk_detaches() {
    // Struct fields drop in declaration order. Both image types must name their
    // device handle BEFORE the attachment, so an error path that drops them
    // implicitly still closes the handle before DetachVirtualDisk runs.
    for (kind, dev_field, attach_field) in [
        ("ImageReadSource::Vhdx", "dev: WinRawDevice", "_attached: crate::vhdx::AttachedVhdx"),
        ("ImageTarget::Vhdx", "writer: WinRawDevice", "attached: crate::vhdx::AttachedVhdx"),
    ] {
        let dev = ENGINE.find(dev_field).unwrap_or_else(|| panic!("{kind}: no field {dev_field}"));
        let att = ENGINE
            .find(attach_field)
            .unwrap_or_else(|| panic!("{kind}: no field {attach_field}"));
        assert!(
            dev < att,
            "{kind}: the device handle must be declared before the attachment so it drops \
             first — detaching a disk we still hold open turns a clean teardown into a \
             vetoed one (~180 s)."
        );
    }
}
