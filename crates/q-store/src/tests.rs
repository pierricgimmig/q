use std::fs;
use std::path::PathBuf;
use std::sync::{Arc, Barrier};
use std::thread;
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{params, Connection};

use q_core::{
    Actor, ArtifactInput, BlockRequest, CancelRequest, CaptureRequest, ClaimRequest,
    CompleteRequest, DeleteRequest, EditRequest, HeartbeatRequest, ListFilter, ProjectPolicy,
    QueueError, QueueService, ReadyRequest, RecoverRequest, ReleaseRequest, RiskLevel,
    StaleDisposition, StartRequest, TaskKind, TaskStatus, NO_ELIGIBLE_REASON,
};

use super::{open_connection, Queue};

const BODY: &str = r#"
## Goal
Ship the harness

## Repository / target
github.com/acme/demo

## Scope
Harness only

## Deliverable
A report

## Acceptance criteria
- Compare encodings
- Do not change production storage

## Constraints / do not do
- No production changes

## Dependencies
- None
"#;

fn temp_db() -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("q-store-{nanos}-{}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    dir.join("queue.db")
}

fn queue() -> (Queue, PathBuf) {
    let path = temp_db();
    let queue = Queue::open(&path).unwrap();
    (queue, path)
}

fn actor() -> Actor {
    Actor::human(Some("tester".into()))
}

fn capture_with(queue: &Queue, title: &str, risk: RiskLevel, policy: Option<ProjectPolicy>) -> i64 {
    queue
        .capture(CaptureRequest {
            title: title.into(),
            body: None,
            kind: TaskKind::Implementation,
            priority: 0,
            risk,
            project: Some("demo".into()),
            repo: Some("git@github.com:acme/demo.git".into()),
            capture_path: "/tmp/demo".into(),
            repo_relative_path: None,
            git_root: Some("/tmp/demo".into()),
            git_head: Some("abc123".into()),
            agent_pool: None,
            required_capabilities: vec![],
            dependencies: vec![],
            policy,
            actor: actor(),
            context_source: Some("test".into()),
        })
        .unwrap()
        .id
}

fn capture(queue: &Queue, title: &str) -> i64 {
    capture_with(queue, title, RiskLevel::Low, None)
}

fn make_ready(queue: &Queue, id: i64) {
    let mut edit = EditRequest::empty(actor());
    edit.body = Some(BODY.into());
    queue.edit(id, edit).unwrap();
    queue
        .mark_ready(ReadyRequest {
            task_id: id,
            actor: actor(),
        })
        .unwrap();
}

fn claim(queue: &Queue, agent: &str) -> q_core::ClaimOutcome {
    let mut request = ClaimRequest::new(agent);
    request.maximum_risk = RiskLevel::Medium;
    queue.claim_next(request).unwrap()
}

fn rewind_lease(path: &PathBuf, task_id: i64) {
    let conn = Connection::open(path).unwrap();
    conn.execute(
        "UPDATE claims SET lease_expires_at = '2000-01-01T00:00:00Z' WHERE task_id = ? AND released_at IS NULL",
        params![task_id],
    )
    .unwrap();
}

