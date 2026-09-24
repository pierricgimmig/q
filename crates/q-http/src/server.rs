//! `q serve`: the queue as a single HTTP authority.
//!
//! One process owns the SQLite file. It answers:
//!
//! - `POST /v1/<method>` for the CLI and `q mcp` running elsewhere
//!   (the `rpc` module);
//! - `POST /mcp` for chat apps and IDE agents speaking MCP over HTTP
//!   (the `mcp` module);
//! - the OAuth endpoints chat connectors use to sign in ([`crate::oauth`]).
//!
//! Claims stay serialized by the same `BEGIN IMMEDIATE` transaction they use
//! locally, so two remote agents cannot take the same task. There is no
//! replica and no sync.
//!
//! # Auth
//!
//! Requests carry `Authorization: Bearer <value>`, where the value is either
//! a secret from the token file or an access token this server issued. Tokens
//! have a role. `human` tokens may call everything. `agent` tokens may not
//! call the methods in [`crate::wire::HUMAN_ONLY_METHODS`], and any actor
//! they send is stamped as an agent. Without a token file the server accepts
//! unauthenticated requests as an anonymous human and refuses to bind
//! anything but a loopback address.

use std::future::Future;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::{Path as UrlPath, Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum::{Form, Json, Router};
use q_core::QueueService;
use serde_json::Value;
use tokio::net::TcpListener;

use crate::auth::{Principal, TokenStore};
use crate::oauth::{
    base_url, sign_in_page, AuthorizeForm, AuthorizeParams, OAuthError, OAuthServer,
    RegisterRequest, SigningKey, TokenForm,
};
use crate::wire::{ErrorBody, ErrorEnvelope, HealthBody};

pub struct ServerOptions {
    /// `None` accepts every request as an anonymous human (loopback only).
    pub auth: Option<TokenStore>,
    /// Public origin such as `https://q.example.com`. Used in OAuth metadata
    /// and redirects. Derived from the reverse proxy's forwarded headers when
    /// unset.
    pub public_url: Option<String>,
    /// Key that signs OAuth client ids and tokens.
    pub signing_key: SigningKey,
    /// Directory MCP captures discover their repo and project from.
    pub base_dir: PathBuf,
}

impl ServerOptions {
    /// Loopback defaults with a throwaway signing key. Tests and local runs.
    pub fn local(auth: Option<TokenStore>) -> Self {
        Self {
            auth,
            public_url: None,
            signing_key: SigningKey::ephemeral(),
            base_dir: std::env::temp_dir(),
        }
    }
}

pub struct AppState {
    pub queue: Arc<dyn QueueService>,
    pub options: ServerOptions,
    pub oauth: OAuthServer,
}

/// Build the router.
pub fn router(queue: Arc<dyn QueueService>, options: ServerOptions) -> Router {
    let oauth = OAuthServer::new(options.signing_key.clone());
    let state = Arc::new(AppState {
        queue,
        options,
        oauth,
    });
    Router::new()
        .route("/v1/health", get(health))
        .route("/v1/:method", post(rpc))
        .route(
            "/mcp",
            post(crate::mcp::post)
                .get(crate::mcp::get)
                .delete(crate::mcp::delete),
        )
        .route(
            "/.well-known/oauth-authorization-server",
            get(authorization_server_metadata),
        )
        .route(
            "/.well-known/oauth-authorization-server/mcp",
            get(authorization_server_metadata),
        )
        .route(
            "/.well-known/oauth-protected-resource",
            get(protected_resource_metadata),
        )
        .route(
            "/.well-known/oauth-protected-resource/mcp",
            get(protected_resource_metadata),
        )
        .route("/oauth/register", post(oauth_register))
        .route(
            "/oauth/authorize",
            get(oauth_authorize_page).post(oauth_authorize_submit),
        )
        .route("/oauth/token", post(oauth_token))
        .with_state(state)
}

/// Refuse to expose an unauthenticated server beyond this machine.
pub fn check_bind(addr: &SocketAddr, auth: Option<&TokenStore>) -> Result<(), String> {
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
    options: ServerOptions,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> std::io::Result<()> {
    let app = router(queue, options);
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown)
        .await
}

/// Resolve the caller. Raw token-file secrets and issued access tokens are
/// both accepted.
pub fn authenticate(state: &AppState, headers: &HeaderMap) -> Result<Principal, ErrorBody> {
    let Some(store) = &state.options.auth else {
        return Ok(Principal::anonymous());
    };
    let bearer = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .map(str::trim)
        .filter(|value| !value.is_empty());
    let Some(bearer) = bearer else {
        return Err(ErrorBody::new(
            "unauthorized",
            "missing or invalid bearer token",
        ));
    };
    state
        .oauth
        .authenticate(&store.snapshot(), bearer)
        .ok_or_else(|| ErrorBody::new("unauthorized", "missing or invalid bearer token"))
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
    let principal = match authenticate(&state, &headers) {
        Ok(principal) => principal,
        Err(error) => return error_response(error),
    };
    let queue = state.queue.clone();
    let outcome = tokio::task::spawn_blocking(move || {
        crate::rpc::dispatch(queue.as_ref(), &method, &body, &principal)
    })
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

// ---- OAuth ---------------------------------------------------------------

fn no_store<T: IntoResponse>(inner: T) -> Response {
    let mut response = inner.into_response();
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, "no-store".parse().unwrap());
    response
}

fn oauth_error(status: StatusCode, error: OAuthError) -> Response {
    no_store((status, Json(error.to_json())))
}

async fn authorization_server_metadata(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Json<Value> {
    let base = base_url(state.options.public_url.as_deref(), &headers);
    Json(state.oauth.authorization_server_metadata(&base))
}

async fn protected_resource_metadata(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Json<Value> {
    let base = base_url(state.options.public_url.as_deref(), &headers);
    Json(state.oauth.protected_resource_metadata(&base))
}

async fn oauth_register(State(state): State<Arc<AppState>>, body: Bytes) -> Response {
    let request: RegisterRequest = match serde_json::from_slice(&body) {
        Ok(request) => request,
        Err(err) => {
            return oauth_error(
                StatusCode::BAD_REQUEST,
                OAuthError {
                    code: "invalid_client_metadata",
                    description: format!("invalid registration body: {err}"),
                },
            )
        }
    };
    match state.oauth.register(request) {
        Ok(client) => no_store((StatusCode::CREATED, Json(client))),
        Err(error) => oauth_error(StatusCode::BAD_REQUEST, error),
    }
}

async fn oauth_authorize_page(
    State(state): State<Arc<AppState>>,
    Query(params): Query<AuthorizeParams>,
) -> Response {
    if let Err(error) = state.oauth.check_authorize(&params) {
        return no_store((
            StatusCode::BAD_REQUEST,
            Html(format!(
                "<!doctype html><title>q</title><p>Cannot sign in: {} ({}).</p>",
                crate::oauth::html_escape(&error.description),
                error.code
            )),
        ));
    }
    let hint = if state.options.auth.is_none() {
        Some("This server has no token file, so nobody can sign in. Start it with --auth.")
    } else {
        None
    };
    no_store(Html(sign_in_page(
        &params,
        &state.oauth.client_name(&params.client_id),
        hint,
    )))
}

async fn oauth_authorize_submit(
    State(state): State<Arc<AppState>>,
    Form(form): Form<AuthorizeForm>,
) -> Response {
    let tokens = state
        .options
        .auth
        .as_ref()
        .map(TokenStore::snapshot)
        .unwrap_or_default();
    match state.oauth.authorize(&tokens, &form) {
        Ok(url) => no_store(Redirect::to(&url)),
        Err(error) if error.code == "access_denied" => no_store(Html(sign_in_page(
            &form.params,
            &state.oauth.client_name(&form.params.client_id),
            Some("That token was not recognized. Check tokens.toml and try again."),
        ))),
        Err(error) => oauth_error(StatusCode::BAD_REQUEST, error),
    }
}

async fn oauth_token(State(state): State<Arc<AppState>>, Form(form): Form<TokenForm>) -> Response {
    let tokens = state
        .options
        .auth
        .as_ref()
        .map(TokenStore::snapshot)
        .unwrap_or_default();
    match state.oauth.token(&tokens, &form) {
        Ok(issued) => no_store(Json(issued)),
        Err(error) => oauth_error(StatusCode::BAD_REQUEST, error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::AuthConfig;

    #[test]
    fn unauthenticated_bind_must_be_loopback() {
        let local: SocketAddr = "127.0.0.1:7777".parse().unwrap();
        let public: SocketAddr = "0.0.0.0:7777".parse().unwrap();
        assert!(check_bind(&local, None).is_ok());
        assert!(check_bind(&public, None).is_err());
        let auth = TokenStore::fixed(AuthConfig::default());
        assert!(check_bind(&public, Some(&auth)).is_ok());
    }
}
