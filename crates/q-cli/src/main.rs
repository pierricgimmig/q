mod cli;
mod skill;

use std::fs;
use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use clap::Parser;
use q_core::{
    format_timestamp, lease_from_minutes, Actor, ArtifactInput, BlockRequest, CancelRequest,
    CaptureRequest, ClaimRequest, CompleteRequest, DeleteRequest, EditRequest, HeartbeatRequest,
    ListFilter, QueueError, QueueService, ReadyRequest, RecoverRequest, ReleaseRequest, RiskLevel,
    StaleDisposition, StartRequest, TaskKind, TaskStatus, TaskSummary,
};
use q_project::{discover, render_init_config, DiscoverOptions, ProjectContext};
use q_store::{default_db_path, Queue};

use crate::cli::{Commands, ProjectCommand};

#[tokio::main(flavor = "current_thread")]
async fn main() {
    init_tracing();
    let raw: Vec<String> = std::env::args().skip(1).collect();
    let args = cli::preprocess(raw);
    let cli = match cli::Cli::try_parse_from(std::iter::once(String::from("q")).chain(args)) {
        Ok(cli) => cli,
        Err(error) => error.exit(),
    };
    if let Err(error) = run(cli).await {
        eprintln!("error: {error}");
        std::process::exit(1);
    }
}

fn init_tracing() {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("error"));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .with_target(false)
        .try_init();
}

async fn run(cli: cli::Cli) -> Result<(), CliError> {
    let json = cli.json;
    let db = db_path(&cli);
    let repo = cli.repo.clone();
    let project = cli.project.clone();
    let directory = cli.directory.clone();
    match cli.command {
        Commands::Mcp => {
            tracing::info!(db = %db.display(), "opening queue");
            let queue = Arc::new(Queue::open(&db)?);
            q_mcp::serve(queue, base_dir(directory.as_deref())?).await?;
            Ok(())
        }
        Commands::Skill { command } => match command {
            None => skill::print_skill(json).map_err(CliError::message),
            Some(cli::SkillCommand::Install { target, force }) => {
                let home = skill::home_dir().map_err(CliError::message)?;
                skill::install_skill(&home, &target, force, json).map_err(CliError::message)
            }
        },
        Commands::Project { command } => match command {
            ProjectCommand::Show => {
                let context = resolve_context(directory.as_deref(), repo, project)?;
                emit(json, &context, || print_context(&context));
                Ok(())
            }
            ProjectCommand::Init { yes, force } => {
                init_project(directory.as_deref(), repo, project, yes, force)
            }
        },
        other => {
            tracing::info!(db = %db.display(), "opening queue");
            let queue = Queue::open(&db)?;
            dispatch(
                &queue,
                &db,
                repo,
                project,
                directory.as_deref(),
                other,
                json,
            )
        }
    }
}