#[test]
fn fresh_database_enables_wal_foreign_keys_and_busy_timeout() {
    let path = temp_db();
    let conn = open_connection(&path).unwrap();
    let mode: String = conn
        .query_row("PRAGMA journal_mode", [], |row| row.get(0))
        .unwrap();
    assert_eq!(mode.to_lowercase(), "wal");
    let fk: i64 = conn
        .query_row("PRAGMA foreign_keys", [], |row| row.get(0))
        .unwrap();
    assert_eq!(fk, 1);
    let busy: i64 = conn
        .query_row("PRAGMA busy_timeout", [], |row| row.get(0))
        .unwrap();
    assert_eq!(busy, 5000);
    let version: i64 = conn
        .query_row("SELECT MAX(version) FROM schema_migrations", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(version, 1);

    let (queue, _) = (Queue::open(&path).unwrap(), path.clone());
    let id = capture(&queue, "persist me");
    let again = open_connection(&path).unwrap();
    let count: i64 = again
        .query_row("SELECT COUNT(*) FROM tasks", [], |row| row.get(0))
        .unwrap();
    assert_eq!(count, 1);
    assert_eq!(queue.get(id).unwrap().task.title, "persist me");

    let bad = again.execute(
        "INSERT INTO artifacts (task_id, kind, value, created_at) VALUES (99999, 'pr', 'x', '2020-01-01T00:00:00Z')",
        [],
    );
    assert!(bad.is_err());
}

#[test]
fn capture_preserves_original_text_and_normalizes_repo() {
    let (queue, _) = queue();
    let id = capture(&queue, "Benchmark delta coding");
    let mut edit = EditRequest::empty(actor());
    edit.title = Some("Renamed".into());
    edit.body = Some(BODY.into());
    queue.edit(id, edit).unwrap();
    let task = queue.get(id).unwrap().task;
    assert_eq!(task.title, "Renamed");
    assert_eq!(task.original_capture, "Benchmark delta coding");
    assert_eq!(task.status, TaskStatus::Inbox);
    assert_eq!(task.repo.as_deref(), Some("github.com/acme/demo"));
    let events = queue.events(id).unwrap();
    assert!(events
        .iter()
        .any(|event| event.event_type == "task_created"));
    assert!(events.iter().any(|event| event.event_type == "task_edited"));
}

#[test]
fn sparse_inbox_task_can_be_marked_ready() {
    let (queue, _) = queue();
    let id = capture(&queue, "sparse idea");
    assert_eq!(queue.get(id).unwrap().task.status, TaskStatus::Inbox);
    let outcome = queue
        .mark_ready(ReadyRequest {
            task_id: id,
            actor: actor(),
        })
        .unwrap();
    assert_eq!(outcome.task.status, TaskStatus::Ready);
    assert!(outcome
        .warnings
        .iter()
        .any(|warning| warning.contains("Goal")));
    assert_eq!(queue.get(id).unwrap().task.status, TaskStatus::Ready);
    let events = queue.events(id).unwrap();
    assert!(events.iter().any(|event| event.event_type == "task_ready"));
}

#[test]
fn specified_task_can_be_marked_ready() {
    let (queue, _) = queue();
    let id = capture(&queue, "specified");
    make_ready(&queue, id);
    assert_eq!(queue.get(id).unwrap().task.status, TaskStatus::Ready);
}

#[test]
fn concurrent_claims_have_exactly_one_winner() {
    let (queue, _) = queue();
    let id = capture(&queue, "only one");
    make_ready(&queue, id);
    let barrier = Arc::new(Barrier::new(2));
    let mut handles = Vec::new();
    for name in ["codex-local-01", "claude-local-01"] {
        let queue = queue.clone();
        let barrier = barrier.clone();
        handles.push(thread::spawn(move || {
            barrier.wait();
            claim(&queue, name)
        }));
    }
    let results: Vec<_> = handles
        .into_iter()
        .map(|handle| handle.join().unwrap())
        .collect();
    assert_eq!(results.iter().filter(|result| result.found).count(), 1);
    assert_eq!(results.iter().filter(|result| !result.found).count(), 1);
    let loser = results.iter().find(|result| !result.found).unwrap();
    assert_eq!(loser.reason.as_deref(), Some(NO_ELIGIBLE_REASON));
    assert_eq!(queue.get(id).unwrap().task.status, TaskStatus::Claimed);
}

#[test]
fn expired_claims_are_recovered_with_events_and_can_be_reclaimed() {
    let (queue, path) = queue();
    let id = capture(&queue, "stale");
    make_ready(&queue, id);
    let first = claim(&queue, "agent-a");
    assert!(first.found);
    rewind_lease(&path, id);
    let err = queue
        .heartbeat(HeartbeatRequest {
            task_id: id,
            claim_token: first.claim.as_ref().unwrap().token.clone(),
            lease: None,
            actor: actor(),
        })
        .unwrap_err();
    assert!(matches!(err, QueueError::ClaimExpired));

    let recovered = queue
        .recover_stale(RecoverRequest {
            reason: "agent went away".into(),
            to: None,
            actor: actor(),
        })
        .unwrap();
    assert_eq!(recovered.len(), 1);
    assert_eq!(recovered[0].new_status, TaskStatus::Ready);
    assert_eq!(queue.get(id).unwrap().task.status, TaskStatus::Ready);
    let events = queue.events(id).unwrap();
    assert!(events
        .iter()
        .any(|event| event.event_type == "task_recovered"));
    assert!(queue.get(id).unwrap().claim.unwrap().branch.is_none());

    let second = claim(&queue, "agent-b");
    assert!(second.found);
    assert_eq!(second.task.unwrap().task.status, TaskStatus::Claimed);
}

#[test]
fn claim_recovers_expired_work_inside_the_claim_transaction() {
    let (queue, path) = queue();
    let id = capture(&queue, "recover on claim");
    make_ready(&queue, id);
    let first = claim(&queue, "agent-a");
    rewind_lease(&path, id);
    let second = claim(&queue, "agent-b");
    assert!(second.found);
    assert_eq!(second.claim.unwrap().agent_id, "agent-b");
    let events = queue.events(id).unwrap();
    assert!(events
        .iter()
        .any(|event| event.event_type == "task_recovered"));
    assert!(
        events
            .iter()
            .filter(|event| event.event_type == "task_claimed")
            .count()
            >= 2
    );
    let _ = first;
}

#[test]
fn heartbeat_extends_only_a_matching_unexpired_token() {
    let (queue, path) = queue();
    let id = capture(&queue, "beat");
    make_ready(&queue, id);
    let outcome = claim(&queue, "agent-a");
    let token = outcome.claim.unwrap().token;
    let before = queue.get(id).unwrap().claim.unwrap();
    let err = queue
        .heartbeat(HeartbeatRequest {
            task_id: id,
            claim_token: "not-the-token".into(),
            lease: None,
            actor: actor(),
        })
        .unwrap_err();
    assert!(matches!(err, QueueError::TokenMismatch));
    let unchanged = queue.get(id).unwrap().claim.unwrap();
    assert_eq!(unchanged.lease_expires_at, before.lease_expires_at);

    let updated = queue
        .heartbeat(HeartbeatRequest {
            task_id: id,
            claim_token: token,
            lease: Some(std::time::Duration::from_secs(60 * 60)),
            actor: actor(),
        })
        .unwrap();
    assert!(updated.lease_expires_at >= before.lease_expires_at);
    assert!(updated.active);

    rewind_lease(&path, id);
    let err = queue
        .heartbeat(HeartbeatRequest {
            task_id: id,
            claim_token: updated.token,
            lease: None,
            actor: actor(),
        })
        .unwrap_err();
    assert!(matches!(err, QueueError::ClaimExpired));
}

#[test]
fn complete_release_and_block_require_the_claim_token() {
    let (queue, _) = queue();
    let id = capture(&queue, "owned");
    make_ready(&queue, id);
    let outcome = claim(&queue, "agent-a");
    let token = outcome.claim.unwrap().token;

    let wrong = queue
        .release(ReleaseRequest {
            task_id: id,
            claim_token: "nope".into(),
            reason: "stopping".into(),
            actor: actor(),
        })
        .unwrap_err();
    assert!(matches!(wrong, QueueError::TokenMismatch));

    let blocked = queue
        .block(BlockRequest {
            task_id: id,
            claim_token: None,
            reason: "need a decision".into(),
            actor: actor(),
        })
        .unwrap_err();
    assert!(matches!(blocked, QueueError::InvalidInput(_)));

    queue
        .start(StartRequest {
            task_id: id,
            claim_token: token.clone(),
            branch: Some("agent/task-trace".into()),
            worktree_path: Some("/tmp/wt".into()),
            actor: actor(),
        })
        .unwrap();
    assert_eq!(queue.get(id).unwrap().task.status, TaskStatus::InProgress);
    assert_eq!(
        queue.get(id).unwrap().claim.unwrap().branch.as_deref(),
        Some("agent/task-trace")
    );

    let err = queue
        .complete(CompleteRequest {
            task_id: id,
            claim_token: Some("nope".into()),
            summary: "done".into(),
            target: Some(TaskStatus::Done),
            artifacts: vec![],
            actor: actor(),
        })
        .unwrap_err();
    assert!(matches!(err, QueueError::TokenMismatch));

    let detail = queue
        .complete(CompleteRequest {
            task_id: id,
            claim_token: Some(token),
            summary: "Benchmark report committed".into(),
            target: Some(TaskStatus::Done),
            artifacts: vec![ArtifactInput {
                kind: "report".into(),
                value: "./docs/benchmarks/trace-encoding.md".into(),
            }],
            actor: actor(),
        })
        .unwrap();
    assert_eq!(detail.task.status, TaskStatus::Done);
    assert!(detail
        .artifacts
        .iter()
        .any(|artifact| artifact.kind == "report"));
    assert!(!detail.claim.unwrap().active);

    let id = capture(&queue, "release me");
    make_ready(&queue, id);
    let token = claim(&queue, "agent-b").claim.unwrap().token;
    queue
        .release(ReleaseRequest {
            task_id: id,
            claim_token: token.clone(),
            reason: "Missing credentials for benchmark host".into(),
            actor: actor(),
        })
        .unwrap();
    assert_eq!(queue.get(id).unwrap().task.status, TaskStatus::Ready);

    let id = capture(&queue, "block me");
    make_ready(&queue, id);
    let token = claim(&queue, "agent-c").claim.unwrap().token;
    queue
        .block(BlockRequest {
            task_id: id,
            claim_token: Some(token),
            reason: "Need storage-format decision first".into(),
            actor: actor(),
        })
        .unwrap();
    let task = queue.get(id).unwrap().task;
    assert_eq!(task.status, TaskStatus::Blocked);
    assert_eq!(
        task.blocked_reason.as_deref(),
        Some("Need storage-format decision first")
    );
}

#[test]
fn events_are_append_only_across_status_changes() {
    let (queue, _) = queue();
    let id = capture(&queue, "audit");
    make_ready(&queue, id);
    let before = queue.events(id).unwrap();
    assert!(before.len() >= 2);
    let token = claim(&queue, "agent-a").claim.unwrap().token;
    queue
        .heartbeat(HeartbeatRequest {
            task_id: id,
            claim_token: token,
            lease: None,
            actor: actor(),
        })
        .unwrap();
    let after = queue.events(id).unwrap();
    assert!(after.len() > before.len());
    assert_eq!(&after[..before.len()], &before[..]);
    assert!(after.iter().any(|event| event.event_type == "task_claimed"));
    assert!(after
        .iter()
        .any(|event| event.event_type == "task_heartbeat"));
}

#[test]
fn inbox_and_high_risk_tasks_are_not_claimed_by_default() {
    let (queue, _) = queue();
    let inbox = capture(&queue, "not ready");
    let outcome = claim(&queue, "agent-a");
    assert!(!outcome.found);
    assert_eq!(outcome.reason.as_deref(), Some(NO_ELIGIBLE_REASON));
    assert_eq!(queue.get(inbox).unwrap().task.status, TaskStatus::Inbox);

    let risky = capture_with(&queue, "dangerous", RiskLevel::High, None);
    make_ready(&queue, risky);
    let outcome = claim(&queue, "agent-a");
    assert!(!outcome.found);

    let mut allowed = ClaimRequest::new("agent-a");
    allowed.maximum_risk = RiskLevel::High;
    let outcome = queue.claim_next(allowed).unwrap();
    assert!(outcome.found);
    assert_eq!(outcome.task.unwrap().task.id, risky);

    let external = capture_with(
        &queue,
        "spend money",
        RiskLevel::ExternalAction,
        Some(ProjectPolicy {
            allow_external_actions: false,
            ..ProjectPolicy::default()
        }),
    );
    queue
        .mark_ready(ReadyRequest {
            task_id: external,
            actor: actor(),
        })
        .unwrap();
    let mut request = ClaimRequest::new("agent-a");
    request.maximum_risk = RiskLevel::ExternalAction;
    assert!(!queue.claim_next(request).unwrap().found);
}

fn release_claim(queue: &Queue, task_id: i64, token: &str) {
    queue
        .release(ReleaseRequest {
            task_id,
            claim_token: token.to_string(),
            reason: "put it back".into(),
            actor: actor(),
        })
        .unwrap();
}

#[test]
fn claims_prefer_higher_priority_then_older_tasks() {
    let (queue, _) = queue();
    let older = capture(&queue, "older");
    let newer = capture(&queue, "newer");
    make_ready(&queue, older);
    make_ready(&queue, newer);
    let winner = claim(&queue, "agent-a");
    assert_eq!(winner.task.as_ref().unwrap().task.id, older);
    release_claim(&queue, older, &winner.claim.unwrap().token);

    let mut edit = EditRequest::empty(actor());
    edit.priority = Some(10);
    queue.edit(newer, edit).unwrap();
    let winner = claim(&queue, "agent-b");
    assert_eq!(winner.task.as_ref().unwrap().task.id, newer);
}

#[test]
fn capabilities_and_unfinished_dependencies_block_selection() {
    let (queue, _) = queue();
    let dependency = capture(&queue, "dependency");
    let blocked = capture(&queue, "needs rust and dependency");
    make_ready(&queue, dependency);
    make_ready(&queue, blocked);
    let mut edit = EditRequest::empty(actor());
    edit.required_capabilities = Some(vec!["rust".into()]);
    edit.dependencies = Some(vec![dependency]);
    edit.priority = Some(100);
    queue.edit(blocked, edit).unwrap();

    let plain = claim(&queue, "plain");
    assert_eq!(plain.task.as_ref().unwrap().task.id, dependency);
    let token = plain.claim.unwrap().token;
    queue
        .start(StartRequest {
            task_id: dependency,
            claim_token: token.clone(),
            branch: None,
            worktree_path: None,
            actor: actor(),
        })
        .unwrap();
    queue
        .complete(CompleteRequest {
            task_id: dependency,
            claim_token: Some(token),
            summary: "dependency finished".into(),
            target: Some(TaskStatus::Done),
            artifacts: vec![],
            actor: actor(),
        })
        .unwrap();

    let unskilled = ClaimRequest::new("unskilled");
    assert!(!queue.claim_next(unskilled).unwrap().found);
    let mut skilled = ClaimRequest::new("rust-agent");
    skilled.capabilities = vec!["rust".into()];
    let claimed = queue.claim_next(skilled).unwrap();
    assert_eq!(claimed.task.unwrap().task.id, blocked);
}

#[test]
fn project_cap_limits_active_claims() {
    let (queue, _) = queue();
    let policy = ProjectPolicy {
        max_parallel_jobs: Some(1),
        ..ProjectPolicy::default()
    };
    let first = queue
        .capture(CaptureRequest {
            title: "slot one".into(),
            body: Some(BODY.into()),
            kind: TaskKind::Research,
            priority: 50,
            risk: RiskLevel::Low,
            project: Some("capped".into()),
            repo: Some("github.com/acme/capped".into()),
            capture_path: "/tmp/capped".into(),
            repo_relative_path: None,
            git_root: None,
            git_head: None,
            agent_pool: None,
            required_capabilities: vec![],
            dependencies: vec![],
            policy: Some(policy),
            actor: actor(),
            context_source: None,
        })
        .unwrap();
    let second = queue
        .capture(CaptureRequest {
            title: "slot two".into(),
            body: Some(BODY.into()),
            kind: TaskKind::Research,
            priority: 40,
            risk: RiskLevel::Low,
            project: Some("capped".into()),
            repo: Some("github.com/acme/capped".into()),
            capture_path: "/tmp/capped".into(),
            repo_relative_path: None,
            git_root: None,
            git_head: None,
            agent_pool: None,
            required_capabilities: vec![],
            dependencies: vec![],
            policy: None,
            actor: actor(),
            context_source: None,
        })
        .unwrap();
    for id in [first.id, second.id] {
        queue
            .mark_ready(ReadyRequest {
                task_id: id,
                actor: actor(),
            })
            .unwrap();
    }
    let held = claim(&queue, "cap-agent");
    assert_eq!(held.task.unwrap().task.id, first.id);
    let mut only_capped = ClaimRequest::new("cap-agent-2");
    only_capped.allowed_projects = vec!["capped".into()];
    assert!(!queue.claim_next(only_capped).unwrap().found);
}

#[test]
fn cancel_block_and_reopen_follow_the_state_machine() {
    let (queue, _) = queue();
    let id = capture(&queue, "triage");
    let err = queue
        .cancel(q_core::CancelRequest {
            task_id: id,
            reason: "   ".into(),
            actor: actor(),
        })
        .unwrap_err();
    assert!(matches!(err, QueueError::InvalidInput(_)));
    queue
        .block(BlockRequest {
            task_id: id,
            claim_token: None,
            reason: "not yet".into(),
            actor: actor(),
        })
        .unwrap();
    queue
        .cancel(CancelRequest {
            task_id: id,
            reason: "superseded".into(),
            actor: actor(),
        })
        .unwrap();
    assert_eq!(queue.get(id).unwrap().task.status, TaskStatus::Cancelled);
    queue.reopen(id, actor()).unwrap();
    assert_eq!(queue.get(id).unwrap().task.status, TaskStatus::Inbox);
    let err = queue
        .recover_stale(RecoverRequest {
            reason: " ".into(),
            to: Some(StaleDisposition::Blocked),
            actor: actor(),
        })
        .unwrap_err();
    assert!(matches!(err, QueueError::InvalidInput(_)));
}

#[test]
fn reopen_done_returns_to_ready_without_an_active_claim() {
    let (queue, _) = queue();
    let id = capture(&queue, "finished");
    make_ready(&queue, id);
    let token = claim(&queue, "agent-a").claim.unwrap().token;
    queue
        .complete(CompleteRequest {
            task_id: id,
            claim_token: Some(token),
            summary: "shipped".into(),
            target: Some(TaskStatus::Done),
            artifacts: vec![],
            actor: actor(),
        })
        .unwrap();

    let reopened = queue.reopen(id, actor()).unwrap();
    assert_eq!(reopened.status, TaskStatus::Ready);
    let detail = queue.get(id).unwrap();
    assert_eq!(detail.task.status, TaskStatus::Ready);
    assert!(detail.claim.is_none_or(|claim| !claim.active));

    let outcome = claim(&queue, "agent-b");
    assert!(outcome.found);
    let task = outcome.task.unwrap().task;
    assert_eq!(task.id, id);
    assert_eq!(task.status, TaskStatus::Claimed);
    assert!(queue.get(id).unwrap().claim.unwrap().active);
}

fn count_rows(path: &PathBuf, sql: &str, task_id: i64) -> i64 {
    let conn = Connection::open(path).unwrap();
    conn.query_row(sql, params![task_id], |row| row.get(0))
        .unwrap()
}

#[test]
fn delete_removes_inbox_and_ready_tasks_and_cascades_dependents() {
    let (queue, path) = queue();
    let inbox = capture(&queue, "inbox task");
    let ready = capture(&queue, "ready task");
    make_ready(&queue, ready);
    let mut edit = EditRequest::empty(actor());
    edit.dependencies = Some(vec![inbox]);
    queue.edit(ready, edit).unwrap();

    let removed = queue
        .delete(DeleteRequest {
            task_id: inbox,
            force: false,
            actor: actor(),
        })
        .unwrap();
    assert_eq!(removed.task_id, inbox);
    assert_eq!(removed.status, TaskStatus::Inbox);
    assert!(!removed.active_claim_cleared);
    assert!(removed.events_removed >= 1);
    assert_eq!(removed.dependencies_removed, 1);
    assert!(matches!(
        queue.get(inbox).unwrap_err(),
        QueueError::NotFound(_)
    ));
    let listed = queue
        .list(ListFilter {
            status: None,
            project: None,
            repo: None,
            kind: None,
            limit: 100,
            include_terminal: false,
        })
        .unwrap();
    assert!(listed.iter().all(|task| task.id != inbox));
    assert!(listed.iter().any(|task| task.id == ready));
    assert_eq!(
        count_rows(
            &path,
            "SELECT COUNT(*) FROM claims WHERE task_id = ?",
            inbox
        ),
        0
    );
    assert_eq!(
        count_rows(
            &path,
            "SELECT COUNT(*) FROM events WHERE task_id = ?",
            inbox
        ),
        0
    );
    assert_eq!(
        count_rows(
            &path,
            "SELECT COUNT(*) FROM task_dependencies WHERE task_id = ?1 OR depends_on_task_id = ?1",
            inbox
        ),
        0
    );
    assert_eq!(
        queue.get(ready).unwrap().task.dependencies,
        Vec::<i64>::new()
    );

    let done = queue
        .delete(DeleteRequest {
            task_id: ready,
            force: false,
            actor: actor(),
        })
        .unwrap();
    assert_eq!(done.status, TaskStatus::Ready);
    assert!(matches!(
        queue.get(ready).unwrap_err(),
        QueueError::NotFound(_)
    ));
    let listed = queue
        .list(ListFilter {
            status: Some(TaskStatus::Ready),
            project: None,
            repo: None,
            kind: None,
            limit: 100,
            include_terminal: false,
        })
        .unwrap();
    assert!(listed.is_empty());
}

#[test]
fn delete_rejects_an_active_claim_unless_forced_and_clears_it() {
    let (queue, path) = queue();
    let id = capture(&queue, "claimed work");
    make_ready(&queue, id);
    let outcome = claim(&queue, "agent-a");
    assert!(outcome.found);
    let token = outcome.claim.unwrap().token;
    queue
        .complete(CompleteRequest {
            task_id: id,
            claim_token: Some(token),
            summary: "shipped".into(),
            target: Some(TaskStatus::Done),
            artifacts: vec![ArtifactInput {
                kind: "report".into(),
                value: "notes".into(),
            }],
            actor: actor(),
        })
        .unwrap();
    let done = queue
        .delete(DeleteRequest {
            task_id: id,
            force: false,
            actor: actor(),
        })
        .unwrap();
    assert!(done.artifacts_removed >= 1);
    assert_eq!(
        count_rows(
            &path,
            "SELECT COUNT(*) FROM artifacts WHERE task_id = ?",
            id
        ),
        0
    );
    assert_eq!(
        count_rows(&path, "SELECT COUNT(*) FROM claims WHERE task_id = ?", id),
        0
    );

    let active = capture(&queue, "still claimed");
    make_ready(&queue, active);
    assert!(claim(&queue, "agent-b").found);
    let err = queue
        .delete(DeleteRequest {
            task_id: active,
            force: false,
            actor: actor(),
        })
        .unwrap_err();
    match err {
        QueueError::Conflict(message) => {
            assert!(message.contains("active claim"), "{message}");
            assert!(message.contains("agent-b"), "{message}");
            assert!(message.contains("--force"), "{message}");
        }
        other => panic!("unexpected error: {other:?}"),
    }
    assert_eq!(queue.get(active).unwrap().task.status, TaskStatus::Claimed);

    let removed = queue
        .delete(DeleteRequest {
            task_id: active,
            force: true,
            actor: actor(),
        })
        .unwrap();
    assert!(removed.forced);
    assert!(removed.active_claim_cleared);
    assert!(removed.claims_removed >= 1);
    assert!(matches!(
        queue.get(active).unwrap_err(),
        QueueError::NotFound(_)
    ));
    assert_eq!(
        count_rows(
            &path,
            "SELECT COUNT(*) FROM claims WHERE task_id = ?",
            active
        ),
        0
    );
    assert_eq!(
        count_rows(
            &path,
            "SELECT COUNT(*) FROM events WHERE task_id = ?",
            active
        ),
        0
    );

    let expired = capture(&queue, "expired lease");
    make_ready(&queue, expired);
    let claimed = claim(&queue, "agent-c");
    let expired_id = claimed.task.unwrap().task.id;
    rewind_lease(&path, expired_id);
    let removed = queue
        .delete(DeleteRequest {
            task_id: expired_id,
            force: false,
            actor: actor(),
        })
        .unwrap();
    assert!(!removed.active_claim_cleared);
    assert!(removed.claims_removed >= 1);
    assert_eq!(
        count_rows(
            &path,
            "SELECT COUNT(*) FROM claims WHERE task_id = ?",
            expired_id
        ),
        0
    );
}

#[test]
fn empty_claim_is_success_and_list_filters() {
    let (queue, _) = queue();
    let outcome = claim(&queue, "nobody");
    assert!(!outcome.found);
    let id = capture(&queue, "listed");
    let rows = queue
        .list(ListFilter {
            status: Some(TaskStatus::Inbox),
            project: Some("demo".into()),
            repo: Some("https://github.com/acme/demo.git".into()),
            kind: Some(TaskKind::Implementation),
            limit: 10,
            include_terminal: false,
        })
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].id, id);
    let status = queue.status().unwrap();
    assert_eq!(status.counts.inbox, 1);
    assert_eq!(status.active_claims, 0);
}

