//! What an `.obk` must do, and what it must refuse.
//!
//! The refusals are the point of stage E3: an image whose cluster table, index,
//! root or open header has been edited — or which has had a perfectly valid
//! record from an older backup of the same chain spliced into it — must not
//! answer with data. Each test below edits a real image and then reads it.

use std::io::Cursor;
use std::path::Path;

use uuid::Uuid;

use super::header::ObkOpenHeader;
use super::index::ClusterState;
use super::reader::{read_open_header, ObkChain, ObkImage};
use super::writer::{ObkCompression, ObkWriter, ObkWriterOptions};
use crate::seal::cluster::framed_len;
use crate::seal::keypair::{JobKeyPair, KdfParams, SealedPrivateKey};
use crate::seal::recovery::RecoveryKey;
use crate::seal::wrap::wrap_chain_key;
use crate::seal::ChainKey;
use crate::sidecar::ChainRole;

const CLUSTER_BITS: u32 = 12;

/// The io tests build images with the same fixture; a second copy of the key
/// setup would be a second thing to keep true.
pub(crate) fn cluster_bits() -> u32 {
    CLUSTER_BITS
}
const CLUSTER: usize = 1 << CLUSTER_BITS;
const VIRTUAL_SIZE: u64 = 64 * CLUSTER as u64;

/// Deterministic incompressible cluster: zstd cannot shrink it, so the record
/// is stored raw and two of them always have the same length — which is what
/// lets the tampering tests splice one over another.
fn noise(seed: u64) -> Vec<u8> {
    let mut state = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    let mut data = vec![0u8; CLUSTER];
    for b in data.iter_mut() {
        state = state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        *b = (state >> 33) as u8;
    }
    data
}

fn text(fill: u8) -> Vec<u8> {
    vec![fill; CLUSTER]
}

/// One job, one chain, one portable envelope — reused by every image a test
/// builds, exactly as a real chain reuses them.
pub(crate) struct Fixture {
    pub(crate) chain: ChainKey,
    chain_id: Uuid,
    pair: JobKeyPair,
    recovery: RecoveryKey,
    key_file: SealedPrivateKey,
}

impl Fixture {
    pub(crate) fn new() -> Self {
        let chain = ChainKey::generate().unwrap();
        let pair = JobKeyPair::generate().unwrap();
        let recovery = RecoveryKey::generate().unwrap();
        let key_file = SealedPrivateKey::seal(
            "job-under-test",
            &pair,
            "пароль архива",
            &recovery,
            &KdfParams::test_profile(),
        )
        .unwrap();
        Self {
            chain,
            chain_id: Uuid::new_v4(),
            pair,
            recovery,
            key_file,
        }
    }

    pub(crate) fn options(
        &self,
        backup_id: Uuid,
        role: ChainRole,
        parent: Option<(Uuid, &str)>,
    ) -> ObkWriterOptions {
        ObkWriterOptions {
            chain_id: self.chain_id,
            backup_id,
            parent_backup_id: parent.map(|(id, _)| id),
            role,
            created_at: "2026-09-21T12:00:00Z".into(),
            virtual_size: VIRTUAL_SIZE,
            cluster_bits: CLUSTER_BITS,
            compression: ObkCompression::Zstd { level: 3 },
            backing_file: parent.map(|(_, name)| name.to_string()),
            key_file: self.key_file.clone(),
            wrapped_chain_key: wrap_chain_key(&self.pair.public, self.chain_id.as_bytes(), &self.chain)
                .unwrap(),
            source_id: Some("volume-under-test".into()),
            consolidated_increments: 0,
            chain_started_at: None,
        }
    }

    fn build(&self, opts: ObkWriterOptions, clusters: &[(u64, Vec<u8>)], metadata: &[u8]) -> Vec<u8> {
        let mut writer = ObkWriter::new(Cursor::new(Vec::new()), &self.chain, opts).unwrap();
        for (index, data) in clusters {
            writer.write_cluster(*index, data).unwrap();
        }
        writer.finish(metadata).unwrap().into_inner()
    }

