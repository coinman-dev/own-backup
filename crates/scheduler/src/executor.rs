use std::future::Future;
use std::pin::Pin;

use chrono::Utc;
use tokio::task::JoinHandle;
use tracing::{error, info, warn};
use uuid::Uuid;

use crate::cron_parser;

/// The callback type: an async function that runs a backup for the given job ID.
/// Returns `Ok(())` on success, `Err(String)` on failure.
pub type JobCallback =
    Box<dyn Fn(Uuid) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send>> + Send + Sync>;

/// Executes a backup job on a cron schedule.
///
/// Spawns a tokio task that sleeps until the next scheduled run time, invokes
/// the callback, and handles retries with exponential backoff on failure.
pub struct JobExecutor;

/// Retry backoff durations in seconds: 1s, 5s, 25s.
const RETRY_DELAYS_SECS: [u64; 3] = [1, 5, 25];

impl JobExecutor {
    /// Spawn a tokio task that runs the given job on the provided schedule.
    ///
    /// The task will:
    /// 1. Sleep until the next scheduled time.
    /// 2. Invoke the callback.
    /// 3. On failure, retry up to 3 times with exponential backoff (1s, 5s, 25s).
    /// 4. If all retries fail, log the error and wait for the next scheduled time.
    ///
    /// The returned `JoinHandle` can be used to cancel the task via `.abort()`.
    pub fn run(
        schedule: cron::Schedule,
        job_id: Uuid,
        callback: JobCallback,
    ) -> JoinHandle<()> {
        tokio::spawn(async move {
            loop {
                // Determine when to run next.
                let next = match cron_parser::next_run(&schedule) {
                    Some(t) => t,
                    None => {
                        warn!(
                            job_id = %job_id,
                            "No upcoming run time found for schedule, stopping executor"
                        );
                        return;
                    }
                };

                let now = Utc::now();
                if next > now {
                    let duration = (next - now)
                        .to_std()
                        .unwrap_or(std::time::Duration::from_secs(1));
                    info!(
                        job_id = %job_id,
                        next_run = %next,
                        sleep_secs = duration.as_secs(),
                        "Sleeping until next scheduled run"
                    );
                    tokio::time::sleep(duration).await;
                }

                // Execute with retries.
                let mut succeeded = false;
                for (attempt, &delay_secs) in RETRY_DELAYS_SECS.iter().enumerate() {
                    info!(
                        job_id = %job_id,
                        attempt = attempt + 1,
                        "Executing scheduled backup"
                    );

                    match callback(job_id).await {
                        Ok(()) => {
                            info!(job_id = %job_id, "Scheduled backup completed successfully");
                            succeeded = true;
                            break;
                        }
                        Err(e) => {
                            if attempt + 1 >= RETRY_DELAYS_SECS.len() {
                                error!(
                                    job_id = %job_id,
                                    attempt = attempt + 1,
                                    error = %e,
                                    "All retry attempts exhausted"
                                );
                                break;
                            }
                            warn!(
                                job_id = %job_id,
                                attempt = attempt + 1,
                                error = %e,
                                retry_in_secs = delay_secs,
                                "Backup failed, will retry"
                            );
                            tokio::time::sleep(std::time::Duration::from_secs(delay_secs)).await;
                        }
                    }
                }

                if !succeeded {
                    error!(
                        job_id = %job_id,
                        "Job execution failed, waiting for next scheduled time"
                    );
                }
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use tokio::sync::Mutex;

    #[tokio::test]
    async fn executor_calls_callback() {
        // Schedule that fires every second.
        let schedule = cron_parser::parse_schedule("* * * * * * *").unwrap();
        let job_id = Uuid::new_v4();

        let call_count = Arc::new(Mutex::new(0u32));
        let count_clone = call_count.clone();

        let callback: JobCallback = Box::new(move |_id| {
            let count = count_clone.clone();
            Box::pin(async move {
                let mut c = count.lock().await;
                *c += 1;
                Ok(())
            })
        });

        let handle = JobExecutor::run(schedule, job_id, callback);

        // Wait enough time for at least one invocation.
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        handle.abort();

        let count = *call_count.lock().await;
        assert!(count >= 1, "Expected at least 1 call, got {count}");
    }

    #[tokio::test]
    async fn executor_retries_on_failure() {
        let schedule = cron_parser::parse_schedule("* * * * * * *").unwrap();
        let job_id = Uuid::new_v4();

        let attempt_count = Arc::new(Mutex::new(0u32));
        let count_clone = attempt_count.clone();

        let callback: JobCallback = Box::new(move |_id| {
            let count = count_clone.clone();
            Box::pin(async move {
                let mut c = count.lock().await;
                *c += 1;
                if *c <= 2 {
                    Err("Simulated failure".into())
                } else {
                    Ok(())
                }
            })
        });

        let handle = JobExecutor::run(schedule, job_id, callback);

        // Give enough time for retries (1s + 5s should cover 3 attempts).
        tokio::time::sleep(std::time::Duration::from_secs(8)).await;
        handle.abort();

        let count = *attempt_count.lock().await;
        assert!(count >= 3, "Expected at least 3 attempts, got {count}");
    }
}
