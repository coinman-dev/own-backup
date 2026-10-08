//! Real process termination and NTFS exhaustion with production OBK/scratch APIs.
use own_backup_backup::overlay::{SealedSlots, SlotStore};
use own_backup_backup::seal::context::ArchiveKeys;
use own_backup_backup::seal::keypair::{JobKeyPair, KdfParams, SealedPrivateKey};
use own_backup_backup::seal::obk::{ObkChain, ObkCompression, ObkWriter, ObkWriterOptions};
use own_backup_backup::seal::{recovery::RecoveryKey, wrap::wrap_chain_key, ChainKey};
use own_backup_backup::sidecar::ChainRole;
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::fs::{self, File, OpenOptions};
use std::io::{Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};
use uuid::Uuid;

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;
const PASSWORD: &str = "OBK interruption synthetic fixture";
const CLUSTER: usize = 65536;
const COUNT: u64 = 64;
const DATA_START: usize = 4 * 1024 * 1024;

fn check(ok: bool, message: &str) -> Result<()> {
    if ok {
        Ok(())
    } else {
        Err(message.into())
    }
}

fn hash(path: &Path) -> Result<String> {
    Ok(format!("{:x}", Sha256::digest(fs::read(path)?)))
}

fn data(index: u64) -> Vec<u8> {
    vec![index as u8 + 33; CLUSTER]
}

fn options(base: &Path, key: &ChainKey) -> Result<ObkWriterOptions> {
    let header = own_backup_backup::seal::obk::read_open_header(base)?;
    let public = header.key_file.public_key()?;
    Ok(ObkWriterOptions {
        chain_id: header.chain_id,
        backup_id: Uuid::new_v4(),
        parent_backup_id: Some(header.backup_id),
        role: ChainRole::Incremental,
        created_at: "2026-10-08T00:00:00Z".into(),
        virtual_size: COUNT * CLUSTER as u64,
        cluster_bits: 16,
        compression: ObkCompression::None,
        backing_file: Some("base.obk".into()),
        key_file: header.key_file,
        wrapped_chain_key: wrap_chain_key(&public, header.chain_id.as_bytes(), key)?,
        source_id: Some("disposable interruption fixture".into()),
        consolidated_increments: 0,
        chain_started_at: None,
    })
}

fn key(base: &Path) -> Result<ChainKey> {
    Ok(ArchiveKeys::from_image_password(base, PASSWORD)?
        .open_chain_of(base)?
        .key)
}

fn pause(ready: &Path, phase: &str) -> Result<()> {
    let mut file = File::create_new(ready)?;
    file.write_all(phase.as_bytes())?;
    file.sync_all()?;
    // Only the parent can release this checkpoint, by terminating the process.
    loop {
        std::thread::sleep(Duration::from_secs(1));
    }
}

struct ObservedFile {
    file: File,
    phase: Option<String>,
    ready: PathBuf,
}

impl Write for ObservedFile {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        let at = self.file.stream_position()?;
        let written = self.file.write(bytes)?;
        if at >= DATA_START as u64
            && bytes.len() >= 84
            && &bytes[..4] == b"OBKR"
            && written == bytes.len()
        {
            let index = u64::from_be_bytes(bytes[24..32].try_into().unwrap());
            let phase = match (bytes[6], index) {
                (1, _) => "data",
                (3, _) => "index",
                (2, 0) => "root",
                (2, _) => "metadata",
                _ => "unknown",
            };
            if self.phase.as_deref() == Some(phase) {
                self.file.sync_all()?;
                pause(&self.ready, phase)
                    .map_err(|error| std::io::Error::other(error.to_string()))?;
            }
        }
        Ok(written)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.file.flush()
    }
}
impl Seek for ObservedFile {
    fn seek(&mut self, from: SeekFrom) -> std::io::Result<u64> {
        self.file.seek(from)
    }
}