    fn full(&self, clusters: &[(u64, Vec<u8>)], metadata: &[u8]) -> (Uuid, Vec<u8>) {
        let id = Uuid::new_v4();
        (id, self.build(self.options(id, ChainRole::Full, None), clusters, metadata))
    }

    fn open(&self, image: &[u8]) -> ObkImage<Cursor<Vec<u8>>> {
        ObkImage::open(Cursor::new(image.to_vec()), &self.chain).unwrap()
    }

    fn open_err(&self, image: &[u8]) -> String {
        match ObkImage::open(Cursor::new(image.to_vec()), &self.chain) {
            Ok(_) => String::new(),
            Err(e) => e.to_string(),
        }
    }

    fn write_file(&self, dir: &Path, name: &str, image: &[u8]) {
        std::fs::write(dir.join(name), image).unwrap();
    }
}

/// Offsets and lengths of the records of one purpose, found by their framing.
/// Ciphertext can contain the magic by chance, so every candidate is checked
/// against the chain identity and its own length before being believed.
fn records(image: &[u8], chain_id: &Uuid, purpose: u8) -> Vec<(usize, usize, u64)> {
    let mut found = Vec::new();
    for at in 0..image.len().saturating_sub(84) {
        if &image[at..at + 4] != b"OBKR"
            || u16::from_be_bytes([image[at + 4], image[at + 5]]) != 2
            || image[at + 6] != purpose
            || &image[at + 8..at + 24] != chain_id.as_bytes()
        {
            continue;
        }
        let Ok(len) = framed_len(&image[at..]) else {
            continue;
        };
        if at + len > image.len() {
            continue;
        }
        let index = u64::from_be_bytes(image[at + 24..at + 32].try_into().unwrap());
        found.push((at, len, index));
    }
    found
}

fn record_at(image: &[u8], chain_id: &Uuid, purpose: u8, index: u64) -> (usize, usize) {
    let hit = records(image, chain_id, purpose)
        .into_iter()
        .find(|(_, _, i)| *i == index)
        .unwrap_or_else(|| panic!("no record of purpose {purpose} for index {index}"));
    (hit.0, hit.1)
}

/// Byte offset of a guest cluster's L2 entry, walked exactly as a reader would.
fn l2_entry_offset(image: &[u8], cluster: u64) -> usize {
    let cluster_bits = u32::from_be_bytes(image[20..24].try_into().unwrap());
    let cluster_size = 1u64 << cluster_bits;
    let l1_offset = u64::from_be_bytes(image[40..48].try_into().unwrap());
    let per_table = cluster_size / 8;
    let l1_index = cluster / per_table;
    let at = (l1_offset + l1_index * 8) as usize;
    let l1_entry = u64::from_be_bytes(image[at..at + 8].try_into().unwrap());
    let l2_offset = l1_entry & crate::qcow2::OFFSET_MASK;
    (l2_offset + (cluster % per_table) * 8) as usize
}

#[test]
fn every_kind_of_cluster_comes_back_out_of_a_full_image() {
    let f = Fixture::new();
    let clusters = vec![
        (0, text(0xAA)),          // compresses
        (1, noise(1)),            // does not
        (5, vec![0u8; CLUSTER]),  // all zeros: not stored at all
        (63, noise(2)),           // the last cluster of the disk
    ];
    let metadata = b"sealed sidecar goes here".to_vec();
    let (_, image) = f.full(&clusters, &metadata);

    let mut reader = f.open(&image);
    assert_eq!(reader.cluster_size(), CLUSTER as u64);
    assert_eq!(reader.total_clusters(), 64);
    assert_eq!(&reader.sealed_metadata().unwrap()[..], &metadata[..]);

    for (index, data) in &clusters {
        let read = reader.read_cluster(*index).unwrap();
        if data.iter().all(|&b| b == 0) {
            // A full backup keeps zeros out of the image: absent reads as zeros.
            assert!(read.is_none(), "cluster {index}");
            assert_eq!(reader.cluster_state(*index).unwrap(), ClusterState::Absent);
        } else {
            assert_eq!(&read.unwrap()[..], &data[..], "cluster {index}");
        }
    }
    // Never-written clusters are absent, and a read past the disk is an error.
    assert!(reader.read_cluster(7).unwrap().is_none());
    assert!(reader.read_cluster(64).is_err());
    // Only clusters that carry data cost a record.
    assert_eq!(records(&image, &f.chain_id, 1).len(), 3);
}

