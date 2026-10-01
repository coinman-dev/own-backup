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
    ops.push(RestoreOp::RefreshDiskState);
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
    let cancel_requested = !name.ends_with("_complete");
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
            if cancel_requested && !accepted {
                if let Some(index) = state.current {
                    let op = &state.ops[index];
                    if op.kind == verify_kind
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
    if cancel_requested {
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
    tokio::time::sleep(Duration::from_millis(50)).await;
    Ok(final_state)
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
