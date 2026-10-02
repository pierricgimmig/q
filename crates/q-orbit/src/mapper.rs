//! Queue events in, Orbit timeline records out. No I/O.
//!
//! Identity: a task is process `pid = Q_PID_BASE + id * TASK_STRIDE`, whose
//! main thread has `tid = pid`. Every agent (and every named sub-thread) that
//! touches the task gets a slot `1..TASK_STRIDE` above that, so tids are
//! unique across tasks. The queue itself is process `Q_PID_BASE` with one
//! value lane, `active claims`.
//!
//! Depths on the task's main thread: 0 status phase, 1 human and system
//! instants, 2.. `waits on #dep` spans. Depths on an agent thread: 0
//! `claimed`, 1 `in_progress`, 2.. notes, heartbeats, artifacts, user
//! spans from `@begin`/`@end` notes, and the processes the agent launches
//! from `@exec <command>` / `@exit <code> <command>` notes (what `q exec`
//! writes): a span `$ <command>` with an `exit <code>` mark where it ends.
//!
//! Open spans cannot be drawn until they end, since the ring in Orbit is
//! append-only. With [`MapperOptions::segment`] set, [`Mapper::tick`] emits
//! each open span as adjacent segments so it grows on screen while the work
//! runs; the close then emits the last segment.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use q_core::{Event, TaskStatus};
use time::OffsetDateTime;

use crate::wire::{EventsBody, Instant, ProcessName, Span, SpanTrack, ThreadName, Value};

/// First pid the bridge uses. Far above any real pid, below Orbit's own
/// synthetic ids (`AGENT_PID` is `0xA6E7_0000`).
pub const Q_PID_BASE: u32 = 0x7100_0000;
/// Ids per task: the main thread plus up to 255 agent or sub-agent threads.
pub const TASK_STRIDE: u32 = 256;
/// Name of the queue-wide process.
pub const QUEUE_PROCESS: &str = "q queue";
/// Value lane on the queue process.
pub const ACTIVE_CLAIMS: &str = "active claims";
/// Value lane on a task for reported progress.
pub const PROGRESS_LANE: &str = "progress %";

const MAIN_INSTANT_DEPTH: u8 = 1;
const WAITS_DEPTH: u8 = 2;
const AGENT_INSTANT_DEPTH: u8 = 2;

/// Marker that opens a nested span in a log note: `@begin label`.
pub const BEGIN_MARKER: &str = "@begin";
/// Marker that closes the innermost open span: `@end`.
pub const END_MARKER: &str = "@end";
/// Marker that opens a process span: `@exec <command line>`.
pub const EXEC_MARKER: &str = "@exec";
/// Marker that closes a process span: `@exit <code> [(<duration>)] [<command line>]`.
pub const EXIT_MARKER: &str = "@exit";
/// Prefix of a process span's name, before the command line.
pub const PROCESS_PREFIX: &str = "$ ";

#[derive(Debug, Clone)]
pub struct MapperOptions {
    /// Emit each open span as segments this long on [`Mapper::tick`]. `None`
    /// draws a span only when it ends.
    pub segment: Option<Duration>,
    /// Characters of the title kept in the process name.
    pub title_chars: usize,
    /// Characters of a note kept in its instant name.
    pub note_chars: usize,
    /// Characters of a command line kept in a process span's name.
    pub command_chars: usize,
}

impl Default for MapperOptions {
    fn default() -> Self {
        Self {
            segment: Some(Duration::from_secs(10)),
            title_chars: 48,
            note_chars: 60,
            command_chars: 120,
        }
    }
}

/// What the bridge fetches about a task the log alone does not say: its
/// title (for tasks created before the tail began) and its dependencies.
#[derive(Debug, Clone)]
pub struct TaskInfo {
    pub id: i64,
    pub title: String,
    pub created_at: OffsetDateTime,
    /// `(dependency id, its status)`.
    pub dependencies: Vec<(i64, TaskStatus)>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum SpanKey {
    Phase(i64),
    Claim(i64),
    InProgress(i64),
    WaitsOn { task: i64, dep: i64 },
    User { task: i64, tid: u32, seq: u64 },
}

#[derive(Debug, Clone)]
struct OpenSpan {
    key: SpanKey,
    pid: u32,
    tid: u32,
    name: String,
    depth: u8,
    start_ns: u64,
    emitted_to_ns: u64,
}

/// An open span from a note on one thread: a `@begin` span, or a process
/// span whose command line an `@exit` note names to close it.
#[derive(Debug, Clone)]
struct StackEntry {
    key: SpanKey,
    command: Option<String>,
}

#[derive(Debug, Default)]
struct TaskState {
    pid: u32,
    title: Option<String>,
    /// Thread key (agent id, or `agent/sub`) to tid.
    slots: HashMap<String, u32>,
    /// Thread display names by tid.
    thread_names: HashMap<u32, String>,
    /// The agent holding the active claim.
    agent: Option<String>,
    /// Open `@begin` and `@exec` spans per thread, innermost last.
    user_stacks: HashMap<u32, Vec<StackEntry>>,
    /// Dependencies already turned into wait spans.
    deps_seen: HashSet<i64>,
}

/// Stateful event-to-timeline mapper.
#[derive(Debug)]
pub struct Mapper {
    options: MapperOptions,
    tasks: HashMap<i64, TaskState>,
    open: Vec<OpenSpan>,
    active_claims: HashSet<i64>,
    user_seq: u64,
    queue_named: bool,
    events_seen: u64,
}

impl Default for Mapper {
    fn default() -> Self {
        Self::new(MapperOptions::default())
    }
}

/// Process id for a task.
pub fn task_pid(task_id: i64) -> u32 {
    let id = u32::try_from(task_id).unwrap_or(0);
    Q_PID_BASE.saturating_add(id.saturating_mul(TASK_STRIDE))
}

/// Unix nanoseconds for a timestamp; 0 before the epoch.
pub fn unix_ns(at: OffsetDateTime) -> u64 {
    u64::try_from(at.unix_timestamp_nanos()).unwrap_or(0)
}

impl Mapper {
    pub fn new(options: MapperOptions) -> Self {
        Self {
            options,
            tasks: HashMap::new(),
            open: Vec::new(),
            active_claims: HashSet::new(),
            user_seq: 0,
            queue_named: false,
            events_seen: 0,
        }
    }

    /// True once the mapper has seen this task (in an event or in
    /// [`Mapper::task_info`]).
    pub fn knows_task(&self, task_id: i64) -> bool {
        self.tasks.contains_key(&task_id)
    }

    /// True once the task has a title.
    pub fn has_title(&self, task_id: i64) -> bool {
        self.tasks
            .get(&task_id)
            .map(|task| task.title.is_some())
            .unwrap_or(false)
    }