fn write_image(base: &Path, candidate: &Path, phase: Option<&str>, ready: &Path) -> Result<()> {
    let key = key(base)?;
    let file = ObservedFile {
        file: File::create_new(candidate)?,
        phase: phase.map(str::to_owned),
        ready: ready.to_path_buf(),
    };
    let mut writer = ObkWriter::new(file, &key, options(base, &key)?)?;
    for index in 0..COUNT {
        writer.write_cluster(index, &data(index + 64))?;
    }
    writer
        .finish(b"interruption-fixture-metadata")?
        .file
        .sync_all()?;
    Ok(())
}

fn records(path: &Path, start: usize) -> Result<HashSet<Vec<u8>>> {
    let bytes = fs::read(path)?;
    check(
        bytes.len() <= 16 * 1024 * 1024,
        "fixture exceeds bounded scan size",
    )?;
    let mut records = HashSet::new();
    let mut at = start;
    while at + 84 <= bytes.len() {
        if &bytes[at..at + 4] == b"OBKR" {
            let len = own_backup_backup::seal::cluster::framed_len(&bytes[at..])?;
            if at + len > bytes.len() {
                break;
            }
            let mut id = vec![bytes[at + 6]];
            id.extend_from_slice(&bytes[at + 40..at + 84]);
            check(records.insert(id), "encryption identity reused within file")?;
            at += len;
        } else {
            at += 1;
        }
    }
    Ok(records)
}

fn check_fresh(before: &HashSet<Vec<u8>>, after: &HashSet<Vec<u8>>) -> Result<()> {
    check(
        before.is_disjoint(after),
        "retry reused encryption identities",
    )?;
    let sessions = |records: &HashSet<Vec<u8>>| {
        records
            .iter()
            .map(|id| id[..33].to_vec())
            .collect::<HashSet<_>>()
    };
    check(
        sessions(before).is_disjoint(&sessions(after)),
        "retry reused a writer session",
    )
}

fn verify(base: &Path, leaf: &Path, updated: bool) -> Result<String> {
    let key = key(base)?;
    let mut chain = ObkChain::open(leaf, &key)?;
    let mut digest = Sha256::new();
    for index in 0..COUNT {
        let actual = chain.read_cluster(index)?;
        check(
            actual.as_slice() == data(index + if updated { 64 } else { 0 }),
            "reopened chain bytes differ",
        )?;
        digest.update(&actual[..]);
    }
    Ok(format!("{:x}", digest.finalize()))
}

