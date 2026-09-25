//! Browser origins are configuration, never inferred from request headers.

use axum::extract::{Request, State};
use axum::http::{header, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use std::sync::Arc;

use crate::server::AppState;

/// Normalize a scheme/host/port origin, rejecting URL credentials and other parts.
pub fn canonical_origin(value: &str) -> Result<String, String> {
    let url = url::Url::parse(value).map_err(|_| format!("invalid origin: {value}"))?;
    if !matches!(url.scheme(), "https" | "http")
        || !url.has_host()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.path() != "/"
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(format!(
            "expected an http(s) origin with no path, credentials, query, or fragment: {value}"
        ));
    }
    Ok(url.origin().ascii_serialization())
}

pub(crate) async fn guard(
    State(state): State<Arc<AppState>>,
    request: Request,
    next: Next,
) -> Response {
    let mut origins = request.headers().get_all(header::ORIGIN).iter();
    if let Some(origin) = origins.next() {
        // "null", multiple origins, and malformed values never convey trust.
        let allowed = origins.next().is_none()
            && origin
                .to_str()
                .ok()
                .and_then(|value| canonical_origin(value).ok())
                .is_some_and(|origin| state.options.public_url.as_deref() == Some(origin.as_str()));
        if !allowed {
            return (StatusCode::FORBIDDEN, "untrusted Origin").into_response();
        }
    }
    next.run(request).await
}
