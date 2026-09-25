//! HTTP transport for [`q_core::QueueService`].
//!
//! [`server`] exposes a queue as `q serve`: a JSON API for remote CLI and
//! `q mcp` clients, MCP over HTTP for chat apps and IDE agents, and the OAuth
//! endpoints chat connectors sign in with. [`client::RemoteQueue`] is a
//! `QueueService` that talks to such a server.

pub mod auth;
pub mod client;
mod crypto;
mod grants;
mod mcp;
pub mod oauth;
pub mod origin;
mod rpc;
pub mod server;
pub mod wire;

pub use auth::{AuthConfig, Principal, Role, TokenStore};
pub use client::RemoteQueue;
pub use grants::GrantStore;
pub use oauth::SigningKey;
pub use server::{check_bind, serve_on, ServerOptions};
