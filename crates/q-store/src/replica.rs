//! Local-first sync against a Turso/libsql central authority.
//!
//! # Why this is not an embedded replica
//!
//! `libsql::Builder::new_remote_replica` keeps a local read replica and
//! **forwards writes to the primary**. That cannot accept writes while the
//! primary is unreachable. `new_synced_database` / Turso sync can write
//! offline, but the stock conflict rule is last-push-wins and would silently
//! overwrite a claim. q keeps the working copy in the local SQLite file
//! (unchanged rusqlite transactions, including `BEGIN IMMEDIATE` claims) and
//! uses `libsql::Builder::new_remote` only as the central authority.
//!
//! # Claims
//!
//! Creating a task offline is allowed. The row stays local (`dirty = 1`) and
//! is pushed on the next successful sync. Dequeue is not. When a remote URL is
//! configured, `claim` requires a live link to this authority and a successful
//! compare-and-swap before a token is returned. An offline or unreachable
//! authority yields no eligible work. Local-only queues (no remote URL) claim
//! against the local file.
//!
//! # Idempotency
//!
//! Tasks carry `idempotency_key`. Capture stores an explicit key or a derived
//! `content:` hash. On sync, two public ids with the same key collapse to the
//! authority's row. The losing local row is tombstoned and a `task_deduped`
//! event is recorded.
//!
//! # Sync
//!
//! Tasks are keyed by `public_id`, claims by `claim_token`. Dirty local rows
//! are pushed. Remote rows the local file does not have, or that are newer and
//! not dirty locally, are pulled. If the authority already has a claim token
//! the local row lacks — the crash window after a compare-and-swap, before
//! the local commit — that remote row is adopted quietly and the local active
//! claim, if any, is released with `release_reason = superseded`. The same
//! token is this worker's own heartbeat or completion catching up. A status
//! compare-and-swap that loses on push adopts the fresh remote row and counts
//! a conflict. A fresh offline task the authority has never seen is inserted.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use rusqlite::{params, Connection, OptionalExtension};
use serde_json::json;
use time::OffsetDateTime;
use uuid::Uuid;

use q_core::{format_timestamp, QueueError};

use crate::libsql_remote;
use crate::session::{Db, SqlVal};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkState {
    LocalOnly,
    Online,
    Offline,
}