fn dispatch(
    queue: &Queue,
    db: &Path,
    repo: Option<String>,
    project: Option<String>,
    directory: Option<&Path>,
    command: Commands,
    json: bool,
) -> Result<(), CliError> {
    match command {
        Commands::Add {
            title,
            kind,
            priority,
            risk,
            body,
            body_file,
            capability,
            agent_pool,
            depends_on,
        } => {
            let context = resolve_context(directory, repo, project)?;
            let body = read_body(body, body_file.as_deref())?;
            let kind = match kind {
                Some(kind) => TaskKind::parse(&kind)?,
                None => context.default_kind.unwrap_or(TaskKind::Implementation),
            };
            let risk = match risk {
                Some(risk) => RiskLevel::parse(&risk)?,
                None => RiskLevel::Low,
            };
            let task = queue.capture(CaptureRequest {
                title: title.join(" "),
                body,
                kind,
                priority: priority.unwrap_or(0),
                risk,
                project: context.project,
                repo: context.repo,
                capture_path: context.capture_path.display().to_string(),
                repo_relative_path: context.repo_relative_path,
                git_root: context.git_root.map(|path| path.display().to_string()),
                git_head: context.git_head,
                agent_pool: agent_pool.or(context.agent_pool),
                required_capabilities: split_caps(capability),
                dependencies: parse_ids(depends_on.as_deref())?,
                policy: context.policy,
                actor: human_actor(),
                context_source: serde_json::to_value(context.source)
                    .ok()
                    .and_then(|value| value.as_str().map(str::to_string)),
            })?;
            let id = task.id;
            emit(json, &task, || {
                println!("captured #{id} [{}] {}", task.status, task.title);
            });
            Ok(())
        }
        Commands::Ls {
            status,
            kind,
            limit,
            all,
        } => {
            let status = match status {
                Some(status) => Some(TaskStatus::parse(&status)?),
                None => None,
            };
            let kind = match kind {
                Some(kind) => Some(TaskKind::parse(&kind)?),
                None => None,
            };
            let tasks = queue.list(ListFilter {
                status,
                project: project.clone(),
                repo: repo.clone(),
                kind,
                limit,
                include_terminal: all,
            })?;
            emit(json, &serde_json::json!({"tasks": tasks}), || {
                print_task_list(&tasks);
            });
            Ok(())
        }
        Commands::Show { id } => {
            let detail = queue.get(id)?;
            emit(json, &detail, || print_detail(&detail));
            Ok(())
        }
        Commands::Edit {
            id,
            title,
            body,
            body_file,
            kind,
            priority,
            risk,
            capability,
            agent_pool,
            depends_on,
            clear_project,
            clear_repo,
            clear_agent_pool,
        } => {
            let mut request = EditRequest::empty(human_actor());
            request.title = title;
            request.body = read_body(body, body_file.as_deref())?;
            request.kind = match kind {
                Some(kind) => Some(TaskKind::parse(&kind)?),
                None => None,
            };
            request.priority = priority;
            request.risk = match risk {
                Some(risk) => Some(RiskLevel::parse(&risk)?),
                None => None,
            };
            if !capability.is_empty() {
                request.required_capabilities = Some(split_caps(capability));
            }
            request.agent_pool = agent_pool;
            if depends_on.is_some() {
                request.dependencies = Some(parse_ids(depends_on.as_deref())?);
            }
            request.project = project.clone();
            request.repo = repo.clone();
            request.clear_project = clear_project;
            request.clear_repo = clear_repo;
            request.clear_agent_pool = clear_agent_pool;
            if !request.has_changes() {
                let current = queue.get(id)?;
                let edited = edit_in_editor(current.task.body.as_deref().unwrap_or(""))?;
                match edited {
                    Some(body) => request.body = Some(body),
                    None => {
                        return Err(CliError::message(
                            "no changes specified; pass flags or set $EDITOR",
                        ));
                    }
                }
            }
            let task = queue.edit(id, request)?;
            emit(json, &task, || println!("updated #{}", task.id));
            Ok(())
        }
        Commands::Ready { id } => {
            let outcome = queue.mark_ready(ReadyRequest {
                task_id: id,
                actor: human_actor(),
            })?;
            for warning in &outcome.warnings {
                eprintln!("warning: {warning}");
            }
            emit(json, &outcome, || println!("task {id} is ready"));
            Ok(())
        }
        Commands::Block {
            id,
            reason,
            claim_token,
        } => {
            let task = queue.block(BlockRequest {
                task_id: id,
                claim_token,
                reason,
                actor: human_actor(),
            })?;
            emit(json, &task, || println!("blocked #{}", task.id));
            Ok(())
        }
        Commands::Cancel { id, reason } => {
            let task = queue.cancel(CancelRequest {
                task_id: id,
                reason,
                actor: human_actor(),
            })?;
            emit(json, &task, || println!("cancelled #{}", task.id));
            Ok(())
        }
        Commands::Delete { id, reason, force } => {
            let outcome = queue.delete(DeleteRequest {
                task_id: id,
                reason,
                force,
                actor: human_actor(),
            })?;
            emit(json, &outcome, || {
                println!(
                    "deleted #{} [{}] {}",
                    outcome.task_id, outcome.status, outcome.title
                );
                if outcome.active_claim_cleared {
                    println!("cleared active claim");
                }
                println!(
                    "removed claims={} events={} artifacts={} dependencies={}",
                    outcome.claims_removed,
                    outcome.events_removed,
                    outcome.artifacts_removed,
                    outcome.dependencies_removed
                );
                println!("reason: {}", outcome.reason);
            });
            Ok(())
        }
        Commands::Claim {
            agent,
            capability,
            kind,
            max_risk,
            lease_minutes,
            agent_pool,
        } => {
            let mut request = ClaimRequest::new(agent);
            request.capabilities = split_caps(capability);
            for kind in kind {
                request.allowed_kinds.push(TaskKind::parse(&kind)?);
            }
            request.maximum_risk = RiskLevel::parse(&max_risk)?;
            if let Some(minutes) = lease_minutes {
                request.lease = lease_from_minutes(minutes)?;
            }
            request.agent_pool = agent_pool;
            if let Some(repo) = repo.clone() {
                request.allowed_repos.push(repo);
            }
            if let Some(project) = project.clone() {
                request.allowed_projects.push(project);
            }
            let outcome = queue.claim_next(request)?;
            emit(json, &outcome, || {
                if outcome.found {
                    let task = outcome.task.as_ref().unwrap();
                    let claim = outcome.claim.as_ref().unwrap();
                    println!("claimed #{} {}", task.task.id, task.task.title);
                    println!("token: {}", claim.token);
                    println!("lease_expires_at: {}", claim.lease_expires_at);
                } else {
                    println!("no eligible ready tasks");
                }
            });
            Ok(())
        }
        Commands::Heartbeat {
            id,
            claim_token,
            lease_minutes,
        } => {
            let lease = match lease_minutes {
                Some(minutes) => Some(lease_from_minutes(minutes)?),
                None => None,
            };
            let claim = queue.heartbeat(HeartbeatRequest {
                task_id: id,
                claim_token,
                lease,
                actor: human_actor(),
            })?;
            emit(json, &claim, || {
                println!(
                    "heartbeat #{} until {}",
                    claim.task_id, claim.lease_expires_at
                );
            });
            Ok(())
        }
        Commands::Start {
            id,
            claim_token,
            branch,
            worktree,
        } => {
            let detail = queue.start(StartRequest {
                task_id: id,
                claim_token,
                branch,
                worktree_path: worktree,
                actor: human_actor(),
            })?;
            emit(json, &detail, || println!("started #{}", detail.task.id));
            Ok(())
        }
        Commands::Complete {
            id,
            claim_token,
            summary,
            status,
            artifact,
        } => {
            let target = match status {
                Some(status) => Some(TaskStatus::parse(&status)?),
                None => None,
            };
            let mut artifacts = Vec::new();
            for item in artifact {
                artifacts.push(parse_artifact(&item)?);
            }
            let detail = queue.complete(CompleteRequest {
                task_id: id,
                claim_token,
                summary,
                target,
                artifacts,
                actor: human_actor(),
            })?;
            emit(json, &detail, || {
                println!("#{} is now {}", detail.task.id, detail.task.status);
            });
            Ok(())
        }
        Commands::Release {
            id,
            claim_token,
            reason,
        } => {
            let task = queue.release(ReleaseRequest {
                task_id: id,
                claim_token,
                reason,
                actor: human_actor(),
            })?;
            emit(json, &task, || {
                println!("released #{} back to ready", task.id)
            });
            Ok(())
        }
        Commands::Status => {
            let status = queue.status()?;
            let body = serde_json::json!({
                "db": db,
                "counts": status.counts,
                "active_claims": status.active_claims,
                "expired_claims": status.expired_claims,
            });
            emit(json, &body, || {
                println!("database: {}", db.display());
                println!("inbox: {}", status.counts.inbox);
                println!("ready: {}", status.counts.ready);
                println!("claimed: {}", status.counts.claimed);
                println!("in_progress: {}", status.counts.in_progress);
                println!("review: {}", status.counts.review);
                println!("blocked: {}", status.counts.blocked);
                println!("done: {}", status.counts.done);
                println!("cancelled: {}", status.counts.cancelled);
                println!("active claims: {}", status.active_claims);
                println!("expired claims: {}", status.expired_claims);
            });
            Ok(())
        }
        Commands::RecoverStale { reason, to } => {
            let to = match to {
                Some(value) => Some(StaleDisposition::parse(&value)?),
                None => None,
            };
            let recovered = queue.recover_stale(RecoverRequest {
                reason,
                to,
                actor: human_actor(),
            })?;
            emit(json, &serde_json::json!({"recovered": recovered}), || {
                if recovered.is_empty() {
                    println!("no expired claims");
                } else {
                    for record in &recovered {
                        println!(
                            "recovered #{} {} -> {} ({})",
                            record.task_id,
                            record.previous_status,
                            record.new_status,
                            record.reason
                        );
                    }
                }
            });
            Ok(())
        }
        Commands::Events { id } => {
            let events = queue.events(id)?;
            emit(json, &serde_json::json!({"events": events}), || {
                for event in &events {
                    println!(
                        "#{} {} {} {}",
                        event.id, event.event_type, event.actor_type, event.created_at
                    );
                }
            });
            Ok(())
        }
        Commands::Reopen { id } => {
            let task = queue.reopen(id, human_actor())?;
            emit(json, &task, || {
                println!("reopened #{} as {}", task.id, task.status);
            });
            Ok(())
        }
        Commands::Project { .. } | Commands::Mcp | Commands::Skill { .. } => {
            unreachable!("handled before queue open")
        }
    }
}

