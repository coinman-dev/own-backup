//! How far a whole run has got — the copy, its check, and the consolidation
//! after it — as one number for the round indicator.
//!
//! Each event says how far its own phase is. The indicator used to put the
//! copy at 0–50 %, the check at 50–100 %, and everything after at 100 %:
//! when consolidation was a matter of minutes that was near enough. On
//! 18.11.2026 the run's consolidation took 45 of its 65 minutes, and the
//! indicator said 100 % for all of them.
//!
//! So the run is outlined before it starts, from what is already on the
//! backup disk: whether this copy will push the chain over its window, which
//! increment that folds, whether the fold rebuilds the base. Each phase gets
//! a weight in "reads and writes of the base", and every event is stamped
//! with the share of the whole that is behind it. The number does not go
//! back — except when a phase the outline did not foresee starts: it is added
//! then, and the share is counted anew rather than left standing at 100 %.
//!
//! A step an earlier run left half-done (its marker still beside the base)
//! is finished first, before the copy. It used to be left out, and the
//! indicator showed that step's own percent instead: on 27.11.2026 a step
//! interrupted by a shutdown two days before took the first 40 minutes of
//! the run, and the indicator ran up with its rebuild and fell back to 0
//! when its check began. It is outlined now too, from its marker.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use crate::{EventSink, Job, JobProgress, JobRun, QueueSnapshot};

/// Weights of a run's phases, in reads of the full copy.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Outline {
    /// One (fold, check) per step left half-done by an earlier run, finished
    /// before the copy.
    pub pending: Vec<(f64, f64)>,
    pub copy: f64,
    pub verify: f64,
    /// One (fold, check) per consolidation step the run will take.
    pub steps: Vec<(f64, f64)>,
}

/// A copy that changes little reads the source whole and writes little: on
/// the owner's machine an increment's copy took 5 minutes where reading the
/// full copy back took 13.
const INCREMENT_COPY: f64 = 0.4;

impl Outline {
    /// What the run is expected to do, from the job and the chain on disk.
    /// `None` when there is nothing to go by — several volumes, each with
    /// its own copy and check, are counted the old way.
    pub(crate) fn of(
        job: &Job,
        keys: Option<&own_backup_backup::seal::context::ArchiveKeys>,
    ) -> Option<Self> {
        if job.disk_source.is_none() && job.source_list().len() > 1 {
            return None;
        }
        let folder = PathBuf::from(&job.dest_folder);
        let pending_steps = own_backup_backup::consolidate::pending_steps(&folder);
        let pending = pending_steps
            .iter()
            .map(|step| {
                use own_backup_backup::consolidate::FoldCheck;
                let check = step.check.map(|c| c == FoldCheck::Whole);
                weigh_step(&step.base, &step.victim, keys, check)
            })
            .collect();
        let mut outline = Self::of_chain(job, &folder, keys, &pending_steps);
        outline.pending = pending;
        Some(outline)
    }

    /// The run after the pending steps: the copy, its check, the
    /// consolidation it brings. The pending steps' increments are counted as
    /// gone — they are, by the time the copy starts.
    fn of_chain(
        job: &Job,
        folder: &Path,
        keys: Option<&own_backup_backup::seal::context::ArchiveKeys>,
        pending: &[own_backup_backup::consolidate::PendingStep],
    ) -> Self {
        let chain = own_backup_backup::chains::list_job_chains(folder, &job.id)
            .unwrap_or_default()
            .into_iter()
            .map(|mut c| {
                c.images
                    .retain(|i| !pending.iter().any(|p| p.victim == i.image_path));
                c
            })
            .filter(|c| !c.images.is_empty())
            .max_by(|a, b| {
                let newest = |c: &own_backup_backup::chains::Chain| {
                    c.images
                        .last()
                        .map(|i| i.created_at.clone())
                        .unwrap_or_default()
                };
                newest(a).cmp(&newest(b))
            });
        let size = |p: &Path| std::fs::metadata(p).map(|m| m.len() as f64).unwrap_or(0.0);
        let Some(chain) = chain else {
            return Self::first(job);
        };
        let base_size = size(&chain.images[0].image_path);
        if base_size <= 0.0 {
            return Self::first(job);
        }
        let increments = chain.images.len() - 1;
        let incremental = job.scheme.method != "full"
            && (job.scheme.is_consolidation()
                || job
                    .scheme
                    .rolls_over_after()
                    .is_none_or(|n| chain.increments_ever() < n));
        if !incremental {
            return Self::first(job);
        }
        let share = |p: &Path| (size(p) / base_size).clamp(0.0, 4.0);
        let typical_increment = if increments > 0 {
            chain.images[1..]
                .iter()
                .map(|i| share(&i.image_path))
                .sum::<f64>()
                / increments as f64
        } else {
            0.1
        };
        let verify = if !job.advanced.validate_after {
            0.0
        } else if job.advanced.verify_latest_only {
            typical_increment.clamp(0.05, 1.0)
        } else {
            1.0
        };

        let mut steps = Vec::new();
        if let Some(keep) = job
            .scheme
            .rolling_incrementals
            .filter(|_| job.scheme.is_consolidation())
        {
            let over = (increments + 1).saturating_sub(keep as usize);
            // The check the job asks of a step: none, only what the fold
            // wrote, or the whole base after a rebuild.
            let check = job
                .advanced
                .validate_after
                .then_some(!job.advanced.verify_latest_only);
            for victim in chain.images.iter().skip(1).take(over) {
                steps.push(weigh_step(
                    &chain.images[0].image_path,
                    &victim.image_path,
                    keys,
                    check,
                ));
            }
        }
        Self {
            pending: Vec::new(),
            copy: INCREMENT_COPY,
            verify,
            steps,
        }
    }