impl LinkState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::LocalOnly => "local_only",
            Self::Online => "online",
            Self::Offline => "offline",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteConfig {
    pub url: String,
    pub auth_token: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncReport {
    pub link: LinkState,
    pub pulled: u64,
    pub pushed: u64,
    pub conflicts: u64,
    pub deduped: u64,
}

#[derive(Debug, Clone)]
pub(crate) enum RemoteBackend {
    Libsql,
    /// Second SQLite file used as the authority in tests. Not constructed by the library.
    #[allow(dead_code)]
    Sqlite(PathBuf),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ClaimSide {
    pub status: String,
    pub updated_at: String,
    pub latest_claim_token: Option<String>,
    pub dirty: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MergeChoice {
    KeepLocal,
    AdoptRemote,
}

/// A remote claim token the local row does not share means the authority
/// already accepted a claim. Adopt that row even when the local copy is dirty.
/// That is the crash window after compare-and-swap, before the local commit.
fn remote_claim_wins(local: &ClaimSide, remote: &ClaimSide) -> bool {
    remote.latest_claim_token.is_some()
        && local.latest_claim_token.as_deref() != remote.latest_claim_token.as_deref()
}

/// Pure merge decision. See the module docs.
pub(crate) fn choose_merge(local: &ClaimSide, remote: &ClaimSide) -> MergeChoice {
    if remote_claim_wins(local, remote) {
        return MergeChoice::AdoptRemote;
    }
    if local.dirty {
        return MergeChoice::KeepLocal;
    }
    if remote.updated_at >= local.updated_at {
        return MergeChoice::AdoptRemote;
    }
    MergeChoice::KeepLocal
}

pub fn resolve_remote_config(url: Option<String>, token: Option<String>) -> Option<RemoteConfig> {
    let url = first_nonempty([
        url,
        env_var("Q_TURSO_URL"),
        env_var("LIBSQL_URL"),
        env_var("TURSO_DATABASE_URL"),
    ]);
    let auth_token = first_nonempty([
        token,
        env_var("Q_TURSO_AUTH_TOKEN"),
        env_var("LIBSQL_AUTH_TOKEN"),
        env_var("TURSO_AUTH_TOKEN"),
    ])
    .unwrap_or_default();
    url.map(|url| RemoteConfig { url, auth_token })
}

fn env_var(key: &str) -> Option<String> {
    std::env::var(key).ok()
}

fn first_nonempty(values: [Option<String>; 4]) -> Option<String> {
    values
        .into_iter()
        .flatten()
        .find(|value| !value.trim().is_empty())
}

pub(crate) fn probe(backend: &RemoteBackend, config: &RemoteConfig) -> bool {
    match backend {
        RemoteBackend::Sqlite(path) => crate::open_connection(path).is_ok(),
        RemoteBackend::Libsql => {
            libsql_remote::with_remote(&config.url, &config.auth_token, |db| {
                db.query("SELECT 1", &[])?;
                Ok(())
            })
            .is_ok()
        }
    }
}

pub(crate) fn synchronize(
    local_path: &Path,
    backend: &RemoteBackend,
    config: &RemoteConfig,
) -> Result<SyncReport, QueueError> {
    let mut local = crate::open_connection(local_path)?;
    with_backend(backend, config, |remote| {
        crate::schema::migrate_in(remote)?;
        run_sync(&mut local, remote)
    })
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn publish_local_claim(
    conn: &Connection,
    backend: &RemoteBackend,
    config: &RemoteConfig,
    task_id: i64,
    agent_id: &str,
    token: &str,
    claimed_at: &str,
    lease_expires_at: &str,
) -> Result<bool, QueueError> {
    let bundle = load_publish_bundle(conn, task_id, agent_id, token, claimed_at, lease_expires_at)?;
    with_backend(backend, config, |remote| {
        crate::schema::migrate_in(remote)?;
        publish_bundle(remote, &bundle)
    })
}

pub(crate) fn record_tombstone(
    conn: &Connection,
    entity: &str,
    public_id: &str,
    now: &str,
) -> Result<(), QueueError> {
    conn.execute(
        "INSERT INTO sync_tombstones (entity, public_id, deleted_at) VALUES (?1, ?2, ?3)
         ON CONFLICT(entity, public_id) DO UPDATE SET deleted_at = excluded.deleted_at",
        params![entity, public_id, now],
    )
    .map_err(|err| QueueError::Database(err.to_string()))?;
    Ok(())
}

fn with_backend<T>(
    backend: &RemoteBackend,
    config: &RemoteConfig,
    f: impl FnOnce(&mut dyn Db) -> Result<T, QueueError>,
) -> Result<T, QueueError> {
    match backend {
        RemoteBackend::Libsql => {
            libsql_remote::with_remote(&config.url, &config.auth_token, |db| f(db))
        }
        RemoteBackend::Sqlite(path) => {
            let mut conn = crate::open_connection(path)?;
            f(&mut conn)
        }
    }
}

#[derive(Debug, Clone)]
struct TaskRec {
    public_id: String,
    title: String,
    body: Option<String>,
    original_capture: String,
    status: String,
    kind: String,
    priority: i64,
    risk: String,
    project_name: Option<String>,
    repo: Option<String>,
    capture_path: String,
    repo_relative_path: Option<String>,
    git_root: Option<String>,
    git_head: Option<String>,
    agent_pool: Option<String>,
    required_capabilities_json: String,
    blocked_reason: Option<String>,
    feature_public_id: Option<String>,
    created_at: String,
    updated_at: String,
    idempotency_key: Option<String>,
}

#[derive(Debug, Clone)]
struct ClaimRec {
    id: i64,
    task_public_id: String,
    agent_id: String,
    claim_token: String,
    claimed_at: String,
    heartbeat_at: String,
    lease_expires_at: String,
    branch: Option<String>,
    worktree_path: Option<String>,
    released_at: Option<String>,
    release_reason: Option<String>,
}

#[derive(Debug, Clone)]
struct EventRec {
    public_id: String,
    task_public_id: String,
    event_type: String,
    actor_type: String,
    actor_id: Option<String>,
    payload_json: String,
    created_at: String,
}

#[derive(Debug, Clone)]
struct ArtifactRec {
    public_id: String,
    task_public_id: String,
    kind: String,
    value: String,
    created_at: String,
}

#[derive(Debug, Clone)]
struct FeatureRec {
    public_id: String,
    title: String,
    body: Option<String>,
    created_at: String,
    updated_at: String,
}

#[derive(Debug, Clone)]
struct ProjectRec {
    name: String,
    repo: Option<String>,
    config_path: Option<String>,
    max_parallel_jobs: Option<i64>,
    require_pr: i64,
    allow_external_actions: i64,
    stale_disposition: String,
    created_at: String,
    updated_at: String,
}

struct Snapshot {
    tasks: Vec<TaskRec>,
    claims: Vec<ClaimRec>,
    events: Vec<EventRec>,
    artifacts: Vec<ArtifactRec>,
    deps: Vec<(String, String)>,
    features: Vec<FeatureRec>,
    projects: Vec<ProjectRec>,
    tombstones: Vec<(String, String, String)>,
}

struct PublishBundle {
    task: TaskRec,
    claim: ClaimRec,
    project: Option<ProjectRec>,
    feature: Option<FeatureRec>,
}

fn run_sync(local: &mut dyn Db, remote: &mut dyn Db) -> Result<SyncReport, QueueError> {
    let remote_snap = load_snapshot(remote)?;
    local.begin_immediate()?;
    let result = apply_and_push(local, remote, &remote_snap);
    if result.is_err() {
        let _ = local.rollback();
    } else if let Err(err) = local.commit() {
        let _ = local.rollback();
        return Err(err);
    }
    result
}

fn apply_and_push(
    local: &mut dyn Db,
    remote: &mut dyn Db,
    remote_snap: &Snapshot,
) -> Result<SyncReport, QueueError> {
    let mut pulled = 0u64;
    let mut conflicts = 0u64;
    let tombstones: HashSet<(String, String)> = remote_snap
        .tombstones
        .iter()
        .map(|(entity, public_id, _)| (entity.clone(), public_id.clone()))
        .collect();
    for (entity, public_id, deleted_at) in &remote_snap.tombstones {
        if apply_tombstone(local, entity, public_id, deleted_at)? {
            pulled += 1;
        }
    }
    for feature in &remote_snap.features {
        upsert_local_feature(local, feature)?;
    }
    for project in &remote_snap.projects {
        upsert_local_project(local, project)?;
    }
    let dedup_notes = fold_idempotency_duplicates(local, remote_snap)?;
    let mut adopted = HashSet::new();
    for task in &remote_snap.tasks {
        if tombstones.contains(&("task".to_string(), task.public_id.clone())) {
            continue;
        }
        let local_id = lookup_local_task(local, &task.public_id)?;
        if local_id.is_none() {
            insert_local_task(local, task)?;
            adopted.insert(task.public_id.clone());
            pulled += 1;
            continue;
        }
        let local_side = local_side(local, &task.public_id)?;
        let remote_side = side_for(task, remote_snap);
        match choose_merge(&local_side, &remote_side) {
            MergeChoice::KeepLocal => {}
            MergeChoice::AdoptRemote => {
                adopt_existing(local, task)?;
                adopted.insert(task.public_id.clone());
                pulled += 1;
            }
        }
    }
    for public_id in &adopted {
        replace_local_deps(local, public_id, remote_snap)?;
        copy_claims(local, public_id, remote_snap)?;
    }
    import_events(local, remote_snap)?;
    import_artifacts(local, remote_snap)?;
    let mut deduped = 0u64;
    for note in &dedup_notes {
        if record_dedup_event(local, note)? {
            deduped += 1;
        }
    }
    let (pushed, push_deduped) = push_dirty(local, remote, remote_snap, &mut conflicts)?;
    deduped += push_deduped;
    push_tombstones(local, remote)?;
    Ok(SyncReport {
        link: LinkState::Online,
        pulled,
        pushed,
        conflicts,
        deduped,
    })
}

struct DedupNote {
    kept_public_id: String,
    discarded_public_id: String,
    idempotency_key: String,
    local_status: String,
    local_claim_token: Option<String>,
}

fn fold_idempotency_duplicates(
    local: &mut dyn Db,
    remote_snap: &Snapshot,
) -> Result<Vec<DedupNote>, QueueError> {
    let mut notes = Vec::new();
    let now = format_timestamp(OffsetDateTime::now_utc());
    for remote_task in &remote_snap.tasks {
        let Some(key) = remote_task
            .idempotency_key
            .as_deref()
            .filter(|key| !key.is_empty())
        else {
            continue;
        };
        let rows = local.query(
            "SELECT public_id FROM tasks WHERE idempotency_key = ? AND public_id != ?",
            &[SqlVal::text(key), SqlVal::text(&remote_task.public_id)],
        )?;
        for row in rows {
            let discarded = req_text(&row, 0)?;
            let side = local_side(local, &discarded)?;
            discard_local_task(local, &discarded, &now)?;
            notes.push(DedupNote {
                kept_public_id: remote_task.public_id.clone(),
                discarded_public_id: discarded,
                idempotency_key: key.to_string(),
                local_status: side.status,
                local_claim_token: side.latest_claim_token,
            });
        }
    }
    Ok(notes)
}

fn discard_local_task(local: &mut dyn Db, public_id: &str, now: &str) -> Result<(), QueueError> {
    local.execute(
        "INSERT INTO sync_tombstones (entity, public_id, deleted_at) VALUES ('task', ?, ?)
         ON CONFLICT(entity, public_id) DO UPDATE SET deleted_at = excluded.deleted_at",
        &[SqlVal::text(public_id), SqlVal::text(now)],
    )?;
    local.execute(
        "DELETE FROM tasks WHERE public_id = ?",
        &[SqlVal::text(public_id)],
    )?;
    Ok(())
}

fn record_dedup_event(local: &mut dyn Db, note: &DedupNote) -> Result<bool, QueueError> {
    let Some(id) = lookup_local_task(local, &note.kept_public_id)? else {
        return Ok(false);
    };
    let now = format_timestamp(OffsetDateTime::now_utc());
    let payload = json!({
        "scope": "sync",
        "resolution": "remote_authority",
        "idempotency_key": note.idempotency_key,
        "kept_public_id": note.kept_public_id,
        "discarded_public_id": note.discarded_public_id,
        "local_status": note.local_status,
        "local_claim_token": note.local_claim_token,
    });
    local.execute(
        "INSERT INTO events (
            task_id, event_type, actor_type, actor_id, payload_json, public_id, dirty, created_at
         ) VALUES (?, 'task_deduped', 'system', 'q', ?, ?, 1, ?)",
        &[
            SqlVal::Int(id),
            SqlVal::text(payload.to_string()),
            SqlVal::text(Uuid::now_v7().to_string()),
            SqlVal::text(now),
        ],
    )?;
    local.execute(
        "UPDATE tasks SET dirty = 1 WHERE id = ?",
        &[SqlVal::Int(id)],
    )?;
    Ok(true)
}

fn conflicting_idempotency_owner(
    remote: &mut dyn Db,
    task: &TaskRec,
) -> Result<Option<Box<DedupPull>>, QueueError> {
    let Some(key) = task
        .idempotency_key
        .as_deref()
        .filter(|key| !key.is_empty())
    else {
        return Ok(None);
    };
    let Some(other) = load_task_by_idempotency(remote, key)? else {
        return Ok(None);
    };
    if other.public_id == task.public_id {
        return Ok(None);
    }
    let claims = load_remote_claims(remote, &other.public_id)?;
    Ok(Some(Box::new(DedupPull {
        task: other,
        claims,
    })))
}

fn side_for(task: &TaskRec, snap: &Snapshot) -> ClaimSide {
    ClaimSide {
        status: task.status.clone(),
        updated_at: task.updated_at.clone(),
        latest_claim_token: latest_token(&snap.claims, &task.public_id),
        dirty: false,
    }
}

fn latest_token(claims: &[ClaimRec], public_id: &str) -> Option<String> {
    claims
        .iter()
        .filter(|claim| claim.task_public_id == public_id)
        .max_by_key(|claim| claim.id)
        .map(|claim| claim.claim_token.clone())
}

fn local_side(local: &mut dyn Db, public_id: &str) -> Result<ClaimSide, QueueError> {
    let row = one(
        local,
        "SELECT status, updated_at, dirty,
            (SELECT claim_token FROM claims WHERE task_id = tasks.id ORDER BY id DESC LIMIT 1)
         FROM tasks WHERE public_id = ?",
        &[SqlVal::text(public_id)],
    )?
    .ok_or_else(|| QueueError::Database(format!("missing local task {public_id}")))?;
    Ok(ClaimSide {
        status: req_text(&row, 0)?,
        updated_at: req_text(&row, 1)?,
        dirty: req_int(&row, 2)? != 0,
        latest_claim_token: opt_text(&row, 3)?,
    })
}

fn apply_tombstone(
    local: &mut dyn Db,
    entity: &str,
    public_id: &str,
    deleted_at: &str,
) -> Result<bool, QueueError> {
    local.execute(
        "INSERT INTO sync_tombstones (entity, public_id, deleted_at) VALUES (?, ?, ?)
         ON CONFLICT(entity, public_id) DO UPDATE SET deleted_at = excluded.deleted_at",
        &[
            SqlVal::text(entity),
            SqlVal::text(public_id),
            SqlVal::text(deleted_at),
        ],
    )?;
    let removed = match entity {
        "task" => local.execute(
            "DELETE FROM tasks WHERE public_id = ?",
            &[SqlVal::text(public_id)],
        )?,
        "feature" => {
            local.execute(
                "UPDATE tasks SET feature_id = NULL WHERE feature_id IN (SELECT id FROM features WHERE public_id = ?)",
                &[SqlVal::text(public_id)],
            )?;
            local.execute(
                "DELETE FROM features WHERE public_id = ?",
                &[SqlVal::text(public_id)],
            )?
        }
        _ => 0,
    };
    Ok(removed > 0)
}

fn adopt_existing(local: &mut dyn Db, task: &TaskRec) -> Result<(), QueueError> {
    let id = lookup_local_task(local, &task.public_id)?.ok_or_else(|| {
        QueueError::Database(format!("task {} disappeared during adopt", task.public_id))
    })?;
    let feature_id = match &task.feature_public_id {
        Some(public_id) => lookup_local_feature(local, public_id)?,
        None => None,
    };
    let project_id = match &task.project_name {
        Some(name) => lookup_local_project(local, name)?,
        None => None,
    };
    local.execute(
        "UPDATE tasks SET
            title = ?, body = ?, original_capture = ?, status = ?, kind = ?, priority = ?,
            risk = ?, project_id = ?, project_name = ?, repo = ?, capture_path = ?,
            repo_relative_path = ?, git_root = ?, git_head = ?, agent_pool = ?,
            required_capabilities_json = ?, blocked_reason = ?, feature_id = ?,
            created_at = ?, updated_at = ?, idempotency_key = ?, dirty = 0
         WHERE id = ?",
        &[
            SqlVal::text(&task.title),
            SqlVal::opt_text(task.body.clone()),
            SqlVal::text(&task.original_capture),
            SqlVal::text(&task.status),
            SqlVal::text(&task.kind),
            SqlVal::Int(task.priority),
            SqlVal::text(&task.risk),
            opt_int(project_id),
            SqlVal::opt_text(task.project_name.clone()),
            SqlVal::opt_text(task.repo.clone()),
            SqlVal::text(&task.capture_path),
            SqlVal::opt_text(task.repo_relative_path.clone()),
            SqlVal::opt_text(task.git_root.clone()),
            SqlVal::opt_text(task.git_head.clone()),
            SqlVal::opt_text(task.agent_pool.clone()),
            SqlVal::text(&task.required_capabilities_json),
            SqlVal::opt_text(task.blocked_reason.clone()),
            opt_int(feature_id),
            SqlVal::text(&task.created_at),
            SqlVal::text(&task.updated_at),
            SqlVal::opt_text(task.idempotency_key.clone()),
            SqlVal::Int(id),
        ],
    )?;
    Ok(())
}

fn copy_claims(local: &mut dyn Db, public_id: &str, snap: &Snapshot) -> Result<(), QueueError> {
    let Some(task_id) = lookup_local_task(local, public_id)? else {
        return Ok(());
    };
    // One unreleased claim per task. Release any local active claim the
    // authority does not still hold before inserting the remote token.
    if let Some(active) = snap
        .claims
        .iter()
        .filter(|claim| claim.task_public_id == public_id && claim.released_at.is_none())
        .max_by_key(|claim| claim.id)
    {
        local.execute(
            "UPDATE claims SET dirty = 0,
                released_at = COALESCE(released_at, ?),
                release_reason = CASE WHEN released_at IS NULL THEN 'superseded' ELSE release_reason END
             WHERE task_id = ? AND released_at IS NULL AND claim_token != ?",
            &[
                SqlVal::text(&active.claimed_at),
                SqlVal::Int(task_id),
                SqlVal::text(&active.claim_token),
            ],
        )?;
    }
    for claim in snap
        .claims
        .iter()
        .filter(|claim| claim.task_public_id == public_id)
    {
        let exists = one(
            local,
            "SELECT id FROM claims WHERE claim_token = ?",
            &[SqlVal::text(&claim.claim_token)],
        )?;
        if exists.is_some() {
            local.execute(
                "UPDATE claims SET
                    agent_id = ?, claimed_at = ?, heartbeat_at = ?, lease_expires_at = ?,
                    branch = ?, worktree_path = ?, released_at = ?, release_reason = ?, dirty = 0
                 WHERE claim_token = ?",
                &[
                    SqlVal::text(&claim.agent_id),
                    SqlVal::text(&claim.claimed_at),
                    SqlVal::text(&claim.heartbeat_at),
                    SqlVal::text(&claim.lease_expires_at),
                    SqlVal::opt_text(claim.branch.clone()),
                    SqlVal::opt_text(claim.worktree_path.clone()),
                    SqlVal::opt_text(claim.released_at.clone()),
                    SqlVal::opt_text(claim.release_reason.clone()),
                    SqlVal::text(&claim.claim_token),
                ],
            )?;
        } else {
            local.execute(
                "INSERT INTO claims (
                    task_id, agent_id, claim_token, claimed_at, heartbeat_at, lease_expires_at,
                    branch, worktree_path, released_at, release_reason, dirty
                 ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 0)",
                &claim_insert_params(claim, task_id),
            )?;
        }
    }
    Ok(())
}

fn claim_insert_params(claim: &ClaimRec, task_id: i64) -> Vec<SqlVal> {
    vec![
        SqlVal::Int(task_id),
        SqlVal::text(&claim.agent_id),
        SqlVal::text(&claim.claim_token),
        SqlVal::text(&claim.claimed_at),
        SqlVal::text(&claim.heartbeat_at),
        SqlVal::text(&claim.lease_expires_at),
        SqlVal::opt_text(claim.branch.clone()),
        SqlVal::opt_text(claim.worktree_path.clone()),
        SqlVal::opt_text(claim.released_at.clone()),
        SqlVal::opt_text(claim.release_reason.clone()),
    ]
}

fn replace_local_deps(
    local: &mut dyn Db,
    public_id: &str,
    snap: &Snapshot,
) -> Result<(), QueueError> {
    let Some(task_id) = lookup_local_task(local, public_id)? else {
        return Ok(());
    };
    local.execute(
        "DELETE FROM task_dependencies WHERE task_id = ?",
        &[SqlVal::Int(task_id)],
    )?;
    for (task_public, depends_on) in &snap.deps {
        if task_public != public_id {
            continue;
        }
        let Some(dep_id) = lookup_local_task(local, depends_on)? else {
            continue;
        };
        local.execute(
            "INSERT OR IGNORE INTO task_dependencies (task_id, depends_on_task_id) VALUES (?, ?)",
            &[SqlVal::Int(task_id), SqlVal::Int(dep_id)],
        )?;
    }
    Ok(())
}

fn import_events(local: &mut dyn Db, snap: &Snapshot) -> Result<(), QueueError> {
    for event in &snap.events {
        let Some(task_id) = lookup_local_task(local, &event.task_public_id)? else {
            continue;
        };
        local.execute(
            "INSERT OR IGNORE INTO events (
                task_id, event_type, actor_type, actor_id, payload_json, public_id, dirty, created_at
             ) VALUES (?, ?, ?, ?, ?, ?, 0, ?)",
            &[
                SqlVal::Int(task_id),
                SqlVal::text(&event.event_type),
                SqlVal::text(&event.actor_type),
                SqlVal::opt_text(event.actor_id.clone()),
                SqlVal::text(&event.payload_json),
                SqlVal::text(&event.public_id),
                SqlVal::text(&event.created_at),
            ],
        )?;
    }
    Ok(())
}

fn import_artifacts(local: &mut dyn Db, snap: &Snapshot) -> Result<(), QueueError> {
    for artifact in &snap.artifacts {
        let Some(task_id) = lookup_local_task(local, &artifact.task_public_id)? else {
            continue;
        };
        local.execute(
            "INSERT OR IGNORE INTO artifacts (task_id, kind, value, public_id, dirty, created_at)
             VALUES (?, ?, ?, ?, 0, ?)",
            &[
                SqlVal::Int(task_id),
                SqlVal::text(&artifact.kind),
                SqlVal::text(&artifact.value),
                SqlVal::text(&artifact.public_id),
                SqlVal::text(&artifact.created_at),
            ],
        )?;
    }
    Ok(())
}

fn push_dirty(
    local: &mut dyn Db,
    remote: &mut dyn Db,
    snap: &Snapshot,
    conflicts: &mut u64,
) -> Result<(u64, u64), QueueError> {
    let rows = local.query(
        "SELECT public_id FROM tasks
         WHERE dirty = 1
           AND public_id NOT IN (SELECT public_id FROM sync_tombstones WHERE entity = 'task')
         ORDER BY id ASC",
        &[],
    )?;
    let mut pushed = 0u64;
    let mut deduped = 0u64;
    for row in rows {
        let public_id = req_text(&row, 0)?;
        match push_one(local, remote, &public_id, snap)? {
            PushResult::Pushed => pushed += 1,
            PushResult::Conflict => {
                *conflicts += 1;
            }
            PushResult::Deduped => deduped += 1,
        }
    }
    Ok((pushed, deduped))
}

enum PushResult {
    Pushed,
    Conflict,
    Deduped,
}

fn push_one(
    local: &mut dyn Db,
    remote: &mut dyn Db,
    public_id: &str,
    snap: &Snapshot,
) -> Result<PushResult, QueueError> {
    let Some(task) = load_local_task(local, public_id)? else {
        return Ok(PushResult::Pushed);
    };
    let project = match &task.project_name {
        Some(name) => load_local_project(local, name)?,
        None => None,
    };
    let feature = match &task.feature_public_id {
        Some(id) => load_local_feature(local, id)?,
        None => None,
    };
    let claims = load_local_claims(local, public_id)?;
    let events = load_local_events(local, public_id)?;
    let artifacts = load_local_artifacts(local, public_id)?;
    let deps = load_local_deps(local, public_id)?;
    remote.begin_immediate()?;
    let outcome = push_one_remote(
        remote,
        &task,
        project.as_ref(),
        feature.as_ref(),
        &claims,
        &events,
        &artifacts,
        &deps,
        snap,
    );
    let outcome = match outcome {
        Ok(RemoteWrite::Accepted) => {
            remote.commit()?;
            RemoteWrite::Accepted
        }
        Ok(rejected) => {
            let _ = remote.rollback();
            rejected
        }
        Err(err) => {
            let _ = remote.rollback();
            return Err(err);
        }
    };
    match outcome {
        RemoteWrite::Accepted => {
            local.execute(
                "UPDATE tasks SET dirty = 0 WHERE public_id = ?",
                &[SqlVal::text(public_id)],
            )?;
            if let Some(id) = lookup_local_task(local, public_id)? {
                local.execute(
                    "UPDATE claims SET dirty = 0 WHERE task_id = ?",
                    &[SqlVal::Int(id)],
                )?;
                local.execute(
                    "UPDATE events SET dirty = 0 WHERE task_id = ?",
                    &[SqlVal::Int(id)],
                )?;
                local.execute(
                    "UPDATE artifacts SET dirty = 0 WHERE task_id = ?",
                    &[SqlVal::Int(id)],
                )?;
            }
            Ok(PushResult::Pushed)
        }
        RemoteWrite::Rejected(fresh) => {
            adopt_existing(local, &fresh.task)?;
            copy_claims(local, &fresh.task.public_id, &fresh.snap)?;
            replace_local_deps(local, &fresh.task.public_id, &fresh.snap)?;
            Ok(PushResult::Conflict)
        }
        RemoteWrite::Deduped(pull) => {
            let side = local_side(local, public_id)?;
            let note = DedupNote {
                kept_public_id: pull.task.public_id.clone(),
                discarded_public_id: public_id.to_string(),
                idempotency_key: pull.task.idempotency_key.clone().unwrap_or_default(),
                local_status: side.status,
                local_claim_token: side.latest_claim_token,
            };
            let now = format_timestamp(OffsetDateTime::now_utc());
            discard_local_task(local, public_id, &now)?;
            if lookup_local_task(local, &pull.task.public_id)?.is_none() {
                insert_local_task(local, &pull.task)?;
            }
            let snap = Snapshot {
                tasks: vec![pull.task.clone()],
                claims: pull.claims.clone(),
                events: Vec::new(),
                artifacts: Vec::new(),
                deps: Vec::new(),
                features: Vec::new(),
                projects: Vec::new(),
                tombstones: Vec::new(),
            };
            copy_claims(local, &pull.task.public_id, &snap)?;
            record_dedup_event(local, &note)?;
            Ok(PushResult::Deduped)
        }
    }
}

struct RejectedPull {
    task: TaskRec,
    snap: Snapshot,
}

enum RemoteWrite {
    Accepted,
    Rejected(Box<RejectedPull>),
    Deduped(Box<DedupPull>),
}

struct DedupPull {
    task: TaskRec,
    claims: Vec<ClaimRec>,
}

#[allow(clippy::too_many_arguments)]
fn push_one_remote(
    remote: &mut dyn Db,
    task: &TaskRec,
    project: Option<&ProjectRec>,
    feature: Option<&FeatureRec>,
    claims: &[ClaimRec],
    events: &[EventRec],
    artifacts: &[ArtifactRec],
    deps: &[String],
    previous: &Snapshot,
) -> Result<RemoteWrite, QueueError> {
    if let Some(project) = project {
        upsert_remote_project(remote, project)?;
    }
    if let Some(feature) = feature {
        upsert_remote_feature(remote, feature)?;
    }
    if let Some(pull) = conflicting_idempotency_owner(remote, task)? {
        return Ok(RemoteWrite::Deduped(pull));
    }
    let existing = load_remote_task(remote, &task.public_id)?;
    if let Some(existing) = existing {
        let remote_claims = load_remote_claims(remote, &task.public_id)?;
        let remote_token = remote_claims
            .iter()
            .max_by_key(|claim| claim.id)
            .map(|claim| claim.claim_token.clone());
        let local_token = claims
            .iter()
            .max_by_key(|claim| claim.id)
            .map(|claim| claim.claim_token.clone());
        // The authority already recorded a different claim (crash after CAS,
        // or a claim this replica never wrote). Do not push over it.
        if remote_token.is_some() && local_token.as_deref() != remote_token.as_deref() {
            let snap = Snapshot {
                tasks: vec![existing.clone()],
                claims: remote_claims,
                events: load_remote_events(remote, &task.public_id)?,
                artifacts: load_remote_artifacts(remote, &task.public_id)?,
                deps: load_remote_deps(remote, &task.public_id)?,
                features: vec![],
                projects: vec![],
                tombstones: vec![],
            };
            let _ = previous;
            return Ok(RemoteWrite::Rejected(Box::new(RejectedPull {
                task: existing,
                snap,
            })));
        }
        if existing.status != task.status {
            let updated = remote.execute(
                "UPDATE tasks SET status = ? WHERE public_id = ? AND status = ?",
                &[
                    SqlVal::text(&task.status),
                    SqlVal::text(&task.public_id),
                    SqlVal::text(&existing.status),
                ],
            )?;
            if updated == 0 {
                let fresh = load_remote_task(remote, &task.public_id)?.unwrap_or(existing);
                let snap = Snapshot {
                    tasks: vec![fresh.clone()],
                    claims: load_remote_claims(remote, &task.public_id)?,
                    events: load_remote_events(remote, &task.public_id)?,
                    artifacts: load_remote_artifacts(remote, &task.public_id)?,
                    deps: load_remote_deps(remote, &task.public_id)?,
                    features: vec![],
                    projects: vec![],
                    tombstones: vec![],
                };
                return Ok(RemoteWrite::Rejected(Box::new(RejectedPull {
                    task: fresh,
                    snap,
                })));
            }
        }
        write_task_fields(remote, task)?;
    } else {
        insert_remote_task(remote, task)?;
    }
    for claim in claims {
        upsert_remote_claim(remote, claim, &task.public_id)?;
    }
    for event in events {
        remote.execute(
            "INSERT OR IGNORE INTO events (
                task_id, event_type, actor_type, actor_id, payload_json, public_id, dirty, created_at
             ) VALUES (
                (SELECT id FROM tasks WHERE public_id = ?), ?, ?, ?, ?, ?, 0, ?
             )",
            &[
                SqlVal::text(&task.public_id),
                SqlVal::text(&event.event_type),
                SqlVal::text(&event.actor_type),
                SqlVal::opt_text(event.actor_id.clone()),
                SqlVal::text(&event.payload_json),
                SqlVal::text(&event.public_id),
                SqlVal::text(&event.created_at),
            ],
        )?;
    }
    for artifact in artifacts {
        remote.execute(
            "INSERT OR IGNORE INTO artifacts (task_id, kind, value, public_id, dirty, created_at)
             VALUES ((SELECT id FROM tasks WHERE public_id = ?), ?, ?, ?, 0, ?)",
            &[
                SqlVal::text(&task.public_id),
                SqlVal::text(&artifact.kind),
                SqlVal::text(&artifact.value),
                SqlVal::text(&artifact.public_id),
                SqlVal::text(&artifact.created_at),
            ],
        )?;
    }
    replace_remote_deps(remote, &task.public_id, deps)?;
    Ok(RemoteWrite::Accepted)
}

