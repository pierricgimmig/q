//! Shared domain model and service API for `q`.
//!
//! CLI and MCP adapters call [`QueueService`]. They do not implement state
//! transitions or SQL themselves.

mod error;
mod host;
mod model;
mod repo;
mod service;
mod timeutil;
mod transition;
mod tree;
mod validate;

pub use error::QueueError;
pub use host::{local_hostname, local_worker_identity, normalize_tags};
pub use model::*;
pub use repo::normalize_repo_url;
pub use service::QueueService;
pub use timeutil::{format_timestamp, parse_timestamp};
pub use transition::{ensure_transition, transition_allowed};
pub use tree::{build_feature_forest, build_task_tree, TreeTask};
pub use validate::{acceptance_criteria, body_template};

/// Lease length when a claim or heartbeat does not set one.
///
/// The lease is measured from the last heartbeat. A worker that stops
/// heartbeating loses the task after this long.
pub const DEFAULT_LEASE_MINUTES: u64 = 30;
pub const MIN_LEASE_MINUTES: u64 = 1;
pub const MAX_LEASE_MINUTES: u64 = 24 * 60;
/// `q top` flags a worker whose last heartbeat is older than this.
pub const DEFAULT_STALE_HEARTBEAT_SECS: u64 = 120;
/// How often `q serve` releases expired leases.
pub const DEFAULT_LEASE_SWEEP_SECS: u64 = 15;
pub const NO_ELIGIBLE_REASON: &str = "no_eligible_ready_tasks";
/// Machine reason stored on `task_recovered` when a lease runs out.
pub const LEASE_EXPIRED_REASON: &str = "lease_expired";

use std::time::Duration;

pub fn default_lease() -> Duration {
    Duration::from_secs(DEFAULT_LEASE_MINUTES * 60)
}

pub fn lease_from_minutes(minutes: u64) -> Result<Duration, QueueError> {
    if !(MIN_LEASE_MINUTES..=MAX_LEASE_MINUTES).contains(&minutes) {
        return Err(QueueError::InvalidInput(format!(
            "lease must be between {MIN_LEASE_MINUTES} and {MAX_LEASE_MINUTES} minutes"
        )));
    }
    Ok(Duration::from_secs(minutes * 60))
}
