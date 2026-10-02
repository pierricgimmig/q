//! The loop: read new queue events, map them, post to Orbit, repeat.

use std::time::Duration;

use q_core::{QueueError, QueueService, TaskStatus};
use time::OffsetDateTime;

use crate::client::{OrbitClient, OrbitError};
use crate::mapper::{Mapper, MapperOptions, TaskInfo};
use crate::wire::EventsBody;

/// Events fetched per `events_since` call.
pub const PAGE_SIZE: u32 = 500;
/// Timeline events kept in an unsent batch while Orbit is unreachable.
pub const PENDING_LIMIT: usize = 100_000;

#[derive(Debug, Clone)]
pub struct BridgeOptions {
    /// Seconds between polls of the queue.
    pub interval: Duration,
    /// Draw events newer than this age at start. `None` replays everything
    /// the log holds; `Some(0)` draws nothing from the past. Older events
    /// still build the state, so work that is open now is drawn from its
    /// real start.
    pub history: Option<Duration>,
    /// How often every known name is re-sent, so a viewer that pressed
    /// Record (which clears names) gets them back.
    pub names_every: Duration,
    pub mapper: MapperOptions,
    /// Do one pass and return.
    pub once: bool,
}

impl Default for BridgeOptions {
    fn default() -> Self {
        Self {
            interval: Duration::from_secs(2),
            history: Some(Duration::from_secs(60 * 60)),
            names_every: Duration::from_secs(30),
            mapper: MapperOptions::default(),
            once: false,
        }
    }
}

/// What one push did, for a progress line.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize)]
pub struct BatchReport {
    pub events: usize,
    pub names: usize,
    pub accepted: u64,
    pub dropped_before_start: u64,
    pub last_event_id: i64,
    pub open_spans: usize,
    pub active_claims: usize,
}

/// Totals when the loop returns.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize)]
pub struct BridgeReport {
    pub orbit_url: String,
    pub events_seen: u64,
    pub batches: u64,
    pub pushed_events: u64,
    pub accepted: u64,
    pub dropped_before_start: u64,
    pub failed_posts: u64,
    pub last_event_id: i64,
    pub open_spans: usize,
    pub active_claims: usize,
    pub tasks: usize,
}

#[derive(Debug)]
pub enum BridgeError {
    Queue(QueueError),
    Orbit(OrbitError),
}

impl std::fmt::Display for BridgeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Queue(err) => write!(f, "{err}"),
            Self::Orbit(err) => write!(f, "{err}"),
        }
    }
}

impl std::error::Error for BridgeError {}

impl From<QueueError> for BridgeError {
    fn from(err: QueueError) -> Self {
        Self::Queue(err)
    }
}

impl From<OrbitError> for BridgeError {
    fn from(err: OrbitError) -> Self {
        Self::Orbit(err)
    }
}

/// Run the bridge until `stop` returns true (checked between polls) or,
/// with `once`, after one pass. `on_batch` is called after every successful
/// push. Queue errors end the loop; Orbit errors are retried with the batch
/// kept for the next attempt.
pub fn run(
    queue: &dyn QueueService,
    client: &OrbitClient,
    options: &BridgeOptions,
    stop: &dyn Fn() -> bool,
    on_batch: &mut dyn FnMut(&BatchReport),
) -> Result<BridgeReport, BridgeError> {
    let mut bridge = Bridge::new(options.clone());
    // Fail fast on a bad URL or a dead service; after this, outages are retried.
    client.status()?;
    loop {
        bridge.poll(queue)?;
        bridge.push(client, on_batch);
        if options.once || stop() {
            break;
        }
        sleep_until(options.interval, stop);
    }
    Ok(bridge.report(client.url()))
}

/// One bridge instance: the mapper, the cursor into the log, and what has
/// not been sent yet. Separated from [`run`] so tests can drive it.
pub struct Bridge {
    options: BridgeOptions,
    mapper: Mapper,
    after_id: i64,
    emit_from: Option<OffsetDateTime>,
    pending: EventsBody,
    names_sent_at: Option<OffsetDateTime>,
    report: BridgeReport,
}

impl Bridge {
    pub fn new(options: BridgeOptions) -> Self {
        let now = OffsetDateTime::now_utc();
        let emit_from = options.history.map(|age| now - age);
        Self {
            mapper: Mapper::new(options.mapper.clone()),
            options,
            after_id: 0,
            emit_from,
            pending: EventsBody::unix(),
            names_sent_at: None,
            report: BridgeReport::default(),
        }
    }

    pub fn mapper(&self) -> &Mapper {
        &self.mapper
    }

