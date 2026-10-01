//! Disposable Windows lab: drive real restore plans and cancel only verification.
use own_backup_client::{ArchiveSecret, BackupClient};
use own_backup_common::restore_plan::{
    FormatFs, PartRef, PartitionKind, PartitionScheme, PartitionSpec, PartitionTableSpec,
    RestoreMode, RestoreOp, RestorePlan,
};
use own_backup_common::restore_state::{OpStatus, RestoreRunState, RunStatus};
use serde::Deserialize;
use std::time::Duration;

#[derive(Deserialize)]
struct Config {
    image: String,
    source_index: u32,
    source_size: u64,
    target_disk: u32,
    target_size: u64,
    cases: Vec<String>,
    results: String,
    #[serde(default)]
    letter: Option<char>,
}

fn plan(config: &Config, files: bool) -> RestorePlan {
    let part = PartRef {
        index: 1,
        offset: 1024 * 1024,
        size: config.source_size,
    };
    let mut ops = vec![RestoreOp::WritePartitionTable {
        table: PartitionTableSpec {
            scheme: PartitionScheme::Gpt,
            sector_size: 512,
            disk_size_bytes: config.target_size,
            disk_id: String::new(),
            entries: vec![PartitionSpec {
                index: 1,
                first_lba: part.offset / 512,
                last_lba: (part.offset + part.size) / 512 - 1,
                kind: PartitionKind::Gpt {
                    type_guid: "EBD0A0A2-B9E5-4433-87C0-68B6B72699C7".into(),
                    part_guid: String::new(),
                    attributes: 0,
                    name: "A08".into(),
                },
            }],
        },
    }];
    if files {
        ops.push(RestoreOp::Format {
            part,
            fs: FormatFs::Ntfs,
            label: "A08".into(),
            quick: true,
        });
    }
    ops.push(RestoreOp::RestorePartition {
        src_index: config.source_index,
        dst: part,
        mode: if files {
            RestoreMode::Files
        } else {
            RestoreMode::Sectors
        },
        wipe_free_space: false,
        reencrypt: false,
    });
    ops.push(match config.letter {
        Some(letter) => RestoreOp::AssignLetter { part, letter },
        None => RestoreOp::RefreshDiskState,
    });
    let mut plan = RestorePlan::new(config.target_disk, config.image.clone().into(), ops);
    plan.verify_after = true;
    plan
}

