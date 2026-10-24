//! Real Windows backup cancellation through the installed service's named pipe.
#[cfg(windows)]
mod audit {
    use own_backup_client::{BackupClient, ClientEvent};
    use own_backup_core::{DiskPartSpec, DiskSourceSpec, Job, JobRun};
    use std::path::{Path, PathBuf};
    use std::time::Duration;
    use tokio_stream::StreamExt;
    type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

    fn say(text: impl AsRef<str>) {
        use std::io::Write;
        println!("{}", text.as_ref());
        if let Ok(mut port) = std::fs::OpenOptions::new().write(true).open(r"\\.\COM1") {
            let _ = writeln!(port, "{}", text.as_ref());
        }
    }
    async fn client() -> Result<BackupClient> {
        let config = own_backup_common::config::AppConfig::default();
        let token = std::fs::read_to_string(config.token_path())?;
        Ok(BackupClient::connect(config.grpc_address, token.trim()).await?)
    }
    async fn run_to_finish(control: &mut BackupClient, job: &str) -> Result<JobRun> {
        let old = control
            .get_job(job)
            .await?
            .and_then(|j| j.last_run)
            .map(|r| r.id)
            .unwrap_or_default();
        control.run_job(job).await?;
        for _ in 0..600 {
            if let Some(run) = control.get_job(job).await?.and_then(|j| j.last_run) {
                if run.id != old && !run.finished_at.is_empty() {
                    return Ok(run);
                }
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        Err("backup completion timeout".into())
    }
    fn image_files(folder: &Path) -> Result<Vec<String>> {
        let mut paths = Vec::new();
        for e in std::fs::read_dir(folder)? {
            let e = e?;
            if e.path().extension().is_some_and(|s| s == "qcow2") {
                paths.push(e.path().display().to_string());
            }
        }
        paths.sort();
        Ok(paths)
    }
    fn hash(path: &str) -> Result<String> {
        let output = std::process::Command::new("powershell.exe")
            .args([
                "-NoProfile",
                "-Command",
                &format!(
                    "(Get-FileHash -Algorithm SHA256 -LiteralPath '{}').Hash",
                    path.replace('\'', "''")
                ),
            ])
            .output()?;
        if !output.status.success() {
            return Err("file hash failed".into());
        }
        let hash = String::from_utf8(output.stdout)?.trim().to_owned();
        if hash.len() != 64 {
            return Err("invalid file hash".into());
        }
        Ok(hash)
    }
    pub async fn cases() -> Result<()> {
        let fixture: serde_json::Value =
            serde_json::from_slice(&std::fs::read(r"C:\Lab\a07\fixture.json")?)?;
        let source = fixture["source"].as_str().ok_or("source missing")?;
        let mut control = client().await?;
        let mut reports = Vec::new();
        for shape in ["volume", "disk"] {
            for phase in ["prepare", "verify"] {
                let folder = PathBuf::from(format!(r"C:\Lab\a07\{shape}-{phase}"));
                std::fs::create_dir_all(&folder)?;
                let mut job = Job::default();
                job.name = format!("AuditA07{shape}{phase}");
                job.source = source.into();
                job.source_label = "A07 synthetic NTFS".into();
                job.sources = vec![source.into()];
                job.dest_folder = folder.display().to_string();
                job.image_format = own_backup_common::types::ImageFormat::Qcow2;
                job.schedule.kind = "none".into();
                job.scheme.method = "full".into();
                job.scheme.keep_chains = Some(1);
                job.advanced.validate_after = false;
                job.advanced.retry_attempts = 0;
                job.exclusions.enabled = phase == "prepare";
                job.exclusions.patterns = vec!["*.a07-never-matched".into()];
                if shape == "disk" {
                    job.disk_source = Some(DiskSourceSpec {
                        disk_number: 1,
                        device: None,
                        disk_model: "A07 disposable disk".into(),
                        bootable: false,
                        parts: vec![DiskPartSpec {
                            part_index: fixture["partition"].as_u64().ok_or("partition missing")?
                                as u32,
                            device: source.into(),
                            filesystem: "ntfs".into(),
                        }],
                    });
                }
                let mut job = control.save_job(&job).await?;
                let baseline = if phase == "verify" {
                    let run = run_to_finish(&mut control, &job.id).await?;
                    if run.status != "success" {
                        return Err(format!("baseline failed: {}", run.error).into());
                    }
                    job.advanced.validate_after = true;
                    job = control.save_job(&job).await?;
                    Some(run)
                } else {
                    None
                };
                let before = image_files(&folder)?;
                let baseline_hashes = before
                    .iter()
                    .map(|p| Ok((p.clone(), hash(p)?)))
                    .collect::<Result<Vec<_>>>()?;
                for name in ["request", "ready", "release"] {
                    let _ = std::fs::remove_file(Path::new(r"C:\Lab\a07-gate").join(name));
                }
                std::fs::write(
                    r"C:\Lab\a07-gate\request",
                    serde_json::json!({"job":job.id,"phase":phase}).to_string(),
                )?;
                let mut subscription = control.clone();
                let mut events = subscription.stream_events().await?;
                let old = control
                    .get_job(&job.id)
                    .await?
                    .and_then(|j| j.last_run)
                    .map(|r| r.id)
                    .unwrap_or_default();
                control.run_job(&job.id).await?;
                let mut progress = None;
                for _ in 0..600 {
                    if let Ok(Some(event)) =
                        tokio::time::timeout(Duration::from_millis(100), events.next()).await
                    {
                        if let ClientEvent::Progress(p) = event {
                            if p.job_id == job.id && p.phase == phase {
                                progress = Some(p);
                            }
                        }
                    }
                    if progress.is_some() && Path::new(r"C:\Lab\a07-gate\ready").exists() {
                        break;
                    }
                }
                let progress = progress.ok_or("requested real phase progress not observed")?;
                let ready: serde_json::Value =
                    serde_json::from_slice(&std::fs::read(r"C:\Lab\a07-gate\ready")?)?;
                if ready["phase"] != phase || ready["job"] != job.id {
                    return Err("wrong progress callback gate".into());
                }
                if control.get_queue().await?.current.as_deref() != Some(job.id.as_str()) {
                    return Err("job not current while phase was paused".into());
                }
                let cancel_at = chrono::Utc::now().to_rfc3339();
                control.cancel_running().await?;
                std::fs::write(r"C:\Lab\a07-gate\release", "release")?;
                let mut later = Vec::new();
                let mut finished = None;
                for _ in 0..600 {
                    if let Ok(Some(event)) =
                        tokio::time::timeout(Duration::from_millis(100), events.next()).await
                    {
                        match event {
                            ClientEvent::Progress(p) if p.job_id == job.id => later.push(p),
                            ClientEvent::Finished(r) if r.job_id == job.id => {
                                finished = Some(r);
                                break;
                            }
                            _ => {}
                        }
                    }
                    if let Some(run) = control.get_job(&job.id).await?.and_then(|j| j.last_run) {
                        if run.id != old && !run.finished_at.is_empty() {
                            finished = Some(run);
                            break;
                        }
                    }
                }
                let run = finished.ok_or("cancelled result missing")?;
                if run.status != "cancelled" || run.error.is_empty() {
                    return Err(format!("wrong cancellation result: {run:?}").into());
                }
                if later.iter().any(|p| {
                    p.blocks_done > progress.blocks_done
                        || (phase == "prepare" && p.phase == "backup")
                }) {
                    return Err("work progressed after callback cancellation".into());
                }
                let after = image_files(&folder)?;
                if phase == "prepare" {
                    if after != before || run.bytes_copied != 0 {
                        return Err("preparation cancellation produced an image/payload".into());
                    }
                } else {
                    if after.len() != before.len() + 1
                        || run.image_path.is_empty()
                        || !Path::new(&run.image_path).exists()
                        || run.bytes_copied == 0
                    {
                        return Err(
                            "post-copy cancellation lost the finished image or ran retention"
                                .into(),
                        );
                    }
                    if !before.iter().all(|p| Path::new(p).exists()) {
                        return Err("retention removed baseline after cancelled verify".into());
                    }
                }
                let history = control.list_activity().await?;
                if !history.iter().any(|r| {
                    r.id == run.id
                        && serde_json::to_value(r).ok() == serde_json::to_value(&run).ok()
                }) {
                    return Err("history differs from cancelled run".into());
                }
                let queue = control.get_queue().await?;
                if queue.current.is_some() {
                    return Err("queue remained current after cancellation".into());
                }
                for (path, expected) in &baseline_hashes {
                    if hash(path)? != *expected {
                        return Err("baseline image changed".into());
                    }
                }
                let report = serde_json::json!({"shape":shape,"phase":phase,"pass":true,"job":job.id,"cancel_at":cancel_at,"callback":ready,"progress":progress,"later_progress":later,"run":run,"baseline":baseline,"baseline_hashes":baseline_hashes,"images_before":before,"images_after":after,"queue_idle":true});
                std::fs::write(
                    folder.join("result.json"),
                    serde_json::to_vec_pretty(&report)?,
                )?;
                say(format!(
                    "A07-CASE-PASS shape={shape} phase={phase} status=cancelled callback_done={}",
                    ready["done"]
                ));
                reports.push(report);
                drop(events);
                let _ = std::fs::remove_file(r"C:\Lab\a07-gate\request");
            }
        }
        std::fs::write(
            r"C:\Lab\a07\results.json",
            serde_json::to_vec_pretty(&serde_json::json!({"pass":true,"cases":reports}))?,
        )?;
        say("A07-RPC-PASS cases=4");
        Ok(())
    }

    pub fn check_images() -> Result<()> {
        let cases: serde_json::Value =
            serde_json::from_slice(&std::fs::read(r"C:\Lab\a07\results.json")?)?;
        let mut reports = Vec::new();
        for case in cases["cases"].as_array().ok_or("cases missing")? {
            for path in case["images_after"].as_array().ok_or("images missing")? {
                let path = path.as_str().ok_or("image path missing")?;
                let summary =
                    own_backup_backup::engine_win::verify_image(Path::new(path), |_, _| Ok(()))?;
                if !summary.valid() || summary.blocks_checked == 0 {
                    return Err("preserved image invalid".into());
                }
                reports.push(serde_json::json!({"image":path,"valid":true,"blocks_checked":summary.blocks_checked,"sha256":hash(path)?}));
            }
        }
        if reports.len() != 4 {
            return Err("expected four preserved images".into());
        }
        std::fs::write(
            r"C:\Lab\a07\images-checked.json",
            serde_json::to_vec_pretty(&reports)?,
        )?;
        say("A07-IMAGES-PASS preserved_images=4 full_readback=true");
        Ok(())
    }

    pub fn scanner() -> Result<()> {
        use std::io::{Read, Seek, SeekFrom};
        use std::sync::{
            atomic::{AtomicBool, AtomicU64, Ordering},
            Arc,
        };
        struct Counted<T> {
            inner: T,
            cancelled: Arc<AtomicBool>,
            after: Arc<AtomicU64>,
        }
        impl<T: Read> Read for Counted<T> {
            fn read(&mut self, b: &mut [u8]) -> std::io::Result<usize> {
                if self.cancelled.load(Ordering::SeqCst) {
                    self.after.fetch_add(1, Ordering::SeqCst);
                }
                self.inner.read(b)
            }
        }
        impl<T: Seek> Seek for Counted<T> {
            fn seek(&mut self, p: SeekFrom) -> std::io::Result<u64> {
                self.inner.seek(p)
            }
        }
        let f: serde_json::Value =
            serde_json::from_slice(&std::fs::read(r"C:\Lab\a07\fixture.json")?)?;
        let path = format!(
            "part:1:{}:{}",
            f["offset"].as_u64().ok_or("offset")?,
            f["size"].as_u64().ok_or("size")?
        );
        let mut raw = own_backup_backup::win_device::WinRawDevice::open_read(&path)?;
        let cur = own_backup_backup::ntfs_scan::RawVolumeCursor::new(&mut raw);
        let buffered = own_backup_backup::ntfs_scan::BufferedVolume::new(cur, 4 << 20, 6);
        let cancelled = Arc::new(AtomicBool::new(false));
        let after = Arc::new(AtomicU64::new(0));
        let mut source = Counted {
            inner: buffered,
            cancelled: cancelled.clone(),
            after: after.clone(),
        };
        let mut point = None;
        let error = own_backup_backup::ntfs_scan::scan_mft_with_cancel(
            &mut source,
            &["*.a07-never-matched".into()],
            |done, total| {
                point = Some((done, total));
                cancelled.store(true, Ordering::SeqCst);
                Err(own_backup_common::OwnBackupError::Cancelled)
            },
        )
        .err()
        .ok_or("MFT scan ignored cancellation")?;
        if !matches!(error, own_backup_common::OwnBackupError::Cancelled)
            || point.is_none()
            || after.load(Ordering::SeqCst) != 0
        {
            return Err(format!("scanner cancellation/read-boundary incorrect: {error}").into());
        }
        let report = serde_json::json!({"pass":true,"callback":point,"reads_after_cancel_callback":after.load(Ordering::SeqCst),"error":"Cancelled"});
        std::fs::write(
            r"C:\Lab\a07\scanner-cancel.json",
            serde_json::to_vec_pretty(&report)?,
        )?;
        say(format!("A07-SCANNER-PASS {report}"));
        Ok(())
    }
}
#[cfg(windows)]
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    match std::env::args().nth(1).as_deref() {
        Some("scanner") => audit::scanner(),
        Some("check-images") => audit::check_images(),
        _ => audit::cases().await,
    }
}
#[cfg(not(windows))]
fn main() {
    eprintln!("audit_a07_cancel is Windows-only");
}