fn publish_bundle(remote: &mut dyn Db, bundle: &PublishBundle) -> Result<bool, QueueError> {
    remote.begin_immediate()?;
    let result = publish_in_tx(remote, bundle);
    match result {
        Ok(accepted) => {
            remote.commit()?;
            Ok(accepted)
        }
        Err(err) => {
            let _ = remote.rollback();
            Err(err)
        }
    }
}

fn publish_in_tx(remote: &mut dyn Db, bundle: &PublishBundle) -> Result<bool, QueueError> {
    if let Some(project) = &bundle.project {
        upsert_remote_project(remote, project)?;
    }
    if let Some(feature) = &bundle.feature {
        upsert_remote_feature(remote, feature)?;
    }
    let existing = load_remote_task(remote, &bundle.task.public_id)?;
    let Some(existing) = existing else {
        let mut task = bundle.task.clone();
        task.status = "claimed".into();
        insert_remote_task(remote, &task)?;
        upsert_remote_claim(remote, &bundle.claim, &task.public_id)?;
        return Ok(true);
    };
    let remote_token = load_remote_claims(remote, &bundle.task.public_id)?
        .into_iter()
        .filter(|claim| claim.released_at.is_none())
        .max_by_key(|claim| claim.id)
        .map(|claim| claim.claim_token);
    if remote_token.is_some() && remote_token.as_deref() != Some(bundle.claim.claim_token.as_str())
    {
        return Ok(false);
    }
    if !matches!(existing.status.as_str(), "ready" | "inbox" | "claimed") {
        return Ok(false);
    }
    if existing.status != "claimed" {
        let updated = remote.execute(
            "UPDATE tasks SET status = 'claimed', updated_at = ?, dirty = 0 WHERE public_id = ? AND status = ?",
            &[
                SqlVal::text(&bundle.claim.claimed_at),
                SqlVal::text(&bundle.task.public_id),
                SqlVal::text(&existing.status),
            ],
        )?;
        if updated == 0 {
            return Ok(false);
        }
    }
    let mut task = bundle.task.clone();
    task.status = "claimed".into();
    task.updated_at = bundle.claim.claimed_at.clone();
    write_task_fields(remote, &task)?;
    upsert_remote_claim(remote, &bundle.claim, &task.public_id)?;
    Ok(true)
}

