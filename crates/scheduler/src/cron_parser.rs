use chrono::{DateTime, Utc};
use cron::Schedule;
use std::str::FromStr;

use own_backup_common::error::OwnBackupError;
use own_backup_common::Result;

pub fn parse_schedule(expr: &str) -> Result<Schedule> {
    Schedule::from_str(expr)
        .map_err(|e| OwnBackupError::Config(format!("Invalid cron expression '{}': {}", expr, e)))
}

pub fn next_run(schedule: &Schedule) -> Option<DateTime<Utc>> {
    schedule.upcoming(Utc).next()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_valid_cron() {
        let schedule = parse_schedule("0 0 2 * * * *").unwrap();
        let next = next_run(&schedule);
        assert!(next.is_some());
        assert!(next.unwrap() > Utc::now());
    }

    #[test]
    fn parse_invalid_cron() {
        let result = parse_schedule("not a cron expression");
        assert!(result.is_err());
    }

    #[test]
    fn daily_at_2am() {
        let schedule = parse_schedule("0 0 2 * * * *").unwrap();
        let next = next_run(&schedule).unwrap();
        assert_eq!(next.format("%H").to_string(), "02");
    }
}
