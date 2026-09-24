//! `q serve`: the queue as a single HTTP authority.
//!
//! One process owns the SQLite file and exposes every
//! [`QueueService`] method as `POST /v1/<method>`. Claims stay serialized by
//! the same `BEGIN IMMEDIATE` transaction they use locally, so two remote
//! agents cannot take the same task. There is no replica and no sync.
//!
//! # Auth
//!
//! Requests carry `Authorization: Bearer <secret>`. Tokens are loaded from a
//! TOML file and have a role. `human` tokens may call everything. `agent`
//! tokens may not call the methods in [`HUMAN_ONLY_METHODS`], and any actor
//! they send is stamped as an agent so an agent cannot record events as a
//! human. Without a token file the server accepts unauthenticated requests as
//! an anonymous human, and refuses to bind anything but a loopback address.

use std::future::Future;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::{Path as UrlPath, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use q_core::{
    Actor, ActorKind, BlockRequest, CancelRequest, CaptureRequest, ClaimRequest, CompleteRequest,
    CreateFeatureRequest, DeleteRequest, HeartbeatRequest, ListFilter, QueueError, QueueService,
    ReadyRequest, RecoverRequest, ReleaseRequest, StartRequest, TreeQuery,
};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::net::TcpListener;

use crate::wire::{
    EditBody, EditFeatureBody, EmptyBody, ErrorBody, ErrorEnvelope, HealthBody, IdBody, ReopenBody,
    HUMAN_ONLY_METHODS,
};

const MIN_SECRET_LEN: usize = 16;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    Human,
    Agent,
}

impl Role {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Human => "human",
            Self::Agent => "agent",
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct AuthToken {
    pub name: String,
    pub role: Role,
    pub secret: String,
}

/// Token file contents.
///
/// ```toml
/// [[tokens]]
/// name = "pierric"
/// role = "human"
/// secret = "..."
///
/// [[tokens]]
/// name = "codex-vps"
/// role = "agent"
/// secret = "..."
/// ```
#[derive(Debug, Clone, Default, Deserialize)]
pub struct AuthConfig {
    #[serde(default)]
    pub tokens: Vec<AuthToken>,
}

impl AuthConfig {
    pub fn load(path: &Path) -> Result<Self, String> {
        let text = std::fs::read_to_string(path)
            .map_err(|err| format!("cannot read token file {}: {err}", path.display()))?;
        Self::parse(&text).map_err(|err| format!("{}: {err}", path.display()))
    }

    pub fn parse(text: &str) -> Result<Self, String> {
        let config: AuthConfig = toml::from_str(text).map_err(|err| err.to_string())?;
        if config.tokens.is_empty() {
            return Err("token file defines no tokens".into());
        }
        let mut names = std::collections::HashSet::new();
        for token in &config.tokens {
            let name = token.name.trim();
            if name.is_empty() {
                return Err("every token needs a name".into());
            }
            if !names.insert(name.to_string()) {
                return Err(format!("duplicate token name {name}"));
            }
            if token.secret.len() < MIN_SECRET_LEN {
                return Err(format!(
                    "token {name}: secret must be at least {MIN_SECRET_LEN} characters"
                ));
            }
        }
        Ok(config)
    }

