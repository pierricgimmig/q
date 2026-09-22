//! SQLite implementation of [`q_core::QueueService`].
//!
//! Claim, recovery, and every other state change go through this crate. The
//! CLI and MCP adapters do not keep a second copy of the SQL.

mod dbpath;
mod schema;

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::Duration;

use q_core::{
    acceptance_criteria, default_lease, ensure_transition, format_timestamp, lease_from_minutes,
    normalize_repo_url, parse_timestamp, readiness_warnings, Actor, Artifact, ArtifactInput,
    BlockRequest, CancelRequest, CaptureRequest, Claim, ClaimLease, ClaimOutcome, ClaimRequest,
    ClaimTask, CompleteRequest, CreateFeatureRequest, DeleteFeatureOutcome, DeleteOutcome,
    DeleteRequest, EditFeatureRequest, EditRequest, Event, Feature, HeartbeatRequest, ListFilter,
    ProjectPolicy, QueueError, QueueService, QueueStatus, ReadyOutcome, ReadyRequest,
    RecoverRequest, RecoveryRecord, ReleaseRequest, RiskLevel, StaleDisposition, StartRequest,
    StatusCounts, Task, TaskDetail, TaskKind, TaskStatus, TaskSummary,
};
use q_dispatch::{is_eligible, EligibilityTask};
use rusqlite::{params, Connection, OptionalExtension, Transaction, TransactionBehavior};
use serde_json::{json, Value};
use time::OffsetDateTime;
use uuid::Uuid;

pub use dbpath::{default_db_path, resolve_db_path};

const TASK_SELECT: &str = "\
SELECT tasks.id, tasks.public_id, tasks.title, tasks.body, tasks.original_capture, tasks.status, \
tasks.kind, tasks.priority, tasks.risk, tasks.project_name, tasks.repo, tasks.capture_path, \
tasks.repo_relative_path, tasks.git_root, tasks.git_head, tasks.agent_pool, \
tasks.required_capabilities_json, tasks.blocked_reason, tasks.created_at, tasks.updated_at, \
tasks.feature_id, features.title \
FROM tasks \
LEFT JOIN features ON features.id = tasks.feature_id";

const FEATURE_SELECT: &str = "\
SELECT features.id, features.public_id, features.title, features.body, features.created_at, \
features.updated_at, \
(SELECT COUNT(*) FROM tasks WHERE tasks.feature_id = features.id) \
FROM features";

const CLAIM_SELECT: &str = "\
SELECT id, task_id, agent_id, claim_token, claimed_at, heartbeat_at, lease_expires_at, \
branch, worktree_path, released_at, release_reason \
FROM claims";

const EVENT_SELECT: &str = "\
SELECT id, task_id, event_type, actor_type, actor_id, payload_json, created_at FROM events";

#[derive(Debug, Clone)]
pub struct Queue {
    path: PathBuf,
}

impl Queue {
    pub fn open(path: impl Into<PathBuf>) -> Result<Self, QueueError> {
        let path = path.into();
        let _conn = open_connection(&path)?;
        Ok(Self { path })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    fn with_conn<T>(
        &self,
        f: impl FnOnce(&Connection) -> Result<T, QueueError>,
    ) -> Result<T, QueueError> {
        let conn = open_connection(&self.path)?;
        f(&conn)
    }
}

pub(crate) fn open_connection(path: &Path) -> Result<Connection, QueueError> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent).map_err(|err| {
                QueueError::Database(format!("create {}: {err}", parent.display()))
            })?;
        }
    }
    let mut conn = Connection::open(path).db()?;
    let mode: String = conn
        .query_row("PRAGMA journal_mode = WAL", [], |row| row.get(0))
        .db()?;
    if !mode.eq_ignore_ascii_case("wal") {
        return Err(QueueError::Database(format!(
            "expected WAL journal mode, got {mode}"
        )));
    }
    conn.execute_batch("PRAGMA foreign_keys = ON;").db()?;
    conn.busy_timeout(Duration::from_millis(5000)).db()?;
    schema::migrate(&mut conn)?;
    Ok(conn)
}

trait DbResult<T> {
    fn db(self) -> Result<T, QueueError>;
}

impl<T> DbResult<T> for rusqlite::Result<T> {
    fn db(self) -> Result<T, QueueError> {
        self.map_err(|err| QueueError::Database(err.to_string()))
    }
}

fn now_parts() -> (OffsetDateTime, String) {
    let now = OffsetDateTime::now_utc();
    (now, format_timestamp(now))
}

fn dedupe_strings(items: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    for item in items {
        let trimmed = item.trim();
        if trimmed.is_empty() {
            continue;
        }
        if !out.iter().any(|existing: &String| existing == trimmed) {
            out.push(trimmed.to_string());
        }
    }
    out
}

fn dedupe_ids(items: &[i64]) -> Vec<i64> {
    let mut out = Vec::new();
    for id in items {
        if !out.contains(id) {
            out.push(*id);
        }
    }
    out
}

fn norm_repo(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(normalize_repo_url)
        .filter(|text| !text.is_empty())
}

fn clamp_limit(limit: u32) -> u32 {
    if limit == 0 {
        100
    } else {
        limit.min(500)
    }
}

fn lease_or_default(lease: Duration) -> Result<Duration, QueueError> {
    let minutes = lease.as_secs() / 60;
    if !lease.as_secs().is_multiple_of(60) {
        return Err(QueueError::InvalidInput(
            "lease must be a whole number of minutes".into(),
        ));
    }
    lease_from_minutes(minutes)
}

fn add_lease(now: OffsetDateTime, lease: Duration) -> Result<OffsetDateTime, QueueError> {
    let extra = time::Duration::try_from(lease)
        .map_err(|_| QueueError::InvalidInput("lease is out of range".into()))?;
    now.checked_add(extra)
        .ok_or_else(|| QueueError::InvalidInput("lease overflow".into()))
}

fn insert_event(
    conn: &Connection,
    task_id: Option<i64>,
    event_type: &str,
    actor: &Actor,
    payload: Value,
    now: &str,
) -> Result<(), QueueError> {
    let payload = serde_json::to_string(&payload).unwrap_or_else(|_| "{}".to_string());
    conn.execute(
        "INSERT INTO events (task_id, event_type, actor_type, actor_id, payload_json, created_at)
         VALUES (?, ?, ?, ?, ?, ?)",
        params![
            task_id,
            event_type,
            actor.kind.as_str(),
            actor.id,
            payload,
            now
        ],
    )
    .db()?;
    Ok(())
}

fn map_task(row: &rusqlite::Row<'_>) -> rusqlite::Result<Task> {
    let public_id: String = row.get(1)?;
    let status: String = row.get(5)?;
    let kind: String = row.get(6)?;
    let risk: String = row.get(8)?;
    let caps: String = row.get(16)?;
    let created_at: String = row.get(18)?;
    let updated_at: String = row.get(19)?;
    let required_capabilities: Vec<String> = serde_json::from_str(&caps).unwrap_or_default();
    Ok(Task {
        id: row.get(0)?,
        public_id: parse_uuid(1, &public_id)?,
        title: row.get(2)?,
        body: row.get(3)?,
        original_capture: row.get(4)?,
        status: parse_status(5, &status)?,
        kind: parse_kind(6, &kind)?,
        priority: row.get(7)?,
        risk: parse_risk(8, &risk)?,
        project: row.get(9)?,
        repo: row.get(10)?,
        capture_path: row.get(11)?,
        repo_relative_path: row.get(12)?,
        git_root: row.get(13)?,
        git_head: row.get(14)?,
        agent_pool: row.get(15)?,
        required_capabilities,
        dependencies: Vec::new(),
        blocked_reason: row.get(17)?,
        feature_id: row.get(20)?,
        feature: row.get(21)?,
        created_at: parse_time(18, &created_at)?,
        updated_at: parse_time(19, &updated_at)?,
    })
}

