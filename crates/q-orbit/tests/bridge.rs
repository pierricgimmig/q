//! Drive the bridge against a real queue and a mock Orbit over loopback.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use q_core::{
    Actor, CaptureRequest, ClaimRequest, CompleteRequest, LogRequest, QueueService, RiskLevel,
    StartRequest, TaskKind, TaskStatus,
};
use q_orbit::bridge::{BatchReport, Bridge, BridgeOptions};
use q_orbit::mapper::{task_pid, Q_PID_BASE};
use q_orbit::wire::EventsBody;
use q_orbit::{MapperOptions, OrbitClient};
use q_store::Queue;

fn temp_db() -> PathBuf {
    std::env::temp_dir().join(format!("q-orbit-{}.db", uuid::Uuid::new_v4()))
}

/// A stand-in for orbit-service: records every `/api/events` body and
/// answers like the real endpoint. `/api/status` answers `{}`.
struct MockOrbit {
    url: String,
    bodies: Arc<Mutex<Vec<EventsBody>>>,
    fail: Arc<Mutex<bool>>,
}

impl MockOrbit {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let bodies: Arc<Mutex<Vec<EventsBody>>> = Arc::default();
        let fail = Arc::new(Mutex::new(false));
        let seen = bodies.clone();
        let failing = fail.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut request_line = String::new();
                if reader.read_line(&mut request_line).is_err() {
                    continue;
                }
                let mut length = 0usize;
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).is_err() || line == "\r\n" || line.is_empty() {
                        break;
                    }
                    if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                        length = value.trim().parse().unwrap_or(0);
                    }
                }
                let mut body = vec![0u8; length];
                if length > 0 {
                    reader.read_exact(&mut body).unwrap();
                }
                let path = request_line.split_whitespace().nth(1).unwrap_or("");
                let (status, reply) = if *failing.lock().unwrap() {
                    ("503 Service Unavailable", "down".to_string())
                } else if path == "/api/status" {
                    ("200 OK", r#"{"capturing":false}"#.to_string())
                } else if path == "/api/events" {
                    let parsed: EventsBody = serde_json::from_slice(&body).expect("events body");
                    let accepted = parsed.event_count() as u64;
                    let named = (parsed.processes.len() + parsed.threads.len()) as u64;
                    seen.lock().unwrap().push(parsed);
                    (
                        "200 OK",
                        format!(
                            r#"{{"accepted":{accepted},"dropped_before_start":0,"named":{named},"monotonic_now_ns":1,"capture_start_ns":0}}"#
                        ),
                    )
                } else {
                    ("404 Not Found", "no".to_string())
                };
                let _ = write!(
                    stream,
                    "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{reply}",
                    reply.len()
                );
                let _ = stream.flush();
            }
        });
        Self { url, bodies, fail }
    }

    fn bodies(&self) -> Vec<EventsBody> {
        self.bodies.lock().unwrap().clone()
    }

    fn set_failing(&self, failing: bool) {
        *self.fail.lock().unwrap() = failing;
    }
}

fn capture(title: &str) -> CaptureRequest {
    CaptureRequest {
        title: title.into(),
        body: None,
        kind: TaskKind::Implementation,
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
        actor: Actor::human(Some("pierric".into())),
        context_source: None,
        hold: false,
        tags: vec![],
    }
}

/// A span, or the mark a span becomes when it opens and closes within the
/// same second (the log keeps whole seconds).
fn marked(body: &EventsBody, name: &str, tid: u32) -> bool {
    body.spans.iter().any(|s| s.name == name && s.tid == tid)
        || body.instants.iter().any(|i| i.name == name && i.tid == tid)
}

fn options() -> BridgeOptions {
    BridgeOptions {
        history: None,
        mapper: MapperOptions {
            segment: None,
            ..MapperOptions::default()
        },
        once: true,
        ..BridgeOptions::default()
    }
}

#[test]
fn a_claimed_task_reaches_orbit_as_a_process_with_an_agent_thread() {
    let queue = Queue::open(temp_db()).unwrap();
    let orbit = MockOrbit::start();
    let client = OrbitClient::new(&orbit.url).unwrap();

    let task = queue.capture(capture("Benchmark trace encoding")).unwrap();
    let mut dependent = capture("Ship the report");
    dependent.dependencies = vec![task.id];
    let dependent = queue.capture(dependent).unwrap();
    let claimed = queue.claim_next(ClaimRequest::new("claude-1")).unwrap();
    let token = claimed.claim.unwrap().token;
    queue
        .start(StartRequest {
            task_id: task.id,
            claim_token: token.clone(),
            branch: Some("agent/task-1".into()),
            worktree_path: None,
            actor: Actor::agent("claude-1"),
        })
        .unwrap();
    queue
        .log(LogRequest {
            task_id: task.id,
            claim_token: Some(token.clone()),
            message: Some("[explore] @begin Survey the API".into()),
            progress: None,
            artifacts: vec![],
            actor: Actor::agent("claude-1"),
        })
        .unwrap();

    let mut batches = Vec::<BatchReport>::new();
    let report = q_orbit::run(&queue, &client, &options(), &|| false, &mut |b| {
        batches.push(b.clone())
    })
    .unwrap();
    assert_eq!(report.batches, 1);
    assert_eq!(report.failed_posts, 0);
    assert_eq!(report.tasks, 2);
    assert_eq!(report.active_claims, 1);
    assert!(report.events_seen >= 5, "{report:?}");
    assert_eq!(batches.len(), 1);
    assert_eq!(batches[0].events as u64, report.pushed_events);

    let bodies = orbit.bodies();
    assert_eq!(bodies.len(), 1);
    let body = &bodies[0];
    let pid = task_pid(task.id);
    assert!(body
        .processes
        .iter()
        .any(|p| p.pid == pid && p.name == format!("#{} Benchmark trace encoding", task.id)));
    assert!(body
        .processes
        .iter()
        .any(|p| p.pid == Q_PID_BASE && p.name == "q queue"));
    assert!(body
        .threads
        .iter()
        .any(|t| t.pid == pid && t.tid == pid + 1 && t.name == "claude-1"));
    assert!(body
        .threads
        .iter()
        .any(|t| t.pid == pid && t.tid == pid + 2 && t.name == "explore"));
    // Finished phases are drawn; the open claim is not drawn yet (no segments).
    assert!(marked(body, "ready", pid), "{:#?}", body.spans);
    assert!(marked(body, "claimed", pid));
    assert!(!marked(body, "claimed", pid + 1));
    assert!(body
        .instants
        .iter()
        .any(|i| i.name == "created" && i.tid == pid));
    assert!(body
        .values
        .iter()
        .any(|v| v.name == "active claims" && v.value == 1.0));
    // The dependent task waits on this one; the edge comes from `get`, not the log.
    assert!(body
        .processes
        .iter()
        .any(|p| p.pid == task_pid(dependent.id)));
    assert_eq!(
        report.open_spans, 6,
        "claimed phase, the dependent's ready phase, claim, in_progress, the explore span, the dependent's wait: {report:?}"
    );
}

