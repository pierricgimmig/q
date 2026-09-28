//! Drive a real `q serve` over a loopback socket through `RemoteQueue`.

use std::path::PathBuf;
use std::sync::Arc;

use q_core::{
    Actor, ActorKind, ArtifactInput, CaptureRequest, ClaimRequest, CompleteRequest,
    HeartbeatRequest, HoldRequest, ListFilter, LogRequest, QueueError, QueueService, ReadyRequest,
    RiskLevel, StartRequest, TaskKind, TaskStatus,
};
use q_http::{serve_on, AuthConfig, RemoteQueue, ServerOptions, TokenStore};
use q_store::Queue;
use tokio::sync::oneshot;

const HUMAN_SECRET: &str = "human-secret-0123456789";
const AGENT_SECRET: &str = "agent-secret-0123456789";

fn temp_db() -> PathBuf {
    std::env::temp_dir().join(format!("q-http-{}.db", uuid::Uuid::new_v4()))
}

struct Server {
    url: String,
    stop: Option<oneshot::Sender<()>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Server {
    /// Start a server on an ephemeral port in its own runtime thread so the
    /// blocking `ureq` client can run on the test thread.
    fn start(auth: Option<AuthConfig>) -> Self {
        let queue: Arc<dyn QueueService> = Arc::new(Queue::open(temp_db()).unwrap());
        Self::start_on(
            queue,
            auth,
            std::time::Duration::from_secs(q_core::DEFAULT_LEASE_SWEEP_SECS),
        )
    }

    fn start_on(
        queue: Arc<dyn QueueService>,
        auth: Option<AuthConfig>,
        sweep: std::time::Duration,
    ) -> Self {
        let (stop, stopped) = oneshot::channel::<()>();
        let (ready, started) = std::sync::mpsc::channel::<String>();
        let thread = std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            runtime.block_on(async move {
                let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                let addr = listener.local_addr().unwrap();
                ready.send(format!("http://{addr}")).unwrap();
                let mut options = ServerOptions::local(auth.map(TokenStore::fixed));
                options.sweep_interval = sweep;
                serve_on(queue, listener, options, async move {
                    let _ = stopped.await;
                })
                .await
                .unwrap();
            });
        });
        let url = started.recv().unwrap();
        Self {
            url,
            stop: Some(stop),
            thread: Some(thread),
        }
    }

    fn client(&self, token: Option<&str>) -> RemoteQueue {
        RemoteQueue::new(&self.url, token.map(str::to_string)).unwrap()
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn auth() -> AuthConfig {
    AuthConfig::parse(&format!(
        "[[tokens]]\nname=\"pierric\"\nrole=\"human\"\nsecret=\"{HUMAN_SECRET}\"\n\
         [[tokens]]\nname=\"vps-agent\"\nrole=\"agent\"\nsecret=\"{AGENT_SECRET}\"\n"
    ))
    .unwrap()
}

fn capture(title: &str, actor: Actor) -> CaptureRequest {
    CaptureRequest {
        title: title.into(),
        body: None,
        kind: TaskKind::Research,
        priority: 0,
        risk: RiskLevel::Low,
        project: None,
        repo: None,
        capture_path: "/tmp".into(),
        repo_relative_path: None,
        git_root: None,
        git_head: None,
        agent_pool: None,
        required_capabilities: vec![],
        dependencies: vec![],
        feature: None,
        policy: None,
        actor,
        context_source: None,
        // Tests exercise the human gate, so captures start held.
        hold: true,
        tags: vec![],
    }
}

#[test]
fn full_claim_lifecycle_over_http() {
    let server = Server::start(None);
    let queue = server.client(None);
    let health = queue.health().unwrap();
    assert!(health.ok);

    let task = queue
        .capture(capture("Benchmark trace encoding", Actor::human(None)))
        .unwrap();
    assert_eq!(task.status, TaskStatus::Held);

    // A plain capture is ready at once; hold moves it back over the wire.
    let mut open = capture("Open capture", Actor::human(None));
    open.hold = false;
    let open = queue.capture(open).unwrap();
    assert_eq!(open.status, TaskStatus::Ready);
    let open = queue
        .hold(HoldRequest {
            task_id: open.id,
            actor: Actor::human(None),
        })
        .unwrap();
    assert_eq!(open.status, TaskStatus::Held);

    // Held work is not claimable.
    let none = queue.claim_next(ClaimRequest::new("agent-1")).unwrap();
    assert!(!none.found);
    assert_eq!(none.reason.as_deref(), Some(q_core::NO_ELIGIBLE_REASON));

    queue
        .mark_ready(ReadyRequest {
            task_id: task.id,
            actor: Actor::human(None),
        })
        .unwrap();

    let claimed = queue.claim_next(ClaimRequest::new("agent-1")).unwrap();
    assert!(claimed.found);
    let lease = claimed.claim.unwrap();
    assert_eq!(claimed.task.unwrap().task.id, task.id);

    // A second agent gets nothing: the server serializes claims.
    let other = queue.claim_next(ClaimRequest::new("agent-2")).unwrap();
    assert!(!other.found);

    let claim = queue
        .heartbeat(HeartbeatRequest {
            task_id: task.id,
            claim_token: lease.token.clone(),
            lease: None,
            activity: None,
            actor: Actor::agent("agent-1"),
        })
        .unwrap();
    assert!(claim.lease_expires_at >= lease.lease_expires_at);

    let wrong = queue
        .heartbeat(HeartbeatRequest {
            task_id: task.id,
            claim_token: "not-the-token".into(),
            lease: None,
            activity: None,
            actor: Actor::agent("agent-1"),
        })
        .unwrap_err();
    assert!(matches!(wrong, QueueError::TokenMismatch), "{wrong}");

    let detail = queue
        .start(StartRequest {
            task_id: task.id,
            claim_token: lease.token.clone(),
            branch: Some("agent/task-1".into()),
            worktree_path: None,
            actor: Actor::agent("agent-1"),
        })
        .unwrap();
    assert_eq!(detail.task.status, TaskStatus::InProgress);

    // Mid-task notes and inline artifacts go through the same authority.
    let logged = queue
        .log(LogRequest {
            task_id: task.id,
            claim_token: Some(lease.token.clone()),
            message: Some("Encoder read; varint path looks slow".into()),
            progress: Some(25),
            artifacts: vec![ArtifactInput {
                kind: "report".into(),
                value: "notes.md".into(),
                content: Some("# Notes\n\nvarint\n".into()),
            }],
            actor: Actor::human(None),
        })
        .unwrap();
    let note = logged
        .events
        .iter()
        .find(|event| event.event_type == q_core::NOTE_EVENT)
        .expect("note event");
    assert_eq!(note.actor_id.as_deref(), Some("agent-1"));
    assert_eq!(logged.task.progress, Some(25));
    let stored = logged
        .artifacts
        .iter()
        .find(|artifact| artifact.kind == "report")
        .unwrap();
    assert_eq!(stored.content_bytes, Some(16));
    let fetched = queue.artifact(stored.id).unwrap();
    assert_eq!(fetched.content.as_deref(), Some("# Notes\n\nvarint\n"));

    let done = queue
        .complete(CompleteRequest {
            task_id: task.id,
            claim_token: Some(lease.token),
            summary: "Report committed".into(),
            target: None,
            artifacts: vec![],
            actor: Actor::agent("agent-1"),
        })
        .unwrap();
    assert_eq!(done.task.status, TaskStatus::Done);
    assert_eq!(done.task.progress, Some(100), "completion is 100%");

    let missing = queue.get(9999).unwrap_err();
    assert!(matches!(missing, QueueError::NotFound(9999)), "{missing}");

    let listed = queue
        .list(ListFilter {
            include_terminal: true,
            ..ListFilter::default()
        })
        .unwrap();
    assert_eq!(listed.len(), 2);
    let status = queue.status().unwrap();
    assert_eq!(status.counts.done, 1);
    assert_eq!(status.counts.held, 1);
    let events = queue.events(task.id).unwrap();
    assert!(events
        .iter()
        .any(|event| event.event_type == "task_claimed"));
}

#[test]
fn tokens_gate_access_and_roles() {
    let server = Server::start(Some(auth()));

    let anonymous = server.client(None);
    let denied = anonymous.status().unwrap_err();
    assert!(matches!(denied, QueueError::Transport(_)), "{denied}");
    assert!(denied.to_string().contains("unauthorized"), "{denied}");

    let bad = server.client(Some("wrong-secret-0123456789"));
    assert!(bad
        .status()
        .unwrap_err()
        .to_string()
        .contains("unauthorized"));

    // Health needs no token.
    assert!(anonymous.health().unwrap().ok);

    let human = server.client(Some(HUMAN_SECRET));
    let agent = server.client(Some(AGENT_SECRET));

    // An agent token cannot pretend to be a human: the capture is recorded
    // as an agent named after the token.
    let task = agent
        .capture(capture("Agent captured", Actor::human(None)))
        .unwrap();
    let events = human.events(task.id).unwrap();
    let created = events
        .iter()
        .find(|event| event.event_type == "task_created")
        .unwrap();
    assert_eq!(created.actor_type, ActorKind::Agent.as_str());
    assert_eq!(created.actor_id.as_deref(), Some("vps-agent"));

    // Only humans mark work ready.
    let forbidden = agent
        .mark_ready(ReadyRequest {
            task_id: task.id,
            actor: Actor::human(None),
        })
        .unwrap_err();
    assert!(forbidden.to_string().contains("forbidden"), "{forbidden}");
    assert_eq!(human.get(task.id).unwrap().task.status, TaskStatus::Held);

    human
        .mark_ready(ReadyRequest {
            task_id: task.id,
            actor: Actor::human(None),
        })
        .unwrap();
    let claimed = agent.claim_next(ClaimRequest::new("vps-agent")).unwrap();
    assert!(claimed.found);

    let reopened = agent.reopen(task.id, Actor::human(None)).unwrap_err();
    assert!(reopened.to_string().contains("forbidden"), "{reopened}");
}

#[test]
fn http_and_mcp_claim_filter_tags_and_keep_the_client_host() {
    use q_core::{FailRequest, NoteRequest};
    use serde_json::{json, Value};

    let server = Server::start(None);
    let queue = server.client(None);
    let mut rust = capture("parser", Actor::human(None));
    rust.hold = false;
    rust.tags = vec!["rust".into()];
    let rust = queue.capture(rust).unwrap();
    let mut docs = capture("guide", Actor::human(None));
    docs.hold = false;
    docs.tags = vec!["docs".into()];
    let docs = queue.capture(docs).unwrap();

    let mut request = ClaimRequest::new("remote-agent");
    request.agent_model = Some("opus".into());
    request.agent_host = Some("remote-worker-host".into());
    request.tags = vec!["Rust".into()];
    let claimed = queue.claim_next(request).unwrap();
    assert!(claimed.found);
    assert_eq!(claimed.task.unwrap().task.id, rust.id);
    let lease = claimed.claim.unwrap();
    assert_eq!(lease.agent_host.as_deref(), Some("remote-worker-host"));
    assert_eq!(lease.agent_model.as_deref(), Some("opus"));
    let stored = queue.get(rust.id).unwrap().claim.unwrap();
    assert_eq!(stored.agent_host.as_deref(), Some("remote-worker-host"));
    assert_ne!(
        stored.agent_host.as_deref(),
        q_core::local_hostname().as_deref()
    );

    let noted = queue
        .note(NoteRequest {
            task_id: rust.id,
            claim_token: lease.token.clone(),
            message: "running tests".into(),
            actor: Actor::agent("remote-agent"),
        })
        .unwrap();
    assert!(noted.events.iter().any(|event| {
        event.event_type == q_core::NOTE_EVENT && event.payload["message"] == "running tests"
    }));
    let listed = queue
        .list(ListFilter {
            tags: vec!["rust".into()],
            ..ListFilter::default()
        })
        .unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].latest_note.as_deref(), Some("running tests"));

    let failed = queue
        .fail(FailRequest {
            task_id: rust.id,
            claim_token: lease.token,
            note: Some("tests failed".into()),
            actor: Actor::agent("remote-agent"),
        })
        .unwrap();
    assert_eq!(failed.status, TaskStatus::Ready);
    assert_eq!(failed.failure_count, 1);

    let response = ureq::post(&format!("{}/mcp", server.url))
        .set("Accept", "application/json, text/event-stream")
        .send_json(json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": {
                "name": "queue_claim_next",
                "arguments": {"agent_id": "mcp-remote", "tags": ["docs"]}
            }
        }))
        .unwrap();
    let body: Value = response.into_json().unwrap();
    let text = body["result"]["content"][0]["text"].as_str().unwrap();
    let mcp_claim: Value = serde_json::from_str(text).unwrap();
    assert_eq!(mcp_claim["task"]["id"], docs.id);
    assert!(
        mcp_claim["claim"].get("agent_host").is_none(),
        "HTTP MCP must not stamp the server hostname: {mcp_claim}"
    );
}

