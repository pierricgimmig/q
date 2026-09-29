//! stdio MCP adapter.
//!
//! This crate speaks newline-delimited JSON-RPC directly instead of linking the
//! current `rmcp` major. The queue core stays behind [`q_core::QueueService`],
//! and the only bytes written to stdout are protocol messages.

use std::path::PathBuf;
use std::sync::Arc;

use q_core::{
    Actor, ArtifactInput, BlockRequest, CaptureRequest, ClaimRequest, CompleteRequest,
    CreateFeatureRequest, DeleteRequest, FailRequest, HeartbeatRequest, ListFilter, LogRequest,
    NoteRequest, QueueError, QueueService, ReleaseRequest, RiskLevel, StartRequest, TaskKind,
    TaskStatus, TreeQuery, NO_ELIGIBLE_REASON,
};
use q_project::{discover, DiscoverOptions};
use serde_json::{json, Map, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

const SERVER_NAME: &str = "q";
const SERVER_VERSION: &str = "0.1.0";
const KNOWN_VERSIONS: &[&str] = &["2024-11-05", "2025-03-26", "2025-06-18", "2025-11-25"];

/// Per-call context handed to every tool.
pub struct ToolContext {
    pub base_dir: PathBuf,
    /// Recorded on events. stdio sessions are the agent `mcp`; `q serve`
    /// sets the authenticated principal.
    pub actor: Actor,
    /// Expose `queue_ready` and `queue_reopen`. Only `q serve` sets this, for
    /// human tokens, so a local stdio agent never sees a ready tool.
    pub human_tools: bool,
    /// Reject a `capture_path` outside `base_dir`. `q serve` sets this: a
    /// remote token holder must not be able to run discovery (git, config
    /// files) against arbitrary directories on the server.
    pub confine_capture_path: bool,
    /// Fill a missing model from `Q_AGENT_MODEL` and a missing host from this
    /// machine. True for stdio `q mcp` on the worker. False for `q serve`,
    /// which records the host and model the remote client sent.
    pub record_local_identity: bool,
}

pub struct Session {
    initialized: bool,
    ctx: ToolContext,
}

impl Session {
    pub fn new(base_dir: PathBuf) -> Self {
        Self {
            initialized: false,
            ctx: ToolContext {
                base_dir,
                actor: Actor::agent("mcp"),
                human_tools: false,
                confine_capture_path: false,
                record_local_identity: true,
            },
        }
    }

    /// Record events as this actor instead of the agent `mcp`.
    pub fn with_actor(mut self, actor: Actor) -> Self {
        self.ctx.actor = actor;
        self
    }

    /// Expose the human-only triage tools.
    pub fn with_human_tools(mut self, human_tools: bool) -> Self {
        self.ctx.human_tools = human_tools;
        self
    }

    /// Treat the session as already initialized. Streamable HTTP is
    /// stateless, so each request may arrive without an `initialize`.
    pub fn stateless(mut self) -> Self {
        self.initialized = true;
        self
    }

    /// Only accept a `capture_path` inside the session's base directory.
    pub fn confined(mut self) -> Self {
        self.ctx.confine_capture_path = true;
        self
    }

    /// This session is running on `q serve`, not on the worker's machine.
    /// Claim identity is whatever the client sent.
    pub fn remote(mut self) -> Self {
        self.ctx.record_local_identity = false;
        self
    }

    /// Handle one parsed JSON-RPC message. `None` means a notification.
    pub fn handle_value(&mut self, queue: &dyn QueueService, message: &Value) -> Option<Value> {
        self.handle_message(queue, message)
    }

    /// Handle one JSON-RPC line. `None` means the client sent a notification.
    /// This is the stdio wrapper around the message handler.
    pub fn handle_line(&mut self, queue: &dyn QueueService, line: &str) -> Option<String> {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            return None;
        }
        let message: Value = match serde_json::from_str(trimmed) {
            Ok(value) => value,
            Err(_) => return Some(rpc_error(Value::Null, -32700, "parse error", None).to_string()),
        };
        self.handle_message(queue, &message)
            .map(|response| response.to_string())
    }

    fn handle_message(&mut self, queue: &dyn QueueService, message: &Value) -> Option<Value> {
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
                        "instructions": "Local queue. Captured tasks are ready and claimable at once unless captured with hold, which keeps them held until a human marks them ready. High and external-action risk are excluded from default claims."
                    }),
                ))
            }
            "ping" | "logging/setLevel" => Some(rpc_result(id, json!({}))),
            "tools/list" => {
                if !self.initialized {
                    return Some(rpc_error(id, -32600, "server not initialized", None));
                }
                Some(rpc_result(
                    id,
                    json!({ "tools": tool_definitions(self.ctx.human_tools) }),
                ))
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

    fn call_tool(&self, queue: &dyn QueueService, id: &Value, params: Option<&Value>) -> Value {
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
        match dispatch_tool(queue, &self.ctx, &name, &arguments) {
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

/// Tools that make work claimable. Listed and callable only for human
/// principals over `q serve`.
pub const HUMAN_ONLY_TOOLS: &[&str] = &["queue_ready", "queue_reopen"];

fn dispatch_tool(
    queue: &dyn QueueService,
    ctx: &ToolContext,
    name: &str,
    arguments: &Value,
) -> Result<Value, ToolFailure> {
    let args = arguments.as_object().expect("object checked by caller");
    if HUMAN_ONLY_TOOLS.contains(&name) && !ctx.human_tools {
        return Err(ToolFailure::Invalid(format!(
            "{name} is only available to a human signed in to q serve"
        )));
    }
    match name {
        "queue_capture" => queue_capture(queue, ctx, args),
        "queue_list" => queue_list(queue, args),
        "queue_get" => queue_get(queue, args),
        "queue_tree" => queue_tree(queue, args),
        "queue_status" => queue_status(queue, args),
        "queue_feature_create" => queue_feature_create(queue, args),
        "queue_feature_list" => queue_feature_list(queue, args),
        "queue_feature_get" => queue_feature_get(queue, args),
        "queue_edit" => queue_edit(queue, ctx, args),
        "queue_ready" => queue_ready(queue, ctx, args),
        "queue_reopen" => queue_reopen(queue, ctx, args),
        "queue_cancel" => queue_cancel(queue, ctx, args),
        "queue_claim_next" => queue_claim_next(queue, ctx, args),
        "queue_fail" => queue_fail(queue, ctx, args),
        "queue_escalate" => queue_escalate(queue, ctx, args),
        "queue_note" => queue_note(queue, ctx, args),
        "queue_heartbeat" => queue_heartbeat(queue, ctx, args),
        "queue_start" => queue_start(queue, ctx, args),
        "queue_block" => queue_block(queue, ctx, args),
        "queue_complete" => queue_complete(queue, ctx, args),
        "queue_release" => queue_release(queue, ctx, args),
        "queue_delete" => queue_delete(queue, ctx, args),
        "queue_log" => queue_log(queue, args),
        "queue_artifact" => queue_artifact(queue, args),
        other => Err(ToolFailure::Invalid(format!("unknown tool {other}"))),
    }
}

fn queue_status(queue: &dyn QueueService, args: &Map<String, Value>) -> Result<Value, ToolFailure> {
    expect_keys(args, &[])?;
    let status = queue.status()?;
    Ok(serde_json::to_value(status).unwrap_or(Value::Null))
}

fn queue_ready(
    queue: &dyn QueueService,
    ctx: &ToolContext,
    args: &Map<String, Value>,
) -> Result<Value, ToolFailure> {
    expect_keys(args, &["task_id", "id"])?;
    let outcome = queue.mark_ready(q_core::ReadyRequest {
        task_id: required_task_id(args)?,
        actor: ctx.actor.clone(),
    })?;
    Ok(serde_json::to_value(outcome).unwrap_or(Value::Null))
}

fn queue_reopen(
    queue: &dyn QueueService,
    ctx: &ToolContext,
    args: &Map<String, Value>,
) -> Result<Value, ToolFailure> {
    expect_keys(args, &["task_id", "id"])?;
    let task = queue.reopen(required_task_id(args)?, ctx.actor.clone())?;
    Ok(serde_json::to_value(task).unwrap_or(Value::Null))
}

fn queue_cancel(
    queue: &dyn QueueService,
    ctx: &ToolContext,
    args: &Map<String, Value>,
) -> Result<Value, ToolFailure> {
    expect_keys(args, &["task_id", "id"])?;
    let task = queue.cancel(q_core::CancelRequest {
        task_id: required_task_id(args)?,
        actor: ctx.actor.clone(),
    })?;
    Ok(serde_json::to_value(task).unwrap_or(Value::Null))
}

fn queue_edit(
    queue: &dyn QueueService,
    ctx: &ToolContext,
    args: &Map<String, Value>,
) -> Result<Value, ToolFailure> {
    expect_keys(
        args,
        &[
            "task_id",
            "id",
            "title",
            "body",
            "kind",
            "priority",
            "risk",
            "project",
            "repo",
            "agent_pool",
            "capabilities",
            "dependencies",
            "feature",
            "clear_project",
            "clear_repo",
            "clear_agent_pool",
            "clear_feature",
            "tags",
            "clear_tags",
        ],
    )?;
    let task_id = required_task_id(args)?;
    let mut request = q_core::EditRequest::empty(ctx.actor.clone());
    request.title = optional_string(args, "title")?;
    request.body = optional_string(args, "body")?;
    request.kind = match optional_string(args, "kind")? {
        Some(kind) => Some(TaskKind::parse(&kind)?),
        None => None,
    };
    request.priority = optional_i64(args, "priority")?.map(|value| value as i32);
    request.risk = match optional_string(args, "risk")? {
        Some(risk) => Some(RiskLevel::parse(&risk)?),
        None => None,
    };
    request.project = optional_string(args, "project")?;
    request.repo = optional_string(args, "repo")?;
    request.agent_pool = optional_string(args, "agent_pool")?;
    if args.contains_key("capabilities") {
        request.required_capabilities = Some(optional_string_array(args, "capabilities")?);
    }
    if args.contains_key("dependencies") {
        request.dependencies = Some(optional_i64_array(args, "dependencies")?);
    }
    request.feature = optional_feature(args)?;
    request.clear_project = optional_bool(args, "clear_project")?;
    request.clear_repo = optional_bool(args, "clear_repo")?;
    request.clear_agent_pool = optional_bool(args, "clear_agent_pool")?;
    request.clear_feature = optional_bool(args, "clear_feature")?;
    if args.contains_key("tags") {
        request.tags = Some(optional_string_array(args, "tags")?);
    }
    request.clear_tags = optional_bool(args, "clear_tags")?;
    if !request.has_changes() {
        return Err(ToolFailure::Invalid("no fields to change".into()));
    }
    let task = queue.edit(task_id, request)?;
    Ok(serde_json::to_value(task).unwrap_or(Value::Null))
}

/// Resolve a caller-supplied `capture_path`. A confined session only accepts
/// an existing directory inside `base_dir`, resolved through symlinks.
fn capture_directory(ctx: &ToolContext, path: PathBuf) -> Result<PathBuf, ToolFailure> {
    if !ctx.confine_capture_path {
        return Ok(path);
    }
    let base = ctx
        .base_dir
        .canonicalize()
        .map_err(|err| ToolFailure::Invalid(format!("served directory is unavailable: {err}")))?;
    let candidate = if path.is_absolute() {
        path
    } else {
        ctx.base_dir.join(path)
    };
    let resolved = candidate.canonicalize().map_err(|_| {
        ToolFailure::Invalid(
            "capture_path must be an existing directory inside the served directory".into(),
        )
    })?;
    if !resolved.starts_with(&base) {
        return Err(ToolFailure::Invalid(
            "capture_path must be inside the served directory".into(),
        ));
    }
    Ok(resolved)
}

fn queue_capture(
    queue: &dyn QueueService,
    ctx: &ToolContext,
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
            "hold",
            "tags",
        ],
    )?;
    let title = required_string(args, "title")?;
    let directory = match optional_string(args, "capture_path")? {
        Some(path) => capture_directory(ctx, PathBuf::from(path))?,
        None => ctx.base_dir.clone(),
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
        actor: ctx.actor.clone(),
        context_source: serde_json::to_value(context.source)
            .ok()
            .and_then(|value| value.as_str().map(str::to_string)),
        hold: optional_bool(args, "hold")?,
        tags: optional_string_array(args, "tags")?,
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
            "tags",
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
        tags: optional_string_array(args, "tags")?,
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
    ctx: &ToolContext,
    args: &Map<String, Value>,
) -> Result<Value, ToolFailure> {
    expect_keys(
        args,
        &[
            "agent_id",
            "task_id",
            "id",
            "capabilities",
            "allowed_repos",
            "allowed_projects",
            "allowed_kinds",
            "maximum_risk",
            "lease_minutes",
            "agent_pool",
            "agent_model",
            "agent_host",
            "tags",
            "max_failures",
        ],
    )?;
    let agent_id = required_string(args, "agent_id")?;
    let mut request = ClaimRequest::new(agent_id);
    request.task_id = match optional_i64(args, "task_id")? {
        Some(id) => Some(id),
        None => optional_i64(args, "id")?,
    };
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
    request.agent_model = optional_string(args, "agent_model")?;
    request.agent_host = optional_string(args, "agent_host")?;
    request.tags = optional_string_array(args, "tags")?;
    request.max_failures = match optional_u64(args, "max_failures")? {
        Some(value) => Some(
            u32::try_from(value)
                .map_err(|_| ToolFailure::Invalid("max_failures does not fit in u32".into()))?,
        ),
        None => None,
    };
    if ctx.record_local_identity {
        let (model, host) =
            q_core::local_worker_identity(request.agent_model.clone(), request.agent_host.clone());
        request.agent_model = model;
        request.agent_host = host;
    }
    let outcome = queue.claim_next(request)?;
    if !outcome.found {
        debug_assert_eq!(outcome.reason.as_deref(), Some(NO_ELIGIBLE_REASON));
    }
    Ok(serde_json::to_value(outcome).unwrap_or(Value::Null))
}

fn queue_fail(
    queue: &dyn QueueService,
    ctx: &ToolContext,
    args: &Map<String, Value>,
) -> Result<Value, ToolFailure> {
    expect_keys(args, &["task_id", "id", "claim_token", "note", "message"])?;
    let task = queue.fail(FailRequest {
        task_id: required_task_id(args)?,
        claim_token: required_string(args, "claim_token")?,
        note: optional_string(args, "note")?.or(optional_string(args, "message")?),
        actor: ctx.actor.clone(),
    })?;
    Ok(serde_json::to_value(task).unwrap_or(Value::Null))
}

fn queue_escalate(
    queue: &dyn QueueService,
    ctx: &ToolContext,
    args: &Map<String, Value>,
) -> Result<Value, ToolFailure> {
    expect_keys(args, &["task_id", "id", "claim_token", "reason", "message"])?;
    let task = queue.escalate(q_core::EscalateRequest {
        task_id: required_task_id(args)?,
        claim_token: required_string(args, "claim_token")?,
        reason: optional_string(args, "reason")?
            .or(optional_string(args, "message")?)
            .unwrap_or_default(),
        actor: ctx.actor.clone(),
    })?;
    Ok(serde_json::to_value(task).unwrap_or(Value::Null))
}

fn queue_note(
    queue: &dyn QueueService,
    ctx: &ToolContext,
    args: &Map<String, Value>,
) -> Result<Value, ToolFailure> {
    expect_keys(args, &["task_id", "id", "claim_token", "message"])?;
    let detail = queue.note(NoteRequest {
        task_id: required_task_id(args)?,
        claim_token: required_string(args, "claim_token")?,
        message: required_string(args, "message")?,
        actor: ctx.actor.clone(),
    })?;
    Ok(serde_json::to_value(detail).unwrap_or(Value::Null))
}

fn queue_heartbeat(
    queue: &dyn QueueService,
    ctx: &ToolContext,
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
        actor: ctx.actor.clone(),
    })?;
    Ok(serde_json::to_value(claim).unwrap_or(Value::Null))
}

