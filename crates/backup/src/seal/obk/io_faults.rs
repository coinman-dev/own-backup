//! Real device I/O faults from QEMU, not errors returned by a mocked writer.
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use super::{apply_changed_with, data_faults, ObkImage, PreviousContents, RebuildOptions};
use crate::seal::ChainKey;

fn port() -> std::fs::File {
    std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(r"\\.\COM1")
        .unwrap()
}
fn say(text: &str) {
    let mut p = port();
    writeln!(p, "{text}").unwrap();
    p.flush().unwrap();
}
fn gate(label: &str) {
    let mut p = port();
    writeln!(p, "H03-IO-{label}").unwrap();
    p.flush().unwrap();
    let mut byte = [0u8; 1];
    loop {
        match p.read(&mut byte) {
            Ok(1) if byte[0] == b'G' => break,
            Ok(_) => std::thread::sleep(std::time::Duration::from_millis(50)),
            Err(e) => panic!("serial host gate: {e}"),
        }
    }
}
fn update(base: &Path, key: &ChainKey) -> own_backup_common::Result<super::RebuildStats> {
    apply_changed_with(
        base,
        key,
        &[2, 4, 6],
        |n| {
            Ok(vec![
                match n {
                    2 => 0xa2,
                    4 => 0,
                    6 => 0xa6,
                    _ => unreachable!(),
                };
                4096
            ])
        },
        RebuildOptions {
            previous: PreviousContents::Keep,
            metadata: Some(b"data=new"),
            ..Default::default()
        },
    )
}
#[test]
#[ignore = "requires isolated disposable NTFS disk and QEMU blkdebug host control"]
fn vm_device_io_errors_and_inflight_cut() {
    let root = PathBuf::from(std::env::var_os("OWN_BACKUP_H03_IO_ROOT").expect("root"));
    let evidence = PathBuf::from(
        std::env::var_os("OWN_BACKUP_H03_IO_EVIDENCE").expect("system-disk evidence"),
    );
    let name = std::env::var("OWN_BACKUP_H03_IO_CASE").expect("case");
    assert!(["write-eio", "flush-eio", "inflight-cut"].contains(&name.as_str()));
    let base = root.join("base.obk");
    let leaf = root.join("leaf.obk");
    match std::env::var("OWN_BACKUP_H03_IO_PHASE").as_deref() {
        Ok("prepare") => {
            std::fs::create_dir(&root).unwrap();
            let f = data_faults::prepare(&root, "keep-first-record");
            let before = data_faults::before(&root, &f.chain);
            let mut file = std::fs::File::create_new(evidence.join("before.json")).unwrap();
            file.write_all(&serde_json::to_vec_pretty(&before).unwrap())
                .unwrap();
            file.sync_all().unwrap();
            say(&format!("H03-IO-PREPARE-PASS {before}"));
        }
        Ok("io") => {
            let expected: serde_json::Value =
                serde_json::from_slice(&std::fs::read(evidence.join("before.json")).unwrap())
                    .unwrap();
            let key = super::read_open_header(&base)
                .unwrap()
                .chain_key_with_password("пароль архива")
                .unwrap();
            assert_eq!(data_faults::before(&root, &key), expected);
            say("H03-IO-BASELINE-PASS");
            // Host switches only the dedicated target disk after this readiness.
            // No filesystem writes follow READY until the host acknowledges.
            gate("READY");
            say("H03-IO-START");
            let result = update(&base, &key);
            let outcome = serde_json::json!({"case":name,"operation_ok":result.is_ok(),"error":result.err().map(|e|e.to_string())});
            say(&format!("H03-IO-OPERATION-DONE {outcome}"));
            // Disable the fault layer before touching data/evidence or shutting
            // down. Save outcome on C:, never on the failing target volume.
            gate("DISARM");
            let mut file = std::fs::File::create(evidence.join("io-outcome.json")).unwrap();
            file.write_all(&serde_json::to_vec_pretty(&outcome).unwrap())
                .unwrap();
            file.sync_all().unwrap();
            say("H03-IO-IO-PHASE-PASS");
        }
        Ok("verify") => {
            let expected: serde_json::Value =
                serde_json::from_slice(&std::fs::read(evidence.join("before.json")).unwrap())
                    .unwrap();
            let key = super::read_open_header(&base)
                .unwrap()
                .chain_key_with_password("пароль архива")
                .unwrap();
            let mut image = ObkImage::open_path(&base, &key).unwrap();
            let metadata = image.sealed_metadata().unwrap();
            let updated = metadata.as_slice() == b"data=new";
            assert!(updated || metadata.as_slice() == b"data=old");
            let generation = image.open_header().header_generation;
            assert_eq!(
                generation,
                expected["generation"].as_u64().unwrap() + u64::from(updated)
            );
            assert_eq!(
                serde_json::to_value(image.open_header().backup_id).unwrap(),
                expected["backup_id"]
            );
            assert_eq!(
                serde_json::to_value(image.open_header().chain_id).unwrap(),
                expected["chain_id"]
            );
            drop(image);
            let before_retry = data_faults::digest(&leaf, &key, updated);
            use sha2::{Digest, Sha256};
            let leaf_hash = serde_json::json!(format!(
                "{:x}",
                Sha256::digest(std::fs::read(&leaf).unwrap())
            ));
            assert_eq!(leaf_hash, expected["leaf_sha256"]);
            update(&base, &key).unwrap();
            let after = data_faults::digest(&leaf, &key, true);
            let mut image = ObkImage::open_path(&base, &key).unwrap();
            assert_eq!(&image.sealed_metadata().unwrap()[..], b"data=new");
            assert_eq!(image.open_header().header_generation, generation + 1);
            let result = serde_json::json!({"case":name,"pass":true,"retry_pass":true,"clusters_checked":64,"recovered_updated":updated,
                "generation_before_retry":generation,"generation_after_retry":generation+1,"chain_sha256_before_retry":before_retry,"chain_sha256_after_retry":after});
            std::fs::write(
                evidence.join("result.json"),
                serde_json::to_vec_pretty(&result).unwrap(),
            )
            .unwrap();
            say(&format!("H03-IO-VERIFY-PASS {result}"));
        }
        _ => panic!("expected prepare/io/verify"),
    }
}