fn parse_uuid(idx: usize, value: &str) -> rusqlite::Result<Uuid> {
    Uuid::parse_str(value).map_err(|err| conversion(idx, err))
}

fn parse_status(idx: usize, value: &str) -> rusqlite::Result<TaskStatus> {
    TaskStatus::parse(value).map_err(|err| conversion(idx, err))
}

fn parse_kind(idx: usize, value: &str) -> rusqlite::Result<TaskKind> {
    TaskKind::parse(value).map_err(|err| conversion(idx, err))
}

fn parse_risk(idx: usize, value: &str) -> rusqlite::Result<RiskLevel> {
    RiskLevel::parse(value).map_err(|err| conversion(idx, err))
}

fn parse_time(idx: usize, value: &str) -> rusqlite::Result<OffsetDateTime> {
    parse_timestamp(value).map_err(|err| conversion(idx, err))
}

fn conversion<E>(idx: usize, err: E) -> rusqlite::Error
where
    E: std::error::Error + Send + Sync + 'static,
{
    rusqlite::Error::FromSqlConversionFailure(idx, rusqlite::types::Type::Text, Box::new(err))
}

fn load_dependencies(conn: &Connection, task_id: i64) -> Result<Vec<i64>, QueueError> {
    let mut stmt = conn
        .prepare(
            "SELECT depends_on_task_id FROM task_dependencies WHERE task_id = ? ORDER BY depends_on_task_id",
        )
        .db()?;
    let rows = stmt.query_map(params![task_id], |row| row.get(0)).db()?;
    let mut deps = Vec::new();
    for row in rows {
        deps.push(row.db()?);
    }
    Ok(deps)
}

fn load_task(conn: &Connection, id: i64) -> Result<Task, QueueError> {
    let mut task = conn
        .query_row(
            &format!("{TASK_SELECT} WHERE tasks.id = ?"),
            params![id],
            map_task,
        )
        .optional()
        .db()?
        .ok_or(QueueError::NotFound(id))?;
    task.dependencies = load_dependencies(conn, id)?;
    Ok(task)
}

fn load_task_in(tx: &Transaction<'_>, id: i64) -> Result<Task, QueueError> {
    load_task(tx, id)
}

fn map_claim(row: &rusqlite::Row<'_>, now: &str) -> rusqlite::Result<Claim> {
    let claimed_at: String = row.get(4)?;
    let heartbeat_at: String = row.get(5)?;
    let lease_expires_at: String = row.get(6)?;
    let released_at: Option<String> = row.get(9)?;
    let released = match released_at {
        Some(value) => Some(parse_time(9, &value)?),
        None => None,
    };
    Ok(Claim {
        id: row.get(0)?,
        task_id: row.get(1)?,
        agent_id: row.get(2)?,
        token: row.get(3)?,
        claimed_at: parse_time(4, &claimed_at)?,
        heartbeat_at: parse_time(5, &heartbeat_at)?,
        lease_expires_at: parse_time(6, &lease_expires_at)?,
        branch: row.get(7)?,
        worktree_path: row.get(8)?,
        released_at: released,
        release_reason: row.get(10)?,
        active: released.is_none() && lease_expires_at.as_str() > now,
    })
}

fn load_latest_claim(
    conn: &Connection,
    task_id: i64,
    now: &str,
) -> Result<Option<Claim>, QueueError> {
    conn.query_row(
        &format!("{CLAIM_SELECT} WHERE task_id = ? ORDER BY id DESC LIMIT 1"),
        params![task_id],
        |row| map_claim(row, now),
    )
    .optional()
    .db()
}

struct ActiveClaim {
    id: i64,
    agent_id: String,
}

fn require_active_claim(
    conn: &Connection,
    task_id: i64,
    token: &str,
    now: &str,
) -> Result<ActiveClaim, QueueError> {
    let token = token.trim();
    if token.is_empty() {
        return Err(QueueError::InvalidInput("claim token is required".into()));
    }
    let row = conn
        .query_row(
            "SELECT id, agent_id, released_at, lease_expires_at FROM claims WHERE task_id = ? AND claim_token = ?",
            params![task_id, token],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, String>(3)?,
                ))
            },
        )
        .optional()
        .db()?;
    let Some((id, agent_id, released_at, lease_expires_at)) = row else {
        return Err(QueueError::TokenMismatch);
    };
    if released_at.is_some() {
        return Err(QueueError::TokenMismatch);
    }
    if lease_expires_at.as_str() <= now {
        return Err(QueueError::ClaimExpired);
    }
    Ok(ActiveClaim { id, agent_id })
}

fn set_status(
    conn: &Connection,
    id: i64,
    from: TaskStatus,
    to: TaskStatus,
    now: &str,
) -> Result<(), QueueError> {
    let updated = conn
        .execute(
            "UPDATE tasks SET status = ?, updated_at = ? WHERE id = ? AND status = ?",
            params![to.as_str(), now, id, from.as_str()],
        )
        .db()?;
    if updated != 1 {
        return Err(QueueError::Conflict(format!(
            "task {id} changed concurrently"
        )));
    }
    Ok(())
}

fn retire_claim(conn: &Connection, claim_id: i64, now: &str) -> Result<(), QueueError> {
    conn.execute(
        "UPDATE claims SET released_at = ?, release_reason = NULL WHERE id = ? AND released_at IS NULL",
        params![now, claim_id],
    )
    .db()?;
    Ok(())
}

fn load_dep_edges(conn: &Connection) -> Result<HashMap<i64, Vec<i64>>, QueueError> {
    let mut stmt = conn
        .prepare("SELECT task_id, depends_on_task_id FROM task_dependencies")
        .db()?;
    let rows = stmt
        .query_map([], |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)))
        .db()?;
    let mut edges: HashMap<i64, Vec<i64>> = HashMap::new();
    for row in rows {
        let (task_id, depends_on) = row.db()?;
        edges.entry(task_id).or_default().push(depends_on);
    }
    Ok(edges)
}

fn validate_dependencies(
    conn: &Connection,
    task_id: i64,
    deps: &[i64],
) -> Result<Vec<i64>, QueueError> {
    let deps = dedupe_ids(deps);
    for dep in &deps {
        if *dep == task_id {
            return Err(QueueError::InvalidInput(
                "a task cannot depend on itself".into(),
            ));
        }
        let exists: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM tasks WHERE id = ?",
                params![dep],
                |row| row.get(0),
            )
            .db()?;
        if exists == 0 {
            return Err(QueueError::InvalidInput(format!(
                "dependency {dep} does not exist"
            )));
        }
    }
    let edges = load_dep_edges(conn)?;
    let mut stack = deps.clone();
    let mut seen = HashSet::new();
    while let Some(node) = stack.pop() {
        if node == task_id {
            return Err(QueueError::InvalidInput("dependency cycle".into()));
        }
        if !seen.insert(node) {
            continue;
        }
        if let Some(children) = edges.get(&node) {
            stack.extend(children.iter().copied());
        }
    }
    Ok(deps)
}

fn replace_dependencies(conn: &Connection, task_id: i64, deps: &[i64]) -> Result<(), QueueError> {
    let deps = validate_dependencies(conn, task_id, deps)?;
    conn.execute(
        "DELETE FROM task_dependencies WHERE task_id = ?",
        params![task_id],
    )
    .db()?;
    for dep in deps {
        conn.execute(
            "INSERT INTO task_dependencies (task_id, depends_on_task_id) VALUES (?, ?)",
            params![task_id, dep],
        )
        .db()?;
    }
    Ok(())
}

