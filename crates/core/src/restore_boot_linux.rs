//! The boot steps of a restore plan on Linux (PLAN §6, stage 3): the fallback
//! loader on the ESP, the machine's boot entries from the copy, and making a
//! Windows restored from Linux start.
//!
//! They run after the disk was let go (`restore_exec_linux`), through the
//! partitions' own device nodes, each mounted for the duration of the step
//! under `/run/own-backup/restore-mnt` and unmounted when it ends, however it
//! ends.
//!
//! ## Windows from Linux
//!
//! `bcdboot` does not exist here, and there is no need for it while the
//! restored disk carries the copy's own GUIDs: the BCD names the Windows
//! partition and the ESP by partition GUID and disk GUID, so a disk that
//! reproduces both is the disk its BCD describes. Then three things remain,
//! and they are what `engine_win::finalize_boot` does after `bcdboot` too:
//! remove `\Windows\bootstat.dat` (a copy of a running system records a
//! shutdown that never happened, and the first boot stops at the recovery
//! screen), make sure `\EFI\BOOT\BOOTX64.EFI` exists, and give this machine's
//! firmware an entry for the Windows Boot Manager. A disk whose GUIDs had to
//! change, or a BIOS Windows, needs the BCD itself edited — not done yet, and
//! refused with a way forward.

use std::path::{Path, PathBuf};

use tracing::{info, warn};

use own_backup_backup::efivars_linux::{safety_copy_dir, Action, Efivarfs};
use own_backup_backup::linux_tools::TempMount;
use own_backup_backup::restore_disk_linux::partition_node;
use own_backup_common::gpt::{DiskLayout, GPT_TYPE_LINUX_FILESYSTEM};
use own_backup_common::restore_plan::{PartRef, RestoreOp, RestorePlan};
use own_backup_common::{tr, OwnBackupError, Result};

/// Where the Windows Boot Manager lives on an ESP.
const BOOTMGFW: &str = "\\EFI\\Microsoft\\Boot\\bootmgfw.efi";

pub(super) struct BootNote {
    pub(super) message: String,
    pub(super) follow_ups: Vec<String>,
}

pub(super) fn run_boot_op(
    op: &RestoreOp,
    plan: &RestorePlan,
    sidecar: &own_backup_backup::sidecar::Sidecar,
    device: &str,
) -> Result<BootNote> {
    match op {
        RestoreOp::EnsureFallbackBootloader { esp } => ensure_fallback(plan, device, *esp),
        RestoreOp::RestoreBootEntries => restore_boot_entries(plan, sidecar),
        RestoreOp::FinalizeBoot { windows_part, .. }
            if is_linux_partition(captured_layout(sidecar), plan, windows_part.index) =>
        {
            info!(
                part = windows_part.index,
                "the Windows boot step named a Linux partition: nothing to finish"
            );
            Ok(BootNote {
                message: tr!("restore_boot_linux.not_windows").into_owned(),
                follow_ups: Vec::new(),
            })
        }
        RestoreOp::FinalizeBoot {
            windows_part,
            esp,
            uefi,
        } => finalize_windows(plan, sidecar, device, *windows_part, *esp, *uefi),
        _ => Err(OwnBackupError::Config(format!(
            "not a boot step: {}",
            op.kind()
        ))),
    }
}

/// The node of a partition a boot step mounts read-write.
///
/// Found by number, so the number must still name the stretch of disk the
/// plan was checked against; and the partition must not be one the running
/// system stands on (its ESP, anything mounted or held), unless this plan
/// restored that partition — a restored ESP may well carry the running ESP's
/// PARTUUID, and is still ours to finish.
fn node_of(plan: &RestorePlan, device: &str, part: PartRef) -> Result<String> {
    let node = partition_node(device, part.index).ok_or_else(|| {
        OwnBackupError::Device(
            tr!("restore_exec_linux.no_partition_node", part = part.index).into_owned(),
        )
    })?;
    super::linux::check_partition_slot(&node, part)?;
    if !restores(plan, part.index) {
        boot_target_allowed(
            part.index,
            &node,
            &own_backup_restore::protection_of_device(device),
        )?;
    }
    Ok(node)
}