fn queue_start(
    queue: &dyn QueueService,
    ctx: &ToolContext,
    args: &Map<String, Value>,
) -> Result<Value, ToolFailure> {
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
        actor: ctx.actor.clone(),
    })?;
    Ok(serde_json::to_value(detail).unwrap_or(Value::Null))
}

fn queue_block(
    queue: &dyn QueueService,
    ctx: &ToolContext,
    args: &Map<String, Value>,
) -> Result<Value, ToolFailure> {
    expect_keys(args, &["task_id", "id", "claim_token"])?;
    let task = queue.block(BlockRequest {
        task_id: required_task_id(args)?,
        claim_token: Some(required_string(args, "claim_token")?),
        actor: ctx.actor.clone(),
    })?;
    Ok(serde_json::to_value(task).unwrap_or(Value::Null))
}

fn queue_complete(
    queue: &dyn QueueService,
    ctx: &ToolContext,
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
    let artifacts = artifact_inputs(args)?;
    let detail = queue.complete(CompleteRequest {
        task_id: required_task_id(args)?,
        claim_token: Some(required_string(args, "claim_token")?),
        summary: required_string(args, "summary")?,
        target,
        artifacts,
        actor: ctx.actor.clone(),
    })?;
    Ok(serde_json::to_value(detail).unwrap_or(Value::Null))
}

