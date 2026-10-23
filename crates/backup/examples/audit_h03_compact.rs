//! Bounded real-record compaction/crash probe, only disposable fixtures.
use own_backup_backup::seal::context::ArchiveKeys;
use own_backup_backup::seal::keypair::{JobKeyPair, KdfParams, SealedPrivateKey};
use own_backup_backup::seal::obk::{
    apply_changed_with, compact_if_needed, compact_image_with_progress, ObkChain, ObkCompression,
    ObkImage, ObkWriter, ObkWriterOptions, PreviousContents, RebuildOptions,
};
use own_backup_backup::seal::{recovery::RecoveryKey, wrap::wrap_chain_key, ChainKey};
use own_backup_backup::sidecar::ChainRole;
use sha2::{Digest, Sha256};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use uuid::Uuid;
type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;
const PASSWORD: &str = "H03 synthetic compaction";
const COUNT: u64 = 2048;
fn say(s: impl AsRef<str>) {
    println!("{}", s.as_ref());
    let _ = std::io::stdout().flush();
}
fn file_hash(p: &Path) -> Result<String> {
    let mut f = std::fs::File::open(p)?;
    let mut h = Sha256::new();
    let mut b = vec![0; 1 << 20];
    loop {
        let n = f.read(&mut b)?;
        if n == 0 {
            break;
        }
        h.update(&b[..n]);
    }
    Ok(format!("{:x}", h.finalize()))
}
fn digest(p: &Path, k: &ChainKey) -> Result<String> {
    let mut c = ObkChain::open(p, k)?;
    let mut h = Sha256::new();
    for n in 0..COUNT {
        h.update(&c.read_cluster(n)?[..]);
    }
    Ok(format!("{:x}", h.finalize()))
}
fn key(p: &Path) -> Result<ChainKey> {
    Ok(ArchiveKeys::from_image_password(p, PASSWORD)?
        .open_chain_of(p)?
        .key)
}
fn noise(seed: u64) -> Vec<u8> {
    let mut s = seed | 1;
    let mut out = vec![0; 1 << 16];
    for w in out.chunks_exact_mut(8) {
        s ^= s << 13;
        s ^= s >> 7;
        s ^= s << 17;
        w.copy_from_slice(&s.to_le_bytes());
    }
    out
}
fn prepare(folder: &Path) -> Result<()> {
    std::fs::create_dir(folder)?;
    let chain = ChainKey::generate()?;
    let pair = JobKeyPair::generate()?;
    let chain_id = Uuid::new_v4();
    let base_id = Uuid::new_v4();
    let envelope = SealedPrivateKey::seal(
        "H03 compaction fixture",
        &pair,
        PASSWORD,
        &RecoveryKey::generate()?,
        &KdfParams::strong(),
    )?;
    let opts = |id, parent: bool| -> Result<ObkWriterOptions> {
        Ok(ObkWriterOptions {
            chain_id,
            backup_id: id,
            parent_backup_id: parent.then_some(base_id),
            role: if parent {
                ChainRole::Incremental
            } else {
                ChainRole::Full
            },
            created_at: "2026-10-04T00:00:00Z".into(),
            virtual_size: COUNT << 16,
            cluster_bits: 16,
            compression: ObkCompression::None,
            backing_file: parent.then(|| "base.obk".into()),
            key_file: envelope.clone(),
            wrapped_chain_key: wrap_chain_key(&pair.public, chain_id.as_bytes(), &chain)?,
            source_id: Some("H03 real records".into()),
            consolidated_increments: 0,
            chain_started_at: None,
        })
    };
    let base = folder.join("base.obk");
    let child = folder.join("child.obk");
    let mut w = ObkWriter::create(&base, &chain, opts(base_id, false)?)?;
    for n in 0..COUNT {
        w.write_cluster(n, &noise(n + 1000))?;
    }
    w.finish_with_reserve(b"real-record metadata", 4096)?
        .into_inner()?
        .sync_all()?;
    for step in 0..3 {
        let changed: Vec<_> = (1..COUNT / 2).collect();
        apply_changed_with(
            &base,
            &chain,
            &changed,
            |n| Ok(noise(n + 10000 + step * COUNT)),
            RebuildOptions {
                previous: PreviousContents::Keep,
                ..Default::default()
            },
        )?;
    }
    let mut w = ObkWriter::create(&child, &chain, opts(Uuid::new_v4(), true)?)?;
    w.write_zero_cluster(0)?;
    w.write_cluster(1, &noise(99999))?;
    w.finish(b"child metadata")?.into_inner()?.sync_all()?;
    let image = ObkImage::open_path(&base, &chain)?;
    let report = serde_json::json!({"file_sha256":file_hash(&base)?,"chain_sha256":digest(&child,&chain)?,"base_bytes":std::fs::metadata(&base)?.len(),"generation":image.open_header().header_generation,"real_records":COUNT,"logical_data_bytes":COUNT<<16});
    std::fs::write(
        folder.join("before.json"),
        serde_json::to_vec_pretty(&report)?,
    )?;
    say(format!("H03-COMPACT-PREPARED {report}"));
    Ok(())
}
fn check(folder: &Path, published: bool) -> Result<()> {
    let base = folder.join("base.obk");
    let child = folder.join("child.obk");
    let k = key(&base)?;
    let r: serde_json::Value = serde_json::from_slice(&std::fs::read(folder.join("before.json"))?)?;
    if digest(&child, &k)? != r["chain_sha256"].as_str().ok_or("missing digest")? {
        return Err("chain bytes changed".into());
    }
    let hash = file_hash(&base)?;
    if !published && hash != r["file_sha256"].as_str().ok_or("missing hash")? {
        return Err("original modified before publication".into());
    }
    if published
        && std::fs::metadata(&base)?.len() >= r["base_bytes"].as_u64().ok_or("missing size")?
    {
        return Err("published file did not shrink".into());
    }
    say(format!(
        "H03-COMPACT-RECOVER-PASS published={published} chain_sha256={}",
        r["chain_sha256"]
    ));
    Ok(())
}
fn crash(folder: &Path, phase: &str) -> Result<()> {
    let base = folder.join("base.obk");
    let k = key(&base)?;
    compact_image_with_progress(&base, &k, |done, total| {
        if total > 0
            && ((phase == "copy" && done == 1)
                || (phase == "verify" && done == total / 2 + 1)
                || (phase == "publish" && done == total))
        {
            say(format!(
                "H03-COMPACT-EXIT phase={phase} done={done} total={total}"
            ));
            std::process::exit(23);
        }
        Ok(())
    })?;
    if phase == "published" {
        std::process::exit(23)
    }
    Err("requested crash phase not reached".into())
}
fn run(folder: &Path) -> Result<()> {
    let base = folder.join("base.obk");
    let child = folder.join("child.obk");
    let k = key(&base)?;
    let before = file_hash(&base)?;
    let mut source = ObkImage::open_path(&base, &k)?;
    let states = source.defined_cluster_states()?;
    let meta = source.sealed_metadata()?;
    drop(source);
    for phase in ["copy", "verify"] {
        let error = compact_image_with_progress(&base, &k, |done, total| {
            if total > 0
                && ((phase == "copy" && done == 1) || (phase == "verify" && done == total / 2 + 1))
            {
                Err(own_backup_common::OwnBackupError::Cancelled)
            } else {
                Ok(())
            }
        })
        .unwrap_err();
        if !matches!(error, own_backup_common::OwnBackupError::Cancelled)
            || file_hash(&base)? != before
        {
            return Err("cancellation changed source".into());
        }
        say(format!("H03-COMPACT-CANCEL-PASS phase={phase}"));
    }
    let expected = digest(&child, &k)?;
    let stats = compact_if_needed(&base, &k)?.ok_or("cleanup policy did not trigger")?;
    if stats.reclaimed_bytes() < 64 << 20 || digest(&child, &k)? != expected {
        return Err("reclaim/chain contents incorrect".into());
    }
    let mut image = ObkImage::open_path(&base, &k)?;
    if image.defined_cluster_states()? != states || image.sealed_metadata()? != meta {
        return Err("data identities/metadata changed".into());
    }
    drop(image);
    if compact_if_needed(&base, &k)?.is_some() {
        return Err("clean file compacted again".into());
    }
    let report = serde_json::json!({"pass":true,"before_bytes":stats.before_bytes,"after_bytes":stats.after_bytes,"reclaimed_bytes":stats.reclaimed_bytes(),"records_copied":stats.records_copied,"chain_sha256":expected,"after_file_sha256":file_hash(&base)?});
    std::fs::write(
        folder.join("result.json"),
        serde_json::to_vec_pretty(&report)?,
    )?;
    say(format!("H03-COMPACT-PASS {report}"));
    Ok(())
}
#[cfg(windows)]
fn ntfs(image: &Path, target: u32) -> Result<()> {
    use own_backup_backup::engine_win::{restore_disk, verify_image_with_keys, DiskRestoreRequest};
    let keys = ArchiveKeys::from_image_password(image, "лабораторный пароль архива")?;
    let k = keys.open_chain_of(image)?.key;
    let mut source = ObkImage::open_path(image, &k)?;
    let meta = source.sealed_metadata()?;
    drop(source);
    for _ in 0..16 {
        own_backup_backup::seal::obk::update_sealed_metadata(image, &k, &meta)?;
    }
    let stats = own_backup_backup::seal::obk::compact_image(image, &k)?;
    let v = verify_image_with_keys(image, Some(&keys), |_, _| Ok(()))?;
    if !v.valid() {
        return Err("compacted NTFS image invalid".into());
    }
    let r = restore_disk(
        &DiskRestoreRequest {
            manifest_path: image.to_path_buf(),
            target_disk: target,
            verify_checksums: true,
            encryption: Some(keys),
            passphrases: Default::default(),
        },
        |_| Ok(()),
    )?;
    say(format!(
        "H03-COMPACT-NTFS-PASS before={} after={} blocks={} partitions={} bytes={}",
        stats.before_bytes, stats.after_bytes, v.blocks_checked, r.partitions, r.bytes_written
    ));
    Ok(())
}
fn main() -> Result<()> {
    let args: Vec<_> = std::env::args().collect();
    let folder = PathBuf::from(args.get(2).ok_or("mode/path required")?);
    match args.get(1).map(String::as_str) {
        Some("prepare") => prepare(&folder),
        Some("run") => run(&folder),
        Some("crash") => crash(&folder, args.get(3).ok_or("phase required")?),
        Some("check") => check(&folder, args.get(3).is_some_and(|s| s == "published")),
        #[cfg(windows)]
        Some("ntfs") => ntfs(&folder, args.get(3).ok_or("target required")?.parse()?),
        _ => Err("prepare|run|crash|check|ntfs path [phase|target]".into()),
    }
}
