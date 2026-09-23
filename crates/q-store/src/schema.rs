use time::OffsetDateTime;
use uuid::Uuid;

use q_core::{format_timestamp, QueueError};

use crate::session::{query_i64, query_i64s, Db, SqlVal};

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

CREATE TABLE IF NOT EXISTS features (
  id INTEGER PRIMARY KEY,
  public_id TEXT NOT NULL UNIQUE,
  title TEXT NOT NULL,
  body TEXT,
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
  feature_id INTEGER REFERENCES features(id) ON DELETE SET NULL,
  origin TEXT NOT NULL DEFAULT 'local_unsynced',
  created_offline INTEGER NOT NULL DEFAULT 0,
  creator_agent_id TEXT,
  idempotency_key TEXT,
  dirty INTEGER NOT NULL DEFAULT 1,
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
  release_reason TEXT,
  claimed_offline INTEGER NOT NULL DEFAULT 0,
  superseded_at TEXT,
  superseded_reason TEXT,
  dirty INTEGER NOT NULL DEFAULT 1
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
  public_id TEXT,
  dirty INTEGER NOT NULL DEFAULT 1,
  created_at TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS events (
  id INTEGER PRIMARY KEY,
  task_id INTEGER REFERENCES tasks(id) ON DELETE CASCADE,
  event_type TEXT NOT NULL,
  actor_type TEXT NOT NULL,
  actor_id TEXT,
  payload_json TEXT NOT NULL DEFAULT '{}',
  public_id TEXT,
  dirty INTEGER NOT NULL DEFAULT 1,
  created_at TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS tasks_ready_idx
ON tasks(status, priority DESC, created_at ASC);

CREATE INDEX IF NOT EXISTS claims_expiry_idx
ON claims(lease_expires_at);

CREATE INDEX IF NOT EXISTS events_task_idx
ON events(task_id, id);

CREATE INDEX IF NOT EXISTS tasks_feature_idx ON tasks(feature_id);
CREATE INDEX IF NOT EXISTS tasks_origin_idx ON tasks(origin, created_offline);
CREATE UNIQUE INDEX IF NOT EXISTS tasks_idempotency_key
ON tasks(idempotency_key) WHERE idempotency_key IS NOT NULL;
CREATE UNIQUE INDEX IF NOT EXISTS events_public_id ON events(public_id);
CREATE UNIQUE INDEX IF NOT EXISTS artifacts_public_id ON artifacts(public_id);

CREATE TABLE IF NOT EXISTS sync_tombstones (
  entity TEXT NOT NULL,
  public_id TEXT NOT NULL,
  deleted_at TEXT NOT NULL,
  PRIMARY KEY (entity, public_id)
);
"#;

const SCHEMA_V2_TASKS: &str = r#"
ALTER TABLE tasks ADD COLUMN feature_id INTEGER REFERENCES features(id) ON DELETE SET NULL;

CREATE INDEX IF NOT EXISTS tasks_feature_idx ON tasks(feature_id);
"#;

pub fn migrate(conn: &mut dyn Db) -> Result<(), QueueError> {
    migrate_in(conn)
}

pub(crate) fn migrate_in(conn: &mut dyn Db) -> Result<(), QueueError> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS schema_migrations (
            version INTEGER PRIMARY KEY,
            applied_at TEXT NOT NULL
         );",
    )?;
    conn.begin_immediate()?;
    let result = apply_pending(conn);
    if result.is_err() {
        let _ = conn.rollback();
    } else if let Err(err) = conn.commit() {
        let _ = conn.rollback();
        return Err(err);
    }
    result
}

