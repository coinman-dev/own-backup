//! Test-only VM power cut after the product ImageTarget finalizer returns.
//! Inputs needed after crash travel through COM1/host CD, without another disk
//! write after finalization that could incidentally make the namespace durable.
use std::io::{Read, Write};
use std::path::Path;

use sha2::{Digest, Sha256};
use uuid::Uuid;

use super::ImageTarget;
use crate::seal::obk::container_tests::{cluster_bits, Fixture};
use crate::seal::obk::{apply_changed_with, compact_image, PreviousContents, RebuildOptions};
use crate::seal::obk::{read_open_header, ObkChain, ObkImage, ObkStagedWriter};
use crate::seal::ChainKey;
use crate::sidecar::ChainRole;

fn say(message: &str) {
    let mut port = std::fs::OpenOptions::new()
        .write(true)
        .open(r"\\.\COM1")
        .unwrap();
    writeln!(port, "{message}").unwrap();
    port.flush().unwrap();
}

fn file_hash(path: &Path) -> String {
    let mut file = std::fs::File::open(path).unwrap();
    let mut bytes = vec![0; 64 * 1024];
    let mut hash = Sha256::new();
    loop {
        let n = file.read(&mut bytes).unwrap();
        if n == 0 {
            break;
        }
        hash.update(&bytes[..n]);
    }
    format!("{:x}", hash.finalize())
}

fn check_chain(child: &Path, key: &ChainKey) -> String {
    let mut chain = ObkChain::open(child, key).unwrap();
    assert_eq!(chain.chain_len(), 2);
    let mut hash = Sha256::new();
    for n in 0..64 {
        let fill = match n {
            1 => 0x22,
            2 => 0x34,
            4 => 0x55,
            _ => 0,
        };
        let bytes = chain.read_cluster(n).unwrap();
        assert_eq!(&bytes[..], &vec![fill; 1 << cluster_bits()]);
        hash.update(&bytes[..]);
    }
    format!("{:x}", hash.finalize())
}

fn create_images(folder: &Path) -> Fixture {
    let fixture = Fixture::new();
    let base_id = Uuid::new_v4();
    let base = folder.join("base.obk");
    let mut target = ImageTarget::Obk {
        writer: Box::new(
            ObkStagedWriter::create(
                &base,
                &fixture.chain,
                fixture.options(base_id, ChainRole::Full, None),
            )
            .unwrap(),
        ),
    };
    for (n, fill) in [(0, 0x11u8), (2, 0x34), (4, 0x55)] {
        target
            .write_at(n << cluster_bits(), &vec![fill; 1 << cluster_bits()])
            .unwrap();
    }
    target.finalize(b"new base metadata", 4096).unwrap();
    let child = folder.join("child.obk");
    let mut target = ImageTarget::Obk {
        writer: Box::new(
            ObkStagedWriter::create(
                &child,
                &fixture.chain,
                fixture.options(
                    Uuid::new_v4(),
                    ChainRole::Incremental,
                    Some((base_id, "base.obk")),
                ),
            )
            .unwrap(),
        ),
    };
    target.write_at(0, &vec![0; 1 << cluster_bits()]).unwrap();
    target
        .write_at(1 << cluster_bits(), &vec![0x22; 1 << cluster_bits()])
        .unwrap();
    target.finalize(b"new child metadata", 4096).unwrap();
    fixture
}

#[test]
fn the_product_finalizer_closes_new_files_and_the_chain_is_readable() {
    let root = tempfile::tempdir().unwrap();
    let fixture = create_images(root.path());
    assert_eq!(
        check_chain(&root.path().join("child.obk"), &fixture.chain),
        "73731c34cfc500f2a3b86cbf172127168a4f63422c19d5bdb9760d6eaa3c2337"
    );
    let mut base = ObkImage::open_path(&root.path().join("base.obk"), &fixture.chain).unwrap();
    assert_eq!(&base.sealed_metadata().unwrap()[..], b"new base metadata");
}

