//! The body of Orbit's `POST /api/events`, as `orbit-live-server`'s
//! `ingest.rs` reads it. Every list is optional; an empty body is a no-op.

use serde::{Deserialize, Serialize};

/// Which clock the timestamps are on. The bridge always sends `unix_ns`;
/// Orbit moves them onto its capture clock.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Clock {
    #[default]
    MonotonicNs,
    UnixNs,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct EventsBody {
    #[serde(default)]
    pub clock: Clock,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub processes: Vec<ProcessName>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub threads: Vec<ThreadName>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub spans: Vec<Span>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub instants: Vec<Instant>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub values: Vec<Value>,
}

impl EventsBody {
    pub fn unix() -> Self {
        Self {
            clock: Clock::UnixNs,
            ..Self::default()
        }
    }

    /// True when there is nothing to send.
    pub fn is_empty(&self) -> bool {
        self.processes.is_empty()
            && self.threads.is_empty()
            && self.spans.is_empty()
            && self.instants.is_empty()
            && self.values.is_empty()
    }

    /// Number of timeline events (names not counted).
    pub fn event_count(&self) -> usize {
        self.spans.len() + self.instants.len() + self.values.len()
    }

    /// Append everything in `other`.
    pub fn extend(&mut self, other: EventsBody) {
        self.processes.extend(other.processes);
        self.threads.extend(other.threads);
        self.spans.extend(other.spans);
        self.instants.extend(other.instants);
        self.values.extend(other.values);
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProcessName {
    pub pid: u32,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThreadName {
    pub pid: u32,
    pub tid: u32,
    pub name: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SpanTrack {
    #[default]
    Scope,
    Async,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Span {
    pub pid: u32,
    pub tid: u32,
    pub name: String,
    pub start_ns: u64,
    pub duration_ns: u64,
    #[serde(default)]
    pub depth: u8,
    #[serde(default)]
    pub track: SpanTrack,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Instant {
    pub pid: u32,
    pub tid: u32,
    pub name: String,
    pub timestamp_ns: u64,
    #[serde(default)]
    pub depth: u8,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Value {
    pub pid: u32,
    pub tid: u32,
    pub name: String,
    pub timestamp_ns: u64,
    pub value: f64,
}

/// What Orbit answers.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct EventsSummary {
    #[serde(default)]
    pub accepted: u64,
    #[serde(default)]
    pub dropped_before_start: u64,
    #[serde(default)]
    pub named: u64,
    #[serde(default)]
    pub monotonic_now_ns: u64,
    #[serde(default)]
    pub capture_start_ns: u64,
}
