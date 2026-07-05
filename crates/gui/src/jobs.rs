//! Re-export of the orchestrator core types used across the GUI command modules.
//!
//! The orchestrator itself (job store, run queue, scheduler, backup worker) runs
//! in `own-backup-service`; the GUI reaches it over gRPC via
//! [`crate::service_link::ServiceLink`]. This module only keeps the shared type
//! names (`Job`, `JobRun`, `QueueSnapshot`, …) resolvable at `crate::jobs::*`,
//! where the command modules reference them.

pub use own_backup_core::*;
