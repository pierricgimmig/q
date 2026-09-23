//! stdio MCP adapter.
//!
//! This crate speaks newline-delimited JSON-RPC directly instead of linking the
//! current `rmcp` major. The queue core stays behind [`q_core::QueueService`],
//! and the only bytes written to stdout are protocol messages.

use std::path::PathBuf;
use std::sync::Arc;

use q_core::{
    Actor, ArtifactInput, BlockRequest, CaptureRequest, ClaimRequest, CompleteRequest,
    CreateFeatureRequest, DeleteRequest, HeartbeatRequest, ListFilter, QueueError, QueueService,
    ReleaseRequest, RiskLevel, StartRequest, TaskKind, TaskStatus, TreeQuery, NO_ELIGIBLE_REASON,
};
use q_project::{discover, DiscoverOptions};
use serde_json::{json, Map, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

const SERVER_NAME: &str = "q";
const SERVER_VERSION: &str = "0.1.0";
const KNOWN_VERSIONS: &[&str] = &["2024-11-05", "2025-03-26", "2025-06-18", "2025-11-25"];

pub struct Session {
    initialized: bool,
    base_dir: PathBuf,
}

impl Session {
    pub fn new(base_dir: PathBuf) -> Self {
        Self {
            initialized: false,
            base_dir,
        }
    }

    /// Handle one JSON-RPC line. `None` means the client sent a notification.
    pub fn handle_line(&mut self, queue: &dyn QueueService, line: &str) -> Option<String> {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            return None;
        }
        let message: Value = match serde_json::from_str(trimmed) {
            Ok(value) => value,
            Err(_) => return Some(rpc_error(Value::Null, -32700, "parse error", None)),
        };
        if !message.is_object() {
            return Some(rpc_error(Value::Null, -32600, "invalid request", None));
        }
        let id = message.get("id").cloned().unwrap_or(Value::Null);
        let has_id = message
            .get("id")
            .map(|value| !value.is_null())
            .unwrap_or(false);
        let method = message.get("method").and_then(Value::as_str).unwrap_or("");

        if method.is_empty() && has_id {
            return Some(rpc_error(id, -32600, "invalid request", None));
        }
        if !has_id && method != "initialize" {
            match method {
                "notifications/initialized" | "initialized" | "notifications/cancelled" => {
                    return None;
                }
                _ if !self.initialized => return None,
                _ => return None,
            }
        }

        match method {
            "initialize" => {
                self.initialized = true;
                let version = message
                    .pointer("/params/protocolVersion")
                    .and_then(Value::as_str)
                    .filter(|version| KNOWN_VERSIONS.contains(version))
                    .unwrap_or("2025-03-26");
                Some(rpc_result(
                    id,
                    json!({
                        "protocolVersion": version,
                        "capabilities": { "tools": { "listChanged": false } },
                        "serverInfo": { "name": SERVER_NAME, "version": SERVER_VERSION },
                        "instructions": "Local queue. Inbox tasks cannot be claimed until marked ready. High and external-action risk are excluded from default claims."
                    }),
                ))
            }
            "ping" | "logging/setLevel" => Some(rpc_result(id, json!({}))),
            "tools/list" => {
                if !self.initialized {
                    return Some(rpc_error(id, -32600, "server not initialized", None));
                }
                Some(rpc_result(id, json!({ "tools": tool_definitions() })))
            }
            "tools/call" => {
                if !self.initialized {
                    return Some(rpc_error(id, -32600, "server not initialized", None));
                }
                Some(self.call_tool(queue, &id, message.get("params")))
            }
            "notifications/initialized" | "initialized" | "notifications/cancelled" => None,
            _ => Some(rpc_error(id, -32601, "method not found", None)),
        }
    }

    fn call_tool(&self, queue: &dyn QueueService, id: &Value, params: Option<&Value>) -> String {
        let params = params.cloned().unwrap_or_else(|| json!({}));
        let name = match params.get("name").and_then(Value::as_str) {
            Some(name) => name.to_string(),
            None => {
                return rpc_error(
                    id.clone(),
                    -32602,
                    "invalid params",
                    Some(json!({"code": "invalid_input", "error": "tool name is required"})),
                );
            }
        };
        let arguments = params
            .get("arguments")
            .cloned()
            .unwrap_or_else(|| json!({}));
        if !arguments.is_object() {
            return rpc_error(
                id.clone(),
                -32602,
                "invalid params",
                Some(json!({"code": "invalid_input", "error": "arguments must be an object"})),
            );
        }
        match dispatch_tool(queue, &self.base_dir, &name, &arguments) {
            Ok(value) => rpc_result(id.clone(), tool_success(value)),
            Err(ToolFailure::Invalid(message)) => rpc_error(
                id.clone(),
                -32602,
                "invalid params",
                Some(json!({"code": "invalid_input", "error": message})),
            ),
            Err(ToolFailure::Domain(error)) => rpc_result(id.clone(), tool_error(&error)),
        }
    }
}

pub async fn serve(queue: Arc<dyn QueueService>, base_dir: PathBuf) -> std::io::Result<()> {
    tracing::info!("mcp server listening on stdio");
    let mut session = Session::new(base_dir);
    let stdin = BufReader::new(tokio::io::stdin());
    let mut lines = stdin.lines();
    let mut stdout = tokio::io::stdout();
    while let Some(line) = lines.next_line().await? {
        if let Some(response) = session.handle_line(queue.as_ref(), &line) {
            stdout.write_all(response.as_bytes()).await?;
            stdout.write_all(b"\n").await?;
            stdout.flush().await?;
        }
    }
    Ok(())
}

enum ToolFailure {
    Invalid(String),
    Domain(QueueError),
}

