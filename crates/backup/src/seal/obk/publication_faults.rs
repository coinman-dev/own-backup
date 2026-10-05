//! Checkpoints compiled only into the test harness, never into product binaries.
//! Serial notifications at the cut point perform no further filesystem writes.
use std::cell::RefCell;
use std::path::Path;

use sha2::{Digest, Sha256};

use super::container_tests::{cluster_bits, Fixture};
use super::{
    apply_changed_with, compact_image, read_open_header, ObkChain, ObkImage, ObkWriter,
    PreviousContents, RebuildOptions,
};
use crate::seal::ChainKey;
use crate::sidecar::ChainRole;
use uuid::Uuid;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Point {
    BeforeRename,
    AfterRename,
    HeaderWritten,
    AppendedSynced,
    HeaderSynced,
}

type Observer = Box<dyn FnMut(Point, &Path)>;
thread_local! {
    static OBSERVER: RefCell<Option<Observer>> = const { RefCell::new(None) };
}

pub(super) fn hit(point: Point, path: &Path) {
    OBSERVER.with_borrow_mut(|observer| {
        if let Some(observer) = observer {
            observer(point, path);
        }
    });
}

pub(super) struct Guard(Option<Observer>);
impl Drop for Guard {
    fn drop(&mut self) {
        OBSERVER.with_borrow_mut(|observer| *observer = self.0.take());
    }
}
pub(super) fn observe(observer: impl FnMut(Point, &Path) + 'static) -> Guard {
    Guard(OBSERVER.with_borrow_mut(|previous| previous.replace(Box::new(observer))))
}

fn file_hash(path: &Path) -> String {
    format!("{:x}", Sha256::digest(std::fs::read(path).unwrap()))
}

fn chain_hash(path: &Path, key: &ChainKey) -> String {
    let mut chain = ObkChain::open(path, key).unwrap();
    let mut hash = Sha256::new();
    for n in 0..64 {
        hash.update(&chain.read_cluster(n).unwrap()[..]);
    }
    format!("{:x}", hash.finalize())
}

fn prepare(root: &Path) -> Fixture {
    let fixture = Fixture::new();
    let base = root.join("base.obk");
    let base_id = Uuid::new_v4();
    let mut writer = ObkWriter::create(
        &base,
        &fixture.chain,
        fixture.options(base_id, ChainRole::Full, None),
    )
    .unwrap();
    for (n, value) in [(0, 0x11), (2, 0x31), (4, 0x55)] {
        writer
            .write_cluster(n, &vec![value; 1 << cluster_bits()])
            .unwrap();
    }
    writer
        .finish(b"previous metadata")
        .unwrap()
        .into_inner()
        .unwrap()
        .sync_all()
        .unwrap();
    for value in [0x32, 0x33, 0x34] {
        apply_changed_with(
            &base,
            &fixture.chain,
            &[2],
            |_| Ok(vec![value; 1 << cluster_bits()]),
            RebuildOptions {
                previous: PreviousContents::Keep,
                ..Default::default()
            },
        )
        .unwrap();
    }
    let child = root.join("child.obk");
    let mut writer = ObkWriter::create(
        &child,
        &fixture.chain,
        fixture.options(
            Uuid::new_v4(),
            ChainRole::Incremental,
            Some((base_id, "base.obk")),
        ),
    )
    .unwrap();
    writer.write_zero_cluster(0).unwrap();
    writer
        .write_cluster(1, &vec![0x22; 1 << cluster_bits()])
        .unwrap();
    writer
        .finish(b"child metadata")
        .unwrap()
        .into_inner()
        .unwrap()
        .sync_all()
        .unwrap();
    fixture
}

#[test]
fn compaction_observes_the_actual_rename_edges() {
    use std::rc::Rc;
    let root = tempfile::tempdir().unwrap();
    let fixture = prepare(root.path());
    let base = root.path().join("base.obk");
    let child = root.path().join("child.obk");
    let before = file_hash(&base);
    let digest = chain_hash(&child, &fixture.chain);
    let seen = Rc::new(RefCell::new(Vec::new()));
    let events = seen.clone();
    let original = base.clone();
    let old = before.clone();
    let guard = observe(move |point, path| {
        if point == Point::BeforeRename {
            assert_eq!(file_hash(&original), old);
        }
        events.borrow_mut().push((point, file_hash(path)));
    });
    compact_image(&base, &fixture.chain).unwrap();
    drop(guard);
    let events = seen.borrow();
    assert_eq!(events.len(), 2);
    assert_eq!(events[0].0, Point::BeforeRename);
    assert_eq!(events[1].0, Point::AfterRename);
    assert_eq!(events[0].1, events[1].1);
    assert_eq!(events[1].1, file_hash(&base));
    assert_ne!(events[1].1, before);
    assert_eq!(chain_hash(&child, &fixture.chain), digest);
}