fn capture_named(queue: &Queue, title: &str, project: Option<&str>) -> i64 {
    queue
        .capture(CaptureRequest {
            title: title.into(),
            body: None,
            kind: TaskKind::Implementation,
            priority: 0,
            risk: RiskLevel::Low,
            project: project.map(str::to_string),
            repo: None,
            capture_path: "/tmp/demo".into(),
            repo_relative_path: None,
            git_root: None,
            git_head: None,
            agent_pool: None,
            required_capabilities: vec![],
            dependencies: vec![],
            policy: None,
            actor: actor(),
            context_source: Some("test".into()),
        })
        .unwrap()
        .id
}

fn finish(queue: &Queue, id: i64) {
    make_ready(queue, id);
    let token = claim(queue, "agent-list").claim.unwrap().token;
    queue
        .start(StartRequest {
            task_id: id,
            claim_token: token.clone(),
            branch: None,
            worktree_path: None,
            actor: actor(),
        })
        .unwrap();
    queue
        .complete(CompleteRequest {
            task_id: id,
            claim_token: Some(token),
            summary: "done".into(),
            target: Some(TaskStatus::Done),
            artifacts: vec![],
            actor: actor(),
        })
        .unwrap();
}

fn set_updated(path: &PathBuf, id: i64, updated_at: &str) {
    let conn = Connection::open(path).unwrap();
    let changed = conn
        .execute(
            "UPDATE tasks SET updated_at = ?1 WHERE id = ?2",
            params![updated_at, id],
        )
        .unwrap();
    assert_eq!(changed, 1);
}

