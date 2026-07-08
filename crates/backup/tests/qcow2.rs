//! qcow2 format module tests: in-memory roundtrips + conformance validation
//! against qemu-img (`check`, `convert`, and reading qemu-produced images).
//! qemu-img tests skip gracefully when the binary is missing so the suite
//! stays green on minimal CI images.

use std::io::Cursor;
use std::path::Path;
use std::process::Command;

use own_backup_backup::qcow2::{
    Qcow2Compression, Qcow2InPlaceMerge, Qcow2RandomReader, Qcow2Reader, Qcow2Writer,
    Qcow2WriterOptions,
};

const CLUSTER: usize = 64 * 1024;

fn opts(virtual_size: u64, compression: Qcow2Compression) -> Qcow2WriterOptions {
    Qcow2WriterOptions {
        virtual_size,
        compression,
        ..Default::default()
    }
}

/// Deterministic pseudo-random (incompressible) cluster.
fn noise_cluster(seed: u64) -> Vec<u8> {
    let mut state = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    let mut data = vec![0u8; CLUSTER];
    for b in data.iter_mut() {
        state = state.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        *b = (state >> 33) as u8;
    }
    data
}

fn text_cluster(fill: u8) -> Vec<u8> {
    vec![fill; CLUSTER]
}

/// (guest cluster index, data) pairs used by most tests: compressible,
/// incompressible, explicit zeros, and a far cluster in a second L2 table
/// (index 9000 → beyond 512 MiB requires a bigger disk, so use 8191 within
/// the first table plus rely on the 1 GiB test for multi-L2).
fn sample_clusters() -> Vec<(u64, Vec<u8>)> {
    vec![
        (0, text_cluster(0xAA)),
        (1, noise_cluster(1)),
        (5, vec![0u8; CLUSTER]),
        (100, text_cluster(0x33)),
        (127, noise_cluster(2)),
    ]
}

fn build_image(virtual_size: u64, compression: Qcow2Compression, clusters: &[(u64, Vec<u8>)]) -> Vec<u8> {
    let mut writer = Qcow2Writer::new(Cursor::new(Vec::new()), opts(virtual_size, compression)).unwrap();
    for (index, data) in clusters {
        writer.write_cluster(*index, data).unwrap();
    }
    writer.finish().unwrap().into_inner()
}

fn assert_reads_back(image: Vec<u8>, clusters: &[(u64, Vec<u8>)]) {
    let mut reader = Qcow2Reader::open(Cursor::new(image)).unwrap();
    for (index, data) in clusters {
        let read = reader.read_cluster(*index).unwrap();
        if data.iter().all(|&b| b == 0) {
            // Standalone image: zero clusters are simply not stored.
            assert!(read.is_none(), "cluster {index}: zeros should be unallocated");
        } else {
            assert_eq!(read.as_deref(), Some(data.as_slice()), "cluster {index} mismatch");
        }
    }
    // Never-written clusters read as unallocated.
    assert!(reader.read_cluster(7).unwrap().is_none());
    assert!(reader.read_cluster(126).unwrap().is_none());
}

#[test]
fn roundtrip_zstd() {
    let clusters = sample_clusters();
    let image = build_image(8 * 1024 * 1024, Qcow2Compression::Zstd { level: 3 }, &clusters);
    assert_reads_back(image, &clusters);
}

#[test]
fn roundtrip_uncompressed() {
    let clusters = sample_clusters();
    let image = build_image(8 * 1024 * 1024, Qcow2Compression::None, &clusters);
    assert_reads_back(image, &clusters);
}

#[test]
fn roundtrip_multiple_l2_tables() {
    // 1 GiB disk: one L2 table covers 512 MiB, so clusters 0 and 12288 land
    // in different L2 tables.
    let clusters = vec![
        (0u64, text_cluster(0x11)),
        (8191, noise_cluster(3)),
        (8192, text_cluster(0x22)), // first cluster of the second L2 table
        (12288, noise_cluster(4)),
    ];
    let image = build_image(1024 * 1024 * 1024, Qcow2Compression::Zstd { level: 3 }, &clusters);
    let mut reader = Qcow2Reader::open(Cursor::new(image)).unwrap();
    for (index, data) in &clusters {
        assert_eq!(reader.read_cluster(*index).unwrap().as_deref(), Some(data.as_slice()));
    }
}

