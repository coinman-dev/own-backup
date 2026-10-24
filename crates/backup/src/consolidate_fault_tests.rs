//! Test-only coordinator checkpoints; the product contains none of them.
use std::cell::RefCell;
use std::path::Path;

use sha2::{Digest, Sha256};

use super::tests::{obk_keys, obk_sidecar, put_obk, read_obk, OBK_CLUSTER, VSIZE};
use super::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Point {
    FoldedBeforeMarker,
    RepointedBeforeDelete,
    VictimDeletedBeforeMarker,
    MarkerDeletedAfterCommit,
}
type Observer = Box<dyn FnMut(Point, &Path) -> Result<()>>;
thread_local! { static OBSERVER: RefCell<Option<Observer>> = const { RefCell::new(None) }; }

pub(super) fn hit(point: Point, path: &Path) -> Result<()> {
    OBSERVER.with_borrow_mut(|observer| match observer {
        Some(observer) => observer(point, path),
        None => Ok(()),
    })
}
struct Guard(Option<Observer>);
impl Drop for Guard {
    fn drop(&mut self) {
        OBSERVER.with_borrow_mut(|old| *old = self.0.take());
    }
}
fn observe(observer: impl FnMut(Point, &Path) -> Result<()> + 'static) -> Guard {
    Guard(OBSERVER.with_borrow_mut(|old| old.replace(Box::new(observer))))
}

fn files(root: &Path) -> (PathBuf, PathBuf, PathBuf) {
    (
        root.join("e_C_full_1.obk"),
        root.join("e_C_inc_2.obk"),
        root.join("e_C_inc_3.obk"),
    )
}
fn prepare(root: &Path) -> ArchiveKeys {
    let keys = obk_keys();
    let session = keys.start_chain().unwrap();
    let (base, base_id) = put_obk(
        root,
        "e_C_full_1.obk",
        &keys,
        &session,
        ChainRole::Full,
        "2026-01-01T00:00:00Z",
        BackupType::Full,
        None,
        &[(1, 0x11), (2, 0x11), (5, 0x55)],
        &[],
    );
    let (victim, victim_id) = put_obk(
        root,
        "e_C_inc_2.obk",
        &keys,
        &session,
        ChainRole::Incremental,
        "2026-01-02T00:00:00Z",
        BackupType::Incremental,
        Some((base_id, "e_C_full_1.obk")),
        &[(1, 0), (2, 0x22), (3, 0x22)],
        &[],
    );
    let (leaf, _) = put_obk(
        root,
        "e_C_inc_3.obk",
        &keys,
        &session,
        ChainRole::Incremental,
        "2026-01-03T00:00:00Z",
        BackupType::Incremental,
        Some((victim_id, "e_C_inc_2.obk")),
        &[(3, 0x33), (4, 0x33)],
        &[],
    );
    for path in [&base, &victim, &leaf] {
        std::fs::OpenOptions::new()
            .write(true)
            .open(path)
            .unwrap()
            .sync_all()
            .unwrap();
    }
    keys
}
fn digest(leaf: &Path, keys: &ArchiveKeys) -> String {
    let mut hash = Sha256::new();
    for bytes in read_obk(leaf, keys) {
        hash.update(bytes);
    }
    format!("{:x}", hash.finalize())
}
fn validate(root: &Path, keys: &ArchiveKeys, expected: &str) {
    let (base, victim, leaf) = files(root);
    assert!(!victim.exists() && !marker_path(&base).exists());
    assert_eq!(digest(&leaf, keys), expected);
    let values = read_obk(&leaf, keys);
    assert_eq!(values.len() as u64, VSIZE / OBK_CLUSTER);
    for (n, bytes) in values.iter().enumerate() {
        let fill = match n {
            2 => 0x22,
            3 | 4 => 0x33,
            5 => 0x55,
            _ => 0,
        };
        assert_eq!(bytes, &vec![fill; OBK_CLUSTER as usize]);
    }
    let base_sidecar = obk_sidecar(&base, keys);
    let leaf_sidecar = obk_sidecar(&leaf, keys);
    assert_eq!(base_sidecar.info.consolidated_increments, 1);
    assert_eq!(
        base_sidecar.info.chain_started_at.as_deref(),
        Some("2026-01-01T00:00:00Z")
    );
    assert_eq!(base_sidecar.info.chain_role, ChainRole::Full);
    assert_eq!(
        leaf_sidecar.info.parent_image_file_name.as_deref(),
        Some("e_C_full_1.obk")
    );
    assert_eq!(
        leaf_sidecar.manifest.parent_backup_id,
        Some(base_sidecar.manifest.backup_id)
    );
    assert!(resume_pending_with_keys(root, Some(keys))
        .unwrap()
        .finished
        .is_empty());
}