impl From<QueueError> for ToolFailure {
    fn from(error: QueueError) -> Self {
        match error {
            QueueError::InvalidInput(message) => Self::Invalid(message),
            other => Self::Domain(other),
        }
    }
}

fn dispatch_tool(
    queue: &dyn QueueService,
    base_dir: &std::path::Path,
    name: &str,
    arguments: &Value,
) -> Result<Value, ToolFailure> {
    let args = arguments.as_object().expect("object checked by caller");
    match name {
        "queue_capture" => queue_capture(queue, base_dir, args),
        "queue_list" => queue_list(queue, args),
        "queue_get" => queue_get(queue, args),
        "queue_tree" => queue_tree(queue, args),
        "queue_feature_create" => queue_feature_create(queue, args),
        "queue_feature_list" => queue_feature_list(queue, args),
        "queue_feature_get" => queue_feature_get(queue, args),
        "queue_claim_next" => queue_claim_next(queue, args),
        "queue_heartbeat" => queue_heartbeat(queue, args),
        "queue_start" => queue_start(queue, args),
        "queue_block" => queue_block(queue, args),
        "queue_complete" => queue_complete(queue, args),
        "queue_release" => queue_release(queue, args),
        "queue_delete" => queue_delete(queue, args),
        other => Err(ToolFailure::Invalid(format!("unknown tool {other}"))),
    }
}

fn queue_capture(
    queue: &dyn QueueService,
    base_dir: &std::path::Path,
    args: &Map<String, Value>,
) -> Result<Value, ToolFailure> {
    expect_keys(
        args,
        &[
            "title",
            "body",
            "repo",
            "project",
            "capture_path",
            "kind",
            "priority",
            "risk",
            "capabilities",
            "dependencies",
            "agent_pool",
            "feature",
            "agent_id",
        ],
    )?;
    let title = required_string(args, "title")?;
    let directory = match optional_string(args, "capture_path")? {
        Some(path) => PathBuf::from(path),
        None => base_dir.to_path_buf(),
    };
    let context = discover(DiscoverOptions {
        directory,
        explicit_repo: optional_string(args, "repo")?,
        explicit_project: optional_string(args, "project")?,
        global_map: None,
        use_default_map: true,
    })
    .map_err(ToolFailure::from)?;
    let kind = match optional_string(args, "kind")? {
        Some(kind) => TaskKind::parse(&kind).map_err(ToolFailure::from)?,
        None => context.default_kind.unwrap_or(TaskKind::Implementation),
    };
    let risk = match optional_string(args, "risk")? {
        Some(risk) => RiskLevel::parse(&risk).map_err(ToolFailure::from)?,
        None => RiskLevel::Low,
    };
    let task = queue.capture(CaptureRequest {
        title,
        body: optional_string(args, "body")?,
        kind,
        priority: optional_i64(args, "priority")?.unwrap_or(0) as i32,
        risk,
        project: context.project,
        repo: context.repo,
        capture_path: context.capture_path.display().to_string(),
        repo_relative_path: context.repo_relative_path,
        git_root: context.git_root.map(|path| path.display().to_string()),
        git_head: context.git_head,
        agent_pool: optional_string(args, "agent_pool")?.or(context.agent_pool),
        required_capabilities: optional_string_array(args, "capabilities")?,
        dependencies: optional_i64_array(args, "dependencies")?,
        feature: optional_feature(args)?,
        policy: context.policy,
        actor: match optional_string(args, "agent_id")? {
            Some(agent_id) if !agent_id.trim().is_empty() => Actor::agent(agent_id.trim()),
            _ => Actor::agent("mcp"),
        },
        context_source: serde_json::to_value(context.source)
            .ok()
            .and_then(|value| value.as_str().map(str::to_string)),
    })?;
    serde_json::to_value(task).map_err(|err| ToolFailure::Invalid(err.to_string()))
}

fn queue_list(queue: &dyn QueueService, args: &Map<String, Value>) -> Result<Value, ToolFailure> {
    expect_keys(
        args,
        &[
            "status",
            "project",
            "repo",
            "kind",
            "limit",
            "include_terminal",
            "all",
            "feature",
        ],
    )?;
    let status = match optional_string(args, "status")? {
        Some(status) => Some(TaskStatus::parse(&status).map_err(ToolFailure::from)?),
        None => None,
    };
    let kind = match optional_string(args, "kind")? {
        Some(kind) => Some(TaskKind::parse(&kind).map_err(ToolFailure::from)?),
        None => None,
    };
    let limit = optional_u64(args, "limit")?.unwrap_or(50) as u32;
    // Either flag includes done and cancelled when status is omitted.
    // An explicit status is honored on its own.
    let include_terminal = optional_bool(args, "include_terminal")? || optional_bool(args, "all")?;
    let tasks = queue.list(ListFilter {
        status,
        project: optional_string(args, "project")?,
        repo: optional_string(args, "repo")?,
        kind,
        feature: optional_feature(args)?,
        limit,
        include_terminal,
    })?;
    Ok(json!({ "tasks": tasks }))
}

fn queue_feature_create(
    queue: &dyn QueueService,
    args: &Map<String, Value>,
) -> Result<Value, ToolFailure> {
    expect_keys(args, &["title", "body"])?;
    let feature = queue.create_feature(CreateFeatureRequest {
        title: required_string(args, "title")?,
        body: optional_string(args, "body")?,
    })?;
    Ok(serde_json::to_value(feature).unwrap_or(Value::Null))
}

fn queue_feature_list(
    queue: &dyn QueueService,
    args: &Map<String, Value>,
) -> Result<Value, ToolFailure> {
    expect_keys(args, &[])?;
    Ok(json!({ "features": queue.list_features()? }))
}