#[test]
fn the_header_checkpoint_sees_one_coherent_data_and_metadata_generation() {
    use std::rc::Rc;
    let root = tempfile::tempdir().unwrap();
    let fixture = prepare(root.path());
    let base = root.path().join("base.obk");
    let generation = read_open_header(&base).unwrap().header_generation;
    let seen = Rc::new(RefCell::new(Vec::new()));
    let events = seen.clone();
    let guard = observe(move |point, path| {
        events
            .borrow_mut()
            .push((point, read_open_header(path).unwrap().header_generation));
    });
    apply_changed_with(
        &base,
        &fixture.chain,
        &[2],
        |_| Ok(vec![0x44; 1 << cluster_bits()]),
        RebuildOptions {
            previous: PreviousContents::Keep,
            metadata: Some(b"new metadata"),
            ..Default::default()
        },
    )
    .unwrap();
    drop(guard);
    assert_eq!(
        &seen.borrow()[..],
        &[
            (Point::AppendedSynced, generation),
            (Point::HeaderWritten, generation + 1),
            (Point::HeaderSynced, generation + 1)
        ]
    );
    let mut image = ObkImage::open_path(&base, &fixture.chain).unwrap();
    assert_eq!(&image.sealed_metadata().unwrap()[..], b"new metadata");
    let mut chain = ObkChain::open(&root.path().join("child.obk"), &fixture.chain).unwrap();
    assert_eq!(
        &chain.read_cluster(2).unwrap()[..],
        &vec![0x44; 1 << cluster_bits()]
    );
    assert_eq!(
        &chain.read_cluster(1).unwrap()[..],
        &vec![0x22; 1 << cluster_bits()]
    );
}

#[cfg(windows)]
#[test]
fn an_open_handle_blocks_publication_and_retry_succeeds() {
    use std::os::windows::fs::OpenOptionsExt;
    use windows::Win32::Storage::FileSystem::{FILE_SHARE_READ, FILE_SHARE_WRITE};
    let root = tempfile::tempdir().unwrap();
    let fixture = prepare(root.path());
    let base = root.path().join("base.obk");
    let child = root.path().join("child.obk");
    let before = file_hash(&base);
    let digest = chain_hash(&child, &fixture.chain);
    let reader = std::fs::OpenOptions::new()
        .read(true)
        .share_mode((FILE_SHARE_READ | FILE_SHARE_WRITE).0)
        .open(&base)
        .unwrap();
    let failure = compact_image(&base, &fixture.chain)
        .unwrap_err()
        .to_string();
    // MoveFileExW on NTFS may report ACCESS_DENIED for a destination handle
    // lacking FILE_SHARE_DELETE. Both codes must disappear once it is closed.
    println!("H03-RENAME-REFUSED {failure}");
    assert!(
        failure.contains("cannot publish")
            && (failure.contains("0x80070020") || failure.contains("0x80070005")),
        "{failure}"
    );
    assert_eq!(file_hash(&base), before);
    assert_eq!(chain_hash(&child, &fixture.chain), digest);
    assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 2);
    drop(reader);
    compact_image(&base, &fixture.chain).unwrap();
    assert_eq!(chain_hash(&child, &fixture.chain), digest);
}

#[cfg(windows)]
fn say(message: &str) {
    use std::io::Write;
    let mut serial = std::fs::OpenOptions::new()
        .write(true)
        .open(r"\\.\COM1")
        .unwrap();
    writeln!(serial, "{message}").unwrap();
    serial.flush().unwrap();
}

