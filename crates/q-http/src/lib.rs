//! HTTP transport for [`q_core::QueueService`].
//!
//! [`server`] exposes a queue as `q serve`. [`client::RemoteQueue`] is a
//! `QueueService` that talks to such a server, so the CLI and MCP adapters
//! work unchanged against a remote authority.

pub mod client;
pub mod server;
pub mod wire;

pub use client::RemoteQueue;
pub use server::{check_bind, serve_on, AuthConfig, Principal, Role};
