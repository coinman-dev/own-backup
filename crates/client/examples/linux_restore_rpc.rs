//! Linux stand: drive the service's restore and verify RPCs from a shell
//! (`scripts/lab/linux-restore-scenario.sh`).
//!
//!     linux_restore_rpc verify-image IMG
//!     linux_restore_rpc verify-job JOB_ID [--all]
//!     linux_restore_rpc restore-volume IMG TARGET [--keep | --change [--fix-references]]
//!     linux_restore_rpc restore-disk IMG DEVICE [--keep | --change [--fix-references]]
//!     linux_restore_rpc mount IMG [--keep] | unmount IMG
//!     linux_restore_rpc plan-disk IMG TARGET_DISK OUT.json [--entries]
//!     linux_restore_rpc plan-volume IMG TARGET_DISK PART OUT.json [--extend]
//!     linux_restore_rpc plan-format IMG TARGET_DISK PART FS OUT.json
//!     linux_restore_rpc dry-run PLAN.json
//!     linux_restore_rpc apply PLAN.json [--keep | --change [--fix-references]]
//!     linux_restore_rpc boot-status | boot-action JSON
//!
//! A reply is printed as JSON and exits 0. A refusal prints
//! `{"code", "message", "identity_clashes"}` and exits 3 for
//! `FAILED_PRECONDITION`, 4 for anything else; `apply` waits for the run and
//! exits 0 only when it completed. `plan-disk` lays the copy's own captured
//! table out on the target, the way the window's designer starts;
//! `plan-volume` keeps the target's current table (read straight off the
//! device: GPT or primary MBR entries, 512-byte sectors) and puts the volume
//! into one of its partitions (and with `--extend` grows its filesystem to
//! the partition); `plan-format` creates a filesystem (`ext4`, `xfs`,
//! `btrfs`, `ntfs`, `fat32`, `exfat`) on one partition of the target and
//! writes nothing of the copy (which the plan still names, as every plan
//! does).
use std::io::{Read, Seek, SeekFrom};
use std::time::Duration;

use own_backup_client::{identity_clashes_in, ArchiveSecret, BackupClient};
use own_backup_common::engine_dto::{IdentityChoice, IdentityDecision};
use own_backup_common::restore_plan::{
    PartRef, PartitionKind, PartitionScheme, PartitionSpec, PartitionTableSpec, RestoreMode,
    RestoreOp, RestorePlan,
};
use own_backup_common::restore_state::RunStatus;

const ESP: &str = "C12A7328-F81F-11D2-BA4B-00A0C93EC93B";

fn refused(s: tonic::Status) -> ! {
    let out = serde_json::json!({
        "code": format!("{:?}", s.code()),
        "message": s.message(),
        "identity_clashes": identity_clashes_in(&s),
    });
    println!("{}", serde_json::to_string_pretty(&out).unwrap());
    std::process::exit(if s.code() == tonic::Code::FailedPrecondition {
        3
    } else {
        4
    });
}

fn failed(message: String) -> ! {
    println!(
        "{}",
        serde_json::to_string_pretty(&serde_json::json!({ "code": "Error", "message": message }))
            .unwrap()
    );
    std::process::exit(4);
}

fn print(v: &impl serde::Serialize) {
    println!("{}", serde_json::to_string_pretty(v).unwrap());
}

fn guid(b: &[u8]) -> String {
    let d1 = u32::from_le_bytes(b[0..4].try_into().unwrap());
    let d2 = u16::from_le_bytes(b[4..6].try_into().unwrap());
    let d3 = u16::from_le_bytes(b[6..8].try_into().unwrap());
    let tail: String = b[8..16].iter().map(|x| format!("{x:02X}")).collect();
    format!("{d1:08X}-{d2:04X}-{d3:04X}-{}-{}", &tail[..4], &tail[4..])
}

/// Length of a disk or file.
fn read_size(device: &str) -> u64 {
    std::fs::File::open(device)
        .and_then(|mut f| f.seek(SeekFrom::End(0)))
        .unwrap_or_else(|e| failed(format!("{device}: {e}")))
}