#[test]
fn backing_file_child_semantics() {
    let mut writer = Qcow2Writer::new(
        Cursor::new(Vec::new()),
        Qcow2WriterOptions {
            virtual_size: 8 * 1024 * 1024,
            backing_file: Some("parent_full.qcow2".into()),
            ..Default::default()
        },
    )
    .unwrap();
    writer.write_cluster(1, &text_cluster(0x77)).unwrap();
    // In a child, an all-zero write must OVERRIDE the parent (explicit zero
    // cluster), not fall through to it.
    writer.write_cluster(2, &vec![0u8; CLUSTER]).unwrap();
    let image = writer.finish().unwrap().into_inner();

    let mut reader = Qcow2Reader::open(Cursor::new(image)).unwrap();
    assert_eq!(reader.backing_file(), Some("parent_full.qcow2"));
    assert_eq!(reader.read_cluster(1).unwrap().as_deref(), Some(text_cluster(0x77).as_slice()));
    assert_eq!(reader.read_cluster(2).unwrap(), Some(vec![0u8; CLUSTER])); // explicit zero
    assert_eq!(reader.read_cluster(3).unwrap(), None); // falls through to parent
}

#[test]
fn writer_rejects_misuse() {
    let mut writer =
        Qcow2Writer::new(Cursor::new(Vec::new()), opts(1024 * 1024, Qcow2Compression::None)).unwrap();
    assert!(writer.write_cluster(0, &[0u8; 100]).is_err(), "short cluster");
    writer.write_cluster(0, &text_cluster(1)).unwrap();
    assert!(writer.write_cluster(0, &text_cluster(2)).is_err(), "double write");
    assert!(writer.write_cluster(16, &text_cluster(1)).is_err(), "beyond disk");
}

#[test]
fn virtual_size_rounded_up_to_cluster() {
    let image = build_image(CLUSTER as u64 + 1, Qcow2Compression::None, &[]);
    let reader = Qcow2Reader::open(Cursor::new(image)).unwrap();
    assert_eq!(reader.virtual_size(), 2 * CLUSTER as u64);
}

/// Write a qcow2 file on disk with the given clusters (and optional backing
/// file name — relative, resolved against `dir`, as the engines store it).
fn write_image_file(
    dir: &Path,
    name: &str,
    virtual_size: u64,
    backing: Option<&str>,
    clusters: &[(u64, Vec<u8>)],
) -> std::path::PathBuf {
    let mut writer = Qcow2Writer::new(
        Cursor::new(Vec::new()),
        Qcow2WriterOptions {
            virtual_size,
            backing_file: backing.map(|s| s.to_string()),
            ..Default::default()
        },
    )
    .unwrap();
    for (index, data) in clusters {
        writer.write_cluster(*index, data).unwrap();
    }
    let path = dir.join(name);
    std::fs::write(&path, writer.finish().unwrap().into_inner()).unwrap();
    path
}

