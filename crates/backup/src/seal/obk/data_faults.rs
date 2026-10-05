//! Interrupt real data preparation before any new generation is published.
use std::io::{BufWriter, Write};
use std::path::Path;

use sha2::{Digest, Sha256};
use uuid::Uuid;

use super::container_tests::{cluster_bits, Fixture};
use super::publication_faults::{observe, Point};
use super::{
    apply_changed_with, compact_image_with_progress, ObkChain, ObkImage, ObkWriter,
    PreviousContents, RebuildOptions,
};
use crate::seal::ChainKey;
use crate::sidecar::ChainRole;

const OLD: &[u8] = b"data=old";
const NEW: &[u8] = b"data=new";
const CASES: &[&str] = &[
    "keep-first-record",
    "legacy-keep-first-record",
    "compact-copy-in-progress",
    "compact-readback-in-progress",
    "write-index",
    "write-metadata",
    "write-root",
    "write-l2",
    "write-l1",
    "write-refcount-block",
    "write-refcount-table",
    "legacy-write-refcount-table",
];
fn legacy(name: &str) -> bool {
    name.starts_with("legacy-")
}
fn compaction(name: &str) -> bool {
    name.starts_with("compact-")
}
fn hash(path: &Path) -> String {
    format!("{:x}", Sha256::digest(std::fs::read(path).unwrap()))
}
fn bytes(cluster: u64, updated: bool) -> Vec<u8> {
    let value = match (cluster, updated) {
        (0, _) | (4, true) => 0,
        (1, _) => 0xee,
        (2, true) => 0xa2,
        (6, true) => 0xa6,
        _ => cluster as u8 + 0x20,
    };
    vec![value; 1 << cluster_bits()]
}
pub(super) fn digest(path: &Path, key: &ChainKey, updated: bool) -> String {
    let mut chain = ObkChain::open(path, key).unwrap();
    let mut digest = Sha256::new();
    for cluster in 0..64 {
        let actual = chain.read_cluster(cluster).unwrap();
        assert_eq!(&actual[..], &bytes(cluster, updated));
        digest.update(&actual[..]);
    }
    format!("{:x}", digest.finalize())
}
fn rebuild(base: &Path, key: &ChainKey) {
    apply_changed_with(
        base,
        key,
        &[2, 4, 6],
        |n| Ok(bytes(n, true)),
        RebuildOptions {
            previous: PreviousContents::Keep,
            metadata: Some(NEW),
            ..Default::default()
        },
    )
    .unwrap();
}
pub(super) fn prepare(root: &Path, name: &str) -> Fixture {
    let f = Fixture::new();
    let id = Uuid::new_v4();
    for (file, role, parent) in [
        ("base.obk", ChainRole::Full, None),
        ("leaf.obk", ChainRole::Incremental, Some((id, "base.obk"))),
    ] {
        let opts = f.options(
            if parent.is_none() { id } else { Uuid::new_v4() },
            role,
            parent,
        );
        let file = std::fs::File::create_new(root.join(file)).unwrap();
        let mut writer = if legacy(name) {
            ObkWriter::new_legacy(BufWriter::new(file), &f.chain, opts).unwrap()
        } else {
            ObkWriter::new(BufWriter::new(file), &f.chain, opts).unwrap()
        };
        if parent.is_none() {
            for n in 0..64 {
                writer
                    .write_cluster(n, &vec![n as u8 + 0x20; 1 << cluster_bits()])
                    .unwrap();
            }
        } else {
            writer.write_zero_cluster(0).unwrap();
            writer.write_cluster(1, &bytes(1, false)).unwrap();
        }
        writer
            .finish_with_reserve(OLD, 4096)
            .unwrap()
            .into_inner()
            .unwrap()
            .sync_all()
            .unwrap();
    }
    if compaction(name) {
        for value in [0x88, 0x89, 0x22] {
            apply_changed_with(
                &root.join("base.obk"),
                &f.chain,
                &[2],
                |_| Ok(vec![value; 1 << cluster_bits()]),
                RebuildOptions {
                    previous: PreviousContents::Keep,
                    ..Default::default()
                },
            )
            .unwrap();
        }
    }
    f
}
pub(super) fn before(root: &Path, key: &ChainKey) -> serde_json::Value {
    let image = ObkImage::open_path(&root.join("base.obk"), key).unwrap();
    serde_json::json!({
        "base_sha256":hash(&root.join("base.obk")), "leaf_sha256":hash(&root.join("leaf.obk")),
        "chain_sha256":digest(&root.join("leaf.obk"), key, false),
        "generation":image.open_header().header_generation, "backup_id":image.open_header().backup_id,
        "chain_id":image.open_header().chain_id
    })
}
fn retry(root: &Path, key: &ChainKey, name: &str) {
    if compaction(name) {
        compact_image_with_progress(&root.join("base.obk"), key, |_, _| Ok(())).unwrap();
    } else {
        rebuild(&root.join("base.obk"), key);
    }
}
fn validate(
    root: &Path,
    key: &ChainKey,
    name: &str,
    before: &serde_json::Value,
) -> serde_json::Value {
    let base = root.join("base.obk");
    let leaf = root.join("leaf.obk");
    let mut image = ObkImage::open_path(&base, key).unwrap();
    assert_eq!(
        image.open_header().header_generation,
        before["generation"].as_u64().unwrap()
    );
    assert_eq!(
        serde_json::to_value(image.open_header().backup_id).unwrap(),
        before["backup_id"]
    );
    assert_eq!(
        serde_json::to_value(image.open_header().chain_id).unwrap(),
        before["chain_id"]
    );
    assert_eq!(&image.sealed_metadata().unwrap()[..], OLD);
    drop(image);
    assert_eq!(hash(&leaf), before["leaf_sha256"].as_str().unwrap());
    let original_unchanged = hash(&base) == before["base_sha256"].as_str().unwrap();
    if legacy(name) || compaction(name) {
        assert!(original_unchanged);
    }
    let digest_before = digest(&leaf, key, false);
    assert_eq!(digest_before, before["chain_sha256"].as_str().unwrap());
    let stages: Vec<_> = std::fs::read_dir(root)
        .unwrap()
        .map(|p| p.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|p| p.starts_with(".own-backup-obk-"))
        .collect();
    retry(root, key, name);
    let updated = !compaction(name);
    let digest_after = digest(&leaf, key, updated);
    let mut image = ObkImage::open_path(&base, key).unwrap();
    assert_eq!(
        &image.sealed_metadata().unwrap()[..],
        if updated { NEW } else { OLD }
    );
    let generation = image.open_header().header_generation;
    assert_eq!(
        generation,
        before["generation"].as_u64().unwrap() + u64::from(!legacy(name))
    );
    assert_eq!(
        serde_json::to_value(image.open_header().backup_id).unwrap(),
        before["backup_id"]
    );
    assert_eq!(
        serde_json::to_value(image.open_header().chain_id).unwrap(),
        before["chain_id"]
    );
    assert_eq!(hash(&leaf), before["leaf_sha256"].as_str().unwrap());
    // Retry never promotes the interrupted candidate. Registered dead siblings
    // are reclaimed before the new transaction; this fixture has no outsiders.
    let stages_after: Vec<_> = std::fs::read_dir(root)
        .unwrap()
        .map(|p| p.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|p| p.starts_with(".own-backup-obk-"))
        .collect();
    assert!(stages_after.is_empty());
    serde_json::json!({"point":name,"pass":true,"retry_pass":true,"clusters_checked":64,
        "original_file_unchanged":original_unchanged,"generation_before":before["generation"],
        "generation_after_retry":generation,"chain_sha256_before":digest_before,
        "chain_sha256_after_retry":digest_after,"stale_stages":stages,"stages_after_retry":stages_after})
}
fn run_cut(root: &Path, key: &ChainKey, name: &str, stop: impl Fn() + 'static) {
    if compaction(name) {
        let validation = name == "compact-readback-in-progress";
        compact_image_with_progress(&root.join("base.obk"), key, move |done, total| {
            if total > 0 && done == if validation { total / 2 + 1 } else { 1 } {
                stop();
            }
            Ok(())
        })
        .unwrap();
    } else {
        let at = match name {
            "write-index" => Point::IndexWritten,
            "write-metadata" => Point::MetadataWritten,
            "write-root" => Point::RootWritten,
            "write-l2" => Point::L2Written,
            "write-l1" => Point::L1Written,
            "write-refcount-block" => Point::RefcountBlockWritten,
            "write-refcount-table" | "legacy-write-refcount-table" => Point::RefcountTableWritten,
            _ => Point::RecordAppended,
        };
        let _guard = observe(move |point, _| {
            if point == at {
                stop();
            }
        });
        rebuild(&root.join("base.obk"), key);
    }
}
#[test]
fn interrupting_preparation_preserves_the_published_chain_and_allows_retry() {
    use std::cell::Cell;
    use std::rc::Rc;
    for &name in CASES {
        let root = tempfile::tempdir().unwrap();
        let f = prepare(root.path(), name);
        let before = before(root.path(), &f.chain);
        let hit = Rc::new(Cell::new(false));
        let checkpoint = hit.clone();
        let interrupted = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            run_cut(root.path(), &f.chain, name, move || {
                checkpoint.set(true);
                panic!("deliberate preparation cut")
            });
        }));
        assert!(hit.get(), "checkpoint not reached: {name}");
        assert!(interrupted.is_err(), "checkpoint not reached: {name}");
        validate(root.path(), &f.chain, name, &before);
    }
}
#[cfg(windows)]
fn say(text: &str) {
    let mut port = std::fs::OpenOptions::new()
        .write(true)
        .open(r"\\.\COM1")
        .unwrap();
    writeln!(port, "{text}").unwrap();
    port.flush().unwrap();
}
#[cfg(windows)]
#[test]
#[ignore = "requires disposable Windows VM and host-controlled power cut"]
fn vm_power_cut_data_preparation() {
    let root =
        std::path::PathBuf::from(std::env::var_os("OWN_BACKUP_H03_DATA_ROOT").expect("root"));
    let name = std::env::var("OWN_BACKUP_H03_DATA_POINT").expect("point");
    assert!(CASES.contains(&name.as_str()));
    match std::env::var("OWN_BACKUP_H03_DATA_PHASE").as_deref() {
        Ok("prepare") => {
            std::fs::create_dir(&root).unwrap();
            let f = prepare(&root, &name);
            let before = before(&root, &f.chain);
            let mut file = std::fs::File::create_new(root.join("before.json")).unwrap();
            file.write_all(&serde_json::to_vec_pretty(&before).unwrap())
                .unwrap();
            file.sync_all().unwrap();
            say(&format!("H03-DATA-PREPARE-PASS {before}"));
        }
        Ok("cut") => {
            let expected: serde_json::Value =
                serde_json::from_slice(&std::fs::read(root.join("before.json")).unwrap()).unwrap();
            let key = super::read_open_header(&root.join("base.obk"))
                .unwrap()
                .chain_key_with_password("пароль архива")
                .unwrap();
            assert_eq!(before(&root, &key), expected);
            say(&format!("H03-BOUNDARY-BASELINE-PASS point={name}"));
            let point = name.clone();
            run_cut(&root, &key, &name, move || {
                // No disk marker, readback, flush or filesystem operation at the cut.
                say(&format!(
                    "H03-BOUNDARY-CUT {}",
                    serde_json::json!({"point":point})
                ));
                loop {
                    std::thread::sleep(std::time::Duration::from_secs(1));
                }
            });
            panic!("checkpoint not reached");
        }
        Ok("verify") => {
            let expected: serde_json::Value =
                serde_json::from_slice(&std::fs::read(root.join("before.json")).unwrap()).unwrap();
            let key = super::read_open_header(&root.join("base.obk"))
                .unwrap()
                .chain_key_with_password("пароль архива")
                .unwrap();
            let result = validate(&root, &key, &name, &expected);
            std::fs::write(
                root.join("result.json"),
                serde_json::to_vec_pretty(&result).unwrap(),
            )
            .unwrap();
            say(&format!("H03-DATA-VALIDATE-PASS point={name} {result}"));
        }
        _ => panic!("expected prepare, cut or verify"),
    }
}
