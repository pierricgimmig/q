//! Drive a real `q serve` over a loopback socket through `RemoteQueue`.

use std::path::PathBuf;
use std::sync::Arc;

use q_core::{
    Actor, ActorKind, CaptureRequest, ClaimRequest, CompleteRequest, HeartbeatRequest, ListFilter,
    QueueError, QueueService, ReadyRequest, RiskLevel, StartRequest, TaskKind, TaskStatus,
};
use q_http::{serve_on, AuthConfig, RemoteQueue};
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
                serve_on(queue, listener, auth, async move {
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
    assert_eq!(task.status, TaskStatus::Inbox);

    // Inbox work is not claimable.
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
            actor: Actor::agent("agent-1"),
        })
        .unwrap();
    assert!(claim.lease_expires_at >= lease.lease_expires_at);

    let wrong = queue
        .heartbeat(HeartbeatRequest {
            task_id: task.id,
            claim_token: "not-the-token".into(),
            lease: None,
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

    let missing = queue.get(9999).unwrap_err();
    assert!(matches!(missing, QueueError::NotFound(9999)), "{missing}");

    let listed = queue
        .list(ListFilter {
            include_terminal: true,
            ..ListFilter::default()
        })
        .unwrap();
    assert_eq!(listed.len(), 1);
    let status = queue.status().unwrap();
    assert_eq!(status.counts.done, 1);
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
    assert_eq!(human.get(task.id).unwrap().task.status, TaskStatus::Inbox);

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
fn unreachable_server_is_a_transport_error() {
    let queue = RemoteQueue::new("http://127.0.0.1:9", None).unwrap();
    let error = queue.status().unwrap_err();
    assert!(matches!(error, QueueError::Transport(_)), "{error}");
    assert!(error.to_string().contains("cannot reach"), "{error}");
    assert!(RemoteQueue::new("ftp://x", None).is_err());
}
