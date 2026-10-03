//! Bounded index and append-growth acceptance. Explicit zeros are NOT dense data.
use own_backup_backup::seal::keypair::{JobKeyPair, KdfParams, SealedPrivateKey};
use own_backup_backup::seal::obk::{
    apply_changed_with, update_sealed_metadata, ClusterState, ObkChain, ObkCompression, ObkImage,
    ObkWriter, ObkWriterOptions, PreviousContents, RebuildOptions, SECOND_HEADER_OFFSET,
};
use own_backup_backup::seal::recovery::RecoveryKey;
use own_backup_backup::seal::wrap::wrap_chain_key;
use own_backup_backup::seal::ChainKey;
use own_backup_backup::sidecar::ChainRole;
use std::io::{Seek, SeekFrom, Write};
use std::path::Path;
use std::time::Instant;
use uuid::Uuid;
type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

fn check(path: &Path, key: &ChainKey, entries: u64, fill: u8) -> Result<()> {
    let mut image = ObkImage::open_path(path, key)?;
    let states = image.defined_cluster_states()?;
    if states.len() as u64 != entries {
        return Err("defined index count changed".into());
    }
    for (n, state) in states {
        if n < entries - 1 {
            if !matches!(state, ClusterState::Zero)
                || image.raw_l2_entry(n)? != own_backup_backup::qcow2::OFLAG_ZERO
            {
                return Err("zero index/L2 disagrees".into());
            }
        } else if n != entries - 1 || !matches!(state, ClusterState::Present(_)) {
            return Err("nonzero index entry changed".into());
        }
    }
    drop(image);
    let mut chain = ObkChain::open(path, key)?;
    if chain.read_cluster(0)?.iter().any(|b| *b != 0)
        || chain.read_cluster(entries - 1)?.iter().any(|b| *b != fill)
        || chain.read_cluster(entries)?.iter().any(|b| *b != 0x6e)
    {
        return Err("zero/current/inherited bytes differ".into());
    }
    Ok(())
}

