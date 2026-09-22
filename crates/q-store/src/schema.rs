use rusqlite::{params, Connection, TransactionBehavior};
use time::OffsetDateTime;

use q_core::{format_timestamp, QueueError};

const SCHEMA_V1: &str = r#"
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
    if current < 1 {
        tx.execute_batch(SCHEMA_V1)
            .map_err(|err| QueueError::Database(err.to_string()))?;
        tx.execute(
            "INSERT INTO schema_migrations (version, applied_at) VALUES (1, ?)",
            params![format_timestamp(OffsetDateTime::now_utc())],
        )
        .map_err(|err| QueueError::Database(err.to_string()))?;
    }
    tx.commit()
        .map_err(|err| QueueError::Database(err.to_string()))?;
    Ok(())
}