#[test]
fn an_image_without_compression_also_round_trips() {
    let f = Fixture::new();
    let id = Uuid::new_v4();
    let mut opts = f.options(id, ChainRole::Full, None);
    opts.compression = ObkCompression::None;
    let image = f.build(opts, &[(2, text(0x11)), (3, noise(9))], b"");
    let mut reader = f.open(&image);
    assert_eq!(&reader.read_cluster(2).unwrap().unwrap()[..], &text(0x11)[..]);
    assert_eq!(&reader.read_cluster(3).unwrap().unwrap()[..], &noise(9)[..]);
    assert_eq!(&reader.sealed_metadata().unwrap()[..], b"");
}

#[test]
fn foreign_readers_refuse_the_file_instead_of_reading_ciphertext() {
    let f = Fixture::new();
    let (_, image) = f.full(&[(0, noise(1))], b"meta");
    let error = crate::qcow2::Qcow2Reader::open(Cursor::new(image.clone()))
        .err()
        .expect("a plaintext qcow2 reader must refuse an encrypted image")
        .to_string();
    assert!(
        error.contains("unknown incompatible features"),
        "refused for the wrong reason: {error}"
    );
    // The feature name table tells a foreign reader what the bit is.
    assert!(
        image
            .windows(super::OBK_FEATURE_NAME.len())
            .any(|w| w == super::OBK_FEATURE_NAME.as_bytes()),
        "the image does not name its own feature"
    );
}

#[test]
fn the_open_header_lists_a_backup_without_a_key_and_promises_nothing() {
    let f = Fixture::new();
    let (backup_id, image) = f.full(&[(0, noise(1))], b"meta");
    let dir = tempfile::tempdir().unwrap();
    f.write_file(dir.path(), "full.obk", &image);

    let open = read_open_header(&dir.path().join("full.obk")).unwrap();
    assert_eq!(open.backup_id, backup_id);
    assert_eq!(open.chain_id, f.chain_id);
    assert_eq!(open.role, ChainRole::Full);
    assert_eq!(open.parent_backup_id, None);
    // Everything needed to open the image elsewhere travels inside it.
    assert_eq!(
        open.chain_key_with_password("пароль архива").unwrap().as_bytes(),
        f.chain.as_bytes()
    );
    assert_eq!(
        open.chain_key_with_recovery(&f.recovery).unwrap().as_bytes(),
        f.chain.as_bytes()
    );
    assert!(open.chain_key_with_password("не тот пароль").is_err());
    // The local service key store is not in the image, only in the service.
    let json = serde_json::to_string(&open).unwrap();
    assert!(!json.contains("dpapi") && !json.contains("OBKDP"));
}