/// Parse the optional `artifacts` array: objects with `kind`, `value`, and
/// optional `content` (text stored in the database).
fn artifact_inputs(args: &Map<String, Value>) -> Result<Vec<ArtifactInput>, ToolFailure> {
    let mut artifacts = Vec::new();
    if let Some(value) = args.get("artifacts") {
        let items = value
            .as_array()
            .ok_or_else(|| ToolFailure::Invalid("artifacts must be an array".into()))?;
        for item in items {
            let object = item
                .as_object()
                .ok_or_else(|| ToolFailure::Invalid("each artifact must be an object".into()))?;
            expect_keys(object, &["kind", "value", "content"])?;
            artifacts.push(ArtifactInput {
                kind: required_string(object, "kind")?,
                value: required_string(object, "value")?,
                content: optional_string(object, "content")?,
            });
        }
    }
    Ok(artifacts)
}

fn queue_log(queue: &dyn QueueService, args: &Map<String, Value>) -> Result<Value, ToolFailure> {
    expect_keys(
        args,
        &[
            "task_id",
            "id",
            "claim_token",
            "message",
            "progress",
            "artifacts",
        ],
    )?;
    let progress = match optional_u64(args, "progress")? {
        Some(percent) if percent <= 100 => Some(percent as u8),
        Some(_) => {
            return Err(ToolFailure::Invalid(
                "progress is a percent from 0 to 100".into(),
            ))
        }
        None => None,
    };
    let detail = queue.log(LogRequest {
        task_id: required_task_id(args)?,
        claim_token: optional_string(args, "claim_token")?,
        message: optional_string(args, "message")?,
        progress,
        artifacts: artifact_inputs(args)?,
        actor: Actor::agent("mcp"),
    })?;
    Ok(serde_json::to_value(detail).unwrap_or(Value::Null))
}