    /// A full copy: written whole, then read back whole.
    fn first(job: &Job) -> Self {
        Self {
            pending: Vec::new(),
            copy: 1.0,
            verify: if job.advanced.validate_after {
                1.0
            } else {
                0.0
            },
            steps: Vec::new(),
        }
    }
}

/// A consolidation step's (fold, check) in reads of the base: `increment`
/// folded into `base`. `check`: `None` no read-back, `Some(true)` the whole
/// base after a rebuild, `Some(false)` only what the fold wrote.
fn weigh_step(
    base: &Path,
    increment: &Path,
    keys: Option<&own_backup_backup::seal::context::ArchiveKeys>,
    check: Option<bool>,
) -> (f64, f64) {
    let size = |p: &Path| std::fs::metadata(p).map(|m| m.len() as f64).unwrap_or(0.0);
    let base_size = size(base);
    let share = if base_size > 0.0 {
        (size(increment) / base_size).clamp(0.0, 4.0)
    } else {
        0.1
    };
    let rebuilds = increment
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("obk"))
        && keys
            .and_then(|k| k.open_chain_of(base).ok())
            .and_then(|s| {
                own_backup_backup::seal::obk::fold_leaves_base_worth_compacting(
                    base, increment, &s.key,
                )
                .ok()
            })
            .unwrap_or(false);
    // A rebuild reads the base and the increment and writes the new base. A
    // fold in place reads, re-encrypts and appends the increment: on
    // 19.11.2026 2.3 minutes for 2.5 GB, where reading the whole 93 GB chain
    // back took 14.
    let fold = if rebuilds { 2.0 + share } else { 5.0 * share };
    // After a rebuild the whole base is read; after a fold in place only what
    // the fold wrote (`consolidate::execute_with`) — 21 seconds that same
    // morning.
    let check = match check {
        None => 0.0,
        Some(true) if rebuilds => 1.0,
        Some(_) => share.clamp(0.02, 1.0),
    };
    (fold, check)
}

/// Where a consolidation event stands among `steps`: the weight behind it.
/// `at` is the step under way and whether its check has begun. A step past
/// the outline's end is added as an ordinary fold and check; `grew` says so.
fn within_steps(
    steps: &mut Vec<(f64, f64)>,
    at: &mut Option<(usize, bool)>,
    p: &JobProgress,
    frac: f64,
) -> (f64, bool) {
    let checking = p.phase == "consolidate-verify";
    let index = match *at {
        None => 0,
        // Folding again after this step's check: the next step.
        Some((i, true)) if !checking => i + 1,
        Some((i, _)) => i,
    };
    *at = Some((index, checking || at.is_some_and(|(i, c)| i == index && c)));
    let mut grew = false;
    while steps.len() <= index {
        // Not foreseen: an ordinary fold and check.
        steps.push((1.0, 1.0));
        grew = true;
    }
    let (fold, check) = steps[index];
    let before: f64 = steps[..index].iter().map(|(f, c)| f + c).sum();
    let behind = if checking {
        before + fold + check * frac
    } else {
        // A fold in place has no count: it stands at its start.
        before + fold * if p.blocks_total > 0 { frac } else { 0.0 }
    };
    (behind, grew)
}

