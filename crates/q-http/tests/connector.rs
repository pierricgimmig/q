//! Walk the connector flow a chat app runs: discover metadata, register a
//! client, sign in on the authorize page with a q token, exchange the code
//! with PKCE, and call `/mcp` with the access token.

use std::path::PathBuf;
use std::sync::Arc;

use q_core::QueueService;
use q_http::{serve_on, AuthConfig, ServerOptions, SigningKey, TokenStore};
use q_store::Queue;
use serde_json::{json, Value};
use tokio::sync::oneshot;

const HUMAN_SECRET: &str = "human-secret-0123456789";
const AGENT_SECRET: &str = "agent-secret-0123456789";

fn temp_db() -> PathBuf {
    std::env::temp_dir().join(format!("q-connector-{}.db", uuid::Uuid::new_v4()))
}

struct Server {
    url: String,
    stop: Option<oneshot::Sender<()>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Server {
    fn start(auth: Option<AuthConfig>) -> Self {
        Self::start_store(auth.map(TokenStore::fixed))
    }

    fn start_store(auth: Option<TokenStore>) -> Self {
        Self::start_public(auth, None)
    }

    fn start_public(auth: Option<TokenStore>, public_url: Option<String>) -> Self {
        Self::start_bound(auth, public_url, "127.0.0.1:0")
    }

    fn start_bound(auth: Option<TokenStore>, public_url: Option<String>, bind: &str) -> Self {
        let bind = bind.to_string();
        let queue: Arc<dyn QueueService> = Arc::new(Queue::open(temp_db()).unwrap());
        let (stop, stopped) = oneshot::channel::<()>();
        let (ready, started) = std::sync::mpsc::channel::<String>();
        let thread = std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            runtime.block_on(async move {
                let listener = tokio::net::TcpListener::bind(bind).await.unwrap();
                let mut addr = listener.local_addr().unwrap();
                if addr.ip().is_unspecified() {
                    addr.set_ip(std::net::Ipv4Addr::LOCALHOST.into());
                }
                ready.send(format!("http://{addr}")).unwrap();
                let options = ServerOptions {
                    auth,
                    public_url,
                    signing_key: SigningKey::ephemeral(),
                    base_dir: std::env::temp_dir(),
                    grants: Arc::new(q_http::GrantStore::in_memory().unwrap()),
                };
                serve_on(queue, listener, options, async move {
                    let _ = stopped.await;
                })
                .await
                .unwrap();
            });
        });
        let url = started.recv().unwrap();
        Self {
            url,
            stop: Some(stop),
            thread: Some(thread),
        }
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn auth() -> AuthConfig {
    AuthConfig::parse(&format!(
        "[[tokens]]\nname=\"pierric\"\nrole=\"human\"\nsecret=\"{HUMAN_SECRET}\"\n\
         [[tokens]]\nname=\"vps-agent\"\nrole=\"agent\"\nsecret=\"{AGENT_SECRET}\"\n"
    ))
    .unwrap()
}

fn agent() -> ureq::Agent {
    ureq::AgentBuilder::new().redirects(0).build()
}

fn mcp(http: &ureq::Agent, url: &str, bearer: Option<&str>, message: Value) -> (u16, Value) {
    let mut request = http
        .post(&format!("{url}/mcp"))
        .set("Accept", "application/json, text/event-stream");
    if let Some(bearer) = bearer {
        request = request.set("Authorization", &format!("Bearer {bearer}"));
    }
    match request.send_json(message) {
        Ok(response) => {
            let status = response.status();
            if status == 202 {
                return (status, Value::Null);
            }
            (status, response.into_json().unwrap())
        }
        Err(ureq::Error::Status(status, response)) => {
            (status, response.into_json().unwrap_or(Value::Null))
        }
        Err(other) => panic!("{other}"),
    }
}

fn tool_names(body: &Value) -> Vec<String> {
    body["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| tool["name"].as_str().unwrap().to_string())
        .collect()
}

fn tool_text(body: &Value) -> Value {
    let text = body["result"]["content"][0]["text"].as_str().unwrap();
    serde_json::from_str(text).unwrap()
}

#[test]
fn chat_connector_signs_in_with_oauth_and_triages_over_mcp() {
    let server = Server::start(Some(auth()));
    let http = agent();
    let url = server.url.clone();

    // 1. Unauthenticated /mcp points the client at the resource metadata.
    let denied = http
        .post(&format!("{url}/mcp"))
        .send_json(json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"}))
        .unwrap_err();
    let ureq::Error::Status(401, response) = denied else {
        panic!("expected 401")
    };
    let challenge = response.header("www-authenticate").unwrap().to_string();
    assert!(
        challenge.contains("/.well-known/oauth-protected-resource"),
        "{challenge}"
    );

    // 2. Discovery.
    let resource: Value = http
        .get(&format!("{url}/.well-known/oauth-protected-resource"))
        .call()
        .unwrap()
        .into_json()
        .unwrap();
    assert_eq!(resource["resource"], format!("{url}/mcp"));
    let issuer = resource["authorization_servers"][0]
        .as_str()
        .unwrap()
        .to_string();
    let metadata: Value = http
        .get(&format!("{issuer}/.well-known/oauth-authorization-server"))
        .call()
        .unwrap()
        .into_json()
        .unwrap();
    assert_eq!(metadata["code_challenge_methods_supported"][0], "S256");
    let authorize = metadata["authorization_endpoint"].as_str().unwrap();
    let token_endpoint = metadata["token_endpoint"].as_str().unwrap();
    let register = metadata["registration_endpoint"].as_str().unwrap();

    // 3. Dynamic client registration.
    let registration = http
        .post(register)
        .send_json(json!({
            "client_name": "Grok",
            "redirect_uris": ["https://grok.example/oauth/callback"],
            "token_endpoint_auth_method": "none",
            "grant_types": ["authorization_code", "refresh_token"],
            "response_types": ["code"]
        }))
        .unwrap();
    assert_eq!(registration.status(), 201);
    let registration: Value = registration.into_json().unwrap();
    let client_id = registration["client_id"].as_str().unwrap().to_string();

    // 4. The user is sent to the authorize page and pastes a q token.
    let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
    let challenge = "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM";
    let page = http
        .get(authorize)
        .query("response_type", "code")
        .query("client_id", &client_id)
        .query("redirect_uri", "https://grok.example/oauth/callback")
        .query("code_challenge", challenge)
        .query("code_challenge_method", "S256")
        .query("state", "st4te")
        .query("scope", "mcp")
        .query("resource", &format!("{url}/mcp"))
        .call()
        .unwrap()
        .into_string()
        .unwrap();
    assert!(page.contains("Sign in to q"));
    assert!(page.contains("Grok"));

    let wrong = http
        .post(authorize)
        .send_form(&[
            ("response_type", "code"),
            ("client_id", &client_id),
            ("redirect_uri", "https://grok.example/oauth/callback"),
            ("code_challenge", challenge),
            ("code_challenge_method", "S256"),
            ("state", "st4te"),
            ("token", "not-a-token"),
        ])
        .unwrap();
    assert_eq!(wrong.status(), 200);
    assert!(wrong.into_string().unwrap().contains("not recognized"));

    let redirect = http
        .post(authorize)
        .send_form(&[
            ("response_type", "code"),
            ("client_id", &client_id),
            ("redirect_uri", "https://grok.example/oauth/callback"),
            ("code_challenge", challenge),
            ("code_challenge_method", "S256"),
            ("state", "st4te"),
            ("token", HUMAN_SECRET),
            ("resource", &format!("{url}/mcp")),
        ])
        .unwrap();
    assert_eq!(redirect.status(), 303);
    let location = redirect.header("location").unwrap().to_string();
    assert!(location.starts_with("https://grok.example/oauth/callback?code="));
    assert!(location.ends_with("&state=st4te"), "{location}");
    let code = location
        .split("code=")
        .nth(1)
        .unwrap()
        .split('&')
        .next()
        .unwrap()
        .to_string();

    // 5. Code exchange with PKCE.
    let issued: Value = http
        .post(token_endpoint)
        .send_form(&[
            ("grant_type", "authorization_code"),
            ("code", &code),
            ("code_verifier", verifier),
            ("resource", &format!("{url}/mcp")),
            ("redirect_uri", "https://grok.example/oauth/callback"),
            ("client_id", &client_id),
        ])
        .unwrap()
        .into_json()
        .unwrap();
    let access = issued["access_token"].as_str().unwrap().to_string();
    let refresh = issued["refresh_token"].as_str().unwrap().to_string();

    // 6. MCP as the human: triage tools are listed and work, and the events
    //    name the human.
    let (status, listed) = mcp(
        &http,
        &url,
        Some(&access),
        json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"}),
    );
    assert_eq!(status, 200);
    let names = tool_names(&listed);
    assert!(names.iter().any(|name| name == "queue_ready"), "{names:?}");
    assert!(names.iter().any(|name| name == "queue_capture"));

    let (_, initialized) = mcp(
        &http,
        &url,
        Some(&access),
        json!({"jsonrpc": "2.0", "id": 2, "method": "initialize", "params": {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "grok", "version": "1"}}}),
    );
    assert_eq!(initialized["result"]["serverInfo"]["name"], "q");
    let (status, _) = mcp(
        &http,
        &url,
        Some(&access),
        json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
    );
    assert_eq!(status, 202);

    let (_, captured) = mcp(
        &http,
        &url,
        Some(&access),
        json!({"jsonrpc": "2.0", "id": 3, "method": "tools/call", "params": {"name": "queue_capture", "arguments": {"title": "From chat", "capture_path": "/tmp"}}}),
    );
    let task_id = tool_text(&captured)["id"].as_i64().unwrap();
    let (_, readied) = mcp(
        &http,
        &url,
        Some(&access),
        json!({"jsonrpc": "2.0", "id": 4, "method": "tools/call", "params": {"name": "queue_ready", "arguments": {"task_id": task_id}}}),
    );
    assert_eq!(tool_text(&readied)["task"]["status"], "ready");
    let (_, detail) = mcp(
        &http,
        &url,
        Some(&access),
        json!({"jsonrpc": "2.0", "id": 5, "method": "tools/call", "params": {"name": "queue_get", "arguments": {"task_id": task_id}}}),
    );
    let events = tool_text(&detail)["events"].as_array().unwrap().clone();
    let ready_event = events
        .iter()
        .find(|event| event["event_type"] == "task_ready")
        .unwrap();
    assert_eq!(ready_event["actor_type"], "human");
    assert_eq!(ready_event["actor_id"], "pierric");

    // 7. Refresh works, and a raw agent secret as a bearer gets no ready tool.
    let refreshed: Value = http
        .post(token_endpoint)
        .send_form(&[
            ("grant_type", "refresh_token"),
            ("refresh_token", &refresh),
            ("resource", &format!("{url}/mcp")),
            ("client_id", &client_id),
        ])
        .unwrap()
        .into_json()
        .unwrap();
    assert!(refreshed["access_token"].is_string());

    let (status, listed) = mcp(
        &http,
        &url,
        Some(AGENT_SECRET),
        json!({"jsonrpc": "2.0", "id": 6, "method": "tools/list"}),
    );
    assert_eq!(status, 200);
    assert!(!tool_names(&listed).iter().any(|name| name == "queue_ready"));
    let (_, denied) = mcp(
        &http,
        &url,
        Some(AGENT_SECRET),
        json!({"jsonrpc": "2.0", "id": 7, "method": "tools/call", "params": {"name": "queue_reopen", "arguments": {"task_id": task_id}}}),
    );
    assert_eq!(denied["error"]["code"], -32602);

    // 8. GET is 405 and DELETE is 204, as the transport allows.
    let get = http.get(&format!("{url}/mcp")).call().unwrap_err();
    assert!(matches!(get, ureq::Error::Status(405, _)));
    assert_eq!(
        http.delete(&format!("{url}/mcp")).call().unwrap().status(),
        204
    );
}

#[test]
fn loopback_without_tokens_serves_mcp_to_an_anonymous_human() {
    let server = Server::start(None);
    let http = agent();
    let (status, listed) = mcp(
        &http,
        &server.url,
        None,
        json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"}),
    );
    assert_eq!(status, 200);
    assert!(tool_names(&listed).iter().any(|name| name == "queue_ready"));
    // A batch of notifications is accepted with no body.
    let (status, _) = mcp(
        &http,
        &server.url,
        None,
        json!([{"jsonrpc": "2.0", "method": "notifications/initialized"}]),
    );
    assert_eq!(status, 202);
}

#[test]
fn empty_token_file_denies_access_and_reloads_creation_and_last_revocation() {
    let path = std::env::temp_dir().join(format!("q-empty-auth-{}.toml", uuid::Uuid::new_v4()));
    AuthConfig::default().save(&path).unwrap();
    let server = Server::start_store(Some(TokenStore::from_file(&path).unwrap()));
    let http = agent();
    let request = json!({"jsonrpc":"2.0", "id":1, "method":"tools/list"});
    assert_eq!(mcp(&http, &server.url, None, request.clone()).0, 401);
    let secret = AuthConfig::create_token(&path, "me", q_http::Role::Human).unwrap();
    // Force an mtime change even on coarse timestamp filesystems.
    let modified = std::fs::metadata(&path).unwrap().modified().unwrap();
    std::fs::OpenOptions::new()
        .write(true)
        .open(&path)
        .unwrap()
        .set_modified(modified + std::time::Duration::from_secs(2))
        .unwrap();
    assert_eq!(
        mcp(&http, &server.url, Some(&secret), request.clone()).0,
        200
    );
    assert_eq!(mcp(&http, &server.url, None, request.clone()).0, 401);
    AuthConfig::revoke_token(&path, "me").unwrap();
    assert_eq!(
        mcp(&http, &server.url, Some(&secret), request.clone()).0,
        401
    );
    assert_eq!(mcp(&http, &server.url, None, request.clone()).0, 401);
    std::fs::remove_file(path).unwrap();
    assert_eq!(mcp(&http, &server.url, Some(&secret), request).0, 401);
}

#[test]
fn foreign_browser_origins_cannot_mutate_a_local_queue() {
    let server = Server::start(None);
    let http = agent();
    let capture = json!({"jsonrpc":"2.0", "id":1, "method":"tools/call",
        "params":{"name":"queue_capture", "arguments":{"title":"cross-origin write"}}});
    for origin in [
        "https://untrusted.example",
        "null",
        "http://localhost.attacker.example",
        "http://127.0.0.1:1",
    ] {
        // A text/plain POST can be sent by a browser without a CORS preflight.
        let denied = http
            .post(&format!("{}/mcp", server.url))
            .set("Origin", origin)
            .set("Host", "untrusted.example")
            .set("X-Forwarded-Host", "untrusted.example")
            .set("X-Forwarded-Proto", "https")
            .set("Content-Type", "text/plain")
            .send_string(&capture.to_string())
            .unwrap_err();
        assert!(
            matches!(denied, ureq::Error::Status(403, _)),
            "{origin}: {denied}"
        );
    }
    for method in ["GET", "DELETE", "OPTIONS"] {
        let denied = http
            .request(method, &format!("{}/mcp", server.url))
            .set("Origin", "https://untrusted.example")
            .call()
            .unwrap_err();
        assert!(
            matches!(denied, ureq::Error::Status(403, _)),
            "{method}: {denied}"
        );
    }
    // The existing RPC endpoint must not offer a way around the browser guard.
    let denied = http
        .post(&format!("{}/v1/status", server.url))
        .set("Origin", "https://untrusted.example")
        .send_json(json!({}))
        .unwrap_err();
    assert!(matches!(denied, ureq::Error::Status(403, _)));
    let (_, status) = mcp(
        &http,
        &server.url,
        None,
        json!({"jsonrpc":"2.0", "id":2, "method":"tools/call", "params":{"name":"queue_status"}}),
    );
    assert_eq!(tool_text(&status)["counts"]["inbox"], 0);
    // A same-origin browser request still works.
    assert_eq!(
        http.post(&format!("{}/mcp", server.url))
            .set("Origin", &server.url)
            .send_json(capture)
            .unwrap()
            .status(),
        200
    );
}

#[test]
fn agents_need_no_origin_allowlist_and_headers_cannot_change_server_identity() {
    let server = Server::start_public(
        Some(TokenStore::fixed(auth())),
        Some("https://queue.example".into()),
    );
    let http = agent();
    let message = json!({"jsonrpc":"2.0", "id":1, "method":"tools/list"});
    // Normal agents send credentials, no browser Origin, regardless of their host/IP.
    assert_eq!(
        mcp(&http, &server.url, Some(AGENT_SECRET), message.clone()).0,
        200
    );
    assert_eq!(mcp(&http, &server.url, None, message.clone()).0, 401);
    assert_eq!(
        http.post(&format!("{}/mcp", server.url))
            .set("Authorization", &format!("Bearer {AGENT_SECRET}"))
            .set("Origin", "https://queue.example:443")
            .send_json(message.clone())
            .unwrap()
            .status(),
        200
    );
    for origin in [
        "http://queue.example",
        "https://queue.example:8443",
        "https://queue.example.attacker.example",
        "https://queue.example/path",
    ] {
        let denied = http
            .post(&format!("{}/mcp", server.url))
            .set("Authorization", &format!("Bearer {AGENT_SECRET}"))
            .set("Origin", origin)
            .send_json(message.clone())
            .unwrap_err();
        assert!(
            matches!(denied, ureq::Error::Status(403, _)),
            "{origin}: {denied}"
        );
    }
    let metadata: Value = http
        .get(&format!(
            "{}/.well-known/oauth-protected-resource",
            server.url
        ))
        .set("Host", "attacker.example")
        .set("X-Forwarded-Host", "attacker.example")
        .set("X-Forwarded-Proto", "http")
        .call()
        .unwrap()
        .into_json()
        .unwrap();
    assert_eq!(metadata["resource"], "https://queue.example/mcp");
    let denied = http
        .post(&format!("{}/mcp", server.url))
        .set("Host", "attacker.example")
        .set("X-Forwarded-Host", "attacker.example")
        .send_json(message)
        .unwrap_err();
    let ureq::Error::Status(401, response) = denied else {
        panic!("expected 401")
    };
    assert!(response
        .header("www-authenticate")
        .unwrap()
        .contains("https://queue.example/.well-known/"));
}

#[test]
fn all_interface_listener_needs_no_public_hostname_for_token_agents() {
    let server = Server::start_bound(Some(TokenStore::fixed(auth())), None, "0.0.0.0:0");
    let http = agent();
    assert_eq!(
        mcp(
            &http,
            &server.url,
            Some(AGENT_SECRET),
            json!({"jsonrpc":"2.0", "id":1, "method":"tools/list"})
        )
        .0,
        200
    );
    let metadata: Value = http
        .get(&format!(
            "{}/.well-known/oauth-protected-resource",
            server.url
        ))
        .set("Host", "attacker.example")
        .set("X-Forwarded-Host", "attacker.example")
        .set("X-Forwarded-Proto", "https")
        .call()
        .unwrap()
        .into_json()
        .unwrap();
    assert_eq!(metadata["resource"], format!("{}/mcp", server.url));
}