fn queue_feature_get(
    queue: &dyn QueueService,
    args: &Map<String, Value>,
) -> Result<Value, ToolFailure> {
    expect_keys(args, &["id"])?;
    let id =
        optional_i64(args, "id")?.ok_or_else(|| ToolFailure::Invalid("id is required".into()))?;
    Ok(serde_json::to_value(queue.get_feature(id)?).unwrap_or(Value::Null))
}

fn queue_tree(queue: &dyn QueueService, args: &Map<String, Value>) -> Result<Value, ToolFailure> {
    expect_keys(args, &["task_id", "id", "feature"])?;
    let task_id = match (optional_i64(args, "task_id")?, optional_i64(args, "id")?) {
        (Some(left), Some(right)) if left != right => {
            return Err(ToolFailure::Invalid(
                "task_id and id must be the same task".into(),
            ));
        }
        (Some(id), _) | (_, Some(id)) => Some(id),
        (None, None) => None,
    };
    let tree = queue.tree(TreeQuery {
        task_id,
        feature: optional_feature(args)?,
    })?;
    Ok(serde_json::to_value(tree).unwrap_or(Value::Null))
}

fn queue_get(queue: &dyn QueueService, args: &Map<String, Value>) -> Result<Value, ToolFailure> {
    expect_keys(args, &["task_id", "id"])?;
    let task_id = required_task_id(args)?;
    Ok(serde_json::to_value(queue.get(task_id)?).unwrap_or(Value::Null))
}

fn queue_claim_next(
    queue: &dyn QueueService,
    args: &Map<String, Value>,
) -> Result<Value, ToolFailure> {
    expect_keys(
        args,
        &[
            "agent_id",
            "capabilities",
            "allowed_repos",
            "allowed_projects",
            "allowed_kinds",
            "maximum_risk",
            "lease_minutes",
            "agent_pool",
        ],
    )?;
    let agent_id = required_string(args, "agent_id")?;
    let mut request = ClaimRequest::new(agent_id);
    request.capabilities = optional_string_array(args, "capabilities")?;
    request.allowed_repos = optional_string_array(args, "allowed_repos")?;
    request.allowed_projects = optional_string_array(args, "allowed_projects")?;
    let mut kinds = Vec::new();
    for kind in optional_string_array(args, "allowed_kinds")? {
        kinds.push(TaskKind::parse(&kind).map_err(ToolFailure::from)?);
    }
    request.allowed_kinds = kinds;
    if let Some(risk) = optional_string(args, "maximum_risk")? {
        request.maximum_risk = RiskLevel::parse(&risk).map_err(ToolFailure::from)?;
    }
    if let Some(minutes) = optional_u64(args, "lease_minutes")? {
        request.lease = q_core::lease_from_minutes(minutes).map_err(ToolFailure::from)?;
    }
    request.agent_pool = optional_string(args, "agent_pool")?;
    let outcome = queue.claim_next(request)?;
    if !outcome.found {
        debug_assert_eq!(outcome.reason.as_deref(), Some(NO_ELIGIBLE_REASON));
    }
    Ok(serde_json::to_value(outcome).unwrap_or(Value::Null))
}

fn queue_heartbeat(
    queue: &dyn QueueService,
    args: &Map<String, Value>,
) -> Result<Value, ToolFailure> {
    expect_keys(args, &["task_id", "id", "claim_token", "lease_minutes"])?;
    let lease = match optional_u64(args, "lease_minutes")? {
        Some(minutes) => Some(q_core::lease_from_minutes(minutes).map_err(ToolFailure::from)?),
        None => None,
    };
    let claim = queue.heartbeat(HeartbeatRequest {
        task_id: required_task_id(args)?,
        claim_token: required_string(args, "claim_token")?,
        lease,
        actor: Actor::agent("mcp"),
    })?;
    Ok(serde_json::to_value(claim).unwrap_or(Value::Null))
}

fn queue_start(queue: &dyn QueueService, args: &Map<String, Value>) -> Result<Value, ToolFailure> {
    expect_keys(
        args,
        &[
            "task_id",
            "id",
            "claim_token",
            "branch",
            "worktree_path",
            "worktree",
        ],
    )?;
    let detail = queue.start(StartRequest {
        task_id: required_task_id(args)?,
        claim_token: required_string(args, "claim_token")?,
        branch: optional_string(args, "branch")?,
        worktree_path: optional_string(args, "worktree_path")?
            .or(optional_string(args, "worktree")?),
        actor: Actor::agent("mcp"),
    })?;
    Ok(serde_json::to_value(detail).unwrap_or(Value::Null))
}

fn queue_block(queue: &dyn QueueService, args: &Map<String, Value>) -> Result<Value, ToolFailure> {
    expect_keys(args, &["task_id", "id", "claim_token"])?;
    let task = queue.block(BlockRequest {
        task_id: required_task_id(args)?,
        claim_token: Some(required_string(args, "claim_token")?),
        actor: Actor::agent("mcp"),
    })?;
    Ok(serde_json::to_value(task).unwrap_or(Value::Null))
}

fn queue_complete(
    queue: &dyn QueueService,
    args: &Map<String, Value>,
) -> Result<Value, ToolFailure> {
    expect_keys(
        args,
        &[
            "task_id",
            "id",
            "claim_token",
            "summary",
            "status",
            "artifacts",
        ],
    )?;
    let target = match optional_string(args, "status")? {
        Some(status) => Some(TaskStatus::parse(&status).map_err(ToolFailure::from)?),
        None => None,
    };
    let mut artifacts = Vec::new();
    if let Some(value) = args.get("artifacts") {
        let items = value
            .as_array()
            .ok_or_else(|| ToolFailure::Invalid("artifacts must be an array".into()))?;
        for item in items {
            let object = item
                .as_object()
                .ok_or_else(|| ToolFailure::Invalid("each artifact must be an object".into()))?;
            expect_keys(object, &["kind", "value"])?;
            artifacts.push(ArtifactInput {
                kind: required_string(object, "kind")?,
                value: required_string(object, "value")?,
            });
        }
    }
    let detail = queue.complete(CompleteRequest {
        task_id: required_task_id(args)?,
        claim_token: Some(required_string(args, "claim_token")?),
        summary: required_string(args, "summary")?,
        target,
        artifacts,
        actor: Actor::agent("mcp"),
    })?;
    Ok(serde_json::to_value(detail).unwrap_or(Value::Null))
}

