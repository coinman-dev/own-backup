//! Unattended integrity checks. History and failed-image state are kept apart
//! from backup runs, so a successful check can never become a chain parent.
use std::path::Path;

use own_backup_common::{
    cancel::CancelToken,
    engine_dto::{JobVerifyImage, JobVerifySummary},
    tr, OwnBackupError, Result,
};
use serde::{Deserialize, Serialize};

#[cfg(target_os = "windows")]
use crate::JobProgress;

use crate::{EventSink, Job, JobRun, JobStore, ScheduleConfig};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct VerificationConfig {
    pub schedule: ScheduleConfig,
    /// Every restore point, or the latest of each source (including parents).
    pub all_versions: bool,
}
impl Default for VerificationConfig {
    fn default() -> Self {
        Self {
            schedule: ScheduleConfig::default(),
            all_versions: true,
        }
    }
}

impl VerificationConfig {
    pub fn validate(&self) -> std::result::Result<(), String> {
        let cfg = &self.schedule;
        if cfg.kind == "none" {
            return Ok(());
        }
        let time_ok = cfg.time.split_once(':').is_some_and(|(h, m)| {
            matches!((h.parse::<u32>(), m.parse::<u32>()), (Ok(h), Ok(m)) if h < 24 && m < 60)
        });
        let days_ok = cfg.kind != "weekly"
            || (!cfg.days.is_empty()
                && cfg.days.iter().all(|d| {
                    matches!(
                        d.as_str(),
                        "mon" | "tue" | "wed" | "thu" | "fri" | "sat" | "sun"
                    )
                }));
        if !matches!(cfg.kind.as_str(), "daily" | "weekly" | "monthly")
            || !time_ok
            || !days_ok
            || !(1..=28).contains(&cfg.day_of_month)
            || !(1..=60).contains(&cfg.missed_delay_min)
        {
            return Err(tr!("verification.invalid_schedule").into_owned());
        }
        Ok(())
    }
}

pub(crate) fn same_image(a: &str, b: &str) -> bool {
    #[cfg(target_os = "windows")]
    {
        a.replace('/', "\\")
            .eq_ignore_ascii_case(&b.replace('/', "\\"))
    }
    #[cfg(not(target_os = "windows"))]
    {
        a == b
    }
}

pub(crate) fn chain_is_invalid(job: &Job, chain: &own_backup_backup::chains::Chain) -> bool {
    job.verification_blocked
        || chain.images.iter().any(|image| {
            job.invalid_images
                .iter()
                .any(|p| same_image(p, &image.image_path.to_string_lossy()))
        })
}

/// Called while holding the shared operation reservation. No source-volume
/// password is needed; only the service's local archive key opens OBK.
pub fn perform(store: &JobStore, job: &Job, cancel: &CancelToken, sink: &dyn EventSink) -> JobRun {
    let keys = store.archive_keys_for_run(job);
    perform_with(store, job, cancel, |path, index, count| {
        let keys = keys
            .as_ref()
            .map_err(|message| OwnBackupError::Config(message.clone()))?;
        #[cfg(target_os = "windows")]
        {
            use own_backup_backup::engine_win::{verify_image_scoped, VerifyScope};
            let mut last_percent = u64::MAX;
            let mut last_emit = std::time::Instant::now();
            let found = verify_image_scoped(
                path,
                keys.as_ref(),
                VerifyScope::Everything,
                |done, total| {
                    if cancel.is_cancelled() {
                        return Err(OwnBackupError::Cancelled);
                    }
                    let percent = (index as u64 * 100
                        + if total > 0 { done * 100 / total } else { 0 })
                        / count as u64;
                    if percent != last_percent || last_emit.elapsed().as_millis() >= 400 {
                        last_percent = percent;
                        last_emit = std::time::Instant::now();
                        sink.job_progress(JobProgress {
                            job_id: job.id.clone(),
                            phase: "scheduled-verify".into(),
                            percent,
                            blocks_done: done,
                            blocks_total: total,
                            speed_mbps: 0.0,
                            volume_index: index as u64,
                            volume_count: count as u64,
                            volume: job.name.clone(),
                            image_file_name: path
                                .file_name()
                                .unwrap_or_default()
                                .to_string_lossy()
                                .into_owned(),
                            categories: vec![],
                            excluded_files: None,
                            excluded_bytes: None,
                        });
                    }
                    if cancel.is_cancelled() {
                        Err(OwnBackupError::Cancelled)
                    } else {
                        Ok(())
                    }
                },
            )?;
            Ok(JobVerifyImage {
                image_path: path.display().to_string(),
                file_name: path
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned(),
                valid: found.valid(),
                blocks_checked: found.blocks_checked,
                mismatched_blocks: found.mismatched_blocks.len() as u64,
                error: String::new(),
            })
        }
        #[cfg(not(target_os = "windows"))]
        {
            let _ = (keys, path, index, count, sink);
            Err(OwnBackupError::Config(
                tr!("verification.windows_only").into_owned(),
            ))
        }
    })
}

