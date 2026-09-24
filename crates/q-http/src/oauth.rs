//! A minimal OAuth 2.1 authorization server so chat apps can add `q serve`
//! as a custom connector. Grok, Claude, and ChatGPT all discover the
//! endpoints from `/.well-known/oauth-authorization-server`, register a
//! client dynamically, run the PKCE code flow, and then call `/mcp` with a
//! bearer token.
//!
//! Sign-in is deliberately simple: the authorize page asks for one of the
//! secrets in the token file. The connector then acts with that token's role.
//!
//! There is no database. Client ids, access tokens, and refresh tokens are
//! HMAC-signed payloads. The signing key lives in a small key file next to
//! the queue database and is created on first start. Per-principal tokens
//! are signed with a key derived from that principal's secret, so revoking
//! or rotating a token in the token file invalidates every session it
//! signed in. Authorization codes are the only in-memory state and expire in
//! ten minutes.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Mutex;

use axum::http::HeaderMap;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::auth::{AuthConfig, Principal};
use crate::crypto::{
    base64url_decode, base64url_encode, constant_time_eq, hmac_sha256, random_bytes, sha256,
    unix_now,
};

pub const ACCESS_TTL_SECS: u64 = 24 * 60 * 60;
pub const REFRESH_TTL_SECS: u64 = 90 * 24 * 60 * 60;
const CODE_TTL_SECS: u64 = 10 * 60;
const TOKEN_PREFIX: &str = "q1.";

/// Server signing key. 32 random bytes stored as hex in a 0600 file.
#[derive(Clone)]
pub struct SigningKey([u8; 32]);

impl SigningKey {
    pub fn load_or_create(path: &Path) -> Result<Self, String> {
        if path.exists() {
            let text = std::fs::read_to_string(path)
                .map_err(|err| format!("cannot read {}: {err}", path.display()))?;
            let bytes = base64url_decode(text.trim())
                .filter(|bytes| bytes.len() == 32)
                .ok_or_else(|| format!("{} is not a valid key file", path.display()))?;
            let mut key = [0u8; 32];
            key.copy_from_slice(&bytes);
            return Ok(Self(key));
        }
        let key = random_bytes();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|err| format!("cannot create {}: {err}", parent.display()))?;
        }
        std::fs::write(path, base64url_encode(&key))
            .map_err(|err| format!("cannot write {}: {err}", path.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
        }
        Ok(Self(key))
    }

    pub fn ephemeral() -> Self {
        Self(random_bytes())
    }

    fn client_key(&self) -> [u8; 32] {
        hmac_sha256(&self.0, b"q-oauth-client")
    }

    fn principal_key(&self, name: &str, secret: &str) -> [u8; 32] {
        hmac_sha256(
            &self.0,
            format!("q-oauth-principal:{name}:{secret}").as_bytes(),
        )
    }
}

fn sign(key: &[u8], payload: &[u8]) -> String {
    format!(
        "{TOKEN_PREFIX}{}.{}",
        base64url_encode(payload),
        base64url_encode(&hmac_sha256(key, payload))
    )
}

/// Split a signed token into its payload without checking the signature.
/// The caller looks up the key from the payload and then calls [`verify`].
fn unsigned_payload(token: &str) -> Option<(Vec<u8>, Vec<u8>)> {
    let rest = token.strip_prefix(TOKEN_PREFIX)?;
    let (payload, mac) = rest.split_once('.')?;
    Some((base64url_decode(payload)?, base64url_decode(mac)?))
}

fn verify(key: &[u8], payload: &[u8], mac: &[u8]) -> bool {
    constant_time_eq(&hmac_sha256(key, payload), mac)
}

#[derive(Debug, Serialize, Deserialize)]
struct ClientPayload {
    #[serde(rename = "t")]
    kind: String,
    redirect_uris: Vec<String>,
    #[serde(default)]
    name: String,
    iat: u64,
}

#[derive(Debug, Serialize, Deserialize)]
struct TokenPayload {
    #[serde(rename = "t")]
    kind: String,
    #[serde(rename = "n")]
    name: String,
    exp: u64,
    #[serde(rename = "j")]
    nonce: String,
}

