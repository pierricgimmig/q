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

/// Whether a request `Origin` may drive the browser flows served at
/// `public_url`. Exact match, or both loopback on the same scheme and port:
/// a person who opened the page as `localhost` instead of `127.0.0.1` is
/// still on this machine.
pub fn origin_allowed(public_url: &str, origin: &str) -> bool {
    if public_url == origin {
        return true;
    }
    let (Ok(public), Ok(origin)) = (url::Url::parse(public_url), url::Url::parse(origin)) else {
        return false;
    };
    public.scheme() == origin.scheme()
        && public.port_or_known_default() == origin.port_or_known_default()
        && is_loopback(&public)
        && is_loopback(&origin)
}

fn is_loopback(url: &url::Url) -> bool {
    match url.host() {
        Some(url::Host::Domain(host)) => host.eq_ignore_ascii_case("localhost"),
        Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
        Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
        None => false,
    }
}

pub(crate) async fn guard(
    State(state): State<Arc<AppState>>,
    request: Request,
    next: Next,
) -> Response {
    let mut origins = request.headers().get_all(header::ORIGIN).iter();
    if let Some(origin) = origins.next() {
        // "null", multiple origins, and malformed values never convey trust.
        let expected = state.options.public_url.as_deref().unwrap_or("");
        let allowed = origins.next().is_none()
            && origin
                .to_str()
                .ok()
                .and_then(|value| canonical_origin(value).ok())
                .is_some_and(|origin| origin_allowed(expected, &origin));
        if !allowed {
            return (
                StatusCode::FORBIDDEN,
                format!(
                    "untrusted Origin: this server answers for {expected}; open it at that address, or start q serve with --public-url set to the address you use"
                ),
            )
                .into_response();
        }
    }
    next.run(request).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loopback_aliases_share_trust_but_nothing_else_does() {
        assert!(origin_allowed(
            "http://127.0.0.1:7777",
            "http://127.0.0.1:7777"
        ));
        assert!(origin_allowed(
            "http://127.0.0.1:7777",
            "http://localhost:7777"
        ));
        assert!(origin_allowed("http://localhost:7777", "http://[::1]:7777"));
        assert!(!origin_allowed(
            "http://127.0.0.1:7777",
            "http://localhost:7778"
        ));
        assert!(!origin_allowed(
            "http://127.0.0.1:7777",
            "https://localhost:7777"
        ));
        assert!(!origin_allowed(
            "https://q.example.com",
            "https://localhost"
        ));
        assert!(!origin_allowed(
            "https://q.example.com",
            "https://q.example.com.evil.net"
        ));
        assert!(!origin_allowed("http://127.0.0.1:7777", "not a url"));
    }
}