async fn run_case(
    client: &mut BackupClient,
    config: &Config,
    name: &str,
) -> Result<RestoreRunState, Box<dyn std::error::Error>> {
    let files = name.starts_with("files_");
    let mutation = name.strip_prefix("files_corrupt_");
    let cancel_requested = name.ends_with("_early") || name.ends_with("_late");
    let threshold = if name.ends_with("_late") { 95 } else { 0 };
    let verify_kind = if files {
        "verify_files"
    } else {
        "verify_sectors"
    };
    let preview = client
        .dry_run_restore_plan(
            &plan(config, files),
            &Default::default(),
            &ArchiveSecret::none(),
        )
        .await?;
    println!("A08-PREVIEW case={name} json={preview}");
    let plan: RestorePlan = serde_json::from_value(preview["plan"].clone())?;
    let gate = std::path::Path::new(r"C:\Lab");
    if mutation.is_some() {
        let _ = std::fs::remove_file(gate.join("a09-gate.ready"));
        let _ = std::fs::remove_file(gate.join("a09-gate.release"));
        std::fs::write(gate.join("a09-gate.request"), plan.id.to_string())?;
    }
    client
        .apply_restore_plan(&plan, &Default::default(), &ArchiveSecret::none())
        .await?;
    println!(
        "A08-START case={name} plan={} mode={}",
        plan.id,
        if files { "files" } else { "sectors" }
    );
    let deadline = tokio::time::Instant::now() + Duration::from_secs(600);
    let mut accepted = false;
    let mut mutated = false;
    let mut held_directory: Option<std::fs::File> = None;
    let final_state = loop {
        if tokio::time::Instant::now() > deadline {
            return Err(format!("case timeout: {name}").into());
        }
        if let Some(state) = client.get_restore_state().await? {
            if state.plan_id != plan.id {
                return Err("restore state belongs to another plan".into());
            }
            if state.is_finished() {
                break state;
            }
            if let Some(change) = mutation {
                if !mutated
                    && std::fs::read_to_string(gate.join("a09-gate.ready"))
                        .map(|value| value.trim() == plan.id.to_string())
                        .unwrap_or(false)
                {
                    let changed = mutate_restored_tree(change);
                    std::fs::write(gate.join("a09-gate.release"), plan.id.to_string())?;
                    held_directory = changed?;
                    mutated = true;
                    println!("A09-MUTATED case={name} phase=lab_gate");
                    let at = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)?
                        .as_millis();
                    std::fs::write(
                        r"C:\Lab\a09-mutation.log",
                        format!("case={name} epoch_ms={at} current={:?}\n", state.current),
                    )?;
                }
            }
            if cancel_requested && !accepted {
                if let Some(index) = state.current {
                    let op = &state.ops[index];
                    if cancel_requested
                        && op.kind == verify_kind
                        && op.status == OpStatus::Running
                        && op.percent >= threshold
                    {
                        if !state.cancellable {
                            return Err("verification refused cancellation".into());
                        }
                        let reply = client.cancel_restore_plan(&plan.id.to_string()).await?;
                        accepted = reply["cancelled"].as_bool() == Some(true);
                        println!("A08-CANCEL case={name} percent={} bytes_done={} bytes_total={} accepted={accepted}", op.percent, op.bytes_done, op.bytes_total);
                        if !accepted {
                            return Err("cancel request was not accepted".into());
                        }
                        if threshold == 0 && op.percent > 5 {
                            return Err("early cancellation arrived too late".into());
                        }
                        if threshold == 95 && op.percent >= 100 {
                            return Err("late cancellation arrived after verification".into());
                        }
                    }
                }
            }
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    };
    println!(
        "A08-FINAL case={name} json={}",
        serde_json::to_string(&final_state)?
    );
    let verify = final_state
        .ops
        .iter()
        .find(|o| o.kind == verify_kind)
        .ok_or("verification record missing")?;
    if mutation.is_some() {
        if !mutated || final_state.status != RunStatus::Failed || verify.status != OpStatus::Failed
        {
            return Err("a changed restored tree was not rejected".into());
        }
        if final_state.completed().count() != 4 || verify.message.is_empty() {
            return Err("failed verification lost the completed restore or its reason".into());
        }
    } else if cancel_requested {
        if !accepted || final_state.status != RunStatus::Cancelled {
            return Err("restore did not report Cancelled".into());
        }
        if verify.status != OpStatus::Skipped || verify.percent >= 100 {
            return Err("cancelled verification was reported as checked".into());
        }
        if final_state.cancellable || final_state.current.is_some() {
            return Err("terminal run still appears active".into());
        }
        let expected_done = if files { 4 } else { 2 };
        if final_state.completed().count() != expected_done {
            return Err(
                "completed operations were lost or later operations ran after cancel".into(),
            );
        }
        if !files
            && final_state
                .ops
                .iter()
                .find(|o| o.kind == "refresh_disk_state")
                .map(|o| o.status)
                != Some(OpStatus::Skipped)
        {
            return Err("operation after sector verification was not skipped".into());
        }
    } else if final_state.status != RunStatus::Completed
        || final_state.ops.iter().any(|o| o.status != OpStatus::Done)
    {
        return Err("uncancelled restore did not complete and verify".into());
    }
    let history_deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        if client
            .get_restore_history()
            .await?
            .iter()
            .any(|r| r == &final_state)
        {
            break;
        }
        if tokio::time::Instant::now() > history_deadline {
            return Err("final state was not persisted exactly".into());
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    println!(
        "A08-PASS case={name} status={:?} done={} persisted=true",
        final_state.status,
        final_state.completed().count()
    );
    drop(held_directory);
    tokio::time::sleep(Duration::from_millis(50)).await;
    Ok(final_state)
}

#[cfg(target_os = "windows")]
fn mutate_restored_tree(change: &str) -> Result<Option<std::fs::File>, Box<dyn std::error::Error>> {
    use std::io::{Seek, SeekFrom, Write};
    use std::os::windows::fs::OpenOptionsExt;
    let directory = std::path::Path::new(r"T:\a07-data");
    let large = directory.join("large.bin");
    match change {
        "byte" => {
            let mut file = std::fs::OpenOptions::new().write(true).open(&large)?;
            file.seek(SeekFrom::Start(0))?;
            file.write_all(b"X")?;
            file.sync_all()?;
        }
        "name" => std::fs::rename(&large, directory.join("renamed.bin"))?,
        "ads" => std::fs::write(format!("{}:a09", large.display()), b"changed ADS")?,
        "acl" => {
            let before = std::process::Command::new("icacls.exe")
                .arg(directory)
                .output()?;
            let result = std::process::Command::new("icacls.exe")
                .arg(directory)
                .args(["/grant", "*S-1-5-32-545:F"])
                .output()?;
            let after = std::process::Command::new("icacls.exe")
                .arg(directory)
                .output()?;
            if !before.status.success() || !result.status.success() || before.stdout == after.stdout
            {
                return Err("nested directory ACL was not changed".into());
            }
        }
        "reparse" => {
            use std::io::BufRead;
            let mut tcp = std::net::TcpStream::connect(("127.0.0.1", 43996))?;
            tcp.set_read_timeout(Some(Duration::from_secs(15)))?;
            tcp.write_all(b"reparse\n")?;
            let mut reply = String::new();
            std::io::BufReader::new(&mut tcp).read_line(&mut reply)?;
            if reply != "OK\n" {
                return Err(format!("SYSTEM reparse mutation failed: {reply:?}").into());
            }
        }
        "locked_dir" => {
            let handle = std::fs::OpenOptions::new()
                .read(true)
                .share_mode(0)
                .custom_flags(0x0200_0000)
                .open(directory)?;
            return Ok(Some(handle));
        }
        _ => return Err(format!("unknown mutation {change}").into()),
    }
    Ok(None)
}

#[cfg(not(target_os = "windows"))]
fn mutate_restored_tree(
    _change: &str,
) -> Result<Option<std::fs::File>, Box<dyn std::error::Error>> {
    Err("Windows-only lab mutation".into())
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = std::env::args().nth(1).ok_or("config path missing")?;
    let config: Config = serde_json::from_slice(&std::fs::read(path)?)?;
    if config.target_disk < 2
        || config.target_size != 2 * 1024 * 1024 * 1024
        || config.source_size + 2 * 1024 * 1024 > config.target_size
    {
        return Err("target must be the disposable 2-GiB disk".into());
    }
    let app = own_backup_common::config::AppConfig::default();
    let token = std::fs::read_to_string(app.token_path())?;
    let mut client = BackupClient::connect(app.grpc_address, token.trim()).await?;
    if std::env::args().nth(2).as_deref() == Some("--check-history") {
        let expected: Vec<RestoreRunState> =
            serde_json::from_slice(&std::fs::read(&config.results)?)?;
        let history = client.get_restore_history().await?;
        if expected.is_empty() || expected.iter().any(|r| !history.contains(r)) {
            return Err("history changed after service restart".into());
        }
        if client.get_restore_state().await?.as_ref() != expected.last() {
            return Err("last restore report was not reopened after restart".into());
        }
        println!("A08-REOPENED-PASS cases={} persisted=true", expected.len());
        return Ok(());
    }
    let inventory: Vec<own_backup_common::disk::DiskInfo> =
        serde_json::from_str(&client.list_disks_json().await?)?;
    let target = inventory
        .iter()
        .find(|d| {
            d.device_id
                .ends_with(&format!("PhysicalDrive{}", config.target_disk))
        })
        .ok_or("target absent from inventory")?;
    if target.size_bytes != config.target_size
        || target.partitions.iter().any(|p| p.is_system || p.is_boot)
    {
        return Err("target disk is not disposable".into());
    }
    let mut results = Vec::new();
    for name in &config.cases {
        match run_case(&mut client, &config, name).await {
            Ok(state) => {
                results.push(state);
                std::fs::write(&config.results, serde_json::to_vec_pretty(&results)?)?;
            }
            Err(error) => {
                println!("A08-FAIL case={name} error={error}");
                return Err(error);
            }
        }
    }
    println!("A08-CLIENT-PASS cases={}", results.len());
    Ok(())
}