#[derive(Debug, Clone)]
struct PendingCode {
    client_id: String,
    redirect_uri: String,
    code_challenge: String,
    principal: String,
    expires_at: u64,
}

pub struct OAuthServer {
    key: SigningKey,
    codes: Mutex<HashMap<String, PendingCode>>,
}

/// A failed OAuth request: the RFC 6749 error code and a description.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OAuthError {
    pub code: &'static str,
    pub description: String,
}

impl OAuthError {
    fn new(code: &'static str, description: impl Into<String>) -> Self {
        Self {
            code,
            description: description.into(),
        }
    }

    pub fn to_json(&self) -> Value {
        json!({ "error": self.code, "error_description": self.description })
    }
}

/// Registration request body (RFC 7591). Only the fields we act on.
#[derive(Debug, Default, Deserialize)]
pub struct RegisterRequest {
    #[serde(default)]
    pub redirect_uris: Vec<String>,
    #[serde(default)]
    pub client_name: Option<String>,
}

/// Query string for `GET /oauth/authorize`, also echoed by the sign-in form.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct AuthorizeParams {
    #[serde(default)]
    pub response_type: String,
    #[serde(default)]
    pub client_id: String,
    #[serde(default)]
    pub redirect_uri: String,
    #[serde(default)]
    pub code_challenge: String,
    #[serde(default)]
    pub code_challenge_method: String,
    #[serde(default)]
    pub state: Option<String>,
    #[serde(default)]
    pub scope: Option<String>,
    #[serde(default)]
    pub resource: Option<String>,
}

/// Form body for `POST /oauth/authorize`: the query fields plus the secret.
#[derive(Debug, Deserialize)]
pub struct AuthorizeForm {
    #[serde(flatten)]
    pub params: AuthorizeParams,
    #[serde(default)]
    pub token: String,
}

#[derive(Debug, Default, Deserialize)]
pub struct TokenForm {
    #[serde(default)]
    pub grant_type: String,
    #[serde(default)]
    pub code: Option<String>,
    #[serde(default)]
    pub code_verifier: Option<String>,
    #[serde(default)]
    pub redirect_uri: Option<String>,
    #[serde(default)]
    pub client_id: Option<String>,
    #[serde(default)]
    pub refresh_token: Option<String>,
}

impl OAuthServer {
    pub fn new(key: SigningKey) -> Self {
        Self {
            key,
            codes: Mutex::new(HashMap::new()),
        }
    }

    pub fn authorization_server_metadata(&self, base: &str) -> Value {
        json!({
            "issuer": base,
            "authorization_endpoint": format!("{base}/oauth/authorize"),
            "token_endpoint": format!("{base}/oauth/token"),
            "registration_endpoint": format!("{base}/oauth/register"),
            "response_types_supported": ["code"],
            "response_modes_supported": ["query"],
            "grant_types_supported": ["authorization_code", "refresh_token"],
            "code_challenge_methods_supported": ["S256"],
            "token_endpoint_auth_methods_supported": ["none"],
            "scopes_supported": ["mcp"],
        })
    }

    pub fn protected_resource_metadata(&self, base: &str) -> Value {
        json!({
            "resource": format!("{base}/mcp"),
            "authorization_servers": [base],
            "bearer_methods_supported": ["header"],
            "scopes_supported": ["mcp"],
        })
    }

    /// Dynamic client registration. The client id carries the registration,
    /// signed, so nothing is stored.
    pub fn register(&self, request: RegisterRequest) -> Result<Value, OAuthError> {
        if request.redirect_uris.is_empty() {
            return Err(OAuthError::new(
                "invalid_redirect_uri",
                "redirect_uris is required",
            ));
        }
        for uri in &request.redirect_uris {
            if !redirect_uri_allowed(uri) {
                return Err(OAuthError::new(
                    "invalid_redirect_uri",
                    format!("redirect_uri must be https or loopback: {uri}"),
                ));
            }
        }
        let payload = ClientPayload {
            kind: "client".into(),
            redirect_uris: request.redirect_uris.clone(),
            name: request.client_name.clone().unwrap_or_default(),
            iat: unix_now(),
        };
        let bytes = serde_json::to_vec(&payload).map_err(|err| {
            OAuthError::new("server_error", format!("cannot encode client: {err}"))
        })?;
        let client_id = sign(&self.key.client_key(), &bytes);
        Ok(json!({
            "client_id": client_id,
            "client_id_issued_at": payload.iat,
            "client_name": payload.name,
            "redirect_uris": payload.redirect_uris,
            "token_endpoint_auth_method": "none",
            "grant_types": ["authorization_code", "refresh_token"],
            "response_types": ["code"],
        }))
    }