/// Does the plan write partition `index`'s data?
fn restores(plan: &RestorePlan, index: u32) -> bool {
    plan.ops
        .iter()
        .any(|op| matches!(op, RestoreOp::RestorePartition { dst, .. } if dst.index == index))
}

/// Refuse a boot step on a partition the running system uses, or when that
/// cannot be told.
fn boot_target_allowed(
    index: u32,
    node: &str,
    guard: &own_backup_restore::Protection,
) -> Result<()> {
    if let Some(why) = &guard.cannot_tell {
        return Err(OwnBackupError::RestoreIncompatible(
            tr!("restore_exec.cannot_tell_live_system", why = why).into_owned(),
        ));
    }
    if guard.parts.contains(&index) {
        return Err(OwnBackupError::RestoreIncompatible(
            tr!(
                "restore_boot_linux.boot_step_on_live_partition",
                part = index,
                node = node
            )
            .into_owned(),
        ));
    }
    Ok(())
}

fn mount_esp(plan: &RestorePlan, device: &str, esp: PartRef) -> Result<TempMount> {
    TempMount::mount(&node_of(plan, device, esp)?, &[("vfat", "rw")])
}

/// Filesystem UUIDs of the partitions the plan wrote, read off the disk.
fn restored_uuids(plan: &RestorePlan, device: &str) -> Vec<String> {
    let mut out = Vec::new();
    for op in &plan.ops {
        let RestoreOp::RestorePartition { dst, .. } = op else {
            continue;
        };
        let Some(node) = partition_node(device, dst.index) else {
            continue;
        };
        let Ok(dev) = own_backup_backup::linux_device::LinuxRawDevice::open_read(&node) else {
            continue;
        };
        let mut head = vec![0u8; own_backup_backup::fs_uuid::PROBE_LEN.min(dst.size as usize)];
        if dev.read_at(0, &mut head).is_ok() {
            if let Some(uuid) = own_backup_backup::fs_uuid::probe(&head) {
                info!(part = dst.index, %uuid, "restored filesystem identified");
                out.push(uuid);
            }
        }
    }
    out
}

fn ensure_fallback(plan: &RestorePlan, device: &str, esp: PartRef) -> Result<BootNote> {
    let mount = mount_esp(plan, device, esp)?;
    let outcome = own_backup_backup::esp_fallback_tree::ensure_at_root(
        mount.path(),
        &restored_uuids(plan, device),
        &format!("{device} #{}", esp.index),
    )?;
    Ok(BootNote {
        message: outcome.message,
        follow_ups: if outcome.copied && !outcome.replaced {
            vec![tr!("restore_exec.fallback_copied", from = outcome.from).into_owned()]
        } else {
            outcome.follow_ups
        },
    })
}

fn data_dir() -> PathBuf {
    own_backup_common::config::AppConfig::default().data_dir
}