fn set_project_name(path: &PathBuf, id: i64, project: Option<&str>) {
    let conn = Connection::open(path).unwrap();
    let changed = conn
        .execute(
            "UPDATE tasks SET project_name = ?1 WHERE id = ?2",
            params![project, id],
        )
        .unwrap();
    assert_eq!(changed, 1);
}

fn listed(
    queue: &Queue,
    status: Option<TaskStatus>,
    include_terminal: bool,
    limit: u32,
) -> Vec<i64> {
    queue
        .list(ListFilter {
            status,
            project: None,
            repo: None,
            kind: None,
            limit,
            include_terminal,
        })
        .unwrap()
        .into_iter()
        .map(|task| task.id)
        .collect()
}

#[test]
fn list_hides_terminal_statuses_and_sorts_by_project_then_updated_at() {
    let (queue, path) = queue();
    let alpha_done = capture_named(&queue, "alpha done", Some("Alpha"));
    finish(&queue, alpha_done);
    let alpha_new = capture_named(&queue, "alpha new", Some("Alpha"));
    let alpha_old = capture_named(&queue, "alpha old", Some("Alpha"));
    let beta_live = capture_named(&queue, "beta live", Some("beta"));
    let beta_cancelled = capture_named(&queue, "beta cancelled", Some("beta"));
    queue
        .cancel(CancelRequest {
            task_id: beta_cancelled,
            reason: "superseded".into(),
            actor: actor(),
        })
        .unwrap();
    let zeta_first = capture_named(&queue, "zeta first", Some("zeta"));
    let zeta_second = capture_named(&queue, "zeta second", Some("zeta"));
    let none_new = capture_named(&queue, "none new", None);
    let none_old = capture_named(&queue, "none old", None);
    let blank = capture_named(&queue, "blank project", Some("temp"));
    set_project_name(&path, blank, Some("   "));

    set_updated(&path, alpha_done, "2026-07-01T00:00:00Z");
    set_updated(&path, alpha_new, "2026-06-01T00:00:00Z");
    set_updated(&path, alpha_old, "2026-01-01T00:00:00Z");
    set_updated(&path, beta_cancelled, "2026-12-01T00:00:00Z");
    set_updated(&path, beta_live, "2026-08-01T00:00:00Z");
    set_updated(&path, zeta_first, "2026-05-01T00:00:00Z");
    set_updated(&path, zeta_second, "2026-05-01T00:00:00Z");
    set_updated(&path, none_new, "2026-09-01T00:00:00Z");
    set_updated(&path, blank, "2026-04-01T00:00:00Z");
    set_updated(&path, none_old, "2026-02-01T00:00:00Z");

    let open = listed(&queue, None, false, 100);
    assert_eq!(
        open,
        vec![
            alpha_new,
            alpha_old,
            beta_live,
            zeta_second,
            zeta_first,
            none_new,
            blank,
            none_old,
        ]
    );
    assert!(!open.contains(&alpha_done));
    assert!(!open.contains(&beta_cancelled));

    let all = listed(&queue, None, true, 100);
    assert_eq!(
        all,
        vec![
            alpha_done,
            alpha_new,
            alpha_old,
            beta_cancelled,
            beta_live,
            zeta_second,
            zeta_first,
            none_new,
            blank,
            none_old,
        ]
    );

    assert_eq!(
        listed(&queue, Some(TaskStatus::Cancelled), false, 100),
        vec![beta_cancelled]
    );
    assert_eq!(
        listed(&queue, Some(TaskStatus::Done), false, 100),
        vec![alpha_done]
    );
    let inbox = listed(&queue, Some(TaskStatus::Inbox), true, 100);
    assert!(!inbox.contains(&alpha_done));
    assert!(!inbox.contains(&beta_cancelled));
    assert!(inbox.contains(&alpha_new));
    assert_eq!(listed(&queue, None, false, 1), vec![alpha_new]);

    let beta_only = queue
        .list(ListFilter {
            status: None,
            project: Some("beta".into()),
            repo: None,
            kind: None,
            limit: 100,
            include_terminal: true,
        })
        .unwrap();
    assert_eq!(
        beta_only.iter().map(|task| task.id).collect::<Vec<_>>(),
        vec![beta_cancelled, beta_live]
    );
    let blank_row = queue.get(blank).unwrap().task;
    assert_eq!(blank_row.project.as_deref(), Some("   "));
}
