mod cli;
mod skill;
mod style;

use std::fs;
use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use anstyle::Style;
use clap::Parser;
use q_core::{
    format_timestamp, lease_from_minutes, Actor, ArtifactInput, BlockRequest, CancelRequest,
    CaptureRequest, ClaimRequest, CompleteRequest, CreateFeatureRequest, DeleteRequest,
    EditFeatureRequest, EditRequest, HeartbeatRequest, ListFilter, QueueError, QueueService,
    ReadyRequest, RecoverRequest, ReleaseRequest, RiskLevel, StaleDisposition, StartRequest,
    TaskKind, TaskStatus, TaskSummary, TaskTree, TreeNode, TreeQuery,
};
use q_project::{discover, render_init_config, DiscoverOptions, ProjectContext};
use q_store::{default_db_path, Queue};
use time::OffsetDateTime;

use crate::cli::{Commands, FeatureCommand, ProjectCommand};
use crate::style::Paint;

#[tokio::main(flavor = "current_thread")]
async fn main() {
    init_tracing();
    let raw: Vec<String> = std::env::args().skip(1).collect();
    let args = cli::preprocess(raw);
    let cli = match cli::Cli::try_parse_from(std::iter::once(String::from("q")).chain(args)) {
        Ok(cli) => cli,
        Err(error) => error.exit(),
    };
    let err_paint = style::paint_for(cli.color, style::Stream::Stderr);
    if let Err(error) = run(cli).await {
        eprintln!("{} {error}", err_paint.error_label());
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

struct Ui {
    json: bool,
    out: Paint,
    err: Paint,
}

async fn run(cli: cli::Cli) -> Result<(), CliError> {
    let ui = Ui {
        json: cli.json,
        out: style::paint_for(cli.color, style::Stream::Stdout),
        err: style::paint_for(cli.color, style::Stream::Stderr),
    };
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
            None => skill::print_skill(ui.json).map_err(CliError::message),
            Some(cli::SkillCommand::Install { target, force }) => {
                let home = skill::home_dir().map_err(CliError::message)?;
                skill::install_skill(&home, &target, force, ui.json).map_err(CliError::message)
            }
        },
        Commands::Project { command } => match command {
            ProjectCommand::Show => {
                let context = resolve_context(directory.as_deref(), repo, project)?;
                emit(&ui, &context, || print_context(&context, ui.out));
                Ok(())
            }
            ProjectCommand::Init { yes, force } => {
                init_project(directory.as_deref(), repo, project, yes, force, ui.out)
            }
        },
        other => {
            tracing::info!(db = %db.display(), "opening queue");
            let queue = Queue::open(&db)?;
            dispatch(&queue, &db, repo, project, directory.as_deref(), other, &ui)
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
    ui: &Ui,
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
            feature,
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
                feature,
                policy: context.policy,
                actor: human_actor(),
                context_source: serde_json::to_value(context.source)
                    .ok()
                    .and_then(|value| value.as_str().map(str::to_string)),
            })?;
            let id = task.id;
            emit(ui, &task, || {
                confirm(ui, "captured", id, task.status.as_str(), &task.title);
            });
            Ok(())
        }
        Commands::Ls {
            status,
            kind,
            limit,
            all,
            feature,
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
                feature,
                limit,
                include_terminal: all,
            })?;
            emit(ui, &serde_json::json!({"tasks": tasks}), || {
                print_task_list(&tasks, ui.out);
            });
            Ok(())
        }
        Commands::Show { id } => {
            let detail = queue.get(id)?;
            emit(ui, &detail, || print_detail(&detail, ui.out));
            Ok(())
        }
        Commands::Tree { id, feature } => {
            let tree = queue.tree(TreeQuery {
                task_id: id,
                feature,
            })?;
            emit(ui, &tree, || print_tree(&tree, ui.out));
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
            feature,
            clear_feature,
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
            request.feature = feature;
            request.clear_feature = clear_feature;
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
            emit(ui, &task, || {
                confirm(ui, "updated", task.id, task.status.as_str(), &task.title);
            });
            Ok(())
        }
        Commands::Ready { id } => {
            let outcome = queue.mark_ready(ReadyRequest {
                task_id: id,
                actor: human_actor(),
            })?;
            for warning in &outcome.warnings {
                eprintln!("{} {warning}", ui.err.warning_label());
            }
            let task = &outcome.task;
            emit(ui, &outcome, || {
                confirm(ui, "ready", task.id, task.status.as_str(), &task.title);
            });
            Ok(())
        }
        Commands::Block { id, claim_token } => {
            let task = queue.block(BlockRequest {
                task_id: id,
                claim_token,
                actor: human_actor(),
            })?;
            emit(ui, &task, || {
                confirm(ui, "blocked", task.id, task.status.as_str(), &task.title);
            });
            Ok(())
        }
        Commands::Cancel { id } => {
            let task = queue.cancel(CancelRequest {
                task_id: id,
                actor: human_actor(),
            })?;
            emit(ui, &task, || {
                confirm(ui, "cancelled", task.id, task.status.as_str(), &task.title);
            });
            Ok(())
        }
        Commands::Delete { id, force } => {
            let outcome = queue.delete(DeleteRequest {
                task_id: id,
                force,
                actor: human_actor(),
            })?;
            emit(ui, &outcome, || {
                confirm(
                    ui,
                    "deleted",
                    outcome.task_id,
                    outcome.status.as_str(),
                    &outcome.title,
                );
                if outcome.active_claim_cleared {
                    println!("cleared active claim");
                }
                println!(
                    "{}",
                    ui.out.dim(&format!(
                        "removed claims={} events={} artifacts={} dependencies={}",
                        outcome.claims_removed,
                        outcome.events_removed,
                        outcome.artifacts_removed,
                        outcome.dependencies_removed
                    ))
                );
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
            emit(ui, &outcome, || {
                if outcome.found {
                    let task = outcome.task.as_ref().unwrap();
                    let claim = outcome.claim.as_ref().unwrap();
                    confirm(
                        ui,
                        "claimed",
                        task.task.id,
                        task.task.status.as_str(),
                        &task.task.title,
                    );
                    println!("token: {}", claim.token);
                    println!(
                        "lease_expires_at: {}",
                        ui.out.dim(&format_timestamp(claim.lease_expires_at))
                    );
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
            emit(ui, &claim, || {
                println!(
                    "heartbeat {} until {}",
                    ui.out.dim(&format!("#{}", claim.task_id)),
                    ui.out.dim(&format_timestamp(claim.lease_expires_at))
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
            emit(ui, &detail, || {
                confirm(
                    ui,
                    "started",
                    detail.task.id,
                    detail.task.status.as_str(),
                    &detail.task.title,
                );
            });
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
            emit(ui, &detail, || {
                confirm(
                    ui,
                    "completed",
                    detail.task.id,
                    detail.task.status.as_str(),
                    &detail.task.title,
                );
            });
            Ok(())
        }
        Commands::Release { id, claim_token } => {
            let task = queue.release(ReleaseRequest {
                task_id: id,
                claim_token,
                actor: human_actor(),
            })?;
            emit(ui, &task, || {
                confirm(ui, "released", task.id, task.status.as_str(), &task.title);
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
            emit(ui, &body, || print_status(db, &status, ui.out));
            Ok(())
        }
        Commands::RecoverStale { to } => {
            let to = match to {
                Some(value) => Some(StaleDisposition::parse(&value)?),
                None => None,
            };
            let recovered = queue.recover_stale(RecoverRequest {
                to,
                actor: human_actor(),
            })?;
            emit(ui, &serde_json::json!({"recovered": recovered}), || {
                if recovered.is_empty() {
                    println!("no expired claims");
                } else {
                    for record in &recovered {
                        println!(
                            "recovered {} {} -> {}",
                            ui.out.dim(&format!("#{}", record.task_id)),
                            ui.out.status(record.previous_status.as_str()),
                            ui.out.status(record.new_status.as_str()),
                        );
                    }
                }
            });
            Ok(())
        }
        Commands::Events { id } => {
            let events = queue.events(id)?;
            emit(ui, &serde_json::json!({"events": events}), || {
                print_events(&events, ui.out, "");
            });
            Ok(())
        }
        Commands::Reopen { id } => {
            let task = queue.reopen(id, human_actor())?;
            emit(ui, &task, || {
                confirm(ui, "reopened", task.id, task.status.as_str(), &task.title);
            });
            Ok(())
        }
        Commands::Feature { command } => dispatch_feature(queue, command, ui),
        Commands::Project { .. } | Commands::Mcp | Commands::Skill { .. } => {
            unreachable!("handled before queue open")
        }
    }
}

fn dispatch_feature(queue: &Queue, command: FeatureCommand, ui: &Ui) -> Result<(), CliError> {
    match command {
        FeatureCommand::Create { title, body } => {
            let feature = queue.create_feature(CreateFeatureRequest {
                title: title.join(" "),
                body,
            })?;
            let id = feature.id;
            emit(ui, &feature, || {
                println!(
                    "created feature {} {}",
                    ui.out.dim(&format!("#{id}")),
                    ui.out.bold(&feature.title),
                );
            });
            Ok(())
        }
        FeatureCommand::Ls => {
            let features = queue.list_features()?;
            emit(ui, &serde_json::json!({"features": features}), || {
                print_feature_list(&features, ui.out);
            });
            Ok(())
        }
        FeatureCommand::Show { id } => {
            let feature = queue.get_feature(id)?;
            emit(ui, &feature, || print_feature(&feature, ui.out));
            Ok(())
        }
        FeatureCommand::Edit { id, title, body } => {
            if title.is_none() && body.is_none() {
                return Err(CliError::message(
                    "no changes specified; pass --title or --body",
                ));
            }
            let feature = queue.edit_feature(id, EditFeatureRequest { title, body })?;
            emit(ui, &feature, || {
                println!(
                    "updated feature {} {}",
                    ui.out.dim(&format!("#{}", feature.id)),
                    ui.out.bold(&feature.title),
                );
            });
            Ok(())
        }
        FeatureCommand::Delete { id } => {
            let outcome = queue.delete_feature(id)?;
            emit(ui, &outcome, || {
                println!(
                    "deleted feature {} {} ({} tasks detached)",
                    ui.out.dim(&format!("#{}", outcome.id)),
                    ui.out.bold(&outcome.title),
                    outcome.tasks_detached
                );
            });
            Ok(())
        }
    }
}

fn init_project(
    directory: Option<&Path>,
    repo: Option<String>,
    project: Option<String>,
    yes: bool,
    force: bool,
    paint: Paint,
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
    if !yes && !confirm_write(&path)? {
        return Err(CliError::message(
            "refusing to write .agentqueue.toml without confirmation; pass --yes",
        ));
    }
    fs::write(&path, render_init_config(&context))?;
    println!("wrote {}", paint.dim(&path.display().to_string()));
    Ok(())
}

fn confirm_write(path: &Path) -> Result<bool, CliError> {
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
        ids.push(part.parse::<i64>().map_err(|_| {
            CliError::message(format!("invalid task id '{part}'; expected an integer"))
        })?);
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

fn emit(ui: &Ui, value: &impl serde::Serialize, human: impl FnOnce()) {
    if ui.json {
        serde_json::to_writer_pretty(io::stdout(), value).expect("write json");
        println!();
    } else {
        human();
    }
}

fn confirm(ui: &Ui, verb: &str, id: i64, status: &str, title: &str) {
    println!(
        "{verb} {} [{}] {}",
        ui.out.dim(&format!("#{id}")),
        ui.out.status(status),
        ui.out.bold(title),
    );
}

fn meta(paint: Paint, line: &str) {
    println!("{}", paint.dim(line));
}

fn print_context(context: &ProjectContext, paint: Paint) {
    meta(paint, &format!("source: {}", source_name(context.source)));
    meta(
        paint,
        &format!("project: {}", context.project.as_deref().unwrap_or("-")),
    );
    meta(
        paint,
        &format!("repo: {}", context.repo.as_deref().unwrap_or("-")),
    );
    meta(
        paint,
        &format!("capture_path: {}", context.capture_path.display()),
    );
    meta(
        paint,
        &format!(
            "repo_relative_path: {}",
            context.repo_relative_path.as_deref().unwrap_or("-")
        ),
    );
    meta(
        paint,
        &format!(
            "git_root: {}",
            context
                .git_root
                .as_ref()
                .map(|path| path.display().to_string())
                .unwrap_or_else(|| "-".into())
        ),
    );
    meta(
        paint,
        &format!("git_head: {}", context.git_head.as_deref().unwrap_or("-")),
    );
    meta(
        paint,
        &format!(
            "agent_pool: {}",
            context.agent_pool.as_deref().unwrap_or("-")
        ),
    );
}

fn source_name(source: q_project::ContextSource) -> &'static str {
    match source {
        q_project::ContextSource::Explicit => "explicit",
        q_project::ContextSource::PathRule => "path_rule",
        q_project::ContextSource::ConfigFile => "config_file",
        q_project::ContextSource::Git => "git",
        q_project::ContextSource::GlobalMapping => "global_mapping",
        q_project::ContextSource::Unassigned => "unassigned",
    }
}

fn print_status(db: &Path, status: &q_core::QueueStatus, paint: Paint) {
    println!("database: {}", paint.dim(&db.display().to_string()));
    for (label, count) in [
        ("inbox", status.counts.inbox),
        ("ready", status.counts.ready),
        ("claimed", status.counts.claimed),
        ("in_progress", status.counts.in_progress),
        ("review", status.counts.review),
        ("blocked", status.counts.blocked),
        ("done", status.counts.done),
        ("cancelled", status.counts.cancelled),
    ] {
        println!("{}: {count}", paint.status(label));
    }
    println!("active claims: {}", status.active_claims);
    if status.expired_claims > 0 {
        println!(
            "{}: {}",
            paint.paint(
                Style::new().fg_color(Some(anstyle::AnsiColor::Red.into())),
                "expired claims"
            ),
            status.expired_claims
        );
    } else {
        println!("expired claims: {}", status.expired_claims);
    }
}

fn print_events(events: &[q_core::Event], paint: Paint, indent: &str) {
    for event in events {
        println!(
            "{indent}{} {} {} {}",
            paint.dim(&format!("#{}", event.id)),
            event.event_type,
            paint.dim(&event.actor_type),
            paint.dim(&format_timestamp(event.created_at)),
        );
    }
}

const TITLE_MAX_CHARS: usize = 64;
const UNASSIGNED_PROJECT: &str = "(none)";

struct TaskListRow {
    id: String,
    status: String,
    feature: String,
    project: String,
    priority: String,
    updated: String,
    title: String,
}

fn print_task_list(tasks: &[TaskSummary], paint: Paint) {
    if tasks.is_empty() {
        println!("no tasks");
        return;
    }
    println!("{}", render_task_table(tasks, paint));
}

fn render_task_table(tasks: &[TaskSummary], paint: Paint) -> String {
    let now = OffsetDateTime::now_utc();
    let rows: Vec<TaskListRow> = tasks.iter().map(|task| task_list_row(task, now)).collect();
    render_task_rows_painted(&rows, paint)
}

fn task_list_row(task: &TaskSummary, now: OffsetDateTime) -> TaskListRow {
    TaskListRow {
        id: task.id.to_string(),
        status: task.status.to_string(),
        feature: display_project(task.feature.as_deref()),
        project: display_project(task.project.as_deref()),
        priority: task.priority.to_string(),
        updated: style::format_relative(task.updated_at, now),
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

#[cfg(test)]
fn render_task_rows(rows: &[TaskListRow]) -> String {
    render_task_rows_painted(rows, Paint::plain())
}

fn render_task_rows_painted(rows: &[TaskListRow], paint: Paint) -> String {
    let headers = [
        "ID", "STATUS", "FEATURE", "PROJECT", "PRI", "UPDATED", "TITLE",
    ];
    let align_right = [true, false, false, false, true, false, false];
    let widths = [
        column_width("ID", rows.iter().map(|row| row.id.as_str())),
        column_width("STATUS", rows.iter().map(|row| row.status.as_str())),
        column_width("FEATURE", rows.iter().map(|row| row.feature.as_str())),
        column_width("PROJECT", rows.iter().map(|row| row.project.as_str())),
        column_width("PRI", rows.iter().map(|row| row.priority.as_str())),
        column_width("UPDATED", rows.iter().map(|row| row.updated.as_str())),
        column_width("TITLE", rows.iter().map(|row| row.title.as_str())),
    ];
    let header_styles = [style::dim_style(); 7];
    let mut lines = Vec::with_capacity(rows.len() + 1);
    lines.push(format_task_line(
        &headers,
        &header_styles,
        &widths,
        &align_right,
        paint,
    ));
    for row in rows {
        let styles = [
            style::dim_style(),
            style::status_style(&row.status),
            style::dim_style(),
            style::dim_style(),
            style::dim_style(),
            style::dim_style(),
            style::bold_style(),
        ];
        lines.push(format_task_line(
            &[
                row.id.as_str(),
                row.status.as_str(),
                row.feature.as_str(),
                row.project.as_str(),
                row.priority.as_str(),
                row.updated.as_str(),
                row.title.as_str(),
            ],
            &styles,
            &widths,
            &align_right,
            paint,
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

fn format_task_line(
    cells: &[&str],
    styles: &[Style],
    widths: &[usize],
    align_right: &[bool],
    paint: Paint,
) -> String {
    let mut line = String::new();
    for (index, cell) in cells.iter().enumerate() {
        if index > 0 {
            line.push_str("  ");
        }
        let width = widths[index];
        let pad = width.saturating_sub(cell.chars().count());
        let painted = paint.paint(styles[index], cell);
        if align_right[index] {
            line.push_str(&" ".repeat(pad));
            line.push_str(&painted);
        } else {
            line.push_str(&painted);
            line.push_str(&" ".repeat(pad));
        }
    }
    line
}

fn print_feature_list(features: &[q_core::Feature], paint: Paint) {
    if features.is_empty() {
        println!("no features");
        return;
    }
    let now = OffsetDateTime::now_utc();
    let rows: Vec<FeatureListRow> = features
        .iter()
        .map(|feature| FeatureListRow {
            id: feature.id.to_string(),
            tasks: feature.task_count.to_string(),
            updated: style::format_relative(feature.updated_at, now),
            title: format_list_title(&feature.title),
        })
        .collect();
    let headers = ["ID", "TASKS", "UPDATED", "TITLE"];
    let align_right = [true, true, false, false];
    let widths = [
        column_width("ID", rows.iter().map(|row| row.id.as_str())),
        column_width("TASKS", rows.iter().map(|row| row.tasks.as_str())),
        column_width("UPDATED", rows.iter().map(|row| row.updated.as_str())),
        column_width("TITLE", rows.iter().map(|row| row.title.as_str())),
    ];
    let header_styles = [style::dim_style(); 4];
    println!(
        "{}",
        format_task_line(&headers, &header_styles, &widths, &align_right, paint)
    );
    let styles = [
        style::dim_style(),
        style::dim_style(),
        style::dim_style(),
        style::bold_style(),
    ];
    for row in &rows {
        println!(
            "{}",
            format_task_line(
                &[
                    row.id.as_str(),
                    row.tasks.as_str(),
                    row.updated.as_str(),
                    row.title.as_str(),
                ],
                &styles,
                &widths,
                &align_right,
                paint,
            )
        );
    }
}

struct FeatureListRow {
    id: String,
    tasks: String,
    updated: String,
    title: String,
}

fn print_feature(feature: &q_core::Feature, paint: Paint) {
    println!(
        "{} {}",
        paint.dim(&format!("#{}", feature.id)),
        paint.bold(&feature.title),
    );
    meta(paint, &format!("tasks: {}", feature.task_count));
    meta(paint, &format!("public_id: {}", feature.public_id));
    meta(
        paint,
        &format!("created_at: {}", format_timestamp(feature.created_at)),
    );
    meta(
        paint,
        &format!("updated_at: {}", format_timestamp(feature.updated_at)),
    );
    if let Some(body) = &feature.body {
        println!("\n{body}");
    }
}

const TREE_STATUS_WIDTH: usize = 11;

fn print_tree(tree: &TaskTree, paint: Paint) {
    println!("{}", render_tree_with(tree, paint));
}

#[cfg(test)]
fn render_tree(tree: &TaskTree) -> String {
    render_tree_with(tree, Paint::plain())
}

fn render_tree_with(tree: &TaskTree, paint: Paint) -> String {
    if tree.roots.is_empty() {
        return match &tree.feature {
            Some(feature) => format!("no tasks in {}", feature.title),
            None => "no tasks".into(),
        };
    }
    let anchor = tree
        .feature
        .as_ref()
        .map(|feature| feature.id)
        .or_else(|| tree.roots.first().and_then(|node| node.feature_id));
    tree.roots
        .iter()
        .map(|root| render_tree_node(root, "", true, true, anchor, paint))
        .collect::<Vec<_>>()
        .join("\n\n")
}

fn render_tree_node(
    node: &TreeNode,
    prefix: &str,
    is_root: bool,
    is_last: bool,
    anchor: Option<i64>,
    paint: Paint,
) -> String {
    let connector = if is_root {
        ""
    } else if is_last {
        "└── "
    } else {
        "├── "
    };
    let label = node.status.as_str();
    let pad = TREE_STATUS_WIDTH.saturating_sub(label.chars().count());
    let status = format!("{}{}", paint.status(label), " ".repeat(pad));
    let mut line = format!(
        "{prefix}{connector}{id}  {status}  {title}",
        prefix = paint.dim(prefix),
        connector = paint.dim(connector),
        id = paint.dim(&format!("#{}", node.id)),
        title = paint.bold(&format_list_title(&node.title)),
    );
    if let Some(project) = node
        .project
        .as_deref()
        .map(str::trim)
        .filter(|text| !text.is_empty())
    {
        line.push_str(&paint.dim(&format!("  [{project}]")));
    }
    if let Some(feature) = node
        .feature
        .as_deref()
        .filter(|_| show_tree_feature(node, anchor))
    {
        line.push_str(&paint.dim(&format!("  {{{}}}", format_list_title(feature))));
    }
    if node.external {
        line.push_str(&paint.dim("  (external)"));
    }
    if node.already_shown {
        line.push_str(&paint.dim("  (already shown)"));
    }
    if node.cycle {
        line.push_str(&paint.dim("  (cycle)"));
    }
    let child_prefix = if is_root {
        String::new()
    } else if is_last {
        format!("{prefix}    ")
    } else {
        format!("{prefix}│   ")
    };
    let mut lines = vec![line];
    for (index, child) in node.depends_on.iter().enumerate() {
        let last = index + 1 == node.depends_on.len();
        lines.push(render_tree_node(
            child,
            &child_prefix,
            false,
            last,
            anchor,
            paint,
        ));
    }
    lines.join("\n")
}

fn show_tree_feature(node: &TreeNode, anchor: Option<i64>) -> bool {
    match (node.feature_id, anchor) {
        (Some(id), Some(anchor_id)) => id != anchor_id,
        (Some(_), None) => true,
        _ => false,
    }
}

fn print_detail(detail: &q_core::TaskDetail, paint: Paint) {
    let task = &detail.task;
    println!(
        "{} {} [{}]",
        paint.dim(&format!("#{}", task.id)),
        paint.bold(&task.title),
        paint.status(task.status.as_str()),
    );
    meta(
        paint,
        &format!(
            "kind: {}  risk: {}  priority: {}",
            task.kind, task.risk, task.priority
        ),
    );
    meta(
        paint,
        &format!("project: {}", task.project.as_deref().unwrap_or("-")),
    );
    meta(
        paint,
        &format!("repo: {}", task.repo.as_deref().unwrap_or("-")),
    );
    meta(
        paint,
        &format!("feature: {}", task.feature.as_deref().unwrap_or("-")),
    );
    if !task.dependencies.is_empty() {
        let ids = task
            .dependencies
            .iter()
            .map(i64::to_string)
            .collect::<Vec<_>>()
            .join(", ");
        meta(paint, &format!("depends_on: {ids}"));
    }
    if !task.required_capabilities.is_empty() {
        meta(
            paint,
            &format!("capabilities: {}", task.required_capabilities.join(", ")),
        );
    }
    if let Some(pool) = &task.agent_pool {
        meta(paint, &format!("agent_pool: {pool}"));
    }
    meta(
        paint,
        &format!("created_at: {}", format_timestamp(task.created_at)),
    );
    meta(
        paint,
        &format!("updated_at: {}", format_timestamp(task.updated_at)),
    );
    meta(paint, &format!("public_id: {}", task.public_id));
    meta(paint, &format!("capture_path: {}", task.capture_path));
    if let Some(relative) = &task.repo_relative_path {
        meta(paint, &format!("repo_relative_path: {relative}"));
    }
    meta(
        paint,
        &format!("original_capture: {}", task.original_capture),
    );
    if let Some(body) = &task.body {
        println!("\n{body}");
    }
    if !detail.acceptance_criteria.is_empty() {
        println!("\n{}", paint.bold("acceptance criteria:"));
        for item in &detail.acceptance_criteria {
            println!("- {item}");
        }
    }
    if let Some(claim) = &detail.claim {
        println!(
            "\nclaim: agent={} active={} token={}",
            claim.agent_id, claim.active, claim.token
        );
        meta(
            paint,
            &format!(
                "lease_expires_at: {}",
                format_timestamp(claim.lease_expires_at)
            ),
        );
        if let Some(branch) = &claim.branch {
            meta(paint, &format!("branch: {branch}"));
        }
    }
    if !detail.artifacts.is_empty() {
        println!("\n{}", paint.bold("artifacts:"));
        for artifact in &detail.artifacts {
            println!("- {}: {}", artifact.kind, artifact.value);
        }
    }
    if !detail.events.is_empty() {
        println!("\n{}", paint.bold("events:"));
        print_events(&detail.events, paint, "  ");
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
        let message = match error {
            QueueError::NotFound(id) => format!("task {id} not found; try `q ls --all`"),
            QueueError::FeatureNotFound(name) => {
                format!("feature not found: {name}; try `q feature ls`")
            }
            QueueError::InvalidTransition { from, to } => {
                format!("cannot move from {from} to {to}")
            }
            QueueError::TokenMismatch => {
                "claim token does not match the active claim; pass the token from `q claim`"
                    .to_string()
            }
            QueueError::ClaimExpired => {
                "claim lease has expired; claim again or run `q recover-stale`".to_string()
            }
            other => other.to_string(),
        };
        Self { message }
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
        display_project, format_list_title, render_task_rows, render_task_rows_painted,
        render_tree, render_tree_with, truncate_chars, TaskListRow, TITLE_MAX_CHARS,
    };
    use q_core::{TaskStatus, TaskTree, TreeFeature, TreeNode};

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
                feature: "rollout".into(),
                project: "alpha".into(),
                priority: "0".into(),
                updated: "2026-09-22T20:00:00Z".into(),
                title: "Short".into(),
            },
            TaskListRow {
                id: "3".into(),
                status: "in_progress".into(),
                feature: "(none)".into(),
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
        let feature_at = char_index(lines[0], "FEATURE");
        let project_at = char_index(lines[0], "PROJECT");
        assert_eq!(char_index(lines[1], "rollout"), feature_at);
        assert_eq!(char_index(lines[1], "alpha"), project_at);
        assert!(chars_at(lines[2], feature_at).starts_with("(none)"));
        assert!(chars_at(lines[2], project_at).starts_with("(none)"));
        let updated_at = char_index(lines[0], "UPDATED");
        assert_eq!(char_index(lines[1], "2026-09-22T20:00:00Z"), updated_at);
        assert_eq!(char_index(lines[2], "2026-09-22T19:00:00Z"), updated_at);
        assert!(lines[2].contains('…'));
        assert!(!table.contains(&long));
        assert!(lines[0].find("ID").unwrap() < lines[0].find("STATUS").unwrap());
        assert!(lines[0].find("STATUS").unwrap() < lines[0].find("FEATURE").unwrap());
        assert!(lines[0].find("FEATURE").unwrap() < lines[0].find("PROJECT").unwrap());
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
                feature: "(none)".into(),
                project: "alpha".into(),
                priority: "0".into(),
                updated: "3m ago".into(),
                title: "Keep the inbox item".into(),
            },
            TaskListRow {
                id: "2".into(),
                status: "ready".into(),
                feature: "(none)".into(),
                project: "beta".into(),
                priority: "1".into(),
                updated: "1h ago".into(),
                title: "Compare encodings".into(),
            },
            TaskListRow {
                id: "1".into(),
                status: "inbox".into(),
                feature: "(none)".into(),
                project: "(none)".into(),
                priority: "0".into(),
                updated: "2d ago".into(),
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
ID  STATUS  FEATURE  PROJECT  PRI  UPDATED  TITLE
 4  inbox   (none)   alpha      0  3m ago   Keep the inbox item
 2  ready   (none)   beta       1  1h ago   Compare encodings
 1  inbox   (none)   (none)     0  2d ago   Unassigned capture"
        );
    }

    #[test]
    fn color_does_not_change_visible_table_or_tree() {
        let rows = vec![TaskListRow {
            id: "2".into(),
            status: "ready".into(),
            feature: "(none)".into(),
            project: "beta".into(),
            priority: "1".into(),
            updated: "3m ago".into(),
            title: "Compare encodings".into(),
        }];
        let plain = render_task_rows(&rows);
        let colored = render_task_rows_painted(&rows, crate::style::Paint::color());
        assert!(colored.contains('\u{1b}'));
        assert_eq!(anstream::adapter::strip_str(&colored).to_string(), plain);
        assert!(colored.contains("32"));

        let node = TreeNode {
            id: 2,
            status: TaskStatus::Blocked,
            title: "Write the schema".into(),
            project: Some("api".into()),
            feature_id: None,
            feature: None,
            external: false,
            already_shown: false,
            cycle: false,
            depends_on: vec![],
        };
        let tree = TaskTree {
            feature: None,
            roots: vec![node],
        };
        let plain = render_tree(&tree);
        let colored = render_tree_with(&tree, crate::style::Paint::color());
        assert!(colored.contains('\u{1b}'));
        assert!(colored.contains("31"));
        assert_eq!(anstream::adapter::strip_str(&colored).to_string(), plain);
        assert!(plain.contains("└──") || plain.starts_with("#2"));
    }

    #[test]
    fn tree_layout_marks_external_repeats_and_empty_features() {
        let types = TreeNode {
            id: 4,
            status: TaskStatus::Inbox,
            title: "Add the types".into(),
            project: Some("api".into()),
            feature_id: Some(1),
            feature: Some("Rollout".into()),
            external: false,
            already_shown: false,
            cycle: false,
            depends_on: vec![],
        };
        let mut repeated = types.clone();
        repeated.already_shown = true;
        let schema = TreeNode {
            id: 2,
            status: TaskStatus::Ready,
            title: "Write the schema".into(),
            project: Some("api".into()),
            feature_id: Some(1),
            feature: Some("Rollout".into()),
            external: false,
            already_shown: false,
            cycle: false,
            depends_on: vec![types],
        };
        let shared = TreeNode {
            id: 1,
            status: TaskStatus::Done,
            title: "Shared schema".into(),
            project: Some("db".into()),
            feature_id: Some(2),
            feature: Some("Other".into()),
            external: true,
            already_shown: false,
            cycle: false,
            depends_on: vec![repeated],
        };
        let ship = TreeNode {
            id: 3,
            status: TaskStatus::Inbox,
            title: "Ship the rollout".into(),
            project: Some("api".into()),
            feature_id: Some(1),
            feature: Some("Rollout".into()),
            external: false,
            already_shown: false,
            cycle: false,
            depends_on: vec![shared, schema],
        };
        let notes = TreeNode {
            id: 5,
            status: TaskStatus::Blocked,
            title: "  Write\nthe notes  ".into(),
            project: None,
            feature_id: Some(1),
            feature: Some("Rollout".into()),
            external: false,
            already_shown: false,
            cycle: false,
            depends_on: vec![TreeNode {
                id: 3,
                status: TaskStatus::Inbox,
                title: "Ship the rollout".into(),
                project: Some("api".into()),
                feature_id: Some(1),
                feature: Some("Rollout".into()),
                external: false,
                already_shown: false,
                cycle: true,
                depends_on: vec![],
            }],
        };
        let tree = TaskTree {
            feature: Some(TreeFeature {
                id: 1,
                title: "Rollout".into(),
            }),
            roots: vec![ship, notes],
        };
        assert_eq!(
            render_tree(&tree),
            "\
#3  inbox        Ship the rollout  [api]
├── #1  done         Shared schema  [db]  {Other}  (external)
│   └── #4  inbox        Add the types  [api]  (already shown)
└── #2  ready        Write the schema  [api]
    └── #4  inbox        Add the types  [api]

#5  blocked      Write the notes
└── #3  inbox        Ship the rollout  [api]  (cycle)"
        );

        let empty = TaskTree {
            feature: Some(TreeFeature {
                id: 9,
                title: "Empty".into(),
            }),
            roots: vec![],
        };
        assert_eq!(render_tree(&empty), "no tasks in Empty");
    }

    fn char_index(line: &str, needle: &str) -> usize {
        let byte = line
            .find(needle)
            .unwrap_or_else(|| panic!("missing {needle} in {line}"));
        line[..byte].chars().count()
    }

    fn chars_at(line: &str, at: usize) -> &str {
        let byte = line
            .char_indices()
            .nth(at)
            .map(|(index, _)| index)
            .unwrap_or(line.len());
        &line[byte..]
    }
}