/// The same driver with a supplied image reader for host-independent tests.
fn perform_with(
    store: &JobStore,
    job: &Job,
    cancel: &CancelToken,
    mut verify: impl FnMut(&Path, usize, usize) -> Result<JobVerifyImage>,
) -> JobRun {
    let started = std::time::Instant::now();
    let mut run = JobRun {
        operation: "verify".into(),
        id: uuid::Uuid::new_v4().to_string(),
        job_id: job.id.clone(),
        job_name: job.name.clone(),
        started_at: chrono::Utc::now().to_rfc3339(),
        ..Default::default()
    };
    let result = (|| -> Result<()> {
        if cancel.is_cancelled() {
            return Err(OwnBackupError::Cancelled);
        }
        let targets = crate::job_verify_targets(store, job, job.verification.all_versions)?;
        if targets.is_empty() {
            return Err(OwnBackupError::Config(
                tr!("verification.no_copies").into_owned(),
            ));
        }
        let mut summary = JobVerifySummary {
            valid: true,
            images: Vec::new(),
            duration_secs: 0.0,
        };
        for (index, (path, _)) in targets.iter().enumerate() {
            let outcome = if cancel.is_cancelled() {
                Err(OwnBackupError::Cancelled)
            } else {
                verify(path, index, targets.len())
            };
            let image = match outcome {
                Ok(image) => image,
                Err(OwnBackupError::Cancelled) => {
                    summary.valid = false;
                    summary.duration_secs = started.elapsed().as_secs_f64();
                    run.verification = Some(summary);
                    return Err(OwnBackupError::Cancelled);
                }
                Err(e) => JobVerifyImage {
                    image_path: path.display().to_string(),
                    file_name: path
                        .file_name()
                        .unwrap_or_default()
                        .to_string_lossy()
                        .into_owned(),
                    valid: false,
                    blocks_checked: 0,
                    mismatched_blocks: 0,
                    error: e.to_string(),
                },
            };
            summary.valid &= image.valid;
            run.used_blocks += image.blocks_checked;
            summary.images.push(image);
        }
        summary.duration_secs = started.elapsed().as_secs_f64();
        let valid = summary.valid;
        if cancel.is_cancelled() {
            summary.valid = false;
        }
        run.verification = Some(summary);
        if cancel.is_cancelled() {
            return Err(OwnBackupError::Cancelled);
        }
        if valid {
            Ok(())
        } else {
            Err(OwnBackupError::Config(
                tr!("verification.damaged").into_owned(),
            ))
        }
    })();
    run.status = match &result {
        Ok(()) => "success",
        Err(OwnBackupError::Cancelled) => "cancelled",
        Err(_) => "error",
    }
    .into();
    if let Err(e) = result {
        run.error = e.to_string();
    }
    run.finished_at = chrono::Utc::now().to_rfc3339();
    run.duration_secs = started.elapsed().as_secs_f64();
    run
}

#[cfg(test)]
mod tests {
    use super::*;
    use own_backup_backup::sidecar::ChainRole;