fn queue_delete(queue: &dyn QueueService, args: &Map<String, Value>) -> Result<Value, ToolFailure> {
    expect_keys(args, &["task_id", "force"])?;
    let outcome = queue.delete(DeleteRequest {
        task_id: required_task_id(args)?,
        force: optional_bool(args, "force")?,
        actor: Actor::agent("mcp"),
    })?;
    Ok(serde_json::to_value(outcome).unwrap_or(Value::Null))
}

fn queue_release(
    queue: &dyn QueueService,
    args: &Map<String, Value>,
) -> Result<Value, ToolFailure> {
    expect_keys(args, &["task_id", "id", "claim_token"])?;
    let task = queue.release(ReleaseRequest {
        task_id: required_task_id(args)?,
        claim_token: required_string(args, "claim_token")?,
        actor: Actor::agent("mcp"),
    })?;
    Ok(serde_json::to_value(task).unwrap_or(Value::Null))
}

fn expect_keys(args: &Map<String, Value>, allowed: &[&str]) -> Result<(), ToolFailure> {
    for key in args.keys() {
        if !allowed.contains(&key.as_str()) {
            return Err(ToolFailure::Invalid(format!("unexpected field {key}")));
        }
    }
    Ok(())
}

fn required_task_id(args: &Map<String, Value>) -> Result<i64, ToolFailure> {
    if let Some(id) = optional_i64(args, "task_id")? {
        return Ok(id);
    }
    if let Some(id) = optional_i64(args, "id")? {
        return Ok(id);
    }
    Err(ToolFailure::Invalid("task_id is required".into()))
}

fn required_string(args: &Map<String, Value>, key: &str) -> Result<String, ToolFailure> {
    match optional_string(args, key)? {
        Some(value) if !value.trim().is_empty() => Ok(value),
        _ => Err(ToolFailure::Invalid(format!("{key} is required"))),
    }
}

fn optional_bool(args: &Map<String, Value>, key: &str) -> Result<bool, ToolFailure> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(false),
        Some(Value::Bool(value)) => Ok(*value),
        Some(_) => Err(ToolFailure::Invalid(format!("{key} must be a boolean"))),
    }
}

fn optional_feature(args: &Map<String, Value>) -> Result<Option<String>, ToolFailure> {
    match args.get("feature") {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) => Ok(Some(value.clone())),
        Some(Value::Number(number)) => {
            let id = number
                .as_i64()
                .ok_or_else(|| ToolFailure::Invalid("feature must be an id or title".into()))?;
            Ok(Some(id.to_string()))
        }
        Some(_) => Err(ToolFailure::Invalid(
            "feature must be an id or title".into(),
        )),
    }
}

fn optional_string(args: &Map<String, Value>, key: &str) -> Result<Option<String>, ToolFailure> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) => Ok(Some(value.clone())),
        Some(_) => Err(ToolFailure::Invalid(format!("{key} must be a string"))),
    }
}

fn optional_i64(args: &Map<String, Value>, key: &str) -> Result<Option<i64>, ToolFailure> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Number(number)) => number
            .as_i64()
            .ok_or_else(|| ToolFailure::Invalid(format!("{key} must be an integer")))
            .map(Some),
        Some(_) => Err(ToolFailure::Invalid(format!("{key} must be an integer"))),
    }
}

fn optional_u64(args: &Map<String, Value>, key: &str) -> Result<Option<u64>, ToolFailure> {
    match optional_i64(args, key)? {
        None => Ok(None),
        Some(value) if value >= 0 => Ok(Some(value as u64)),
        Some(_) => Err(ToolFailure::Invalid(format!("{key} must be >= 0"))),
    }
}

fn optional_string_array(args: &Map<String, Value>, key: &str) -> Result<Vec<String>, ToolFailure> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::Array(items)) => {
            let mut out = Vec::new();
            for item in items {
                match item.as_str() {
                    Some(value) => out.push(value.to_string()),
                    None => {
                        return Err(ToolFailure::Invalid(format!(
                            "{key} must be an array of strings"
                        )))
                    }
                }
            }
            Ok(out)
        }
        Some(_) => Err(ToolFailure::Invalid(format!(
            "{key} must be an array of strings"
        ))),
    }
}

fn optional_i64_array(args: &Map<String, Value>, key: &str) -> Result<Vec<i64>, ToolFailure> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::Array(items)) => {
            let mut out = Vec::new();
            for item in items {
                match item.as_i64() {
                    Some(value) => out.push(value),
                    None => {
                        return Err(ToolFailure::Invalid(format!(
                            "{key} must be an array of integers"
                        )))
                    }
                }
            }
            Ok(out)
        }
        Some(_) => Err(ToolFailure::Invalid(format!(
            "{key} must be an array of integers"
        ))),
    }
}

fn rpc_result(id: Value, result: Value) -> String {
    serde_json::to_string(&json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": result,
    }))
    .expect("rpc result serializes")
}

fn rpc_error(id: Value, code: i64, message: &str, data: Option<Value>) -> String {
    let mut error = json!({"code": code, "message": message});
    if let Some(data) = data {
        error["data"] = data;
    }
    serde_json::to_string(&json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": error,
    }))
    .expect("rpc error serializes")
}