#[test]
fn a_chain_resolves_parents_and_explicit_zeros_override_them() {
    let f = Fixture::new();
    let dir = tempfile::tempdir().unwrap();

    let base = vec![(0, text(0xAA)), (1, noise(1)), (2, noise(2))];
    let (full_id, full) = f.full(&base, b"full metadata");
    f.write_file(dir.path(), "full.obk", &full);

    // The increment changes cluster 1, zeroes cluster 2 and leaves 0 alone.
    let inc_id = Uuid::new_v4();
    let inc = f.build(
        f.options(inc_id, ChainRole::Incremental, Some((full_id, "full.obk"))),
        &[(1, noise(42)), (2, vec![0u8; CLUSTER])],
        b"increment metadata",
    );
    f.write_file(dir.path(), "inc.obk", &inc);

    let mut chain = ObkChain::open(&dir.path().join("inc.obk"), &f.chain).unwrap();
    assert_eq!(chain.chain_len(), 2);
    assert_eq!(chain.virtual_size(), VIRTUAL_SIZE);
    assert_eq!(&chain.read_cluster(0).unwrap()[..], &text(0xAA)[..], "inherited");
    assert_eq!(&chain.read_cluster(1).unwrap()[..], &noise(42)[..], "overwritten");
    assert_eq!(&chain.read_cluster(2).unwrap()[..], &vec![0u8; CLUSTER][..], "zeroed");
    assert_eq!(&chain.read_cluster(9).unwrap()[..], &vec![0u8; CLUSTER][..], "never written");
    assert_eq!(&chain.sealed_metadata().unwrap()[..], b"increment metadata");

    // A region emptied on the source must not come back from the parent.
    let mut leaf = f.open(&inc);
    assert_eq!(leaf.cluster_state(2).unwrap(), ClusterState::Zero);
    assert_eq!(leaf.cluster_state(0).unwrap(), ClusterState::Absent);

    // A differential sits on the full image, not on the increment.
    let diff = f.build(
        f.options(Uuid::new_v4(), ChainRole::Differential, Some((full_id, "full.obk"))),
        &[(0, noise(7))],
        b"differential metadata",
    );
    f.write_file(dir.path(), "diff.obk", &diff);
    let mut chain = ObkChain::open(&dir.path().join("diff.obk"), &f.chain).unwrap();
    assert_eq!(&chain.read_cluster(0).unwrap()[..], &noise(7)[..]);
    assert_eq!(&chain.read_cluster(1).unwrap()[..], &noise(1)[..], "from the full backup");

    // Reading part of a cluster, across a boundary, gives the same bytes.
    let mut spanning = vec![0u8; CLUSTER];
    chain.read_at(CLUSTER as u64 / 2 * 3, &mut spanning).unwrap();
    let mut expected = noise(1)[CLUSTER / 2..].to_vec();
    expected.extend_from_slice(&noise(2)[..CLUSTER / 2]);
    assert_eq!(spanning, expected);
}

#[test]
fn a_chain_refuses_a_parent_that_is_not_the_one_the_image_was_written_against() {
    let f = Fixture::new();
    let dir = tempfile::tempdir().unwrap();
    let (full_id, full) = f.full(&[(0, noise(1))], b"full");
    f.write_file(dir.path(), "full.obk", &full);

    // Another full backup of the same chain, put where the parent should be.
    let (_, other) = f.full(&[(0, noise(2))], b"other full");
    let inc = f.build(
        f.options(Uuid::new_v4(), ChainRole::Incremental, Some((full_id, "full.obk"))),
        &[(1, noise(3))],
        b"inc",
    );
    f.write_file(dir.path(), "inc.obk", &inc);
    f.write_file(dir.path(), "full.obk", &other);
    let Err(error) = ObkChain::open(&dir.path().join("inc.obk"), &f.chain) else {
        panic!("a substituted parent must be refused");
    };
    let error = error.to_string();
    assert!(error.contains("not the parent"), "unexpected: {error}");

    // An image of another chain cannot be grafted in either: its own chain key
    // is different, so it does not even open.
    let stranger = Fixture::new();
    let (_, foreign) = stranger.full(&[(0, noise(4))], b"foreign");
    f.write_file(dir.path(), "full.obk", &foreign);
    assert!(ObkChain::open(&dir.path().join("inc.obk"), &f.chain).is_err());

    // A chain that points at itself is named a loop, not a deep chain.
    let self_id = Uuid::new_v4();
    let loop_image = f.build(
        f.options(self_id, ChainRole::Incremental, Some((full_id, "loop.obk"))),
        &[(1, noise(5))],
        b"loop",
    );
    f.write_file(dir.path(), "loop.obk", &loop_image);
    let Err(error) = ObkChain::open(&dir.path().join("loop.obk"), &f.chain) else {
        panic!("a chain that points at itself must be refused");
    };
    let error = error.to_string();
    assert!(error.contains("loop"), "unexpected: {error}");
}