    /// Spans currently open (claims, phases, waits, user spans).
    pub fn open_spans(&self) -> usize {
        self.open.len()
    }

    /// Events applied so far.
    pub fn events_seen(&self) -> u64 {
        self.events_seen
    }

    /// Tasks with an active claim.
    pub fn active_claims(&self) -> usize {
        self.active_claims.len()
    }

    /// Every process and thread name known, for a (re)send to Orbit.
    pub fn names(&self) -> EventsBody {
        let mut body = EventsBody::unix();
        if self.queue_named {
            body.processes.push(ProcessName {
                pid: Q_PID_BASE,
                name: QUEUE_PROCESS.into(),
            });
            body.threads.push(ThreadName {
                pid: Q_PID_BASE,
                tid: Q_PID_BASE,
                name: ACTIVE_CLAIMS.into(),
            });
        }
        let mut ids: Vec<&i64> = self.tasks.keys().collect();
        ids.sort();
        for id in ids {
            let task = &self.tasks[id];
            body.processes.push(ProcessName {
                pid: task.pid,
                name: process_name(*id, task.title.as_deref(), self.options.title_chars),
            });
            body.threads.push(ThreadName {
                pid: task.pid,
                tid: task.pid,
                name: "task".into(),
            });
            let mut tids: Vec<(&u32, &String)> = task.thread_names.iter().collect();
            tids.sort();
            for (tid, name) in tids {
                body.threads.push(ThreadName {
                    pid: task.pid,
                    tid: *tid,
                    name: name.clone(),
                });
            }
        }
        body
    }

    /// Register a task's title and dependencies. Opens a `waits on #dep`
    /// span for each dependency that is not done. With `emit` false the
    /// state is updated and nothing is returned.
    pub fn task_info(&mut self, info: TaskInfo, emit: bool) -> EventsBody {
        let mut out = EventsBody::unix();
        let title_chars = self.options.title_chars;
        let task = self.task(info.id);
        let renamed = task.title.as_deref() != Some(info.title.as_str());
        task.title = Some(info.title.clone());
        let pid = task.pid;
        if renamed && emit {
            out.processes.push(ProcessName {
                pid,
                name: process_name(info.id, Some(&info.title), title_chars),
            });
            out.threads.push(ThreadName {
                pid,
                tid: pid,
                name: "task".into(),
            });
        }
        let start_ns = unix_ns(info.created_at);
        for (dep, status) in info.dependencies {
            let task = self.task(info.id);
            if !task.deps_seen.insert(dep) {
                continue;
            }
            if matches!(status, TaskStatus::Done) {
                continue;
            }
            let depth = WAITS_DEPTH.saturating_add(task.deps_seen.len().saturating_sub(1) as u8);
            self.open_span(
                SpanKey::WaitsOn { task: info.id, dep },
                pid,
                pid,
                format!("waits on #{dep}"),
                depth,
                start_ns,
            );
        }
        out
    }

    /// Map one queue event. With `emit` false, only the state changes; open
    /// spans still start, so they are drawn from their real start on the
    /// next tick.
    pub fn apply(&mut self, event: &Event, emit: bool) -> EventsBody {
        self.events_seen += 1;
        let mut out = EventsBody::unix();
        let Some(task_id) = event.task_id else {
            return out;
        };
        let ts = unix_ns(event.created_at);
        let payload = &event.payload;
        let field = |key: &str| {
            payload
                .get(key)
                .and_then(|v| v.as_str())
                .map(str::to_string)
        };

        let is_new = !self.tasks.contains_key(&task_id);
        let pid = self.task(task_id).pid;
        if is_new {
            let title = field("title");
            self.task(task_id).title = title;
            if emit {
                self.name_task(task_id, &mut out);
            }
        } else if event.event_type == "task_created" {
            if let Some(title) = field("title") {
                self.task(task_id).title = Some(title);
                if emit {
                    self.name_task(task_id, &mut out);
                }
            }
        }
        if !self.queue_named {
            self.queue_named = true;
            if emit {
                out.processes.push(ProcessName {
                    pid: Q_PID_BASE,
                    name: QUEUE_PROCESS.into(),
                });
                out.threads.push(ThreadName {
                    pid: Q_PID_BASE,
                    tid: Q_PID_BASE,
                    name: ACTIVE_CLAIMS.into(),
                });
            }
        }

        let actor_is_agent = event.actor_type == "agent";
        let actor = event.actor_id.clone().filter(|id| !id.is_empty());
        let from = field("from").and_then(|s| TaskStatus::parse(&s).ok());
        let to = field("to").and_then(|s| TaskStatus::parse(&s).ok());
        let short = event
            .event_type
            .strip_prefix("task_")
            .unwrap_or(&event.event_type)
            .to_string();

        // Status phases on the main thread. A transition uses `to`. Capture
        // now lands directly in `ready` (or `held`) and records that on
        // `task_created` instead of a later `task_ready` event.
        if let Some(to) = to {
            self.close_span(&SpanKey::Phase(task_id), ts, &mut out, emit);
            if matches!(to, TaskStatus::Done | TaskStatus::Cancelled) {
                push_instant(&mut out, emit, pid, pid, to.as_str(), ts, 0);
                self.close_waits_on(task_id, ts, &mut out, emit);
            } else {
                self.open_span(SpanKey::Phase(task_id), pid, pid, to.as_str().into(), 0, ts);
            }
        } else if event.event_type == "task_created" {
            if let Some(status) = field("status").and_then(|s| TaskStatus::parse(&s).ok()) {
                if !matches!(status, TaskStatus::Done | TaskStatus::Cancelled) {
                    self.open_span(
                        SpanKey::Phase(task_id),
                        pid,
                        pid,
                        status.as_str().into(),
                        0,
                        ts,
                    );
                }
            }
        }

        // Claims: the agent thread.
        let claim_started = matches!(to, Some(TaskStatus::Claimed));
        let claim_ended = matches!(from, Some(TaskStatus::Claimed | TaskStatus::InProgress))
            && !matches!(to, Some(TaskStatus::Claimed | TaskStatus::InProgress));
        if claim_started {
            let agent = field("agent_id")
                .or_else(|| actor.clone())
                .unwrap_or_else(|| "agent".into());
            let tid = self.thread(task_id, &agent, &agent, &mut out, emit);
            self.task(task_id).agent = Some(agent);
            self.open_span(SpanKey::Claim(task_id), pid, tid, "claimed".into(), 0, ts);
            if self.active_claims.insert(task_id) {
                self.push_active_claims(&mut out, emit, ts);
            }
        }
        if matches!(to, Some(TaskStatus::InProgress)) {
            if let Some(agent) = self.task(task_id).agent.clone() {
                let tid = self.thread(task_id, &agent, &agent, &mut out, emit);
                let name = match field("branch") {
                    Some(branch) => format!("in_progress {branch}"),
                    None => "in_progress".into(),
                };
                self.open_span(SpanKey::InProgress(task_id), pid, tid, name, 1, ts);
            }
        }
        if claim_ended {
            self.close_span(&SpanKey::InProgress(task_id), ts, &mut out, emit);
            self.close_span(&SpanKey::Claim(task_id), ts, &mut out, emit);
            self.close_user_spans(task_id, ts, &mut out, emit);
            if let Some(agent) = self.task(task_id).agent.take() {
                let tid = self.thread(task_id, &agent, &agent, &mut out, emit);
                let mut name = short.clone();
                if let Some(summary) = field("summary").filter(|s| !s.is_empty()) {
                    name = format!("{short}: {}", trim(&summary, self.options.note_chars));
                }
                push_instant(&mut out, emit, pid, tid, &name, ts, AGENT_INSTANT_DEPTH);
            }
            if self.active_claims.remove(&task_id) {
                self.push_active_claims(&mut out, emit, ts);
            }
        }

        // The rest: heartbeats, notes, artifacts, and everything else.
        let agent_tid = |this: &mut Self, out: &mut EventsBody| -> Option<u32> {
            let agent = actor
                .clone()
                .filter(|_| actor_is_agent)
                .or_else(|| this.task(task_id).agent.clone())?;
            Some(this.thread(task_id, &agent, &agent, out, emit))
        };
        match event.event_type.as_str() {
            "task_heartbeat" => {
                if let Some(tid) = agent_tid(self, &mut out) {
                    push_instant(
                        &mut out,
                        emit,
                        pid,
                        tid,
                        "heartbeat",
                        ts,
                        AGENT_INSTANT_DEPTH,
                    );
                }
            }
            "task_note" => {
                if let Some(progress) = payload.get("progress").and_then(|v| v.as_f64()) {
                    push_value(&mut out, emit, pid, pid, PROGRESS_LANE, ts, progress);
                }
                if let Some(message) = field("message") {
                    self.apply_note(
                        task_id,
                        &message,
                        actor.as_deref(),
                        actor_is_agent,
                        ts,
                        &mut out,
                        emit,
                    );
                }
            }
            "artifact_added" => {
                let kind = field("kind").unwrap_or_else(|| "artifact".into());
                let value = field("value").unwrap_or_default();
                let name = format!("artifact {kind}: {}", trim(&value, self.options.note_chars));
                match (actor_is_agent, agent_tid(self, &mut out)) {
                    (true, Some(tid)) => {
                        push_instant(&mut out, emit, pid, tid, &name, ts, AGENT_INSTANT_DEPTH)
                    }
                    _ => push_instant(&mut out, emit, pid, pid, &name, ts, MAIN_INSTANT_DEPTH),
                }
            }
            _ if to.is_some() && (claim_started || claim_ended) => {}
            _ => {
                // Status moves without a claim and everything else land on
                // the main thread as instants: created, ready, edited, ...
                let name = match field("branch") {
                    Some(branch) if event.event_type == "task_started" => {
                        format!("{short} {branch}")
                    }
                    _ => short.clone(),
                };
                push_instant(&mut out, emit, pid, pid, &name, ts, MAIN_INSTANT_DEPTH);
            }
        }
        out
    }