    /// Display name a client registered with, or empty.
    pub fn client_name(&self, client_id: &str) -> String {
        self.client(client_id).map(|c| c.name).unwrap_or_default()
    }

    fn client(&self, client_id: &str) -> Result<ClientPayload, OAuthError> {
        let (payload, mac) = unsigned_payload(client_id)
            .ok_or_else(|| OAuthError::new("invalid_client", "malformed client_id"))?;
        if !verify(&self.key.client_key(), &payload, &mac) {
            return Err(OAuthError::new("invalid_client", "unknown client_id"));
        }
        let client: ClientPayload = serde_json::from_slice(&payload)
            .map_err(|_| OAuthError::new("invalid_client", "malformed client_id"))?;
        if client.kind != "client" {
            return Err(OAuthError::new("invalid_client", "not a client_id"));
        }
        Ok(client)
    }

    /// Validate an authorize request before showing the sign-in form. Errors
    /// here must be shown to the user, not redirected, because the redirect
    /// target itself may be wrong.
    pub fn check_authorize(&self, params: &AuthorizeParams) -> Result<(), OAuthError> {
        let client = self.client(&params.client_id)?;
        if !client
            .redirect_uris
            .iter()
            .any(|uri| uri == &params.redirect_uri)
        {
            return Err(OAuthError::new(
                "invalid_request",
                "redirect_uri is not registered for this client",
            ));
        }
        if params.response_type != "code" {
            return Err(OAuthError::new(
                "unsupported_response_type",
                "response_type must be code",
            ));
        }
        if params.code_challenge_method != "S256" || params.code_challenge.is_empty() {
            return Err(OAuthError::new(
                "invalid_request",
                "PKCE with code_challenge_method=S256 is required",
            ));
        }
        Ok(())
    }

    /// Complete sign-in. `secret` is a token-file secret. Returns the
    /// redirect URL carrying the authorization code.
    pub fn authorize(
        &self,
        tokens: &AuthConfig,
        form: &AuthorizeForm,
    ) -> Result<String, OAuthError> {
        self.check_authorize(&form.params)?;
        let token = tokens
            .find_by_secret(&form.token)
            .ok_or_else(|| OAuthError::new("access_denied", "that token is not recognized"))?;
        let code = base64url_encode(&random_bytes());
        let pending = PendingCode {
            client_id: form.params.client_id.clone(),
            redirect_uri: form.params.redirect_uri.clone(),
            code_challenge: form.params.code_challenge.clone(),
            principal: token.name.clone(),
            expires_at: unix_now() + CODE_TTL_SECS,
        };
        if let Ok(mut codes) = self.codes.lock() {
            let now = unix_now();
            codes.retain(|_, pending| pending.expires_at > now);
            codes.insert(code.clone(), pending);
        }
        let mut url = format!(
            "{}{}code={}",
            form.params.redirect_uri,
            if form.params.redirect_uri.contains('?') {
                '&'
            } else {
                '?'
            },
            urlencode(&code)
        );
        if let Some(state) = &form.params.state {
            url.push_str("&state=");
            url.push_str(&urlencode(state));
        }
        Ok(url)
    }