/// Where the run is, for [`Overall::note`].
#[derive(Debug)]
pub(crate) struct Overall {
    outline: Outline,
    /// The copy has begun: consolidation events before it are steps left by
    /// an earlier run, finished first ([`Outline::pending`]).
    started: bool,
    /// The left-over step under way, and whether its check has begun.
    pending_step: Option<(usize, bool)>,
    /// The run's own consolidation step under way, and whether its check
    /// has begun.
    step: Option<(usize, bool)>,
    shown: f64,
}

impl Overall {
    pub(crate) fn new(outline: Outline) -> Self {
        Self {
            outline,
            started: false,
            pending_step: None,
            step: None,
            shown: 0.0,
        }
    }

    /// The whole run's percent after `p`, or `None` when `p` is not part of
    /// the outline.
    pub(crate) fn note(&mut self, p: &JobProgress) -> Option<u64> {
        let frac = (p.percent as f64 / 100.0).clamp(0.0, 1.0);
        let pending: f64 = self.outline.pending.iter().map(|(f, c)| f + c).sum();
        let at = match p.phase.as_str() {
            "prepare" => {
                self.started = true;
                pending
            }
            "backup" => {
                self.started = true;
                let volumes = p.volume_count.max(1) as f64;
                pending + self.outline.copy * ((p.volume_index as f64 + frac) / volumes).min(1.0)
            }
            "finalize" | "release" | "exclusions-open" | "exclusions" if self.started => {
                pending + self.outline.copy
            }
            "verify" if self.started => {
                if self.outline.verify == 0.0 {
                    self.outline.verify = 1.0;
                    self.shown = 0.0;
                }
                pending + self.outline.copy + self.outline.verify * frac
            }
            "consolidate" | "consolidate-verify" => {
                let (steps, step) = if self.started {
                    (&mut self.outline.steps, &mut self.step)
                } else {
                    (&mut self.outline.pending, &mut self.pending_step)
                };
                let (behind, grew) = within_steps(steps, step, p, frac);
                if grew {
                    self.shown = 0.0;
                }
                if self.started {
                    pending + self.outline.copy + self.outline.verify + behind
                } else {
                    behind
                }
            }
            _ => return None,
        };
        let total = self.outline.pending.iter().map(|(f, c)| f + c).sum::<f64>()
            + self.outline.copy
            + self.outline.verify
            + self.outline.steps.iter().map(|(f, c)| f + c).sum::<f64>();
        let share = if total > 0.0 { at / total } else { 0.0 };
        self.shown = self.shown.max(share.clamp(0.0, 1.0));
        // 100 % is the run's end, which is not a progress message: after the
        // last one the increment is still deleted and the run recorded. On
        // 19.11.2026 the indicator stood at 100 % under a check still going.
        Some(((self.shown * 100.0).floor() as u64).min(99))
    }
}

/// An [`EventSink`] that stamps each progress event of one run with the
/// whole run's percent and passes everything on.
pub(crate) struct OverallSink {
    inner: Arc<dyn EventSink>,
    overall: Mutex<Option<Overall>>,
}

impl OverallSink {
    pub(crate) fn new(inner: Arc<dyn EventSink>, outline: Option<Outline>) -> Self {
        Self {
            inner,
            overall: Mutex::new(outline.map(Overall::new)),
        }
    }
}