    /// Read every event after the cursor and map it. Also emits the periodic
    /// segments and, when due, every known name.
    pub fn poll(&mut self, queue: &dyn QueueService) -> Result<(), QueueError> {
        loop {
            let page = queue.events_since(self.after_id, PAGE_SIZE)?;
            let len = page.len();
            for event in &page {
                self.after_id = event.id;
                let emit = self
                    .emit_from
                    .map(|from| event.created_at >= from)
                    .unwrap_or(true);
                if let Some(task_id) = event.task_id {
                    if !self.mapper.has_title(task_id) {
                        if let Some(info) = fetch_task_info(queue, task_id)? {
                            let out = self.mapper.task_info(info, emit);
                            self.absorb(out);
                        }
                    }
                }
                let out = self.mapper.apply(event, emit);
                self.absorb(out);
            }
            if len < PAGE_SIZE as usize {
                break;
            }
        }
        let now = OffsetDateTime::now_utc();
        let ticks = self.mapper.tick(now);
        self.absorb(ticks);
        let names_due = self
            .names_sent_at
            .map(|at| now - at >= self.options.names_every)
            .unwrap_or(true);
        if names_due {
            let names = self.mapper.names();
            self.absorb(names);
            self.names_sent_at = Some(now);
        }
        self.report.events_seen = self.mapper.events_seen();
        self.report.last_event_id = self.after_id;
        Ok(())
    }

    /// Post what is pending. On failure the batch is kept, bounded by
    /// [`PENDING_LIMIT`] timeline events (oldest dropped first).
    pub fn push(&mut self, client: &OrbitClient, on_batch: &mut dyn FnMut(&BatchReport)) {
        if self.pending.is_empty() {
            return;
        }
        let events = self.pending.event_count();
        let names = self.pending.processes.len() + self.pending.threads.len();
        match client.post_events(&self.pending) {
            Ok(summary) => {
                let batch = BatchReport {
                    events,
                    names,
                    accepted: summary.accepted,
                    dropped_before_start: summary.dropped_before_start,
                    last_event_id: self.after_id,
                    open_spans: self.mapper.open_spans(),
                    active_claims: self.mapper.active_claims(),
                };
                self.report.batches += 1;
                self.report.pushed_events += events as u64;
                self.report.accepted += summary.accepted;
                self.report.dropped_before_start += summary.dropped_before_start;
                self.pending = EventsBody::unix();
                on_batch(&batch);
            }
            Err(err) => {
                self.report.failed_posts += 1;
                tracing::warn!(error = %err, pending = events, "orbit push failed; will retry");
                // Names are cheap to resend; make sure they go with the retry.
                self.names_sent_at = None;
                self.trim_pending();
            }
        }
    }

    pub fn report(&self, url: &str) -> BridgeReport {
        let mut report = self.report.clone();
        report.orbit_url = url.to_string();
        report.open_spans = self.mapper.open_spans();
        report.active_claims = self.mapper.active_claims();
        report.tasks = self.mapper.names().processes.len().saturating_sub(1);
        report
    }

    fn absorb(&mut self, out: EventsBody) {
        self.pending.extend(out);
    }

    fn trim_pending(&mut self) {
        let over = self.pending.event_count().saturating_sub(PENDING_LIMIT);
        if over == 0 {
            return;
        }
        let drop_spans = over.min(self.pending.spans.len());
        self.pending.spans.drain(..drop_spans);
        let over = over - drop_spans;
        let drop_instants = over.min(self.pending.instants.len());
        self.pending.instants.drain(..drop_instants);
        let over = over - drop_instants;
        let drop_values = over.min(self.pending.values.len());
        self.pending.values.drain(..drop_values);
    }
}

/// Title and dependency statuses for a task; `None` when the task is gone.
fn fetch_task_info(queue: &dyn QueueService, task_id: i64) -> Result<Option<TaskInfo>, QueueError> {
    let detail = match queue.get(task_id) {
        Ok(detail) => detail,
        Err(QueueError::NotFound(_)) => return Ok(None),
        Err(err) => return Err(err),
    };
    let mut dependencies = Vec::with_capacity(detail.task.dependencies.len());
    for dep in &detail.task.dependencies {
        let status = match queue.get(*dep) {
            Ok(dep_detail) => dep_detail.task.status,
            Err(QueueError::NotFound(_)) => TaskStatus::Done,
            Err(err) => return Err(err),
        };
        dependencies.push((*dep, status));
    }
    Ok(Some(TaskInfo {
        id: task_id,
        title: detail.task.title,
        created_at: detail.task.created_at,
        dependencies,
    }))
}

fn sleep_until(interval: Duration, stop: &dyn Fn() -> bool) {
    let step = Duration::from_millis(100);
    let mut slept = Duration::ZERO;
    while slept < interval {
        if stop() {
            return;
        }
        let nap = step.min(interval - slept);
        std::thread::sleep(nap);
        slept += nap;
    }
}
