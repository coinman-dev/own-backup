//! System-OBK restore acceptance (CHECKS 1), the WinPE half.
//!
//! Runs inside the product's own boot medium, beside the engine it started.
//! Finds the store disk by its marker, opens the newest encrypted copy with the
//! fixture password, and restores it onto the one blank disk through the same
//! kind of plan the restore screen builds for a bootable whole-disk copy:
//! partition table with the copied partition GUIDs, a sector restore of every
//! partition the copy holds, the boot chain, the fallback loader, the boot
//! entries, and a re-read of the disk. Reports to COM1 and to X:\.
#[cfg(windows)]
mod probe {
    use own_backup_backup::seal::context::ArchiveKeys;
    use own_backup_client::{ArchiveSecret, BackupClient};
    use own_backup_common::disk::DiskInfo;
    use own_backup_common::gpt::GPT_TYPE_EFI_SYSTEM;
    use own_backup_common::restore_check::{CheckContext, CheckLevel, CheckResult};
    use own_backup_common::restore_plan::{
        PartRef, PartitionKind, PartitionScheme, PartitionSpec, PartitionTableSpec, RestoreMode,
        RestoreOp, RestorePlan,
    };
    use own_backup_common::restore_state::RunStatus;
    use std::fs::OpenOptions;
    use std::io::Write;
    use std::path::{Path, PathBuf};
    use std::time::{Duration, Instant};

    type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

    const BASIC_DATA: &str = "EBD0A0A2-B9E5-4433-87C0-68B6B72699C7";

    /// The small FAT disk the host attaches for the log: WinPE has no serial
    /// port driver, and X:\ is gone at power-off.
    fn log_disk() -> Option<PathBuf> {
        static FOUND: std::sync::OnceLock<Option<PathBuf>> = std::sync::OnceLock::new();
        FOUND
            .get_or_init(|| {
                (b'C'..=b'Z')
                    .map(|l| PathBuf::from(format!("{}:\\", l as char)))
                    .find(|r| r.join("pe-log.marker").is_file())
            })
            .clone()
    }

    pub fn say(message: &str) {
        println!("{message}");
        let mut targets = vec![
            PathBuf::from(r"X:\pe-sysrestore.log"),
            PathBuf::from(r"\\.\COM1"),
        ];
        if let Some(disk) = log_disk() {
            targets.push(disk.join("pe-sysrestore.log"));
        }
        for path in targets {
            if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(&path) {
                let _ = writeln!(file, "{message}");
                let _ = file.sync_all();
            }
        }
    }

    fn check(ok: bool, message: &str) -> Result<()> {
        if !ok {
            return Err(message.into());
        }
        say(&format!("PE-OK {message}"));
        Ok(())
    }

    fn store_root() -> Result<PathBuf> {
        let roots: Vec<PathBuf> = (b'C'..=b'Z')
            .map(|l| PathBuf::from(format!("{}:\\", l as char)))
            .filter(|r| r.join(r"obk-lab\store.marker").is_file())
            .collect();
        check(roots.len() == 1, "one store disk carries the marker")?;
        Ok(roots[0].clone())
    }

    fn newest_obk(dir: &Path) -> Result<PathBuf> {
        let mut found: Vec<(std::time::SystemTime, PathBuf)> = std::fs::read_dir(dir)?
            .flatten()
            .filter(|e| {
                e.path()
                    .extension()
                    .is_some_and(|x| x.eq_ignore_ascii_case("obk"))
            })
            .filter_map(|e| Some((e.metadata().ok()?.modified().ok()?, e.path())))
            .collect();
        found.sort();
        found
            .pop()
            .map(|(_, p)| p)
            .ok_or_else(|| "no .obk on the store disk".into())
    }

