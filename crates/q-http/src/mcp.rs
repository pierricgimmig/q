//! `POST /mcp`: MCP over Streamable HTTP, stateless.
//!
//! Each request builds a fresh [`Session`] for the authenticated principal.
//! Human principals get the triage tools (`queue_ready`, `queue_reopen`);
//! agents do not. The session records events as the principal, so a human
//! triaging from a chat app shows up as that human, not as an agent.
//!
//! Only the JSON response mode is implemented. `GET /mcp` (a server-push
//! stream) answers 405, which the transport spec allows, and `DELETE /mcp`
//! answers 204 because there is no session to end.

use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::State;
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use q_core::Actor;
use q_mcp::Session;
use serde_json::{json, Value};

use crate::auth::{Principal, Role};
use crate::server::{authenticate, AppState};

pub async fn post(State(state): State<Arc<AppState>>, headers: HeaderMap, body: Bytes) -> Response {
    let principal = match authenticate(&state, &headers) {
        Ok(principal) => principal,
        Err(_) => return unauthorized(&state),
    };
    let message: Value = match serde_json::from_slice(&body) {
        Ok(value) => value,
        Err(err) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({
                    "jsonrpc": "2.0",
                    "id": null,
                    "error": { "code": -32700, "message": format!("parse error: {err}") }
                })),
            )
                .into_response()
        }
    };
    let queue = state.queue.clone();
    let base_dir = state.options.base_dir.clone();
    let outcome = tokio::task::spawn_blocking(move || {
        let mut session = Session::new(base_dir)
            .with_actor(actor_for(&principal))
            .with_human_tools(principal.role == Role::Human)
            .stateless();
        match message {
            Value::Array(items) => {
                let responses: Vec<Value> = items
                    .iter()
                    .filter_map(|item| session.handle_value(queue.as_ref(), item))
                    .collect();
                if responses.is_empty() {
                    None
                } else {
                    Some(Value::Array(responses))
                }
            }
            other => session.handle_value(queue.as_ref(), &other),
        }
    })
    .await;
    match outcome {
        Ok(Some(response)) => (StatusCode::OK, Json(response)).into_response(),
        Ok(None) => StatusCode::ACCEPTED.into_response(),
        Err(join) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({
                "jsonrpc": "2.0",
                "id": null,
                "error": { "code": -32603, "message": format!("worker panicked: {join}") }
            })),
        )
            .into_response(),
    }
}

pub async fn get() -> Response {
    (
        StatusCode::METHOD_NOT_ALLOWED,
        [(header::ALLOW, "POST, DELETE")],
        "server-initiated streams are not supported; POST JSON-RPC to /mcp",
    )
        .into_response()
}

pub async fn delete() -> Response {
    StatusCode::NO_CONTENT.into_response()
}

fn actor_for(principal: &Principal) -> Actor {
    match principal.role {
        Role::Human => Actor::human(Some(principal.name.clone())),
        Role::Agent => Actor::agent(principal.name.clone()),
    }
}

/// A 401 that points OAuth-capable clients at the resource metadata, per the
/// MCP authorization spec.
fn unauthorized(state: &AppState) -> Response {
    let base = state.options.public_url.as_deref().unwrap();
    let challenge = format!(
        "Bearer resource_metadata=\"{base}/.well-known/oauth-protected-resource\", error=\"invalid_token\""
    );
    let mut response = (
        StatusCode::UNAUTHORIZED,
        Json(json!({
            "jsonrpc": "2.0",
            "id": null,
            "error": { "code": -32001, "message": "missing or invalid bearer token" }
        })),
    )
        .into_response();
    if let Ok(value) = HeaderValue::from_str(&challenge) {
        response
            .headers_mut()
            .insert(header::WWW_AUTHENTICATE, value);
    }
    response
}