#[test]
fn chain_read_resolves_through_backing_files() {
    let dir = tempfile::tempdir().unwrap();
    let size = 8 * 1024 * 1024u64;

    // full: clusters 0,1,2 — inc1 overwrites 1, zeroes 2 — inc2 overwrites 0.
    write_image_file(
        dir.path(),
        "full.qcow2",
        size,
        None,
        &[(0, text_cluster(0xA0)), (1, text_cluster(0xA1)), (2, noise_cluster(9))],
    );
    write_image_file(
        dir.path(),
        "inc1.qcow2",
        size,
        Some("full.qcow2"),
        &[(1, text_cluster(0xB1)), (2, vec![0u8; CLUSTER])],
    );
    let leaf = write_image_file(
        dir.path(),
        "inc2.qcow2",
        size,
        Some("inc1.qcow2"),
        &[(0, text_cluster(0xC0))],
    );

    let mut reader = Qcow2RandomReader::open(&leaf).unwrap();
    assert_eq!(reader.chain_len(), 3);

    let mut buf = vec![0u8; CLUSTER];
    reader.read_at(0, &mut buf).unwrap(); // newest wins
    assert!(buf.iter().all(|&b| b == 0xC0));
    reader.read_at(CLUSTER as u64, &mut buf).unwrap(); // from inc1
    assert!(buf.iter().all(|&b| b == 0xB1));
    reader.read_at(2 * CLUSTER as u64, &mut buf).unwrap(); // zeroed by inc1
    assert!(buf.iter().all(|&b| b == 0));
    reader.read_at(3 * CLUSTER as u64, &mut buf).unwrap(); // nobody wrote it
    assert!(buf.iter().all(|&b| b == 0));

    // A read spanning the inc2/inc1 boundary merges layers correctly.
    let mut two = vec![0u8; 2 * CLUSTER];
    reader.read_at(0, &mut two).unwrap();
    assert!(two[..CLUSTER].iter().all(|&b| b == 0xC0));
    assert!(two[CLUSTER..].iter().all(|&b| b == 0xB1));
}

#[test]
fn chain_read_fails_clearly_on_missing_parent() {
    let dir = tempfile::tempdir().unwrap();
    let leaf = write_image_file(
        dir.path(),
        "orphan.qcow2",
        1024 * 1024,
        Some("gone.qcow2"),
        &[(0, text_cluster(1))],
    );
    let err = Qcow2RandomReader::open(&leaf).unwrap_err().to_string();
    assert!(err.contains("gone.qcow2"), "error should name the missing parent: {err}");
}

// ---------------------------------------------------------------------------
// qemu-img conformance (skipped when qemu-img is not installed)
// ---------------------------------------------------------------------------