    /// Exchange a code or refresh token for a new token pair.
    pub fn token(&self, tokens: &AuthConfig, form: &TokenForm) -> Result<Value, OAuthError> {
        let principal_name = match form.grant_type.as_str() {
            "authorization_code" => self.redeem_code(form)?,
            "refresh_token" => {
                let refresh = form.refresh_token.as_deref().ok_or_else(|| {
                    OAuthError::new("invalid_request", "refresh_token is required")
                })?;
                let principal = self
                    .principal_from_token(tokens, refresh, "refresh")
                    .ok_or_else(|| {
                        OAuthError::new("invalid_grant", "refresh token is not valid")
                    })?;
                principal.name
            }
            other => {
                return Err(OAuthError::new(
                    "unsupported_grant_type",
                    format!("grant_type {other} is not supported"),
                ))
            }
        };
        let token = tokens.find_by_name(&principal_name).ok_or_else(|| {
            OAuthError::new("invalid_grant", "the token used to sign in was revoked")
        })?;
        let key = self.key.principal_key(&token.name, &token.secret);
        let access = self.issue(&key, &token.name, "access", ACCESS_TTL_SECS)?;
        let refresh = self.issue(&key, &token.name, "refresh", REFRESH_TTL_SECS)?;
        Ok(json!({
            "access_token": access,
            "token_type": "Bearer",
            "expires_in": ACCESS_TTL_SECS,
            "refresh_token": refresh,
            "scope": "mcp",
        }))
    }

    fn redeem_code(&self, form: &TokenForm) -> Result<String, OAuthError> {
        let code = form
            .code
            .as_deref()
            .ok_or_else(|| OAuthError::new("invalid_request", "code is required"))?;
        let verifier = form
            .code_verifier
            .as_deref()
            .ok_or_else(|| OAuthError::new("invalid_request", "code_verifier is required"))?;
        let pending = self
            .codes
            .lock()
            .ok()
            .and_then(|mut codes| codes.remove(code))
            .ok_or_else(|| OAuthError::new("invalid_grant", "unknown or already used code"))?;
        if pending.expires_at < unix_now() {
            return Err(OAuthError::new("invalid_grant", "code expired"));
        }
        if let Some(client_id) = &form.client_id {
            if client_id != &pending.client_id {
                return Err(OAuthError::new("invalid_grant", "client_id mismatch"));
            }
        }
        if let Some(redirect_uri) = &form.redirect_uri {
            if redirect_uri != &pending.redirect_uri {
                return Err(OAuthError::new("invalid_grant", "redirect_uri mismatch"));
            }
        }
        let expected = base64url_encode(&sha256(verifier.as_bytes()));
        if !constant_time_eq(expected.as_bytes(), pending.code_challenge.as_bytes()) {
            return Err(OAuthError::new("invalid_grant", "PKCE verification failed"));
        }
        Ok(pending.principal)
    }

    fn issue(&self, key: &[u8], name: &str, kind: &str, ttl: u64) -> Result<String, OAuthError> {
        let payload = TokenPayload {
            kind: kind.into(),
            name: name.into(),
            exp: unix_now() + ttl,
            nonce: base64url_encode(&random_bytes()[..12]),
        };
        let bytes = serde_json::to_vec(&payload).map_err(|err| {
            OAuthError::new("server_error", format!("cannot encode token: {err}"))
        })?;
        Ok(sign(key, &bytes))
    }

    /// Resolve a bearer value issued by this server. `None` if it is not one
    /// of ours, is expired, or its principal is gone from the token file.
    pub fn principal_from_token(
        &self,
        tokens: &AuthConfig,
        bearer: &str,
        kind: &str,
    ) -> Option<Principal> {
        let (payload, mac) = unsigned_payload(bearer)?;
        let claims: TokenPayload = serde_json::from_slice(&payload).ok()?;
        if claims.kind != kind || claims.exp < unix_now() {
            return None;
        }
        let token = tokens.find_by_name(&claims.name)?;
        let key = self.key.principal_key(&token.name, &token.secret);
        if !verify(&key, &payload, &mac) {
            return None;
        }
        Some(Principal::from_token(token))
    }

    /// Resolve any bearer: a raw token-file secret or an access token.
    pub fn authenticate(&self, tokens: &AuthConfig, bearer: &str) -> Option<Principal> {
        if let Some(token) = tokens.find_by_secret(bearer) {
            return Some(Principal::from_token(token));
        }
        self.principal_from_token(tokens, bearer, "access")
    }
}

