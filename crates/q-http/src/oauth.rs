//! A minimal OAuth 2.1 authorization server so chat apps can add `q serve`
//! as a custom connector. Grok, Claude, and ChatGPT all discover the
//! endpoints from `/.well-known/oauth-authorization-server`, register a
//! client dynamically, run the PKCE code flow, and then call `/mcp` with a
//! bearer token.
//!
//! Sign-in is deliberately simple: the authorize page asks for one of the
//! secrets in the token file. The connector then acts with that token's role.
//!
//! Client ids, access tokens, and refresh tokens are
//! HMAC-signed payloads. The signing key lives in a small key file next to
//! the queue database and is created on first start. Per-principal tokens
//! are signed with a key derived from that principal's secret, so revoking
//! or rotating a token in the token file invalidates every session it
//! signed in. Authorization codes are the only in-memory state and expire in
//! ten minutes. Refresh-token families are persisted in SQLite so rotation and
//! replay revocation survive restarts.

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};

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
    client_id: String,
    aud: String,
    grant_id: String,
}

#[derive(Debug, Clone)]
struct PendingCode {
    client_id: String,
    redirect_uri: String,
    code_challenge: String,
    principal: String,
    credential_key: [u8; 32],
    expires_at: u64,
}

pub struct OAuthServer {
    key: SigningKey,
    resource: String,
    grants: Arc<crate::GrantStore>,
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
    #[serde(default)]
    pub resource: Option<String>,
}