fn apply_pending(conn: &mut dyn Db) -> Result<(), QueueError> {
    let current = query_i64(
        conn,
        "SELECT COALESCE(MAX(version), 0) FROM schema_migrations",
    )?;
    let applied_at = format_timestamp(OffsetDateTime::now_utc());
    // New files get the current schema and are stamped at version 4. Databases
    // created by older builds walk v1 → v2 → v3 → v4. `current` is read once, so a
    // brand-new file does not also run the upgrade alters.
    if current < 1 {
        conn.execute_batch(SCHEMA_V1)?;
        conn.execute(
            "INSERT INTO schema_migrations (version, applied_at) VALUES (4, ?)",
            &[SqlVal::text(applied_at.clone())],
        )?;
    } else {
        if current < 2 {
            apply_v2(conn, &applied_at)?;
        }
        if current < 3 {
            apply_v3(conn, &applied_at)?;
        }
        if current < 4 {
            apply_v4(conn, &applied_at)?;
        }
    }
    Ok(())
}

fn apply_v2(conn: &mut dyn Db, applied_at: &str) -> Result<(), QueueError> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS features (
            id INTEGER PRIMARY KEY,
            public_id TEXT NOT NULL UNIQUE,
            title TEXT NOT NULL,
            body TEXT,
            created_at TEXT NOT NULL,
            updated_at TEXT NOT NULL
         );",
    )?;
    let has_feature_id = query_i64(
        conn,
        "SELECT COUNT(*) FROM pragma_table_info('tasks') WHERE name = 'feature_id'",
    )?;
    if has_feature_id == 0 {
        conn.execute_batch(SCHEMA_V2_TASKS)?;
    } else {
        conn.execute_batch("CREATE INDEX IF NOT EXISTS tasks_feature_idx ON tasks(feature_id);")?;
    }
    conn.execute(
        "INSERT INTO schema_migrations (version, applied_at) VALUES (2, ?)",
        &[SqlVal::text(applied_at)],
    )?;
    Ok(())
}

const SCHEMA_V3: &str = r#"
ALTER TABLE tasks ADD COLUMN origin TEXT NOT NULL DEFAULT 'local_unsynced';
ALTER TABLE tasks ADD COLUMN created_offline INTEGER NOT NULL DEFAULT 0;
ALTER TABLE tasks ADD COLUMN creator_agent_id TEXT;
ALTER TABLE tasks ADD COLUMN dirty INTEGER NOT NULL DEFAULT 1;

ALTER TABLE claims ADD COLUMN claimed_offline INTEGER NOT NULL DEFAULT 0;
ALTER TABLE claims ADD COLUMN superseded_at TEXT;
ALTER TABLE claims ADD COLUMN superseded_reason TEXT;
ALTER TABLE claims ADD COLUMN dirty INTEGER NOT NULL DEFAULT 1;

ALTER TABLE events ADD COLUMN public_id TEXT;
ALTER TABLE events ADD COLUMN dirty INTEGER NOT NULL DEFAULT 1;

ALTER TABLE artifacts ADD COLUMN public_id TEXT;
ALTER TABLE artifacts ADD COLUMN dirty INTEGER NOT NULL DEFAULT 1;

CREATE TABLE IF NOT EXISTS sync_tombstones (
  entity TEXT NOT NULL,
  public_id TEXT NOT NULL,
  deleted_at TEXT NOT NULL,
  PRIMARY KEY (entity, public_id)
);

CREATE INDEX IF NOT EXISTS tasks_origin_idx ON tasks(origin, created_offline);
"#;