fn queue_artifact(
    queue: &dyn QueueService,
    args: &Map<String, Value>,
) -> Result<Value, ToolFailure> {
    expect_keys(args, &["artifact_id"])?;
    let id = optional_i64(args, "artifact_id")?
        .ok_or_else(|| ToolFailure::Invalid("artifact_id is required".into()))?;
    let artifact = queue.artifact(id)?;
    Ok(serde_json::to_value(artifact).unwrap_or(Value::Null))
}

fn queue_delete(
    queue: &dyn QueueService,
    ctx: &ToolContext,
    args: &Map<String, Value>,
) -> Result<Value, ToolFailure> {
    expect_keys(args, &["task_id", "force"])?;
    let outcome = queue.delete(DeleteRequest {
        task_id: required_task_id(args)?,
        force: optional_bool(args, "force")?,
        actor: ctx.actor.clone(),
    })?;
    Ok(serde_json::to_value(outcome).unwrap_or(Value::Null))
}

fn queue_release(
    queue: &dyn QueueService,
    ctx: &ToolContext,
    args: &Map<String, Value>,
) -> Result<Value, ToolFailure> {
    expect_keys(args, &["task_id", "id", "claim_token"])?;
    let task = queue.release(ReleaseRequest {
        task_id: required_task_id(args)?,
        claim_token: required_string(args, "claim_token")?,
        actor: ctx.actor.clone(),
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

fn rpc_result(id: Value, result: Value) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": result,
    })
}

fn rpc_error(id: Value, code: i64, message: &str, data: Option<Value>) -> Value {
    let mut error = json!({"code": code, "message": message});
    if let Some(data) = data {
        error["data"] = data;
    }
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": error,
    })
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