fn init_project(
    directory: Option<&Path>,
    repo: Option<String>,
    project: Option<String>,
    yes: bool,
    force: bool,
) -> Result<(), CliError> {
    let context = resolve_context(directory, repo, project)?;
    let root = context.git_root.clone().ok_or_else(|| {
        CliError::message("project init requires a git repository; run it inside a worktree")
    })?;
    let path = root.join(".agentqueue.toml");
    if path.exists() && !force {
        return Err(CliError::message(format!(
            "{} already exists; pass --force to overwrite",
            path.display()
        )));
    }
    if !yes && !confirm(&path)? {
        return Err(CliError::message(
            "refusing to write .agentqueue.toml without confirmation; pass --yes",
        ));
    }
    fs::write(&path, render_init_config(&context))?;
    println!("wrote {}", path.display());
    Ok(())
}

fn confirm(path: &Path) -> Result<bool, CliError> {
    if !io::stdin().is_terminal() {
        return Ok(false);
    }
    eprint!("Create {}? [y/N] ", path.display());
    io::stderr().flush()?;
    let mut line = String::new();
    io::stdin().read_line(&mut line)?;
    Ok(matches!(line.trim(), "y" | "Y" | "yes" | "YES"))
}

fn resolve_context(
    directory: Option<&Path>,
    repo: Option<String>,
    project: Option<String>,
) -> Result<ProjectContext, CliError> {
    discover(DiscoverOptions {
        directory: base_dir(directory)?,
        explicit_repo: repo,
        explicit_project: project,
        global_map: None,
        use_default_map: true,
    })
    .map_err(CliError::from)
}

