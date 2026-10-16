//! Disposable Windows acceptance, driven by JSON files. Secrets stay in guest
//! fixture files; only sanitized results leave the VM through the serial port.
use own_backup_client::{ArchiveSecret, BackupClient, ClientEvent};
use own_backup_core::Job;
use serde_json::{json, Value};
use std::{path::Path, time::Duration};
use tokio_stream::StreamExt;

fn field<'a>(v: &'a Value, key: &str) -> Result<&'a str, String> {
    v[key].as_str().ok_or_else(|| format!("missing {key}"))
}
fn merge(dst: &mut Value, patch: &Value) {
    if let (Some(dst), Some(patch)) = (dst.as_object_mut(), patch.as_object()) {
        for (k, v) in patch {
            if let Some(old) = dst.get_mut(k) {
                merge(old, v);
            } else {
                dst.insert(k.clone(), v.clone());
            }
        }
    } else {
        *dst = patch.clone();
    }
}
async fn job(client: &mut BackupClient, id: &str) -> Result<Job, String> {
    client
        .list_jobs()
        .await?
        .into_iter()
        .find(|j| j.id == id)
        .ok_or("job missing".into())
}
fn secret(v: &Value) -> Result<ArchiveSecret, String> {
    match v["secret"].as_str().unwrap_or("none") {
        "none" => Ok(ArchiveSecret::none()),
        "wrong-password" => Ok(ArchiveSecret::password("deliberately-wrong-lab-password")),
        "password" => Ok(ArchiveSecret::password(
            std::fs::read_to_string(field(v, "password_file")?)
                .map_err(|e| e.to_string())?
                .trim(),
        )),
        "recovery" => Ok(ArchiveSecret::recovery_key(
            std::fs::read_to_string(field(v, "recovery_file")?)
                .map_err(|e| e.to_string())?
                .trim(),
        )),
        "wrong-recovery" => Ok(ArchiveSecret::recovery_key("obkr1invalid-lab-recovery-key")),
        other => Err(format!("unknown secret mode {other}")),
    }
}
async fn execute(client: &mut BackupClient, v: &Value) -> Result<Value, String> {
    match field(v, "op")? {
        "provide-source-password" => {
            let password = std::fs::read_to_string(field(v, "source_password_file")?)
                .map_err(|e| e.to_string())?;
            let result = client
                .provide_volume_password(
                    field(v, "volume")?,
                    password.trim(),
                    v["remember"].as_bool().unwrap_or(false),
                )
                .await?;
            if !result.ok {
                return Err("source password was refused".into());
            }
            Ok(serde_json::to_value(result).unwrap())
        }
        "status" => client
            .service_status_json()
            .await
            .and_then(|s| serde_json::from_str(&s).map_err(|e| e.to_string())),
        "create" => {
            let mut value = serde_json::to_value(client.new_job_template().await?).unwrap();
            merge(&mut value, &v["job"]);
            let job: Job = serde_json::from_value(value).map_err(|e| e.to_string())?;
            let saved = if v["encrypted"].as_bool().unwrap_or(false) {
                let password = std::fs::read_to_string(field(v, "password_file")?)
                    .map_err(|e| e.to_string())?;
                let begun = client
                    .begin_archive_encryption(&job, password.trim())
                    .await?;
                // Synthetic fixture recovery key, never printed or put in results.
                std::fs::write(field(v, "recovery_file")?, &begun.recovery_key)
                    .map_err(|e| e.to_string())?;
                client
                    .confirm_archive_encryption(&begun.token, &begun.recovery_key)
                    .await?
            } else {
                client.save_job(&job).await?
            };
            Ok(serde_json::to_value(saved).unwrap())
        }
        // A bootable whole-disk job for the disk Windows runs from, built the
        // way the wizard builds it when that disk is picked with every box
        // ticked: each partition with an openable volume is a source, the disk
        // snapshot is captured, and the copy carries the boot entries.
        "create-system-disk-job" => {
            let disks: Vec<own_backup_common::disk::DiskInfo> =
                serde_json::from_str(&client.list_disks_json().await?)
                    .map_err(|e| e.to_string())?;
            let disk = disks
                .iter()
                .find(|d| d.partitions.iter().any(|p| p.is_system))
                .ok_or("no disk holds the running Windows")?;
            let number: u32 = disk
                .device_id
                .rsplit("PhysicalDrive")
                .next()
                .and_then(|n| n.parse().ok())
                .ok_or("system disk has no number")?;
            let parts: Vec<&own_backup_common::disk::PartitionInfo> = disk
                .partitions
                .iter()
                .filter(|p| !p.volume_device.is_empty())
                .collect();
            let fs = |p: &own_backup_common::disk::PartitionInfo| {
                serde_json::to_value(p.filesystem)
                    .ok()
                    .and_then(|v| v.as_str().map(str::to_string))
                    .unwrap_or_default()
            };
            let sources: Vec<String> = parts.iter().map(|p| p.volume_device.clone()).collect();
            let mut patch = json!({
                "source": sources[0],
                "sources": sources,
                "source_labels": parts.iter().map(|p| p.volume_device.clone()).collect::<Vec<_>>(),
                "disk_source": {
                    "disk_number": number,
                    "disk_model": disk.model,
                    "parts": parts.iter().map(|p| json!({
                        "part_index": p.index,
                        "device": p.volume_device,
                        "filesystem": fs(p),
                    })).collect::<Vec<_>>(),
                    "bootable": true,
                },
                "source_disks": [disk],
            });
            merge(&mut patch, &v["job"]);
            let request = json!({
                "op": "create",
                "encrypted": v["encrypted"],
                "password_file": v["password_file"],
                "recovery_file": v["recovery_file"],
                "job": patch,
            });
            Box::pin(execute(client, &request)).await
        }
        "get" => Ok(serde_json::to_value(job(client, field(v, "id")?).await?).unwrap()),
        "save" => {
            let mut value = serde_json::to_value(job(client, field(v, "id")?).await?).unwrap();
            merge(&mut value, &v["patch"]);
            Ok(serde_json::to_value(
                client
                    .save_job(&serde_json::from_value(value).map_err(|e| e.to_string())?)
                    .await?,
            )
            .unwrap())
        }
        "reauthorize" => client
            .reauthorize_archive(field(v, "id")?, &secret(v)?)
            .await
            .map(|saved| serde_json::to_value(saved).unwrap()),
        "run" => {
            let id = field(v, "id")?;
            let before = job(client, id).await?.last_run.map(|r| r.id);
            client.run_job(id).await?;
            let limit = v["timeout_secs"].as_u64().unwrap_or(900);
            tokio::time::timeout(Duration::from_secs(limit), async {
                loop {
                    let current = job(client, id).await?;
                    if let Some(run) = current.last_run.filter(|r| Some(&r.id) != before.as_ref()) {
                        return Ok(serde_json::to_value(run).unwrap());
                    }
                    tokio::time::sleep(Duration::from_millis(400)).await;
                }
            })
            .await
            .map_err(|_| "backup timeout".to_string())?
        }
        "start-run-at-progress" => {
            let id = field(v, "id")?;
            let mut listener = client.clone();
            let events = listener.stream_events().await?;
            tokio::pin!(events);
            client.run_job(id).await?;
            tokio::time::timeout(Duration::from_secs(180), async {
                while let Some(event) = events.next().await {
                    if let ClientEvent::Progress(progress) = event {
                        if progress.job_id == id
                            && progress.blocks_done > 0
                            && progress.blocks_done < progress.blocks_total
                            && progress.phase != "scheduled-verify"
                        {
                            return Ok(json!({"blocks_done":progress.blocks_done,
                                "blocks_total":progress.blocks_total,"phase":progress.phase,
                                "percent":progress.percent}));
                        }
                    }
                }
                Err("stream closed before an active copy was observed".to_string())
            })
            .await
            .map_err(|_| "no active copy progress".to_string())?
        }
        "cancel-scheduled" => {
            let id = field(v, "id")?;
            let before = job(client, id).await?.last_verification.map(|r| r.id);
            let mut listener = client.clone();
            let events = listener.stream_events().await?;
            tokio::pin!(events);
            let observed=tokio::time::timeout(Duration::from_secs(240),async {
                while let Some(event)=events.next().await {
                    if let ClientEvent::Progress(p)=event {
                        if p.job_id==id && p.phase=="scheduled-verify" && p.blocks_total>0 {
                            client.cancel_engine_op().await?;
                            return Ok(json!({"blocks_done":p.blocks_done,"blocks_total":p.blocks_total,"percent":p.percent}));
                        }
                    }
                }
                Err("event stream closed".to_string())
            }).await.map_err(|_|"no active scheduled verification".to_string())??;
            let run = tokio::time::timeout(Duration::from_secs(60), async {
                loop {
                    if let Some(run) = job(client, id)
                        .await?
                        .last_verification
                        .filter(|r| Some(&r.id) != before.as_ref())
                    {
                        return Ok::<_, String>(run);
                    }
                    tokio::time::sleep(Duration::from_millis(200)).await;
                }
            })
            .await
            .map_err(|_| "cancel result missing".to_string())??;
            if run.status != "cancelled" {
                return Err(format!("expected cancelled, got {}", run.status));
            }
            Ok(json!({"observed":observed,"run":run}))
        }
        "verify" => {
            let result = client
                .verify_image_with_status(field(v, "image")?, &secret(v)?)
                .await;
            match result {
                Ok(summary) => Ok(serde_json::to_value(summary).unwrap()),
                Err(e) => Err(format!("RPC {:?}: {}", e.code(), e.message())),
            }
        }
        "mount" => client
            .mount_image(field(v, "image")?, false, &secret(v)?)
            .await
            .map(|p| json!({"mounted":p})),
        "unmount" => {
            client.unmount_image(field(v, "image")?).await?;
            Ok(json!({"unmounted":true}))
        }
        "export" => client
            .export_image(field(v, "image")?, field(v, "target")?, &secret(v)?)
            .await
            .map(|s| serde_json::to_value(s).unwrap())
            .map_err(|e| e.to_string()),
        "restore-volume" => client
            .restore_volume(field(v, "image")?, field(v, "target")?, &secret(v)?)
            .await
            .map(|s| serde_json::to_value(s).unwrap()),
        "verify-job" => client
            .verify_job(
                field(v, "id")?,
                v["all"].as_bool().unwrap_or(true),
                &secret(v)?,
            )
            .await
            .map(|summary| serde_json::to_value(summary).unwrap())
            .map_err(|error| error.to_string()),
        "versions" => client
            .list_job_versions(field(v, "id")?)
            .await
            .map(|s| serde_json::to_value(s).unwrap()),
        "delete" => client
            .delete_job(field(v, "id")?, v["files"].as_bool().unwrap_or(false))
            .await
            .map(|s| serde_json::to_value(s).unwrap()),
        "activity" => client
            .list_activity()
            .await
            .map(|s| serde_json::to_value(s).unwrap()),
        other => Err(format!("unsupported operation {other}")),
    }
}
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let input = std::env::args().nth(1).ok_or("JSON input path missing")?;
    let v: Value = serde_json::from_slice(&std::fs::read(&input)?)?;
    let config = own_backup_common::config::AppConfig::default();
    let token = std::fs::read_to_string(config.token_path())?;
    let mut client = BackupClient::connect(config.grpc_address, token.trim()).await?;
    let outcome = execute(&mut client, &v).await;
    let expected_error = v["expect_error"].as_bool().unwrap_or(false);
    let report = match &outcome {
        Ok(result) => json!({"ok":true,"result":result}),
        Err(e) => json!({"ok":false,"error":e}),
    };
    std::fs::write(
        Path::new(&input).with_extension("result.json"),
        serde_json::to_vec_pretty(&report)?,
    )?;
    if outcome.is_err() != expected_error {
        return Err(
            "result did not match expected success/failure; see sanitized result file".into(),
        );
    }
    println!(
        "ACCEPT-RPC-PASS op={} expected_error={expected_error}",
        field(&v, "op")?
    );
    Ok(())
}