    /// Emit a segment for every open span older than the segment length.
    pub fn tick(&mut self, now: OffsetDateTime) -> EventsBody {
        let mut out = EventsBody::unix();
        let Some(segment) = self.options.segment else {
            return out;
        };
        let now_ns = unix_ns(now);
        let segment_ns = segment.as_nanos() as u64;
        for span in &mut self.open {
            if now_ns >= span.emitted_to_ns.saturating_add(segment_ns) {
                out.spans.push(Span {
                    pid: span.pid,
                    tid: span.tid,
                    name: span.name.clone(),
                    start_ns: span.emitted_to_ns,
                    duration_ns: now_ns - span.emitted_to_ns,
                    depth: span.depth,
                    track: SpanTrack::Scope,
                });
                span.emitted_to_ns = now_ns;
            }
        }
        out
    }

    fn task(&mut self, task_id: i64) -> &mut TaskState {
        self.tasks.entry(task_id).or_insert_with(|| TaskState {
            pid: task_pid(task_id),
            ..TaskState::default()
        })
    }

    fn name_task(&mut self, task_id: i64, out: &mut EventsBody) {
        let title_chars = self.options.title_chars;
        let task = self.task(task_id);
        out.processes.push(ProcessName {
            pid: task.pid,
            name: process_name(task_id, task.title.as_deref(), title_chars),
        });
        out.threads.push(ThreadName {
            pid: task.pid,
            tid: task.pid,
            name: "task".into(),
        });
    }

    /// The tid for a thread key inside a task, naming it on first use.
    fn thread(
        &mut self,
        task_id: i64,
        key: &str,
        display: &str,
        out: &mut EventsBody,
        emit: bool,
    ) -> u32 {
        let task = self.task(task_id);
        if let Some(tid) = task.slots.get(key) {
            return *tid;
        }
        let slot = (task.slots.len() as u32 + 1).min(TASK_STRIDE - 1);
        let tid = task.pid + slot;
        task.slots.insert(key.to_string(), tid);
        task.thread_names.insert(tid, display.to_string());
        if emit {
            out.threads.push(ThreadName {
                pid: task.pid,
                tid,
                name: display.to_string(),
            });
        }
        tid
    }

    fn open_span(
        &mut self,
        key: SpanKey,
        pid: u32,
        tid: u32,
        name: String,
        depth: u8,
        start_ns: u64,
    ) {
        self.open.retain(|span| span.key != key);
        self.open.push(OpenSpan {
            key,
            pid,
            tid,
            name,
            depth,
            start_ns,
            emitted_to_ns: start_ns,
        });
    }

    fn close_span(&mut self, key: &SpanKey, end_ns: u64, out: &mut EventsBody, emit: bool) {
        let Some(index) = self.open.iter().position(|span| &span.key == key) else {
            return;
        };
        let span = self.open.remove(index);
        if emit && end_ns > span.emitted_to_ns {
            out.spans.push(Span {
                pid: span.pid,
                tid: span.tid,
                name: span.name,
                start_ns: span.emitted_to_ns,
                duration_ns: end_ns - span.emitted_to_ns,
                depth: span.depth,
                track: SpanTrack::Scope,
            });
        } else if emit && end_ns == span.start_ns {
            // Opened and closed in the same second: keep it as a mark.
            out.instants.push(Instant {
                pid: span.pid,
                tid: span.tid,
                name: span.name,
                timestamp_ns: end_ns,
                depth: span.depth,
            });
        }
    }