fn base_dir(directory: Option<&Path>) -> Result<PathBuf, CliError> {
    if let Some(directory) = directory {
        Ok(directory.to_path_buf())
    } else {
        std::env::current_dir()
            .map_err(|err| CliError::message(format!("current directory: {err}")))
    }
}

fn db_path(cli: &cli::Cli) -> PathBuf {
    cli.db.clone().unwrap_or_else(default_db_path)
}

fn human_actor() -> Actor {
    let id = std::env::var("USER")
        .ok()
        .or_else(|| std::env::var("USERNAME").ok());
    Actor::human(id)
}

fn read_body(body: Option<String>, body_file: Option<&Path>) -> Result<Option<String>, CliError> {
    if body.is_some() && body_file.is_some() {
        return Err(CliError::message("pass only one of --body and --body-file"));
    }
    if let Some(path) = body_file {
        let text = fs::read_to_string(path)
            .map_err(|err| CliError::message(format!("read {}: {err}", path.display())))?;
        return Ok(Some(text));
    }
    Ok(body)
}

fn split_caps(values: Vec<String>) -> Vec<String> {
    values
        .into_iter()
        .flat_map(|value| {
            value
                .split(',')
                .map(|part| part.trim().to_string())
                .filter(|part| !part.is_empty())
                .collect::<Vec<_>>()
        })
        .collect()
}