#[test]
fn completion_closes_the_claim_and_the_wait_and_a_second_pass_sends_only_the_new_events() {
    let queue = Queue::open(temp_db()).unwrap();
    let orbit = MockOrbit::start();
    let client = OrbitClient::new(&orbit.url).unwrap();
    let task = queue.capture(capture("First")).unwrap();
    let mut second = capture("Second");
    second.dependencies = vec![task.id];
    let second = queue.capture(second).unwrap();
    let token = queue
        .claim_next(ClaimRequest::new("claude-1"))
        .unwrap()
        .claim
        .unwrap()
        .token;

    let mut bridge = Bridge::new(options());
    bridge.poll(&queue).unwrap();
    bridge.push(&client, &mut |_| {});
    let first_seen = bridge.mapper().events_seen();
    assert_eq!(orbit.bodies().len(), 1);

    let detail = queue
        .complete(CompleteRequest {
            task_id: task.id,
            claim_token: Some(token),
            summary: "shipped".into(),
            target: Some(TaskStatus::Done),
            artifacts: vec![],
            actor: Actor::agent("claude-1"),
        })
        .unwrap();
    assert_eq!(detail.task.status, TaskStatus::Done);

    bridge.poll(&queue).unwrap();
    bridge.push(&client, &mut |_| {});
    assert!(bridge.mapper().events_seen() > first_seen);
    let bodies = orbit.bodies();
    assert_eq!(bodies.len(), 2);
    let body = &bodies[1];
    let pid = task_pid(task.id);
    assert!(
        marked(body, "claimed", pid + 1),
        "the closed claim: {body:#?}"
    );
    assert!(body
        .instants
        .iter()
        .any(|i| i.name == "completed: shipped" && i.tid == pid + 1));
    assert!(body
        .instants
        .iter()
        .any(|i| i.name == "done" && i.tid == pid));
    let wait_name = format!("waits on #{}", task.id);
    assert!(
        marked(body, &wait_name, task_pid(second.id)),
        "the wait closes when the dependency is done: {body:#?}"
    );
    assert!(body
        .values
        .iter()
        .any(|v| v.name == "active claims" && v.value == 0.0));
    // Only the new events went out; the first task's `created` was in batch one.
    assert!(!body.instants.iter().any(|i| i.name == "created"));
    assert_eq!(
        bridge.mapper().open_spans(),
        1,
        "the dependent stays in the ready phase"
    );
}

#[test]
fn an_unreachable_orbit_keeps_the_batch_for_the_next_push() {
    let queue = Queue::open(temp_db()).unwrap();
    let orbit = MockOrbit::start();
    let client = OrbitClient::new(&orbit.url).unwrap();
    queue.capture(capture("Only one")).unwrap();

    let mut bridge = Bridge::new(options());
    bridge.poll(&queue).unwrap();
    orbit.set_failing(true);
    let mut pushes = 0;
    bridge.push(&client, &mut |_| pushes += 1);
    assert_eq!(pushes, 0);
    assert!(orbit.bodies().is_empty());

    orbit.set_failing(false);
    bridge.poll(&queue).unwrap();
    bridge.push(&client, &mut |_| pushes += 1);
    assert_eq!(pushes, 1);
    let bodies = orbit.bodies();
    assert_eq!(bodies.len(), 1);
    assert!(bodies[0].instants.iter().any(|i| i.name == "created"));
    let report = bridge.report(&orbit.url);
    assert_eq!(report.failed_posts, 1);
    assert_eq!(report.batches, 1);
}

#[test]
fn run_fails_fast_when_orbit_is_not_there() {
    let queue = Queue::open(temp_db()).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    drop(listener);
    let client = OrbitClient::new(&url).unwrap();
    let err = q_orbit::run(&queue, &client, &options(), &|| false, &mut |_| {}).unwrap_err();
    assert!(err.to_string().contains("cannot reach Orbit"), "{err}");
    assert!(
        OrbitClient::new("127.0.0.1:1").is_err(),
        "a scheme is required"
    );
}