impl OAuthServer {
    pub fn new(key: SigningKey, resource: String, grants: Arc<crate::GrantStore>) -> Self {
        Self {
            key,
            resource,
            grants,
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
        self.check_resource(params.resource.as_deref())?;
        if params
            .scope
            .as_deref()
            .is_some_and(|scope| scope.split_whitespace().any(|s| s != "mcp"))
        {
            return Err(OAuthError::new(
                "invalid_scope",
                "only the mcp scope is supported",
            ));
        }
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
            credential_key: self.key.principal_key(&token.name, &token.secret),
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

    fn check_resource(&self, resource: Option<&str>) -> Result<(), OAuthError> {
        // A missing indicator defaults to our sole resource; an explicit foreign
        // resource must never silently acquire credentials for this queue.
        if let Some(resource) = resource {
            if url::Url::parse(resource)
                .ok()
                .as_ref()
                .map(url::Url::as_str)
                != Some(self.resource.as_str())
            {
                return Err(OAuthError::new(
                    "invalid_target",
                    "resource must identify this server's /mcp endpoint",
                ));
            }
        }
        Ok(())
    }

    /// Exchange a code or rotate a refresh token. Replays revoke the family.
    pub fn token(&self, tokens: &AuthConfig, form: &TokenForm) -> Result<Value, OAuthError> {
        self.check_resource(form.resource.as_deref())?;
        let client_id = form
            .client_id
            .as_deref()
            .ok_or_else(|| OAuthError::new("invalid_client", "client_id is required"))?;
        self.client(client_id)?;
        let (principal_name, prior) = match form.grant_type.as_str() {
            "authorization_code" => (self.redeem_code(tokens, form)?, None),
            "refresh_token" => {
                let refresh = form.refresh_token.as_deref().ok_or_else(|| {
                    OAuthError::new("invalid_request", "refresh_token is required")
                })?;
                let claims = self
                    .verified_claims(tokens, refresh, "refresh")
                    .ok_or_else(|| {
                        OAuthError::new("invalid_grant", "refresh token is not valid")
                    })?;
                if claims.client_id != client_id {
                    return Err(OAuthError::new("invalid_grant", "client_id mismatch"));
                }
                (claims.name.clone(), Some(claims))
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
        let grant_id = prior
            .as_ref()
            .map(|p| p.grant_id.clone())
            .unwrap_or_else(|| base64url_encode(&random_bytes()));
        let next_nonce = base64url_encode(&random_bytes());
        let refresh_expiry = unix_now() + REFRESH_TTL_SECS;
        let access = self.issue(
            &key,
            &TokenPayload {
                kind: "access".into(),
                name: token.name.clone(),
                exp: unix_now() + ACCESS_TTL_SECS,
                nonce: base64url_encode(&random_bytes()),
                client_id: client_id.into(),
                aud: self.resource.clone(),
                grant_id: grant_id.clone(),
            },
        )?;
        let refresh = self.issue(
            &key,
            &TokenPayload {
                kind: "refresh".into(),
                name: token.name.clone(),
                exp: refresh_expiry,
                nonce: next_nonce.clone(),
                client_id: client_id.into(),
                aud: self.resource.clone(),
                grant_id: grant_id.clone(),
            },
        )?;
        let storage_error =
            |err| OAuthError::new("server_error", format!("grant storage failed: {err}"));
        if let Some(prior) = prior {
            if !self
                .grants
                .rotate(&grant_id, &prior.nonce, &next_nonce, refresh_expiry)
                .map_err(storage_error)?
            {
                return Err(OAuthError::new(
                    "invalid_grant",
                    "refresh token was reused or revoked; sign in again",
                ));
            }
        } else {
            self.grants
                .create(&grant_id, &next_nonce, refresh_expiry)
                .map_err(storage_error)?;
        }
        Ok(json!({
            "access_token": access,
            "token_type": "Bearer",
            "expires_in": ACCESS_TTL_SECS,
            "refresh_token": refresh,
            "scope": "mcp",
        }))
    }

    fn redeem_code(&self, tokens: &AuthConfig, form: &TokenForm) -> Result<String, OAuthError> {
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
        if pending.expires_at <= unix_now() {
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
        let token = tokens.find_by_name(&pending.principal).ok_or_else(|| {
            OAuthError::new("invalid_grant", "the token used to sign in was revoked")
        })?;
        let current_key = self.key.principal_key(&token.name, &token.secret);
        if !constant_time_eq(&current_key, &pending.credential_key) {
            return Err(OAuthError::new(
                "invalid_grant",
                "the token used to sign in was rotated",
            ));
        }
        Ok(pending.principal)
    }

    fn issue(&self, key: &[u8], payload: &TokenPayload) -> Result<String, OAuthError> {
        let bytes = serde_json::to_vec(payload).map_err(|err| {
            OAuthError::new("server_error", format!("cannot encode token: {err}"))
        })?;
        Ok(sign(key, &bytes))
    }

    fn verified_claims(
        &self,
        tokens: &AuthConfig,
        bearer: &str,
        kind: &str,
    ) -> Option<TokenPayload> {
        let (payload, mac) = unsigned_payload(bearer)?;
        let claims: TokenPayload = serde_json::from_slice(&payload).ok()?;
        if claims.kind != kind || claims.exp <= unix_now() || claims.aud != self.resource {
            return None;
        }
        let token = tokens.find_by_name(&claims.name)?;
        let key = self.key.principal_key(&token.name, &token.secret);
        if !verify(&key, &payload, &mac) {
            return None;
        }
        Some(claims)
    }

    /// Resolve an issued bearer, including credential and grant revocation.
    pub fn principal_from_token(
        &self,
        tokens: &AuthConfig,
        bearer: &str,
        kind: &str,
    ) -> Option<Principal> {
        let claims = self.verified_claims(tokens, bearer, kind)?;
        if !self.grants.active(&claims.grant_id) {
            return None;
        }
        Some(Principal::from_token(tokens.find_by_name(&claims.name)?))
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
    let Ok(url) = url::Url::parse(uri) else {
        return false;
    };
    if !url.has_host()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
    {
        return false;
    }
    match url.scheme() {
        "https" => true,
        "http" => match url.host() {
            Some(url::Host::Domain(host)) => host == "localhost",
            Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
            Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
            None => false,
        },
        _ => false,
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::Role;

    fn server() -> OAuthServer {
        OAuthServer::new(
            SigningKey::ephemeral(),
            "https://q.example/mcp".into(),
            Arc::new(crate::GrantStore::in_memory().unwrap()),
        )
    }

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
        let server = server();
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
                resource: None,
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
                    resource: None,
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
                    resource: None,
                },
            )
            .is_err());

        let refreshed = server
            .token(
                &tokens,
                &TokenForm {
                    grant_type: "refresh_token".into(),
                    refresh_token: Some(refresh.into()),
                    client_id: Some(client_id.clone()),
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
        let other = self::server();
        assert!(other.authenticate(&tokens, access).is_none());
    }

    #[test]
    fn pending_codes_are_invalid_after_revocation_or_rotation() {
        for recreate in [false, true] {
            let server = server();
            let mut tokens = tokens();
            let client_id = registered(&server);
            let (verifier, challenge) = pkce();
            let redirect = server
                .authorize(
                    &tokens,
                    &AuthorizeForm {
                        params: AuthorizeParams {
                            response_type: "code".into(),
                            client_id: client_id.clone(),
                            redirect_uri: "https://chat.example/callback".into(),
                            code_challenge: challenge,
                            code_challenge_method: "S256".into(),
                            ..AuthorizeParams::default()
                        },
                        token: tokens.tokens[0].secret.clone(),
                    },
                )
                .unwrap();
            let mut old = tokens.tokens.remove(0);
            if recreate {
                old.secret = "replacement-secret-0123456789".into();
                tokens.tokens.push(old);
            }
            let form = TokenForm {
                grant_type: "authorization_code".into(),
                code: Some(redirect.split_once("code=").unwrap().1.into()),
                code_verifier: Some(verifier),
                client_id: Some(client_id),
                redirect_uri: Some("https://chat.example/callback".into()),
                ..TokenForm::default()
            };
            assert_eq!(
                server.token(&tokens, &form).unwrap_err().code,
                "invalid_grant"
            );
        }
    }

    #[test]
    fn redirect_uris_require_https_or_an_actual_loopback_host() {
        let server = server();
        for uri in [
            "http://localhost.attacker.example/cb",
            "http://127.0.0.1.attacker.example/cb",
            "http://localhost@attacker.example/cb",
            "http://127.0.0.1@attacker.example/cb",
            "http://192.168.1.1/cb",
            "https://example.com/cb#fragment",
            "https://user:password@example.com/cb",
            "not a url",
        ] {
            assert!(
                server
                    .register(RegisterRequest {
                        redirect_uris: vec![uri.into()],
                        client_name: None,
                    })
                    .is_err(),
                "accepted {uri}"
            );
        }
        for uri in [
            "https://chat.example/callback?existing=1",
            "http://localhost:8080/cb",
            "http://127.0.0.1:8080/cb",
            "http://[::1]:8080/cb",
        ] {
            assert!(
                server
                    .register(RegisterRequest {
                        redirect_uris: vec![uri.into()],
                        client_name: None,
                    })
                    .is_ok(),
                "rejected {uri}"
            );
        }
    }

    #[test]
    fn registration_and_authorize_checks() {
        let server = server();
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

    fn code_form(server: &OAuthServer, tokens: &AuthConfig, client_id: &str) -> TokenForm {
        let (verifier, challenge) = pkce();
        let redirect = server
            .authorize(
                tokens,
                &AuthorizeForm {
                    params: AuthorizeParams {
                        response_type: "code".into(),
                        client_id: client_id.into(),
                        redirect_uri: "https://chat.example/callback".into(),
                        code_challenge: challenge,
                        code_challenge_method: "S256".into(),
                        resource: Some(server.resource.clone()),
                        ..AuthorizeParams::default()
                    },
                    token: tokens.tokens[0].secret.clone(),
                },
            )
            .unwrap();
        TokenForm {
            grant_type: "authorization_code".into(),
            code: Some(redirect.split_once("code=").unwrap().1.into()),
            code_verifier: Some(verifier),
            client_id: Some(client_id.into()),
            redirect_uri: Some("https://chat.example/callback".into()),
            resource: Some(server.resource.clone()),
            ..TokenForm::default()
        }
    }

    fn refresh_form(server: &OAuthServer, issued: &Value, client_id: &str) -> TokenForm {
        TokenForm {
            grant_type: "refresh_token".into(),
            client_id: Some(client_id.into()),
            resource: Some(server.resource.clone()),
            refresh_token: Some(issued["refresh_token"].as_str().unwrap().into()),
            ..TokenForm::default()
        }
    }

    #[test]
    fn resources_are_checked_during_authorization_exchange_refresh_and_use() {
        let server = server();
        let tokens = tokens();
        let client_id = registered(&server);
        let mut params = AuthorizeParams {
            response_type: "code".into(),
            client_id: client_id.clone(),
            redirect_uri: "https://chat.example/callback".into(),
            code_challenge: pkce().1,
            code_challenge_method: "S256".into(),
            resource: Some("https://other.example/mcp".into()),
            ..AuthorizeParams::default()
        };
        assert_eq!(
            server
                .authorize(
                    &tokens,
                    &AuthorizeForm {
                        params: params.clone(),
                        token: tokens.tokens[0].secret.clone(),
                    }
                )
                .unwrap_err()
                .code,
            "invalid_target"
        );
        params.resource = Some(server.resource.clone());
        server.check_authorize(&params).unwrap();
        params.scope = Some("mcp admin".into());
        assert_eq!(
            server.check_authorize(&params).unwrap_err().code,
            "invalid_scope"
        );

        let mut form = code_form(&server, &tokens, &client_id);
        form.resource = Some("https://other.example/mcp".into());
        assert_eq!(
            server.token(&tokens, &form).unwrap_err().code,
            "invalid_target"
        );
        form.resource = Some(server.resource.clone());
        let issued = server.token(&tokens, &form).unwrap();
        let mut refresh = refresh_form(&server, &issued, &client_id);
        refresh.resource = Some("https://other.example/mcp".into());
        assert_eq!(
            server.token(&tokens, &refresh).unwrap_err().code,
            "invalid_target"
        );
        refresh.resource = Some(server.resource.clone());
        server.token(&tokens, &refresh).unwrap();
        // Even with the same signing key, principals, and grant database, a
        // different resource cannot accept this token.
        let other = OAuthServer::new(
            server.key.clone(),
            "https://other.example/mcp".into(),
            server.grants.clone(),
        );
        assert!(other
            .authenticate(&tokens, issued["access_token"].as_str().unwrap())
            .is_none());
    }

    #[test]
    fn refresh_binding_rotation_and_family_revocation_survive_restarts() {
        let path = std::env::temp_dir().join(format!("q-grants-{}.db", uuid::Uuid::new_v4()));
        let key = SigningKey::ephemeral();
        let reopen = || {
            OAuthServer::new(
                key.clone(),
                "https://q.example/mcp".into(),
                Arc::new(crate::GrantStore::open(&path).unwrap()),
            )
        };
        let server = reopen();
        let tokens = tokens();
        let client_id = registered(&server);
        let issued = server
            .token(&tokens, &code_form(&server, &tokens, &client_id))
            .unwrap();
        let independent = server
            .token(&tokens, &code_form(&server, &tokens, &client_id))
            .unwrap();
        let mut form = refresh_form(&server, &issued, &client_id);
        form.client_id = None;
        assert_eq!(
            server.token(&tokens, &form).unwrap_err().code,
            "invalid_client"
        );
        form.client_id = Some("unregistered-client".into());
        assert_eq!(
            server.token(&tokens, &form).unwrap_err().code,
            "invalid_client"
        );
        form.client_id = Some(
            server
                .register(RegisterRequest {
                    redirect_uris: vec!["https://other-client.example/cb".into()],
                    client_name: None,
                })
                .unwrap()["client_id"]
                .as_str()
                .unwrap()
                .into(),
        );
        assert_eq!(
            server.token(&tokens, &form).unwrap_err().code,
            "invalid_grant"
        );
        form.client_id = Some(client_id.clone());
        let rotated = server.token(&tokens, &form).unwrap();
        assert_ne!(issued["refresh_token"], rotated["refresh_token"]);
        drop(server);

        let server = reopen();
        assert!(server
            .authenticate(&tokens, rotated["access_token"].as_str().unwrap())
            .is_some());
        let newest = server
            .token(&tokens, &refresh_form(&server, &rotated, &client_id))
            .unwrap();
        // Replaying the original token invalidates the newest refresh AND access tokens.
        assert_eq!(
            server.token(&tokens, &form).unwrap_err().code,
            "invalid_grant"
        );
        drop(server);
        let server = reopen();
        assert!(server
            .authenticate(&tokens, newest["access_token"].as_str().unwrap())
            .is_none());
        assert_eq!(
            server
                .token(&tokens, &refresh_form(&server, &newest, &client_id))
                .unwrap_err()
                .code,
            "invalid_grant"
        );
        assert!(server
            .authenticate(&tokens, independent["access_token"].as_str().unwrap())
            .is_some());
        assert!(server
            .authenticate(&tokens, &tokens.tokens[0].secret)
            .is_some());
        drop(server);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn concurrent_refreshes_cannot_both_consume_the_same_token() {
        let path = std::env::temp_dir().join(format!("q-grants-{}.db", uuid::Uuid::new_v4()));
        let key = SigningKey::ephemeral();
        let server = OAuthServer::new(
            key.clone(),
            "https://q.example/mcp".into(),
            Arc::new(crate::GrantStore::open(&path).unwrap()),
        );
        let tokens = tokens();
        let client_id = registered(&server);
        let issued = server
            .token(&tokens, &code_form(&server, &tokens, &client_id))
            .unwrap();
        let barrier = Arc::new(std::sync::Barrier::new(2));
        let threads: Vec<_> = (0..2)
            .map(|_| {
                // Separate SQLite connections model separate processes/restarts too.
                let contender = OAuthServer::new(
                    key.clone(),
                    server.resource.clone(),
                    Arc::new(crate::GrantStore::open(&path).unwrap()),
                );
                let form = refresh_form(&server, &issued, &client_id);
                let tokens = tokens.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    contender.token(&tokens, &form)
                })
            })
            .collect();
        let outcomes: Vec<_> = threads.into_iter().map(|t| t.join().unwrap()).collect();
        assert_eq!(outcomes.iter().filter(|r| r.is_ok()).count(), 1);
        assert_eq!(
            outcomes
                .iter()
                .filter(|r| r.as_ref().err().is_some_and(|e| e.code == "invalid_grant"))
                .count(),
            1
        );
        let winner = outcomes.into_iter().find_map(Result::ok).unwrap();
        assert!(server
            .authenticate(&tokens, winner["access_token"].as_str().unwrap())
            .is_none());
        drop(server);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn signing_key_can_be_created() {
        let path = std::env::temp_dir().join(format!("q-key-{}", uuid::Uuid::new_v4()));
        let key = SigningKey::load_or_create(&path).unwrap();
        assert_ne!(key.0, [0u8; 32]);
        assert_eq!(key.0, SigningKey::load_or_create(&path).unwrap().0);
        std::fs::remove_file(path).unwrap();
    }
}