fn restore_boot_entries(
    plan: &RestorePlan,
    sidecar: &own_backup_backup::sidecar::Sidecar,
) -> Result<BootNote> {
    let mut note = BootNote {
        message: String::new(),
        follow_ups: Vec::new(),
    };
    let Some(saved) = own_backup_backup::sidecar::boot_entries_of(sidecar, &plan.image_path) else {
        note.message = tr!("restore_exec.no_boot_entries").into_owned();
        note.follow_ups
            .push(tr!("restore_exec.no_boot_entries_follow_up").into_owned());
        return Ok(note);
    };
    let efi = Efivarfs::default();
    let (dir, durable) = safety_copy_dir(&data_dir());
    match efi.safety_copy(&dir, durable) {
        Ok(copy) => {
            let path = copy.path.display();
            note.follow_ups.push(
                if copy.durable {
                    tr!("restore_exec.entries_saved", path = path)
                } else {
                    tr!("restore_exec.entries_saved_temporary", path = path)
                }
                .into_owned(),
            );
        }
        Err(e) => {
            warn!(error = %e, "restore: could not take a safety copy of the boot entries");
            note.follow_ups
                .push(tr!("restore_exec.entries_save_failed", error = e).into_owned());
        }
    }
    let here = own_backup_common::media_mode::meaningful_machine_name().unwrap_or_default();
    if !here.is_empty() && !saved.machine.eq_ignore_ascii_case(&here) {
        note.follow_ups.push(
            tr!(
                "restore_exec.entries_other_machine",
                source = saved.machine,
                here = here
            )
            .into_owned(),
        );
    }
    let here_parts = own_backup_backup::efivars_linux::partition_guids_here(Path::new(
        own_backup_common::linux_block::SYS_CLASS_BLOCK,
    ));
    // Outside the boot medium the system running now booted from one of
    // these entries; the copy's version of it does not replace it.
    let keep_current = !own_backup_restore::is_boot_media();
    match efi.apply_import(&saved, &here_parts, keep_current) {
        Ok(done) => {
            if let Some(n) = done.kept_current {
                note.follow_ups.push(
                    tr!(
                        "restore_boot_linux.entries_kept_current",
                        entry = format!("{n:04X}")
                    )
                    .into_owned(),
                );
            }
            info!(written = done.written, machine = %saved.machine, "restore: boot entries written");
            note.message = tr!(
                "restore_exec.entries_restored",
                written = done.written,
                total = saved.variables.len()
            )
            .into_owned();
            if !done.skipped_orphaned.is_empty() {
                note.follow_ups.push(
                    tr!(
                        "restore_boot_linux.entries_orphaned",
                        entries = done.skipped_orphaned.join("; ")
                    )
                    .into_owned(),
                );
            }
        }
        Err(e) => {
            warn!(error = %e, "restore: boot entries could not be written");
            note.message = tr!("restore_exec.entries_not_restored").into_owned();
            note.follow_ups
                .push(tr!("restore_exec.entries_firmware_refused", error = e).into_owned());
        }
    }
    Ok(note)
}

/// The GPT on the target now.
fn target_layout(device: &str) -> Result<own_backup_common::gpt::DiskLayout> {
    let dev = own_backup_backup::linux_device::LinuxRawDevice::open_read(device)?;
    let sector = dev.sector_size();
    let want = (34 * sector as u64).min(dev.length()) as usize;
    let mut head = vec![0u8; want];
    dev.read_at(0, &mut head)?;
    own_backup_backup::gpt_reader::parse_disk_layout(&head, sector, dev.length())
}

fn same_guid(a: &str, b: &str) -> bool {
    let n = |s: &str| {
        s.trim()
            .trim_matches(|c| c == '{' || c == '}')
            .to_ascii_uppercase()
    };
    !a.trim().is_empty() && n(a) == n(b)
}

/// Does the restored disk carry the copy's own disk GUID, and do `parts`
/// (target index, source index) carry the copy's own partition GUIDs? Only
/// then does the copy's BCD still describe this disk.
fn guids_preserved(
    captured: Option<&own_backup_common::gpt::DiskLayout>,
    now: &own_backup_common::gpt::DiskLayout,
    parts: &[(u32, u32)],
) -> bool {
    let Some(captured) = captured else {
        return false;
    };
    if !same_guid(&captured.disk_guid, &now.disk_guid) {
        return false;
    }
    parts.iter().all(|(dst, src)| {
        let was = captured.partitions.iter().find(|p| p.index == *src);
        let is = now.partitions.iter().find(|p| p.index == *dst);
        matches!((was, is), (Some(w), Some(i)) if same_guid(&w.part_guid, &i.part_guid))
    })
}

fn captured_layout(sidecar: &own_backup_backup::sidecar::Sidecar) -> Option<&DiskLayout> {
    sidecar.disk_manifest.as_ref().map(|d| &d.layout)
}

/// Did target partition `dst` receive a partition the copy's own table calls a
/// Linux filesystem? The window offers the Windows boot step for the partition
/// the system ran from, and Linux has one of those as well.
fn is_linux_partition(layout: Option<&DiskLayout>, plan: &RestorePlan, dst: u32) -> bool {
    let Some(src) = source_of(plan, dst) else {
        return false;
    };
    layout
        .and_then(|l| l.partitions.iter().find(|p| p.index == src))
        .is_some_and(|p| p.type_guid.eq_ignore_ascii_case(GPT_TYPE_LINUX_FILESYSTEM))
}

/// Is there a Windows in this plan to finish? What is said about WinRE and
/// `reagentc` is for that case only.
pub(super) fn plan_restores_windows(
    plan: &RestorePlan,
    sidecar: &own_backup_backup::sidecar::Sidecar,
) -> bool {
    windows_step_is_real(captured_layout(sidecar), plan)
}