#[test]
fn http_escalate_is_not_claimable_until_a_human_marks_it_ready() {
    use q_core::EscalateRequest;

    let server = Server::start(Some(auth()));
    let human = server.client(Some(HUMAN_SECRET));
    let agent = server.client(Some(AGENT_SECRET));

    let mut task = capture("too big for the agent", Actor::human(None));
    task.hold = false;
    let task = human.capture(task).unwrap();
    let claimed = agent.claim_next(ClaimRequest::new("vps-agent")).unwrap();
    assert!(claimed.found);
    let token = claimed.claim.unwrap().token;

    let escalated = agent
        .escalate(EscalateRequest {
            task_id: task.id,
            claim_token: token,
            reason: "needs a human design".into(),
            actor: Actor::agent("vps-agent"),
        })
        .unwrap();
    assert_eq!(escalated.status, TaskStatus::Escalated);
    assert_eq!(
        escalated.escalated_reason.as_deref(),
        Some("needs a human design")
    );
    assert_eq!(escalated.escalated_by.as_deref(), Some("agent:vps-agent"));
    assert!(escalated.escalated_at.is_some());
    assert!(!agent.get(task.id).unwrap().claim.unwrap().active);

    assert!(
        !agent
            .claim_next(ClaimRequest::new("vps-agent"))
            .unwrap()
            .found
    );
    let listed = agent
        .list(ListFilter {
            status: Some(TaskStatus::Escalated),
            ..ListFilter::default()
        })
        .unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].id, task.id);

    let forbidden = agent
        .mark_ready(ReadyRequest {
            task_id: task.id,
            actor: Actor::agent("vps-agent"),
        })
        .unwrap_err();
    assert!(forbidden.to_string().contains("forbidden"), "{forbidden}");
    assert_eq!(
        human.get(task.id).unwrap().task.status,
        TaskStatus::Escalated
    );

    let ready = human
        .mark_ready(ReadyRequest {
            task_id: task.id,
            actor: Actor::human(None),
        })
        .unwrap();
    assert_eq!(ready.task.status, TaskStatus::Ready);
    assert_eq!(ready.task.escalated_reason, None);
    assert_eq!(ready.task.escalated_by, None);
    assert_eq!(ready.task.escalated_at, None);
    let again = agent.claim_next(ClaimRequest::new("vps-agent")).unwrap();
    assert!(again.found);
    assert_eq!(again.task.unwrap().task.id, task.id);
}