fn push_tombstones(local: &mut dyn Db, remote: &mut dyn Db) -> Result<(), QueueError> {
    let rows = local.query(
        "SELECT entity, public_id, deleted_at FROM sync_tombstones ORDER BY deleted_at, public_id",
        &[],
    )?;
    if rows.is_empty() {
        return Ok(());
    }
    remote.begin_immediate()?;
    let result = (|| {
        for row in &rows {
            let entity = req_text(row, 0)?;
            let public_id = req_text(row, 1)?;
            let deleted_at = req_text(row, 2)?;
            remote.execute(
                "INSERT INTO sync_tombstones (entity, public_id, deleted_at) VALUES (?, ?, ?)
                 ON CONFLICT(entity, public_id) DO UPDATE SET deleted_at = excluded.deleted_at",
                &[
                    SqlVal::text(&entity),
                    SqlVal::text(&public_id),
                    SqlVal::text(&deleted_at),
                ],
            )?;
            match entity.as_str() {
                "task" => {
                    remote.execute(
                        "DELETE FROM tasks WHERE public_id = ?",
                        &[SqlVal::text(&public_id)],
                    )?;
                }
                "feature" => {
                    remote.execute(
                        "UPDATE tasks SET feature_id = NULL WHERE feature_id IN (SELECT id FROM features WHERE public_id = ?)",
                        &[SqlVal::text(&public_id)],
                    )?;
                    remote.execute(
                        "DELETE FROM features WHERE public_id = ?",
                        &[SqlVal::text(&public_id)],
                    )?;
                }
                _ => {}
            }
        }
        Ok(())
    })();
    match result {
        Ok(()) => remote.commit(),
        Err(err) => {
            let _ = remote.rollback();
            Err(err)
        }
    }
}