    async fn connect() -> Result<BackupClient> {
        let config = own_backup_common::config::AppConfig::default();
        let deadline = Instant::now() + Duration::from_secs(180);
        loop {
            if let Ok(token) = std::fs::read_to_string(config.token_path()) {
                if let Ok(mut c) =
                    BackupClient::connect(config.grpc_address.clone(), token.trim()).await
                {
                    if c.service_status_json().await.is_ok() {
                        return Ok(c);
                    }
                }
            }
            if Instant::now() > deadline {
                return Err("the engine did not answer within 3 minutes".into());
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
    }

    pub async fn run() -> Result<()> {
        say("PE-START system restore");
        check(
            own_backup_common::media_mode::is_windows_pe(),
            "running in Windows PE",
        )?;
        let store = store_root()?;
        let password = std::fs::read_to_string(store.join(r"obk-lab\password.txt"))?
            .trim()
            .to_string();
        let image = newest_obk(&store.join("obk"))?;
        say(&format!("PE-IMAGE {}", image.display()));

        // What the restore screen reads first: the copy's own layout, from the
        // sealed half, opened with the key the password gives.
        let keys = ArchiveKeys::from_image_password(&image, &password)?;
        let chain = keys.open_chain_of(&image)?;
        let sidecar = own_backup_backup::sidecar::read_sealed_sidecar(&image, &chain.key)?;
        let manifest = sidecar
            .disk_manifest
            .clone()
            .ok_or("the copy is not a whole-disk copy")?;
        let layout = &manifest.layout;
        let in_copy: Vec<u32> = manifest.images.iter().map(|i| i.part_index).collect();
        say(&format!(
            "PE-LAYOUT sector={} parts={:?} images={in_copy:?}",
            layout.sector_size,
            layout
                .partitions
                .iter()
                .map(|p| (p.index, p.type_guid.clone(), p.first_lba, p.last_lba))
                .collect::<Vec<_>>()
        ));

        let mut client = connect().await?;
        let disks: Vec<DiskInfo> = serde_json::from_str(&client.list_disks_json().await?)?;
        let blank: Vec<&DiskInfo> = disks
            .iter()
            .filter(|d| {
                d.partitions.is_empty() && d.size_bytes >= layout.size_bytes.min(d.size_bytes)
            })
            .filter(|d| !d.model.to_ascii_uppercase().starts_with("OWNBACK"))
            .collect();
        check(blank.len() == 1, "exactly one blank target disk")?;
        let target = blank[0];
        let target_number: u32 = target
            .device_id
            .rsplit("PhysicalDrive")
            .next()
            .and_then(|n| n.parse().ok())
            .ok_or("target disk has no number")?;
        let sector = target.sector_size_logical.max(512);
        check(
            sector == layout.sector_size,
            "target sector size matches the copy",
        )?;
        check(
            target.size_bytes
                >= layout
                    .partitions
                    .iter()
                    .map(|p| (p.last_lba + 1) * sector as u64)
                    .max()
                    .unwrap_or(0),
            "target is large enough for the copied layout",
        )?;

        // The table exactly as the restore screen writes it for a whole-disk
        // restore: the copy's slots and partition GUIDs, a fresh disk GUID,
        // attributes left to the writer (the screen sends 0).
        let entries: Vec<PartitionSpec> = layout
            .partitions
            .iter()
            .map(|p| PartitionSpec {
                index: p.index,
                first_lba: p.first_lba,
                last_lba: p.last_lba,
                kind: PartitionKind::Gpt {
                    type_guid: p.type_guid.clone(),
                    part_guid: p.part_guid.clone(),
                    attributes: 0,
                    name: p.name.clone(),
                },
            })
            .collect();
        let refs = |index: u32| -> Option<PartRef> {
            entries.iter().find(|e| e.index == index).map(|e| PartRef {
                index,
                offset: e.first_lba * sector as u64,
                size: (e.last_lba + 1 - e.first_lba) * sector as u64,
            })
        };
        let mut ops = vec![RestoreOp::WritePartitionTable {
            table: PartitionTableSpec {
                scheme: PartitionScheme::Gpt,
                sector_size: sector,
                disk_size_bytes: target.size_bytes,
                disk_id: String::new(),
                entries: entries.clone(),
            },
        }];
        for &index in &in_copy {
            ops.push(RestoreOp::RestorePartition {
                src_index: index,
                dst: refs(index).ok_or("partition missing from the table")?,
                mode: RestoreMode::Sectors,
                wipe_free_space: false,
                reencrypt: false,
            });
        }
        let esp = layout
            .partitions
            .iter()
            .find(|p| {
                p.type_guid.eq_ignore_ascii_case(GPT_TYPE_EFI_SYSTEM) && in_copy.contains(&p.index)
            })
            .and_then(|p| refs(p.index));
        let windows = layout
            .partitions
            .iter()
            .find(|p| p.type_guid.eq_ignore_ascii_case(BASIC_DATA) && in_copy.contains(&p.index))
            .and_then(|p| refs(p.index))
            .ok_or("no Windows partition in the copy")?;
        ops.push(RestoreOp::FinalizeBoot {
            windows_part: windows,
            esp,
            uefi: true,
        });
        if let Some(esp) = esp {
            ops.push(RestoreOp::EnsureFallbackBootloader { esp });
        }
        let carries_entries =
            own_backup_backup::sidecar::boot_entries_of(&sidecar, &image).is_some();
        say(&format!("PE-BOOT-ENTRIES carried={carries_entries}"));
        if carries_entries {
            ops.push(RestoreOp::RestoreBootEntries);
        }
        ops.push(RestoreOp::RefreshDiskState);
        let mut plan = RestorePlan::new(target_number, image.clone(), ops);
        plan.verify_after = true;

        let secret = ArchiveSecret::password(&password);
        let preview = client
            .dry_run_restore_plan(&plan, &CheckContext::default(), &secret)
            .await?;
        let results: Vec<CheckResult> = serde_json::from_value(preview["results"].clone())?;
        for r in &results {
            say(&format!(
                "PE-CHECK {:?} {:?} {}",
                r.level, r.code, r.message
            ));
        }
        check(
            !results.iter().any(|r| r.level == CheckLevel::Error),
            "the plan passes the service's checks",
        )?;
        let plan: RestorePlan = serde_json::from_value(preview["plan"].clone())?;
        client
            .apply_restore_plan(&plan, &Default::default(), &secret)
            .await?;
        say(&format!(
            "PE-APPLIED plan={} target_disk={target_number}",
            plan.id
        ));

        let deadline = Instant::now() + Duration::from_secs(5400);
        let mut last = String::new();
        let state = loop {
            if Instant::now() > deadline {
                return Err("restore did not finish within 90 minutes".into());
            }
            if let Some(state) = client.get_restore_state().await? {
                let now = format!(
                    "{:?} {:?}",
                    state.current,
                    state
                        .current
                        .and_then(|i| state.ops.get(i))
                        .map(|o| o.percent)
                );
                if now != last {
                    say(&format!("PE-PROGRESS {now}"));
                    last = now;
                }
                if state.is_finished() {
                    break state;
                }
            }
            tokio::time::sleep(Duration::from_secs(5)).await;
        };
        say(&format!("PE-STATE {}", serde_json::to_string(&state)?));
        check(
            state.status == RunStatus::Completed,
            "the restore plan completed",
        )?;
        say("PE-RESULT PASS");
        Ok(())
    }
}

#[cfg(windows)]
#[tokio::main]
async fn main() {
    let code = match probe::run().await {
        Ok(()) => 0,
        Err(e) => {
            probe::say(&format!("PE-RESULT FAIL {e}"));
            1
        }
    };
    // The medium powers off when the run is over either way; the host reads
    // the verdict from the serial log.
    let _ = std::process::Command::new("wpeutil")
        .arg("shutdown")
        .status();
    std::process::exit(code);
}

#[cfg(not(windows))]
fn main() {}