#[test]
fn serve_sweep_releases_an_expired_lease() {
    let path = temp_db();
    let queue = Queue::open(&path).unwrap();
    let mut task = capture("expired", Actor::human(None));
    task.hold = false;
    let task = queue.capture(task).unwrap();
    let claimed = queue.claim_next(ClaimRequest::new("agent-1")).unwrap();
    assert!(claimed.found);
    drop(queue);
    let conn = rusqlite::Connection::open(&path).unwrap();
    conn.execute(
        "UPDATE claims SET lease_expires_at = '2000-01-01T00:00:00Z' WHERE task_id = ?1 AND released_at IS NULL",
        rusqlite::params![task.id],
    )
    .unwrap();
    drop(conn);

    let service: Arc<dyn QueueService> = Arc::new(Queue::open(&path).unwrap());
    let server = Server::start_on(service, None, std::time::Duration::from_millis(50));
    let client = server.client(None);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        let detail = client.get(task.id).unwrap();
        if detail.task.status == TaskStatus::Ready {
            assert!(detail.events.iter().any(|event| {
                event.event_type == "task_recovered"
                    && event.payload["reason"] == q_core::LEASE_EXPIRED_REASON
            }));
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "sweep did not release the claim"
        );
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}

#[test]
fn unreachable_server_is_a_transport_error() {
    let queue = RemoteQueue::new("http://127.0.0.1:9", None).unwrap();
    let error = queue.status().unwrap_err();
    assert!(matches!(error, QueueError::Transport(_)), "{error}");
    assert!(error.to_string().contains("cannot reach"), "{error}");
    assert!(RemoteQueue::new("ftp://x", None).is_err());
}
