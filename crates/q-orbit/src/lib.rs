//! `q orbit`: follow queue activity live in the Orbit profiler.
//!
//! The bridge tails the queue's event log through [`q_core::QueueService`]
//! and posts what it sees to a running Orbit service (`POST /api/events`).
//! The mapping is the profiler's own vocabulary:
//!
//! - a **task** is a process: one row group named `#id title`, whose main
//!   thread carries the task's status phases (`ready`, `claimed`,
//!   `in_progress`, ...) as spans and the human actions as instants;
//! - an **agent** working the task is a thread of that process, named after
//!   its agent id, with the claim as a span, `in_progress` nested inside it,
//!   and notes, heartbeats and artifacts as instants; a `@begin`/`@end`
//!   convention in log notes opens nested spans, and a `[name]` prefix files
//!   a note on a named sub-thread, so parallel sub-agents show as parallel
//!   threads;
//! - **dependencies** are spans `waits on #dep` on the dependent task's main
//!   thread, from when the bridge learns of the edge until the dependency is
//!   done;
//! - reported `progress` becomes a value lane on the task.
//!
//! [`mapper::Mapper`] is the pure part: events in, Orbit records out. The
//! [`client::OrbitClient`] posts a [`wire::EventsBody`]. [`bridge`] runs the
//! loop.

pub mod bridge;
pub mod client;
pub mod mapper;
pub mod wire;

pub use bridge::{run, BridgeOptions, BridgeReport};
pub use client::OrbitClient;
pub use mapper::{Mapper, MapperOptions, TaskInfo};
pub use wire::EventsBody;

/// Default Orbit service URL, matching `./rust.sh` in the Orbit repo.
pub const DEFAULT_ORBIT_URL: &str = "http://127.0.0.1:44766";

/// Environment variable read when no `--url` is given.
pub const ORBIT_URL_ENV: &str = "Q_ORBIT_URL";