#[test]
fn another_key_opens_nothing_and_says_so_without_returning_bytes() {
    let f = Fixture::new();
    let (_, image) = f.full(&[(0, noise(1))], b"meta");
    let stranger = ChainKey::generate().unwrap();
    let Err(error) = ObkImage::open(Cursor::new(image.clone()), &stranger) else {
        panic!("a wrong chain key must not open an image");
    };
    let error = error.to_string();
    assert!(error.contains("authentication failed"), "unexpected: {error}");
}

#[test]
fn flipping_any_byte_of_a_data_record_is_caught_before_the_data_is_returned() {
    let f = Fixture::new();
    let (_, image) = f.full(&[(4, noise(1))], b"meta");
    let (at, len) = record_at(&image, &f.chain_id, 1, 4);
    for offset in [0, 7, 40, 84, len / 2, len - 1] {
        let mut broken = image.clone();
        broken[at + offset] ^= 1;
        let mut reader = f.open(&broken);
        assert!(
            reader.read_cluster(4).is_err(),
            "byte {offset} of the record went unnoticed"
        );
    }
    // Truncation must never quietly change what the image reads back. Cutting
    // the refcount tail leaves a readable image — a read-only reader does not
    // use refcounts — so the rule is not "it must fail" but "it must not lie".
    for cut in [image.len() - CLUSTER, image.len() / 2, CLUSTER * 3] {
        let Ok(mut reader) = ObkImage::open(Cursor::new(image[..cut].to_vec()), &f.chain) else {
            continue;
        };
        if let Ok(read) = reader.read_cluster(4) {
            assert_eq!(
                read.map(|data| data.to_vec()),
                Some(noise(1)),
                "truncating to {cut} bytes changed what cluster 4 reads"
            );
        }
    }
}

#[test]
fn an_older_valid_record_of_the_same_cluster_is_refused() {
    let f = Fixture::new();
    // Two backups of the same chain, both storing cluster 4, both raw and so
    // the same length: a byte-for-byte splice is possible.
    let (_, old) = f.full(&[(4, noise(1))], b"old");
    let (_, new) = f.full(&[(4, noise(2))], b"new");
    let (old_at, old_len) = record_at(&old, &f.chain_id, 1, 4);
    let (new_at, new_len) = record_at(&new, &f.chain_id, 1, 4);
    assert_eq!(old_len, new_len, "both records are raw and one cluster long");

    let mut spliced = new.clone();
    spliced[new_at..new_at + new_len].copy_from_slice(&old[old_at..old_at + old_len]);
    let mut reader = f.open(&spliced);
    let error = reader.read_cluster(4).expect_err("replay must be refused").to_string();
    assert!(
        error.contains("does not match its authenticated index"),
        "unexpected: {error}"
    );
}

#[test]
fn two_records_of_one_image_cannot_change_places() {
    let f = Fixture::new();
    let (_, image) = f.full(&[(4, noise(1)), (5, noise(2))], b"meta");
    let (a, len) = record_at(&image, &f.chain_id, 1, 4);
    let (b, other_len) = record_at(&image, &f.chain_id, 1, 5);
    assert_eq!(len, other_len);

    let mut swapped = image.clone();
    swapped[a..a + len].copy_from_slice(&image[b..b + len]);
    swapped[b..b + len].copy_from_slice(&image[a..a + len]);
    let mut reader = f.open(&swapped);
    assert!(reader.read_cluster(4).is_err(), "cluster 4 read a moved record");
    assert!(reader.read_cluster(5).is_err(), "cluster 5 read a moved record");
}