    fn fixture() -> (tempfile::TempDir, std::sync::Arc<JobStore>, Job) {
        let dir = tempfile::tempdir().unwrap();
        let store = JobStore::load(dir.path());
        let mut job = Job {
            id: "verify-job".into(),
            name: "job".into(),
            source: "C:".into(),
            dest_folder: dir.path().display().to_string(),
            ..Default::default()
        };
        job.scheme.method = "incremental".into();
        let job = store.upsert(job).unwrap();
        crate::tests::put_real_image(
            dir.path(),
            "job_C_full_20261001-120000.qcow2",
            ChainRole::Full,
            None,
            &job.id,
            "2026-10-01T12:00:00Z",
        );
        crate::tests::put_real_image(
            dir.path(),
            "job_C_inc_20261002-120000.qcow2",
            ChainRole::Incremental,
            Some("job_C_full_20261001-120000.qcow2"),
            &job.id,
            "2026-10-02T12:00:00Z",
        );
        (dir, store, job)
    }
    fn checked(path: &Path, valid: bool) -> JobVerifyImage {
        JobVerifyImage {
            image_path: path.display().to_string(),
            file_name: path.file_name().unwrap().to_string_lossy().into_owned(),
            valid,
            blocks_checked: 8,
            mismatched_blocks: u64::from(!valid),
            error: String::new(),
        }
    }

    #[test]
    fn corruption_survives_restart_and_forces_a_new_chain_for_volume_and_disk() {
        let (dir, store, job) = fixture();
        let backup = JobRun {
            id: "backup".into(),
            job_id: job.id.clone(),
            status: "success".into(),
            image_path: "backup".into(),
            ..Default::default()
        };
        store.record_run(backup).unwrap();
        assert_eq!(
            crate::plan_run_for_volume(&job, dir.path(), "C:").kind,
            "incremental"
        );
        let run = perform_with(&store, &job, &CancelToken::default(), |path, _, _| {
            Ok(checked(path, false))
        });
        assert_eq!(run.status, "error");
        assert_eq!(run.verification.as_ref().unwrap().images.len(), 2);
        store.record_run(run).unwrap();
        let restored = JobStore::load(dir.path());
        let mut job = restored.get(&job.id).unwrap();
        assert_eq!(job.last_run.as_ref().unwrap().id, "backup");
        assert_eq!(restored.last_success(&job.id).unwrap().id, "backup");
        assert_eq!(
            crate::plan_run_for_volume(&job, dir.path(), "C:").kind,
            "full"
        );
        job.scheme.method = "differential".into();
        assert_eq!(
            crate::plan_run_for_volume(&job, dir.path(), "C:").kind,
            "full"
        );
        crate::tests::put_real_image(
            dir.path(),
            "job_DISK4_full_20261001-120000.qcow2",
            ChainRole::Full,
            None,
            &job.id,
            "2026-10-01T12:00:00Z",
        );
        assert_eq!(
            crate::plan_run_for_disk(&job, dir.path(), 4).kind,
            "differential"
        );
        job.invalid_images.push(
            dir.path()
                .join("job_DISK4_full_20261001-120000.qcow2")
                .display()
                .to_string(),
        );
        assert_eq!(crate::plan_run_for_disk(&job, dir.path(), 4).kind, "full");
        // A caller cannot erase failed-image state by saving a stale draft.
        let saved = restored
            .upsert(Job {
                invalid_images: vec![],
                last_verification: None,
                ..job
            })
            .unwrap();
        assert_eq!(saved.invalid_images.len(), 2);
        assert!(saved.last_verification.is_some());
    }

    #[test]
    fn a_valid_check_preserves_backup_history_and_can_clear_repaired_images() {
        let (_dir, store, job) = fixture();
        let run = perform_with(&store, &job, &CancelToken::default(), |path, _, _| {
            Ok(checked(path, false))
        });
        store.record_run(run).unwrap();
        let job = store.get(&job.id).unwrap();
        let run = perform_with(&store, &job, &CancelToken::default(), |path, _, _| {
            Ok(checked(path, true))
        });
        assert_eq!(run.status, "success");
        store.record_run(run).unwrap();
        assert!(store.get(&job.id).unwrap().invalid_images.is_empty());
        assert!(store.last_success(&job.id).is_none());
        assert!(store.get(&job.id).unwrap().last_run.is_none());
    }

