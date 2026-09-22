use std::fmt;
use std::str::FromStr;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use uuid::Uuid;

use crate::timeutil::ts;
use crate::QueueError;

fn norm_token(value: &str) -> String {
    value.trim().to_ascii_lowercase().replace('-', "_")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    Inbox,
    Ready,
    Claimed,
    InProgress,
    Review,
    Blocked,
    Done,
    Cancelled,
}

impl TaskStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Inbox => "inbox",
            Self::Ready => "ready",
            Self::Claimed => "claimed",
            Self::InProgress => "in_progress",
            Self::Review => "review",
            Self::Blocked => "blocked",
            Self::Done => "done",
            Self::Cancelled => "cancelled",
        }
    }

    pub fn parse(value: &str) -> Result<Self, QueueError> {
        match norm_token(value).as_str() {
            "inbox" => Ok(Self::Inbox),
            "ready" => Ok(Self::Ready),
            "claimed" => Ok(Self::Claimed),
            "in_progress" => Ok(Self::InProgress),
            "review" => Ok(Self::Review),
            "blocked" => Ok(Self::Blocked),
            "done" => Ok(Self::Done),
            "cancelled" | "canceled" => Ok(Self::Cancelled),
            other => Err(QueueError::InvalidInput(format!(
                "unknown status '{other}' (expected inbox, ready, claimed, in_progress, review, blocked, done, cancelled)"
            ))),
        }
    }
}

impl fmt::Display for TaskStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for TaskStatus {
    type Err = QueueError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse(s)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskKind {
    Implementation,
    Research,
    Review,
    Benchmark,
    Documentation,
    Other,
}

impl TaskKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Implementation => "implementation",
            Self::Research => "research",
            Self::Review => "review",
            Self::Benchmark => "benchmark",
            Self::Documentation => "documentation",
            Self::Other => "other",
        }
    }

    pub fn parse(value: &str) -> Result<Self, QueueError> {
        match norm_token(value).as_str() {
            "implementation" | "impl" => Ok(Self::Implementation),
            "research" => Ok(Self::Research),
            "review" => Ok(Self::Review),
            "benchmark" => Ok(Self::Benchmark),
            "documentation" | "docs" => Ok(Self::Documentation),
            "other" => Ok(Self::Other),
            other => Err(QueueError::InvalidInput(format!(
                "unknown kind '{other}' (expected implementation, research, review, benchmark, documentation, other)"
            ))),
        }
    }
}

impl fmt::Display for TaskKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for TaskKind {
    type Err = QueueError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse(s)
    }
}

#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum RiskLevel {
    #[default]
    Low = 0,
    Medium = 1,
    High = 2,
    ExternalAction = 3,
}

impl RiskLevel {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::ExternalAction => "external_action",
        }
    }

    pub fn parse(value: &str) -> Result<Self, QueueError> {
        match norm_token(value).as_str() {
            "low" => Ok(Self::Low),
            "medium" | "med" => Ok(Self::Medium),
            "high" => Ok(Self::High),
            "external_action" | "external" => Ok(Self::ExternalAction),
            other => Err(QueueError::InvalidInput(format!(
                "unknown risk '{other}' (expected low, medium, high, external_action)"
            ))),
        }
    }
}