fn upsert_project(
    conn: &Connection,
    name: &str,
    repo: Option<&str>,
    policy: Option<&ProjectPolicy>,
    now: &str,
) -> Result<i64, QueueError> {
    let name = name.trim();
    if name.is_empty() {
        return Err(QueueError::InvalidInput("project name is empty".into()));
    }
    let require_pr = policy.map(|item| i64::from(item.require_pr)).unwrap_or(0);
    let allow_external = policy
        .map(|item| i64::from(item.allow_external_actions))
        .unwrap_or(0);
    let stale = policy
        .map(|item| item.stale_disposition.as_str())
        .unwrap_or("ready");
    let max_parallel = policy.and_then(|item| item.max_parallel_jobs);
    let config_path = policy.and_then(|item| item.config_path.clone());
    conn.execute(
        "INSERT INTO projects (
            name, repo, config_path, max_parallel_jobs, require_pr, allow_external_actions,
            stale_disposition, created_at, updated_at
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?8)
         ON CONFLICT(name) DO UPDATE SET
            repo = COALESCE(excluded.repo, projects.repo),
            updated_at = excluded.updated_at",
        params![
            name,
            repo,
            config_path,
            max_parallel,
            require_pr,
            allow_external,
            stale,
            now
        ],
    )
    .db()?;
    if let Some(policy) = policy {
        conn.execute(
            "UPDATE projects
             SET repo = COALESCE(?1, repo),
                 config_path = COALESCE(?2, config_path),
                 max_parallel_jobs = ?3,
                 require_pr = ?4,
                 allow_external_actions = ?5,
                 stale_disposition = ?6,
                 updated_at = ?7
             WHERE name = ?8",
            params![
                repo,
                policy.config_path,
                policy.max_parallel_jobs,
                i64::from(policy.require_pr),
                i64::from(policy.allow_external_actions),
                policy.stale_disposition.as_str(),
                now,
                name
            ],
        )
        .db()?;
    }
    conn.query_row(
        "SELECT id FROM projects WHERE name = ?",
        params![name],
        |row| row.get(0),
    )
    .db()
}

struct ProjectMeta {
    max_parallel_jobs: Option<i64>,
    allow_external_actions: bool,
    require_pr: bool,
    stale_disposition: StaleDisposition,
}

fn project_meta(conn: &Connection, name: Option<&str>) -> Result<ProjectMeta, QueueError> {
    let Some(name) = name.map(str::trim).filter(|name| !name.is_empty()) else {
        return Ok(ProjectMeta {
            max_parallel_jobs: None,
            allow_external_actions: false,
            require_pr: false,
            stale_disposition: StaleDisposition::Ready,
        });
    };
    let row = conn
        .query_row(
            "SELECT max_parallel_jobs, allow_external_actions, require_pr, stale_disposition
             FROM projects WHERE name = ?",
            params![name],
            |row| {
                Ok((
                    row.get::<_, Option<i64>>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, String>(3)?,
                ))
            },
        )
        .optional()
        .db()?;
    match row {
        Some((max_parallel_jobs, allow, require_pr, stale)) => Ok(ProjectMeta {
            max_parallel_jobs,
            allow_external_actions: allow != 0,
            require_pr: require_pr != 0,
            stale_disposition: StaleDisposition::parse(&stale).unwrap_or_default(),
        }),
        None => Ok(ProjectMeta {
            max_parallel_jobs: None,
            allow_external_actions: false,
            require_pr: false,
            stale_disposition: StaleDisposition::Ready,
        }),
    }
}

fn active_in_project(conn: &Connection, project: Option<&str>) -> Result<i64, QueueError> {
    let Some(project) = project else {
        return Ok(0);
    };
    conn.query_row(
        "SELECT COUNT(*) FROM tasks WHERE project_name = ? AND status IN ('claimed', 'in_progress')",
        params![project],
        |row| row.get(0),
    )
    .db()
}

fn dependency_statuses(conn: &Connection, deps: &[i64]) -> Result<Vec<TaskStatus>, QueueError> {
    let mut statuses = Vec::new();
    for dep in deps {
        let status = conn
            .query_row(
                "SELECT status FROM tasks WHERE id = ?",
                params![dep],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .db()?;
        match status {
            Some(value) => statuses.push(TaskStatus::parse(&value)?),
            None => statuses.push(TaskStatus::Blocked),
        }
    }
    Ok(statuses)
}

fn recover_expired(
    conn: &Connection,
    now: &str,
    override_to: Option<StaleDisposition>,
    actor: &Actor,
) -> Result<Vec<RecoveryRecord>, QueueError> {
    let mut stmt = conn
        .prepare(
            "SELECT c.id, c.task_id, c.agent_id, t.status, t.project_name
             FROM claims c
             JOIN tasks t ON t.id = c.task_id
             WHERE c.released_at IS NULL
               AND c.lease_expires_at <= ?1
               AND t.status IN ('claimed', 'in_progress')
             ORDER BY c.id ASC",
        )
        .db()?;
    let rows = stmt
        .query_map(params![now], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, Option<String>>(4)?,
            ))
        })
        .db()?;
    let mut pending = Vec::new();
    for row in rows {
        pending.push(row.db()?);
    }
    drop(stmt);

    let mut recovered = Vec::new();
    for (claim_id, task_id, agent_id, status_text, project_name) in pending {
        let from = TaskStatus::parse(&status_text)?;
        let disposition = match override_to {
            Some(value) => value,
            None => project_meta(conn, project_name.as_deref())?.stale_disposition,
        };
        let to = disposition.status();
        ensure_transition(from, to)?;
        if to == TaskStatus::Blocked {
            let updated = conn
                .execute(
                    "UPDATE tasks SET status = 'blocked', blocked_reason = NULL, updated_at = ? WHERE id = ? AND status = ?",
                    params![now, task_id, from.as_str()],
                )
                .db()?;
            if updated != 1 {
                return Err(QueueError::Conflict(format!(
                    "task {task_id} changed concurrently"
                )));
            }
        } else {
            set_status(conn, task_id, from, to, now)?;
        }
        retire_claim(conn, claim_id, now)?;
        insert_event(
            conn,
            Some(task_id),
            "task_recovered",
            actor,
            json!({
                "from": from.as_str(),
                "to": to.as_str(),
                "agent_id": agent_id,
            }),
            now,
        )?;
        recovered.push(RecoveryRecord {
            task_id,
            previous_status: from,
            new_status: to,
            agent_id,
        });
    }
    Ok(recovered)
}

fn select_eligible(conn: &Connection, request: &ClaimRequest) -> Result<Option<i64>, QueueError> {
    let mut stmt = conn
        .prepare(
            "SELECT id FROM tasks WHERE status = 'ready' ORDER BY priority DESC, created_at ASC, id ASC",
        )
        .db()?;
    let ids = stmt
        .query_map([], |row| row.get::<_, i64>(0))
        .db()?
        .collect::<Result<Vec<_>, _>>()
        .db()?;
    drop(stmt);

    for id in ids {
        let task = load_task(conn, id)?;
        let meta = project_meta(conn, task.project.as_deref())?;
        let active = active_in_project(conn, task.project.as_deref())?;
        let dependency_statuses = dependency_statuses(conn, &task.dependencies)?;
        let candidate = EligibilityTask {
            status: task.status,
            kind: task.kind,
            risk: task.risk,
            project: task.project.clone(),
            repo: task.repo.clone(),
            agent_pool: task.agent_pool.clone(),
            required_capabilities: task.required_capabilities.clone(),
            dependency_statuses,
        };
        if is_eligible(
            &candidate,
            request,
            active,
            meta.max_parallel_jobs,
            meta.allow_external_actions,
        ) {
            return Ok(Some(id));
        }
    }
    Ok(None)
}

fn detail_for(conn: &Connection, id: i64) -> Result<TaskDetail, QueueError> {
    let (_, now) = now_parts();
    let task = load_task(conn, id)?;
    let acceptance = acceptance_criteria(task.body.as_deref());
    let claim = load_latest_claim(conn, id, &now)?;
    let artifacts = load_artifacts(conn, id)?;
    let events = load_events(conn, Some(id), true, 30)?;
    Ok(TaskDetail {
        task,
        acceptance_criteria: acceptance,
        claim,
        artifacts,
        events,
    })
}