#[test]
fn the_cluster_table_cannot_be_pointed_at_another_record() {
    let f = Fixture::new();
    let (_, image) = f.full(&[(4, noise(1)), (5, noise(2))], b"meta");
    let mut edited = image.clone();
    let from = l2_entry_offset(&image, 5);
    let to = l2_entry_offset(&image, 4);
    let entry = image[from..from + 8].to_vec();
    edited[to..to + 8].copy_from_slice(&entry);

    let mut reader = f.open(&edited);
    let error = reader.read_cluster(4).expect_err("a moved pointer must be refused");
    assert!(error.to_string().contains("does not match its authenticated index"));

    // Dropping the entry entirely is a disagreement with the index, not a hole.
    let mut cleared = image.clone();
    cleared[to..to + 8].fill(0);
    let mut reader = f.open(&cleared);
    let error = reader.read_cluster(4).expect_err("a dropped pointer must be refused");
    assert!(error.to_string().contains("disagree"), "unexpected: {error}");

    // And an entry invented for a cluster the index does not list is refused
    // rather than silently believed.
    let mut invented = image.clone();
    let unused = l2_entry_offset(&image, 6);
    invented[unused..unused + 8].copy_from_slice(&entry);
    let mut reader = f.open(&invented);
    assert!(reader.read_cluster(6).is_err());
}

#[test]
fn the_index_and_the_root_cannot_be_replaced_by_valid_records_of_another_backup() {
    let f = Fixture::new();
    let (_, old) = f.full(&[(4, noise(1))], b"old");
    let (_, new) = f.full(&[(4, noise(2))], b"new");

    // The index chunk of another backup: same purpose, same ordinal, same
    // chain — and refused, because the root names one exact record.
    let (old_at, old_len) = record_at(&old, &f.chain_id, 3, 0);
    let (new_at, new_len) = record_at(&new, &f.chain_id, 3, 0);
    if old_len == new_len {
        let mut spliced = new.clone();
        spliced[new_at..new_at + new_len].copy_from_slice(&old[old_at..old_at + old_len]);
        let mut reader = f.open(&spliced);
        assert!(reader.read_cluster(4).is_err(), "a foreign index chunk was believed");
    }

    // The root record of another backup, likewise.
    let (old_root, old_root_len) = record_at(&old, &f.chain_id, 2, 0);
    let (new_root, new_root_len) = record_at(&new, &f.chain_id, 2, 0);
    if old_root_len == new_root_len {
        let mut spliced = new.clone();
        spliced[new_root..new_root + new_root_len]
            .copy_from_slice(&old[old_root..old_root + old_root_len]);
        assert!(
            !f.open_err(&spliced).is_empty(),
            "a foreign root record was believed"
        );
    }

    // Tampering with the sealed metadata is caught when it is read.
    let (meta_at, _) = record_at(&new, &f.chain_id, 2, 1);
    let mut broken = new.clone();
    broken[meta_at + 90] ^= 1;
    let mut reader = f.open(&broken);
    assert!(reader.sealed_metadata().is_err());
}

#[test]
fn the_open_header_cannot_quietly_relabel_an_image() {
    let f = Fixture::new();
    let (backup_id, image) = f.full(&[(0, noise(1))], b"meta");

    // Identities are in the open header for listings and in the sealed root for
    // real. Editing one of them makes the image refuse to open.
    let json = serde_json::to_string(&Uuid::new_v4()).unwrap();
    let old = serde_json::to_string(&backup_id).unwrap();
    let at = image
        .windows(old.len())
        .position(|w| w == old.as_bytes())
        .expect("the open header carries the backup id");
    let mut relabelled = image.clone();
    relabelled[at..at + json.len()].copy_from_slice(json.as_bytes());
    let error = f.open_err(&relabelled);
    assert!(error.contains("disagree"), "unexpected: {error}");

    // Pointing the root pointer somewhere else fails too: the digits of the
    // offset are changed in place, so the JSON stays the same length.
    let open: ObkOpenHeader = read_open_header_from_bytes(&image);
    assert!(open.root.offset > 0);
    let needle = b"\"offset\":";
    let at = image
        .windows(needle.len())
        .position(|w| w == needle)
        .expect("the open header carries the root offset")
        + needle.len();
    let mut moved = image.clone();
    let digits = image[at..].iter().take_while(|b| b.is_ascii_digit()).count();
    assert!(digits > 0, "the root offset is a number");
    moved[at] = if image[at] == b'1' { b'2' } else { b'1' };
    assert!(!f.open_err(&moved).is_empty());
}