    #[test]
    fn cancellation_preserves_findings_and_does_not_call_an_incomplete_check_valid() {
        let (_dir, store, job) = fixture();
        let cancel = CancelToken::default();
        let run = perform_with(&store, &job, &cancel, |path, _, _| {
            cancel.cancel();
            Ok(checked(path, false))
        });
        assert_eq!(run.status, "cancelled");
        assert_eq!(run.verification.as_ref().unwrap().images.len(), 1);
        store.record_run(run).unwrap();
        assert_eq!(store.get(&job.id).unwrap().invalid_images.len(), 1);
    }

    #[test]
    fn unreadable_catalog_blocks_incremental_planning_and_missing_copies_are_errors() {
        let (dir, store, job) = fixture();
        std::fs::write(
            own_backup_backup::sidecar_path_for(
                &dir.path().join("job_C_inc_20261002-120000.qcow2"),
            ),
            b"broken",
        )
        .unwrap();
        let run = perform_with(&store, &job, &CancelToken::default(), |_, _, _| {
            panic!("unreadable catalog must stop before reading")
        });
        assert_eq!(run.status, "error");
        store.record_run(run).unwrap();
        assert!(store.get(&job.id).unwrap().verification_blocked);
        let empty = Job {
            dest_folder: dir.path().join("missing").display().to_string(),
            ..job
        };
        let run = perform_with(&store, &empty, &CancelToken::default(), |_, _, _| {
            panic!("no copies")
        });
        assert_eq!(run.status, "error");
    }

    #[test]
    fn a_fresh_full_chain_can_be_used_while_the_old_corrupt_chain_stays_on_disk() {
        let (dir, store, job) = fixture();
        let run = perform_with(&store, &job, &CancelToken::default(), |path, _, _| {
            Ok(checked(path, false))
        });
        store.record_run(run).unwrap();
        let job = store.get(&job.id).unwrap();
        crate::tests::put_real_image(
            dir.path(),
            "job_C_full_20261007-120000.qcow2",
            ChainRole::Full,
            None,
            &job.id,
            "2026-10-07T12:00:00Z",
        );
        let plan = crate::plan_run_for_volume(&job, dir.path(), "C:");
        assert_eq!(plan.kind, "incremental");
        assert!(plan
            .parent_image
            .unwrap()
            .ends_with("job_C_full_20261007-120000.qcow2"));
    }

    #[test]
    fn malformed_job_rows_and_duplicate_identities_refuse_startup_without_rewriting_data() {
        let dir = tempfile::tempdir().unwrap();
        let jobs = dir.path().join("jobs.json");
        for bytes in [
            br#"[{"value": [{"id":"real-job"}], "Count":1}]"#.as_slice(),
            br#"[{"id":"same"}, {"id":"same"}]"#.as_slice(),
        ] {
            std::fs::write(&jobs, bytes).unwrap();
            let result = JobStore::try_load(dir.path());
            assert!(
                result.is_err(),
                "bad rows must not reach the authorization sweep"
            );
            assert_eq!(
                result.err().unwrap().kind(),
                std::io::ErrorKind::InvalidData
            );
            assert_eq!(std::fs::read(&jobs).unwrap(), bytes);
        }
    }

    #[test]
    fn invalid_verification_schedules_are_rejected_and_legacy_jobs_stay_manual() {
        let legacy: Job = serde_json::from_str(r#"{"name":"old"}"#).unwrap();
        assert_eq!(legacy.verification.schedule.kind, "none");
        assert!(legacy.last_verification.is_none());
        let mut cfg = VerificationConfig::default();
        for kind in ["event", "nonstop", "invalid"] {
            cfg.schedule.kind = kind.into();
            assert!(cfg.validate().is_err());
        }
        cfg.schedule.kind = "weekly".into();
        cfg.schedule.days.clear();
        assert!(cfg.validate().is_err());
        cfg.schedule.days = vec!["wed".into()];
        assert!(cfg.validate().is_ok());
        cfg.schedule.time = "25:00".into();
        assert!(cfg.validate().is_err());
        let dir = tempfile::tempdir().unwrap();
        let store = JobStore::load(dir.path());
        assert!(store
            .upsert(Job {
                verification: cfg,
                ..legacy
            })
            .is_err());
        assert!(store.list().is_empty());
    }
}