fn windows_step_is_real(layout: Option<&DiskLayout>, plan: &RestorePlan) -> bool {
    plan.ops.iter().any(|op| match op {
        RestoreOp::FinalizeBoot { windows_part, .. } => {
            !is_linux_partition(layout, plan, windows_part.index)
        }
        _ => false,
    })
}

fn source_of(plan: &RestorePlan, dst: u32) -> Option<u32> {
    plan.ops.iter().find_map(|op| match op {
        RestoreOp::RestorePartition {
            src_index, dst: d, ..
        } if d.index == dst => Some(*src_index),
        _ => None,
    })
}

/// Remove `Windows\bootstat.dat` under `root`, matching names the way Windows
/// does (case-insensitively). `Ok(false)` when it was not there.
fn remove_boot_status(root: &Path) -> std::io::Result<bool> {
    let find = |dir: &Path, name: &str| -> Option<PathBuf> {
        std::fs::read_dir(dir).ok()?.flatten().find_map(|e| {
            e.file_name()
                .to_string_lossy()
                .eq_ignore_ascii_case(name)
                .then(|| e.path())
        })
    };
    let Some(windows) = find(root, "Windows") else {
        return Ok(false);
    };
    let Some(file) = find(&windows, "bootstat.dat") else {
        return Ok(false);
    };
    std::fs::remove_file(file)?;
    Ok(true)
}