fn redirect_uri_allowed(uri: &str) -> bool {
    uri.starts_with("https://")
        || uri.starts_with("http://localhost")
        || uri.starts_with("http://127.0.0.1")
        || uri.starts_with("http://[::1]")
}

pub fn urlencode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

pub fn html_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// The sign-in page. One field: a token-file secret.
pub fn sign_in_page(params: &AuthorizeParams, client_name: &str, error: Option<&str>) -> String {
    let hidden = [
        ("response_type", params.response_type.as_str()),
        ("client_id", params.client_id.as_str()),
        ("redirect_uri", params.redirect_uri.as_str()),
        ("code_challenge", params.code_challenge.as_str()),
        (
            "code_challenge_method",
            params.code_challenge_method.as_str(),
        ),
        ("state", params.state.as_deref().unwrap_or("")),
        ("scope", params.scope.as_deref().unwrap_or("")),
        ("resource", params.resource.as_deref().unwrap_or("")),
    ]
    .iter()
    .filter(|(_, value)| !value.is_empty())
    .map(|(name, value)| {
        format!(
            "<input type=\"hidden\" name=\"{name}\" value=\"{}\">",
            html_escape(value)
        )
    })
    .collect::<Vec<_>>()
    .join("\n      ");
    let error = error
        .map(|text| format!("<p class=\"error\">{}</p>", html_escape(text)))
        .unwrap_or_default();
    let who = if client_name.is_empty() {
        "A connector".to_string()
    } else {
        html_escape(client_name)
    };
    format!(
        r#"<!doctype html>
<html lang="en">
<head>
  <meta charset="utf-8">
  <meta name="viewport" content="width=device-width, initial-scale=1">
  <title>Sign in to q</title>
  <style>
    body {{ font: 16px/1.5 system-ui, sans-serif; margin: 0; padding: 48px 16px; background: #f5f5f4; color: #1c1917; }}
    main {{ max-width: 380px; margin: 0 auto; background: #fff; border-radius: 12px; padding: 28px; box-shadow: 0 1px 3px rgba(0,0,0,.1); }}
    h1 {{ font-size: 20px; margin: 0 0 8px; }}
    p {{ margin: 0 0 16px; color: #57534e; }}
    input[type=password] {{ width: 100%; box-sizing: border-box; font: inherit; padding: 10px 12px; border: 1px solid #d6d3d1; border-radius: 8px; }}
    button {{ margin-top: 14px; width: 100%; font: inherit; font-weight: 600; padding: 10px; border: 0; border-radius: 8px; background: #1c1917; color: #fff; cursor: pointer; }}
    .error {{ color: #b91c1c; }}
    @media (prefers-color-scheme: dark) {{
      body {{ background: #1c1917; color: #fafaf9; }}
      main {{ background: #292524; }}
      p {{ color: #a8a29e; }}
      input[type=password] {{ background: #1c1917; color: #fafaf9; border-color: #44403c; }}
      button {{ background: #fafaf9; color: #1c1917; }}
    }}
  </style>
</head>
<body>
  <main>
    <h1>Sign in to q</h1>
    <p>{who} wants to use your queue. Paste a token from your <code>tokens.toml</code>. The connector gets that token's role.</p>
    {error}
    <form method="post" action="/oauth/authorize">
      {hidden}
      <input type="password" name="token" placeholder="q token" autocomplete="off" autofocus required>
      <button type="submit">Allow</button>
    </form>
  </main>
</body>
</html>
"#
    )
}

/// Public origin for metadata and redirects: the configured URL, else the
/// forwarded scheme and host from the reverse proxy, else the Host header.
pub fn base_url(configured: Option<&str>, headers: &HeaderMap) -> String {
    if let Some(url) = configured {
        return url.trim_end_matches('/').to_string();
    }
    let proto = headers
        .get("x-forwarded-proto")
        .and_then(|v| v.to_str().ok())
        .map(|v| v.split(',').next().unwrap_or(v).trim().to_string())
        .filter(|v| v == "http" || v == "https")
        .unwrap_or_else(|| "http".to_string());
    let host = headers
        .get("x-forwarded-host")
        .or_else(|| headers.get(axum::http::header::HOST))
        .and_then(|v| v.to_str().ok())
        .map(|v| v.split(',').next().unwrap_or(v).trim().to_string())
        .unwrap_or_else(|| "localhost".to_string());
    format!("{proto}://{host}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::Role;

    fn tokens() -> AuthConfig {
        AuthConfig::parse(
            "[[tokens]]\nname=\"me\"\nrole=\"human\"\nsecret=\"human-secret-0123456789\"\n[[tokens]]\nname=\"bot\"\nrole=\"agent\"\nsecret=\"agent-secret-0123456789\"\n",
        )
        .unwrap()
    }

    fn pkce() -> (String, String) {
        let verifier = base64url_encode(&random_bytes());
        let challenge = base64url_encode(&sha256(verifier.as_bytes()));
        (verifier, challenge)
    }

    fn registered(server: &OAuthServer) -> String {
        server
            .register(RegisterRequest {
                redirect_uris: vec!["https://chat.example/callback".into()],
                client_name: Some("Grok".into()),
            })
            .unwrap()["client_id"]
            .as_str()
            .unwrap()
            .to_string()
    }

    #[test]
    fn code_flow_with_pkce_issues_tokens_bound_to_the_principal() {
        let server = OAuthServer::new(SigningKey::ephemeral());
        let tokens = tokens();
        let client_id = registered(&server);
        let (verifier, challenge) = pkce();
        let params = AuthorizeParams {
            response_type: "code".into(),
            client_id: client_id.clone(),
            redirect_uri: "https://chat.example/callback".into(),
            code_challenge: challenge,
            code_challenge_method: "S256".into(),
            state: Some("xyz".into()),
            ..AuthorizeParams::default()
        };
        server.check_authorize(&params).unwrap();

        let wrong = server.authorize(
            &tokens,
            &AuthorizeForm {
                params: params.clone(),
                token: "nope".into(),
            },
        );
        assert_eq!(wrong.unwrap_err().code, "access_denied");

        let redirect = server
            .authorize(
                &tokens,
                &AuthorizeForm {
                    params: params.clone(),
                    token: "human-secret-0123456789".into(),
                },
            )
            .unwrap();
        assert!(redirect.starts_with("https://chat.example/callback?code="));
        assert!(redirect.ends_with("&state=xyz"));
        let code = redirect
            .split("code=")
            .nth(1)
            .unwrap()
            .split('&')
            .next()
            .unwrap()
            .to_string();

        let bad_verifier = server.token(
            &tokens,
            &TokenForm {
                grant_type: "authorization_code".into(),
                code: Some(code.clone()),
                code_verifier: Some("wrong".into()),
                redirect_uri: Some(params.redirect_uri.clone()),
                client_id: Some(client_id.clone()),
                refresh_token: None,
            },
        );
        // A failed exchange consumes the code, as the spec requires.
        assert_eq!(bad_verifier.unwrap_err().code, "invalid_grant");

        let redirect = server
            .authorize(
                &tokens,
                &AuthorizeForm {
                    params: params.clone(),
                    token: "human-secret-0123456789".into(),
                },
            )
            .unwrap();
        let code = redirect
            .split("code=")
            .nth(1)
            .unwrap()
            .split('&')
            .next()
            .unwrap()
            .to_string();
        let issued = server
            .token(
                &tokens,
                &TokenForm {
                    grant_type: "authorization_code".into(),
                    code: Some(code.clone()),
                    code_verifier: Some(verifier.clone()),
                    redirect_uri: Some(params.redirect_uri.clone()),
                    client_id: Some(client_id.clone()),
                    refresh_token: None,
                },
            )
            .unwrap();
        let access = issued["access_token"].as_str().unwrap();
        let refresh = issued["refresh_token"].as_str().unwrap();
        assert_eq!(issued["token_type"], "Bearer");

        let principal = server.authenticate(&tokens, access).unwrap();
        assert_eq!(principal.name, "me");
        assert_eq!(principal.role, Role::Human);
        // Refresh tokens are not access tokens, and codes are one-shot.
        assert!(server.authenticate(&tokens, refresh).is_none());
        assert!(server
            .token(
                &tokens,
                &TokenForm {
                    grant_type: "authorization_code".into(),
                    code: Some(code),
                    code_verifier: Some(verifier),
                    redirect_uri: None,
                    client_id: None,
                    refresh_token: None,
                },
            )
            .is_err());

        let refreshed = server
            .token(
                &tokens,
                &TokenForm {
                    grant_type: "refresh_token".into(),
                    refresh_token: Some(refresh.into()),
                    ..TokenForm::default()
                },
            )
            .unwrap();
        assert!(server
            .authenticate(&tokens, refreshed["access_token"].as_str().unwrap())
            .is_some());

        // Revoking the underlying token kills every session it signed in.
        let mut revoked = tokens.clone();
        revoked.tokens.retain(|token| token.name != "me");
        assert!(server.authenticate(&revoked, access).is_none());
        // Rotating the secret does too.
        let mut rotated = tokens.clone();
        rotated.tokens[0].secret = "rotated-secret-0123456789".into();
        assert!(server.authenticate(&rotated, access).is_none());
        // A raw secret still works as a bearer.
        assert_eq!(
            server
                .authenticate(&tokens, "agent-secret-0123456789")
                .unwrap()
                .role,
            Role::Agent
        );
        // Tokens from another server's key are rejected.
        let other = OAuthServer::new(SigningKey::ephemeral());
        assert!(other.authenticate(&tokens, access).is_none());
    }

    #[test]
    fn registration_and_authorize_checks() {
        let server = OAuthServer::new(SigningKey::ephemeral());
        assert_eq!(
            server
                .register(RegisterRequest::default())
                .unwrap_err()
                .code,
            "invalid_redirect_uri"
        );
        assert_eq!(
            server
                .register(RegisterRequest {
                    redirect_uris: vec!["http://evil.example/cb".into()],
                    client_name: None,
                })
                .unwrap_err()
                .code,
            "invalid_redirect_uri"
        );
        let client_id = registered(&server);
        let mut params = AuthorizeParams {
            response_type: "code".into(),
            client_id: client_id.clone(),
            redirect_uri: "https://other.example/cb".into(),
            code_challenge: "abc".into(),
            code_challenge_method: "S256".into(),
            ..AuthorizeParams::default()
        };
        assert_eq!(
            server.check_authorize(&params).unwrap_err().code,
            "invalid_request"
        );
        params.redirect_uri = "https://chat.example/callback".into();
        params.code_challenge_method = "plain".into();
        assert_eq!(
            server.check_authorize(&params).unwrap_err().code,
            "invalid_request"
        );
        params.code_challenge_method = "S256".into();
        params.client_id = "q1.bogus.bogus".into();
        assert_eq!(
            server.check_authorize(&params).unwrap_err().code,
            "invalid_client"
        );
        params.client_id = client_id;
        server.check_authorize(&params).unwrap();
        let page = sign_in_page(&params, "Grok <3", Some("bad \"token\""));
        assert!(page.contains("Grok &lt;3"));
        assert!(page.contains("bad &quot;token&quot;"));
        assert!(page.contains("name=\"code_challenge\""));
    }

    #[test]
    fn base_url_prefers_configuration_then_forwarded_headers() {
        let mut headers = HeaderMap::new();
        headers.insert("host", "127.0.0.1:7777".parse().unwrap());
        assert_eq!(base_url(None, &headers), "http://127.0.0.1:7777");
        headers.insert("x-forwarded-proto", "https".parse().unwrap());
        headers.insert("x-forwarded-host", "q.example.com".parse().unwrap());
        assert_eq!(base_url(None, &headers), "https://q.example.com");
        assert_eq!(
            base_url(Some("https://q.example.com/"), &headers),
            "https://q.example.com"
        );
        let key = SigningKey::load_or_create(
            &std::env::temp_dir().join(format!("q-key-{}", uuid::Uuid::new_v4())),
        )
        .unwrap();
        assert_ne!(key.0, [0u8; 32]);
    }
}