#[test]
fn every_coordinator_checkpoint_resumes_without_counting_the_fold_twice() {
    for point in [
        Point::FoldedBeforeMarker,
        Point::RepointedBeforeDelete,
        Point::VictimDeletedBeforeMarker,
        Point::MarkerDeletedAfterCommit,
    ] {
        let root = tempfile::tempdir().unwrap();
        let keys = prepare(root.path());
        let (base, victim, leaf) = files(root.path());
        let expected = digest(&leaf, &keys);
        let guard = observe(move |at, _| {
            if at == point {
                Err(OwnBackupError::Cancelled)
            } else {
                Ok(())
            }
        });
        assert!(matches!(
            execute(&base, &victim, Some(&leaf), Some(&keys)),
            Err(OwnBackupError::Cancelled)
        ));
        drop(guard);
        assert_eq!(
            marker_path(&base).exists(),
            point != Point::MarkerDeletedAfterCommit
        );
        let report = resume_pending_with_keys(root.path(), Some(&keys)).unwrap();
        assert!(report.all_finished(), "{point:?}: {report:?}");
        assert_eq!(
            report.finished.len(),
            if point == Point::MarkerDeletedAfterCommit {
                0
            } else {
                1
            }
        );
        validate(root.path(), &keys, &expected);
    }
}

/// The same checkpoints on a VHDX chain folded and repointed in userspace
/// (`vhdx_chain`, the arm used off Windows): every interruption resumes to a
/// base that reads as the folded increment did and a leaf that reads as it
/// always did, with the fold counted once.
#[cfg(not(target_os = "windows"))]
#[test]
fn every_coordinator_checkpoint_resumes_a_vhdx_chain() {
    use super::tests::{read_vhdx_disk, three_image_vhdx_chain};

    for point in [
        Point::FoldedBeforeMarker,
        Point::RepointedBeforeDelete,
        Point::VictimDeletedBeforeMarker,
        Point::MarkerDeletedAfterCommit,
    ] {
        let root = tempfile::tempdir().unwrap();
        let (base, victim, leaf, victim_id) = three_image_vhdx_chain(root.path());
        let (victim_before, leaf_before) = (read_vhdx_disk(&victim), read_vhdx_disk(&leaf));
        let guard = observe(move |at, _| {
            if at == point {
                Err(OwnBackupError::Cancelled)
            } else {
                Ok(())
            }
        });
        assert!(matches!(
            execute(&base, &victim, Some(&leaf), None),
            Err(OwnBackupError::Cancelled)
        ));
        drop(guard);
        assert_eq!(
            marker_path(&base).exists(),
            point != Point::MarkerDeletedAfterCommit
        );
        let report = resume_pending(root.path()).unwrap();
        assert!(report.all_finished(), "{point:?}: {report:?}");
        assert_eq!(
            report.finished.len(),
            if point == Point::MarkerDeletedAfterCommit {
                0
            } else {
                1
            }
        );

        assert!(
            !victim.exists() && !marker_path(&base).exists(),
            "{point:?}"
        );
        assert_eq!(read_vhdx_disk(&base), victim_before, "{point:?}");
        assert_eq!(read_vhdx_disk(&leaf), leaf_before, "{point:?}");
        assert!(
            crate::vhdx_bat::vhdx_parent_linkage_matches(&leaf, &base).unwrap(),
            "{point:?}"
        );
        let base_sidecar = read_image_sidecar(&base).unwrap();
        assert_eq!(base_sidecar.info.consolidated_increments, 1, "{point:?}");
        assert_eq!(base_sidecar.info.chain_role, ChainRole::Full, "{point:?}");
        assert_eq!(base_sidecar.manifest.backup_id, victim_id, "{point:?}");
        assert_eq!(
            read_image_sidecar(&leaf)
                .unwrap()
                .info
                .parent_image_file_name
                .as_deref(),
            Some("v_C_full_1.vhdx"),
            "{point:?}"
        );
        assert!(resume_pending(root.path()).unwrap().finished.is_empty());
    }
}

#[cfg(windows)]
fn say(value: &str) {
    use std::io::Write;
    let mut port = std::fs::OpenOptions::new()
        .write(true)
        .open(r"\\.\COM1")
        .unwrap();
    writeln!(port, "{value}").unwrap();
    port.flush().unwrap();
}