fn load_snapshot(db: &mut dyn Db) -> Result<Snapshot, QueueError> {
    Ok(Snapshot {
        tasks: load_tasks(db)?,
        claims: load_claims(db)?,
        events: load_events(db)?,
        artifacts: load_artifacts(db)?,
        deps: load_deps(db)?,
        features: load_features(db)?,
        projects: load_projects(db)?,
        tombstones: load_tombstones(db)?,
    })
}

fn load_tasks(db: &mut dyn Db) -> Result<Vec<TaskRec>, QueueError> {
    let rows = db.query(
        "SELECT tasks.public_id, tasks.title, tasks.body, tasks.original_capture, tasks.status,
            tasks.kind, tasks.priority, tasks.risk, tasks.project_name, tasks.repo,
            tasks.capture_path, tasks.repo_relative_path, tasks.git_root, tasks.git_head,
            tasks.agent_pool, tasks.required_capabilities_json, tasks.blocked_reason,
            features.public_id, tasks.created_at, tasks.updated_at, tasks.idempotency_key
         FROM tasks
         LEFT JOIN features ON features.id = tasks.feature_id
         ORDER BY tasks.id",
        &[],
    )?;
    rows.iter().map(|row| parse_task(row)).collect()
}

fn parse_task(row: &[SqlVal]) -> Result<TaskRec, QueueError> {
    Ok(TaskRec {
        public_id: req_text(row, 0)?,
        title: req_text(row, 1)?,
        body: opt_text(row, 2)?,
        original_capture: req_text(row, 3)?,
        status: req_text(row, 4)?,
        kind: req_text(row, 5)?,
        priority: req_int(row, 6)?,
        risk: req_text(row, 7)?,
        project_name: opt_text(row, 8)?,
        repo: opt_text(row, 9)?,
        capture_path: req_text(row, 10)?,
        repo_relative_path: opt_text(row, 11)?,
        git_root: opt_text(row, 12)?,
        git_head: opt_text(row, 13)?,
        agent_pool: opt_text(row, 14)?,
        required_capabilities_json: req_text(row, 15)?,
        blocked_reason: opt_text(row, 16)?,
        feature_public_id: opt_text(row, 17)?,
        created_at: req_text(row, 18)?,
        updated_at: req_text(row, 19)?,
        idempotency_key: opt_text(row, 20)?,
    })
}

fn load_local_task(local: &mut dyn Db, public_id: &str) -> Result<Option<TaskRec>, QueueError> {
    let rows = local.query(
        "SELECT tasks.public_id, tasks.title, tasks.body, tasks.original_capture, tasks.status,
            tasks.kind, tasks.priority, tasks.risk, tasks.project_name, tasks.repo,
            tasks.capture_path, tasks.repo_relative_path, tasks.git_root, tasks.git_head,
            tasks.agent_pool, tasks.required_capabilities_json, tasks.blocked_reason,
            features.public_id, tasks.created_at, tasks.updated_at, tasks.idempotency_key
         FROM tasks
         LEFT JOIN features ON features.id = tasks.feature_id
         WHERE tasks.public_id = ?",
        &[SqlVal::text(public_id)],
    )?;
    rows.first().map(|row| parse_task(row)).transpose()
}

fn load_remote_task(remote: &mut dyn Db, public_id: &str) -> Result<Option<TaskRec>, QueueError> {
    load_local_task(remote, public_id)
}

fn load_task_by_idempotency(db: &mut dyn Db, key: &str) -> Result<Option<TaskRec>, QueueError> {
    let rows = db.query(
        "SELECT tasks.public_id, tasks.title, tasks.body, tasks.original_capture, tasks.status,
            tasks.kind, tasks.priority, tasks.risk, tasks.project_name, tasks.repo,
            tasks.capture_path, tasks.repo_relative_path, tasks.git_root, tasks.git_head,
            tasks.agent_pool, tasks.required_capabilities_json, tasks.blocked_reason,
            features.public_id, tasks.created_at, tasks.updated_at, tasks.idempotency_key
         FROM tasks
         LEFT JOIN features ON features.id = tasks.feature_id
         WHERE tasks.idempotency_key = ?",
        &[SqlVal::text(key)],
    )?;
    rows.first().map(|row| parse_task(row)).transpose()
}

fn load_claims(db: &mut dyn Db) -> Result<Vec<ClaimRec>, QueueError> {
    let rows = db.query(
        "SELECT claims.id, tasks.public_id, claims.agent_id, claims.claim_token, claims.claimed_at,
            claims.heartbeat_at, claims.lease_expires_at, claims.branch, claims.worktree_path,
            claims.released_at, claims.release_reason
         FROM claims JOIN tasks ON tasks.id = claims.task_id
         ORDER BY claims.id",
        &[],
    )?;
    rows.iter().map(|row| parse_claim(row)).collect()
}

fn parse_claim(row: &[SqlVal]) -> Result<ClaimRec, QueueError> {
    Ok(ClaimRec {
        id: req_int(row, 0)?,
        task_public_id: req_text(row, 1)?,
        agent_id: req_text(row, 2)?,
        claim_token: req_text(row, 3)?,
        claimed_at: req_text(row, 4)?,
        heartbeat_at: req_text(row, 5)?,
        lease_expires_at: req_text(row, 6)?,
        branch: opt_text(row, 7)?,
        worktree_path: opt_text(row, 8)?,
        released_at: opt_text(row, 9)?,
        release_reason: opt_text(row, 10)?,
    })
}

fn load_local_claims(local: &mut dyn Db, public_id: &str) -> Result<Vec<ClaimRec>, QueueError> {
    let rows = local.query(
        "SELECT claims.id, tasks.public_id, claims.agent_id, claims.claim_token, claims.claimed_at,
            claims.heartbeat_at, claims.lease_expires_at, claims.branch, claims.worktree_path,
            claims.released_at, claims.release_reason
         FROM claims JOIN tasks ON tasks.id = claims.task_id
         WHERE tasks.public_id = ?
         ORDER BY claims.id",
        &[SqlVal::text(public_id)],
    )?;
    rows.iter().map(|row| parse_claim(row)).collect()
}

