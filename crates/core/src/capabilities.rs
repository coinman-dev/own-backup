//! What the backup engine of this build carries out — the one table both the
//! run (which refuses a job asking for more, [`refusals`]) and the window
//! (which offers only this, the service's `BackupCapabilities` RPC) read.
//!
//! A job the window builds from this answer passes [`refusals`] by
//! construction; a job saved elsewhere (CLI, an older window, a job file made
//! on Windows) is refused at run time with every reason at once, before
//! anything is touched. When the engine learns something, flip it here and
//! both sides follow.

use own_backup_common::tr;
use own_backup_common::types::ImageFormat;
use serde::Serialize;

use crate::Job;

/// The answer, as JSON for the window.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct BackupCapabilities {
    /// `windows` or `linux`: whose engine this is.
    pub os: &'static str,
    /// Backup methods a run carries out (`full`, `incremental`, `differential`).
    /// A chain also needs its container in [`Self::chain_formats`].
    pub methods: Vec<&'static str>,
    /// Containers a chain (incremental, differential) is written in: `vhdx`,
    /// `qcow2`, `obk` (an encrypted job). A full copy is written in any.
    pub chain_formats: Vec<&'static str>,
    /// Exclusion masks are applied (the excluded files are really removed, or
    /// the run says which ones could not be).
    pub exclusions: bool,
    /// One image of a whole disk (`disk_source`): the real partition table with
    /// the chosen volumes at their offsets. Without it each volume gets its own
    /// image file.
    pub whole_disk: bool,
    /// How such a disk must be named in `disk_source`: `number` (Windows,
    /// `PhysicalDriveN`) or `device` (Linux, `/dev/disk/by-id/…` in
    /// `disk_source.device` — decision R1; a number names no disk there).
    pub disk_named_by: &'static str,
    /// `advanced.speed_limit_mbps`.
    pub speed_limit: bool,
    /// `advanced.low_priority`.
    pub low_priority: bool,
    /// `advanced.after_backup` (sleep / power off).
    pub after_backup: bool,
    /// Encrypted archives (OBK): needs the machine's key store.
    pub archive_encryption: bool,
    /// What keeps that key store's keys on this machine: `dpapi` (Windows),
    /// `tpm2` or `host` (Linux, `systemd-creds`); empty when there is no
    /// store — `archive_encryption` is then false ([`with_key_store`]).
    pub archive_key_protection: &'static str,
    /// Event triggers the scheduler fires (`schedule.event_trigger`).
    pub schedule_events: Vec<&'static str>,
    /// `schedule.wake_to_run`: a wake timer is armed for the next slot.
    pub wake_to_run: bool,
    /// The machine is kept awake while a copy runs.
    pub keeps_awake: bool,
}

/// This build's answer.
pub fn current() -> BackupCapabilities {
    #[cfg(target_os = "linux")]
    {
        linux()
    }
    #[cfg(not(target_os = "linux"))]
    {
        windows()
    }
}

/// The Windows engine: everything the wizard has.
pub fn windows() -> BackupCapabilities {
    BackupCapabilities {
        os: "windows",
        methods: vec!["full", "incremental", "differential"],
        chain_formats: vec!["vhdx", "qcow2", "obk"],
        exclusions: true,
        whole_disk: true,
        disk_named_by: "number",
        speed_limit: true,
        low_priority: true,
        after_backup: true,
        archive_encryption: true,
        archive_key_protection: "dpapi",
        schedule_events: vec!["dest_connected", "logon", "logoff", "startup"],
        wake_to_run: true,
        keeps_awake: true,
    }
}

