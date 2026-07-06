//! Turn a job's [`ScheduleConfig`] into a cron pattern and decide when it is
//! due (plan v2, phase 4).
//!
//! The GUI schedule model (`kind` + `days` + `HH:MM`) is friendlier than raw
//! cron, so we translate it to a 7-field cron expression and reuse the `cron`
//! crate for the actual "when does it fire" maths. Times are interpreted in the
//! machine's local timezone (the user set "14:00" meaning 14:00 locally); the
//! service passes `Local::now().fixed_offset()`.

use std::str::FromStr;

use chrono::{DateTime, Duration, FixedOffset};
use cron::Schedule;

use own_backup_core::ScheduleConfig;

const WEEKDAYS: &[(&str, &str)] = &[
    ("mon", "MON"),
    ("tue", "TUE"),
    ("wed", "WED"),
    ("thu", "THU"),
    ("fri", "FRI"),
    ("sat", "SAT"),
    ("sun", "SUN"),
];

/// Translate a schedule to a 7-field cron expression
/// (`sec min hour day-of-month month day-of-week year`), or `None` when the
/// schedule is not a simple time trigger (`none`, and — for now — `event` /
/// `nonstop`, which need OS event hooks / an interval field).
pub fn schedule_to_cron(cfg: &ScheduleConfig) -> Option<String> {
    let (h, m) = parse_hhmm(&cfg.time)?;
    match cfg.kind.as_str() {
        "daily" => Some(format!("0 {m} {h} * * * *")),
        "weekly" => {
            let dows: Vec<&str> = cfg
                .days
                .iter()
                .filter_map(|d| {
                    let d = d.to_lowercase();
                    WEEKDAYS.iter().find(|(k, _)| *k == d).map(|(_, v)| *v)
                })
                .collect();
            if dows.is_empty() {
                None
            } else {
                Some(format!("0 {m} {h} * * {} *", dows.join(",")))
            }
        }
        // Configurable day-of-month, clamped to 1–28 so the slot exists in every
        // month (a "31" would silently skip Feb/Apr/… — the UI limits it too).
        "monthly" => {
            let dom = cfg.day_of_month.clamp(1, 28);
            Some(format!("0 {m} {h} {dom} * * *"))
        }
        _ => None,
    }
}

fn parse_hhmm(t: &str) -> Option<(u32, u32)> {
    let (h, m) = t.split_once(':')?;
    let h: u32 = h.trim().parse().ok()?;
    let m: u32 = m.trim().parse().ok()?;
    (h < 24 && m < 60).then_some((h, m))
}

/// Parse a cron expression produced by [`schedule_to_cron`].
pub fn parse(cron: &str) -> Option<Schedule> {
    Schedule::from_str(cron).ok()
}

/// Should a job fire at `now`, given when it last fired?
///
/// - `run_missed`: catch up. Fires if ANY scheduled slot passed since
///   `last_fired` — so a machine that was off through a slot runs it once on
///   the next tick.
/// - not `run_missed`: fire only for a slot that just passed (within `fresh`),
///   and only if it has not been serviced yet.
///
/// `last_fired` is the job's most recent run start; `None` = never run.
pub fn is_due(
    sched: &Schedule,
    now: DateTime<FixedOffset>,
    last_fired: Option<DateTime<FixedOffset>>,
    run_missed: bool,
    fresh: Duration,
) -> bool {
    if run_missed {
        // Look back from the last run (or up to a year for a never-run job) and
        // see whether a slot has come due since.
        let floor = last_fired.unwrap_or_else(|| now - Duration::days(366));
        matches!(sched.after(&floor).next(), Some(slot) if slot <= now)
    } else {
        // Only a slot inside (now - fresh, now] counts, and only if the last
        // run predates it (otherwise we already handled that slot).
        match sched.after(&(now - fresh)).next() {
            Some(slot) if slot <= now => last_fired.map_or(true, |lf| lf < slot),
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn cfg(kind: &str, time: &str, days: &[&str], run_missed: bool) -> ScheduleConfig {
        ScheduleConfig {
            kind: kind.into(),
            time: time.into(),
            days: days.iter().map(|s| s.to_string()).collect(),
            run_missed,
            ..Default::default()
        }
    }

    fn at(y: i32, mo: u32, d: u32, h: u32, mi: u32, s: u32) -> DateTime<FixedOffset> {
        FixedOffset::east_opt(0)
            .unwrap()
            .with_ymd_and_hms(y, mo, d, h, mi, s)
            .unwrap()
    }

    #[test]
    fn cron_translation_parses() {
        assert!(parse(&schedule_to_cron(&cfg("daily", "14:30", &[], false)).unwrap()).is_some());
        assert!(parse(
            &schedule_to_cron(&cfg("weekly", "09:00", &["mon", "wed", "fri"], false)).unwrap()
        )
        .is_some());
        assert!(parse(&schedule_to_cron(&cfg("monthly", "03:15", &[], false)).unwrap()).is_some());
        assert!(schedule_to_cron(&cfg("none", "14:00", &[], false)).is_none());
        assert!(schedule_to_cron(&cfg("weekly", "14:00", &[], false)).is_none()); // no days
        assert!(schedule_to_cron(&cfg("daily", "bad", &[], false)).is_none());
    }

    #[test]
    fn weekly_fires_on_configured_weekday() {
        // 2026-07-01 is a Wednesday.
        let sched = parse(&schedule_to_cron(&cfg("weekly", "14:00", &["wed"], false)).unwrap()).unwrap();
        let next = sched.after(&at(2026, 6, 29, 0, 0, 0)).next().unwrap(); // Mon
        assert_eq!(next.format("%Y-%m-%d %H:%M").to_string(), "2026-07-01 14:00");
    }

    #[test]
    fn fresh_slot_fires_once_then_stops() {
        let sched = parse(&schedule_to_cron(&cfg("daily", "14:00", &[], false)).unwrap()).unwrap();
        let fresh = Duration::seconds(120);

        // 30s after the slot, never run -> due.
        assert!(is_due(&sched, at(2026, 7, 1, 14, 0, 30), None, false, fresh));
        // Already ran this slot -> not due.
        let ran = Some(at(2026, 7, 1, 14, 0, 5));
        assert!(!is_due(&sched, at(2026, 7, 1, 14, 0, 30), ran, false, fresh));
        // Two hours later, never run, no catch-up -> not due (missed window).
        assert!(!is_due(&sched, at(2026, 7, 1, 16, 0, 0), None, false, fresh));
    }

    #[test]
    fn run_missed_catches_up() {
        let sched = parse(&schedule_to_cron(&cfg("daily", "14:00", &[], true)).unwrap()).unwrap();
        let fresh = Duration::seconds(120);
        // Two hours past the slot, never run, catch-up on -> due.
        assert!(is_due(&sched, at(2026, 7, 1, 16, 0, 0), None, true, fresh));
        // Last run was after today's slot -> nothing owed -> not due.
        let ran = Some(at(2026, 7, 1, 15, 0, 0));
        assert!(!is_due(&sched, at(2026, 7, 1, 16, 0, 0), ran, true, fresh));
    }
}