    fn close_waits_on(&mut self, dep: i64, end_ns: u64, out: &mut EventsBody, emit: bool) {
        let keys: Vec<SpanKey> = self
            .open
            .iter()
            .filter(|span| matches!(span.key, SpanKey::WaitsOn { dep: d, .. } if d == dep))
            .map(|span| span.key.clone())
            .collect();
        for key in keys {
            self.close_span(&key, end_ns, out, emit);
        }
    }

    fn close_user_spans(&mut self, task_id: i64, end_ns: u64, out: &mut EventsBody, emit: bool) {
        let stacks = std::mem::take(&mut self.task(task_id).user_stacks);
        for (_, stack) in stacks {
            for entry in stack.into_iter().rev() {
                self.close_span(&entry.key, end_ns, out, emit);
            }
        }
    }

    fn push_active_claims(&self, out: &mut EventsBody, emit: bool, ts: u64) {
        push_value(
            out,
            emit,
            Q_PID_BASE,
            Q_PID_BASE,
            ACTIVE_CLAIMS,
            ts,
            self.active_claims.len() as f64,
        );
    }

    #[allow(clippy::too_many_arguments)]
    fn apply_note(
        &mut self,
        task_id: i64,
        message: &str,
        actor: Option<&str>,
        actor_is_agent: bool,
        ts: u64,
        out: &mut EventsBody,
        emit: bool,
    ) {
        let pid = self.task(task_id).pid;
        let note = parse_note(message);
        let agent = if actor_is_agent {
            actor.map(str::to_string)
        } else {
            None
        }
        .or_else(|| self.task(task_id).agent.clone());

        // Which thread: a named sub-thread of the agent, the agent, or the
        // task's main thread for a human.
        let (tid, base_depth) = match (&note.thread, &agent, actor_is_agent) {
            (Some(sub), Some(agent), _) => {
                let key = format!("{agent}/{sub}");
                (
                    self.thread(task_id, &key, sub, out, emit),
                    AGENT_INSTANT_DEPTH,
                )
            }
            (Some(sub), None, _) => {
                let key = format!("human/{sub}");
                (
                    self.thread(task_id, &key, sub, out, emit),
                    AGENT_INSTANT_DEPTH,
                )
            }
            (None, Some(agent), true) => (
                self.thread(task_id, agent, agent, out, emit),
                AGENT_INSTANT_DEPTH,
            ),
            _ => (pid, MAIN_INSTANT_DEPTH),
        };
        let stack_len = self
            .task(task_id)
            .user_stacks
            .get(&tid)
            .map(Vec::len)
            .unwrap_or(0) as u8;
        let depth = base_depth.saturating_add(stack_len);
        match note.action {
            NoteAction::Begin(label) => {
                let key = self.push_stack(task_id, tid, None);
                self.open_span(key, pid, tid, label, depth, ts);
            }
            NoteAction::End => {
                let popped = self
                    .task(task_id)
                    .user_stacks
                    .get_mut(&tid)
                    .and_then(Vec::pop);
                match popped {
                    Some(entry) => self.close_span(&entry.key, ts, out, emit),
                    None => push_instant(out, emit, pid, tid, "@end without @begin", ts, depth),
                }
            }
            NoteAction::Exec(command) => {
                let name = format!(
                    "{PROCESS_PREFIX}{}",
                    trim(&command, self.options.command_chars)
                );
                let key = self.push_stack(task_id, tid, Some(command));
                self.open_span(key, pid, tid, name, depth, ts);
            }
            NoteAction::Exit {
                code,
                duration,
                command,
            } => {
                // The most recent open process with this command line, else
                // the most recent open process, else nothing to close.
                let stack = self.task(task_id).user_stacks.entry(tid).or_default();
                let index = stack
                    .iter()
                    .rposition(|entry| entry.command.is_some() && entry.command == command)
                    .or_else(|| stack.iter().rposition(|entry| entry.command.is_some()));
                let popped = index.map(|index| stack.remove(index));
                let mut label = format!("exit {code}");
                if let Some(duration) = duration {
                    label.push_str(&format!(" ({duration})"));
                }
                match popped {
                    Some(entry) => {
                        let span_depth = self
                            .open
                            .iter()
                            .find(|span| span.key == entry.key)
                            .map(|span| span.depth)
                            .unwrap_or(depth);
                        self.close_span(&entry.key, ts, out, emit);
                        push_instant(
                            out,
                            emit,
                            pid,
                            tid,
                            &label,
                            ts,
                            span_depth.saturating_add(1),
                        );
                    }
                    None => {
                        let name = match command {
                            Some(command) => format!(
                                "{label} without @exec: {}",
                                trim(&command, self.options.command_chars)
                            ),
                            None => format!("{label} without @exec"),
                        };
                        push_instant(out, emit, pid, tid, &name, ts, depth);
                    }
                }
            }
            NoteAction::Text(text) => {
                let name = format!("note: {}", trim(&text, self.options.note_chars));
                push_instant(out, emit, pid, tid, &name, ts, depth);
            }
        }
    }

    /// Record a new note-opened span on a thread's stack and return its key.
    fn push_stack(&mut self, task_id: i64, tid: u32, command: Option<String>) -> SpanKey {
        self.user_seq += 1;
        let key = SpanKey::User {
            task: task_id,
            tid,
            seq: self.user_seq,
        };
        self.task(task_id)
            .user_stacks
            .entry(tid)
            .or_default()
            .push(StackEntry {
                key: key.clone(),
                command,
            });
        key
    }
}

/// A parsed log note: optional `[thread]` prefix, then `@begin label`,
/// `@end`, `@exec command`, `@exit code [(duration)] [command]`, or plain
/// text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Note {
    pub thread: Option<String>,
    pub action: NoteAction,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NoteAction {
    Begin(String),
    End,
    /// A process was launched: the command line, whitespace collapsed.
    Exec(String),
    /// A process ended. `code` is the exit status as written (`0`, `101`,
    /// `130`), `duration` the optional parenthesised text after it, and
    /// `command` the command line that follows, if any.
    Exit {
        code: String,
        duration: Option<String>,
        command: Option<String>,
    },
    Text(String),
}

/// Parse the note convention. `[explore] @begin Survey the API` opens a span
/// named `Survey the API` on sub-thread `explore`; `[explore] @end` closes
/// it; `@exec cargo build` opens a process span and `@exit 0 (1.2s) cargo
/// build` closes it; anything else is text. A `[thread]` prefix needs a
/// non-empty name without spaces, otherwise it is text.
pub fn parse_note(message: &str) -> Note {
    let trimmed = message.trim();
    let (thread, rest) = match trimmed.strip_prefix('[') {
        Some(after) => match after.split_once(']') {
            Some((name, rest))
                if !name.trim().is_empty() && !name.trim().contains(char::is_whitespace) =>
            {
                (Some(name.trim().to_string()), rest.trim())
            }
            _ => (None, trimmed),
        },
        None => (None, trimmed),
    };
    let action = if let Some(label) = rest.strip_prefix(BEGIN_MARKER) {
        let label = label.trim();
        if label.is_empty() {
            NoteAction::Begin("span".into())
        } else {
            NoteAction::Begin(collapse(label))
        }
    } else if rest == END_MARKER || rest.starts_with(&format!("{END_MARKER} ")) {
        NoteAction::End
    } else if let Some(command) = marker_rest(rest, EXEC_MARKER) {
        let command = collapse(command);
        if command.is_empty() {
            NoteAction::Text(collapse(rest))
        } else {
            NoteAction::Exec(command)
        }
    } else if let Some(exit) = marker_rest(rest, EXIT_MARKER) {
        parse_exit(exit).unwrap_or_else(|| NoteAction::Text(collapse(rest)))
    } else {
        NoteAction::Text(collapse(rest))
    };
    Note { thread, action }
}

/// The text after `marker` when `text` is the marker alone or the marker
/// followed by whitespace; `@ending` is not `@end`.
fn marker_rest<'a>(text: &'a str, marker: &str) -> Option<&'a str> {
    let rest = text.strip_prefix(marker)?;
    if rest.is_empty() || rest.starts_with(char::is_whitespace) {
        Some(rest)
    } else {
        None
    }
}

