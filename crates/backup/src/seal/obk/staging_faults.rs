//! Actual VM cuts around ownership, publication and interrupted reclamation.
#[cfg(windows)]
use super::*;
#[cfg(windows)]
use crate::seal::obk::{
    compact_image_with_progress, data_faults,
    publication_faults::{observe, Point},
    ObkImage,
};

#[cfg(windows)]
fn say(text: &str) {
    let mut port = OpenOptions::new().write(true).open(r"\\.\COM1").unwrap();
    writeln!(port, "{text}").unwrap();
    port.flush().unwrap();
}
#[cfg(windows)]
fn hash(path: &Path) -> String {
    use sha2::{Digest, Sha256};
    format!("{:x}", Sha256::digest(std::fs::read(path).unwrap()))
}
#[cfg(windows)]
fn point(name: &str) -> Point {
    match name {
        "cleanup-staged-before-rename" | "cleanup-final-handles-before-rename" => {
            Point::BeforeRename
        }
        "cleanup-owner-after-rename" => Point::AfterRename,
        "cleanup-between-stage-and-owner" => Point::OrphanStageRemoved,
        _ => panic!("unknown cleanup point"),
    }
}
#[cfg(windows)]
fn protected(root: &Path, key: &ChainKey) -> std::collections::BTreeMap<String, String> {
    let foreign = root.join("foreign-control.bin");
    std::fs::write(&foreign, b"foreign-target control").unwrap();
    let (owner, stage) = super::tests::abandon(&std::fs::canonicalize(&foreign).unwrap(), key);
    let legacy = root.join(".own-backup-obk-TZ6W6D.tmp");
    std::fs::write(&legacy, b"old unregistered control").unwrap();
    let fake_prefix = format!("{PREFIX}{}", Uuid::new_v4().simple());
    let fake_owner = root.join(format!("{fake_prefix}.owner"));
    let fake_stage = root.join(format!("{fake_prefix}.tmp"));
    std::fs::write(&fake_owner, b"OBKSTG01 forged ownership").unwrap();
    std::fs::write(&fake_stage, b"malformed-owner control").unwrap();
    [foreign, owner, stage, legacy, fake_owner, fake_stage]
        .into_iter()
        .map(|p| {
            (
                p.file_name().unwrap().to_string_lossy().into_owned(),
                hash(&p),
            )
        })
        .collect()
}
#[cfg(windows)]
fn check_protected(root: &Path, expected: &std::collections::BTreeMap<String, String>) {
    for (name, digest) in expected {
        assert_eq!(&hash(&root.join(name)), digest, "protected {name}");
    }
}
#[cfg(windows)]
fn leftovers(root: &Path, protected: &std::collections::BTreeMap<String, String>) -> Vec<PathBuf> {
    std::fs::read_dir(root)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| {
            p.file_name()
                .unwrap()
                .to_str()
                .is_some_and(|n| n.starts_with(PREFIX) && !protected.contains_key(n))
        })
        .collect()
}
#[cfg(windows)]
#[test]
#[ignore = "requires disposable Windows VM and host-controlled power cut"]
fn vm_power_cut_staged_cleanup() {
    let root = PathBuf::from(std::env::var_os("OWN_BACKUP_H03_CLEAN_ROOT").expect("root"));
    let name = std::env::var("OWN_BACKUP_H03_CLEAN_POINT").expect("point");
    let cut_at = point(&name);
    let base = root.join("base.obk");
    let leaf = root.join("leaf.obk");
    match std::env::var("OWN_BACKUP_H03_CLEAN_PHASE").as_deref() {
        Ok("prepare") => {
            std::fs::create_dir(&root).unwrap();
            let f = data_faults::prepare(&root, "compact-copy-in-progress");
            let controls = protected(&root, &f.chain);
            if cut_at == Point::OrphanStageRemoved {
                // A durable abandoned pair makes the cleanup interruption
                // reachable. Ownership/bytes are identical to a dead process.
                super::tests::abandon(&std::fs::canonicalize(&base).unwrap(), &f.chain);
            }
            let before = serde_json::json!({"baseline":data_faults::before(&root,&f.chain),"protected":controls});
            let mut file = File::create_new(root.join("before.json")).unwrap();
            file.write_all(&serde_json::to_vec_pretty(&before).unwrap())
                .unwrap();
            file.sync_all().unwrap();
            say(&format!("H03-CLEANUP-PREPARE-PASS {before}"));
        }
        Ok("cut") => {
            let expected: serde_json::Value =
                serde_json::from_slice(&std::fs::read(root.join("before.json")).unwrap()).unwrap();
            let key = crate::seal::obk::read_open_header(&base)
                .unwrap()
                .chain_key_with_password("пароль архива")
                .unwrap();
            assert_eq!(data_faults::before(&root, &key), expected["baseline"]);
            let controls = serde_json::from_value(expected["protected"].clone()).unwrap();
            check_protected(&root, &controls);
            say(&format!("H03-BOUNDARY-BASELINE-PASS point={name}"));
            let chosen = name.clone();
            let _guard = observe(move |at, _| {
                if at == cut_at {
                    say(&format!(
                        "H03-BOUNDARY-CUT {}",
                        serde_json::json!({"point":chosen})
                    ));
                    loop {
                        std::thread::sleep(std::time::Duration::from_secs(1));
                    }
                }
            });
            compact_image_with_progress(&base, &key, |_, _| Ok(())).unwrap();
            panic!("checkpoint not reached");
        }
        Ok("verify") => {
            let expected: serde_json::Value =
                serde_json::from_slice(&std::fs::read(root.join("before.json")).unwrap()).unwrap();
            let controls = serde_json::from_value(expected["protected"].clone()).unwrap();
            let key = crate::seal::obk::read_open_header(&base)
                .unwrap()
                .chain_key_with_password("пароль архива")
                .unwrap();
            let baseline = &expected["baseline"];
            let mut image = ObkImage::open_path(&base, &key).unwrap();
            let generation = image.open_header().header_generation;
            assert_eq!(
                generation,
                baseline["generation"].as_u64().unwrap() + u64::from(cut_at == Point::AfterRename)
            );
            assert_eq!(
                serde_json::to_value(image.open_header().backup_id).unwrap(),
                baseline["backup_id"]
            );
            assert_eq!(
                serde_json::to_value(image.open_header().chain_id).unwrap(),
                baseline["chain_id"]
            );
            assert_eq!(&image.sealed_metadata().unwrap()[..], b"data=old");
            drop(image);
            if cut_at != Point::AfterRename {
                assert_eq!(hash(&base), baseline["base_sha256"].as_str().unwrap());
            }
            assert_eq!(hash(&leaf), baseline["leaf_sha256"].as_str().unwrap());
            let chain_hash = data_faults::digest(&leaf, &key, false);
            assert_eq!(chain_hash, baseline["chain_sha256"].as_str().unwrap());
            check_protected(&root, &controls);
            let stale = leftovers(&root, &controls);
            assert!(
                stale
                    .iter()
                    .any(|p| p.extension().is_some_and(|e| e == "owner")),
                "a durable dead owner must be observed"
            );
            if cut_at == Point::BeforeRename {
                assert!(stale
                    .iter()
                    .any(|p| p.extension().is_some_and(|e| e == "tmp")));
            }
            let canonical = std::fs::canonicalize(&base).unwrap();
            let (live_owner, live_stage) = create(&canonical, &key).unwrap();
            std::fs::write(&live_stage, b"live candidate control").unwrap();
            let mut saw_cleanup = false;
            compact_image_with_progress(&base, &key, |done, total| {
                if done == 0 && total > 0 {
                    assert!(stale.iter().all(|p| !p.exists()));
                    assert_eq!(
                        std::fs::read(&live_stage).unwrap(),
                        b"live candidate control"
                    );
                    assert!(live_owner.path().exists());
                    check_protected(&root, &controls);
                    saw_cleanup = true;
                }
                Ok(())
            })
            .unwrap();
            assert!(saw_cleanup);
            assert!(live_owner.path().exists());
            assert_eq!(
                std::fs::read(&live_stage).unwrap(),
                b"live candidate control"
            );
            drop(live_stage);
            drop(live_owner);
            assert!(leftovers(&root, &controls).is_empty());
            assert_eq!(cleanup(&canonical, &key).unwrap(), 0);
            check_protected(&root, &controls);
            assert_eq!(hash(&leaf), baseline["leaf_sha256"].as_str().unwrap());
            assert_eq!(data_faults::digest(&leaf, &key, false), chain_hash);
            let after = ObkImage::open_path(&base, &key).unwrap();
            assert_eq!(after.open_header().header_generation, generation + 1);
            let result = serde_json::json!({"point":name,"pass":true,"retry_pass":true,"clusters_checked":64,
                "generation_before_retry":generation,"generation_after_retry":generation+1,"chain_sha256":chain_hash,
                "stale_files_before_retry":stale.iter().map(|p|p.file_name().unwrap().to_string_lossy().into_owned()).collect::<Vec<_>>(),
                "stale_files_after_retry":0,"protected_files_checked":controls.len(),"active_candidate_preserved":true});
            std::fs::write(
                root.join("result.json"),
                serde_json::to_vec_pretty(&result).unwrap(),
            )
            .unwrap();
            say(&format!("H03-CLEANUP-VALIDATE-PASS point={name} {result}"));
        }
        _ => panic!("expected prepare, cut or verify"),
    }
}