fn qemu_img_available() -> bool {
    Command::new("qemu-img")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn qemu_supports_zstd(dir: &Path) -> bool {
    let path = dir.join("probe.qcow2");
    Command::new("qemu-img")
        .args(["create", "-f", "qcow2", "-o", "compression_type=zstd"])
        .arg(&path)
        .arg("1M")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Expected raw disk contents for a cluster list.
fn expected_raw(virtual_size: u64, clusters: &[(u64, Vec<u8>)]) -> Vec<u8> {
    let mut raw = vec![0u8; virtual_size as usize];
    for (index, data) in clusters {
        let off = *index as usize * CLUSTER;
        raw[off..off + CLUSTER].copy_from_slice(data);
    }
    raw
}

fn qemu_validate(image_path: &Path, virtual_size: u64, clusters: &[(u64, Vec<u8>)]) {
    // 1. qemu-img check: exit 0 = no errors, no leaks.
    let check = Command::new("qemu-img")
        .args(["check", "-f", "qcow2"])
        .arg(image_path)
        .output()
        .expect("run qemu-img check");
    assert!(
        check.status.success(),
        "qemu-img check failed:\n{}\n{}",
        String::from_utf8_lossy(&check.stdout),
        String::from_utf8_lossy(&check.stderr)
    );

    // 2. qemu-img convert to raw must reproduce the exact disk contents.
    let raw_path = image_path.with_extension("raw");
    let convert = Command::new("qemu-img")
        .args(["convert", "-f", "qcow2", "-O", "raw"])
        .arg(image_path)
        .arg(&raw_path)
        .output()
        .expect("run qemu-img convert");
    assert!(
        convert.status.success(),
        "qemu-img convert failed: {}",
        String::from_utf8_lossy(&convert.stderr)
    );
    let mut raw = std::fs::read(&raw_path).unwrap();
    // qemu-img may drop trailing sparse zeros on some filesystems; pad back.
    raw.resize(virtual_size as usize, 0);
    assert_eq!(raw, expected_raw(virtual_size, clusters), "raw contents differ");
}

#[test]
fn qemu_img_validates_our_images() {
    if !qemu_img_available() {
        eprintln!("qemu-img not found — skipping conformance test");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let virtual_size = 8 * 1024 * 1024;
    let clusters = sample_clusters();

    let mut variants = vec![("plain.qcow2", Qcow2Compression::None)];
    if qemu_supports_zstd(dir.path()) {
        variants.push(("zstd.qcow2", Qcow2Compression::Zstd { level: 3 }));
    } else {
        eprintln!("qemu-img built without zstd — validating uncompressed variant only");
    }

    for (name, compression) in variants {
        let image = build_image(virtual_size, compression, &clusters);
        let path = dir.path().join(name);
        std::fs::write(&path, image).unwrap();
        qemu_validate(&path, virtual_size, &clusters);
    }
}

#[test]
fn qemu_img_validates_backing_chain() {
    if !qemu_img_available() {
        eprintln!("qemu-img not found — skipping conformance test");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let virtual_size = 8 * 1024 * 1024u64;

    let parent_clusters = vec![(0u64, text_cluster(0xAA)), (1, noise_cluster(7))];
    let parent = build_image(virtual_size, Qcow2Compression::Zstd { level: 3 }, &parent_clusters);
    let parent_path = dir.path().join("parent.qcow2");
    std::fs::write(&parent_path, parent).unwrap();

    let mut writer = Qcow2Writer::new(
        Cursor::new(Vec::new()),
        Qcow2WriterOptions {
            virtual_size,
            backing_file: Some(parent_path.to_str().unwrap().into()),
            ..Default::default()
        },
    )
    .unwrap();
    writer.write_cluster(1, &text_cluster(0xBB)).unwrap(); // overwrite parent
    writer.write_cluster(0, &vec![0u8; CLUSTER]).unwrap(); // zero out parent data
    let child_path = dir.path().join("child.qcow2");
    std::fs::write(&child_path, writer.finish().unwrap().into_inner()).unwrap();

    // Chain resolved by qemu must show: cluster 0 zeroed, cluster 1 = 0xBB.
    let expected = expected_raw(virtual_size, &[(1, text_cluster(0xBB))]);
    let raw_path = dir.path().join("chain.raw");
    let convert = Command::new("qemu-img")
        .args(["convert", "-f", "qcow2", "-O", "raw"])
        .arg(&child_path)
        .arg(&raw_path)
        .output()
        .expect("run qemu-img convert");
    assert!(
        convert.status.success(),
        "qemu-img convert (chain) failed: {}",
        String::from_utf8_lossy(&convert.stderr)
    );
    let mut raw = std::fs::read(&raw_path).unwrap();
    raw.resize(virtual_size as usize, 0);
    assert_eq!(raw, expected, "chain contents differ");
}

/// The engine stores a bare file name as the backing reference — prove qemu
/// resolves it (against the image directory) exactly like our chain reader,
/// and that both produce identical flattened contents.
#[test]
fn qemu_and_our_reader_agree_on_relative_backing_chain() {
    if !qemu_img_available() {
        eprintln!("qemu-img not found — skipping conformance test");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let size = 8 * 1024 * 1024u64;

    write_image_file(
        dir.path(),
        "full.qcow2",
        size,
        None,
        &[(0, text_cluster(0xA0)), (5, noise_cluster(11))],
    );
    let child = write_image_file(
        dir.path(),
        "inc.qcow2",
        size,
        Some("full.qcow2"), // relative — the engine's convention
        &[(0, text_cluster(0xB0)), (7, text_cluster(0x07))],
    );

    let raw_path = dir.path().join("flat.raw");
    let out = Command::new("qemu-img")
        .args(["convert", "-f", "qcow2", "-O", "raw"])
        .arg(&child)
        .arg(&raw_path)
        .output()
        .expect("run qemu-img convert");
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let mut qemu_flat = std::fs::read(&raw_path).unwrap();
    qemu_flat.resize(size as usize, 0);

    let mut ours = vec![0u8; size as usize];
    Qcow2RandomReader::open(&child).unwrap().read_at(0, &mut ours).unwrap();

    assert_eq!(qemu_flat, ours, "qemu and our chain reader disagree");
    let expected =
        expected_raw(size, &[(0, text_cluster(0xB0)), (5, noise_cluster(11)), (7, text_cluster(0x07))]);
    assert_eq!(ours, expected);
}

#[test]
fn we_read_qemu_produced_images() {
    if !qemu_img_available() {
        eprintln!("qemu-img not found — skipping interop test");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let virtual_size = 8 * 1024 * 1024u64;
    let clusters = sample_clusters();

    let raw_path = dir.path().join("source.raw");
    std::fs::write(&raw_path, expected_raw(virtual_size, &clusters)).unwrap();

    // Plain qcow2 from qemu (always available).
    let mut cases = vec![(dir.path().join("qemu-plain.qcow2"), vec![
        "convert", "-f", "raw", "-O", "qcow2",
    ])];
    // zstd-compressed qcow2 from qemu, when supported.
    if qemu_supports_zstd(dir.path()) {
        cases.push((dir.path().join("qemu-zstd.qcow2"), vec![
            "convert", "-f", "raw", "-O", "qcow2", "-c", "-o", "compression_type=zstd",
        ]));
    }

    for (qcow_path, args) in cases {
        let out = Command::new("qemu-img")
            .args(&args)
            .arg(&raw_path)
            .arg(&qcow_path)
            .output()
            .expect("run qemu-img convert");
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));

        let file = std::fs::File::open(&qcow_path).unwrap();
        let mut reader = Qcow2Reader::open(std::io::BufReader::new(file)).unwrap();
        assert_eq!(reader.virtual_size(), virtual_size);
        for (index, data) in &clusters {
            let read = reader.read_cluster(*index).unwrap().unwrap_or_else(|| vec![0u8; CLUSTER]);
            assert_eq!(&read, data, "cluster {index} from {}", qcow_path.display());
        }
    }
}

// ---------------------------------------------------------------------------
// In-place merge (qcow2 NTFS/ReFS true removal folds touched blocks back in)
// ---------------------------------------------------------------------------

/// Read every expected cluster back through our own reader after a merge.
fn assert_file_reads_back(path: &Path, expected: &[(u64, Vec<u8>)]) {
    let mut reader = Qcow2Reader::open(std::fs::File::open(path).unwrap()).unwrap();
    for (index, data) in expected {
        let read = reader.read_cluster(*index).unwrap();
        if data.iter().all(|&b| b == 0) {
            assert!(read.is_none(), "cluster {index}: zeroed → unallocated");
        } else {
            assert_eq!(read.as_deref(), Some(data.as_slice()), "cluster {index} mismatch after merge");
        }
    }
}

#[test]
fn merge_in_place_overwrite_add_and_zero() {
    let dir = tempfile::tempdir().unwrap();
    let virtual_size = 8 * 1024 * 1024u64;

    let qemu = qemu_img_available();
    let zstd_ok = qemu && qemu_supports_zstd(dir.path());
    let (compression, zstd_level) = if zstd_ok {
        (Qcow2Compression::Zstd { level: 3 }, Some(3))
    } else {
        (Qcow2Compression::None, None)
    };

    // Initial image: mix of compressible, incompressible, near and far clusters.
    let initial = vec![
        (0u64, text_cluster(0xAA)),
        (1, noise_cluster(1)),
        (5, text_cluster(0x55)),
        (100, noise_cluster(2)),
        (127, text_cluster(0x77)),
    ];
    let path = dir.path().join("img.qcow2");
    std::fs::write(&path, build_image(virtual_size, compression, &initial)).unwrap();

    // Merge: overwrite a compressible + an incompressible cluster, zero one out,
    // and allocate a brand-new cluster.
    {
        let mut merge = Qcow2InPlaceMerge::open(&path, zstd_level).unwrap();
        merge.write_cluster(0, &text_cluster(0xBB)).unwrap();
        merge.write_cluster(1, &noise_cluster(42)).unwrap();
        merge.write_cluster(100, &vec![0u8; CLUSTER]).unwrap(); // deleted → zeros
        merge.write_cluster(3, &text_cluster(0x33)).unwrap();   // newly allocated
        merge.commit().unwrap();
    }

    let expected = vec![
        (0u64, text_cluster(0xBB)),
        (1, noise_cluster(42)),
        (3, text_cluster(0x33)),
        (5, text_cluster(0x55)),   // untouched — kept in place
        (100, vec![0u8; CLUSTER]), // zeroed
        (127, text_cluster(0x77)), // untouched
    ];

    assert_file_reads_back(&path, &expected);

    if qemu {
        // qemu-img check must be clean (exit 0: no corruption AND no leaked
        // clusters — dead old clusters are refcount-0 free space, not leaks).
        qemu_validate(&path, virtual_size, &expected);
    } else {
        eprintln!("qemu-img not found — skipping conformance for merge test");
    }
}

#[test]
fn merge_no_writes_is_noop() {
    let dir = tempfile::tempdir().unwrap();
    let virtual_size = 4 * 1024 * 1024u64;
    let clusters = vec![(0u64, text_cluster(0xAA)), (2, noise_cluster(5))];
    let path = dir.path().join("noop.qcow2");
    let original = build_image(virtual_size, Qcow2Compression::None, &clusters);
    std::fs::write(&path, &original).unwrap();

    Qcow2InPlaceMerge::open(&path, None).unwrap().commit().unwrap();

    // Untouched byte-for-byte, and still valid.
    assert_eq!(std::fs::read(&path).unwrap(), original, "no-write merge must not change the file");
    if qemu_img_available() {
        qemu_validate(&path, virtual_size, &clusters);
    }
}

#[test]
fn merge_spans_multiple_l2_tables_and_repeats() {
    let dir = tempfile::tempdir().unwrap();
    // 1 GiB: one L2 table covers 512 MiB → clusters 0 and 12288 sit in
    // different L2 tables, so the merge must rebuild both + the L1.
    let virtual_size = 1024 * 1024 * 1024u64;

    let qemu = qemu_img_available();
    let zstd_ok = qemu && qemu_supports_zstd(dir.path());
    let (compression, zstd_level) = if zstd_ok {
        (Qcow2Compression::Zstd { level: 3 }, Some(3))
    } else {
        (Qcow2Compression::None, None)
    };

    let initial = vec![
        (0u64, text_cluster(0x11)),
        (8192, text_cluster(0x22)), // first cluster of the 2nd L2 table
        (12288, noise_cluster(4)),
    ];
    let path = dir.path().join("big.qcow2");
    std::fs::write(&path, build_image(virtual_size, compression, &initial)).unwrap();

    // First merge: touch a cluster in each L2 table + add a new far one.
    {
        let mut m = Qcow2InPlaceMerge::open(&path, zstd_level).unwrap();
        m.write_cluster(0, &noise_cluster(100)).unwrap();
        m.write_cluster(12288, &text_cluster(0x44)).unwrap();
        m.write_cluster(9000, &text_cluster(0x99)).unwrap(); // new, 2nd table
        m.commit().unwrap();
    }
    // Second merge on the already-merged image (idempotency / re-open works).
    {
        let mut m = Qcow2InPlaceMerge::open(&path, zstd_level).unwrap();
        m.write_cluster(8192, &vec![0u8; CLUSTER]).unwrap(); // zero it out
        m.commit().unwrap();
    }

    let expected = vec![
        (0u64, noise_cluster(100)),
        (8192, vec![0u8; CLUSTER]), // zeroed by the 2nd merge
        (9000, text_cluster(0x99)),
        (12288, text_cluster(0x44)),
    ];
    assert_file_reads_back(&path, &expected);
    if qemu {
        qemu_validate(&path, virtual_size, &expected);
    }
}