/// `<code> [(<duration>)] [<command line>]`. The code is one token of
/// digits, an optional leading minus, or `?` for unknown.
fn parse_exit(text: &str) -> Option<NoteAction> {
    let mut words = text.split_whitespace();
    let code = words.next()?;
    let digits = code.strip_prefix('-').unwrap_or(code);
    let numeric = !digits.is_empty() && digits.chars().all(|c| c.is_ascii_digit());
    if code != "?" && !numeric {
        return None;
    }
    let mut rest: Vec<&str> = words.collect();
    let duration = match rest.first() {
        Some(word) if word.starts_with('(') && word.ends_with(')') && word.len() > 2 => {
            let word = rest.remove(0);
            Some(word[1..word.len() - 1].to_string())
        }
        _ => None,
    };
    let command = if rest.is_empty() {
        None
    } else {
        Some(rest.join(" "))
    };
    Some(NoteAction::Exit {
        code: code.to_string(),
        duration,
        command,
    })
}

fn collapse(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn trim(text: &str, max_chars: usize) -> String {
    let text = collapse(text);
    if text.chars().count() <= max_chars {
        return text;
    }
    let mut out: String = text.chars().take(max_chars.saturating_sub(1)).collect();
    out.push('…');
    out
}

fn process_name(task_id: i64, title: Option<&str>, title_chars: usize) -> String {
    match title {
        Some(title) => format!("#{task_id} {}", trim(title, title_chars)),
        None => format!("#{task_id}"),
    }
}

fn push_instant(
    out: &mut EventsBody,
    emit: bool,
    pid: u32,
    tid: u32,
    name: &str,
    ts: u64,
    depth: u8,
) {
    if emit {
        out.instants.push(Instant {
            pid,
            tid,
            name: name.to_string(),
            timestamp_ns: ts,
            depth,
        });
    }
}

fn push_value(
    out: &mut EventsBody,
    emit: bool,
    pid: u32,
    tid: u32,
    name: &str,
    ts: u64,
    value: f64,
) {
    if emit {
        out.values.push(Value {
            pid,
            tid,
            name: name.to_string(),
            timestamp_ns: ts,
            value,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use time::macros::datetime;

    fn at(seconds: i64) -> OffsetDateTime {
        datetime!(2026-09-27 04:00:00 UTC) + Duration::from_secs(seconds as u64)
    }

    fn ns(seconds: i64) -> u64 {
        unix_ns(at(seconds))
    }

    fn event(
        id: i64,
        task: i64,
        kind: &str,
        actor: (&str, Option<&str>),
        payload: serde_json::Value,
        seconds: i64,
    ) -> Event {
        Event {
            id,
            task_id: Some(task),
            event_type: kind.into(),
            actor_type: actor.0.into(),
            actor_id: actor.1.map(str::to_string),
            payload,
            created_at: at(seconds),
        }
    }

    fn lifecycle() -> Vec<Event> {
        let human = ("human", Some("pierric"));
        let agent = ("agent", Some("claude-1"));
        vec![
            event(
                1,
                21,
                "task_created",
                human,
                json!({"title": "Link q with Orbit", "status": "inbox"}),
                0,
            ),
            event(
                2,
                21,
                "task_ready",
                human,
                json!({"from": "inbox", "to": "ready"}),
                10,
            ),
            event(
                3,
                21,
                "task_claimed",
                agent,
                json!({"from": "ready", "to": "claimed", "agent_id": "claude-1"}),
                20,
            ),
            event(
                4,
                21,
                "task_started",
                agent,
                json!({"from": "claimed", "to": "in_progress", "branch": "agent/task-21"}),
                30,
            ),
            event(
                5,
                21,
                "task_heartbeat",
                agent,
                json!({"lease_expires_at": "x"}),
                40,
            ),
            event(
                6,
                21,
                "task_note",
                agent,
                json!({"message": "[explore] @begin Survey the Orbit API"}),
                50,
            ),
            event(
                7,
                21,
                "task_note",
                agent,
                json!({"message": "Reading the ring code", "progress": 40}),
                60,
            ),
            event(
                8,
                21,
                "task_note",
                agent,
                json!({"message": "[explore] @end"}),
                70,
            ),
            event(
                9,
                21,
                "artifact_added",
                agent,
                json!({"kind": "pr", "value": "https://example/pr/1", "artifact_id": 3}),
                80,
            ),
            event(
                10,
                21,
                "task_completed",
                agent,
                json!({"from": "in_progress", "to": "done", "summary": "Bridge shipped"}),
                90,
            ),
        ]
    }

    fn apply_all(mapper: &mut Mapper, events: &[Event]) -> EventsBody {
        let mut out = EventsBody::unix();
        for event in events {
            out.extend(mapper.apply(event, true));
        }
        out
    }

    fn span<'a>(body: &'a EventsBody, name: &str) -> &'a Span {
        body.spans
            .iter()
            .find(|span| span.name == name)
            .unwrap_or_else(|| panic!("no span {name:?} in {:#?}", body.spans))
    }

    fn instant<'a>(body: &'a EventsBody, name: &str) -> &'a Instant {
        body.instants
            .iter()
            .find(|instant| instant.name == name)
            .unwrap_or_else(|| panic!("no instant {name:?} in {:#?}", body.instants))
    }

    #[test]
    fn a_task_is_a_process_and_its_agent_a_thread() {
        let mut mapper = Mapper::new(MapperOptions {
            segment: None,
            ..MapperOptions::default()
        });
        let body = apply_all(&mut mapper, &lifecycle());
        let pid = task_pid(21);
        assert_eq!(pid, Q_PID_BASE + 21 * TASK_STRIDE);

        let process = body.processes.iter().find(|p| p.pid == pid).unwrap();
        assert_eq!(process.name, "#21 Link q with Orbit");
        let queue = body.processes.iter().find(|p| p.pid == Q_PID_BASE).unwrap();
        assert_eq!(queue.name, QUEUE_PROCESS);

        let agent = body.threads.iter().find(|t| t.name == "claude-1").unwrap();
        assert_eq!(agent.pid, pid);
        assert_eq!(agent.tid, pid + 1);
        let sub = body.threads.iter().find(|t| t.name == "explore").unwrap();
        assert_eq!(sub.tid, pid + 2);

        // Main thread: phases as adjacent spans, terminal status as a mark.
        let ready = span(&body, "ready");
        assert_eq!((ready.tid, ready.depth), (pid, 0));
        assert_eq!(
            (ready.start_ns, ready.duration_ns),
            (ns(10), ns(20) - ns(10))
        );
        let claimed_phase = body
            .spans
            .iter()
            .find(|s| s.name == "claimed" && s.tid == pid)
            .unwrap();
        assert_eq!(claimed_phase.duration_ns, ns(30) - ns(20));
        let in_progress_phase = body
            .spans
            .iter()
            .find(|s| s.name == "in_progress" && s.tid == pid)
            .unwrap();
        assert_eq!(in_progress_phase.duration_ns, ns(90) - ns(30));
        assert_eq!(instant(&body, "done").tid, pid);
        assert_eq!(instant(&body, "created").depth, MAIN_INSTANT_DEPTH);

        // Agent thread: the claim, in_progress nested, instants above.
        let claim = body
            .spans
            .iter()
            .find(|s| s.name == "claimed" && s.tid == pid + 1)
            .unwrap();
        assert_eq!(
            (claim.start_ns, claim.duration_ns, claim.depth),
            (ns(20), ns(90) - ns(20), 0)
        );
        let working = span(&body, "in_progress agent/task-21");
        assert_eq!(
            (working.tid, working.depth, working.duration_ns),
            (pid + 1, 1, ns(90) - ns(30))
        );
        assert_eq!(instant(&body, "heartbeat").tid, pid + 1);
        assert_eq!(instant(&body, "heartbeat").depth, AGENT_INSTANT_DEPTH);
        assert_eq!(instant(&body, "note: Reading the ring code").tid, pid + 1);
        assert_eq!(
            instant(&body, "artifact pr: https://example/pr/1").tid,
            pid + 1
        );
        assert_eq!(instant(&body, "completed: Bridge shipped").tid, pid + 1);

        // The sub-agent thread holds the @begin/@end span.
        let survey = span(&body, "Survey the Orbit API");
        assert_eq!((survey.tid, survey.depth), (pid + 2, AGENT_INSTANT_DEPTH));
        assert_eq!(
            (survey.start_ns, survey.duration_ns),
            (ns(50), ns(70) - ns(50))
        );

        // Progress and the queue's active-claims lane.
        let progress = body
            .values
            .iter()
            .find(|v| v.name == PROGRESS_LANE)
            .unwrap();
        assert_eq!((progress.tid, progress.value), (pid, 40.0));
        let claims: Vec<f64> = body
            .values
            .iter()
            .filter(|v| v.name == ACTIVE_CLAIMS)
            .map(|v| v.value)
            .collect();
        assert_eq!(claims, vec![1.0, 0.0]);

        assert_eq!(mapper.open_spans(), 0);
        assert_eq!(mapper.events_seen(), 10);
    }

    #[test]
    fn open_spans_grow_by_segments_until_they_close() {
        let mut mapper = Mapper::new(MapperOptions {
            segment: Some(Duration::from_secs(5)),
            ..MapperOptions::default()
        });
        let events = lifecycle();
        apply_all(&mut mapper, &events[..4]);
        assert_eq!(mapper.open_spans(), 3, "phase, claim, in_progress");
        assert_eq!(mapper.active_claims(), 1);

        // At 31 only the claim (open since 20) is five seconds old; its
        // first segment runs from its real start.
        let first = mapper.tick(at(31));
        assert_eq!(first.spans.len(), 1, "{:#?}", first.spans);
        let claim = &first.spans[0];
        assert_eq!(
            (claim.name.as_str(), claim.tid),
            ("claimed", task_pid(21) + 1)
        );
        assert_eq!(
            (claim.start_ns, claim.duration_ns),
            (ns(20), ns(31) - ns(20))
        );
        // At 35 the two spans opened at 30 are due; the claim is not yet.
        let second = mapper.tick(at(35));
        assert_eq!(second.spans.len(), 2, "{:#?}", second.spans);
        assert!(second
            .spans
            .iter()
            .any(|s| s.name == "in_progress agent/task-21" && s.start_ns == ns(30)));
        assert!(second
            .spans
            .iter()
            .any(|s| s.name == "in_progress" && s.tid == task_pid(21)));
        // The next segment starts where the previous one ended.
        let third = mapper.tick(at(42));
        assert_eq!(third.spans.len(), 3);
        let claim = third
            .spans
            .iter()
            .find(|s| s.name == "claimed" && s.tid == task_pid(21) + 1)
            .unwrap();
        assert_eq!(
            (claim.start_ns, claim.duration_ns),
            (ns(31), ns(42) - ns(31))
        );

        // The close emits only the remainder.
        let close = mapper.apply(&events[9], true);
        let claim = close
            .spans
            .iter()
            .find(|s| s.name == "claimed" && s.tid == task_pid(21) + 1)
            .unwrap();
        assert_eq!(
            (claim.start_ns, claim.duration_ns),
            (ns(42), ns(90) - ns(42))
        );
        assert_eq!(mapper.open_spans(), 0);
    }

    #[test]
    fn replayed_history_updates_state_without_output() {
        let mut mapper = Mapper::default();
        let events = lifecycle();
        for event in &events[..4] {
            assert!(mapper.apply(event, false).is_empty());
        }
        assert_eq!(mapper.open_spans(), 3);
        // Names are still known for a later send.
        let names = mapper.names();
        assert!(names
            .processes
            .iter()
            .any(|p| p.name == "#21 Link q with Orbit"));
        assert!(names.threads.iter().any(|t| t.name == "claude-1"));
        // The first tick draws the open work from its real start.
        let tick = mapper.tick(at(100));
        let claim = tick
            .spans
            .iter()
            .find(|s| s.name == "claimed" && s.tid == task_pid(21) + 1)
            .unwrap();
        assert_eq!(claim.start_ns, ns(20));
    }

    #[test]
    fn dependencies_are_wait_spans_closed_by_the_dependency() {
        let mut mapper = Mapper::new(MapperOptions {
            segment: None,
            ..MapperOptions::default()
        });
        let info = TaskInfo {
            id: 5,
            title: "Ship the rollout".into(),
            created_at: at(0),
            dependencies: vec![(3, TaskStatus::Ready), (4, TaskStatus::Done)],
        };
        let body = mapper.task_info(info, true);
        assert_eq!(body.processes[0].name, "#5 Ship the rollout");
        assert_eq!(
            mapper.open_spans(),
            1,
            "only the unfinished dependency waits"
        );

        let done = event(
            9,
            3,
            "task_completed",
            ("human", Some("pierric")),
            json!({"from": "review", "to": "done"}),
            50,
        );
        let body = mapper.apply(&done, true);
        let wait = span(&body, "waits on #3");
        assert_eq!(
            (wait.pid, wait.tid, wait.depth),
            (task_pid(5), task_pid(5), WAITS_DEPTH)
        );
        assert_eq!((wait.start_ns, wait.duration_ns), (ns(0), ns(50) - ns(0)));
        assert_eq!(mapper.open_spans(), 0);
    }

    #[test]
    fn a_late_title_renames_the_process() {
        let mut mapper = Mapper::default();
        let heartbeat = event(1, 7, "task_heartbeat", ("agent", Some("a")), json!({}), 0);
        let body = mapper.apply(&heartbeat, true);
        assert_eq!(body.processes[0].name, "#7");
        assert!(!mapper.has_title(7));
        let body = mapper.task_info(
            TaskInfo {
                id: 7,
                title: "A title".into(),
                created_at: at(0),
                dependencies: vec![],
            },
            true,
        );
        assert_eq!(body.processes[0].name, "#7 A title");
        assert!(mapper.has_title(7));
        assert!(mapper
            .task_info(
                TaskInfo {
                    id: 7,
                    title: "A title".into(),
                    created_at: at(0),
                    dependencies: vec![],
                },
                true,
            )
            .is_empty());
    }

    #[test]
    fn notes_follow_the_thread_and_marker_convention() {
        assert_eq!(
            parse_note("[explore] @begin Survey   the API"),
            Note {
                thread: Some("explore".into()),
                action: NoteAction::Begin("Survey the API".into()),
            }
        );
        assert_eq!(
            parse_note("[explore] @end"),
            Note {
                thread: Some("explore".into()),
                action: NoteAction::End,
            }
        );
        assert_eq!(
            parse_note("@begin"),
            Note {
                thread: None,
                action: NoteAction::Begin("span".into()),
            }
        );
        assert_eq!(
            parse_note("[not a thread] plain"),
            Note {
                thread: None,
                action: NoteAction::Text("[not a thread] plain".into()),
            }
        );
        assert_eq!(
            parse_note("  @ending is text  "),
            Note {
                thread: None,
                action: NoteAction::Text("@ending is text".into()),
            }
        );

        let mut mapper = Mapper::new(MapperOptions {
            segment: None,
            ..MapperOptions::default()
        });
        let events = lifecycle();
        apply_all(&mut mapper, &events[..4]);
        let pid = task_pid(21);
        // Nested @begin on the agent's own thread.
        let a = event(
            20,
            21,
            "task_note",
            ("agent", Some("claude-1")),
            json!({"message": "@begin outer"}),
            40,
        );
        let b = event(
            21,
            21,
            "task_note",
            ("agent", Some("claude-1")),
            json!({"message": "@begin inner"}),
            41,
        );
        let c = event(
            22,
            21,
            "task_note",
            ("agent", Some("claude-1")),
            json!({"message": "inside"}),
            42,
        );
        let d = event(
            23,
            21,
            "task_note",
            ("agent", Some("claude-1")),
            json!({"message": "@end"}),
            43,
        );
        let e = event(
            24,
            21,
            "task_note",
            ("agent", Some("claude-1")),
            json!({"message": "@end"}),
            44,
        );
        let f = event(
            25,
            21,
            "task_note",
            ("agent", Some("claude-1")),
            json!({"message": "@end"}),
            45,
        );
        let mut body = EventsBody::unix();
        for ev in [&a, &b, &c, &d, &e, &f] {
            body.extend(mapper.apply(ev, true));
        }
        let inside = instant(&body, "note: inside");
        assert_eq!(
            (inside.tid, inside.depth),
            (pid + 1, AGENT_INSTANT_DEPTH + 2)
        );
        let inner = span(&body, "inner");
        assert_eq!(
            (inner.depth, inner.duration_ns),
            (AGENT_INSTANT_DEPTH + 1, ns(43) - ns(41))
        );
        let outer = span(&body, "outer");
        assert_eq!(
            (outer.depth, outer.duration_ns),
            (AGENT_INSTANT_DEPTH, ns(44) - ns(40))
        );
        assert_eq!(instant(&body, "@end without @begin").tid, pid + 1);

        // A human note goes on the task's main thread.
        let human = event(
            26,
            21,
            "task_note",
            ("human", Some("pierric")),
            json!({"message": "looks right"}),
            46,
        );
        let body = mapper.apply(&human, true);
        let note = instant(&body, "note: looks right");
        assert_eq!((note.tid, note.depth), (pid, MAIN_INSTANT_DEPTH));

        // Completion closes user spans still open.
        let g = event(
            27,
            21,
            "task_note",
            ("agent", Some("claude-1")),
            json!({"message": "@begin dangling"}),
            47,
        );
        mapper.apply(&g, true);
        let body = mapper.apply(&events[9], true);
        assert_eq!(span(&body, "dangling").duration_ns, ns(90) - ns(47));
    }

    #[test]
    fn exec_and_exit_notes_parse() {
        assert_eq!(
            parse_note("@exec cargo  build --locked"),
            Note {
                thread: None,
                action: NoteAction::Exec("cargo build --locked".into()),
            }
        );
        assert_eq!(
            parse_note("[bench] @exit 0 (12.3s) cargo build --locked"),
            Note {
                thread: Some("bench".into()),
                action: NoteAction::Exit {
                    code: "0".into(),
                    duration: Some("12.3s".into()),
                    command: Some("cargo build --locked".into()),
                },
            }
        );
        assert_eq!(
            parse_note("@exit 101"),
            Note {
                thread: None,
                action: NoteAction::Exit {
                    code: "101".into(),
                    duration: None,
                    command: None,
                },
            }
        );
        assert_eq!(
            parse_note("@exit ? git push"),
            Note {
                thread: None,
                action: NoteAction::Exit {
                    code: "?".into(),
                    duration: None,
                    command: Some("git push".into()),
                },
            }
        );
        // Not the convention: a bare @exec, a non-numeric code, a longer word.
        assert_eq!(parse_note("@exec").action, NoteAction::Text("@exec".into()));
        assert_eq!(
            parse_note("@exit soon").action,
            NoteAction::Text("@exit soon".into())
        );
        assert_eq!(
            parse_note("@executed the plan").action,
            NoteAction::Text("@executed the plan".into())
        );
    }

    #[test]
    fn launched_processes_are_spans_under_the_agent_with_an_exit_mark() {
        let mut mapper = Mapper::new(MapperOptions {
            segment: None,
            ..MapperOptions::default()
        });
        let events = lifecycle();
        apply_all(&mut mapper, &events[..4]);
        let pid = task_pid(21);
        let agent = ("agent", Some("claude-1"));
        let note = |id: i64, message: &str, seconds: i64| {
            event(
                id,
                21,
                "task_note",
                agent,
                json!({"message": message}),
                seconds,
            )
        };
        let mut body = EventsBody::unix();
        for ev in [
            // Inside a @begin span, a build that fails, then tests that pass.
            note(40, "@begin Fix the parser", 40),
            note(41, "@exec cargo build --locked", 41),
            note(42, "@exit 101 (3.4s) cargo build --locked", 45),
            note(43, "@exec cargo test --workspace", 46),
            note(44, "@exit 0 (20.0s) cargo test --workspace", 66),
            note(45, "@end", 67),
        ] {
            body.extend(mapper.apply(&ev, true));
        }
        // The process spans nest on the agent's thread under in_progress
        // (depth 1) and the @begin span (depth 2).
        let build = span(&body, "$ cargo build --locked");
        assert_eq!((build.tid, build.depth), (pid + 1, AGENT_INSTANT_DEPTH + 1));
        assert_eq!(
            (build.start_ns, build.duration_ns),
            (ns(41), ns(45) - ns(41))
        );
        let failed = instant(&body, "exit 101 (3.4s)");
        assert_eq!(
            (failed.tid, failed.timestamp_ns, failed.depth),
            (pid + 1, ns(45), AGENT_INSTANT_DEPTH + 2),
            "the exit mark sits inside the span, at its end"
        );
        let tests = span(&body, "$ cargo test --workspace");
        assert_eq!(
            (tests.depth, tests.start_ns, tests.duration_ns),
            (AGENT_INSTANT_DEPTH + 1, ns(46), ns(66) - ns(46))
        );
        assert_eq!(instant(&body, "exit 0 (20.0s)").timestamp_ns, ns(66));
        assert_eq!(span(&body, "Fix the parser").duration_ns, ns(67) - ns(40));
        assert_eq!(mapper.open_spans(), 3, "phase, claim, in_progress");

        // Overlapping processes close by command line, not by order; an
        // @exit with no command closes the most recent process; an @exit
        // with nothing open is a mark saying so.
        let mut body = EventsBody::unix();
        for ev in [
            note(50, "@exec sleep 30", 70),
            note(51, "@exec git push", 71),
            note(52, "@exit 0 (5.0s) sleep 30", 75),
            note(53, "@exit 1", 76),
            note(54, "@exit 0 gh pr create", 77),
        ] {
            body.extend(mapper.apply(&ev, true));
        }
        let sleep = span(&body, "$ sleep 30");
        assert_eq!(
            (sleep.depth, sleep.duration_ns),
            (AGENT_INSTANT_DEPTH, ns(75) - ns(70))
        );
        let push = span(&body, "$ git push");
        assert_eq!(
            (push.depth, push.duration_ns),
            (AGENT_INSTANT_DEPTH + 1, ns(76) - ns(71))
        );
        assert_eq!(instant(&body, "exit 1").depth, AGENT_INSTANT_DEPTH + 2);
        assert_eq!(
            instant(&body, "exit 0 without @exec: gh pr create").depth,
            AGENT_INSTANT_DEPTH
        );

        // On a sub-agent thread, and long command lines are trimmed.
        let long = format!("@exec python3 {}", "x".repeat(200));
        let mut body = EventsBody::unix();
        for ev in [
            note(60, "[bench] @exec cargo bench", 80),
            note(61, "[bench] @exit 0 (60.0s) cargo bench", 140),
            note(62, &long, 141),
        ] {
            body.extend(mapper.apply(&ev, true));
        }
        let bench = span(&body, "$ cargo bench");
        assert_eq!((bench.tid, bench.depth), (pid + 2, AGENT_INSTANT_DEPTH));
        assert_eq!(instant(&body, "exit 0 (60.0s)").tid, pid + 2);
        assert_eq!(mapper.open_spans(), 4, "phase, claim, in_progress, python3");
        // The dangling process closes with the claim.
        let done = event(
            70,
            21,
            "task_completed",
            agent,
            json!({"from": "in_progress", "to": "done", "summary": "ok"}),
            150,
        );
        let body = mapper.apply(&done, true);
        let python = body
            .spans
            .iter()
            .find(|s| s.name.starts_with("$ python3 "))
            .unwrap();
        assert_eq!(python.name.chars().count(), 2 + 120);
        assert!(python.name.ends_with('…'));
        assert_eq!(python.duration_ns, ns(150) - ns(141));
        assert_eq!(mapper.open_spans(), 0);
    }

    #[test]
    fn a_release_ends_the_agent_thread_and_a_reclaim_reuses_it() {
        let mut mapper = Mapper::new(MapperOptions {
            segment: None,
            ..MapperOptions::default()
        });
        let events = lifecycle();
        apply_all(&mut mapper, &events[..4]);
        let pid = task_pid(21);
        let released = event(
            30,
            21,
            "task_released",
            ("agent", Some("claude-1")),
            json!({"from": "in_progress", "to": "ready"}),
            100,
        );
        let body = mapper.apply(&released, true);
        assert_eq!(instant(&body, "released").tid, pid + 1);
        assert_eq!(mapper.active_claims(), 0);
        assert_eq!(mapper.open_spans(), 1, "the ready phase");

        let other = event(
            31,
            21,
            "task_claimed",
            ("agent", Some("codex-2")),
            json!({"from": "ready", "to": "claimed", "agent_id": "codex-2"}),
            110,
        );
        let body = mapper.apply(&other, true);
        let thread = body.threads.iter().find(|t| t.name == "codex-2").unwrap();
        assert_eq!(thread.tid, pid + 2, "a new agent gets the next slot");
        let again = event(
            32,
            21,
            "task_claimed",
            ("agent", Some("claude-1")),
            json!({"from": "ready", "to": "claimed", "agent_id": "claude-1"}),
            120,
        );
        let body = mapper.apply(&again, true);
        assert!(
            body.threads.iter().all(|t| t.name != "claude-1"),
            "known thread, no rename"
        );
    }

    #[test]
    fn events_without_a_task_are_ignored() {
        let mut mapper = Mapper::default();
        let mut event = event(1, 1, "queue_recovered", ("system", None), json!({}), 0);
        event.task_id = None;
        assert!(mapper.apply(&event, true).is_empty());
        assert!(!mapper.knows_task(1));
    }
}