/// The Linux engine (`engine_linux` over the shared `engine_core`).
pub fn linux() -> BackupCapabilities {
    BackupCapabilities {
        os: "linux",
        methods: vec!["full", "incremental", "differential"],
        // qcow2 and `.obk` chains are the shared engine's (L2). A VHDX child
        // made on Linux waits for its Windows check (CHECKS 12, decision R2):
        // quietly writing full copies where a chain was asked for would be a
        // plan that is not one.
        chain_formats: vec!["qcow2", "obk"],
        // L3: ext/XFS/btrfs/FAT/NTFS on a source that holds still; a source
        // that does not is copied whole and the run says why.
        exclusions: true,
        // E6, named by its by-id device.
        whole_disk: true,
        disk_named_by: "device",
        // The pace is held in the copy's progress callback, the same code as
        // on Windows; low priority is `ioprio`/`nice` on the copying thread
        // (`power_linux::BackgroundIo`).
        speed_limit: true,
        low_priority: true,
        // logind: `Suspend`, `ScheduleShutdown` (stage 4).
        after_backup: true,
        // The key store (`seal/local_linux.rs`, machine key through
        // `systemd-creds`) together with the key directory left out of every
        // copy by the engine (decision 6/E4). Whether this machine has a
        // store is the service's to say ([`with_key_store`]); a run whose keys
        // cannot be opened says so (`archive_keys_for_run`).
        archive_encryption: true,
        archive_key_protection: "",
        // logon/logoff are logind's session signals, heard by the installed
        // engine (`logind_events`); the destination is watched in the mount
        // table, startup is a timer.
        schedule_events: vec!["dest_connected", "logon", "logoff", "startup"],
        // `CLOCK_REALTIME_ALARM` timer (`power_linux::WakeTimer`).
        wake_to_run: true,
        // logind `sleep` block inhibitor (`power_linux::KeepAwake`).
        keeps_awake: true,
    }
}

/// `caps` as this machine's key store answers: what protects it, or — when
/// there is no store (no `systemd-creds`, not the installed service, the boot
/// medium) — no encrypted archives offered at all.
pub fn with_key_store(
    mut caps: BackupCapabilities,
    protection: own_backup_common::Result<&'static str>,
) -> BackupCapabilities {
    match protection {
        Ok(p) => caps.archive_key_protection = p,
        Err(_) => {
            caps.archive_encryption = false;
            caps.archive_key_protection = "";
        }
    }
    caps
}

/// The container a chain of this job would be written in, by the names of
/// [`BackupCapabilities::chain_formats`].
fn chain_format(job: &Job) -> &'static str {
    if job.archive_encryption.is_some() {
        "obk"
    } else {
        match job.image_format {
            ImageFormat::Vhdx => "vhdx",
            ImageFormat::Qcow2 => "qcow2",
        }
    }
}

fn format_name(format: &str) -> &str {
    match format {
        "vhdx" => "VHDX",
        "obk" => "OBK",
        other => other,
    }
}