#[cfg(windows)]
#[test]
#[ignore = "requires a disposable Windows VM and host-controlled power cut"]
fn vm_power_cut_at_publication_boundary() {
    use std::io::Write;
    let root = std::path::PathBuf::from(
        std::env::var_os("OWN_BACKUP_H03_BOUNDARY_ROOT").expect("fixture root"),
    );
    let name = std::env::var("OWN_BACKUP_H03_BOUNDARY_POINT").expect("cut point");
    let point = match name.as_str() {
        "before-rename" => Point::BeforeRename,
        "after-rename" => Point::AfterRename,
        "header-before-sync" => Point::HeaderWritten,
        _ => panic!("unknown cut point: {name}"),
    };
    let base = root.join("base.obk");
    let child = root.join("child.obk");
    match std::env::var("OWN_BACKUP_H03_BOUNDARY_PHASE").as_deref() {
        Ok("prepare") => {
            std::fs::create_dir(&root).unwrap();
            let fixture = prepare(&root);
            let before = serde_json::json!({
                "point":name,"file_sha256":file_hash(&base),
                "chain_sha256":chain_hash(&child,&fixture.chain),
                "generation":read_open_header(&base).unwrap().header_generation,
                "bytes":std::fs::metadata(&base).unwrap().len()
            });
            let mut manifest = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(root.join("before.json"))
                .unwrap();
            manifest
                .write_all(&serde_json::to_vec_pretty(&before).unwrap())
                .unwrap();
            manifest.sync_all().unwrap();
            drop(manifest);
            say(&format!("H03-BOUNDARY-PREPARE-PASS {before}"));
        }
        Ok("cut") => {
            let before: serde_json::Value =
                serde_json::from_slice(&std::fs::read(root.join("before.json")).unwrap()).unwrap();
            assert_eq!(before["point"], name);
            let key = read_open_header(&child)
                .unwrap()
                .chain_key_with_password("пароль архива")
                .unwrap();
            assert_eq!(file_hash(&base), before["file_sha256"].as_str().unwrap());
            assert_eq!(
                chain_hash(&child, &key),
                before["chain_sha256"].as_str().unwrap()
            );
            say(&format!("H03-BOUNDARY-BASELINE-PASS point={name}"));
            let selected = name.clone();
            let _guard = observe(move |at, path| {
                if at == point {
                    let header = read_open_header(path).unwrap();
                    say(&format!(
                        "H03-BOUNDARY-CUT {}",
                        serde_json::json!({
                            "point":selected,"candidate_sha256":file_hash(path),
                            "candidate_generation":header.header_generation
                        })
                    ));
                    loop {
                        std::thread::sleep(std::time::Duration::from_secs(1));
                    }
                }
            });
            if point == Point::HeaderWritten {
                apply_changed_with(
                    &base,
                    &key,
                    &[2],
                    |_| Ok(vec![0x44; 1 << cluster_bits()]),
                    RebuildOptions {
                        previous: PreviousContents::Keep,
                        metadata: Some(b"new metadata"),
                        ..Default::default()
                    },
                )
                .unwrap();
            } else {
                compact_image(&base, &key).unwrap();
            }
            panic!("cut point not reached");
        }
        Ok("verify") => {
            let before: serde_json::Value =
                serde_json::from_slice(&std::fs::read(root.join("before.json")).unwrap()).unwrap();
            assert_eq!(before["point"], name);
            let key = read_open_header(&child)
                .unwrap()
                .chain_key_with_password("пароль архива")
                .unwrap();
            let mut chain = ObkChain::open(&child, &key).unwrap();
            assert_eq!(chain.chain_len(), 2);
            assert_eq!(
                &chain.read_cluster(0).unwrap()[..],
                &vec![0; 1 << cluster_bits()]
            );
            assert_eq!(
                &chain.read_cluster(1).unwrap()[..],
                &vec![0x22; 1 << cluster_bits()]
            );
            assert_eq!(
                &chain.read_cluster(4).unwrap()[..],
                &vec![0x55; 1 << cluster_bits()]
            );
            let mut image = ObkImage::open_path(&base, &key).unwrap();
            let generation = image.open_header().header_generation;
            let metadata = image.sealed_metadata().unwrap();
            drop(image); // Windows publication requires all old reader handles closed.
            let old_generation = before["generation"].as_u64().unwrap();
            let old = generation == old_generation;
            if point == Point::HeaderWritten {
                assert!(old || generation == old_generation + 1);
                assert_eq!(
                    &chain.read_cluster(2).unwrap()[..],
                    &vec![if old { 0x34 } else { 0x44 }; 1 << cluster_bits()]
                );
                assert_eq!(
                    &metadata[..],
                    if old {
                        b"previous metadata".as_slice()
                    } else {
                        b"new metadata".as_slice()
                    }
                );
            } else {
                assert_eq!(
                    chain_hash(&child, &key),
                    before["chain_sha256"].as_str().unwrap()
                );
                if point == Point::BeforeRename {
                    assert!(old);
                    assert_eq!(file_hash(&base), before["file_sha256"].as_str().unwrap());
                } else {
                    assert_eq!(generation, old_generation + 1);
                }
            }
            drop(chain);
            let mut report = serde_json::json!({"recovery_pass":true,"point":name,"generation":generation,
                "old_generation":old_generation,"old_point":old,"file_sha256":file_hash(&base),
                "chain_sha256":chain_hash(&child,&key)});
            say(&format!("H03-BOUNDARY-RECOVER-PASS {report}"));
            // A retry must still work after crash recovery, using new sessions.
            if point == Point::HeaderWritten {
                apply_changed_with(
                    &base,
                    &key,
                    &[2],
                    |_| Ok(vec![0x44; 1 << cluster_bits()]),
                    RebuildOptions {
                        previous: PreviousContents::Keep,
                        metadata: Some(b"new metadata"),
                        ..Default::default()
                    },
                )
                .unwrap();
            } else {
                compact_image(&base, &key).unwrap();
            }
            let mut retry = ObkChain::open(&child, &key).unwrap();
            for n in 0..64 {
                let fill = match n {
                    1 => 0x22,
                    2 => {
                        if point == Point::HeaderWritten {
                            0x44
                        } else {
                            0x34
                        }
                    }
                    4 => 0x55,
                    _ => 0,
                };
                assert_eq!(
                    &retry.read_cluster(n).unwrap()[..],
                    &vec![fill; 1 << cluster_bits()]
                );
            }
            drop(retry);
            assert_eq!(
                read_open_header(&base).unwrap().header_generation,
                generation + 1
            );
            report["retry_pass"] = true.into();
            report["pass"] = true.into();
            report["retry_chain_sha256"] = chain_hash(&child, &key).into();
            std::fs::write(
                root.join("result.json"),
                serde_json::to_vec_pretty(&report).unwrap(),
            )
            .unwrap();
            say(&format!("H03-BOUNDARY-RETRY-PASS point={name}"));
        }
        phase => panic!("unknown phase: {phase:?}"),
    }
}