fn parse_ids(value: Option<&str>) -> Result<Vec<i64>, CliError> {
    let Some(value) = value else {
        return Ok(Vec::new());
    };
    let mut ids = Vec::new();
    for part in value.split(',') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        ids.push(
            part.parse::<i64>()
                .map_err(|_| CliError::message(format!("invalid task id '{part}'")))?,
        );
    }
    Ok(ids)
}

fn parse_artifact(value: &str) -> Result<ArtifactInput, CliError> {
    let (kind, artifact_value) = value
        .split_once('=')
        .ok_or_else(|| CliError::message("artifact must be kind=value"))?;
    let kind = kind.trim();
    let artifact_value = artifact_value.trim();
    if kind.is_empty() || artifact_value.is_empty() {
        return Err(CliError::message("artifact kind and value are required"));
    }
    Ok(ArtifactInput {
        kind: kind.to_string(),
        value: artifact_value.to_string(),
    })
}

fn edit_in_editor(current: &str) -> Result<Option<String>, CliError> {
    let editor = std::env::var("VISUAL")
        .or_else(|_| std::env::var("EDITOR"))
        .ok();
    let Some(editor) = editor else {
        return Ok(None);
    };
    let path = std::env::temp_dir().join(format!("q-edit-{}.md", std::process::id()));
    fs::write(&path, current)?;
    let status = Command::new(&editor)
        .arg(&path)
        .status()
        .map_err(|err| CliError::message(format!("failed to launch {editor}: {err}")))?;
    if !status.success() {
        let _ = fs::remove_file(&path);
        return Err(CliError::message(format!("editor exited with {status}")));
    }
    let body = fs::read_to_string(&path)?;
    let _ = fs::remove_file(&path);
    Ok(Some(body))
}

fn emit(json: bool, value: &impl serde::Serialize, human: impl FnOnce()) {
    if json {
        serde_json::to_writer_pretty(io::stdout(), value).expect("write json");
        println!();
    } else {
        human();
    }
}

fn print_context(context: &ProjectContext) {
    println!("source: {:?}", context.source);
    println!("project: {}", context.project.as_deref().unwrap_or("-"));
    println!("repo: {}", context.repo.as_deref().unwrap_or("-"));
    println!("capture_path: {}", context.capture_path.display());
    println!(
        "repo_relative_path: {}",
        context.repo_relative_path.as_deref().unwrap_or("-")
    );
    println!(
        "git_root: {}",
        context
            .git_root
            .as_ref()
            .map(|path| path.display().to_string())
            .unwrap_or_else(|| "-".into())
    );
    println!("git_head: {}", context.git_head.as_deref().unwrap_or("-"));
    println!(
        "agent_pool: {}",
        context.agent_pool.as_deref().unwrap_or("-")
    );
}

const TITLE_MAX_CHARS: usize = 64;
const UNASSIGNED_PROJECT: &str = "(none)";

struct TaskListRow {
    id: String,
    status: String,
    project: String,
    priority: String,
    updated: String,
    title: String,
}

fn print_task_list(tasks: &[TaskSummary]) {
    if tasks.is_empty() {
        println!("no tasks");
        return;
    }
    println!("{}", render_task_table(tasks));
}

fn render_task_table(tasks: &[TaskSummary]) -> String {
    let rows: Vec<TaskListRow> = tasks.iter().map(task_list_row).collect();
    render_task_rows(&rows)
}

fn task_list_row(task: &TaskSummary) -> TaskListRow {
    TaskListRow {
        id: task.id.to_string(),
        status: task.status.to_string(),
        project: display_project(task.project.as_deref()),
        priority: task.priority.to_string(),
        updated: format_timestamp(task.updated_at),
        title: format_list_title(&task.title),
    }
}

fn display_project(project: Option<&str>) -> String {
    match project.map(str::trim).filter(|text| !text.is_empty()) {
        Some(name) => name.to_string(),
        None => UNASSIGNED_PROJECT.to_string(),
    }
}

fn format_list_title(title: &str) -> String {
    let flat = title.split_whitespace().collect::<Vec<_>>().join(" ");
    truncate_chars(&flat, TITLE_MAX_CHARS)
}

fn truncate_chars(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let keep = max_chars.saturating_sub(1);
    let mut out: String = text.chars().take(keep).collect();
    out.push('…');
    out
}