#[cfg(windows)]
#[test]
#[ignore = "requires disposable Windows VM and host-controlled power cut"]
fn vm_power_cut_during_encrypted_consolidation() {
    use std::io::Write;
    let root = PathBuf::from(std::env::var_os("OWN_BACKUP_H03_CONSOLIDATION_ROOT").expect("root"));
    let name = std::env::var("OWN_BACKUP_H03_CONSOLIDATION_POINT").expect("point");
    let selected = match name.as_str() {
        "fold-before-marker" => Point::FoldedBeforeMarker,
        "repoint-before-delete" => Point::RepointedBeforeDelete,
        "victim-deleted-before-marker" => Point::VictimDeletedBeforeMarker,
        "marker-deleted-after-commit" => Point::MarkerDeletedAfterCommit,
        _ => panic!("unknown point {name}"),
    };
    let (base, victim, leaf) = files(&root);
    match std::env::var("OWN_BACKUP_H03_CONSOLIDATION_PHASE").as_deref() {
        Ok("prepare") => {
            std::fs::create_dir(&root).unwrap();
            let keys = prepare(&root);
            let before = serde_json::json!({"point":name,"chain_sha256":digest(&leaf,&keys),
                "base_id":obk_sidecar(&base,&keys).manifest.backup_id,
                "victim_id":obk_sidecar(&victim,&keys).manifest.backup_id});
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(root.join("before.json"))
                .unwrap();
            file.write_all(&serde_json::to_vec_pretty(&before).unwrap())
                .unwrap();
            file.sync_all().unwrap();
            say(&format!("H03-CONSOLIDATION-PREPARE-PASS {before}"));
        }
        Ok("cut") => {
            let before: serde_json::Value =
                serde_json::from_slice(&std::fs::read(root.join("before.json")).unwrap()).unwrap();
            let keys = ArchiveKeys::from_image_password(&base, "пароль архива").unwrap();
            assert_eq!(before["point"], name);
            assert_eq!(
                digest(&leaf, &keys),
                before["chain_sha256"].as_str().unwrap()
            );
            assert!(!marker_path(&base).exists());
            say(&format!("H03-BOUNDARY-BASELINE-PASS point={name}"));
            let chosen = name.clone();
            let _guard = observe(move |at, path| {
                if at == selected {
                    say(&format!(
                        "H03-BOUNDARY-CUT {}",
                        serde_json::json!({"point":chosen,
                        "marker_exists":marker_path(path).exists(),"victim_exists":path.with_file_name("e_C_inc_2.obk").exists()})
                    ));
                    loop {
                        std::thread::sleep(std::time::Duration::from_secs(1));
                    }
                }
                Ok(())
            });
            execute(&base, &victim, Some(&leaf), Some(&keys)).unwrap();
            panic!("checkpoint not reached");
        }
        Ok("verify") => {
            let before: serde_json::Value =
                serde_json::from_slice(&std::fs::read(root.join("before.json")).unwrap()).unwrap();
            let keys = ArchiveKeys::from_image_password(&base, "пароль архива").unwrap();
            let marker_before = read_marker(&marker_path(&base));
            if selected != Point::MarkerDeletedAfterCommit {
                assert!(marker_before.is_some(), "durable marker missing");
            }
            let victim_before = victim.exists();
            let report = resume_pending_with_keys(&root, Some(&keys)).unwrap();
            assert!(report.all_finished(), "{report:?}");
            assert_eq!(
                report.finished.len(),
                if marker_before.is_some() { 1 } else { 0 }
            );
            validate(&root, &keys, before["chain_sha256"].as_str().unwrap());
            assert_eq!(
                obk_sidecar(&base, &keys).manifest.backup_id.to_string(),
                before["victim_id"].as_str().unwrap()
            );
            let result = serde_json::json!({"pass":true,"point":name,"marker_merged_before_resume":marker_before.as_ref().map(|m|m.merged),
                "victim_exists_before_resume":victim_before,"resumed":report.finished.len(),
                "chain_sha256":digest(&leaf,&keys),"folded_count":1,"marker_removed":true,"victim_removed":true,
                "leaf_parent":"e_C_full_1.obk","clusters_checked":VSIZE/OBK_CLUSTER});
            std::fs::write(
                root.join("result.json"),
                serde_json::to_vec_pretty(&result).unwrap(),
            )
            .unwrap();
            say(&format!("H03-CONSOLIDATION-RECOVER-PASS {result}"));
            say(&format!("H03-CONSOLIDATION-VALIDATE-PASS point={name}"));
        }
        other => panic!("unknown phase {other:?}"),
    }
}