impl EventSink for OverallSink {
    fn job_progress(&self, mut progress: JobProgress) {
        if let Some(overall) = self
            .overall
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_mut()
        {
            progress.overall_percent = overall.note(&progress);
        }
        self.inner.job_progress(progress);
    }
    fn queue_changed(&self, snapshot: QueueSnapshot) {
        self.inner.queue_changed(snapshot);
    }
    fn job_finished(&self, run: JobRun) {
        self.inner.job_finished(run);
    }
    fn before_backup(&self, job: &Job, cancel: &std::sync::atomic::AtomicBool) {
        self.inner.before_backup(job, cancel);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(phase: &str, percent: u64, total: u64) -> JobProgress {
        JobProgress {
            job_id: "j".into(),
            phase: phase.into(),
            percent,
            blocks_done: percent,
            blocks_total: total,
            speed_mbps: 0.0,
            volume_index: 0,
            volume_count: 1,
            volume: String::new(),
            image_file_name: String::new(),
            categories: Vec::new(),
            excluded_files: None,
            excluded_bytes: None,
            stalled_secs: None,
            phase_started_ms: None,
            overall_percent: None,
        }
    }

    /// The run of 18.11.2026 as outlined: a small copy, the whole chain
    /// read back, a rebuild of the base and its check. The indicator moves
    /// through all of them and stands at 100 only at the end.
    #[test]
    fn the_indicator_spans_the_consolidation() {
        let mut o = Overall::new(Outline {
            pending: Vec::new(),
            copy: 0.4,
            verify: 1.0,
            steps: vec![(2.4, 1.0)],
        });
        assert_eq!(o.note(&at("prepare", 0, 0)), Some(0));
        assert_eq!(o.note(&at("backup", 100, 100)), Some(8));
        assert_eq!(o.note(&at("finalize", 0, 0)), Some(8));
        assert_eq!(o.note(&at("verify", 100, 100)), Some(29));
        assert_eq!(o.note(&at("consolidate", 0, 0)), Some(29));
        assert_eq!(o.note(&at("consolidate", 50, 100)), Some(54));
        assert_eq!(o.note(&at("consolidate", 100, 100)), Some(79));
        assert_eq!(o.note(&at("consolidate-verify", 0, 0)), Some(79));
        assert_eq!(o.note(&at("consolidate-verify", 50, 100)), Some(89));
        assert_eq!(o.note(&at("consolidate-verify", 100, 100)), Some(99));
    }

    /// A step nobody foresaw is added when it starts and the share is
    /// counted anew — not left at 100 % while the step runs.
    #[test]
    fn an_unforeseen_step_is_counted_when_it_starts() {
        let mut o = Overall::new(Outline {
            pending: Vec::new(),
            copy: 0.4,
            verify: 1.0,
            steps: Vec::new(),
        });
        o.note(&at("backup", 100, 100));
        assert_eq!(o.note(&at("verify", 100, 100)), Some(99));
        assert_eq!(o.note(&at("consolidate", 0, 0)), Some(41));
        assert_eq!(o.note(&at("consolidate-verify", 50, 100)), Some(85));
        assert_eq!(
            o.note(&at("consolidate-verify", 10, 100)),
            Some(85),
            "never back"
        );

        // Two steps where one was foreseen: the second is counted as it comes.
        let mut o = Overall::new(Outline {
            pending: Vec::new(),
            copy: 0.4,
            verify: 1.0,
            steps: vec![(1.0, 1.0)],
        });
        o.note(&at("backup", 100, 100));
        o.note(&at("verify", 100, 100));
        assert_eq!(o.note(&at("consolidate-verify", 100, 100)), Some(99));
        assert_eq!(o.note(&at("consolidate", 0, 0)), Some(62));
        assert_eq!(o.note(&at("consolidate-verify", 100, 100)), Some(99));
    }

    /// A step an earlier run left half-done is finished before the copy,
    /// and the indicator counts it as the run's first part — it does not
    /// run up to 100 % with the step's rebuild and fall back to 0 when the
    /// step's check begins (27.11.2026).
    #[test]
    fn a_step_left_by_an_earlier_run_is_the_runs_first_part() {
        // Left over: a rebuild and a whole check; then this run's copy, its
        // check and one more step. Total 3 + 1.4 + 3 = 7.4.
        let mut o = Overall::new(Outline {
            pending: vec![(2.0, 1.0)],
            copy: 0.4,
            verify: 1.0,
            steps: vec![(2.0, 1.0)],
        });
        assert_eq!(o.note(&at("consolidate", 0, 0)), Some(0));
        assert_eq!(o.note(&at("consolidate", 100, 100)), Some(27));
        assert_eq!(o.note(&at("consolidate-verify", 0, 100)), Some(27));
        assert_eq!(o.note(&at("consolidate-verify", 100, 100)), Some(40));
        assert_eq!(o.note(&at("prepare", 0, 0)), Some(40));
        assert_eq!(o.note(&at("backup", 100, 100)), Some(45));
        assert_eq!(o.note(&at("verify", 100, 100)), Some(59));
        // The run's own step is the outline's, not the left-over one again.
        assert_eq!(o.note(&at("consolidate", 100, 100)), Some(86));
        assert_eq!(o.note(&at("consolidate-verify", 100, 100)), Some(99));

        // Left over but not foreseen: counted as it comes, not shown as the
        // step's own percent.
        let mut o = Overall::new(Outline {
            pending: Vec::new(),
            copy: 0.4,
            verify: 1.0,
            steps: Vec::new(),
        });
        assert_eq!(o.note(&at("consolidate-verify", 50, 100)), Some(44));
        assert_eq!(o.note(&at("backup", 0, 100)), Some(58));
    }
}