fn tool_success(value: Value) -> Value {
    json!({
        "content": [{ "type": "text", "text": serde_json::to_string(&value).unwrap_or_else(|_| "{}".into()) }],
        "isError": false
    })
}

fn tool_error(error: &QueueError) -> Value {
    let body = json!({"error": error.to_string(), "code": error.code()});
    json!({
        "content": [{ "type": "text", "text": serde_json::to_string(&body).unwrap_or_else(|_| "{}".into()) }],
        "isError": true
    })
}

fn tool_definitions() -> Vec<Value> {
    vec![
        tool(
            "queue_capture",
            "Capture an inbox task. Inbox work is never claimable until a human marks it ready.",
            json!({
                "type": "object",
                "required": ["title"],
                "properties": {
                    "title": {"type": "string"},
                    "body": {"type": "string"},
                    "repo": {"type": "string"},
                    "project": {"type": "string"},
                    "capture_path": {"type": "string"},
                    "kind": {"type": "string", "enum": ["implementation", "research", "review", "benchmark", "documentation", "other"]},
                    "priority": {"type": "integer"},
                    "risk": {"type": "string", "enum": ["low", "medium", "high", "external_action"]},
                    "capabilities": {"type": "array", "items": {"type": "string"}},
                    "dependencies": {"type": "array", "items": {"type": "integer"}},
                    "agent_pool": {"type": "string"},
                    "feature": {
                        "description": "Feature id or unique title. The task keeps its own repo and project.",
                        "anyOf": [{"type": "string"}, {"type": "integer"}]
                    },
                    "agent_id": {
                        "type": "string",
                        "description": "Agent that created the task. Required to claim it while offline from the Turso authority. Defaults to mcp."
                    }
                },
                "additionalProperties": false
            }),
        ),
        tool(
            "queue_list",
            "List bounded task summaries. Done and cancelled tasks are omitted unless status is set or include_terminal (alias all) is true. Ordered by feature (blank last), then project (blank last), then updated_at descending. Optional feature filters by id or unique title.",
            json!({
                "type": "object",
                "properties": {
                    "status": {"type": "string"},
                    "project": {"type": "string"},
                    "repo": {"type": "string"},
                    "kind": {"type": "string"},
                    "limit": {"type": "integer", "minimum": 1, "maximum": 500},
                    "include_terminal": {
                        "type": "boolean",
                        "description": "Include done and cancelled tasks when status is omitted. Defaults to false."
                    },
                    "all": {
                        "type": "boolean",
                        "description": "Alias of include_terminal. Either flag set to true includes terminal tasks."
                    },
                    "feature": {
                        "description": "Feature id or unique title.",
                        "anyOf": [{"type": "string"}, {"type": "integer"}]
                    }
                },
                "additionalProperties": false
            }),
        ),
        tool(
            "queue_feature_create",
            "Create a feature. A feature groups tasks that may span repos; each task keeps its own repo and project.",
            json!({
                "type": "object",
                "required": ["title"],
                "properties": {
                    "title": {"type": "string"},
                    "body": {"type": "string"}
                },
                "additionalProperties": false
            }),
        ),
        tool(
            "queue_feature_list",
            "List features ordered by title.",
            json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false
            }),
        ),
        tool(
            "queue_feature_get",
            "Fetch one feature by id, including how many tasks reference it.",
            json!({
                "type": "object",
                "required": ["id"],
                "properties": {
                    "id": {"type": "integer"}
                },
                "additionalProperties": false
            }),
        ),
        tool(
            "queue_tree",
            "Show a dependency tree. Children are tasks that must be done first. Pass task_id for one task, or feature (id or unique title) for every task in a feature. Dependencies outside that feature are marked external. A repeated node sets already_shown and omits children.",
            json!({
                "type": "object",
                "properties": {
                    "task_id": {"type": "integer"},
                    "id": {"type": "integer", "description": "Alias of task_id."},
                    "feature": {
                        "description": "Feature id or unique title. Omit task_id to show the whole feature.",
                        "anyOf": [{"type": "string"}, {"type": "integer"}]
                    }
                },
                "additionalProperties": false
            }),
        ),
        tool(
            "queue_get",
            "Fetch one task plus its latest claim, artifacts, and recent events.",
            json!({
                "type": "object",
                "properties": {
                    "task_id": {"type": "integer"},
                    "id": {"type": "integer"}
                },
                "additionalProperties": false
            }),
        ),
        tool(
            "queue_claim_next",
            "Atomically claim one eligible ready task, or return found=false when none are eligible.",
            json!({
                "type": "object",
                "required": ["agent_id"],
                "properties": {
                    "agent_id": {"type": "string"},
                    "capabilities": {"type": "array", "items": {"type": "string"}},
                    "allowed_repos": {"type": "array", "items": {"type": "string"}},
                    "allowed_projects": {"type": "array", "items": {"type": "string"}},
                    "allowed_kinds": {"type": "array", "items": {"type": "string"}},
                    "maximum_risk": {"type": "string", "enum": ["low", "medium", "high", "external_action"]},
                    "lease_minutes": {"type": "integer"},
                    "agent_pool": {"type": "string"}
                },
                "additionalProperties": false
            }),
        ),
        tool(
            "queue_heartbeat",
            "Extend a claim lease. Requires the task id and matching claim token.",
            json!({
                "type": "object",
                "required": ["task_id", "claim_token"],
                "properties": {
                    "task_id": {"type": "integer"},
                    "claim_token": {"type": "string"},
                    "lease_minutes": {"type": "integer"}
                },
                "additionalProperties": false
            }),
        ),
        tool(
            "queue_start",
            "Mark a claimed task in progress and optionally record a branch or worktree.",
            json!({
                "type": "object",
                "required": ["task_id", "claim_token"],
                "properties": {
                    "task_id": {"type": "integer"},
                    "claim_token": {"type": "string"},
                    "branch": {"type": "string"},
                    "worktree_path": {"type": "string"}
                },
                "additionalProperties": false
            }),
        ),
        tool(
            "queue_block",
            "Block claimed work. Requires the task id and matching claim token.",
            json!({
                "type": "object",
                "required": ["task_id", "claim_token"],
                "properties": {
                    "task_id": {"type": "integer"},
                    "claim_token": {"type": "string"}
                },
                "additionalProperties": false
            }),
        ),
        tool(
            "queue_complete",
            "Complete claimed work. Implementation tasks with require_pr land in review.",
            json!({
                "type": "object",
                "required": ["task_id", "claim_token", "summary"],
                "properties": {
                    "task_id": {"type": "integer"},
                    "claim_token": {"type": "string"},
                    "summary": {"type": "string"},
                    "status": {"type": "string", "enum": ["review", "done"]},
                    "artifacts": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "required": ["kind", "value"],
                            "properties": {
                                "kind": {"type": "string"},
                                "value": {"type": "string"}
                            }
                        }
                    }
                },
                "additionalProperties": false
            }),
        ),
        tool(
            "queue_delete",
            "Hard-delete a task and its claims, events, artifacts, and dependency rows. Unlike cancel, nothing remains in the database. An unexpired claim is rejected unless force is true.",
            json!({
                "type": "object",
                "required": ["task_id"],
                "properties": {
                    "task_id": {"type": "integer"},
                    "force": {"type": "boolean"}
                },
                "additionalProperties": false
            }),
        ),
        tool(
            "queue_release",
            "Release a claim back to ready. Requires the task id and matching claim token.",
            json!({
                "type": "object",
                "required": ["task_id", "claim_token"],
                "properties": {
                    "task_id": {"type": "integer"},
                    "claim_token": {"type": "string"}
                },
                "additionalProperties": false
            }),
        ),
    ]
}