fn load_artifacts(conn: &Connection, task_id: i64) -> Result<Vec<Artifact>, QueueError> {
    let mut stmt = conn
        .prepare(
            "SELECT id, task_id, kind, value, created_at FROM artifacts WHERE task_id = ? ORDER BY id ASC",
        )
        .db()?;
    let rows = stmt
        .query_map(params![task_id], |row| {
            let created_at: String = row.get(4)?;
            Ok(Artifact {
                id: row.get(0)?,
                task_id: row.get(1)?,
                kind: row.get(2)?,
                value: row.get(3)?,
                created_at: parse_time(4, &created_at)?,
            })
        })
        .db()?;
    let mut artifacts = Vec::new();
    for row in rows {
        artifacts.push(row.db()?);
    }
    Ok(artifacts)
}

fn load_events(
    conn: &Connection,
    task_id: Option<i64>,
    recent_first_window: bool,
    limit: i64,
) -> Result<Vec<Event>, QueueError> {
    let (sql, param_task): (String, Option<i64>) = if let Some(task_id) = task_id {
        if recent_first_window {
            (
                format!("{EVENT_SELECT} WHERE task_id = ? ORDER BY id DESC LIMIT ?"),
                Some(task_id),
            )
        } else {
            (
                format!("{EVENT_SELECT} WHERE task_id = ? ORDER BY id ASC LIMIT ?"),
                Some(task_id),
            )
        }
    } else {
        (format!("{EVENT_SELECT} ORDER BY id ASC LIMIT ?"), None)
    };
    let mut stmt = conn.prepare(&sql).db()?;
    let mut rows = Vec::new();
    let map = |row: &rusqlite::Row<'_>| -> rusqlite::Result<Event> {
        let payload: String = row.get(5)?;
        let created_at: String = row.get(6)?;
        let payload: Value = serde_json::from_str(&payload).unwrap_or_else(|_| json!({}));
        Ok(Event {
            id: row.get(0)?,
            task_id: row.get(1)?,
            event_type: row.get(2)?,
            actor_type: row.get(3)?,
            actor_id: row.get(4)?,
            payload,
            created_at: parse_time(6, &created_at)?,
        })
    };
    if let Some(task_id) = param_task {
        let mapped = stmt.query_map(params![task_id, limit], map).db()?;
        for row in mapped {
            rows.push(row.db()?);
        }
    } else {
        let mapped = stmt.query_map(params![limit], map).db()?;
        for row in mapped {
            rows.push(row.db()?);
        }
    }
    if recent_first_window {
        rows.reverse();
    }
    Ok(rows)
}

fn insert_artifact(
    conn: &Connection,
    task_id: i64,
    artifact: &ArtifactInput,
    actor: &Actor,
    now: &str,
) -> Result<(), QueueError> {
    let kind = artifact.kind.trim();
    let value = artifact.value.trim();
    if kind.is_empty() || value.is_empty() {
        return Err(QueueError::InvalidInput(
            "artifact kind and value are required".into(),
        ));
    }
    if !kind
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '-')
    {
        return Err(QueueError::InvalidInput(
            "artifact kind must be a short token such as pr, commit, branch, report, or benchmark"
                .into(),
        ));
    }
    conn.execute(
        "INSERT INTO artifacts (task_id, kind, value, created_at) VALUES (?, ?, ?, ?)",
        params![task_id, kind, value, now],
    )
    .db()?;
    insert_event(
        conn,
        Some(task_id),
        "artifact_added",
        actor,
        json!({"kind": kind, "value": value}),
        now,
    )?;
    Ok(())
}

fn count_for_task(conn: &Connection, sql: &str, task_id: i64) -> Result<i64, QueueError> {
    conn.query_row(sql, params![task_id], |row| row.get(0)).db()
}

fn clean_feature_title(title: &str) -> Result<String, QueueError> {
    let title = title.trim();
    if title.is_empty() {
        return Err(QueueError::InvalidInput("title is required".into()));
    }
    if title.chars().any(|ch| ch == '\n' || ch == '\r') {
        return Err(QueueError::InvalidInput(
            "feature title must be a single line".into(),
        ));
    }
    Ok(title.to_string())
}

fn optional_body(body: Option<&str>) -> Option<String> {
    body.map(str::trim)
        .filter(|text| !text.is_empty())
        .map(str::to_string)
}

fn resolve_feature_id(conn: &Connection, selector: &str) -> Result<i64, QueueError> {
    let selector = selector.trim();
    if selector.is_empty() {
        return Err(QueueError::InvalidInput("feature is required".into()));
    }
    if let Ok(id) = selector.parse::<i64>() {
        let exists: Option<i64> = conn
            .query_row("SELECT id FROM features WHERE id = ?", params![id], |row| {
                row.get(0)
            })
            .optional()
            .db()?;
        if exists.is_some() {
            return Ok(id);
        }
    }
    let mut stmt = conn
        .prepare("SELECT id FROM features WHERE title = ?1 COLLATE NOCASE")
        .db()?;
    let rows = stmt.query_map(params![selector], |row| row.get(0)).db()?;
    let mut ids = Vec::new();
    for row in rows {
        ids.push(row.db()?);
    }
    match ids.as_slice() {
        [id] => Ok(*id),
        [] => Err(QueueError::FeatureNotFound(selector.to_string())),
        _ => Err(QueueError::Conflict(format!(
            "feature title '{selector}' matches more than one feature; pass the id"
        ))),
    }
}

fn optional_feature_id(
    conn: &Connection,
    selector: Option<&str>,
) -> Result<Option<i64>, QueueError> {
    let Some(selector) = selector else {
        return Ok(None);
    };
    let selector = selector.trim();
    if selector.is_empty() {
        return Err(QueueError::InvalidInput("feature is required".into()));
    }
    Ok(Some(resolve_feature_id(conn, selector)?))
}

fn map_feature(row: &rusqlite::Row<'_>) -> rusqlite::Result<Feature> {
    let public_id: String = row.get(1)?;
    let created_at: String = row.get(4)?;
    let updated_at: String = row.get(5)?;
    Ok(Feature {
        id: row.get(0)?,
        public_id: parse_uuid(1, &public_id)?,
        title: row.get(2)?,
        body: row.get(3)?,
        created_at: parse_time(4, &created_at)?,
        updated_at: parse_time(5, &updated_at)?,
        task_count: row.get(6)?,
    })
}

fn load_feature(conn: &Connection, id: i64) -> Result<Feature, QueueError> {
    conn.query_row(
        &format!("{FEATURE_SELECT} WHERE features.id = ?"),
        params![id],
        map_feature,
    )
    .optional()
    .db()?
    .ok_or_else(|| QueueError::FeatureNotFound(id.to_string()))
}

fn ensure_exists(conn: &Connection, id: i64) -> Result<(), QueueError> {
    let exists: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM tasks WHERE id = ?",
            params![id],
            |row| row.get(0),
        )
        .db()?;
    if exists == 0 {
        Err(QueueError::NotFound(id))
    } else {
        Ok(())
    }
}