fn main() -> Result<()> {
    let args: Vec<_> = std::env::args().collect();
    if args.len() != 3 {
        return Err("usage: audit_h03_index_growth <new folder> <65536|262144|1048576>".into());
    }
    let count: u64 = args[2].parse()?;
    if ![1 << 16, 1 << 18, 1 << 20].contains(&count) {
        return Err("bounded entry count required".into());
    }
    let folder = Path::new(&args[1]);
    std::fs::create_dir(folder)?;
    let started = Instant::now();
    let chain = ChainKey::generate()?;
    let pair = JobKeyPair::generate()?;
    let envelope = SealedPrivateKey::seal(
        "H03 index fixture",
        &pair,
        "synthetic fixture",
        &RecoveryKey::generate()?,
        &KdfParams::strong(),
    )?;
    let chain_id = Uuid::new_v4();
    let base_id = Uuid::new_v4();
    let options = |id, parent: bool| -> Result<ObkWriterOptions> {
        Ok(ObkWriterOptions {
            chain_id,
            backup_id: id,
            parent_backup_id: parent.then_some(base_id),
            role: if parent {
                ChainRole::Incremental
            } else {
                ChainRole::Full
            },
            created_at: "2026-10-03T00:00:00Z".into(),
            virtual_size: (count + 1) << 16,
            cluster_bits: 16,
            compression: ObkCompression::None,
            backing_file: parent.then(|| "base.obk".into()),
            key_file: envelope.clone(),
            wrapped_chain_key: wrap_chain_key(&pair.public, chain_id.as_bytes(), &chain)?,
            source_id: Some("H03 synthetic index".into()),
            consolidated_increments: 0,
            chain_started_at: None,
        })
    };
    let base = folder.join("base.obk");
    let child = folder.join("index.obk");
    let mut writer = ObkWriter::create(&base, &chain, options(base_id, false)?)?;
    writer.write_cluster(0, &vec![0x7b; 1 << 16])?;
    writer.write_cluster(count, &vec![0x6e; 1 << 16])?;
    writer.finish(b"base")?.into_inner()?.sync_all()?;
    let mut writer = ObkWriter::create(&child, &chain, options(Uuid::new_v4(), true)?)?;
    for n in 0..count - 1 {
        writer.write_zero_cluster(n)?;
    }
    writer.write_cluster(count - 1, &vec![0x42; 1 << 16])?;
    writer
        .finish_with_reserve(b"initial", 4096)?
        .into_inner()?
        .sync_all()?;
    check(&child, &chain, count, 0x42)?;
    let initial = std::fs::metadata(&child)?.len();
    let mut updates = Vec::new();
    for n in 1..=32 {
        let metadata = format!("metadata generation {n}");
        update_sealed_metadata(&child, &chain, metadata.as_bytes())?;
        let mut opened = ObkImage::open_path(&child, &chain)?;
        if &opened.sealed_metadata()?[..] != metadata.as_bytes() {
            return Err("new metadata not published".into());
        }
        updates.push(serde_json::json!({"iteration":n,"bytes":std::fs::metadata(&child)?.len(),"generation":opened.open_header().header_generation}));
    }
    check(&child, &chain, count, 0x42)?;
    let mut rebuilds = Vec::new();
    for step in 1..=3 {
        let before = std::fs::metadata(&child)?.len();
        let stats = apply_changed_with(
            &child,
            &chain,
            &[count - 1],
            |_| Ok(vec![0x42 + step; 1 << 16]),
            RebuildOptions {
                metadata: Some(format!("rebuild {step}").as_bytes()),
                previous: PreviousContents::Keep,
                ..Default::default()
            },
        )?;
        let after = std::fs::metadata(&child)?.len();
        if after <= before || stats.bytes_orphaned == 0 || stats.defined_clusters != count {
            return Err("append accounting incorrect".into());
        }
        check(&child, &chain, count, 0x42 + step)?;
        rebuilds.push(serde_json::json!({"step":step,"before":before,"after":after,"bytes_orphaned":stats.bytes_orphaned,"defined_clusters":stats.defined_clusters}));
    }
    let current = ObkImage::open_path(&child, &chain)?
        .open_header()
        .header_generation;
    let fallback = folder.join("torn-copy.obk");
    std::fs::copy(&child, &fallback)?;
    let mut file = std::fs::OpenOptions::new().write(true).open(&fallback)?;
    file.seek(SeekFrom::Start(if current.is_multiple_of(2) {
        0
    } else {
        SECOND_HEADER_OFFSET
    }))?;
    file.write_all(&[0; 512])?;
    file.sync_all()?;
    drop(file);
    let mut image = ObkImage::open_path(&fallback, &chain)?;
    let recovered = image.open_header().header_generation;
    if recovered + 1 != current || &image.sealed_metadata()?[..] != b"rebuild 2" {
        return Err("previous generation lost during repeated updates".into());
    }
    drop(image);
    // Same backing directory; all old zero mappings and the former data record survive.
    check(&fallback, &chain, count, 0x44)?;
    let report = serde_json::json!({"pass":true,"index_entries":count,"zero_entries":count-1,
        "present_data_records":1,"virtual_bytes":(count+1)<<16,"initial_image_bytes":initial,
        "metadata_updates":updates,"rebuilds":rebuilds,"final_image_bytes":std::fs::metadata(&child)?.len(),
        "current_generation":current,"recovered_generation":recovered,"authenticated_index_and_l2_checked":count,
        "automatic_compaction":false,"dense_data_test":false,"seconds":started.elapsed().as_secs_f64()});
    std::fs::write(
        folder.join("result.json"),
        serde_json::to_vec_pretty(&report)?,
    )?;
    println!("H03-INDEX-PASS {report}");
    Ok(())
}
