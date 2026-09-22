//! Shared domain model and service API for `q`.
//!
//! CLI and MCP adapters call [`QueueService`]. They do not implement state
//! transitions or SQL themselves.

mod error;
mod model;
mod repo;
mod service;
mod timeutil;
mod transition;
mod tree;
mod validate;

pub use error::QueueError;
pub use model::*;
pub use repo::normalize_repo_url;
pub use service::QueueService;
pub use timeutil::{format_timestamp, parse_timestamp};
pub use transition::{ensure_transition, transition_allowed};
pub use tree::{build_feature_forest, build_task_tree, TreeTask};
pub use validate::{acceptance_criteria, missing_recommended_sections, readiness_warnings};

pub const DEFAULT_LEASE_MINUTES: u64 = 45;
pub const MIN_LEASE_MINUTES: u64 = 1;
pub const MAX_LEASE_MINUTES: u64 = 24 * 60;
pub const NO_ELIGIBLE_REASON: &str = "no_eligible_ready_tasks";

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