fn apply_v3(conn: &mut dyn Db, applied_at: &str) -> Result<(), QueueError> {
    // Each ALTER is skipped when a previous attempt added the column but did not
    // record the version (crash mid-migration).
    if !column_exists(conn, "tasks", "origin")? {
        conn.execute_batch(SCHEMA_V3)?;
    } else {
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS sync_tombstones (
                entity TEXT NOT NULL,
                public_id TEXT NOT NULL,
                deleted_at TEXT NOT NULL,
                PRIMARY KEY (entity, public_id)
             );
             CREATE INDEX IF NOT EXISTS tasks_origin_idx ON tasks(origin, created_offline);",
        )?;
        ensure_column(
            conn,
            "tasks",
            "created_offline",
            "ALTER TABLE tasks ADD COLUMN created_offline INTEGER NOT NULL DEFAULT 0",
        )?;
        ensure_column(
            conn,
            "tasks",
            "creator_agent_id",
            "ALTER TABLE tasks ADD COLUMN creator_agent_id TEXT",
        )?;
        ensure_column(
            conn,
            "tasks",
            "dirty",
            "ALTER TABLE tasks ADD COLUMN dirty INTEGER NOT NULL DEFAULT 1",
        )?;
        ensure_column(
            conn,
            "claims",
            "claimed_offline",
            "ALTER TABLE claims ADD COLUMN claimed_offline INTEGER NOT NULL DEFAULT 0",
        )?;
        ensure_column(
            conn,
            "claims",
            "superseded_at",
            "ALTER TABLE claims ADD COLUMN superseded_at TEXT",
        )?;
        ensure_column(
            conn,
            "claims",
            "superseded_reason",
            "ALTER TABLE claims ADD COLUMN superseded_reason TEXT",
        )?;
        ensure_column(
            conn,
            "claims",
            "dirty",
            "ALTER TABLE claims ADD COLUMN dirty INTEGER NOT NULL DEFAULT 1",
        )?;
        ensure_column(
            conn,
            "events",
            "public_id",
            "ALTER TABLE events ADD COLUMN public_id TEXT",
        )?;
        ensure_column(
            conn,
            "events",
            "dirty",
            "ALTER TABLE events ADD COLUMN dirty INTEGER NOT NULL DEFAULT 1",
        )?;
        ensure_column(
            conn,
            "artifacts",
            "public_id",
            "ALTER TABLE artifacts ADD COLUMN public_id TEXT",
        )?;
        ensure_column(
            conn,
            "artifacts",
            "dirty",
            "ALTER TABLE artifacts ADD COLUMN dirty INTEGER NOT NULL DEFAULT 1",
        )?;
    }
    backfill_public_ids(conn, "events")?;
    backfill_public_ids(conn, "artifacts")?;
    conn.execute_batch(
        "CREATE UNIQUE INDEX IF NOT EXISTS events_public_id ON events(public_id);
         CREATE UNIQUE INDEX IF NOT EXISTS artifacts_public_id ON artifacts(public_id);",
    )?;
    conn.execute(
        "INSERT INTO schema_migrations (version, applied_at) VALUES (3, ?)",
        &[SqlVal::text(applied_at)],
    )?;
    Ok(())
}

fn apply_v4(conn: &mut dyn Db, applied_at: &str) -> Result<(), QueueError> {
    ensure_column(
        conn,
        "tasks",
        "idempotency_key",
        "ALTER TABLE tasks ADD COLUMN idempotency_key TEXT",
    )?;
    conn.execute_batch(
        "CREATE UNIQUE INDEX IF NOT EXISTS tasks_idempotency_key
         ON tasks(idempotency_key) WHERE idempotency_key IS NOT NULL;",
    )?;
    conn.execute(
        "INSERT INTO schema_migrations (version, applied_at) VALUES (4, ?)",
        &[SqlVal::text(applied_at)],
    )?;
    Ok(())
}

fn column_exists(conn: &mut dyn Db, table: &str, column: &str) -> Result<bool, QueueError> {
    let count = query_i64(
        conn,
        &format!("SELECT COUNT(*) FROM pragma_table_info('{table}') WHERE name = '{column}'"),
    )?;
    Ok(count > 0)
}

fn ensure_column(
    conn: &mut dyn Db,
    table: &str,
    column: &str,
    alter: &str,
) -> Result<(), QueueError> {
    if !column_exists(conn, table, column)? {
        conn.execute_batch(alter)?;
    }
    Ok(())
}

fn backfill_public_ids(conn: &mut dyn Db, table: &str) -> Result<(), QueueError> {
    let ids = query_i64s(
        conn,
        &format!("SELECT id FROM {table} WHERE public_id IS NULL"),
    )?;
    for id in ids {
        conn.execute(
            &format!("UPDATE {table} SET public_id = ? WHERE id = ?"),
            &[SqlVal::text(Uuid::now_v7().to_string()), SqlVal::Int(id)],
        )?;
    }
    Ok(())
}