fn tool_definitions(human_tools: bool) -> Vec<Value> {
    let mut tools = vec![
        tool(
            "queue_capture",
            "Capture a task. It is ready and claimable at once unless hold is true, which keeps it held until a human runs q ready.",
            json!({
                "type": "object",
                "required": ["title"],
                "properties": {
                    "title": {"type": "string"},
                    "body": {"type": "string"},
                    "hold": {
                        "type": "boolean",
                        "description": "Create the task as held instead of ready. Held work is never claimable until a human marks it ready. Defaults to false."
                    },
                    "repo": {"type": "string"},
                    "project": {"type": "string"},
                    "capture_path": {"type": "string"},
                    "kind": {"type": "string", "enum": ["implementation", "research", "review", "benchmark", "documentation", "other"]},
                    "priority": {"type": "integer"},
                    "risk": {"type": "string", "enum": ["low", "medium", "high", "external_action"]},
                    "capabilities": {"type": "array", "items": {"type": "string"}},
                    "dependencies": {"type": "array", "items": {"type": "integer"}},
                    "agent_pool": {"type": "string"},
                    "tags": {
                        "type": "array",
                        "items": {"type": "string"},
                        "description": "Labels stored on the task. A later claim can filter on them."
                    },
                    "feature": {
                        "description": "Feature id or unique title. The task keeps its own repo and project.",
                        "anyOf": [{"type": "string"}, {"type": "integer"}]
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
                    "status": {
                        "type": "string",
                        "description": "held, ready, claimed, in_progress, review, blocked, escalated, done, or cancelled."
                    },
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
                    },
                    "tags": {
                        "type": "array",
                        "items": {"type": "string"},
                        "description": "Keep tasks that carry every one of these tags."
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
            "Atomically claim one eligible ready task, or return found=false when none are eligible. Pass task_id to claim that task instead; it must be ready and pass the same filters, or the call fails and says why.",
            json!({
                "type": "object",
                "required": ["agent_id"],
                "properties": {
                    "agent_id": {"type": "string"},
                    "task_id": {"type": "integer"},
                    "id": {"type": "integer"},
                    "capabilities": {"type": "array", "items": {"type": "string"}},
                    "allowed_repos": {"type": "array", "items": {"type": "string"}},
                    "allowed_projects": {"type": "array", "items": {"type": "string"}},
                    "allowed_kinds": {"type": "array", "items": {"type": "string"}},
                    "maximum_risk": {"type": "string", "enum": ["low", "medium", "high", "external_action"]},
                    "lease_minutes": {"type": "integer"},
                    "agent_pool": {"type": "string"},
                    "agent_model": {
                        "type": "string",
                        "description": "Model name recorded on the claim. Stdio sessions fall back to Q_AGENT_MODEL."
                    },
                    "agent_host": {
                        "type": "string",
                        "description": "Hostname recorded on the claim. Stdio sessions detect this machine when omitted. A q serve session stores only what the client sends."
                    },
                    "tags": {
                        "type": "array",
                        "items": {"type": "string"},
                        "description": "Claim only tasks that carry every one of these tags. Omit to leave tag filtering unrestricted."
                    },
                    "max_failures": {
                        "type": "integer",
                        "description": "Skip tasks that have already failed this many times. Omit for no cap."
                    }
                },
                "additionalProperties": false
            }),
        ),
        tool(
            "queue_fail",
            "Release the claim and return the task to ready so another agent can take it. Increments the task's failure count. note is optional.",
            json!({
                "type": "object",
                "required": ["task_id", "claim_token"],
                "properties": {
                    "task_id": {"type": "integer"},
                    "claim_token": {"type": "string"},
                    "note": {"type": "string", "description": "Optional short failure note stored on the event."}
                },
                "additionalProperties": false
            }),
        ),
        tool(
            "queue_escalate",
            "Release the claim and park the task as escalated for a human to review. Use this when the task is too big or you lack the tools or context. It is not claimable again until a human marks it ready. reason is required. A genuine execution failure uses queue_fail.",
            json!({
                "type": "object",
                "required": ["task_id", "claim_token", "reason"],
                "properties": {
                    "task_id": {"type": "integer"},
                    "claim_token": {"type": "string"},
                    "reason": {"type": "string", "description": "Why this task needs a human before another agent tries it."}
                },
                "additionalProperties": false
            }),
        ),
        tool(
            "queue_note",
            "Append a short status line to a claimed task. q top shows the latest note while the claim is active.",
            json!({
                "type": "object",
                "required": ["task_id", "claim_token", "message"],
                "properties": {
                    "task_id": {"type": "integer"},
                    "claim_token": {"type": "string"},
                    "message": {"type": "string"}
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
                    "artifacts": ARTIFACTS_SCHEMA
                },
                "additionalProperties": false
            }),
        ),
        tool(
            "queue_log",
            "Append to a task's log: a timestamped note (thinking steps, findings), a progress percent, artifacts, or any mix. Pass the claim token so the entry is attributed to your agent id. Report progress as you pass milestones; q top shows it for in-progress tasks. An artifact with content stores that text (for example a Markdown or HTML report) in the database.",
            json!({
                "type": "object",
                "required": ["task_id"],
                "properties": {
                    "task_id": {"type": "integer"},
                    "claim_token": {"type": "string"},
                    "message": {"type": "string"},
                    "progress": {"type": "integer", "minimum": 0, "maximum": 100, "description": "Percent complete"},
                    "artifacts": ARTIFACTS_SCHEMA
                },
                "additionalProperties": false
            }),
        ),
        tool(
            "queue_artifact",
            "Fetch one artifact by id with its stored content.",
            json!({
                "type": "object",
                "required": ["artifact_id"],
                "properties": {
                    "artifact_id": {"type": "integer"}
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
        tool(
            "queue_status",
            "Counts per status plus active and expired claims.",
            json!({"type": "object", "properties": {}, "additionalProperties": false}),
        ),
        tool(
            "queue_edit",
            "Edit task fields. Omitted fields are unchanged. Pass an empty array to clear capabilities or dependencies, and clear_* flags to unset project, repo, agent_pool, or feature.",
            json!({
                "type": "object",
                "required": ["task_id"],
                "properties": {
                    "task_id": {"type": "integer"},
                    "title": {"type": "string"},
                    "body": {"type": "string"},
                    "kind": {"type": "string", "enum": ["implementation", "research", "review", "benchmark", "documentation", "other"]},
                    "priority": {"type": "integer"},
                    "risk": {"type": "string", "enum": ["low", "medium", "high", "external_action"]},
                    "project": {"type": "string"},
                    "repo": {"type": "string"},
                    "agent_pool": {"type": "string"},
                    "capabilities": {"type": "array", "items": {"type": "string"}},
                    "dependencies": {"type": "array", "items": {"type": "integer"}},
                    "feature": {"anyOf": [{"type": "string"}, {"type": "integer"}]},
                    "clear_project": {"type": "boolean"},
                    "clear_repo": {"type": "boolean"},
                    "clear_agent_pool": {"type": "boolean"},
                    "clear_feature": {"type": "boolean"},
                    "tags": {
                        "type": "array",
                        "items": {"type": "string"},
                        "description": "Replace the task's tags. An empty array clears them."
                    },
                    "clear_tags": {"type": "boolean"}
                },
                "additionalProperties": false
            }),
        ),
        tool(
            "queue_cancel",
            "Cancel a task. The task and its history are kept; use queue_delete to remove it.",
            json!({
                "type": "object",
                "required": ["task_id"],
                "properties": {"task_id": {"type": "integer"}},
                "additionalProperties": false
            }),
        ),
    ];
    if human_tools {
        tools.push(tool(
            "queue_ready",
            "Move a held or blocked task to ready so agents may claim it. Confirm the task is well specified first.",
            json!({
                "type": "object",
                "required": ["task_id"],
                "properties": {"task_id": {"type": "integer"}},
                "additionalProperties": false
            }),
        ));
        tools.push(tool(
            "queue_reopen",
            "Move a done task back to ready.",
            json!({
                "type": "object",
                "required": ["task_id"],
                "properties": {"task_id": {"type": "integer"}},
                "additionalProperties": false
            }),
        ));
    }
    tools
}

/// Schema for an `artifacts` array on complete and log.
const ARTIFACTS_SCHEMA: &str = "__artifacts__";

fn tool(name: &str, description: &str, mut input_schema: Value) -> Value {
    if let Some(artifacts) = input_schema
        .get_mut("properties")
        .and_then(|properties| properties.get_mut("artifacts"))
    {
        if artifacts.as_str() == Some(ARTIFACTS_SCHEMA) {
            *artifacts = json!({
                "type": "array",
                "items": {
                    "type": "object",
                    "required": ["kind", "value"],
                    "properties": {
                        "kind": {"type": "string"},
                        "value": {"type": "string"},
                        "content": {"type": "string", "description": "Text to store in the database, such as a report body"}
                    }
                }
            });
        }
    }
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
            "queue_log",
            "queue_artifact",
        ] {
            assert!(names.iter().any(|candidate| candidate == name), "{name}");
        }
    }

    #[test]
    fn queue_claim_next_takes_a_task_id() {
        let queue = temp_queue();
        let mut session = Session::new(std::env::temp_dir());
        call(
            &mut session,
            &queue,
            "initialize",
            1,
            json!({"protocolVersion": "2025-03-26", "capabilities": {}, "clientInfo": {"name": "test", "version": "0"}}),
        );
        let first = call(
            &mut session,
            &queue,
            "tools/call",
            2,
            json!({"name": "queue_capture", "arguments": {"title": "first", "priority": 10}}),
        );
        let first = tool_body(&first)["id"].as_i64().unwrap();
        let second = call(
            &mut session,
            &queue,
            "tools/call",
            3,
            json!({"name": "queue_capture", "arguments": {"title": "second"}}),
        );
        let second = tool_body(&second)["id"].as_i64().unwrap();

        let claimed = call(
            &mut session,
            &queue,
            "tools/call",
            4,
            json!({"name": "queue_claim_next", "arguments": {"agent_id": "bot-7", "task_id": second}}),
        );
        assert_eq!(claimed["result"]["isError"], false, "{claimed}");
        let body = tool_body(&claimed);
        assert_eq!(body["found"], true);
        assert_eq!(body["task"]["id"], second);

        // Asking for it again is a domain error, not a silent found=false.
        let again = call(
            &mut session,
            &queue,
            "tools/call",
            5,
            json!({"name": "queue_claim_next", "arguments": {"agent_id": "bot-8", "id": second}}),
        );
        assert_eq!(again["result"]["isError"], true, "{again}");
        let text = again["result"]["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("already claimed by bot-7"), "{text}");

        // The higher-priority task is still there for an untargeted claim.
        let next = call(
            &mut session,
            &queue,
            "tools/call",
            6,
            json!({"name": "queue_claim_next", "arguments": {"agent_id": "bot-8"}}),
        );
        assert_eq!(tool_body(&next)["task"]["id"], first);
    }

    #[test]
    fn queue_log_and_queue_artifact_round_trip() {
        let queue = temp_queue();
        let mut session = Session::new(std::env::temp_dir());
        call(
            &mut session,
            &queue,
            "initialize",
            1,
            json!({"protocolVersion": "2025-03-26", "capabilities": {}, "clientInfo": {"name": "test", "version": "0"}}),
        );
        let captured = call(
            &mut session,
            &queue,
            "tools/call",
            2,
            json!({"name": "queue_capture", "arguments": {"title": "log me"}}),
        );
        // Captures are ready by default, so the task is claimable at once.
        let id = tool_body(&captured)["id"].as_i64().unwrap();
        let claimed = call(
            &mut session,
            &queue,
            "tools/call",
            3,
            json!({"name": "queue_claim_next", "arguments": {"agent_id": "bot-7"}}),
        );
        let token = tool_body(&claimed)["claim"]["token"]
            .as_str()
            .unwrap()
            .to_string();

        let logged = call(
            &mut session,
            &queue,
            "tools/call",
            4,
            json!({"name": "queue_log", "arguments": {
                "task_id": id,
                "claim_token": token.clone(),
                "message": "Thinking: start with the parser",
                "artifacts": [{"kind": "report", "value": "notes.md", "content": "# Notes\n"}]
            }}),
        );
        assert_eq!(logged["result"]["isError"], false, "{logged}");
        let detail = tool_body(&logged);
        assert!(detail.get("progress").is_none());
        let note = detail["events"]
            .as_array()
            .unwrap()
            .iter()
            .find(|event| event["event_type"] == "task_note")
            .unwrap();
        assert_eq!(note["actor_id"], "bot-7");
        let artifact_id = detail["artifacts"][0]["id"].as_i64().unwrap();
        assert_eq!(detail["artifacts"][0]["content_bytes"], 8);

        let fetched = call(
            &mut session,
            &queue,
            "tools/call",
            5,
            json!({"name": "queue_artifact", "arguments": {"artifact_id": artifact_id}}),
        );
        assert_eq!(tool_body(&fetched)["content"], "# Notes\n");
        let progressed = call(
            &mut session,
            &queue,
            "tools/call",
            8,
            json!({"name": "queue_log", "arguments": {"task_id": id, "claim_token": token, "progress": 40}}),
        );
        assert_eq!(tool_body(&progressed)["progress"], 40, "{progressed}");
        let too_much = call(
            &mut session,
            &queue,
            "tools/call",
            9,
            json!({"name": "queue_log", "arguments": {"task_id": id, "progress": 101}}),
        );
        assert_eq!(too_much["error"]["code"], -32602, "{too_much}");

        let empty = call(
            &mut session,
            &queue,
            "tools/call",
            6,
            json!({"name": "queue_log", "arguments": {"task_id": id}}),
        );
        assert_eq!(empty["error"]["code"], -32602, "{empty}");
        assert!(empty["error"]["data"]["error"]
            .as_str()
            .unwrap()
            .contains("message, a progress percent, or at least one artifact"));

        let unknown_key = call(
            &mut session,
            &queue,
            "tools/call",
            7,
            json!({"name": "queue_log", "arguments": {"task_id": id, "note": "x"}}),
        );
        assert_eq!(unknown_key["error"]["code"], -32602);
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
        assert_eq!(queue.get(id).unwrap().task.status, TaskStatus::Ready);

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
        assert_eq!(body["status"], "ready");
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
                hold: true,
                tags: vec![],
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

    #[test]
    fn human_tools_are_gated_by_the_session() {
        let queue = temp_queue();

        // A stdio session never lists or runs the ready tool.
        let mut agent = Session::new(std::env::temp_dir());
        call(&mut agent, &queue, "initialize", 1, json!({}));
        let listed = call(&mut agent, &queue, "tools/list", 2, json!({}));
        let names: Vec<String> = listed["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|tool| tool["name"].as_str().unwrap().to_string())
            .collect();
        assert!(!names.iter().any(|name| name == "queue_ready"));
        assert!(names.iter().any(|name| name == "queue_edit"));
        assert!(names.iter().any(|name| name == "queue_cancel"));
        assert!(names.iter().any(|name| name == "queue_status"));
        let captured = call(
            &mut agent,
            &queue,
            "tools/call",
            3,
            json!({"name": "queue_capture", "arguments": {"title": "Chat triage", "hold": true, "capture_path": std::env::temp_dir()}}),
        );
        let id = tool_body(&captured)["id"].as_i64().unwrap();
        let denied = call(
            &mut agent,
            &queue,
            "tools/call",
            4,
            json!({"name": "queue_ready", "arguments": {"task_id": id}}),
        );
        assert_eq!(denied["error"]["code"], -32602);
        assert!(denied["error"]["data"]["error"]
            .as_str()
            .unwrap()
            .contains("human"));

        // A human session over q serve lists it, runs it, and is recorded as
        // the human, not as the agent `mcp`.
        let mut human = Session::new(std::env::temp_dir())
            .with_actor(Actor::human(Some("pierric".into())))
            .with_human_tools(true)
            .stateless();
        let listed = call(&mut human, &queue, "tools/list", 5, json!({}));
        assert!(listed["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .any(|tool| tool["name"] == "queue_ready"));
        let edited = call(
            &mut human,
            &queue,
            "tools/call",
            6,
            json!({"name": "queue_edit", "arguments": {"task_id": id, "priority": 5, "body": "Goal: ship it"}}),
        );
        assert_eq!(tool_body(&edited)["priority"], 5);
        let ready = call(
            &mut human,
            &queue,
            "tools/call",
            7,
            json!({"name": "queue_ready", "arguments": {"task_id": id}}),
        );
        assert_eq!(tool_body(&ready)["task"]["status"], "ready");
        let status = call(
            &mut human,
            &queue,
            "tools/call",
            8,
            json!({"name": "queue_status", "arguments": {}}),
        );
        assert_eq!(tool_body(&status)["counts"]["ready"], 1);
        let events = queue.events(id).unwrap();
        let ready_event = events
            .iter()
            .find(|event| event.event_type == "task_ready")
            .unwrap();
        assert_eq!(ready_event.actor_type, "human");
        assert_eq!(ready_event.actor_id.as_deref(), Some("pierric"));
        let cancelled = call(
            &mut human,
            &queue,
            "tools/call",
            9,
            json!({"name": "queue_cancel", "arguments": {"task_id": id}}),
        );
        assert_eq!(tool_body(&cancelled)["status"], "cancelled");
        let nothing = call(
            &mut human,
            &queue,
            "tools/call",
            10,
            json!({"name": "queue_edit", "arguments": {"task_id": id}}),
        );
        assert_eq!(nothing["error"]["code"], -32602);
    }

    #[test]
    fn confined_sessions_keep_capture_paths_inside_the_served_directory() {
        let queue = temp_queue();
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let base = std::env::temp_dir().join(format!("q-confined-{nanos}"));
        std::fs::create_dir_all(base.join("inner")).unwrap();
        let mut session = Session::new(base.clone()).stateless().confined();
        let outside = session
            .handle_value(
                &queue,
                &json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {
                    "name": "queue_capture",
                    "arguments": {"title": "peek", "capture_path": "/"}
                }}),
            )
            .unwrap();
        assert_eq!(outside["error"]["code"], -32602, "{outside}");
        assert!(outside["error"]["data"]["error"]
            .as_str()
            .unwrap()
            .contains("served directory"));
        let inside = session
            .handle_value(
                &queue,
                &json!({"jsonrpc": "2.0", "id": 2, "method": "tools/call", "params": {
                    "name": "queue_capture",
                    "arguments": {"title": "ok", "capture_path": "inner"}
                }}),
            )
            .unwrap();
        assert_eq!(inside["result"]["isError"], false, "{inside}");
        // An unconfined (stdio) session keeps the old behaviour.
        let mut local = Session::new(base.clone()).stateless();
        let anywhere = local
            .handle_value(
                &queue,
                &json!({"jsonrpc": "2.0", "id": 3, "method": "tools/call", "params": {
                    "name": "queue_capture",
                    "arguments": {"title": "local", "capture_path": std::env::temp_dir()}
                }}),
            )
            .unwrap();
        assert_eq!(anywhere["result"]["isError"], false, "{anywhere}");
        let _ = std::fs::remove_dir_all(base);
    }

    #[test]
    fn capture_is_ready_by_default_and_hold_keeps_it_held() {
        let queue = temp_queue();
        let mut session = Session::new(std::env::temp_dir());
        call(
            &mut session,
            &queue,
            "initialize",
            1,
            json!({"protocolVersion": "2025-03-26", "capabilities": {}, "clientInfo": {"name": "test", "version": "0"}}),
        );
        let open = tool_body(&call(
            &mut session,
            &queue,
            "tools/call",
            2,
            json!({"name": "queue_capture", "arguments": {"title": "open work"}}),
        ));
        assert_eq!(open["status"], "ready");
        let held = tool_body(&call(
            &mut session,
            &queue,
            "tools/call",
            3,
            json!({"name": "queue_capture", "arguments": {"title": "held work", "hold": true}}),
        ));
        assert_eq!(held["status"], "held");

        let claim = tool_body(&call(
            &mut session,
            &queue,
            "tools/call",
            4,
            json!({"name": "queue_claim_next", "arguments": {"agent_id": "bot"}}),
        ));
        assert_eq!(claim["task"]["id"], open["id"]);
        let none = tool_body(&call(
            &mut session,
            &queue,
            "tools/call",
            5,
            json!({"name": "queue_claim_next", "arguments": {"agent_id": "bot-2"}}),
        ));
        assert_eq!(none["found"], false);

        // Filtering by held returns only the held task.
        let listed = tool_body(&call(
            &mut session,
            &queue,
            "tools/call",
            6,
            json!({"name": "queue_list", "arguments": {"status": "held"}}),
        ));
        let tasks = listed["tasks"].as_array().unwrap();
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0]["id"], held["id"]);
        assert_eq!(tasks[0]["status"], "held");

        let tools = call(&mut session, &queue, "tools/list", 7, json!({}));
        let capture_tool = tools["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .find(|tool| tool["name"] == "queue_capture")
            .unwrap();
        assert_eq!(
            capture_tool["inputSchema"]["properties"]["hold"]["type"],
            "boolean"
        );
    }

    #[test]
    fn claim_tags_identity_fail_and_notes_round_trip() {
        let queue = temp_queue();
        let mut session = Session::new(std::env::temp_dir());
        call(
            &mut session,
            &queue,
            "initialize",
            1,
            json!({"protocolVersion": "2025-03-26", "capabilities": {}, "clientInfo": {"name": "test", "version": "0"}}),
        );
        let rust = tool_body(&call(
            &mut session,
            &queue,
            "tools/call",
            2,
            json!({"name": "queue_capture", "arguments": {"title": "parser", "tags": ["rust"]}}),
        ));
        let docs = tool_body(&call(
            &mut session,
            &queue,
            "tools/call",
            3,
            json!({"name": "queue_capture", "arguments": {"title": "guide", "tags": ["docs"]}}),
        ));
        let claimed = tool_body(&call(
            &mut session,
            &queue,
            "tools/call",
            4,
            json!({"name": "queue_claim_next", "arguments": {
                "agent_id": "bot",
                "tags": ["Rust"],
                "agent_model": "opus"
            }}),
        ));
        assert_eq!(claimed["found"], true);
        assert_eq!(claimed["task"]["id"], rust["id"]);
        assert_eq!(claimed["claim"]["agent_model"], "opus");
        assert!(
            !claimed["claim"]["agent_host"]
                .as_str()
                .unwrap_or("")
                .is_empty(),
            "stdio fills the local hostname: {claimed}"
        );
        let token = claimed["claim"]["token"].as_str().unwrap();
        let noted = tool_body(&call(
            &mut session,
            &queue,
            "tools/call",
            5,
            json!({"name": "queue_note", "arguments": {
                "task_id": rust["id"],
                "claim_token": token,
                "message": "running tests"
            }}),
        ));
        assert!(noted["events"]
            .as_array()
            .unwrap()
            .iter()
            .any(|event| event["event_type"] == "task_note"
                && event["payload"]["message"] == "running tests"));
        let failed = tool_body(&call(
            &mut session,
            &queue,
            "tools/call",
            6,
            json!({"name": "queue_fail", "arguments": {
                "task_id": rust["id"],
                "claim_token": token,
                "note": "tests failed"
            }}),
        ));
        assert_eq!(failed["status"], "ready");
        assert_eq!(failed["failure_count"], 1);
        let skipped = tool_body(&call(
            &mut session,
            &queue,
            "tools/call",
            7,
            json!({"name": "queue_claim_next", "arguments": {
                "agent_id": "bot-2",
                "tags": ["rust"],
                "max_failures": 1
            }}),
        ));
        assert_eq!(skipped["found"], false);

        let mut remote = Session::new(std::env::temp_dir()).remote();
        call(
            &mut remote,
            &queue,
            "initialize",
            1,
            json!({"protocolVersion": "2025-03-26", "capabilities": {}, "clientInfo": {"name": "test", "version": "0"}}),
        );
        let remote_claim = tool_body(&call(
            &mut remote,
            &queue,
            "tools/call",
            2,
            json!({"name": "queue_claim_next", "arguments": {
                "agent_id": "remote-bot",
                "tags": ["docs"]
            }}),
        ));
        assert_eq!(remote_claim["task"]["id"], docs["id"]);
        assert!(
            remote_claim["claim"].get("agent_host").is_none(),
            "{remote_claim}"
        );
        assert!(
            remote_claim["claim"].get("agent_model").is_none(),
            "{remote_claim}"
        );

        let tools = call(&mut session, &queue, "tools/list", 8, json!({}));
        let names: Vec<_> = tools["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|tool| tool["name"].as_str().unwrap())
            .collect();
        assert!(names.contains(&"queue_fail"), "{names:?}");
        assert!(names.contains(&"queue_note"), "{names:?}");
    }

    #[test]
    fn escalate_releases_the_claim_until_a_human_marks_it_ready() {
        let queue = temp_queue();
        let mut session = Session::new(std::env::temp_dir());
        call(
            &mut session,
            &queue,
            "initialize",
            1,
            json!({"protocolVersion": "2025-03-26", "capabilities": {}, "clientInfo": {"name": "test", "version": "0"}}),
        );
        let captured = tool_body(&call(
            &mut session,
            &queue,
            "tools/call",
            2,
            json!({"name": "queue_capture", "arguments": {"title": "too big"}}),
        ));
        let id = captured["id"].as_i64().unwrap();
        let claimed = tool_body(&call(
            &mut session,
            &queue,
            "tools/call",
            3,
            json!({"name": "queue_claim_next", "arguments": {"agent_id": "bot"}}),
        ));
        assert_eq!(claimed["task"]["id"], id);
        let token = claimed["claim"]["token"].as_str().unwrap();
        let escalated = tool_body(&call(
            &mut session,
            &queue,
            "tools/call",
            4,
            json!({"name": "queue_escalate", "arguments": {
                "task_id": id,
                "claim_token": token,
                "reason": "missing the schema"
            }}),
        ));
        assert_eq!(escalated["status"], "escalated");
        assert_eq!(escalated["escalated_reason"], "missing the schema");
        assert_eq!(escalated["escalated_by"], "agent:bot");
        assert!(escalated.get("escalated_at").is_some());
        assert!(escalated.get("failure_count").is_none());

        let none = tool_body(&call(
            &mut session,
            &queue,
            "tools/call",
            5,
            json!({"name": "queue_claim_next", "arguments": {"agent_id": "bot-2"}}),
        ));
        assert_eq!(none["found"], false);
        let listed = tool_body(&call(
            &mut session,
            &queue,
            "tools/call",
            6,
            json!({"name": "queue_list", "arguments": {"status": "escalated"}}),
        ));
        assert_eq!(listed["tasks"].as_array().unwrap().len(), 1);
        assert_eq!(listed["tasks"][0]["id"], id);
        assert_eq!(listed["tasks"][0]["escalated_by"], "agent:bot");

        let denied = call(
            &mut session,
            &queue,
            "tools/call",
            7,
            json!({"name": "queue_ready", "arguments": {"task_id": id}}),
        );
        assert_eq!(denied["error"]["code"], -32602);

        let tools = call(&mut session, &queue, "tools/list", 8, json!({}));
        let names: Vec<_> = tools["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|tool| tool["name"].as_str().unwrap())
            .collect();
        assert!(names.contains(&"queue_escalate"), "{names:?}");

        let mut human = Session::new(std::env::temp_dir())
            .with_actor(Actor::human(Some("pierric".into())))
            .with_human_tools(true);
        call(
            &mut human,
            &queue,
            "initialize",
            1,
            json!({"protocolVersion": "2025-03-26", "capabilities": {}, "clientInfo": {"name": "test", "version": "0"}}),
        );
        let ready = tool_body(&call(
            &mut human,
            &queue,
            "tools/call",
            2,
            json!({"name": "queue_ready", "arguments": {"task_id": id}}),
        ));
        assert_eq!(ready["task"]["status"], "ready");
        assert!(ready["task"].get("escalated_reason").is_none());
        assert!(ready["task"].get("escalated_by").is_none());
        let again = tool_body(&call(
            &mut session,
            &queue,
            "tools/call",
            9,
            json!({"name": "queue_claim_next", "arguments": {"agent_id": "bot-3"}}),
        ));
        assert_eq!(again["found"], true);
        assert_eq!(again["task"]["id"], id);
    }
}