fn load_remote_claims(remote: &mut dyn Db, public_id: &str) -> Result<Vec<ClaimRec>, QueueError> {
    load_local_claims(remote, public_id)
}

fn load_events(db: &mut dyn Db) -> Result<Vec<EventRec>, QueueError> {
    let rows = db.query(
        "SELECT events.public_id, tasks.public_id, events.event_type, events.actor_type,
            events.actor_id, events.payload_json, events.created_at
         FROM events JOIN tasks ON tasks.id = events.task_id
         WHERE events.public_id IS NOT NULL
         ORDER BY events.id",
        &[],
    )?;
    rows.iter().map(|row| parse_event(row)).collect()
}

fn parse_event(row: &[SqlVal]) -> Result<EventRec, QueueError> {
    Ok(EventRec {
        public_id: req_text(row, 0)?,
        task_public_id: req_text(row, 1)?,
        event_type: req_text(row, 2)?,
        actor_type: req_text(row, 3)?,
        actor_id: opt_text(row, 4)?,
        payload_json: req_text(row, 5)?,
        created_at: req_text(row, 6)?,
    })
}

fn load_local_events(local: &mut dyn Db, public_id: &str) -> Result<Vec<EventRec>, QueueError> {
    let rows = local.query(
        "SELECT events.public_id, tasks.public_id, events.event_type, events.actor_type,
            events.actor_id, events.payload_json, events.created_at
         FROM events JOIN tasks ON tasks.id = events.task_id
         WHERE tasks.public_id = ? AND events.public_id IS NOT NULL AND events.dirty = 1
         ORDER BY events.id",
        &[SqlVal::text(public_id)],
    )?;
    rows.iter().map(|row| parse_event(row)).collect()
}

fn load_remote_events(remote: &mut dyn Db, public_id: &str) -> Result<Vec<EventRec>, QueueError> {
    let rows = remote.query(
        "SELECT events.public_id, tasks.public_id, events.event_type, events.actor_type,
            events.actor_id, events.payload_json, events.created_at
         FROM events JOIN tasks ON tasks.id = events.task_id
         WHERE tasks.public_id = ? AND events.public_id IS NOT NULL
         ORDER BY events.id",
        &[SqlVal::text(public_id)],
    )?;
    rows.iter().map(|row| parse_event(row)).collect()
}

fn load_artifacts(db: &mut dyn Db) -> Result<Vec<ArtifactRec>, QueueError> {
    let rows = db.query(
        "SELECT artifacts.public_id, tasks.public_id, artifacts.kind, artifacts.value, artifacts.created_at
         FROM artifacts JOIN tasks ON tasks.id = artifacts.task_id
         WHERE artifacts.public_id IS NOT NULL
         ORDER BY artifacts.id",
        &[],
    )?;
    rows.iter().map(|row| parse_artifact(row)).collect()
}

fn parse_artifact(row: &[SqlVal]) -> Result<ArtifactRec, QueueError> {
    Ok(ArtifactRec {
        public_id: req_text(row, 0)?,
        task_public_id: req_text(row, 1)?,
        kind: req_text(row, 2)?,
        value: req_text(row, 3)?,
        created_at: req_text(row, 4)?,
    })
}

fn load_local_artifacts(
    local: &mut dyn Db,
    public_id: &str,
) -> Result<Vec<ArtifactRec>, QueueError> {
    let rows = local.query(
        "SELECT artifacts.public_id, tasks.public_id, artifacts.kind, artifacts.value, artifacts.created_at
         FROM artifacts JOIN tasks ON tasks.id = artifacts.task_id
         WHERE tasks.public_id = ? AND artifacts.public_id IS NOT NULL AND artifacts.dirty = 1",
        &[SqlVal::text(public_id)],
    )?;
    rows.iter().map(|row| parse_artifact(row)).collect()
}

fn load_remote_artifacts(
    remote: &mut dyn Db,
    public_id: &str,
) -> Result<Vec<ArtifactRec>, QueueError> {
    let rows = remote.query(
        "SELECT artifacts.public_id, tasks.public_id, artifacts.kind, artifacts.value, artifacts.created_at
         FROM artifacts JOIN tasks ON tasks.id = artifacts.task_id
         WHERE tasks.public_id = ? AND artifacts.public_id IS NOT NULL",
        &[SqlVal::text(public_id)],
    )?;
    rows.iter().map(|row| parse_artifact(row)).collect()
}

fn load_deps(db: &mut dyn Db) -> Result<Vec<(String, String)>, QueueError> {
    let rows = db.query(
        "SELECT parent.public_id, child.public_id
         FROM task_dependencies
         JOIN tasks parent ON parent.id = task_dependencies.task_id
         JOIN tasks child ON child.id = task_dependencies.depends_on_task_id",
        &[],
    )?;
    let mut deps = Vec::new();
    for row in rows {
        deps.push((req_text(&row, 0)?, req_text(&row, 1)?));
    }
    Ok(deps)
}

fn load_local_deps(local: &mut dyn Db, public_id: &str) -> Result<Vec<String>, QueueError> {
    let rows = local.query(
        "SELECT child.public_id
         FROM task_dependencies
         JOIN tasks parent ON parent.id = task_dependencies.task_id
         JOIN tasks child ON child.id = task_dependencies.depends_on_task_id
         WHERE parent.public_id = ?",
        &[SqlVal::text(public_id)],
    )?;
    rows.iter().map(|row| req_text(row, 0)).collect()
}

fn load_remote_deps(
    remote: &mut dyn Db,
    public_id: &str,
) -> Result<Vec<(String, String)>, QueueError> {
    let deps = load_local_deps(remote, public_id)?;
    Ok(deps
        .into_iter()
        .map(|dep| (public_id.to_string(), dep))
        .collect())
}

fn load_features(db: &mut dyn Db) -> Result<Vec<FeatureRec>, QueueError> {
    let rows = db.query(
        "SELECT public_id, title, body, created_at, updated_at FROM features ORDER BY id",
        &[],
    )?;
    rows.iter()
        .map(|row| {
            Ok(FeatureRec {
                public_id: req_text(row, 0)?,
                title: req_text(row, 1)?,
                body: opt_text(row, 2)?,
                created_at: req_text(row, 3)?,
                updated_at: req_text(row, 4)?,
            })
        })
        .collect()
}

fn load_local_feature(
    local: &mut dyn Db,
    public_id: &str,
) -> Result<Option<FeatureRec>, QueueError> {
    let rows = local.query(
        "SELECT public_id, title, body, created_at, updated_at FROM features WHERE public_id = ?",
        &[SqlVal::text(public_id)],
    )?;
    rows.first()
        .map(|row| {
            Ok(FeatureRec {
                public_id: req_text(row, 0)?,
                title: req_text(row, 1)?,
                body: opt_text(row, 2)?,
                created_at: req_text(row, 3)?,
                updated_at: req_text(row, 4)?,
            })
        })
        .transpose()
}

fn load_projects(db: &mut dyn Db) -> Result<Vec<ProjectRec>, QueueError> {
    let rows = db.query(
        "SELECT name, repo, config_path, max_parallel_jobs, require_pr, allow_external_actions,
            stale_disposition, created_at, updated_at
         FROM projects ORDER BY id",
        &[],
    )?;
    rows.iter().map(|row| parse_project(row)).collect()
}

fn parse_project(row: &[SqlVal]) -> Result<ProjectRec, QueueError> {
    Ok(ProjectRec {
        name: req_text(row, 0)?,
        repo: opt_text(row, 1)?,
        config_path: opt_text(row, 2)?,
        max_parallel_jobs: opt_int_col(row, 3)?,
        require_pr: req_int(row, 4)?,
        allow_external_actions: req_int(row, 5)?,
        stale_disposition: req_text(row, 6)?,
        created_at: req_text(row, 7)?,
        updated_at: req_text(row, 8)?,
    })
}

fn load_local_project(local: &mut dyn Db, name: &str) -> Result<Option<ProjectRec>, QueueError> {
    let rows = local.query(
        "SELECT name, repo, config_path, max_parallel_jobs, require_pr, allow_external_actions,
            stale_disposition, created_at, updated_at
         FROM projects WHERE name = ?",
        &[SqlVal::text(name)],
    )?;
    rows.first().map(|row| parse_project(row)).transpose()
}

fn load_tombstones(db: &mut dyn Db) -> Result<Vec<(String, String, String)>, QueueError> {
    let rows = db.query(
        "SELECT entity, public_id, deleted_at FROM sync_tombstones",
        &[],
    )?;
    let mut out = Vec::new();
    for row in rows {
        out.push((req_text(&row, 0)?, req_text(&row, 1)?, req_text(&row, 2)?));
    }
    Ok(out)
}