#[test]
#[ignore = "requires disposable Windows VM and host-controlled power cut"]
fn vm_power_cut_after_new_images_finalize() {
    let root = std::path::PathBuf::from(
        std::env::var_os("OWN_BACKUP_H03_NAMESPACE_ROOT").expect("owned root"),
    );
    let point = std::env::var("OWN_BACKUP_H03_NAMESPACE_POINT").expect("point");
    assert!(matches!(
        point.as_str(),
        "namespace-existing-directory"
            | "namespace-new-directory"
            | "namespace-new-directory-compact"
    ));
    let existing = point == "namespace-existing-directory";
    let folder = root.join("images");
    let phase = std::env::var("OWN_BACKUP_H03_NAMESPACE_PHASE").expect("phase");
    match phase.as_str() {
        "prepare" => {
            std::fs::create_dir(&root).unwrap();
            if existing {
                std::fs::create_dir(&folder).unwrap();
            }
            let marker = if existing {
                folder.join("baseline.txt")
            } else {
                root.join("baseline.txt")
            };
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(marker)
                .unwrap();
            file.write_all(b"Directory baseline for clean shutdown")
                .unwrap();
            file.sync_all().unwrap();
            let manifest = serde_json::json!({"point":point,"images_dir_preexisting":existing});
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(root.join("before.json"))
                .unwrap();
            file.write_all(&serde_json::to_vec_pretty(&manifest).unwrap())
                .unwrap();
            file.sync_all().unwrap();
            say(&format!("H03-NAMESPACE-PREPARE-PASS {manifest}"));
        }
        "cut" => {
            let before: serde_json::Value =
                serde_json::from_slice(&std::fs::read(root.join("before.json")).unwrap()).unwrap();
            assert_eq!(before["point"], point);
            let marker = if existing {
                folder.join("baseline.txt")
            } else {
                root.join("baseline.txt")
            };
            assert_eq!(
                std::fs::read(marker).unwrap(),
                b"Directory baseline for clean shutdown"
            );
            if existing {
                assert!(folder.is_dir());
            } else {
                assert!(!folder.exists());
                std::fs::create_dir(&folder).unwrap();
            }
            assert!(!folder.join("base.obk").exists() && !folder.join("child.obk").exists());
            say(&format!("H03-BOUNDARY-BASELINE-PASS point={point}"));
            let fixture = create_images(&folder);
            if point == "namespace-new-directory-compact" {
                for _ in 0..3 {
                    apply_changed_with(
                        &folder.join("base.obk"),
                        &fixture.chain,
                        &[2],
                        |_| Ok(vec![0x34; 1 << cluster_bits()]),
                        RebuildOptions {
                            previous: PreviousContents::Keep,
                            ..Default::default()
                        },
                    )
                    .unwrap();
                }
                compact_image(&folder.join("base.obk"), &fixture.chain).unwrap();
            }
            let child = folder.join("child.obk");
            let report = serde_json::json!({"point":point,"finalize":"ImageTarget::finalize",
                "after_compaction":point=="namespace-new-directory-compact",
                "base_sha256":file_hash(&folder.join("base.obk")),"child_sha256":file_hash(&child),
                "chain_sha256":check_chain(&child,&fixture.chain),"images_dir_preexisting":existing});
            // These reads and COM1 output do not explicitly sync directory or
            // create a ready/result file on the tested disk after finalize.
            say(&format!("H03-BOUNDARY-CUT {report}"));
            loop {
                std::thread::sleep(std::time::Duration::from_secs(1));
            }
        }
        "verify" => {
            let expected: serde_json::Value =
                serde_json::from_slice(&std::fs::read(r"C:\Lab\namespace-expected.json").unwrap())
                    .unwrap();
            assert_eq!(expected["point"], point);
            let base = folder.join("base.obk");
            let child = folder.join("child.obk");
            let names = serde_json::json!({"point":point,"directory_exists":folder.is_dir(),
                "base_exists":base.is_file(),"child_exists":child.is_file()});
            say(&format!("H03-NAMESPACE-NAMES {names}"));
            assert!(
                base.is_file() && child.is_file(),
                "new image names disappeared: {names}"
            );
            assert_eq!(file_hash(&base), expected["base_sha256"].as_str().unwrap());
            assert_eq!(
                file_hash(&child),
                expected["child_sha256"].as_str().unwrap()
            );
            let key = read_open_header(&child)
                .unwrap()
                .chain_key_with_password("пароль архива")
                .unwrap();
            let hash = check_chain(&child, &key);
            assert_eq!(hash, expected["chain_sha256"].as_str().unwrap());
            let mut image = ObkImage::open_path(&base, &key).unwrap();
            assert_eq!(&image.sealed_metadata().unwrap()[..], b"new base metadata");
            drop(image);
            let mut image = ObkImage::open_path(&child, &key).unwrap();
            assert_eq!(&image.sealed_metadata().unwrap()[..], b"new child metadata");
            drop(image);
            let report = serde_json::json!({"pass":true,"point":point,"names":names,
                "base_sha256":file_hash(&base),"child_sha256":file_hash(&child),"chain_sha256":hash,
                "clusters_checked":64,"finalizer":"product ImageTarget::finalize"});
            std::fs::write(
                root.join("result.json"),
                serde_json::to_vec_pretty(&report).unwrap(),
            )
            .unwrap();
            say(&format!("H03-BOUNDARY-RECOVER-PASS {report}"));
            say(&format!("H03-NAMESPACE-VALIDATE-PASS point={point}"));
        }
        _ => panic!("unexpected namespace phase: {phase}"),
    }
}