fn render_task_rows(rows: &[TaskListRow]) -> String {
    let id_w = column_width("ID", rows.iter().map(|row| row.id.as_str()));
    let status_w = column_width("STATUS", rows.iter().map(|row| row.status.as_str()));
    let project_w = column_width("PROJECT", rows.iter().map(|row| row.project.as_str()));
    let priority_w = column_width("PRI", rows.iter().map(|row| row.priority.as_str()));
    let updated_w = column_width("UPDATED", rows.iter().map(|row| row.updated.as_str()));
    let title_w = column_width("TITLE", rows.iter().map(|row| row.title.as_str()));
    let widths = [id_w, status_w, project_w, priority_w, updated_w, title_w];
    let align_right = [true, false, false, true, false, false];
    let mut lines = Vec::with_capacity(rows.len() + 1);
    lines.push(format_task_line(
        &["ID", "STATUS", "PROJECT", "PRI", "UPDATED", "TITLE"],
        widths,
        align_right,
    ));
    for row in rows {
        lines.push(format_task_line(
            &[
                row.id.as_str(),
                row.status.as_str(),
                row.project.as_str(),
                row.priority.as_str(),
                row.updated.as_str(),
                row.title.as_str(),
            ],
            widths,
            align_right,
        ));
    }
    lines.join("\n")
}

fn column_width<'a>(header: &str, values: impl Iterator<Item = &'a str>) -> usize {
    values
        .map(|value| value.chars().count())
        .chain(std::iter::once(header.chars().count()))
        .max()
        .unwrap_or(0)
}

fn format_task_line(cells: &[&str], widths: [usize; 6], align_right: [bool; 6]) -> String {
    let mut line = String::new();
    for (index, cell) in cells.iter().enumerate() {
        if index > 0 {
            line.push_str("  ");
        }
        let width = widths[index];
        if align_right[index] {
            line.push_str(&format!("{cell:>width$}"));
        } else {
            line.push_str(&format!("{cell:<width$}"));
        }
    }
    line
}

fn print_detail(detail: &q_core::TaskDetail) {
    let task = &detail.task;
    println!("#{} {} [{}]", task.id, task.title, task.status);
    println!(
        "kind: {}  risk: {}  priority: {}",
        task.kind, task.risk, task.priority
    );
    println!("project: {}", task.project.as_deref().unwrap_or("-"));
    println!("repo: {}", task.repo.as_deref().unwrap_or("-"));
    println!("capture_path: {}", task.capture_path);
    if let Some(relative) = &task.repo_relative_path {
        println!("repo_relative_path: {relative}");
    }
    println!("original_capture: {}", task.original_capture);
    if let Some(body) = &task.body {
        println!("\n{body}");
    }
    if !detail.acceptance_criteria.is_empty() {
        println!("\nacceptance criteria:");
        for item in &detail.acceptance_criteria {
            println!("- {item}");
        }
    }
    if let Some(claim) = &detail.claim {
        println!(
            "\nclaim: agent={} active={} token={}",
            claim.agent_id, claim.active, claim.token
        );
        println!("lease_expires_at: {}", claim.lease_expires_at);
        if let Some(branch) = &claim.branch {
            println!("branch: {branch}");
        }
    }
    if !detail.artifacts.is_empty() {
        println!("\nartifacts:");
        for artifact in &detail.artifacts {
            println!("- {}: {}", artifact.kind, artifact.value);
        }
    }
}

#[derive(Debug)]
struct CliError {
    message: String,
}