fn finalize_windows(
    plan: &RestorePlan,
    sidecar: &own_backup_backup::sidecar::Sidecar,
    device: &str,
    windows_part: PartRef,
    esp: Option<PartRef>,
    uefi: bool,
) -> Result<BootNote> {
    if !uefi {
        return Err(OwnBackupError::RestoreIncompatible(
            tr!("restore_boot_linux.windows_bios").into_owned(),
        ));
    }
    let esp = esp.ok_or_else(|| {
        OwnBackupError::RestoreIncompatible(tr!("restore_boot_linux.windows_no_esp").into_owned())
    })?;
    // Only a whole-disk copy says which GUIDs its BCD names.
    if sidecar.disk_manifest.is_none() {
        return Err(OwnBackupError::RestoreIncompatible(
            tr!("restore_boot_linux.windows_not_disk_copy").into_owned(),
        ));
    }
    let now = target_layout(device)?;
    let pairs: Vec<(u32, u32)> = [windows_part.index, esp.index]
        .iter()
        .filter_map(|d| source_of(plan, *d).map(|s| (*d, s)))
        .collect();
    let captured = sidecar.disk_manifest.as_ref().map(|d| &d.layout);
    if pairs.len() != 2 || !guids_preserved(captured, &now, &pairs) {
        return Err(OwnBackupError::RestoreIncompatible(
            tr!("restore_boot_linux.windows_guids_changed").into_owned(),
        ));
    }

    let mut follow_ups = Vec::new();
    // The boot status first: of the three it is the one a restart cannot
    // repair by itself.
    let cleared = match TempMount::mount(
        &node_of(plan, device, windows_part)?,
        &[("ntfs3", "rw"), ("ntfs-3g", "rw")],
    ) {
        Ok(m) => match remove_boot_status(m.path()) {
            Ok(c) => c,
            Err(e) => {
                warn!(error = %e, "bootstat.dat could not be removed");
                follow_ups.push(tr!("restore_boot_linux.boot_status_kept", error = e).into_owned());
                false
            }
        },
        Err(e) => {
            warn!(error = %e, "the Windows partition could not be mounted to clear its boot status");
            follow_ups.push(tr!("restore_boot_linux.boot_status_kept", error = e).into_owned());
            false
        }
    };

    let esp_mount = mount_esp(plan, device, esp)?;
    let loader = BOOTMGFW
        .split('\\')
        .filter(|c| !c.is_empty())
        .fold(esp_mount.path().to_path_buf(), |p, c| p.join(c));
    if !loader.is_file() {
        return Err(OwnBackupError::RestoreIncompatible(
            tr!("restore_boot_linux.no_windows_loader", path = BOOTMGFW).into_owned(),
        ));
    }
    let fallback = own_backup_backup::esp_fallback_tree::ensure_at_root(
        esp_mount.path(),
        &[],
        &format!("{device} #{}", esp.index),
    )?;
    if fallback.copied && !fallback.replaced {
        follow_ups.push(tr!("restore_exec.fallback_copied", from = fallback.from).into_owned());
    }
    drop(esp_mount);

    // This machine's firmware: an entry for the Windows Boot Manager on the
    // restored ESP, unless one already names exactly that file there.
    let efi = Efivarfs::default();
    let esp_entry = now.partitions.iter().find(|p| p.index == esp.index);
    match (efi.is_uefi(), esp_entry) {
        (true, Some(p)) => {
            let hd = own_backup_backup::efivars::HardDrive::from_gpt(
                p.index,
                p.first_lba,
                p.last_lba,
                &p.part_guid,
            )
            .ok_or_else(|| {
                OwnBackupError::Device(
                    tr!(
                        "restore_boot_linux.bad_partition_guid",
                        guid = p.part_guid.as_str()
                    )
                    .into_owned(),
                )
            })?;
            match efi.entry_for(&hd, BOOTMGFW) {
                Ok(Some(n)) => follow_ups.push(
                    tr!(
                        "restore_boot_linux.entry_present",
                        entry = format!("{n:04X}")
                    )
                    .into_owned(),
                ),
                Ok(None) => {
                    let (dir, durable) = safety_copy_dir(&data_dir());
                    match efi.apply_action(
                        Action::Create {
                            name: "Windows Boot Manager".into(),
                            hd,
                            file: BOOTMGFW.into(),
                            position: None,
                        },
                        &dir,
                        durable,
                    ) {
                        Ok(done) => {
                            follow_ups.push(done.message);
                            follow_ups
                                .push(tr!("restore_boot_linux.entry_last_in_menu").into_owned());
                        }
                        Err(e) => follow_ups.push(
                            tr!("restore_exec.entries_firmware_refused", error = e).into_owned(),
                        ),
                    }
                }
                Err(e) => follow_ups
                    .push(tr!("restore_exec.entries_firmware_refused", error = e).into_owned()),
            }
        }
        _ => follow_ups.push(tr!("restore_boot_linux.no_firmware_entry").into_owned()),
    }

    Ok(BootNote {
        message: if cleared {
            tr!("restore_exec.boot_status_cleared").into_owned()
        } else {
            String::new()
        },
        follow_ups,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use own_backup_common::gpt::GptPartitionEntry;
    use own_backup_common::restore_plan::RestoreMode;

    fn plan_finishing(dst: u32, src: u32) -> RestorePlan {
        let part = PartRef {
            index: dst,
            offset: 1 << 20,
            size: 1 << 20,
        };
        RestorePlan::new(
            1,
            "/tmp/img.vhdx".into(),
            vec![
                RestoreOp::RestorePartition {
                    src_index: src,
                    dst: part,
                    mode: RestoreMode::Sectors,
                    wipe_free_space: false,
                    reencrypt: false,
                },
                RestoreOp::FinalizeBoot {
                    windows_part: part,
                    esp: None,
                    uefi: true,
                },
            ],
        )
    }

    fn layout_of(index: u32, type_guid: &str) -> DiskLayout {
        DiskLayout {
            disk_guid: String::new(),
            sector_size: 512,
            size_bytes: 1 << 30,
            partitions: vec![GptPartitionEntry {
                type_guid: type_guid.into(),
                ..entry(index, "P")
            }],
        }
    }

    /// A Kubuntu restored from the window got the Windows step for its root
    /// partition, and with it advice about `reagentc` (owner, Hyper-V, 30.10).
    #[test]
    fn a_linux_root_is_not_windows_to_finish() {
        let plan = plan_finishing(3, 5);
        let linux = layout_of(5, "0fc63daf-8483-4772-8e79-3d69d8477de4");
        assert!(is_linux_partition(Some(&linux), &plan, 3));
        assert!(!windows_step_is_real(Some(&linux), &plan));
    }

    #[test]
    fn windows_is_still_finished() {
        let plan = plan_finishing(3, 5);
        let basic = layout_of(5, "EBD0A0A2-B9E5-4433-87C0-68B6B72699C7");
        assert!(windows_step_is_real(Some(&basic), &plan));
        // Nothing says otherwise: the step runs and answers for itself, as before.
        assert!(windows_step_is_real(None, &plan));
        let elsewhere = layout_of(9, "0FC63DAF-8483-4772-8E79-3D69D8477DE4");
        assert!(windows_step_is_real(Some(&elsewhere), &plan));
    }

    #[test]
    fn no_step_no_windows() {
        let plan = RestorePlan::new(1, "/tmp/img.vhdx".into(), Vec::new());
        assert!(!windows_step_is_real(None, &plan));
    }

    fn entry(index: u32, guid: &str) -> GptPartitionEntry {
        GptPartitionEntry {
            index,
            type_guid: String::new(),
            part_guid: guid.into(),
            first_lba: 2048 * index as u64,
            last_lba: 2048 * index as u64 + 2047,
            attributes: 0,
            name: String::new(),
        }
    }

    #[test]
    fn a_boot_step_is_refused_on_a_partition_the_running_system_uses() {
        use own_backup_restore::Protection;
        let live = Protection {
            parts: vec![1, 2],
            cannot_tell: None,
        };
        assert!(boot_target_allowed(3, "/dev/sdz3", &live).is_ok());
        let err = boot_target_allowed(1, "/dev/sdz1", &live).unwrap_err();
        assert!(matches!(err, OwnBackupError::RestoreIncompatible(_)));
        assert!(err.to_string().contains("/dev/sdz1"), "{err}");
        let unsure = Protection {
            parts: Vec::new(),
            cannot_tell: Some("mountinfo".into()),
        };
        assert!(boot_target_allowed(3, "/dev/sdz3", &unsure).is_err());

        // Only a partition the plan itself restores is exempt.
        let esp = PartRef {
            index: 1,
            offset: 1 << 20,
            size: 100 << 20,
        };
        let mut plan = RestorePlan::new(
            0,
            "/tmp/copy.qcow2".into(),
            vec![RestoreOp::EnsureFallbackBootloader { esp }],
        );
        assert!(!restores(&plan, 1));
        plan.ops.push(RestoreOp::RestorePartition {
            src_index: 1,
            dst: esp,
            mode: own_backup_common::restore_plan::RestoreMode::Sectors,
            wipe_free_space: false,
            reencrypt: false,
        });
        assert!(restores(&plan, 1));
    }

    #[test]
    fn the_boot_status_is_found_whatever_its_case() {
        let root = tempfile::tempdir().unwrap();
        let win = root.path().join("WINDOWS");
        std::fs::create_dir_all(&win).unwrap();
        std::fs::write(win.join("BootStat.DAT"), b"x").unwrap();
        assert!(remove_boot_status(root.path()).unwrap());
        assert!(!win.join("BootStat.DAT").exists());
        assert!(!remove_boot_status(root.path()).unwrap(), "gone now");
    }

    #[test]
    fn guids_count_as_preserved_only_when_disk_and_partitions_match() {
        let mut sidecar_layout = DiskLayout {
            disk_guid: "{AAAA0000-0000-0000-0000-000000000001}".into(),
            sector_size: 512,
            size_bytes: 1 << 30,
            partitions: vec![entry(1, "E5P"), entry(3, "W1N")],
        };
        let now = DiskLayout {
            disk_guid: "aaaa0000-0000-0000-0000-000000000001".into(),
            partitions: vec![entry(1, "e5p"), entry(2, "w1n")],
            ..sidecar_layout.clone()
        };
        // Target 2 received source 3.
        assert!(guids_preserved(
            Some(&sidecar_layout),
            &now,
            &[(1, 1), (2, 3)]
        ));
        assert!(!guids_preserved(
            Some(&sidecar_layout),
            &now,
            &[(1, 1), (2, 1)]
        ));
        assert!(!guids_preserved(None, &now, &[(1, 1), (2, 3)]));
        sidecar_layout.disk_guid = "{BBBB0000-0000-0000-0000-000000000001}".into();
        assert!(!guids_preserved(
            Some(&sidecar_layout),
            &now,
            &[(1, 1), (2, 3)]
        ));
    }
}