fn insert_local_task(local: &mut dyn Db, task: &TaskRec) -> Result<i64, QueueError> {
    let feature_id = match &task.feature_public_id {
        Some(public_id) => lookup_local_feature(local, public_id)?,
        None => None,
    };
    let project_id = match &task.project_name {
        Some(name) => lookup_local_project(local, name)?,
        None => None,
    };
    local.execute(
        "INSERT INTO tasks (
            public_id, title, body, original_capture, status, kind, priority, risk,
            project_id, project_name, repo, capture_path, repo_relative_path, git_root, git_head,
            agent_pool, required_capabilities_json, blocked_reason, feature_id,
            idempotency_key, dirty, created_at, updated_at
         ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 0, ?, ?)",
        &task_insert_params(task, project_id, feature_id),
    )?;
    Ok(local.last_insert_rowid())
}

fn insert_remote_task(remote: &mut dyn Db, task: &TaskRec) -> Result<(), QueueError> {
    let feature_id = match &task.feature_public_id {
        Some(public_id) => lookup_local_feature(remote, public_id)?,
        None => None,
    };
    let project_id = match &task.project_name {
        Some(name) => lookup_local_project(remote, name)?,
        None => None,
    };
    remote.execute(
        "INSERT INTO tasks (
            public_id, title, body, original_capture, status, kind, priority, risk,
            project_id, project_name, repo, capture_path, repo_relative_path, git_root, git_head,
            agent_pool, required_capabilities_json, blocked_reason, feature_id,
            idempotency_key, dirty, created_at, updated_at
         ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 0, ?, ?)",
        &task_insert_params(task, project_id, feature_id),
    )?;
    Ok(())
}

fn task_insert_params(
    task: &TaskRec,
    project_id: Option<i64>,
    feature_id: Option<i64>,
) -> Vec<SqlVal> {
    vec![
        SqlVal::text(&task.public_id),
        SqlVal::text(&task.title),
        SqlVal::opt_text(task.body.clone()),
        SqlVal::text(&task.original_capture),
        SqlVal::text(&task.status),
        SqlVal::text(&task.kind),
        SqlVal::Int(task.priority),
        SqlVal::text(&task.risk),
        opt_int(project_id),
        SqlVal::opt_text(task.project_name.clone()),
        SqlVal::opt_text(task.repo.clone()),
        SqlVal::text(&task.capture_path),
        SqlVal::opt_text(task.repo_relative_path.clone()),
        SqlVal::opt_text(task.git_root.clone()),
        SqlVal::opt_text(task.git_head.clone()),
        SqlVal::opt_text(task.agent_pool.clone()),
        SqlVal::text(&task.required_capabilities_json),
        SqlVal::opt_text(task.blocked_reason.clone()),
        opt_int(feature_id),
        SqlVal::opt_text(task.idempotency_key.clone()),
        SqlVal::text(&task.created_at),
        SqlVal::text(&task.updated_at),
    ]
}

fn write_task_fields(remote: &mut dyn Db, task: &TaskRec) -> Result<(), QueueError> {
    let feature_id = match &task.feature_public_id {
        Some(public_id) => lookup_local_feature(remote, public_id)?,
        None => None,
    };
    let project_id = match &task.project_name {
        Some(name) => lookup_local_project(remote, name)?,
        None => None,
    };
    remote.execute(
        "UPDATE tasks SET
            title = ?, body = ?, original_capture = ?, status = ?, kind = ?, priority = ?,
            risk = ?, project_id = ?, project_name = ?, repo = ?, capture_path = ?,
            repo_relative_path = ?, git_root = ?, git_head = ?, agent_pool = ?,
            required_capabilities_json = ?, blocked_reason = ?, feature_id = ?,
            updated_at = ?, idempotency_key = ?, dirty = 0
         WHERE public_id = ?",
        &[
            SqlVal::text(&task.title),
            SqlVal::opt_text(task.body.clone()),
            SqlVal::text(&task.original_capture),
            SqlVal::text(&task.status),
            SqlVal::text(&task.kind),
            SqlVal::Int(task.priority),
            SqlVal::text(&task.risk),
            opt_int(project_id),
            SqlVal::opt_text(task.project_name.clone()),
            SqlVal::opt_text(task.repo.clone()),
            SqlVal::text(&task.capture_path),
            SqlVal::opt_text(task.repo_relative_path.clone()),
            SqlVal::opt_text(task.git_root.clone()),
            SqlVal::opt_text(task.git_head.clone()),
            SqlVal::opt_text(task.agent_pool.clone()),
            SqlVal::text(&task.required_capabilities_json),
            SqlVal::opt_text(task.blocked_reason.clone()),
            opt_int(feature_id),
            SqlVal::text(&task.updated_at),
            SqlVal::opt_text(task.idempotency_key.clone()),
            SqlVal::text(&task.public_id),
        ],
    )?;
    Ok(())
}

fn upsert_remote_claim(
    remote: &mut dyn Db,
    claim: &ClaimRec,
    task_public_id: &str,
) -> Result<(), QueueError> {
    let Some(task_id) = lookup_local_task(remote, task_public_id)? else {
        return Err(QueueError::Database(format!(
            "remote task {task_public_id} missing for claim"
        )));
    };
    let exists = one(
        remote,
        "SELECT id FROM claims WHERE claim_token = ?",
        &[SqlVal::text(&claim.claim_token)],
    )?;
    if exists.is_some() {
        remote.execute(
            "UPDATE claims SET
                heartbeat_at = ?, lease_expires_at = ?, branch = ?, worktree_path = ?,
                released_at = ?, release_reason = ?, dirty = 0
             WHERE claim_token = ?",
            &[
                SqlVal::text(&claim.heartbeat_at),
                SqlVal::text(&claim.lease_expires_at),
                SqlVal::opt_text(claim.branch.clone()),
                SqlVal::opt_text(claim.worktree_path.clone()),
                SqlVal::opt_text(claim.released_at.clone()),
                SqlVal::opt_text(claim.release_reason.clone()),
                SqlVal::text(&claim.claim_token),
            ],
        )?;
    } else {
        remote.execute(
            "INSERT INTO claims (
                task_id, agent_id, claim_token, claimed_at, heartbeat_at, lease_expires_at,
                branch, worktree_path, released_at, release_reason, dirty
             ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 0)",
            &claim_insert_params(claim, task_id),
        )?;
    }
    Ok(())
}

fn replace_remote_deps(
    remote: &mut dyn Db,
    public_id: &str,
    deps: &[String],
) -> Result<(), QueueError> {
    let Some(task_id) = lookup_local_task(remote, public_id)? else {
        return Ok(());
    };
    remote.execute(
        "DELETE FROM task_dependencies WHERE task_id = ?",
        &[SqlVal::Int(task_id)],
    )?;
    for dep in deps {
        let Some(dep_id) = lookup_local_task(remote, dep)? else {
            continue;
        };
        remote.execute(
            "INSERT OR IGNORE INTO task_dependencies (task_id, depends_on_task_id) VALUES (?, ?)",
            &[SqlVal::Int(task_id), SqlVal::Int(dep_id)],
        )?;
    }
    Ok(())
}

fn upsert_local_feature(local: &mut dyn Db, feature: &FeatureRec) -> Result<(), QueueError> {
    local.execute(
        "INSERT INTO features (public_id, title, body, created_at, updated_at)
         VALUES (?, ?, ?, ?, ?)
         ON CONFLICT(public_id) DO UPDATE SET
            title = excluded.title,
            body = excluded.body,
            updated_at = excluded.updated_at
         WHERE excluded.updated_at >= features.updated_at",
        &[
            SqlVal::text(&feature.public_id),
            SqlVal::text(&feature.title),
            SqlVal::opt_text(feature.body.clone()),
            SqlVal::text(&feature.created_at),
            SqlVal::text(&feature.updated_at),
        ],
    )?;
    Ok(())
}

fn upsert_remote_feature(remote: &mut dyn Db, feature: &FeatureRec) -> Result<(), QueueError> {
    upsert_local_feature(remote, feature)
}

fn upsert_local_project(local: &mut dyn Db, project: &ProjectRec) -> Result<(), QueueError> {
    local.execute(
        "INSERT INTO projects (
            name, repo, config_path, max_parallel_jobs, require_pr, allow_external_actions,
            stale_disposition, created_at, updated_at
         ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)
         ON CONFLICT(name) DO UPDATE SET
            repo = excluded.repo,
            config_path = excluded.config_path,
            max_parallel_jobs = excluded.max_parallel_jobs,
            require_pr = excluded.require_pr,
            allow_external_actions = excluded.allow_external_actions,
            stale_disposition = excluded.stale_disposition,
            updated_at = excluded.updated_at
         WHERE excluded.updated_at >= projects.updated_at",
        &[
            SqlVal::text(&project.name),
            SqlVal::opt_text(project.repo.clone()),
            SqlVal::opt_text(project.config_path.clone()),
            opt_int(project.max_parallel_jobs),
            SqlVal::Int(project.require_pr),
            SqlVal::Int(project.allow_external_actions),
            SqlVal::text(&project.stale_disposition),
            SqlVal::text(&project.created_at),
            SqlVal::text(&project.updated_at),
        ],
    )?;
    Ok(())
}