fn killed_child(root: &Path, mode: &str, phase: &str, candidate: &Path) -> Result<()> {
    let ready = root.join(format!("{phase}.ready"));
    let mut child = Command::new(std::env::current_exe()?)
        .args([
            mode,
            root.to_str().ok_or("root path")?,
            phase,
            candidate.to_str().ok_or("candidate path")?,
        ])
        .stdout(File::create(root.join(format!("{phase}.out")))?)
        .stderr(File::create(root.join(format!("{phase}.err")))?)
        .stdin(Stdio::null())
        .spawn()?;
    let deadline = Instant::now() + Duration::from_secs(90);
    while !ready.exists() {
        if let Some(status) = child.try_wait()? {
            return Err(format!("worker exited before {phase}: {status}").into());
        }
        if Instant::now() > deadline {
            child.kill()?;
            child.wait()?;
            return Err(format!("worker checkpoint {phase} timed out").into());
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    check(
        fs::read_to_string(ready)? == phase,
        "wrong child checkpoint",
    )?;
    child.kill()?;
    check(
        !child.wait()?.success(),
        "worker was not forcibly terminated",
    )?;
    Ok(())
}

fn prepare(root: &Path) -> Result<()> {
    fs::create_dir(root)?;
    let pair = JobKeyPair::generate()?;
    let recovery = RecoveryKey::generate()?;
    let key = ChainKey::generate()?;
    let chain_id = Uuid::new_v4();
    let envelope = SealedPrivateKey::seal(
        "interruption-fixture",
        &pair,
        PASSWORD,
        &recovery,
        &KdfParams::strong(),
    )?;
    let opts = ObkWriterOptions {
        chain_id,
        backup_id: Uuid::new_v4(),
        parent_backup_id: None,
        role: ChainRole::Full,
        created_at: "2026-10-08T00:00:00Z".into(),
        virtual_size: COUNT * CLUSTER as u64,
        cluster_bits: 16,
        compression: ObkCompression::None,
        backing_file: None,
        key_file: envelope,
        wrapped_chain_key: wrap_chain_key(&pair.public, chain_id.as_bytes(), &key)?,
        source_id: Some("disposable interruption fixture".into()),
        consolidated_increments: 0,
        chain_started_at: None,
    };
    let base = root.join("base.obk");
    let mut writer = ObkWriter::create(&base, &key, opts)?;
    for index in 0..COUNT {
        writer.write_cluster(index, &data(index))?;
    }
    writer
        .finish(b"baseline-metadata")?
        .into_inner()?
        .sync_all()?;
    check(
        !verify(&base, &base, false)?.is_empty(),
        "baseline unreadable",
    )?;
    Ok(())
}

fn scratch_child(root: &Path, path: &Path) -> Result<()> {
    let base = root.join("base.obk");
    let key = key(&base)?;
    let chain = own_backup_backup::seal::obk::read_open_header(&base)?.chain_id;
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(path)?;
    let sync = file.try_clone()?;
    let mut scratch = SealedSlots::new(file, CLUSTER as u64, &key, *chain.as_bytes())?;
    scratch.write_in_slot(0, 2, 0, &data(200))?;
    sync.sync_all()?;
    pause(&root.join("scratch.ready"), "scratch")
}

fn run(root: &Path, full_volume: &Path) -> Result<()> {
    check(
        full_volume.join("obk-exhaustion.marker").exists(),
        "dedicated exhaustion marker missing",
    )?;
    prepare(root)?;
    let base = root.join("base.obk");
    let baseline = hash(&base)?;
    let baseline_data = verify(&base, &base, false)?;
    let mut reports = Vec::new();
    for phase in ["data", "index", "metadata", "root"] {
        let candidate = root.join(format!("{phase}.obk"));
        killed_child(root, "child-image", phase, &candidate)?;
        let chain_key = key(&base)?;
        check(
            ObkChain::open(&candidate, &chain_key).is_err(),
            "unfinished image accepted as a restore point",
        )?;
        let interrupted = records(&candidate, DATA_START)?;
        check(
            !interrupted.is_empty(),
            "no persisted encrypted records at checkpoint",
        )?;
        check(
            hash(&base)? == baseline && verify(&base, &base, false)? == baseline_data,
            "kill modified published parent",
        )?;
        let retry = root.join(format!("{phase}-retry.obk"));
        write_image(&base, &retry, None, Path::new("unused"))?;
        let fresh = records(&retry, DATA_START)?;
        check_fresh(&interrupted, &fresh)?;
        let restored = verify(&base, &retry, true)?;
        reports.push(serde_json::json!({"case":phase,"pass":true,"process_killed":true,"unfinished_refused":true,"parent_unchanged":true,"fresh_record_identities":true,"fresh_writer_sessions":true,"interrupted_records":interrupted.len(),"retry_records":fresh.len(),"control_sha256":restored}));
        fs::remove_file(candidate)?;
        fs::remove_file(retry)?;
    }
    let scratch_path = root.join("sealed.scratch");
    killed_child(root, "child-scratch", "scratch", &scratch_path)?;
    let interrupted = records(&scratch_path, 0)?;
    let scratch_bytes = fs::read(&scratch_path)?;
    let plaintext = data(200);
    check(
        !scratch_bytes
            .windows(128)
            .any(|window| window == &plaintext[..128]),
        "plaintext found in scratch",
    )?;
    let chain_key = key(&base)?;
    let chain = own_backup_backup::seal::obk::read_open_header(&base)?.chain_id;
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&scratch_path)?;
    let sync = file.try_clone()?;
    let mut scratch = SealedSlots::new(file, CLUSTER as u64, &chain_key, *chain.as_bytes())?;
    let mut reopened = vec![0; CLUSTER];
    check(
        scratch.read_in_slot(0, 2, 0, &mut reopened).is_err(),
        "old scratch trusted without in-memory identity",
    )?;
    scratch.write_in_slot(0, 2, 0, &data(201))?;
    scratch.read_in_slot(0, 2, 0, &mut reopened)?;
    check(reopened == data(201), "scratch retry bytes differ")?;
    sync.sync_all()?;
    drop(scratch);
    drop(sync);
    check_fresh(&interrupted, &records(&scratch_path, 0)?)?;
    check(hash(&base)? == baseline, "scratch modified parent")?;
    reports.push(serde_json::json!({"case":"scratch","pass":true,"process_killed":true,"old_scratch_refused":true,"fresh_record_identities":true,"fresh_writer_sessions":true,"plaintext_found":false,"parent_unchanged":true}));

    // A real filesystem capacity failure: fill only the dedicated 96 MiB disk.
    let reserve = full_volume.join("reserve.bin");
    File::create_new(&reserve)?.write_all(&vec![0xa5; 6 * 1024 * 1024])?;
    let filler_path = full_volume.join("filler.bin");
    let mut filler = File::create_new(&filler_path)?;
    let chunk = vec![0x5a; 1024 * 1024];
    let fill_error = loop {
        match filler.write_all(&chunk) {
            Ok(()) => {}
            Err(error) => break error,
        }
    };
    let fill_code = fill_error.raw_os_error();
    #[cfg(windows)]
    check(
        matches!(fill_code, Some(39 | 112)),
        "filler failed for a reason other than disk full",
    )?;
    drop(filler);
    fs::remove_file(reserve)?;
    let failed = full_volume.join("exhausted.obk");
    let exhausted = write_image(&base, &failed, None, Path::new("unused"))
        .expect_err("full disk accepted complete image");
    let error = exhausted.to_string();
    #[cfg(windows)]
    check(
        error.contains("112") || error.contains("39"),
        "OBK write failed for a reason other than disk full",
    )?;
    check(
        hash(&base)? == baseline && verify(&base, &base, false)? == baseline_data,
        "disk full damaged parent",
    )?;
    check(
        ObkChain::open(&failed, &chain_key).is_err(),
        "disk-full image accepted",
    )?;
    let interrupted = records(&failed, DATA_START)?;
    check(
        !interrupted.is_empty(),
        "disk full occurred before any encrypted data",
    )?;
    fs::remove_file(filler_path)?;
    fs::remove_file(failed)?;
    fs::copy(&base, full_volume.join("base.obk"))?;
    let retry = full_volume.join("full-retry.obk");
    write_image(&base, &retry, None, Path::new("unused"))?;
    check_fresh(&interrupted, &records(&retry, DATA_START)?)?;
    let restored = verify(&base, &retry, true)?;
    reports.push(serde_json::json!({"case":"disk-full","pass":true,"fill_os_error":fill_code,"write_error":error,"unfinished_refused":true,"parent_unchanged":true,"fresh_record_identities":true,"fresh_writer_sessions":true,"retry_on_original_destination":true,"interrupted_records":interrupted.len(),"control_sha256":restored}));
    fs::write(
        root.join("result.json"),
        serde_json::to_vec_pretty(
            &serde_json::json!({"pass":true,"cases":reports,"clusters_per_image":COUNT,"baseline_sha256":baseline,"baseline_data_sha256":baseline_data}),
        )?,
    )?;
    println!("OBK-INTERRUPTIONS-PASS");
    Ok(())
}

fn main() -> Result<()> {
    let args: Vec<_> = std::env::args().collect();
    let root = Path::new(args.get(2).ok_or("mode/root required")?);
    match args.get(1).map(String::as_str) {
        Some("run") => run(
            root,
            Path::new(args.get(3).ok_or("dedicated exhaustion volume required")?),
        ),
        Some("child-image") => write_image(
            &root.join("base.obk"),
            Path::new(args.get(4).ok_or("candidate required")?),
            Some(args.get(3).ok_or("phase required")?),
            &root.join(format!("{}.ready", args[3])),
        ),
        Some("child-scratch") => {
            scratch_child(root, Path::new(args.get(4).ok_or("scratch required")?))
        }
        _ => Err("run|child-image|child-scratch required".into()),
    }
}
