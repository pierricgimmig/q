use rusqlite::{params, Connection, TransactionBehavior};
use time::OffsetDateTime;

use q_core::{format_timestamp, QueueError};

pub(crate) const SCHEMA_V1: &str = r#"
CREATE TABLE IF NOT EXISTS projects (
  id INTEGER PRIMARY KEY,
  name TEXT NOT NULL UNIQUE,
  repo TEXT,
  config_path TEXT,
  max_parallel_jobs INTEGER,
  require_pr INTEGER NOT NULL DEFAULT 0,
  allow_external_actions INTEGER NOT NULL DEFAULT 0,
  stale_disposition TEXT NOT NULL DEFAULT 'ready',
  created_at TEXT NOT NULL,
  updated_at TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS tasks (
  id INTEGER PRIMARY KEY,
  public_id TEXT NOT NULL UNIQUE,
  title TEXT NOT NULL,
  body TEXT,
  original_capture TEXT NOT NULL,
  status TEXT NOT NULL,
  kind TEXT NOT NULL,
  priority INTEGER NOT NULL DEFAULT 0,
  risk TEXT NOT NULL DEFAULT 'low',
  project_id INTEGER REFERENCES projects(id),
  project_name TEXT,
  repo TEXT,
  capture_path TEXT NOT NULL,
  repo_relative_path TEXT,
  git_root TEXT,
  git_head TEXT,
  agent_pool TEXT,
  required_capabilities_json TEXT NOT NULL DEFAULT '[]',
  blocked_reason TEXT,
  created_at TEXT NOT NULL,
  updated_at TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS claims (
  id INTEGER PRIMARY KEY,
  task_id INTEGER NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
  agent_id TEXT NOT NULL,
  claim_token TEXT NOT NULL UNIQUE,
  claimed_at TEXT NOT NULL,
  heartbeat_at TEXT NOT NULL,
  lease_expires_at TEXT NOT NULL,
  branch TEXT,
  worktree_path TEXT,
  released_at TEXT,
  release_reason TEXT
);

CREATE UNIQUE INDEX IF NOT EXISTS claims_one_active
ON claims(task_id) WHERE released_at IS NULL;

CREATE TABLE IF NOT EXISTS task_dependencies (
  task_id INTEGER NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
  depends_on_task_id INTEGER NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
  PRIMARY KEY (task_id, depends_on_task_id),
  CHECK (task_id != depends_on_task_id)
);

CREATE TABLE IF NOT EXISTS artifacts (
  id INTEGER PRIMARY KEY,
  task_id INTEGER NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
  kind TEXT NOT NULL,
  value TEXT NOT NULL,
  created_at TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS events (
  id INTEGER PRIMARY KEY,
  task_id INTEGER REFERENCES tasks(id) ON DELETE CASCADE,
  event_type TEXT NOT NULL,
  actor_type TEXT NOT NULL,
  actor_id TEXT,
  payload_json TEXT NOT NULL DEFAULT '{}',
  created_at TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS tasks_ready_idx
ON tasks(status, priority DESC, created_at ASC);

CREATE INDEX IF NOT EXISTS claims_expiry_idx
ON claims(lease_expires_at);

CREATE INDEX IF NOT EXISTS events_task_idx
ON events(task_id, id);
"#;

const SCHEMA_V2_TASKS: &str = r#"
ALTER TABLE tasks ADD COLUMN feature_id INTEGER REFERENCES features(id) ON DELETE SET NULL;

CREATE INDEX IF NOT EXISTS tasks_feature_idx ON tasks(feature_id);
"#;

pub fn migrate(conn: &mut Connection) -> Result<(), QueueError> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS schema_migrations (
            version INTEGER PRIMARY KEY,
            applied_at TEXT NOT NULL
         );",
    )
    .map_err(|err| QueueError::Database(err.to_string()))?;
    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(|err| QueueError::Database(err.to_string()))?;
    let current: i64 = tx
        .query_row(
            "SELECT COALESCE(MAX(version), 0) FROM schema_migrations",
            [],
            |row| row.get(0),
        )
        .map_err(|err| QueueError::Database(err.to_string()))?;
    let applied_at = format_timestamp(OffsetDateTime::now_utc());
    if current < 1 {
        tx.execute_batch(SCHEMA_V1)
            .map_err(|err| QueueError::Database(err.to_string()))?;
        tx.execute(
            "INSERT INTO schema_migrations (version, applied_at) VALUES (1, ?)",
            params![applied_at],
        )
        .map_err(|err| QueueError::Database(err.to_string()))?;
    }
    if current < 2 {
        apply_v2(&tx, &applied_at)?;
    }
    if current < 3 {
        apply_v3(&tx, &applied_at)?;
    }
    if current < 4 {
        apply_v4(&tx, &applied_at)?;
    }
    if current < 5 {
        apply_v5(&tx, &applied_at)?;
    }
    // Additive column checks run on every open, independent of the version
    // row. A database migrated by another branch can already sit above the
    // version a step is gated on, as `apply_v2` guards `feature_id` too.
    ensure_artifact_content(&tx)?;
    ensure_task_progress(&tx)?;
    ensure_claim_identity(&tx)?;
    ensure_task_failures_and_tags(&tx)?;
    ensure_escalation(&tx)?;
    ensure_claim_activity(&tx)?;
    tx.commit()
        .map_err(|err| QueueError::Database(err.to_string()))?;
    Ok(())
}

fn apply_v2(tx: &rusqlite::Transaction<'_>, applied_at: &str) -> Result<(), QueueError> {
    tx.execute_batch(
        "CREATE TABLE IF NOT EXISTS features (
            id INTEGER PRIMARY KEY,
            public_id TEXT NOT NULL UNIQUE,
            title TEXT NOT NULL,
            body TEXT,
            created_at TEXT NOT NULL,
            updated_at TEXT NOT NULL
         );",
    )
    .map_err(|err| QueueError::Database(err.to_string()))?;
    let has_feature_id: i64 = tx
        .query_row(
            "SELECT COUNT(*) FROM pragma_table_info('tasks') WHERE name = 'feature_id'",
            [],
            |row| row.get(0),
        )
        .map_err(|err| QueueError::Database(err.to_string()))?;
    if has_feature_id == 0 {
        tx.execute_batch(SCHEMA_V2_TASKS)
            .map_err(|err| QueueError::Database(err.to_string()))?;
    } else {
        tx.execute_batch("CREATE INDEX IF NOT EXISTS tasks_feature_idx ON tasks(feature_id);")
            .map_err(|err| QueueError::Database(err.to_string()))?;
    }
    tx.execute(
        "INSERT INTO schema_migrations (version, applied_at) VALUES (2, ?)",
        params![applied_at],
    )
    .map_err(|err| QueueError::Database(err.to_string()))?;
    Ok(())
}

/// v3: artifacts may store their content (a report body) in the database.
fn apply_v3(tx: &rusqlite::Transaction<'_>, applied_at: &str) -> Result<(), QueueError> {
    ensure_artifact_content(tx)?;
    ensure_task_progress(tx)?;
    tx.execute(
        "INSERT INTO schema_migrations (version, applied_at) VALUES (3, ?)",
        params![applied_at],
    )
    .map_err(|err| QueueError::Database(err.to_string()))?;
    Ok(())
}

/// Add `artifacts.content` when it is missing. Safe to call on every open.
fn ensure_artifact_content(tx: &rusqlite::Transaction<'_>) -> Result<(), QueueError> {
    let has_content: i64 = tx
        .query_row(
            "SELECT COUNT(*) FROM pragma_table_info('artifacts') WHERE name = 'content'",
            [],
            |row| row.get(0),
        )
        .map_err(|err| QueueError::Database(err.to_string()))?;
    if has_content == 0 {
        tx.execute_batch("ALTER TABLE artifacts ADD COLUMN content TEXT;")
            .map_err(|err| QueueError::Database(err.to_string()))?;
    }
    Ok(())
}

/// v4: claim identity (model, host), failure counts, and tags.
fn apply_v4(tx: &rusqlite::Transaction<'_>, applied_at: &str) -> Result<(), QueueError> {
    ensure_claim_identity(tx)?;
    ensure_task_failures_and_tags(tx)?;
    tx.execute(
        "INSERT INTO schema_migrations (version, applied_at) VALUES (4, ?)",
        params![applied_at],
    )
    .map_err(|err| QueueError::Database(err.to_string()))?;
    Ok(())
}

fn column_exists(
    tx: &rusqlite::Transaction<'_>,
    table: &str,
    column: &str,
) -> Result<bool, QueueError> {
    // `pragma_table_info` does not take a bound table name on every SQLite build.
    let sql = match table {
        "claims" => "SELECT COUNT(*) FROM pragma_table_info('claims') WHERE name = ?1",
        "tasks" => "SELECT COUNT(*) FROM pragma_table_info('tasks') WHERE name = ?1",
        other => {
            return Err(QueueError::Database(format!(
                "unknown table in migration check: {other}"
            )))
        }
    };
    let count: i64 = tx
        .query_row(sql, params![column], |row| row.get(0))
        .map_err(|err| QueueError::Database(err.to_string()))?;
    Ok(count > 0)
}

/// Add `claims.agent_model` and `claims.agent_host` when they are missing.
fn ensure_claim_identity(tx: &rusqlite::Transaction<'_>) -> Result<(), QueueError> {
    if !column_exists(tx, "claims", "agent_model")? {
        tx.execute_batch("ALTER TABLE claims ADD COLUMN agent_model TEXT;")
            .map_err(|err| QueueError::Database(err.to_string()))?;
    }
    if !column_exists(tx, "claims", "agent_host")? {
        tx.execute_batch("ALTER TABLE claims ADD COLUMN agent_host TEXT;")
            .map_err(|err| QueueError::Database(err.to_string()))?;
    }
    Ok(())
}

/// v5: escalation reason, who, and when. The status itself is just a text value.
fn apply_v5(tx: &rusqlite::Transaction<'_>, applied_at: &str) -> Result<(), QueueError> {
    ensure_escalation(tx)?;
    tx.execute(
        "INSERT INTO schema_migrations (version, applied_at) VALUES (5, ?)",
        params![applied_at],
    )
    .map_err(|err| QueueError::Database(err.to_string()))?;
    Ok(())
}

/// Add escalation columns when they are missing.
fn ensure_escalation(tx: &rusqlite::Transaction<'_>) -> Result<(), QueueError> {
    for column in ["escalated_reason", "escalated_by", "escalated_at"] {
        if !column_exists(tx, "tasks", column)? {
            tx.execute_batch(&format!("ALTER TABLE tasks ADD COLUMN {column} TEXT;"))
                .map_err(|err| QueueError::Database(err.to_string()))?;
        }
    }
    Ok(())
}

/// Add `tasks.failure_count` and `tasks.tags_json` when they are missing.
fn ensure_task_failures_and_tags(tx: &rusqlite::Transaction<'_>) -> Result<(), QueueError> {
    if !column_exists(tx, "tasks", "failure_count")? {
        tx.execute_batch("ALTER TABLE tasks ADD COLUMN failure_count INTEGER NOT NULL DEFAULT 0;")
            .map_err(|err| QueueError::Database(err.to_string()))?;
    }
    if !column_exists(tx, "tasks", "tags_json")? {
        tx.execute_batch("ALTER TABLE tasks ADD COLUMN tags_json TEXT NOT NULL DEFAULT '[]';")
            .map_err(|err| QueueError::Database(err.to_string()))?;
    }
    Ok(())
}

/// Add `tasks.progress` (percent complete) when it is missing.
fn ensure_task_progress(tx: &rusqlite::Transaction<'_>) -> Result<(), QueueError> {
    let has_progress: i64 = tx
        .query_row(
            "SELECT COUNT(*) FROM pragma_table_info('tasks') WHERE name = 'progress'",
            [],
            |row| row.get(0),
        )
        .map_err(|err| QueueError::Database(err.to_string()))?;
    if has_progress == 0 {
        tx.execute_batch("ALTER TABLE tasks ADD COLUMN progress INTEGER;")
            .map_err(|err| QueueError::Database(err.to_string()))?;
    }
    Ok(())
}

/// Add `claims.activity` (what the agent last said it was doing) when missing.
fn ensure_claim_activity(tx: &rusqlite::Transaction<'_>) -> Result<(), QueueError> {
    let has: i64 = tx
        .query_row(
            "SELECT COUNT(*) FROM pragma_table_info('claims') WHERE name = 'activity'",
            [],
            |row| row.get(0),
        )
        .map_err(|err| QueueError::Database(err.to_string()))?;
    if has == 0 {
        tx.execute_batch("ALTER TABLE claims ADD COLUMN activity TEXT;")
            .map_err(|err| QueueError::Database(err.to_string()))?;
    }
    Ok(())
}