fn tool(name: &str, description: &str, input_schema: Value) -> Value {
    json!({
        "name": name,
        "description": description,
        "inputSchema": input_schema
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use q_store::Queue;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_queue() -> Queue {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("q-mcp-{nanos}.db"));
        Queue::open(path).unwrap()
    }

    fn call(session: &mut Session, queue: &Queue, method: &str, id: i64, params: Value) -> Value {
        let line = serde_json::to_string(&json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        }))
        .unwrap();
        let response = session.handle_line(queue, &line).unwrap();
        assert!(!response.contains('\n'));
        serde_json::from_str(&response).unwrap()
    }

    #[test]
    fn invalid_input_and_empty_claim_are_structured() {
        let queue = temp_queue();
        let mut session = Session::new(std::env::temp_dir());
        let init = call(
            &mut session,
            &queue,
            "initialize",
            1,
            json!({"protocolVersion": "2025-03-26", "capabilities": {}, "clientInfo": {"name": "test", "version": "0"}}),
        );
        assert_eq!(init["result"]["serverInfo"]["name"], "q");
        assert!(session
            .handle_line(
                &queue,
                r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#
            )
            .is_none());

        let missing = call(
            &mut session,
            &queue,
            "tools/call",
            2,
            json!({"name": "queue_capture", "arguments": {}}),
        );
        assert_eq!(missing["error"]["code"], -32602);
        assert_eq!(missing["error"]["data"]["code"], "invalid_input");

        let claim = call(
            &mut session,
            &queue,
            "tools/call",
            3,
            json!({"name": "queue_claim_next", "arguments": {"agent_id": "codex-local-01"}}),
        );
        assert!(claim.get("error").is_none());
        assert_eq!(claim["result"]["isError"], false);
        let text = claim["result"]["content"][0]["text"].as_str().unwrap();
        assert!(!text.contains('\n'));
        let body: Value = serde_json::from_str(text).unwrap();
        assert_eq!(body["found"], false);
        assert_eq!(body["reason"], NO_ELIGIBLE_REASON);

        let tools = call(&mut session, &queue, "tools/list", 4, json!({}));
        let names: Vec<_> = tools["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|tool| tool["name"].as_str().unwrap().to_string())
            .collect();
        for name in [
            "queue_capture",
            "queue_list",
            "queue_get",
            "queue_tree",
            "queue_feature_create",
            "queue_feature_list",
            "queue_feature_get",
            "queue_claim_next",
            "queue_heartbeat",
            "queue_start",
            "queue_block",
            "queue_complete",
            "queue_release",
            "queue_delete",
        ] {
            assert!(names.iter().any(|candidate| candidate == name), "{name}");
        }
    }

    fn tool_body(response: &Value) -> Value {
        let text = response["result"]["content"][0]["text"].as_str().unwrap();
        serde_json::from_str(text).unwrap()
    }

    #[test]
    fn queue_delete_removes_the_task_and_honors_force() {
        let queue = temp_queue();
        let mut session = Session::new(std::env::temp_dir());
        call(
            &mut session,
            &queue,
            "initialize",
            1,
            json!({"protocolVersion": "2025-03-26", "capabilities": {}, "clientInfo": {"name": "test", "version": "0"}}),
        );
        assert!(session
            .handle_line(
                &queue,
                r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#
            )
            .is_none());

        let captured = call(
            &mut session,
            &queue,
            "tools/call",
            2,
            json!({"name": "queue_capture", "arguments": {"title": "drop me"}}),
        );
        let created = tool_body(&captured);
        let id = created["id"].as_i64().unwrap();

        let with_reason = call(
            &mut session,
            &queue,
            "tools/call",
            6,
            json!({"name": "queue_delete", "arguments": {"task_id": id, "reason": "duplicate"}}),
        );
        assert_eq!(with_reason["error"]["code"], -32602);
        assert!(with_reason["error"]["data"]["error"]
            .as_str()
            .unwrap()
            .contains("reason"));
        assert_eq!(queue.get(id).unwrap().task.status, TaskStatus::Inbox);

        let deleted = call(
            &mut session,
            &queue,
            "tools/call",
            3,
            json!({"name": "queue_delete", "arguments": {"task_id": id}}),
        );
        assert_eq!(deleted["result"]["isError"], false);
        let body = tool_body(&deleted);
        assert_eq!(body["task_id"], id);
        assert_eq!(body["status"], "inbox");
        assert!(body.get("reason").is_none());
        assert_eq!(body["active_claim_cleared"], false);
        assert!(matches!(queue.get(id), Err(QueueError::NotFound(_))));

        let again = queue
            .capture(q_core::CaptureRequest {
                title: "claimed".into(),
                body: None,
                kind: TaskKind::Research,
                priority: 0,
                risk: RiskLevel::Low,
                project: None,
                repo: None,
                capture_path: "/tmp".into(),
                repo_relative_path: None,
                git_root: None,
                git_head: None,
                agent_pool: None,
                required_capabilities: vec![],
                dependencies: vec![],
                feature: None,
                policy: None,
                actor: Actor::agent("mcp"),
                context_source: None,
            })
            .unwrap();
        queue
            .mark_ready(q_core::ReadyRequest {
                task_id: again.id,
                actor: Actor::agent("mcp"),
            })
            .unwrap();
        assert!(
            queue
                .claim_next(q_core::ClaimRequest::new("mcp-agent"))
                .unwrap()
                .found
        );

        let rejected = call(
            &mut session,
            &queue,
            "tools/call",
            4,
            json!({"name": "queue_delete", "arguments": {"task_id": again.id}}),
        );
        assert_eq!(rejected["result"]["isError"], true);
        let error = tool_body(&rejected);
        assert_eq!(error["code"], "conflict");
        assert!(error["error"].as_str().unwrap().contains("active claim"));
        assert_eq!(
            queue.get(again.id).unwrap().task.status,
            TaskStatus::Claimed
        );

        let forced = call(
            &mut session,
            &queue,
            "tools/call",
            5,
            json!({"name": "queue_delete", "arguments": {"task_id": again.id, "force": true}}),
        );
        assert_eq!(forced["result"]["isError"], false);
        let body = tool_body(&forced);
        assert_eq!(body["active_claim_cleared"], true);
        assert!(body["claims_removed"].as_i64().unwrap() >= 1);
        assert!(matches!(queue.get(again.id), Err(QueueError::NotFound(_))));
    }

    #[test]
    fn queue_list_omits_terminal_tasks_unless_asked() {
        let queue = temp_queue();
        let mut session = Session::new(std::env::temp_dir());
        call(
            &mut session,
            &queue,
            "initialize",
            1,
            json!({"protocolVersion": "2025-03-26", "capabilities": {}, "clientInfo": {"name": "test", "version": "0"}}),
        );

        let visible = call(
            &mut session,
            &queue,
            "tools/call",
            2,
            json!({"name": "queue_capture", "arguments": {"title": "visible work", "project": "alpha"}}),
        );
        let visible_id = tool_body(&visible)["id"].as_i64().unwrap();
        let hidden = call(
            &mut session,
            &queue,
            "tools/call",
            3,
            json!({"name": "queue_capture", "arguments": {"title": "hidden work", "project": "beta"}}),
        );
        let hidden_id = tool_body(&hidden)["id"].as_i64().unwrap();
        queue
            .cancel(q_core::CancelRequest {
                task_id: hidden_id,
                actor: Actor::agent("mcp"),
            })
            .unwrap();

        let default_list = call(
            &mut session,
            &queue,
            "tools/call",
            4,
            json!({"name": "queue_list", "arguments": {}}),
        );
        let default_ids = task_ids(&tool_body(&default_list));
        assert_eq!(default_ids, vec![visible_id]);

        let included = call(
            &mut session,
            &queue,
            "tools/call",
            5,
            json!({"name": "queue_list", "arguments": {"include_terminal": true}}),
        );
        let included_ids = task_ids(&tool_body(&included));
        assert_eq!(included_ids, vec![visible_id, hidden_id]);

        let alias = call(
            &mut session,
            &queue,
            "tools/call",
            6,
            json!({"name": "queue_list", "arguments": {"include_terminal": false, "all": true}}),
        );
        assert_eq!(task_ids(&tool_body(&alias)), vec![visible_id, hidden_id]);

        let cancelled = call(
            &mut session,
            &queue,
            "tools/call",
            7,
            json!({"name": "queue_list", "arguments": {"status": "cancelled"}}),
        );
        let cancelled_tasks = tool_body(&cancelled)["tasks"].as_array().unwrap().clone();
        assert_eq!(cancelled_tasks.len(), 1);
        assert_eq!(cancelled_tasks[0]["id"], hidden_id);
        assert_eq!(cancelled_tasks[0]["status"], "cancelled");

        let tools = call(&mut session, &queue, "tools/list", 8, json!({}));
        let list_tool = tools["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .find(|tool| tool["name"] == "queue_list")
            .unwrap();
        let properties = &list_tool["inputSchema"]["properties"];
        assert!(properties.get("include_terminal").is_some());
        assert!(properties.get("all").is_some());
        assert!(list_tool["description"]
            .as_str()
            .unwrap()
            .contains("include_terminal"));
    }

    #[test]
    fn features_are_created_and_filter_capture_across_repos() {
        let queue = temp_queue();
        let mut session = Session::new(std::env::temp_dir());
        call(
            &mut session,
            &queue,
            "initialize",
            1,
            json!({"protocolVersion": "2025-03-26", "capabilities": {}, "clientInfo": {"name": "test", "version": "0"}}),
        );

        let created = call(
            &mut session,
            &queue,
            "tools/call",
            2,
            json!({"name": "queue_feature_create", "arguments": {"title": "Cross-repo rollout", "body": "span services"}}),
        );
        assert_eq!(created["result"]["isError"], false);
        let feature = tool_body(&created);
        let feature_id = feature["id"].as_i64().unwrap();
        assert_eq!(feature["title"], "Cross-repo rollout");

        let first = call(
            &mut session,
            &queue,
            "tools/call",
            3,
            json!({"name": "queue_capture", "arguments": {
                "title": "Migration",
                "repo": "github.com/acme/queue",
                "project": "queue",
                "feature": "cross-repo rollout"
            }}),
        );
        let second = call(
            &mut session,
            &queue,
            "tools/call",
            4,
            json!({"name": "queue_capture", "arguments": {
                "title": "Client",
                "repo": "github.com/acme/client",
                "project": "client",
                "feature": feature_id
            }}),
        );
        let other = call(
            &mut session,
            &queue,
            "tools/call",
            5,
            json!({"name": "queue_capture", "arguments": {"title": "Loose note", "project": "notes"}}),
        );
        let first_id = tool_body(&first)["id"].as_i64().unwrap();
        let second_id = tool_body(&second)["id"].as_i64().unwrap();
        let other_id = tool_body(&other)["id"].as_i64().unwrap();
        assert_eq!(tool_body(&first)["feature"], "Cross-repo rollout");
        assert_eq!(tool_body(&second)["repo"], "github.com/acme/client");

        let filtered = call(
            &mut session,
            &queue,
            "tools/call",
            6,
            json!({"name": "queue_list", "arguments": {"feature": "Cross-repo rollout"}}),
        );
        let ids = task_ids(&tool_body(&filtered));
        assert!(
            ids.contains(&first_id) && ids.contains(&second_id),
            "{ids:?}"
        );
        assert!(!ids.contains(&other_id));

        let listed = call(
            &mut session,
            &queue,
            "tools/call",
            7,
            json!({"name": "queue_feature_list", "arguments": {}}),
        );
        let listed_body = tool_body(&listed);
        let features = listed_body["features"].as_array().unwrap();
        assert_eq!(features.len(), 1);
        assert_eq!(features[0]["task_count"], 2);

        let fetched = call(
            &mut session,
            &queue,
            "tools/call",
            8,
            json!({"name": "queue_feature_get", "arguments": {"id": feature_id}}),
        );
        assert_eq!(tool_body(&fetched)["title"], "Cross-repo rollout");

        let missing = call(
            &mut session,
            &queue,
            "tools/call",
            9,
            json!({"name": "queue_list", "arguments": {"feature": "no such feature"}}),
        );
        assert_eq!(missing["result"]["isError"], true);
        assert_eq!(tool_body(&missing)["code"], "not_found");
    }

    #[test]
    fn queue_tree_returns_dependencies_for_a_task_or_feature() {
        let queue = temp_queue();
        let mut session = Session::new(std::env::temp_dir());
        call(
            &mut session,
            &queue,
            "initialize",
            1,
            json!({"protocolVersion": "2025-03-26", "capabilities": {}, "clientInfo": {"name": "test", "version": "0"}}),
        );

        let feature = tool_body(&call(
            &mut session,
            &queue,
            "tools/call",
            2,
            json!({"name": "queue_feature_create", "arguments": {"title": "Rollout"}}),
        ));
        let feature_id = feature["id"].as_i64().unwrap();
        let leaf = tool_body(&call(
            &mut session,
            &queue,
            "tools/call",
            3,
            json!({"name": "queue_capture", "arguments": {"title": "Add the types", "feature": feature_id}}),
        ));
        let leaf_id = leaf["id"].as_i64().unwrap();
        let top = tool_body(&call(
            &mut session,
            &queue,
            "tools/call",
            4,
            json!({"name": "queue_capture", "arguments": {
                "title": "Ship the rollout",
                "feature": "Rollout",
                "dependencies": [leaf_id]
            }}),
        ));
        let top_id = top["id"].as_i64().unwrap();

        let missing = call(
            &mut session,
            &queue,
            "tools/call",
            5,
            json!({"name": "queue_tree", "arguments": {}}),
        );
        assert_eq!(missing["error"]["code"], -32602);

        let tree = tool_body(&call(
            &mut session,
            &queue,
            "tools/call",
            6,
            json!({"name": "queue_tree", "arguments": {"task_id": top_id}}),
        ));
        assert!(tree.get("feature").is_none());
        assert_eq!(tree["roots"][0]["id"], top_id);
        assert_eq!(tree["roots"][0]["depends_on"][0]["id"], leaf_id);

        let forest = tool_body(&call(
            &mut session,
            &queue,
            "tools/call",
            7,
            json!({"name": "queue_tree", "arguments": {"feature": "Rollout"}}),
        ));
        assert_eq!(forest["feature"]["id"], feature_id);
        assert_eq!(forest["roots"][0]["id"], top_id);
        assert_eq!(forest["roots"][0]["depends_on"][0]["id"], leaf_id);
        assert!(forest["roots"][0]["depends_on"][0]
            .get("external")
            .is_none());
    }

    fn task_ids(body: &Value) -> Vec<i64> {
        body["tasks"]
            .as_array()
            .unwrap()
            .iter()
            .map(|task| task["id"].as_i64().unwrap())
            .collect()
    }
}