impl QueueService for Queue {
    fn capture(&self, request: CaptureRequest) -> Result<Task, QueueError> {
        let title = request.title.trim();
        if title.is_empty() {
            return Err(QueueError::InvalidInput("title is required".into()));
        }
        let body = request
            .body
            .as_deref()
            .map(str::trim)
            .filter(|text| !text.is_empty())
            .map(str::to_string);
        let original_capture = match &body {
            Some(text) => format!("{title}\n\n{text}"),
            None => title.to_string(),
        };
        let repo = norm_repo(request.repo.as_deref());
        let project = request
            .project
            .as_deref()
            .map(str::trim)
            .filter(|text| !text.is_empty())
            .map(str::to_string);
        let caps = serde_json::to_string(&dedupe_strings(&request.required_capabilities))
            .map_err(|err| QueueError::InvalidInput(err.to_string()))?;
        let mut conn = open_connection(&self.path)?;
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .db()?;
        let (_, now) = now_parts();
        let project_id = if let Some(name) = &project {
            Some(upsert_project(
                &tx,
                name,
                repo.as_deref(),
                request.policy.as_ref(),
                &now,
            )?)
        } else {
            None
        };
        let feature_id = optional_feature_id(&tx, request.feature.as_deref())?;
        let public_id = Uuid::now_v7().to_string();
        tx.execute(
            "INSERT INTO tasks (
                public_id, title, body, original_capture, status, kind, priority, risk,
                project_id, project_name, repo, capture_path, repo_relative_path, git_root,
                git_head, agent_pool, required_capabilities_json, blocked_reason, feature_id,
                created_at, updated_at
             ) VALUES (?, ?, ?, ?, 'inbox', ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, NULL, ?, ?, ?)",
            params![
                public_id,
                title,
                body,
                original_capture,
                request.kind.as_str(),
                request.priority,
                request.risk.as_str(),
                project_id,
                &project,
                &repo,
                &request.capture_path,
                &request.repo_relative_path,
                &request.git_root,
                &request.git_head,
                &request.agent_pool,
                caps,
                feature_id,
                &now,
                &now
            ],
        )
        .db()?;
        let id = tx.last_insert_rowid();
        if !request.dependencies.is_empty() {
            replace_dependencies(&tx, id, &request.dependencies)?;
        }
        insert_event(
            &tx,
            Some(id),
            "task_created",
            &request.actor,
            json!({
                "title": title,
                "status": "inbox",
                "kind": request.kind.as_str(),
                "risk": request.risk.as_str(),
                "project": project,
                "repo": repo,
                "feature_id": feature_id,
                "capture_path": request.capture_path,
                "source": request.context_source,
            }),
            &now,
        )?;
        tx.commit().db()?;
        self.with_conn(|conn| load_task(conn, id))
    }

    fn list(&self, filter: ListFilter) -> Result<Vec<TaskSummary>, QueueError> {
        let conn = open_connection(&self.path)?;
        let status = filter.status.map(|status| status.as_str().to_string());
        let project = filter
            .project
            .as_deref()
            .map(str::trim)
            .filter(|text| !text.is_empty())
            .map(str::to_string);
        let repo = norm_repo(filter.repo.as_deref());
        let kind = filter.kind.map(|kind| kind.as_str().to_string());
        let feature_id = optional_feature_id(&conn, filter.feature.as_deref())?;
        let limit = i64::from(clamp_limit(filter.limit));
        // `include_terminal` applies only when status is unset. An explicit
        // status, including done or cancelled, is honored on its own.
        let include_terminal = i64::from(filter.include_terminal);
        let mut stmt = conn
            .prepare(
                "SELECT tasks.id, tasks.public_id, tasks.title, tasks.status, tasks.kind, \
                 tasks.priority, tasks.risk, tasks.project_name, tasks.repo, tasks.agent_pool, \
                 tasks.created_at, tasks.updated_at, tasks.feature_id, features.title \
                 FROM tasks \
                 LEFT JOIN features ON features.id = tasks.feature_id \
                 WHERE ((?1 IS NOT NULL AND tasks.status = ?1) \
                     OR (?1 IS NULL AND (?6 != 0 OR tasks.status NOT IN ('done', 'cancelled')))) \
                   AND (?2 IS NULL OR tasks.project_name = ?2) \
                   AND (?3 IS NULL OR tasks.repo = ?3) \
                   AND (?4 IS NULL OR tasks.kind = ?4) \
                   AND (?7 IS NULL OR tasks.feature_id = ?7) \
                 ORDER BY \
                   CASE \
                     WHEN features.title IS NULL OR TRIM(features.title) = '' THEN 1 \
                     ELSE 0 \
                   END, \
                   CASE \
                     WHEN features.title IS NULL OR TRIM(features.title) = '' THEN '' \
                     ELSE features.title \
                   END COLLATE NOCASE, \
                   CASE \
                     WHEN tasks.project_name IS NULL OR TRIM(tasks.project_name) = '' THEN 1 \
                     ELSE 0 \
                   END, \
                   CASE \
                     WHEN tasks.project_name IS NULL OR TRIM(tasks.project_name) = '' THEN '' \
                     ELSE tasks.project_name \
                   END COLLATE NOCASE, \
                   tasks.updated_at DESC, \
                   tasks.id DESC \
                 LIMIT ?5",
            )
            .db()?;
        let rows = stmt
            .query_map(
                params![
                    status,
                    project,
                    repo,
                    kind,
                    limit,
                    include_terminal,
                    feature_id
                ],
                |row| {
                    let public_id: String = row.get(1)?;
                    let status: String = row.get(3)?;
                    let kind: String = row.get(4)?;
                    let risk: String = row.get(6)?;
                    let created_at: String = row.get(10)?;
                    let updated_at: String = row.get(11)?;
                    Ok(TaskSummary {
                        id: row.get(0)?,
                        public_id: parse_uuid(1, &public_id)?,
                        title: row.get(2)?,
                        status: parse_status(3, &status)?,
                        kind: parse_kind(4, &kind)?,
                        priority: row.get(5)?,
                        risk: parse_risk(6, &risk)?,
                        project: row.get(7)?,
                        repo: row.get(8)?,
                        agent_pool: row.get(9)?,
                        created_at: parse_time(10, &created_at)?,
                        updated_at: parse_time(11, &updated_at)?,
                        feature_id: row.get(12)?,
                        feature: row.get(13)?,
                    })
                },
            )
            .db()?;
        let mut tasks = Vec::new();
        for row in rows {
            tasks.push(row.db()?);
        }
        Ok(tasks)
    }

    fn get(&self, id: i64) -> Result<TaskDetail, QueueError> {
        let conn = open_connection(&self.path)?;
        detail_for(&conn, id)
    }

    fn edit(&self, id: i64, request: EditRequest) -> Result<Task, QueueError> {
        if !request.has_changes() {
            return Err(QueueError::InvalidInput("no changes specified".into()));
        }
        if request.clear_feature && request.feature.is_some() {
            return Err(QueueError::InvalidInput(
                "pass either a feature or clear_feature, not both".into(),
            ));
        }
        let mut conn = open_connection(&self.path)?;
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .db()?;
        let (_, now) = now_parts();
        let mut task = load_task_in(&tx, id)?;
        let mut fields = Vec::new();
        if let Some(title) = request.title.as_deref() {
            let title = title.trim();
            if title.is_empty() {
                return Err(QueueError::InvalidInput("title is required".into()));
            }
            task.title = title.to_string();
            fields.push("title");
        }
        if let Some(body) = request.body.as_deref() {
            let body = body.trim();
            task.body = if body.is_empty() {
                None
            } else {
                Some(body.to_string())
            };
            fields.push("body");
        }
        if let Some(kind) = request.kind {
            task.kind = kind;
            fields.push("kind");
        }
        if let Some(priority) = request.priority {
            task.priority = priority;
            fields.push("priority");
        }
        if let Some(risk) = request.risk {
            task.risk = risk;
            fields.push("risk");
        }
        if request.clear_repo {
            task.repo = None;
            fields.push("repo");
        } else if let Some(repo) = request.repo.as_deref() {
            task.repo = norm_repo(Some(repo));
            fields.push("repo");
        }
        if request.clear_agent_pool {
            task.agent_pool = None;
            fields.push("agent_pool");
        } else if let Some(pool) = request.agent_pool.as_deref() {
            let pool = pool.trim();
            task.agent_pool = if pool.is_empty() {
                None
            } else {
                Some(pool.to_string())
            };
            fields.push("agent_pool");
        }
        if let Some(caps) = &request.required_capabilities {
            task.required_capabilities = dedupe_strings(caps);
            fields.push("required_capabilities");
        }
        let mut project_id: Option<i64> = tx
            .query_row(
                "SELECT project_id FROM tasks WHERE id = ?",
                params![id],
                |row| row.get(0),
            )
            .db()?;
        if request.clear_project {
            task.project = None;
            project_id = None;
            fields.push("project");
        } else if let Some(project) = request.project.as_deref() {
            let project = project.trim();
            if project.is_empty() {
                task.project = None;
                project_id = None;
            } else {
                project_id = Some(upsert_project(
                    &tx,
                    project,
                    task.repo.as_deref(),
                    None,
                    &now,
                )?);
                task.project = Some(project.to_string());
            }
            fields.push("project");
        }
        if request.clear_feature {
            task.feature_id = None;
            task.feature = None;
            fields.push("feature");
        } else if let Some(selector) = request.feature.as_deref() {
            task.feature_id = Some(resolve_feature_id(&tx, selector)?);
            fields.push("feature");
        }
        let caps = serde_json::to_string(&task.required_capabilities)
            .map_err(|err| QueueError::InvalidInput(err.to_string()))?;
        tx.execute(
            "UPDATE tasks SET
                title = ?, body = ?, kind = ?, priority = ?, risk = ?, project_id = ?,
                project_name = ?, repo = ?, agent_pool = ?, required_capabilities_json = ?,
                feature_id = ?, updated_at = ?
             WHERE id = ?",
            params![
                task.title,
                task.body,
                task.kind.as_str(),
                task.priority,
                task.risk.as_str(),
                project_id,
                task.project,
                task.repo,
                task.agent_pool,
                caps,
                task.feature_id,
                now,
                id
            ],
        )
        .db()?;
        if let Some(deps) = &request.dependencies {
            replace_dependencies(&tx, id, deps)?;
            fields.push("dependencies");
        }
        insert_event(
            &tx,
            Some(id),
            "task_edited",
            &request.actor,
            json!({"fields": fields}),
            &now,
        )?;
        tx.commit().db()?;
        self.with_conn(|conn| load_task(conn, id))
    }

    fn mark_ready(&self, request: ReadyRequest) -> Result<ReadyOutcome, QueueError> {
        let mut conn = open_connection(&self.path)?;
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .db()?;
        let (_, now) = now_parts();
        let task = load_task_in(&tx, request.task_id)?;
        ensure_transition(task.status, TaskStatus::Ready)?;
        let risk_note = if task.risk >= RiskLevel::High {
            Some(format!(
                "risk is {}; default claims will not select this task",
                task.risk
            ))
        } else {
            None
        };
        let warnings = readiness_warnings(task.body.as_deref(), risk_note);
        set_status(&tx, task.id, task.status, TaskStatus::Ready, &now)?;
        tx.execute(
            "UPDATE tasks SET blocked_reason = NULL WHERE id = ?",
            params![task.id],
        )
        .db()?;
        insert_event(
            &tx,
            Some(task.id),
            "task_ready",
            &request.actor,
            json!({
                "from": task.status.as_str(),
                "to": "ready",
                "warnings": warnings,
            }),
            &now,
        )?;
        tx.commit().db()?;
        let task = self.with_conn(|conn| load_task(conn, request.task_id))?;
        Ok(ReadyOutcome { task, warnings })
    }

    fn block(&self, request: BlockRequest) -> Result<Task, QueueError> {
        let mut conn = open_connection(&self.path)?;
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .db()?;
        let (_, now) = now_parts();
        let task = load_task_in(&tx, request.task_id)?;
        ensure_transition(task.status, TaskStatus::Blocked)?;
        let mut actor = request.actor.clone();
        if matches!(task.status, TaskStatus::Claimed | TaskStatus::InProgress) {
            let token = request.claim_token.as_deref().ok_or_else(|| {
                QueueError::InvalidInput("active claim requires --claim-token".into())
            })?;
            let claim = require_active_claim(&tx, task.id, token, &now)?;
            actor = Actor::agent(claim.agent_id);
            retire_claim(&tx, claim.id, &now)?;
        }
        let updated = tx
            .execute(
                "UPDATE tasks SET status = 'blocked', blocked_reason = NULL, updated_at = ? WHERE id = ? AND status = ?",
                params![now, task.id, task.status.as_str()],
            )
            .db()?;
        if updated != 1 {
            return Err(QueueError::Conflict(format!(
                "task {} changed concurrently",
                task.id
            )));
        }
        insert_event(
            &tx,
            Some(task.id),
            "task_blocked",
            &actor,
            json!({"from": task.status.as_str(), "to": "blocked"}),
            &now,
        )?;
        tx.commit().db()?;
        self.with_conn(|conn| load_task(conn, request.task_id))
    }

    fn cancel(&self, request: CancelRequest) -> Result<Task, QueueError> {
        let mut conn = open_connection(&self.path)?;
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .db()?;
        let (_, now) = now_parts();
        let task = load_task_in(&tx, request.task_id)?;
        ensure_transition(task.status, TaskStatus::Cancelled)?;
        set_status(&tx, task.id, task.status, TaskStatus::Cancelled, &now)?;
        insert_event(
            &tx,
            Some(task.id),
            "task_cancelled",
            &request.actor,
            json!({"from": task.status.as_str(), "to": "cancelled"}),
            &now,
        )?;
        tx.commit().db()?;
        self.with_conn(|conn| load_task(conn, request.task_id))
    }

    fn delete(&self, request: DeleteRequest) -> Result<DeleteOutcome, QueueError> {
        let mut conn = open_connection(&self.path)?;
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .db()?;
        let (_, now) = now_parts();
        let task = load_task_in(&tx, request.task_id)?;
        let active = tx
            .query_row(
                "SELECT agent_id, lease_expires_at FROM claims
                 WHERE task_id = ?1 AND released_at IS NULL AND lease_expires_at > ?2
                 ORDER BY id DESC LIMIT 1",
                params![task.id, now],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
            )
            .optional()
            .db()?;
        if let Some((agent_id, lease_expires_at)) = &active {
            if !request.force {
                return Err(QueueError::Conflict(format!(
                    "task {} has an active claim held by {agent_id} until {lease_expires_at}; pass --force to delete the task and clear the claim",
                    task.id
                )));
            }
        }
        let claims_removed = count_for_task(
            &tx,
            "SELECT COUNT(*) FROM claims WHERE task_id = ?",
            task.id,
        )?;
        let events_removed = count_for_task(
            &tx,
            "SELECT COUNT(*) FROM events WHERE task_id = ?",
            task.id,
        )?;
        let artifacts_removed = count_for_task(
            &tx,
            "SELECT COUNT(*) FROM artifacts WHERE task_id = ?",
            task.id,
        )?;
        let dependencies_removed: i64 = tx
            .query_row(
                "SELECT COUNT(*) FROM task_dependencies WHERE task_id = ?1 OR depends_on_task_id = ?1",
                params![task.id],
                |row| row.get(0),
            )
            .db()?;
        // Schema FKs use ON DELETE CASCADE. Delete dependents explicitly in this
        // same transaction so a schema without cascade still leaves no orphans.
        // Events reference the task, so a task_deleted row cannot survive.
        tx.execute("DELETE FROM claims WHERE task_id = ?", params![task.id])
            .db()?;
        tx.execute("DELETE FROM artifacts WHERE task_id = ?", params![task.id])
            .db()?;
        tx.execute("DELETE FROM events WHERE task_id = ?", params![task.id])
            .db()?;
        tx.execute(
            "DELETE FROM task_dependencies WHERE task_id = ?1 OR depends_on_task_id = ?1",
            params![task.id],
        )
        .db()?;
        let deleted = tx
            .execute("DELETE FROM tasks WHERE id = ?", params![task.id])
            .db()?;
        if deleted != 1 {
            return Err(QueueError::NotFound(task.id));
        }
        tx.commit().db()?;
        Ok(DeleteOutcome {
            task_id: task.id,
            public_id: task.public_id,
            title: task.title,
            status: task.status,
            forced: request.force,
            active_claim_cleared: active.is_some(),
            claims_removed,
            events_removed,
            artifacts_removed,
            dependencies_removed,
        })
    }

    fn claim_next(&self, request: ClaimRequest) -> Result<ClaimOutcome, QueueError> {
        let agent_id = request.agent_id.trim();
        if agent_id.is_empty() {
            return Err(QueueError::InvalidInput("agent_id is required".into()));
        }
        let lease = lease_or_default(request.lease)?;
        let request = ClaimRequest {
            agent_id: agent_id.to_string(),
            capabilities: dedupe_strings(&request.capabilities),
            allowed_repos: request
                .allowed_repos
                .iter()
                .filter_map(|repo| norm_repo(Some(repo)))
                .collect(),
            allowed_projects: request
                .allowed_projects
                .iter()
                .map(|project| project.trim().to_string())
                .filter(|project| !project.is_empty())
                .collect(),
            allowed_kinds: request.allowed_kinds.clone(),
            agent_pool: request
                .agent_pool
                .as_deref()
                .map(str::trim)
                .filter(|pool| !pool.is_empty())
                .map(str::to_string),
            maximum_risk: request.maximum_risk,
            lease,
        };
        let mut conn = open_connection(&self.path)?;
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .db()?;
        let (now_dt, now) = now_parts();
        recover_expired(&tx, &now, None, &Actor::system())?;
        let Some(task_id) = select_eligible(&tx, &request)? else {
            tx.commit().db()?;
            tracing::info!(agent_id = %request.agent_id, found = false, "claim_next");
            return Ok(ClaimOutcome::none());
        };
        let expires = format_timestamp(add_lease(now_dt, lease)?);
        let updated = tx
            .execute(
                "UPDATE tasks SET status = 'claimed', updated_at = ? WHERE id = ? AND status = 'ready'",
                params![now, task_id],
            )
            .db()?;
        if updated != 1 {
            tx.commit().db()?;
            tracing::info!(agent_id = %request.agent_id, found = false, "claim_next");
            return Ok(ClaimOutcome::none());
        }
        let token = Uuid::new_v4().to_string();
        tx.execute(
            "INSERT INTO claims (
                task_id, agent_id, claim_token, claimed_at, heartbeat_at, lease_expires_at
             ) VALUES (?, ?, ?, ?, ?, ?)",
            params![task_id, &request.agent_id, &token, &now, &now, &expires],
        )
        .db()?;
        insert_event(
            &tx,
            Some(task_id),
            "task_claimed",
            &Actor::agent(&request.agent_id),
            json!({
                "from": "ready",
                "to": "claimed",
                "agent_id": request.agent_id,
                "lease_expires_at": expires,
            }),
            &now,
        )?;
        tx.commit().db()?;
        tracing::info!(agent_id = %request.agent_id, found = true, task_id, "claim_next");
        let detail = self.get(task_id)?;
        Ok(ClaimOutcome {
            found: true,
            reason: None,
            task: Some(ClaimTask {
                acceptance_criteria: detail.acceptance_criteria,
                task: detail.task,
            }),
            claim: Some(ClaimLease {
                token,
                lease_expires_at: parse_timestamp(&expires)?,
                agent_id: request.agent_id,
            }),
        })
    }

    fn heartbeat(&self, request: HeartbeatRequest) -> Result<Claim, QueueError> {
        let lease = match request.lease {
            Some(lease) => lease_or_default(lease)?,
            None => default_lease(),
        };
        let mut conn = open_connection(&self.path)?;
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .db()?;
        let (now_dt, now) = now_parts();
        ensure_exists(&tx, request.task_id)?;
        let claim = require_active_claim(&tx, request.task_id, &request.claim_token, &now)?;
        let expires = format_timestamp(add_lease(now_dt, lease)?);
        tx.execute(
            "UPDATE claims SET heartbeat_at = ?, lease_expires_at = ? WHERE id = ?",
            params![now, expires, claim.id],
        )
        .db()?;
        insert_event(
            &tx,
            Some(request.task_id),
            "task_heartbeat",
            &Actor::agent(&claim.agent_id),
            json!({"lease_expires_at": expires}),
            &now,
        )?;
        tx.commit().db()?;
        self.with_conn(|conn| load_latest_claim(conn, request.task_id, &now))?
            .ok_or(QueueError::NotFound(request.task_id))
    }

    fn start(&self, request: StartRequest) -> Result<TaskDetail, QueueError> {
        let mut conn = open_connection(&self.path)?;
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .db()?;
        let (_, now) = now_parts();
        let task = load_task_in(&tx, request.task_id)?;
        ensure_transition(task.status, TaskStatus::InProgress)?;
        let claim = require_active_claim(&tx, task.id, &request.claim_token, &now)?;
        set_status(&tx, task.id, task.status, TaskStatus::InProgress, &now)?;
        tx.execute(
            "UPDATE claims SET branch = COALESCE(?, branch), worktree_path = COALESCE(?, worktree_path), heartbeat_at = ? WHERE id = ?",
            params![&request.branch, &request.worktree_path, &now, claim.id],
        )
        .db()?;
        insert_event(
            &tx,
            Some(task.id),
            "task_started",
            &Actor::agent(&claim.agent_id),
            json!({
                "from": task.status.as_str(),
                "to": "in_progress",
                "branch": request.branch,
                "worktree_path": request.worktree_path,
            }),
            &now,
        )?;
        tx.commit().db()?;
        self.get(request.task_id)
    }

    fn complete(&self, request: CompleteRequest) -> Result<TaskDetail, QueueError> {
        if let Some(target) = request.target {
            if target != TaskStatus::Review && target != TaskStatus::Done {
                return Err(QueueError::InvalidInput(
                    "complete target must be review or done".into(),
                ));
            }
        }
        let summary = request.summary.trim().to_string();
        let mut conn = open_connection(&self.path)?;
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .db()?;
        let (_, now) = now_parts();
        let task = load_task_in(&tx, request.task_id)?;
        let meta = project_meta(&tx, task.project.as_deref())?;
        let mut target = if task.status == TaskStatus::Review {
            TaskStatus::Done
        } else {
            request.target.unwrap_or(TaskStatus::Done)
        };
        if task.status != TaskStatus::Review
            && meta.require_pr
            && task.kind == TaskKind::Implementation
            && target == TaskStatus::Done
        {
            target = TaskStatus::Review;
        }

        let actor = if matches!(task.status, TaskStatus::Claimed | TaskStatus::InProgress) {
            if summary.is_empty() {
                return Err(QueueError::InvalidInput(
                    "complete requires a nonempty summary".into(),
                ));
            }
            let token = request.claim_token.as_deref().unwrap_or("");
            let claim = require_active_claim(&tx, task.id, token, &now)?;
            let mut status = task.status;
            if status == TaskStatus::Claimed {
                ensure_transition(status, TaskStatus::InProgress)?;
                set_status(&tx, task.id, status, TaskStatus::InProgress, &now)?;
                insert_event(
                    &tx,
                    Some(task.id),
                    "task_started",
                    &Actor::agent(&claim.agent_id),
                    json!({"from": "claimed", "to": "in_progress", "implicit": true}),
                    &now,
                )?;
                status = TaskStatus::InProgress;
            }
            ensure_transition(status, target)?;
            set_status(&tx, task.id, status, target, &now)?;
            retire_claim(&tx, claim.id, &now)?;
            Actor::agent(claim.agent_id)
        } else if task.status == TaskStatus::Review {
            ensure_transition(task.status, target)?;
            set_status(&tx, task.id, task.status, target, &now)?;
            request.actor.clone()
        } else {
            return Err(QueueError::InvalidTransition {
                from: task.status,
                to: target,
            });
        };

        for artifact in &request.artifacts {
            insert_artifact(&tx, task.id, artifact, &actor, &now)?;
        }
        if !summary.is_empty() {
            insert_artifact(
                &tx,
                task.id,
                &ArtifactInput {
                    kind: "summary".into(),
                    value: summary.clone(),
                },
                &actor,
                &now,
            )?;
        }
        insert_event(
            &tx,
            Some(task.id),
            "task_completed",
            &actor,
            json!({
                "from": task.status.as_str(),
                "to": target.as_str(),
                "summary": summary,
            }),
            &now,
        )?;
        tx.commit().db()?;
        self.get(request.task_id)
    }

    fn release(&self, request: ReleaseRequest) -> Result<Task, QueueError> {
        let mut conn = open_connection(&self.path)?;
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .db()?;
        let (_, now) = now_parts();
        let task = load_task_in(&tx, request.task_id)?;
        ensure_transition(task.status, TaskStatus::Ready)?;
        let claim = require_active_claim(&tx, task.id, &request.claim_token, &now)?;
        set_status(&tx, task.id, task.status, TaskStatus::Ready, &now)?;
        retire_claim(&tx, claim.id, &now)?;
        insert_event(
            &tx,
            Some(task.id),
            "task_released",
            &Actor::agent(&claim.agent_id),
            json!({"from": task.status.as_str(), "to": "ready"}),
            &now,
        )?;
        tx.commit().db()?;
        self.with_conn(|conn| load_task(conn, request.task_id))
    }

    fn recover_stale(&self, request: RecoverRequest) -> Result<Vec<RecoveryRecord>, QueueError> {
        let mut conn = open_connection(&self.path)?;
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .db()?;
        let (_, now) = now_parts();
        let recovered = recover_expired(&tx, &now, request.to, &request.actor)?;
        tx.commit().db()?;
        Ok(recovered)
    }

    fn events(&self, task_id: i64) -> Result<Vec<Event>, QueueError> {
        let conn = open_connection(&self.path)?;
        ensure_exists(&conn, task_id)?;
        load_events(&conn, Some(task_id), false, 1000)
    }

    fn status(&self) -> Result<QueueStatus, QueueError> {
        let conn = open_connection(&self.path)?;
        let (_, now) = now_parts();
        let mut counts = StatusCounts::default();
        let mut stmt = conn
            .prepare("SELECT status, COUNT(*) FROM tasks GROUP BY status")
            .db()?;
        let rows = stmt
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
            })
            .db()?;
        for row in rows {
            let (status, count) = row.db()?;
            match TaskStatus::parse(&status)? {
                TaskStatus::Inbox => counts.inbox = count,
                TaskStatus::Ready => counts.ready = count,
                TaskStatus::Claimed => counts.claimed = count,
                TaskStatus::InProgress => counts.in_progress = count,
                TaskStatus::Review => counts.review = count,
                TaskStatus::Blocked => counts.blocked = count,
                TaskStatus::Done => counts.done = count,
                TaskStatus::Cancelled => counts.cancelled = count,
            }
        }
        drop(stmt);
        let active_claims: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM claims WHERE released_at IS NULL AND lease_expires_at > ?",
                params![now],
                |row| row.get(0),
            )
            .db()?;
        let expired_claims: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM claims c
                 JOIN tasks t ON t.id = c.task_id
                 WHERE c.released_at IS NULL
                   AND c.lease_expires_at <= ?1
                   AND t.status IN ('claimed', 'in_progress')",
                params![now],
                |row| row.get(0),
            )
            .db()?;
        Ok(QueueStatus {
            counts,
            active_claims,
            expired_claims,
        })
    }

    fn reopen(&self, id: i64, actor: Actor) -> Result<Task, QueueError> {
        let mut conn = open_connection(&self.path)?;
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .db()?;
        let (_, now) = now_parts();
        let task = load_task_in(&tx, id)?;
        let to = match task.status {
            TaskStatus::Done => TaskStatus::Ready,
            TaskStatus::Cancelled => TaskStatus::Inbox,
            other => {
                return Err(QueueError::InvalidInput(format!(
                    "only done or cancelled tasks can be reopened (status is {other})"
                )));
            }
        };
        ensure_transition(task.status, to)?;
        set_status(&tx, task.id, task.status, to, &now)?;
        if to == TaskStatus::Inbox {
            tx.execute(
                "UPDATE tasks SET blocked_reason = NULL WHERE id = ?",
                params![id],
            )
            .db()?;
        }
        insert_event(
            &tx,
            Some(id),
            "task_reopened",
            &actor,
            json!({"from": task.status.as_str(), "to": to.as_str()}),
            &now,
        )?;
        tx.commit().db()?;
        self.with_conn(|conn| load_task(conn, id))
    }

    fn create_feature(&self, request: CreateFeatureRequest) -> Result<Feature, QueueError> {
        let title = clean_feature_title(&request.title)?;
        let body = optional_body(request.body.as_deref());
        let mut conn = open_connection(&self.path)?;
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .db()?;
        let (_, now) = now_parts();
        let public_id = Uuid::now_v7().to_string();
        tx.execute(
            "INSERT INTO features (public_id, title, body, created_at, updated_at)
             VALUES (?, ?, ?, ?, ?)",
            params![public_id, title, body, now, now],
        )
        .db()?;
        let id = tx.last_insert_rowid();
        tx.commit().db()?;
        self.with_conn(|conn| load_feature(conn, id))
    }

    fn list_features(&self) -> Result<Vec<Feature>, QueueError> {
        let conn = open_connection(&self.path)?;
        let mut stmt = conn
            .prepare(&format!(
                "{FEATURE_SELECT} ORDER BY features.title COLLATE NOCASE, features.id"
            ))
            .db()?;
        let rows = stmt.query_map(params![], map_feature).db()?;
        let mut features = Vec::new();
        for row in rows {
            features.push(row.db()?);
        }
        Ok(features)
    }

    fn get_feature(&self, id: i64) -> Result<Feature, QueueError> {
        self.with_conn(|conn| load_feature(conn, id))
    }

    fn edit_feature(&self, id: i64, request: EditFeatureRequest) -> Result<Feature, QueueError> {
        if !request.has_changes() {
            return Err(QueueError::InvalidInput("no changes specified".into()));
        }
        let mut conn = open_connection(&self.path)?;
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .db()?;
        let (_, now) = now_parts();
        let mut feature = load_feature(&tx, id)?;
        if let Some(title) = request.title.as_deref() {
            feature.title = clean_feature_title(title)?;
        }
        if let Some(body) = request.body.as_deref() {
            feature.body = optional_body(Some(body));
        }
        tx.execute(
            "UPDATE features SET title = ?, body = ?, updated_at = ? WHERE id = ?",
            params![feature.title, feature.body, now, id],
        )
        .db()?;
        tx.commit().db()?;
        self.with_conn(|conn| load_feature(conn, id))
    }

    fn delete_feature(&self, id: i64) -> Result<DeleteFeatureOutcome, QueueError> {
        let mut conn = open_connection(&self.path)?;
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .db()?;
        let feature = load_feature(&tx, id)?;
        let tasks_detached: i64 = tx
            .query_row(
                "SELECT COUNT(*) FROM tasks WHERE feature_id = ?",
                params![id],
                |row| row.get(0),
            )
            .db()?;
        tx.execute(
            "UPDATE tasks SET feature_id = NULL WHERE feature_id = ?",
            params![id],
        )
        .db()?;
        let deleted = tx
            .execute("DELETE FROM features WHERE id = ?", params![id])
            .db()?;
        if deleted != 1 {
            return Err(QueueError::FeatureNotFound(id.to_string()));
        }
        tx.commit().db()?;
        Ok(DeleteFeatureOutcome {
            id: feature.id,
            public_id: feature.public_id,
            title: feature.title,
            tasks_detached,
        })
    }
}

#[cfg(test)]
mod tests;