impl fmt::Display for RiskLevel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for RiskLevel {
    type Err = QueueError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse(s)
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StaleDisposition {
    #[default]
    Ready,
    Blocked,
}

impl StaleDisposition {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ready => "ready",
            Self::Blocked => "blocked",
        }
    }

    pub fn parse(value: &str) -> Result<Self, QueueError> {
        match norm_token(value).as_str() {
            "ready" => Ok(Self::Ready),
            "blocked" => Ok(Self::Blocked),
            other => Err(QueueError::InvalidInput(format!(
                "unknown stale disposition '{other}' (expected ready or blocked)"
            ))),
        }
    }

    pub fn status(self) -> TaskStatus {
        match self {
            Self::Ready => TaskStatus::Ready,
            Self::Blocked => TaskStatus::Blocked,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActorKind {
    Human,
    Agent,
    System,
}

impl ActorKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Human => "human",
            Self::Agent => "agent",
            Self::System => "system",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Actor {
    pub kind: ActorKind,
    pub id: Option<String>,
}

impl Actor {
    pub fn human(id: Option<String>) -> Self {
        Self {
            kind: ActorKind::Human,
            id,
        }
    }

    pub fn agent(id: impl Into<String>) -> Self {
        Self {
            kind: ActorKind::Agent,
            id: Some(id.into()),
        }
    }

    pub fn system() -> Self {
        Self {
            kind: ActorKind::System,
            id: Some("q".into()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectPolicy {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_parallel_jobs: Option<i64>,
    pub require_pr: bool,
    pub allow_external_actions: bool,
    pub stale_disposition: StaleDisposition,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub config_path: Option<String>,
}

impl Default for ProjectPolicy {
    fn default() -> Self {
        Self {
            max_parallel_jobs: None,
            require_pr: false,
            allow_external_actions: false,
            stale_disposition: StaleDisposition::Ready,
            config_path: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Task {
    pub id: i64,
    pub public_id: Uuid,
    pub title: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    pub original_capture: String,
    pub status: TaskStatus,
    pub kind: TaskKind,
    pub priority: i32,
    pub risk: RiskLevel,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repo: Option<String>,
    pub capture_path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repo_relative_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub git_root: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub git_head: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_pool: Option<String>,
    pub required_capabilities: Vec<String>,
    pub dependencies: Vec<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub blocked_reason: Option<String>,
    #[serde(with = "ts")]
    pub created_at: OffsetDateTime,
    #[serde(with = "ts")]
    pub updated_at: OffsetDateTime,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskSummary {
    pub id: i64,
    pub public_id: Uuid,
    pub title: String,
    pub status: TaskStatus,
    pub kind: TaskKind,
    pub priority: i32,
    pub risk: RiskLevel,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repo: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_pool: Option<String>,
    #[serde(with = "ts")]
    pub created_at: OffsetDateTime,
    #[serde(with = "ts")]
    pub updated_at: OffsetDateTime,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Claim {
    pub id: i64,
    pub task_id: i64,
    pub agent_id: String,
    pub token: String,
    #[serde(with = "ts")]
    pub claimed_at: OffsetDateTime,
    #[serde(with = "ts")]
    pub heartbeat_at: OffsetDateTime,
    #[serde(with = "ts")]
    pub lease_expires_at: OffsetDateTime,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub worktree_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none", with = "ts_opt")]
    pub released_at: Option<OffsetDateTime>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub release_reason: Option<String>,
    pub active: bool,
}

mod ts_opt {
    use super::*;
    use serde::{Deserializer, Serializer};

    pub fn serialize<S>(value: &Option<OffsetDateTime>, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match value {
            Some(ts) => ts::serialize(ts, serializer),
            None => serializer.serialize_none(),
        }
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<Option<OffsetDateTime>, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = Option::<String>::deserialize(deserializer)?;
        match value {
            Some(text) => crate::parse_timestamp(&text)
                .map(Some)
                .map_err(serde::de::Error::custom),
            None => Ok(None),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Artifact {
    pub id: i64,
    pub task_id: i64,
    pub kind: String,
    pub value: String,
    #[serde(with = "ts")]
    pub created_at: OffsetDateTime,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArtifactInput {
    pub kind: String,
    pub value: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Event {
    pub id: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub task_id: Option<i64>,
    pub event_type: String,
    pub actor_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub actor_id: Option<String>,
    pub payload: serde_json::Value,
    #[serde(with = "ts")]
    pub created_at: OffsetDateTime,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskDetail {
    #[serde(flatten)]
    pub task: Task,
    pub acceptance_criteria: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub claim: Option<Claim>,
    pub artifacts: Vec<Artifact>,
    pub events: Vec<Event>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClaimLease {
    pub token: String,
    #[serde(with = "ts")]
    pub lease_expires_at: OffsetDateTime,
    pub agent_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClaimTask {
    #[serde(flatten)]
    pub task: Task,
    pub acceptance_criteria: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClaimOutcome {
    pub found: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub task: Option<ClaimTask>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub claim: Option<ClaimLease>,
}

impl ClaimOutcome {
    pub fn none() -> Self {
        Self {
            found: false,
            reason: Some(crate::NO_ELIGIBLE_REASON.to_string()),
            task: None,
            claim: None,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatusCounts {
    pub inbox: i64,
    pub ready: i64,
    pub claimed: i64,
    pub in_progress: i64,
    pub review: i64,
    pub blocked: i64,
    pub done: i64,
    pub cancelled: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueueStatus {
    pub counts: StatusCounts,
    pub active_claims: i64,
    pub expired_claims: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecoveryRecord {
    pub task_id: i64,
    pub previous_status: TaskStatus,
    pub new_status: TaskStatus,
    pub agent_id: String,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReadyOutcome {
    pub task: Task,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct CaptureRequest {
    pub title: String,
    pub body: Option<String>,
    pub kind: TaskKind,
    pub priority: i32,
    pub risk: RiskLevel,
    pub project: Option<String>,
    pub repo: Option<String>,
    pub capture_path: String,
    pub repo_relative_path: Option<String>,
    pub git_root: Option<String>,
    pub git_head: Option<String>,
    pub agent_pool: Option<String>,
    pub required_capabilities: Vec<String>,
    pub dependencies: Vec<i64>,
    pub policy: Option<ProjectPolicy>,
    pub actor: Actor,
    pub context_source: Option<String>,
}

/// Filters for [`crate::QueueService::list`].
///
/// When `status` is set, only that status is returned. That includes `done`
/// and `cancelled`, and `include_terminal` is ignored. When `status` is unset
/// and `include_terminal` is false, those two terminal statuses are omitted.
///
/// Rows are ordered by project name (case-insensitive). Null and blank
/// projects sort last. Within a project, `updated_at` is newest first, then
/// id descending.
#[derive(Debug, Clone)]
pub struct ListFilter {
    pub status: Option<TaskStatus>,
    pub project: Option<String>,
    pub repo: Option<String>,
    pub kind: Option<TaskKind>,
    pub limit: u32,
    /// Include `done` and `cancelled` when `status` is unset.
    pub include_terminal: bool,
}

impl Default for ListFilter {
    fn default() -> Self {
        Self {
            status: None,
            project: None,
            repo: None,
            kind: None,
            limit: 100,
            include_terminal: false,
        }
    }
}

#[derive(Debug, Clone)]
pub struct EditRequest {
    pub title: Option<String>,
    pub body: Option<String>,
    pub kind: Option<TaskKind>,
    pub priority: Option<i32>,
    pub risk: Option<RiskLevel>,
    pub project: Option<String>,
    pub repo: Option<String>,
    pub agent_pool: Option<String>,
    pub required_capabilities: Option<Vec<String>>,
    pub dependencies: Option<Vec<i64>>,
    pub clear_project: bool,
    pub clear_repo: bool,
    pub clear_agent_pool: bool,
    pub actor: Actor,
}

impl EditRequest {
    pub fn empty(actor: Actor) -> Self {
        Self {
            title: None,
            body: None,
            kind: None,
            priority: None,
            risk: None,
            project: None,
            repo: None,
            agent_pool: None,
            required_capabilities: None,
            dependencies: None,
            clear_project: false,
            clear_repo: false,
            clear_agent_pool: false,
            actor,
        }
    }

    pub fn has_changes(&self) -> bool {
        self.title.is_some()
            || self.body.is_some()
            || self.kind.is_some()
            || self.priority.is_some()
            || self.risk.is_some()
            || self.project.is_some()
            || self.repo.is_some()
            || self.agent_pool.is_some()
            || self.required_capabilities.is_some()
            || self.dependencies.is_some()
            || self.clear_project
            || self.clear_repo
            || self.clear_agent_pool
    }
}

#[derive(Debug, Clone)]
pub struct ReadyRequest {
    pub task_id: i64,
    pub actor: Actor,
}

#[derive(Debug, Clone)]
pub struct BlockRequest {
    pub task_id: i64,
    pub claim_token: Option<String>,
    pub reason: String,
    pub actor: Actor,
}

#[derive(Debug, Clone)]
pub struct CancelRequest {
    pub task_id: i64,
    pub reason: String,
    pub actor: Actor,
}

/// Hard-delete. Distinct from [`CancelRequest`], which keeps the task row.
#[derive(Debug, Clone)]
pub struct DeleteRequest {
    pub task_id: i64,
    pub reason: String,
    pub force: bool,
    pub actor: Actor,
}

/// Confirmation of a hard delete. Related rows are gone with the task, including
/// events, so this outcome is the caller's record of what was removed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeleteOutcome {
    pub task_id: i64,
    pub public_id: Uuid,
    pub title: String,
    pub status: TaskStatus,
    pub reason: String,
    pub forced: bool,
    pub active_claim_cleared: bool,
    pub claims_removed: i64,
    pub events_removed: i64,
    pub artifacts_removed: i64,
    pub dependencies_removed: i64,
}

#[derive(Debug, Clone)]
pub struct ClaimRequest {
    pub agent_id: String,
    pub capabilities: Vec<String>,
    pub allowed_repos: Vec<String>,
    pub allowed_projects: Vec<String>,
    pub allowed_kinds: Vec<TaskKind>,
    pub agent_pool: Option<String>,
    pub maximum_risk: RiskLevel,
    pub lease: Duration,
}

impl ClaimRequest {
    pub fn new(agent_id: impl Into<String>) -> Self {
        Self {
            agent_id: agent_id.into(),
            capabilities: Vec::new(),
            allowed_repos: Vec::new(),
            allowed_projects: Vec::new(),
            allowed_kinds: Vec::new(),
            agent_pool: None,
            maximum_risk: RiskLevel::Medium,
            lease: crate::default_lease(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct HeartbeatRequest {
    pub task_id: i64,
    pub claim_token: String,
    pub lease: Option<Duration>,
    pub actor: Actor,
}

#[derive(Debug, Clone)]
pub struct StartRequest {
    pub task_id: i64,
    pub claim_token: String,
    pub branch: Option<String>,
    pub worktree_path: Option<String>,
    pub actor: Actor,
}

#[derive(Debug, Clone)]
pub struct CompleteRequest {
    pub task_id: i64,
    pub claim_token: Option<String>,
    pub summary: String,
    pub target: Option<TaskStatus>,
    pub artifacts: Vec<ArtifactInput>,
    pub actor: Actor,
}

#[derive(Debug, Clone)]
pub struct ReleaseRequest {
    pub task_id: i64,
    pub claim_token: String,
    pub reason: String,
    pub actor: Actor,
}

#[derive(Debug, Clone)]
pub struct RecoverRequest {
    pub reason: String,
    pub to: Option<StaleDisposition>,
    pub actor: Actor,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::NO_ELIGIBLE_REASON;
    use time::macros::datetime;

    fn sample_task() -> Task {
        Task {
            id: 184,
            public_id: Uuid::nil(),
            title: "Benchmark trace encoding variants".into(),
            body: None,
            original_capture: "Benchmark trace encoding variants".into(),
            status: TaskStatus::Claimed,
            kind: TaskKind::Benchmark,
            priority: 10,
            risk: RiskLevel::Low,
            project: Some("profiler-core".into()),
            repo: Some("github.com/acme/profiler-core".into()),
            capture_path: "/tmp/profiler-core/crates/trace".into(),
            repo_relative_path: Some("crates/trace".into()),
            git_root: Some("/tmp/profiler-core".into()),
            git_head: Some("abc".into()),
            agent_pool: None,
            required_capabilities: vec!["rust".into()],
            dependencies: vec![],
            blocked_reason: None,
            created_at: datetime!(2026-09-22 0:00:00 UTC),
            updated_at: datetime!(2026-09-22 0:00:00 UTC),
        }
    }

    #[test]
    fn json_status_kind_and_risk_use_snake_case() {
        let task = sample_task();
        let value = serde_json::to_value(&task).unwrap();
        assert_eq!(value["status"], "claimed");
        assert_eq!(value["kind"], "benchmark");
        assert_eq!(value["risk"], "low");
        assert_eq!(value["id"], 184);
        assert_eq!(value["created_at"], "2026-09-22T00:00:00Z");
        assert!(value.get("body").is_none());
    }

    #[test]
    fn claim_outcome_json_shapes() {
        let none = ClaimOutcome::none();
        let value = serde_json::to_value(&none).unwrap();
        assert_eq!(value["found"], false);
        assert_eq!(value["reason"], NO_ELIGIBLE_REASON);
        assert!(value.get("task").is_none());
        assert!(value.get("claim").is_none());

        let found = ClaimOutcome {
            found: true,
            reason: None,
            task: Some(ClaimTask {
                acceptance_criteria: vec!["Compare encodings".into()],
                task: sample_task(),
            }),
            claim: Some(ClaimLease {
                token: "opaque-token".into(),
                lease_expires_at: datetime!(2026-09-22 0:45:00 UTC),
                agent_id: "codex-local-01".into(),
            }),
        };
        let value = serde_json::to_value(&found).unwrap();
        assert_eq!(value["found"], true);
        assert!(value.get("reason").is_none());
        assert_eq!(value["claim"]["token"], "opaque-token");
        assert_eq!(value["claim"]["lease_expires_at"], "2026-09-22T00:45:00Z");
        assert_eq!(value["task"]["acceptance_criteria"][0], "Compare encodings");
        assert_eq!(value["task"]["repo"], "github.com/acme/profiler-core");
    }

    #[test]
    fn risk_orders_low_to_external() {
        assert!(RiskLevel::Low < RiskLevel::Medium);
        assert!(RiskLevel::Medium < RiskLevel::High);
        assert!(RiskLevel::High < RiskLevel::ExternalAction);
    }
}