impl CliError {
    fn message(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl std::fmt::Display for CliError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl From<QueueError> for CliError {
    fn from(error: QueueError) -> Self {
        Self {
            message: error.to_string(),
        }
    }
}

impl From<std::io::Error> for CliError {
    fn from(error: std::io::Error) -> Self {
        Self {
            message: error.to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        display_project, format_list_title, render_task_rows, truncate_chars, TaskListRow,
        TITLE_MAX_CHARS,
    };

    #[test]
    fn unassigned_project_uses_a_stable_label() {
        assert_eq!(display_project(None), "(none)");
        assert_eq!(display_project(Some("   ")), "(none)");
        assert_eq!(display_project(Some(" alpha ")), "alpha");
    }

    #[test]
    fn titles_collapse_whitespace_and_truncate_with_ellipsis() {
        assert_eq!(format_list_title("  a\nb\t c  "), "a b c");
        let long = "x".repeat(TITLE_MAX_CHARS + 20);
        let shown = format_list_title(&long);
        assert_eq!(shown.chars().count(), TITLE_MAX_CHARS);
        assert!(shown.ends_with('…'));
        assert!(!shown.contains(&long));
        assert_eq!(truncate_chars("short", 64), "short");
    }

    #[test]
    fn task_table_aligns_columns() {
        let long = "y".repeat(TITLE_MAX_CHARS + 8);
        let rows = vec![
            TaskListRow {
                id: "12".into(),
                status: "inbox".into(),
                project: "alpha".into(),
                priority: "0".into(),
                updated: "2026-09-22T20:00:00Z".into(),
                title: "Short".into(),
            },
            TaskListRow {
                id: "3".into(),
                status: "in_progress".into(),
                project: "(none)".into(),
                priority: "10".into(),
                updated: "2026-09-22T19:00:00Z".into(),
                title: format_list_title(&long),
            },
        ];
        let table = render_task_rows(&rows);
        let lines: Vec<&str> = table.lines().collect();
        assert_eq!(lines.len(), 3);
        let width = lines[0].chars().count();
        assert!(lines.iter().all(|line| line.chars().count() == width));
        let project_at = char_index(lines[0], "PROJECT");
        assert_eq!(char_index(lines[1], "alpha"), project_at);
        assert_eq!(char_index(lines[2], "(none)"), project_at);
        let updated_at = char_index(lines[0], "UPDATED");
        assert_eq!(char_index(lines[1], "2026-09-22T20:00:00Z"), updated_at);
        assert_eq!(char_index(lines[2], "2026-09-22T19:00:00Z"), updated_at);
        assert!(lines[2].contains('…'));
        assert!(!table.contains(&long));
        assert!(lines[0].find("ID").unwrap() < lines[0].find("STATUS").unwrap());
        assert!(lines[0].find("STATUS").unwrap() < lines[0].find("PROJECT").unwrap());
        assert!(lines[0].find("PROJECT").unwrap() < lines[0].find("PRI").unwrap());
        assert!(lines[0].find("PRI").unwrap() < lines[0].find("UPDATED").unwrap());
        assert!(lines[0].find("UPDATED").unwrap() < lines[0].find("TITLE").unwrap());
        assert!(lines[1].contains("  0  "));
        assert!(lines[2].contains(" 10  "));
    }

    #[test]
    fn task_table_matches_the_readme_shape() {
        let rows = vec![
            TaskListRow {
                id: "4".into(),
                status: "inbox".into(),
                project: "alpha".into(),
                priority: "0".into(),
                updated: "2026-09-22T20:04:00Z".into(),
                title: "Keep the inbox item".into(),
            },
            TaskListRow {
                id: "2".into(),
                status: "ready".into(),
                project: "beta".into(),
                priority: "1".into(),
                updated: "2026-09-22T20:02:00Z".into(),
                title: "Compare encodings".into(),
            },
            TaskListRow {
                id: "1".into(),
                status: "inbox".into(),
                project: "(none)".into(),
                priority: "0".into(),
                updated: "2026-09-22T20:01:00Z".into(),
                title: "Unassigned capture".into(),
            },
        ];
        let table = render_task_rows(&rows);
        let shown = table
            .lines()
            .map(str::trim_end)
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(
            shown,
            "\
ID  STATUS  PROJECT  PRI  UPDATED               TITLE
 4  inbox   alpha      0  2026-09-22T20:04:00Z  Keep the inbox item
 2  ready   beta       1  2026-09-22T20:02:00Z  Compare encodings
 1  inbox   (none)     0  2026-09-22T20:01:00Z  Unassigned capture"
        );
    }

    fn char_index(line: &str, needle: &str) -> usize {
        let byte = line
            .find(needle)
            .unwrap_or_else(|| panic!("missing {needle} in {line}"));
        line[..byte].chars().count()
    }
}