/// What in `job` an engine with `caps` would not carry out, one localized
/// reason each — empty when the job asks for nothing it would ignore. Only
/// options a run reads are checked; the schedule's wake timer and event are
/// the scheduler's, which simply does not fire what it cannot hear.
pub fn refusals(job: &Job, caps: &BackupCapabilities) -> Vec<String> {
    let mut what = Vec::new();
    let method = job.scheme.method.as_str();
    if method != "full" {
        let format = chain_format(job);
        if !caps.methods.contains(&method) || !caps.chain_formats.contains(&format) {
            let chains: Vec<&str> = caps.chain_formats.iter().map(|f| format_name(f)).collect();
            what.push(
                tr!(
                    "lib.linux_unsupported_scheme",
                    format = format_name(format),
                    chains = chains.join(", ")
                )
                .into_owned(),
            );
        }
    }
    if job.exclusions.enabled && !caps.exclusions {
        what.push(tr!("lib.linux_unsupported_exclusions").into_owned());
    }
    if let Some(ds) = &job.disk_source {
        if !caps.whole_disk {
            what.push(tr!("lib.linux_unsupported_disk").into_owned());
        } else if caps.disk_named_by == "device" && ds.device.is_none() {
            // A job that names its disk by number only was made for Windows.
            what.push(tr!("lib.linux_unsupported_whole_disk").into_owned());
        }
    }
    if job.advanced.speed_limit_mbps > 0 && !caps.speed_limit {
        what.push(tr!("lib.linux_unsupported_speed_limit").into_owned());
    }
    if job.advanced.low_priority && !caps.low_priority {
        what.push(tr!("lib.linux_unsupported_low_priority").into_owned());
    }
    if matches!(job.advanced.after_backup.as_str(), "sleep" | "shutdown") && !caps.after_backup {
        what.push(tr!("lib.linux_unsupported_after_backup").into_owned());
    }
    if job.archive_encryption.is_some() && !caps.archive_encryption {
        what.push(tr!("lib.linux_unsupported_encryption").into_owned());
    }
    what
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows_offers_everything_the_wizard_has() {
        let c = windows();
        assert_eq!(c.methods, ["full", "incremental", "differential"]);
        assert_eq!(c.chain_formats, ["vhdx", "qcow2", "obk"]);
        assert!(c.exclusions && c.whole_disk && c.speed_limit && c.low_priority);
        assert!(c.after_backup && c.archive_encryption && c.wake_to_run && c.keeps_awake);
        assert_eq!(c.disk_named_by, "number");
        assert_eq!(c.schedule_events.len(), 4);
    }

    #[test]
    fn current_is_this_platform() {
        let os = if cfg!(target_os = "linux") {
            "linux"
        } else {
            "windows"
        };
        assert_eq!(current().os, os);
    }

    fn job() -> Job {
        Job {
            image_format: ImageFormat::Qcow2,
            ..Default::default()
        }
    }

    /// Windows refuses nothing a job can hold.
    #[test]
    fn windows_refuses_nothing() {
        let mut j = job();
        j.image_format = ImageFormat::Vhdx;
        j.scheme.method = "incremental".into();
        j.exclusions.enabled = true;
        j.advanced.speed_limit_mbps = 10;
        j.advanced.low_priority = true;
        j.advanced.after_backup = "shutdown".into();
        j.disk_source = Some(crate::DiskSourceSpec {
            disk_number: 1,
            device: None,
            disk_model: String::new(),
            parts: Vec::new(),
            bootable: false,
        });
        assert!(refusals(&j, &windows()).is_empty());
    }

    /// The Linux table refuses exactly what its engine does not do: a VHDX
    /// chain, a disk named by number — and nothing else. Encrypted archives
    /// are offered once the key store is (decision 6/E4).
    #[test]
    fn linux_refuses_vhdx_chains_and_numbered_disks_only() {
        let caps = linux();
        let mut j = job();
        j.scheme.method = "incremental".into();
        j.exclusions.enabled = true;
        j.advanced.speed_limit_mbps = 10;
        j.advanced.low_priority = true;
        j.advanced.after_backup = "sleep".into();
        assert!(refusals(&j, &caps).is_empty(), "{:?}", refusals(&j, &caps));

        j.scheme.method = "differential".into();
        assert!(refusals(&j, &caps).is_empty());
        j.image_format = ImageFormat::Vhdx;
        let r = refusals(&j, &caps);
        assert_eq!(r.len(), 1);
        assert!(r[0].contains("VHDX") && r[0].contains("qcow2"), "{}", r[0]);
        j.scheme.method = "full".into();
        assert!(refusals(&j, &caps).is_empty());

        let mut ds = crate::DiskSourceSpec {
            disk_number: 0,
            device: Some("/dev/disk/by-id/virtio-x".into()),
            disk_model: String::new(),
            parts: Vec::new(),
            bootable: false,
        };
        j.disk_source = Some(ds.clone());
        assert!(refusals(&j, &caps).is_empty());
        ds.device = None;
        j.disk_source = Some(ds);
        assert_eq!(
            refusals(&j, &caps),
            [tr!("lib.linux_unsupported_whole_disk").into_owned()]
        );
    }

    /// Without a key store the window is not offered encryption; with one it
    /// is told what protects it.
    #[test]
    fn the_key_store_decides_whether_encryption_is_offered() {
        let without = with_key_store(
            linux(),
            Err(own_backup_common::OwnBackupError::Encryption(
                "no systemd-creds".into(),
            )),
        );
        assert!(!without.archive_encryption);
        assert_eq!(without.archive_key_protection, "");
        let with = with_key_store(linux(), Ok("tpm2"));
        assert!(with.archive_encryption);
        assert_eq!(with.archive_key_protection, "tpm2");
        assert!(with.chain_formats.contains(&"obk"));
    }

    /// What the window is told it may offer is what a run accepts: a job
    /// using every offered option passes.
    #[test]
    fn a_job_built_from_the_answer_passes() {
        for caps in [windows(), linux()] {
            for method in &caps.methods {
                for format in &caps.chain_formats {
                    if *format == "obk" && !caps.archive_encryption {
                        continue;
                    }
                    let mut j = job();
                    j.scheme.method = (*method).into();
                    j.image_format = if *format == "vhdx" {
                        ImageFormat::Vhdx
                    } else {
                        ImageFormat::Qcow2
                    };
                    j.exclusions.enabled = caps.exclusions;
                    j.advanced.low_priority = caps.low_priority;
                    if caps.speed_limit {
                        j.advanced.speed_limit_mbps = 50;
                    }
                    if caps.after_backup {
                        j.advanced.after_backup = "shutdown".into();
                    }
                    assert!(
                        refusals(&j, &caps).is_empty(),
                        "{} {method} {format}",
                        caps.os
                    );
                }
            }
        }
    }
}