    /// Resolve an `Authorization` header value to a principal.
    fn authenticate(&self, header: Option<&str>) -> Option<Principal> {
        let secret = header?.strip_prefix("Bearer ")?.trim();
        self.tokens
            .iter()
            .find(|token| constant_time_eq(token.secret.as_bytes(), secret.as_bytes()))
            .map(|token| Principal {
                name: token.name.clone(),
                role: token.role,
            })
    }
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// Who is calling. Anonymous loopback callers are a human named `anonymous`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Principal {
    pub name: String,
    pub role: Role,
}

impl Principal {
    pub fn anonymous() -> Self {
        Self {
            name: "anonymous".into(),
            role: Role::Human,
        }
    }
}

struct AppState {
    queue: Arc<dyn QueueService>,
    auth: Option<AuthConfig>,
}

/// Build the router. `auth: None` accepts every request as an anonymous human.
pub fn router(queue: Arc<dyn QueueService>, auth: Option<AuthConfig>) -> Router {
    let state = Arc::new(AppState { queue, auth });
    Router::new()
        .route("/v1/health", get(health))
        .route("/v1/:method", post(rpc))
        .with_state(state)
}

/// Refuse to expose an unauthenticated server beyond this machine.
pub fn check_bind(addr: &SocketAddr, auth: Option<&AuthConfig>) -> Result<(), String> {
    if auth.is_none() && !addr.ip().is_loopback() {
        return Err(format!(
            "refusing to serve on {addr} without a token file; pass --auth FILE or bind 127.0.0.1"
        ));
    }
    Ok(())
}

/// Serve on an already-bound listener until `shutdown` resolves.
pub async fn serve_on(
    queue: Arc<dyn QueueService>,
    listener: TcpListener,
    auth: Option<AuthConfig>,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> std::io::Result<()> {
    let app = router(queue, auth);
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown)
        .await
}

async fn health() -> Json<HealthBody> {
    Json(HealthBody {
        ok: true,
        version: env!("CARGO_PKG_VERSION").to_string(),
    })
}

async fn rpc(
    State(state): State<Arc<AppState>>,
    UrlPath(method): UrlPath<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let principal = match &state.auth {
        None => Principal::anonymous(),
        Some(auth) => {
            let header = headers
                .get(axum::http::header::AUTHORIZATION)
                .and_then(|value| value.to_str().ok());
            match auth.authenticate(header) {
                Some(principal) => principal,
                None => {
                    return error_response(ErrorBody::new(
                        "unauthorized",
                        "missing or invalid bearer token",
                    ))
                }
            }
        }
    };
    let queue = state.queue.clone();
    let outcome =
        tokio::task::spawn_blocking(move || dispatch(queue.as_ref(), &method, &body, &principal))
            .await;
    match outcome {
        Ok(Ok(value)) => (StatusCode::OK, Json(value)).into_response(),
        Ok(Err(error)) => error_response(error),
        Err(join) => error_response(ErrorBody::new(
            "database",
            format!("worker panicked: {join}"),
        )),
    }
}

fn error_response(error: ErrorBody) -> Response {
    let status =
        StatusCode::from_u16(error.http_status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    (status, Json(ErrorEnvelope { error })).into_response()
}

/// Run one method against the queue. Shared by the HTTP handler and tests.
pub fn dispatch(
    queue: &dyn QueueService,
    method: &str,
    body: &[u8],
    principal: &Principal,
) -> Result<Value, ErrorBody> {
    if principal.role == Role::Agent && HUMAN_ONLY_METHODS.contains(&method) {
        return Err(ErrorBody::new(
            "forbidden",
            format!("{method} requires a human token"),
        ));
    }
    match method {
        "capture" => {
            let mut request: CaptureRequest = parse(body)?;
            stamp(&mut request.actor, principal);
            reply(queue.capture(request))
        }
        "list" => {
            let filter: ListFilter = parse(body)?;
            reply(queue.list(filter))
        }
        "get" => {
            let IdBody { id } = parse(body)?;
            reply(queue.get(id))
        }
        "edit" => {
            let EditBody { id, mut request } = parse(body)?;
            stamp(&mut request.actor, principal);
            reply(queue.edit(id, request))
        }
        "mark_ready" => {
            let mut request: ReadyRequest = parse(body)?;
            stamp(&mut request.actor, principal);
            reply(queue.mark_ready(request))
        }
        "block" => {
            let mut request: BlockRequest = parse(body)?;
            stamp(&mut request.actor, principal);
            reply(queue.block(request))
        }
        "cancel" => {
            let mut request: CancelRequest = parse(body)?;
            stamp(&mut request.actor, principal);
            reply(queue.cancel(request))
        }
        "delete" => {
            let mut request: DeleteRequest = parse(body)?;
            stamp(&mut request.actor, principal);
            reply(queue.delete(request))
        }
        "claim_next" => {
            let request: ClaimRequest = parse(body)?;
            reply(queue.claim_next(request))
        }
        "heartbeat" => {
            let mut request: HeartbeatRequest = parse(body)?;
            stamp(&mut request.actor, principal);
            reply(queue.heartbeat(request))
        }
        "start" => {
            let mut request: StartRequest = parse(body)?;
            stamp(&mut request.actor, principal);
            reply(queue.start(request))
        }
        "complete" => {
            let mut request: CompleteRequest = parse(body)?;
            stamp(&mut request.actor, principal);
            reply(queue.complete(request))
        }
        "release" => {
            let mut request: ReleaseRequest = parse(body)?;
            stamp(&mut request.actor, principal);
            reply(queue.release(request))
        }
        "recover_stale" => {
            let mut request: RecoverRequest = parse(body)?;
            stamp(&mut request.actor, principal);
            reply(queue.recover_stale(request))
        }
        "events" => {
            let IdBody { id } = parse(body)?;
            reply(queue.events(id))
        }
        "status" => {
            let EmptyBody {} = parse(body)?;
            reply(queue.status())
        }
        "reopen" => {
            let ReopenBody { id, mut actor } = parse(body)?;
            stamp(&mut actor, principal);
            reply(queue.reopen(id, actor))
        }
        "create_feature" => {
            let request: CreateFeatureRequest = parse(body)?;
            reply(queue.create_feature(request))
        }
        "list_features" => {
            let EmptyBody {} = parse(body)?;
            reply(queue.list_features())
        }
        "get_feature" => {
            let IdBody { id } = parse(body)?;
            reply(queue.get_feature(id))
        }
        "edit_feature" => {
            let EditFeatureBody { id, request } = parse(body)?;
            reply(queue.edit_feature(id, request))
        }
        "delete_feature" => {
            let IdBody { id } = parse(body)?;
            reply(queue.delete_feature(id))
        }
        "tree" => {
            let query: TreeQuery = parse(body)?;
            reply(queue.tree(query))
        }
        other => Err(ErrorBody::new(
            "not_found",
            format!("unknown method {other}"),
        )),
    }
}

/// Agent tokens cannot act as humans. The actor id stays if the caller set
/// one (an agent pool may share a token), otherwise it is the token name.
fn stamp(actor: &mut Actor, principal: &Principal) {
    if principal.role == Role::Agent {
        actor.kind = ActorKind::Agent;
        if actor.id.as_deref().is_none_or(str::is_empty) {
            actor.id = Some(principal.name.clone());
        }
    }
}

fn parse<T: DeserializeOwned>(body: &[u8]) -> Result<T, ErrorBody> {
    let body = if body.iter().all(u8::is_ascii_whitespace) {
        b"{}".as_slice()
    } else {
        body
    };
    serde_json::from_slice(body)
        .map_err(|err| ErrorBody::new("invalid_input", format!("invalid request body: {err}")))
}

fn reply<T: Serialize>(result: Result<T, QueueError>) -> Result<Value, ErrorBody> {
    match result {
        Ok(value) => serde_json::to_value(value)
            .map_err(|err| ErrorBody::new("database", format!("cannot encode reply: {err}"))),
        Err(error) => Err(ErrorBody::from(&error)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_file_parses_and_validates() {
        let config = AuthConfig::parse(
            r#"
            [[tokens]]
            name = "me"
            role = "human"
            secret = "0123456789abcdef0123"

            [[tokens]]
            name = "bot"
            role = "agent"
            secret = "fedcba9876543210fedc"
            "#,
        )
        .unwrap();
        assert_eq!(config.tokens.len(), 2);
        let me = config
            .authenticate(Some("Bearer 0123456789abcdef0123"))
            .unwrap();
        assert_eq!(me.role, Role::Human);
        assert_eq!(me.name, "me");
        let bot = config
            .authenticate(Some("Bearer fedcba9876543210fedc"))
            .unwrap();
        assert_eq!(bot.role, Role::Agent);
        assert!(config.authenticate(Some("Bearer nope")).is_none());
        assert!(config.authenticate(Some("Basic abc")).is_none());
        assert!(config.authenticate(None).is_none());
    }

    #[test]
    fn token_file_rejects_short_secrets_and_duplicates() {
        let short = AuthConfig::parse("[[tokens]]\nname=\"a\"\nrole=\"human\"\nsecret=\"short\"\n");
        assert!(short.unwrap_err().contains("at least"));
        let dup = AuthConfig::parse(
            "[[tokens]]\nname=\"a\"\nrole=\"human\"\nsecret=\"0123456789abcdef\"\n[[tokens]]\nname=\"a\"\nrole=\"agent\"\nsecret=\"0123456789abcdefg\"\n",
        );
        assert!(dup.unwrap_err().contains("duplicate"));
        assert!(AuthConfig::parse("").unwrap_err().contains("no tokens"));
    }

    #[test]
    fn unauthenticated_bind_must_be_loopback() {
        let local: SocketAddr = "127.0.0.1:7777".parse().unwrap();
        let public: SocketAddr = "0.0.0.0:7777".parse().unwrap();
        assert!(check_bind(&local, None).is_ok());
        assert!(check_bind(&public, None).is_err());
        let auth = AuthConfig::default();
        assert!(check_bind(&public, Some(&auth)).is_ok());
    }

    #[test]
    fn agents_are_stamped_and_cannot_mark_ready() {
        let agent = Principal {
            name: "bot".into(),
            role: Role::Agent,
        };
        let mut actor = Actor::human(Some("pierric".into()));
        stamp(&mut actor, &agent);
        assert_eq!(actor.kind, ActorKind::Agent);
        assert_eq!(actor.id.as_deref(), Some("pierric"));
        let mut blank = Actor::human(None);
        stamp(&mut blank, &agent);
        assert_eq!(blank.id.as_deref(), Some("bot"));

        struct Never;
        impl QueueService for Never {
            fn capture(&self, _: CaptureRequest) -> Result<q_core::Task, QueueError> {
                unreachable!()
            }
            fn list(&self, _: ListFilter) -> Result<Vec<q_core::TaskSummary>, QueueError> {
                unreachable!()
            }
            fn get(&self, _: i64) -> Result<q_core::TaskDetail, QueueError> {
                unreachable!()
            }
            fn edit(&self, _: i64, _: q_core::EditRequest) -> Result<q_core::Task, QueueError> {
                unreachable!()
            }
            fn mark_ready(&self, _: ReadyRequest) -> Result<q_core::ReadyOutcome, QueueError> {
                unreachable!()
            }
            fn block(&self, _: BlockRequest) -> Result<q_core::Task, QueueError> {
                unreachable!()
            }
            fn cancel(&self, _: CancelRequest) -> Result<q_core::Task, QueueError> {
                unreachable!()
            }
            fn delete(&self, _: DeleteRequest) -> Result<q_core::DeleteOutcome, QueueError> {
                unreachable!()
            }
            fn claim_next(&self, _: ClaimRequest) -> Result<q_core::ClaimOutcome, QueueError> {
                unreachable!()
            }
            fn heartbeat(&self, _: HeartbeatRequest) -> Result<q_core::Claim, QueueError> {
                unreachable!()
            }
            fn start(&self, _: StartRequest) -> Result<q_core::TaskDetail, QueueError> {
                unreachable!()
            }
            fn complete(&self, _: CompleteRequest) -> Result<q_core::TaskDetail, QueueError> {
                unreachable!()
            }
            fn release(&self, _: ReleaseRequest) -> Result<q_core::Task, QueueError> {
                unreachable!()
            }
            fn recover_stale(
                &self,
                _: RecoverRequest,
            ) -> Result<Vec<q_core::RecoveryRecord>, QueueError> {
                unreachable!()
            }
            fn events(&self, _: i64) -> Result<Vec<q_core::Event>, QueueError> {
                unreachable!()
            }
            fn status(&self) -> Result<q_core::QueueStatus, QueueError> {
                unreachable!()
            }
            fn reopen(&self, _: i64, _: Actor) -> Result<q_core::Task, QueueError> {
                unreachable!()
            }
            fn create_feature(
                &self,
                _: CreateFeatureRequest,
            ) -> Result<q_core::Feature, QueueError> {
                unreachable!()
            }
            fn list_features(&self) -> Result<Vec<q_core::Feature>, QueueError> {
                unreachable!()
            }
            fn get_feature(&self, _: i64) -> Result<q_core::Feature, QueueError> {
                unreachable!()
            }
            fn edit_feature(
                &self,
                _: i64,
                _: q_core::EditFeatureRequest,
            ) -> Result<q_core::Feature, QueueError> {
                unreachable!()
            }
            fn delete_feature(&self, _: i64) -> Result<q_core::DeleteFeatureOutcome, QueueError> {
                unreachable!()
            }
            fn tree(&self, _: TreeQuery) -> Result<q_core::TaskTree, QueueError> {
                unreachable!()
            }
        }
        let body = serde_json::to_vec(&ReadyRequest {
            task_id: 1,
            actor: Actor::human(None),
        })
        .unwrap();
        let denied = dispatch(&Never, "mark_ready", &body, &agent).unwrap_err();
        assert_eq!(denied.code, "forbidden");
        let reopen = serde_json::to_vec(&ReopenBody {
            id: 1,
            actor: Actor::human(None),
        })
        .unwrap();
        assert_eq!(
            dispatch(&Never, "reopen", &reopen, &agent)
                .unwrap_err()
                .code,
            "forbidden"
        );
        assert_eq!(
            dispatch(&Never, "nope", b"{}", &agent).unwrap_err().code,
            "not_found"
        );
        assert_eq!(
            dispatch(&Never, "get", b"not json", &agent)
                .unwrap_err()
                .code,
            "invalid_input"
        );
    }
}