fn read_open_header_from_bytes(image: &[u8]) -> ObkOpenHeader {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("image.obk");
    std::fs::write(&path, image).unwrap();
    read_open_header(&path).unwrap()
}

#[test]
fn the_writer_refuses_what_would_produce_an_unreadable_image() {
    let f = Fixture::new();
    let id = Uuid::new_v4();

    let mut too_small = f.options(id, ChainRole::Full, None);
    too_small.cluster_bits = 9;
    assert!(ObkWriter::new(Cursor::new(Vec::new()), &f.chain, too_small).is_err());

    let mut parent_without_file = f.options(id, ChainRole::Incremental, None);
    parent_without_file.parent_backup_id = Some(Uuid::new_v4());
    assert!(ObkWriter::new(Cursor::new(Vec::new()), &f.chain, parent_without_file).is_err());

    let mut file_without_parent = f.options(id, ChainRole::Full, None);
    file_without_parent.backing_file = Some("full.obk".into());
    assert!(ObkWriter::new(Cursor::new(Vec::new()), &f.chain, file_without_parent).is_err());

    let mut other_chain = f.options(id, ChainRole::Full, None);
    other_chain.wrapped_chain_key =
        wrap_chain_key(&f.pair.public, Uuid::new_v4().as_bytes(), &f.chain).unwrap();
    assert!(ObkWriter::new(Cursor::new(Vec::new()), &f.chain, other_chain).is_err());

    let mut writer = ObkWriter::new(
        Cursor::new(Vec::new()),
        &f.chain,
        f.options(id, ChainRole::Full, None),
    )
    .unwrap();
    writer.write_cluster(0, &noise(1)).unwrap();
    assert!(writer.write_cluster(0, &noise(2)).is_err(), "write-once");
    assert!(writer.write_cluster(1, &noise(1)[..10]).is_err(), "partial cluster");
    assert!(writer.write_cluster(64, &noise(1)).is_err(), "past the disk");
}

#[test]
fn a_zero_cluster_costs_no_record_and_a_child_still_overrides_its_parent() {
    let f = Fixture::new();
    let (full_id, _) = f.full(&[(0, noise(1))], b"full");
    let mut writer = ObkWriter::new(
        Cursor::new(Vec::new()),
        &f.chain,
        f.options(Uuid::new_v4(), ChainRole::Incremental, Some((full_id, "full.obk"))),
    )
    .unwrap();
    writer.write_zero_cluster(3).unwrap();
    writer.write_cluster(4, &vec![0u8; CLUSTER]).unwrap();
    let image = writer.finish(b"child").unwrap().into_inner();

    assert!(records(&image, &f.chain_id, 1).is_empty(), "zeros cost no record");
    let mut reader = f.open(&image);
    assert_eq!(reader.cluster_state(3).unwrap(), ClusterState::Zero);
    assert_eq!(reader.cluster_state(4).unwrap(), ClusterState::Zero);
    assert_eq!(
        &reader.read_cluster(3).unwrap().unwrap()[..],
        &vec![0u8; CLUSTER][..]
    );
}

#[test]
fn an_image_spanning_several_l2_tables_reads_back() {
    // Clusters on both sides of an L2 table boundary, so the reader has to
    // pick the right table as well as the right index entry.
    let f = Fixture::new();
    let per_table = (CLUSTER / 8) as u64; // 512 clusters per L2 table
    let id = Uuid::new_v4();
    let mut opts = f.options(id, ChainRole::Full, None);
    opts.virtual_size = (per_table + 8) * CLUSTER as u64;
    let clusters: Vec<(u64, Vec<u8>)> = [0u64, 1, per_table - 1, per_table, per_table + 7]
        .iter()
        .map(|&c| (c, text((c % 251) as u8 | 1)))
        .collect();
    let image = f.build(opts, &clusters, b"big");

    let mut reader = f.open(&image);
    for (index, data) in &clusters {
        assert_eq!(
            &reader.read_cluster(*index).unwrap().unwrap()[..],
            &data[..],
            "cluster {index}"
        );
    }
    assert!(reader.read_cluster(per_table + 1).unwrap().is_none());
}
