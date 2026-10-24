//! Test-only published metadata/parent-link updates at actual commit edges.
use std::io::BufWriter;
use std::path::Path;

use sha2::{Digest, Sha256};
use uuid::Uuid;

use super::container_tests::{cluster_bits, Fixture};
use super::publication_faults::{observe, Point};
use super::{repoint_with_metadata, update_sealed_metadata, ObkChain, ObkImage, ObkWriter};
use crate::seal::ChainKey;
use crate::sidecar::ChainRole;

const OLD: &[u8] = b"parent=base-a.obk;version=old";
const META_NEW: &[u8] = b"parent=base-a.obk;version=new";
const LINK_NEW: &[u8] = b"parent=base-b.obk;version=new";
const CASES: &[(&str, Point, bool, bool)] = &[
    ("metadata-body-synced", Point::AppendedSynced, false, false),
    ("metadata-header-synced", Point::HeaderSynced, false, false),
    ("repoint-header-pending", Point::HeaderWritten, true, false),
    (
        "legacy-repoint-before-rename",
        Point::BeforeRename,
        true,
        true,
    ),
];
#[allow(dead_code)]
fn case(name: &str) -> (Point, bool, bool) {
    let (_, at, link, legacy) = CASES
        .iter()
        .find(|row| row.0 == name)
        .expect("known metadata case");
    (*at, *link, *legacy)
}
fn hash(path: &Path) -> String {
    format!("{:x}", Sha256::digest(std::fs::read(path).unwrap()))
}
fn prepare(root: &Path, legacy: bool) -> Fixture {
    let f = Fixture::new();
    let id = Uuid::new_v4();
    for (name, role, parent) in [
        ("base-a.obk", ChainRole::Full, None),
        ("leaf.obk", ChainRole::Incremental, Some((id, "base-a.obk"))),
    ] {
        let file = std::fs::File::create_new(root.join(name)).unwrap();
        let opts = f.options(
            if parent.is_none() { id } else { Uuid::new_v4() },
            role,
            parent,
        );
        let mut writer = if legacy {
            ObkWriter::new_legacy(BufWriter::new(file), &f.chain, opts).unwrap()
        } else {
            ObkWriter::new(BufWriter::new(file), &f.chain, opts).unwrap()
        };
        if parent.is_none() {
            writer
                .write_cluster(0, &vec![0x11; 1 << cluster_bits()])
                .unwrap();
            writer
                .write_cluster(2, &vec![0x34; 1 << cluster_bits()])
                .unwrap();
            writer
                .write_cluster(4, &vec![0x55; 1 << cluster_bits()])
                .unwrap();
        } else {
            writer.write_zero_cluster(0).unwrap();
            writer
                .write_cluster(1, &vec![0x22; 1 << cluster_bits()])
                .unwrap();
        }
        writer
            .finish_with_reserve(OLD, 4096)
            .unwrap()
            .into_inner()
            .unwrap()
            .sync_all()
            .unwrap();
    }
    std::fs::copy(root.join("base-a.obk"), root.join("base-b.obk")).unwrap();
    std::fs::OpenOptions::new()
        .write(true)
        .open(root.join("base-b.obk"))
        .unwrap()
        .sync_all()
        .unwrap();
    f
}
fn digest(leaf: &Path, key: &ChainKey) -> String {
    let mut chain = ObkChain::open(leaf, key).unwrap();
    let mut digest = Sha256::new();
    for n in 0..64 {
        let expected = match n {
            1 => 0x22,
            2 => 0x34,
            4 => 0x55,
            _ => 0,
        };
        let bytes = chain.read_cluster(n).unwrap();
        assert_eq!(&bytes[..], &vec![expected; 1 << cluster_bits()]);
        digest.update(&bytes[..]);
    }
    format!("{:x}", digest.finalize())
}
fn update(leaf: &Path, key: &ChainKey, link: bool) {
    if link {
        repoint_with_metadata(leaf, key, "base-b.obk", LINK_NEW).unwrap()
    } else {
        update_sealed_metadata(leaf, key, META_NEW).unwrap()
    }
}
#[test]
fn metadata_and_parent_name_are_coherent_at_each_commit_edge() {
    for &(name, point, link, legacy) in CASES {
        let root = tempfile::tempdir().unwrap();
        let f = prepare(root.path(), legacy);
        let leaf = root.path().join("leaf.obk");
        let before = digest(&leaf, &f.chain);
        let original = hash(&leaf);
        let watched = leaf.clone();
        let _guard = observe(move |at, path| {
            if at == point && at == Point::BeforeRename {
                assert_eq!(hash(&watched), original);
                assert_ne!(hash(path), original);
            }
        });
        update(&leaf, &f.chain, link);
        assert_eq!(digest(&leaf, &f.chain), before, "{name}");
        let mut image = ObkImage::open_path(&leaf, &f.chain).unwrap();
        assert_eq!(
            &image.sealed_metadata().unwrap()[..],
            if link { LINK_NEW } else { META_NEW }
        );
        assert_eq!(
            image.file_header().backing_file.as_deref(),
            Some(if link { "base-b.obk" } else { "base-a.obk" })
        );
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
fn vm_power_cut_published_metadata_and_parent_link() {
    let root =
        std::path::PathBuf::from(std::env::var_os("OWN_BACKUP_H03_META_ROOT").expect("root"));
    let name = std::env::var("OWN_BACKUP_H03_META_POINT").expect("case");
    let (point, link, legacy) = case(&name);
    let leaf = root.join("leaf.obk");
    match std::env::var("OWN_BACKUP_H03_META_PHASE").as_deref() {
        Ok("prepare") => {
            std::fs::create_dir(&root).unwrap();
            let f = prepare(&root, legacy);
            let image = ObkImage::open_path(&leaf, &f.chain).unwrap();
            let before = serde_json::json!({"point":name,"legacy":legacy,"leaf_sha256":hash(&leaf),
                "chain_sha256":digest(&leaf,&f.chain),"generation":image.open_header().header_generation});
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(root.join("before.json"))
                .unwrap();
            file.write_all(&serde_json::to_vec_pretty(&before).unwrap())
                .unwrap();
            file.sync_all().unwrap();
            say(&format!("H03-METADATA-PREPARE-PASS {before}"));
        }
        Ok("cut") => {
            let before: serde_json::Value =
                serde_json::from_slice(&std::fs::read(root.join("before.json")).unwrap()).unwrap();
            let key = super::read_open_header(&leaf)
                .unwrap()
                .chain_key_with_password("пароль архива")
                .unwrap();
            assert_eq!(hash(&leaf), before["leaf_sha256"].as_str().unwrap());
            assert_eq!(
                digest(&leaf, &key),
                before["chain_sha256"].as_str().unwrap()
            );
            say(&format!("H03-BOUNDARY-BASELINE-PASS point={name}"));
            let chosen = name.clone();
            let _guard = observe(move |at, path| {
                if at == point {
                    say(&format!(
                        "H03-BOUNDARY-CUT {}",
                        serde_json::json!({"point":chosen,"candidate_sha256":hash(path)})
                    ));
                    loop {
                        std::thread::sleep(std::time::Duration::from_secs(1));
                    }
                }
            });
            update(&leaf, &key, link);
            panic!("checkpoint not reached");
        }
        Ok("verify") => {
            let before: serde_json::Value =
                serde_json::from_slice(&std::fs::read(root.join("before.json")).unwrap()).unwrap();
            let key = super::read_open_header(&leaf)
                .unwrap()
                .chain_key_with_password("пароль архива")
                .unwrap();
            let mut image = ObkImage::open_path(&leaf, &key).unwrap();
            let generation = image.open_header().header_generation;
            let meta = image.sealed_metadata().unwrap();
            let parent = image.file_header().backing_file.clone();
            drop(image);
            let old = meta.as_slice() == OLD;
            assert!(old || meta.as_slice() == if link { LINK_NEW } else { META_NEW });
            assert_eq!(
                parent.as_deref(),
                Some(if link && !old {
                    "base-b.obk"
                } else {
                    "base-a.obk"
                })
            );
            if point == Point::AppendedSynced || legacy {
                assert!(old);
            }
            if point == Point::HeaderSynced {
                assert!(!old);
            }
            if legacy {
                assert_eq!(hash(&leaf), before["leaf_sha256"].as_str().unwrap());
            } else {
                assert_eq!(
                    generation,
                    before["generation"].as_u64().unwrap() + u64::from(!old)
                );
            }
            assert_eq!(
                digest(&leaf, &key),
                before["chain_sha256"].as_str().unwrap()
            );
            let mut result = serde_json::json!({"point":name,"legacy":legacy,"old_point":old,"generation":generation,
                "parent":parent,"metadata":String::from_utf8_lossy(&meta),"chain_sha256":digest(&leaf,&key)});
            update(&leaf, &key, link);
            let mut image = ObkImage::open_path(&leaf, &key).unwrap();
            assert_eq!(
                &image.sealed_metadata().unwrap()[..],
                if link { LINK_NEW } else { META_NEW }
            );
            assert_eq!(
                image.file_header().backing_file.as_deref(),
                Some(if link { "base-b.obk" } else { "base-a.obk" })
            );
            drop(image);
            assert_eq!(
                digest(&leaf, &key),
                before["chain_sha256"].as_str().unwrap()
            );
            result["pass"] = true.into();
            result["retry_pass"] = true.into();
            result["clusters_checked"] = 64.into();
            std::fs::write(
                root.join("result.json"),
                serde_json::to_vec_pretty(&result).unwrap(),
            )
            .unwrap();
            say(&format!("H03-METADATA-RECOVER-PASS {result}"));
            say(&format!("H03-METADATA-VALIDATE-PASS point={name}"));
        }
        other => panic!("unknown phase {other:?}"),
    }
}