/// The disk's current table, as a plan would write it again.
fn read_table(device: &str) -> PartitionTableSpec {
    let mut f = std::fs::File::open(device).unwrap_or_else(|e| failed(format!("{device}: {e}")));
    let size = f.seek(SeekFrom::End(0)).unwrap();
    let mut head = vec![0u8; 34 * 512];
    f.seek(SeekFrom::Start(0)).unwrap();
    f.read_exact(&mut head).unwrap();
    if &head[512..520] == b"EFI PART" {
        let h = &head[512..1024];
        let entries_lba = u64::from_le_bytes(h[72..80].try_into().unwrap());
        let count = u32::from_le_bytes(h[80..84].try_into().unwrap()) as usize;
        let esize = u32::from_le_bytes(h[84..88].try_into().unwrap()) as usize;
        let mut raw = vec![0u8; count * esize];
        f.seek(SeekFrom::Start(entries_lba * 512)).unwrap();
        f.read_exact(&mut raw).unwrap();
        let entries = raw
            .chunks(esize)
            .enumerate()
            .filter(|(_, e)| e[..16].iter().any(|b| *b != 0))
            .map(|(i, e)| {
                let name: Vec<u16> = e[56..128]
                    .chunks(2)
                    .map(|c| u16::from_le_bytes([c[0], c[1]]))
                    .take_while(|c| *c != 0)
                    .collect();
                PartitionSpec {
                    index: i as u32 + 1,
                    first_lba: u64::from_le_bytes(e[32..40].try_into().unwrap()),
                    last_lba: u64::from_le_bytes(e[40..48].try_into().unwrap()),
                    kind: PartitionKind::Gpt {
                        type_guid: guid(&e[0..16]),
                        part_guid: guid(&e[16..32]),
                        attributes: u64::from_le_bytes(e[48..56].try_into().unwrap()),
                        name: String::from_utf16_lossy(&name),
                    },
                }
            })
            .collect();
        return PartitionTableSpec {
            scheme: PartitionScheme::Gpt,
            sector_size: 512,
            disk_size_bytes: size,
            disk_id: guid(&h[56..72]),
            entries,
        };
    }
    if head[510..512] != [0x55, 0xAA] {
        failed(format!("{device}: neither GPT nor MBR"));
    }
    let entries = (0..4)
        .filter_map(|i| {
            let e = &head[446 + 16 * i..446 + 16 * (i + 1)];
            let start = u32::from_le_bytes(e[8..12].try_into().unwrap()) as u64;
            let count = u32::from_le_bytes(e[12..16].try_into().unwrap()) as u64;
            (e[4] != 0 && count > 0).then(|| PartitionSpec {
                index: i as u32 + 1,
                first_lba: start,
                last_lba: start + count - 1,
                kind: PartitionKind::Mbr {
                    type_byte: e[4],
                    bootable: e[0] == 0x80,
                    logical: false,
                },
            })
        })
        .collect();
    PartitionTableSpec {
        scheme: PartitionScheme::Mbr,
        sector_size: 512,
        disk_size_bytes: size,
        disk_id: format!(
            "{:08x}",
            u32::from_le_bytes(head[440..444].try_into().unwrap())
        ),
        entries,
    }
}

fn part_ref(e: &PartitionSpec) -> PartRef {
    PartRef {
        index: e.index,
        offset: e.first_lba * 512,
        size: (e.last_lba + 1 - e.first_lba) * 512,
    }
}

fn restore_op(src_index: u32, dst: PartRef) -> RestoreOp {
    RestoreOp::RestorePartition {
        src_index,
        dst,
        mode: RestoreMode::Sectors,
        wipe_free_space: true,
        reencrypt: false,
    }
}

fn load_plan(file: &str) -> RestorePlan {
    let text = std::fs::read_to_string(file).unwrap_or_else(|e| failed(format!("{file}: {e}")));
    serde_json::from_str(&text).unwrap_or_else(|e| failed(format!("{file}: {e}")))
}