fn upsert_remote_project(remote: &mut dyn Db, project: &ProjectRec) -> Result<(), QueueError> {
    upsert_local_project(remote, project)
}

fn lookup_local_task(local: &mut dyn Db, public_id: &str) -> Result<Option<i64>, QueueError> {
    one(
        local,
        "SELECT id FROM tasks WHERE public_id = ?",
        &[SqlVal::text(public_id)],
    )?
    .as_ref()
    .map(|row| req_int(row, 0))
    .transpose()
}

fn lookup_local_feature(local: &mut dyn Db, public_id: &str) -> Result<Option<i64>, QueueError> {
    one(
        local,
        "SELECT id FROM features WHERE public_id = ?",
        &[SqlVal::text(public_id)],
    )?
    .as_ref()
    .map(|row| req_int(row, 0))
    .transpose()
}

fn lookup_local_project(local: &mut dyn Db, name: &str) -> Result<Option<i64>, QueueError> {
    one(
        local,
        "SELECT id FROM projects WHERE name = ?",
        &[SqlVal::text(name)],
    )?
    .as_ref()
    .map(|row| req_int(row, 0))
    .transpose()
}

fn load_publish_bundle(
    conn: &Connection,
    task_id: i64,
    agent_id: &str,
    token: &str,
    claimed_at: &str,
    lease_expires_at: &str,
) -> Result<PublishBundle, QueueError> {
    let public_id: String = conn
        .query_row(
            "SELECT public_id FROM tasks WHERE id = ?",
            params![task_id],
            |row| row.get(0),
        )
        .map_err(|err| QueueError::Database(err.to_string()))?;
    let task = load_task_through(conn, &public_id)?;
    let project = match &task.project_name {
        Some(name) => load_project_through(conn, name)?,
        None => None,
    };
    let feature = match &task.feature_public_id {
        Some(id) => load_feature_through(conn, id)?,
        None => None,
    };
    let claim = ClaimRec {
        id: 0,
        task_public_id: public_id,
        agent_id: agent_id.to_string(),
        claim_token: token.to_string(),
        claimed_at: claimed_at.to_string(),
        heartbeat_at: claimed_at.to_string(),
        lease_expires_at: lease_expires_at.to_string(),
        branch: None,
        worktree_path: None,
        released_at: None,
        release_reason: None,
    };
    Ok(PublishBundle {
        task,
        claim,
        project,
        feature,
    })
}

fn load_task_through(conn: &Connection, public_id: &str) -> Result<TaskRec, QueueError> {
    conn.query_row(
        "SELECT tasks.public_id, tasks.title, tasks.body, tasks.original_capture, tasks.status,
            tasks.kind, tasks.priority, tasks.risk, tasks.project_name, tasks.repo,
            tasks.capture_path, tasks.repo_relative_path, tasks.git_root, tasks.git_head,
            tasks.agent_pool, tasks.required_capabilities_json, tasks.blocked_reason,
            features.public_id, tasks.created_at, tasks.updated_at, tasks.idempotency_key
         FROM tasks LEFT JOIN features ON features.id = tasks.feature_id
         WHERE tasks.public_id = ?",
        params![public_id],
        |row| {
            Ok(TaskRec {
                public_id: row.get(0)?,
                title: row.get(1)?,
                body: row.get(2)?,
                original_capture: row.get(3)?,
                status: row.get(4)?,
                kind: row.get(5)?,
                priority: row.get(6)?,
                risk: row.get(7)?,
                project_name: row.get(8)?,
                repo: row.get(9)?,
                capture_path: row.get(10)?,
                repo_relative_path: row.get(11)?,
                git_root: row.get(12)?,
                git_head: row.get(13)?,
                agent_pool: row.get(14)?,
                required_capabilities_json: row.get(15)?,
                blocked_reason: row.get(16)?,
                feature_public_id: row.get(17)?,
                created_at: row.get(18)?,
                updated_at: row.get(19)?,
                idempotency_key: row.get(20)?,
            })
        },
    )
    .map_err(|err| QueueError::Database(err.to_string()))
}

fn load_project_through(conn: &Connection, name: &str) -> Result<Option<ProjectRec>, QueueError> {
    conn.query_row(
        "SELECT name, repo, config_path, max_parallel_jobs, require_pr, allow_external_actions,
            stale_disposition, created_at, updated_at
         FROM projects WHERE name = ?",
        params![name],
        |row| {
            Ok(ProjectRec {
                name: row.get(0)?,
                repo: row.get(1)?,
                config_path: row.get(2)?,
                max_parallel_jobs: row.get(3)?,
                require_pr: row.get(4)?,
                allow_external_actions: row.get(5)?,
                stale_disposition: row.get(6)?,
                created_at: row.get(7)?,
                updated_at: row.get(8)?,
            })
        },
    )
    .optional()
    .map_err(|err| QueueError::Database(err.to_string()))
}

fn load_feature_through(
    conn: &Connection,
    public_id: &str,
) -> Result<Option<FeatureRec>, QueueError> {
    conn.query_row(
        "SELECT public_id, title, body, created_at, updated_at FROM features WHERE public_id = ?",
        params![public_id],
        |row| {
            Ok(FeatureRec {
                public_id: row.get(0)?,
                title: row.get(1)?,
                body: row.get(2)?,
                created_at: row.get(3)?,
                updated_at: row.get(4)?,
            })
        },
    )
    .optional()
    .map_err(|err| QueueError::Database(err.to_string()))
}

fn one(db: &mut dyn Db, sql: &str, params: &[SqlVal]) -> Result<Option<Vec<SqlVal>>, QueueError> {
    let rows = db.query(sql, params)?;
    Ok(rows.into_iter().next())
}

fn req_text(row: &[SqlVal], idx: usize) -> Result<String, QueueError> {
    match row.get(idx) {
        Some(SqlVal::Text(value)) => Ok(value.clone()),
        _ => Err(QueueError::Database(format!(
            "expected text at column {idx}"
        ))),
    }
}

fn opt_text(row: &[SqlVal], idx: usize) -> Result<Option<String>, QueueError> {
    match row.get(idx) {
        Some(SqlVal::Null) | None => Ok(None),
        Some(SqlVal::Text(value)) => Ok(Some(value.clone())),
        Some(SqlVal::Int(value)) => Ok(Some(value.to_string())),
    }
}

fn req_int(row: &[SqlVal], idx: usize) -> Result<i64, QueueError> {
    match row.get(idx) {
        Some(SqlVal::Int(value)) => Ok(*value),
        _ => Err(QueueError::Database(format!(
            "expected integer at column {idx}"
        ))),
    }
}

fn opt_int_col(row: &[SqlVal], idx: usize) -> Result<Option<i64>, QueueError> {
    match row.get(idx) {
        Some(SqlVal::Null) | None => Ok(None),
        Some(SqlVal::Int(value)) => Ok(Some(*value)),
        _ => Err(QueueError::Database(format!(
            "expected integer or null at column {idx}"
        ))),
    }
}

fn opt_int(value: Option<i64>) -> SqlVal {
    match value {
        Some(value) => SqlVal::Int(value),
        None => SqlVal::Null,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn side(status: &str, token: Option<&str>, dirty: bool, updated: &str) -> ClaimSide {
        ClaimSide {
            status: status.into(),
            updated_at: updated.into(),
            latest_claim_token: token.map(str::to_string),
            dirty,
        }
    }

    #[test]
    fn authority_claim_is_adopted_when_the_local_row_has_no_token() {
        let local = side("ready", None, true, "2026-01-01T00:00:04Z");
        let remote = side(
            "claimed",
            Some("remote-token"),
            false,
            "2026-01-01T00:00:03Z",
        );
        assert_eq!(choose_merge(&local, &remote), MergeChoice::AdoptRemote);
    }

    #[test]
    fn same_token_keeps_a_dirty_local_completion() {
        let local = side("done", Some("same"), true, "2026-01-01T00:00:04Z");
        let remote = side("claimed", Some("same"), false, "2026-01-01T00:00:03Z");
        assert_eq!(choose_merge(&local, &remote), MergeChoice::KeepLocal);
    }

    #[test]
    fn dirty_local_row_is_kept_when_the_authority_has_no_claim() {
        let local = side("ready", None, true, "2026-01-01T00:00:02Z");
        let remote = side("inbox", None, false, "2026-01-01T00:00:03Z");
        assert_eq!(choose_merge(&local, &remote), MergeChoice::KeepLocal);
    }

    #[test]
    fn clean_local_row_adopts_a_newer_remote_task() {
        let local = side("ready", None, false, "2026-01-01T00:00:01Z");
        let remote = side("ready", None, false, "2026-01-01T00:00:02Z");
        assert_eq!(choose_merge(&local, &remote), MergeChoice::AdoptRemote);
    }
}