fn save(plan: &RestorePlan, out: &str) {
    std::fs::write(out, serde_json::to_string_pretty(plan).unwrap()).unwrap();
    print(plan);
}

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let flag = |f: &str| args.iter().any(|a| a == f);
    let pos: Vec<&str> = args
        .iter()
        .filter(|a| !a.starts_with("--"))
        .map(String::as_str)
        .collect();
    let choice = if flag("--change") {
        IdentityDecision::change(flag("--fix-references"))
    } else {
        flag("--keep").then_some(IdentityChoice::Keep).into()
    };
    let secret = ArchiveSecret::default();
    let cmd = pos.first().copied().unwrap_or_default();

    // Plans are built here, without the service.
    match cmd {
        "plan-disk" => {
            let [_, image, target, out] = pos[..] else {
                failed("plan-disk IMG TARGET_DISK OUT.json".into())
            };
            let sidecar = own_backup_backup::sidecar::read_sidecar_with_keys(
                std::path::Path::new(image),
                None,
            )
            .unwrap_or_else(|e| failed(format!("{image}: {e}")));
            let layout = sidecar
                .disk_manifest
                .map(|d| d.layout)
                .unwrap_or_else(|| failed(format!("{image} is not a whole-disk copy")));
            let table = PartitionTableSpec {
                scheme: PartitionScheme::Gpt,
                sector_size: layout.sector_size,
                disk_size_bytes: read_size(target),
                disk_id: layout.disk_guid.clone(),
                entries: layout
                    .partitions
                    .iter()
                    .map(|p| PartitionSpec {
                        index: p.index,
                        first_lba: p.first_lba,
                        last_lba: p.last_lba,
                        kind: PartitionKind::Gpt {
                            type_guid: p.type_guid.clone(),
                            part_guid: p.part_guid.clone(),
                            attributes: p.attributes,
                            name: p.name.clone(),
                        },
                    })
                    .collect(),
            };
            let mut ops = vec![RestoreOp::WritePartitionTable {
                table: table.clone(),
            }];
            for e in &table.entries {
                ops.push(restore_op(e.index, part_ref(e)));
            }
            if let Some(esp) = table.entries.iter().find(|e| {
                matches!(&e.kind, PartitionKind::Gpt { type_guid, .. }
                    if type_guid.eq_ignore_ascii_case(ESP))
            }) {
                ops.push(RestoreOp::EnsureFallbackBootloader { esp: part_ref(esp) });
            }
            if flag("--entries") {
                ops.push(RestoreOp::RestoreBootEntries);
            }
            ops.push(RestoreOp::RefreshDiskState);
            let mut plan = RestorePlan::new(0, image.into(), ops);
            plan.verify_after = true;
            plan.target_device = Some(target.to_string());
            return save(&plan, out);
        }
        "plan-volume" => {
            let [_, image, target, part, out] = pos[..] else {
                failed("plan-volume IMG TARGET_DISK PART OUT.json".into())
            };
            let table = read_table(target);
            let part: u32 = part
                .parse()
                .unwrap_or_else(|_| failed("PART is a number".into()));
            let e = table
                .entries
                .iter()
                .find(|e| e.index == part)
                .unwrap_or_else(|| failed(format!("{target} has no partition {part}")))
                .clone();
            let mut ops = vec![
                RestoreOp::WritePartitionTable { table },
                restore_op(1, part_ref(&e)),
            ];
            if flag("--extend") {
                ops.push(RestoreOp::ExtendFilesystem { part: part_ref(&e) });
            }
            ops.push(RestoreOp::RefreshDiskState);
            let mut plan = RestorePlan::new(0, image.into(), ops);
            plan.verify_after = true;
            plan.target_device = Some(target.to_string());
            return save(&plan, out);
        }
        "plan-format" => {
            let [_, image, target, part, fs, out] = pos[..] else {
                failed("plan-format IMG TARGET_DISK PART FS OUT.json".into())
            };
            let table = read_table(target);
            let part: u32 = part
                .parse()
                .unwrap_or_else(|_| failed("PART is a number".into()));
            let e = table
                .entries
                .iter()
                .find(|e| e.index == part)
                .unwrap_or_else(|| failed(format!("{target} has no partition {part}")))
                .clone();
            let fs: own_backup_common::restore_plan::FormatFs =
                serde_json::from_value(serde_json::json!(fs))
                    .unwrap_or_else(|e| failed(format!("{fs}: {e}")));
            let mut plan = RestorePlan::new(
                0,
                image.into(),
                vec![RestoreOp::Format {
                    part: part_ref(&e),
                    fs,
                    label: "restored".into(),
                    quick: true,
                }],
            );
            plan.target_device = Some(target.to_string());
            return save(&plan, out);
        }
        _ => {}
    }

    let token =
        std::fs::read_to_string(own_backup_common::config::AppConfig::default().token_path())
            .map(|s| s.trim().to_string())
            .unwrap_or_default();
    let mut client = BackupClient::connect("http://127.0.0.1:50051", token)
        .await
        .unwrap_or_else(|e| failed(format!("cannot reach the service: {e}")));

    match (cmd, &pos[1..]) {
        ("verify-image", [image]) => match client.verify_image_with_status(image, &secret).await {
            Ok(v) => print(&v),
            Err(s) => refused(s),
        },
        ("verify-job", [job]) => match client.verify_job(job, flag("--all"), &secret).await {
            Ok(v) => print(&v),
            Err(s) => refused(s),
        },
        ("restore-volume", [image, target]) => {
            match client
                .restore_volume_choosing(image, target, choice, &secret)
                .await
            {
                Ok(v) => print(&v),
                Err(s) => refused(s),
            }
        }
        ("restore-disk", [image, device]) => {
            match client
                .restore_device_with_status(image, device, choice, &secret)
                .await
            {
                Ok(v) => print(&v),
                Err(s) => refused(s),
            }
        }
        ("mount", [image]) => match client
            .mount_image_choosing(image, false, choice.choice, &secret)
            .await
        {
            Ok(at) => print(&serde_json::json!({ "mount_point": at })),
            Err(s) => refused(s),
        },
        ("unmount", [image]) => match client.unmount_image(image).await {
            Ok(()) => print(&serde_json::json!({ "unmounted": image })),
            Err(e) => failed(e),
        },
        ("dry-run", [file]) => {
            let plan = load_plan(file);
            match client
                .dry_run_restore_plan(&plan, &Default::default(), &secret)
                .await
            {
                Ok(v) => print(&v),
                Err(e) => failed(e),
            }
        }
        ("apply", [file]) => {
            // The plan as the preview returned it, like the window does.
            let plan = load_plan(file);
            let checked = client
                .dry_run_restore_plan(&plan, &Default::default(), &secret)
                .await
                .unwrap_or_else(|e| failed(e));
            let plan: RestorePlan = serde_json::from_value(checked["plan"].clone()).unwrap();
            if let Err(s) = client
                .apply_restore_plan_with_status(&plan, &Default::default(), &secret, choice)
                .await
            {
                refused(s);
            }
            loop {
                tokio::time::sleep(Duration::from_millis(500)).await;
                let Some(run) = client
                    .get_restore_state()
                    .await
                    .unwrap_or_else(|e| failed(e))
                else {
                    continue;
                };
                if run.plan_id == plan.id && run.is_finished() {
                    print(&run);
                    std::process::exit(if run.status == RunStatus::Completed {
                        0
                    } else {
                        5
                    });
                }
            }
        }
        ("boot-status", []) => match client.boot_status_with_status().await {
            Ok(v) => print(&v),
            Err(s) => refused(s),
        },
        ("boot-action", [json]) => {
            let action: serde_json::Value = serde_json::from_str(json)
                .unwrap_or_else(|e| failed(format!("{json}: {e}")));
            match client.boot_action(&action).await {
                Ok(v) => print(&v),
                Err(e) => failed(e.to_string()),
            }
        }
        _ => failed(format!("unknown command or arguments: {args:?}")),
    }
}
