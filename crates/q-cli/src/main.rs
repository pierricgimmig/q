mod cli;
mod skill;
mod style;
mod workers;

use std::fs;
use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use anstyle::Style;
use clap::Parser;
use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use q_core::{
    format_timestamp, lease_from_minutes, Actor, ArtifactInput, BlockRequest, CancelRequest,
    CaptureRequest, ClaimRequest, CompleteRequest, CreateFeatureRequest, DeleteRequest,
    EditFeatureRequest, EditRequest, HeartbeatRequest, HoldRequest, ListFilter, LogRequest,
    QueueError, QueueService, ReadyRequest, RecoverRequest, ReleaseRequest, RiskLevel,
    StaleDisposition, StartRequest, TaskKind, TaskStatus, TaskSummary, TaskTree, TreeNode,
    TreeQuery,
};
use q_http::RemoteQueue;
use q_project::{discover, render_init_config, DiscoverOptions, ProjectContext};
use q_store::{default_db_path, Queue};
use time::OffsetDateTime;

use crate::cli::{Commands, FeatureCommand, ProjectCommand, TokenCommand};
use crate::style::Paint;

fn main() {
    // Windows gives the process main thread 1 MiB. Parsing this CLI and
    // drawing `q top` both need more than that; the other platforms start
    // the main thread at 8 MiB. Run the real entry point on that larger stack.
    #[cfg(windows)]
    {
        const STACK: usize = 8 * 1024 * 1024;
        let joined = std::thread::Builder::new()
            .name("q".into())
            .stack_size(STACK)
            .spawn(cli_main)
            .expect("failed to start q")
            .join();
        if let Err(payload) = joined {
            std::panic::resume_unwind(payload);
        }
    }
    #[cfg(not(windows))]
    cli_main();
}

#[tokio::main(flavor = "current_thread")]
async fn cli_main() {
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

/// Where the queue lives: a local SQLite file or a `q serve` URL.
enum Backend {
    Local(PathBuf),
    Remote(String),
}

impl Backend {
    fn describe(&self, paint: Paint) -> String {
        match self {
            Self::Local(path) => {
                format!("database: {}", paint.dim(&path.display().to_string()))
            }
            Self::Remote(url) => format!("server: {}", paint.dim(url)),
        }
    }

    fn json_fields(&self) -> (&'static str, String) {
        match self {
            Self::Local(path) => ("db", path.display().to_string()),
            Self::Remote(url) => ("server", url.clone()),
        }
    }
}

fn env_nonempty(key: &str) -> Option<String> {
    std::env::var(key)
        .ok()
        .filter(|value| !value.trim().is_empty())
}

/// Open the service the command will run against. `--server` (or
/// `$Q_SERVER_URL`) selects the remote authority; otherwise the local file.
fn open_service(cli: &cli::Cli) -> Result<(Arc<dyn QueueService>, Backend), CliError> {
    let server = cli
        .server
        .clone()
        .filter(|value| !value.trim().is_empty())
        .or_else(|| env_nonempty("Q_SERVER_URL"));
    match server {
        Some(url) => {
            let token = cli.token.clone().or_else(|| env_nonempty("Q_SERVER_TOKEN"));
            let remote = RemoteQueue::new(&url, token)?;
            tracing::info!(server = %remote.url(), "using remote queue");
            let backend = Backend::Remote(remote.url().to_string());
            Ok((Arc::new(remote), backend))
        }
        None => {
            let db = db_path(cli);
            tracing::info!(db = %db.display(), "opening queue");
            let queue = Queue::open(&db)?;
            Ok((Arc::new(queue), Backend::Local(db)))
        }
    }
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
    let repo = cli.repo.clone();
    let project = cli.project.clone();
    let directory = cli.directory.clone();
    match &cli.command {
        Commands::Mcp => {
            let (queue, _) = open_service(&cli)?;
            q_mcp::serve(queue, base_dir(directory.as_deref())?).await?;
            Ok(())
        }
        Commands::Top {
            interval,
            status,
            kind,
            limit,
            all,
            feature,
            tag,
            stale_after,
            escalated,
            once,
        } => {
            let (interval, limit, all, once) = (*interval, *limit, *all, *once);
            let (status, kind, feature) = (status.clone(), kind.clone(), feature.clone());
            if ui.json {
                return Err(CliError::message(
                    "q top is interactive; use q ls --json or q status --json",
                ));
            }
            if !interval.is_finite() || interval < 0.1 {
                return Err(CliError::message("--interval must be at least 0.1 seconds"));
            }
            let status = if *escalated {
                Some(TaskStatus::Escalated)
            } else {
                match status {
                    Some(status) => Some(TaskStatus::parse(&status)?),
                    None => None,
                }
            };
            let kind = match kind {
                Some(kind) => Some(TaskKind::parse(&kind)?),
                None => None,
            };
            let filter = ListFilter {
                status,
                project: project.clone(),
                repo: repo.clone(),
                kind,
                feature,
                limit: TOP_FETCH_LIMIT,
                include_terminal: true,
                tags: split_caps(tag.clone()),
            };
            let screen = io::stdout().is_terminal() && !once;
            let options = TopOptions {
                interval: std::time::Duration::from_secs_f64(interval),
                show_terminal: all || filter.status.is_some(),
                limit: limit.max(1) as usize,
                once,
                screen,
                keys: screen && io::stdin().is_terminal(),
                stale_after: std::time::Duration::from_secs(*stale_after),
            };
            let (queue, backend) = open_service(&cli)?;
            run_top(queue.as_ref(), &backend, &filter, &options, &ui).await
        }
        Commands::Serve {
            bind,
            auth,
            public_url,
            sweep_interval,
        } => {
            serve(
                &cli,
                bind,
                auth.as_deref(),
                public_url.as_deref(),
                *sweep_interval,
            )
            .await
        }
        Commands::Token { command } => token_command(&cli, command, &ui),
        Commands::Workers { .. } => {
            let context = resolve_context(directory.as_deref(), repo, project)?;
            let cwd = base_dir(directory.as_deref())?;
            workers::run(&cli, &context, &cwd).map_err(CliError::message)
        }
        Commands::Orbit {
            url,
            interval,
            history,
            segment,
            once,
        } => {
            let options = orbit_options(*interval, history, *segment, *once)?;
            let url = url
                .clone()
                .filter(|value| !value.trim().is_empty())
                .or_else(|| env_nonempty(q_orbit::ORBIT_URL_ENV))
                .unwrap_or_else(|| q_orbit::DEFAULT_ORBIT_URL.to_string());
            let (queue, backend) = open_service(&cli)?;
            run_orbit(queue, &backend, &url, options, &ui).await
        }
        Commands::Exec {
            id,
            claim_token,
            thread,
            command,
        } => {
            let (id, claim_token, thread, command) =
                (*id, claim_token.clone(), thread.clone(), command.clone());
            if let Some(name) = thread.as_deref() {
                if name.is_empty() || name.contains(char::is_whitespace) {
                    return Err(CliError::message("--thread needs a name without spaces"));
                }
            }
            let (queue, _) = open_service(&cli)?;
            let code = run_exec(queue.as_ref(), id, claim_token, thread, &command, &ui).await?;
            let _ = io::stdout().flush();
            let _ = io::stderr().flush();
            std::process::exit(code);
        }
        Commands::Skill { command } => match command {
            None => skill::print_skill(ui.json).map_err(CliError::message),
            Some(cli::SkillCommand::Install { target, force }) => {
                let home = skill::home_dir().map_err(CliError::message)?;
                skill::install_skill(&home, target, *force, ui.json).map_err(CliError::message)
            }
        },
        Commands::Project { command } => match command {
            ProjectCommand::Show => {
                let context = resolve_context(directory.as_deref(), repo, project)?;
                emit(&ui, &context, || print_context(&context, ui.out));
                Ok(())
            }
            ProjectCommand::Init { yes, force } => {
                init_project(directory.as_deref(), repo, project, *yes, *force, ui.out)
            }
        },
        _ => {
            let (queue, backend) = open_service(&cli)?;
            dispatch(
                queue.as_ref(),
                &backend,
                repo,
                project,
                directory.as_deref(),
                cli.command,
                &ui,
            )
        }
    }
}

/// Exit code reported when the command could not be started.
const EXEC_NOT_FOUND: i32 = 127;

/// `q exec`: run a command with the log entries `@exec <command>` before it
/// and `@exit <code> (<duration>) <command>` after it, so the bridge draws
/// the process as a scope under the agent's thread. Returns the exit code to
/// pass on. A failure to log is a warning, never a reason to skip the command.
async fn run_exec(
    queue: &dyn QueueService,
    id: i64,
    claim_token: Option<String>,
    thread: Option<String>,
    command: &[String],
    ui: &Ui,
) -> Result<i32, CliError> {
    let Some((program, args)) = command.split_first() else {
        return Err(CliError::message("exec needs a command"));
    };
    let command_line = shell_join(command);
    let prefix = thread
        .as_deref()
        .map(|name| format!("[{name}] "))
        .unwrap_or_default();
    let note = |message: String| {
        let result = queue.log(LogRequest {
            task_id: id,
            claim_token: claim_token.clone(),
            message: Some(message),
            progress: None,
            artifacts: Vec::new(),
            actor: human_actor(),
        });
        if let Err(error) = result {
            eprintln!(
                "{} could not log to task #{id}: {error}",
                ui.err.warning_label()
            );
        }
    };

    note(format!(
        "{prefix}{} {command_line}",
        q_orbit::mapper::EXEC_MARKER
    ));
    let started = std::time::Instant::now();
    let spawned = Command::new(program).args(args).spawn();
    let (code, failure) = match spawned {
        Ok(mut child) => {
            let wait = tokio::task::spawn_blocking(move || child.wait());
            tokio::pin!(wait);
            let status = loop {
                tokio::select! {
                    joined = &mut wait => break joined.map_err(io::Error::other).and_then(|r| r),
                    _ = tokio::signal::ctrl_c() => {
                        // The child got the same signal; keep waiting so its
                        // exit is logged.
                    }
                }
            };
            match status {
                Ok(status) => (exit_code(status), None),
                Err(error) => (1, Some(format!("waiting for {program}: {error}"))),
            }
        }
        Err(error) => (
            EXEC_NOT_FOUND,
            Some(format!("cannot run {program}: {error}")),
        ),
    };
    note(format!(
        "{prefix}{} {code} ({}) {command_line}",
        q_orbit::mapper::EXIT_MARKER,
        format_elapsed(started.elapsed())
    ));
    if let Some(failure) = failure {
        eprintln!("{} {failure}", ui.err.error_label());
    }
    Ok(code)
}

/// The process's exit code; a signal death is 128 + the signal, as in a shell.
fn exit_code(status: std::process::ExitStatus) -> i32 {
    if let Some(code) = status.code() {
        return code;
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(signal) = status.signal() {
            return 128 + signal;
        }
    }
    1
}

/// `48ms`, `1.2s`, `42.0s`, `3m07s`, `1h02m`: one token, for the `@exit` note.
fn format_elapsed(elapsed: std::time::Duration) -> String {
    let secs = elapsed.as_secs();
    if secs < 1 {
        format!("{}ms", elapsed.as_millis())
    } else if secs < 60 {
        format!("{:.1}s", elapsed.as_secs_f64())
    } else if secs < 3600 {
        format!("{}m{:02}s", secs / 60, secs % 60)
    } else {
        format!("{}h{:02}m", secs / 3600, (secs % 3600) / 60)
    }
}

/// One line for a command and its arguments, quoting what a shell would
/// need quoted, so the note reads back as the command that ran.
fn shell_join(command: &[String]) -> String {
    command
        .iter()
        .map(|arg| shell_quote(arg))
        .collect::<Vec<_>>()
        .join(" ")
}

fn shell_quote(arg: &str) -> String {
    let plain = !arg.is_empty()
        && arg.chars().all(|c| {
            c.is_ascii_alphanumeric()
                || matches!(c, '-' | '_' | '.' | '/' | ':' | '=' | '@' | ',' | '+' | '%')
        });
    if plain {
        arg.to_string()
    } else if !arg.contains('\'') {
        format!("'{arg}'")
    } else {
        format!(
            "\"{}\"",
            arg.replace('\\', "\\\\")
                .replace('"', "\\\"")
                .replace('$', "\\$")
                .replace('`', "\\`")
        )
    }
}

/// `q orbit`: tail the log and push it to Orbit until Ctrl-C.
async fn run_orbit(
    queue: Arc<dyn QueueService>,
    backend: &Backend,
    url: &str,
    options: q_orbit::BridgeOptions,
    ui: &Ui,
) -> Result<(), CliError> {
    let client =
        q_orbit::OrbitClient::new(url).map_err(|err| CliError::message(err.to_string()))?;
    let json = ui.json;
    let paint = ui.out;
    if !json {
        println!(
            "{} {}  orbit: {}  every {}  {}",
            paint.bold("q orbit"),
            backend.describe(paint),
            paint.dim(client.url()),
            paint.dim(&format!("{:.1}s", options.interval.as_secs_f64())),
            if options.once {
                paint.dim("one pass")
            } else {
                paint.dim("Ctrl-C quits")
            },
        );
    }
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let stop_flag = stop.clone();
    if !options.once {
        tokio::spawn(async move {
            let _ = tokio::signal::ctrl_c().await;
            stop_flag.store(true, std::sync::atomic::Ordering::Relaxed);
        });
    }
    let worker = tokio::task::spawn_blocking(move || {
        let stop = move || stop.load(std::sync::atomic::Ordering::Relaxed);
        let mut on_batch = |batch: &q_orbit::bridge::BatchReport| {
            if json {
                return;
            }
            let dropped = if batch.dropped_before_start > 0 {
                format!(", {} before capture start", batch.dropped_before_start)
            } else {
                String::new()
            };
            println!(
                "  {}  pushed {} events, {} names (accepted {}{dropped})  {} open spans, {} active claims  {}",
                paint.dim(&format_clock(OffsetDateTime::now_utc())),
                batch.events,
                batch.names,
                batch.accepted,
                batch.open_spans,
                batch.active_claims,
                paint.dim(&format!("through event {}", batch.last_event_id)),
            );
        };
        q_orbit::run(queue.as_ref(), &client, &options, &stop, &mut on_batch)
    });
    let report = worker
        .await
        .map_err(|err| CliError::message(format!("orbit bridge stopped: {err}")))?
        .map_err(|err| CliError::message(err.to_string()))?;
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&report).unwrap_or_default()
        );
    } else {
        println!(
            "{} {} events seen, {} pushed in {} batches, {} tasks, {} open spans",
            paint.bold("done:"),
            report.events_seen,
            report.pushed_events,
            report.batches,
            report.tasks,
            report.open_spans,
        );
    }
    Ok(())
}

fn orbit_options(
    interval: f64,
    history: &str,
    segment: f64,
    once: bool,
) -> Result<q_orbit::BridgeOptions, CliError> {
    if !interval.is_finite() || interval < 0.1 {
        return Err(CliError::message("--interval must be at least 0.1 seconds"));
    }
    if !segment.is_finite() || segment < 0.0 {
        return Err(CliError::message(
            "--segment must be 0 or a positive number of seconds",
        ));
    }
    let history = parse_age(history)?;
    let mut options = q_orbit::BridgeOptions {
        interval: std::time::Duration::from_secs_f64(interval),
        history,
        once,
        ..q_orbit::BridgeOptions::default()
    };
    options.mapper.segment = if segment == 0.0 {
        None
    } else {
        Some(std::time::Duration::from_secs_f64(segment))
    };
    Ok(options)
}

/// `all`, or a number with an optional unit: s, m, h, d. A bare number is seconds.
fn parse_age(value: &str) -> Result<Option<std::time::Duration>, CliError> {
    let value = value.trim();
    if value.eq_ignore_ascii_case("all") {
        return Ok(None);
    }
    let (number, unit) = match value.find(|c: char| !c.is_ascii_digit() && c != '.') {
        Some(index) => value.split_at(index),
        None => (value, "s"),
    };
    let amount: f64 = number.parse().map_err(|_| {
        CliError::message(format!(
            "invalid duration {value:?}; use 30m, 1h, 2d, or all"
        ))
    })?;
    let seconds = match unit.trim() {
        "s" | "sec" | "secs" => amount,
        "m" | "min" | "mins" => amount * 60.0,
        "h" | "hr" | "hrs" => amount * 3600.0,
        "d" | "day" | "days" => amount * 86_400.0,
        _ => {
            return Err(CliError::message(format!(
                "invalid duration {value:?}; use 30m, 1h, 2d, or all"
            )))
        }
    };
    if !seconds.is_finite() || seconds < 0.0 {
        return Err(CliError::message(format!("invalid duration {value:?}")));
    }
    Ok(Some(std::time::Duration::from_secs_f64(seconds)))
}

/// Token file next to the database unless overridden.
fn tokens_path(cli: &cli::Cli, explicit: Option<&Path>) -> PathBuf {
    match explicit {
        Some(path) => path.to_path_buf(),
        None => db_path(cli)
            .parent()
            .map(|dir| dir.join("tokens.toml"))
            .unwrap_or_else(|| PathBuf::from("tokens.toml")),
    }
}

/// `q serve`: own the local file and answer remote CLI, MCP, and chat clients.
async fn serve(
    cli: &cli::Cli,
    bind: &str,
    auth: Option<&Path>,
    public_url: Option<&str>,
    sweep_secs: u64,
) -> Result<(), CliError> {
    if cli.server.is_some() {
        return Err(CliError::message(
            "q serve owns a local database; it cannot be pointed at another --server",
        ));
    }
    let addr: std::net::SocketAddr = bind
        .parse()
        .map_err(|err| CliError::message(format!("invalid --bind {bind}: {err}")))?;
    let db = db_path(cli);
    let tokens = tokens_path(cli, auth);
    let store = if auth.is_some() || tokens.exists() {
        Some(q_http::TokenStore::from_file(&tokens).map_err(CliError::message)?)
    } else {
        None
    };
    if public_url.is_some() && store.is_none() {
        return Err(CliError::message(
            "--public-url requires a token file; create a token first or pass --auth FILE (an empty file denies all access)",
        ));
    }
    q_http::check_bind(&addr, store.as_ref()).map_err(CliError::message)?;
    let key_path = db
        .parent()
        .map(|dir| dir.join("oauth.key"))
        .unwrap_or_else(|| PathBuf::from("oauth.key"));
    let signing_key = q_http::SigningKey::load_or_create(&key_path).map_err(CliError::message)?;
    let grants = Arc::new(
        q_http::GrantStore::open(&db.with_file_name("oauth.db")).map_err(CliError::message)?,
    );
    let queue: Arc<dyn QueueService> = Arc::new(Queue::open(&db)?);
    let listener = tokio::net::TcpListener::bind(addr).await?;
    let local = listener.local_addr()?;
    let mode = match &store {
        Some(store) => format!("{} token(s) from {}", store.len(), tokens.display()),
        None => "no auth, loopback only".to_string(),
    };
    eprintln!(
        "q serve listening on http://{local} (database: {}, {mode})",
        db.display()
    );
    // The origin the server will actually answer for: the canonical form of
    // --public-url, or the loopback address when bound to all interfaces.
    let public = match public_url {
        Some(url) => q_http::canonical_origin(url.trim()).map_err(CliError::message)?,
        None => {
            let mut shown = local;
            if shown.ip().is_unspecified() {
                shown.set_ip(if shown.is_ipv4() {
                    std::net::Ipv4Addr::LOCALHOST.into()
                } else {
                    std::net::Ipv6Addr::LOCALHOST.into()
                });
            }
            format!("http://{shown}")
        }
    };
    eprintln!("connector URL for Grok, Claude, or ChatGPT: {public}/mcp");
    if store.is_none() {
        eprintln!("chat connectors need a token file; run: q token create NAME --role human");
    }
    let options = q_http::ServerOptions {
        auth: store,
        grants,
        public_url: Some(public),
        signing_key,
        base_dir: base_dir(cli.directory.as_deref())?,
        sweep_interval: std::time::Duration::from_secs(sweep_secs),
    };
    q_http::serve_on(queue, listener, options, async {
        let _ = tokio::signal::ctrl_c().await;
        tracing::info!("shutting down");
    })
    .await?;
    Ok(())
}

fn token_command(cli: &cli::Cli, command: &TokenCommand, ui: &Ui) -> Result<(), CliError> {
    match command {
        TokenCommand::Create { name, role, auth } => {
            let role = q_http::Role::parse(role).map_err(CliError::message)?;
            let path = tokens_path(cli, auth.as_deref());
            let secret =
                q_http::AuthConfig::create_token(&path, name, role).map_err(CliError::message)?;
            let body = serde_json::json!({
                "name": name,
                "role": role.as_str(),
                "secret": secret,
                "path": path,
            });
            emit(ui, &body, || {
                println!(
                    "created token {name} ({}) in {}",
                    role.as_str(),
                    path.display()
                );
                println!("secret: {secret}");
                match role {
                    q_http::Role::Human => println!(
                        "Paste it on the connector sign-in page, or export it as Q_SERVER_TOKEN."
                    ),
                    q_http::Role::Agent => println!(
                        "Give it to that agent as Q_SERVER_TOKEN, or as a Bearer header for /mcp."
                    ),
                }
                println!("A q serve started with a token file picks it up without a restart; restart servers started without authentication.");
            });
            Ok(())
        }
        TokenCommand::Ls { auth } => {
            let path = tokens_path(cli, auth.as_deref());
            let config = if path.exists() {
                q_http::AuthConfig::load(&path).map_err(CliError::message)?
            } else {
                q_http::AuthConfig::default()
            };
            let tokens: Vec<serde_json::Value> = config
                .tokens
                .iter()
                .map(|token| serde_json::json!({"name": token.name, "role": token.role.as_str()}))
                .collect();
            emit(
                ui,
                &serde_json::json!({"path": path, "tokens": tokens}),
                || {
                    println!("token file: {}", path.display());
                    if config.tokens.is_empty() {
                        println!("(no tokens)");
                    }
                    for token in &config.tokens {
                        println!("{}\t{}", token.name, token.role.as_str());
                    }
                },
            );
            Ok(())
        }
        TokenCommand::Revoke { name, auth } => {
            let path = tokens_path(cli, auth.as_deref());
            q_http::AuthConfig::revoke_token(&path, name).map_err(CliError::message)?;
            emit(
                ui,
                &serde_json::json!({"revoked": name, "path": path}),
                || {
                    println!("revoked token {name}");
                },
            );
            Ok(())
        }
    }
}

fn dispatch(
    queue: &dyn QueueService,
    backend: &Backend,
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
            edit,
            capability,
            agent_pool,
            depends_on,
            feature,
            tag,
            hold,
        } => {
            let context = resolve_context(directory, repo, project)?;
            let mut body = read_body(body, body_file.as_deref())?;
            if edit {
                let seed = body.clone().unwrap_or_else(q_core::body_template);
                body = require_editor(edit_in_editor(&seed)?)?;
            }
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
                hold,
                tags: split_caps(tag.clone()),
            })?;
            let id = task.id;
            emit(ui, &task, || {
                confirm_capture(ui, id, task.status.as_str(), &task.title);
            });
            Ok(())
        }
        Commands::Ls {
            status,
            escalated,
            kind,
            limit,
            all,
            feature,
            tag,
        } => {
            let status = if escalated {
                Some(TaskStatus::Escalated)
            } else {
                match status {
                    Some(status) => Some(TaskStatus::parse(&status)?),
                    None => None,
                }
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
                tags: split_caps(tag.clone()),
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
            edit,
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
            tag,
            clear_tags,
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
            if !tag.is_empty() {
                request.tags = Some(split_caps(tag));
            }
            request.clear_tags = clear_tags;
            if edit {
                let seed = match request.body.take() {
                    Some(body) => body,
                    None => queue.get(id)?.task.body.unwrap_or_default(),
                };
                request.body = Some(require_editor(edit_in_editor(&seed)?)?.unwrap_or_default());
            } else if !request.has_changes() {
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
        Commands::Ready { ids } => for_each_task(
            ui,
            &ids,
            |id| {
                let outcome = queue.mark_ready(ReadyRequest {
                    task_id: id,
                    actor: human_actor(),
                })?;
                for warning in &outcome.warnings {
                    eprintln!("{} {warning}", ui.err.warning_label());
                }
                Ok(outcome)
            },
            |outcome| {
                let task = &outcome.task;
                confirm(ui, "ready", task.id, task.status.as_str(), &task.title);
            },
        ),
        Commands::Hold { id } => {
            let task = queue.hold(HoldRequest {
                task_id: id,
                actor: human_actor(),
            })?;
            emit(ui, &task, || {
                confirm(ui, "held", task.id, task.status.as_str(), &task.title);
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
        Commands::Cancel { ids } => for_each_task(
            ui,
            &ids,
            |id| {
                Ok(queue.cancel(CancelRequest {
                    task_id: id,
                    actor: human_actor(),
                })?)
            },
            |task| confirm(ui, "cancelled", task.id, task.status.as_str(), &task.title),
        ),
        Commands::Delete { ids, force } => for_each_task(
            ui,
            &ids,
            |id| {
                Ok(queue.delete(DeleteRequest {
                    task_id: id,
                    force,
                    actor: human_actor(),
                })?)
            },
            |outcome| {
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
            },
        ),
        Commands::Claim {
            id,
            agent,
            capability,
            kind,
            max_risk,
            lease_minutes,
            agent_pool,
            model,
            host,
            tag,
            max_failures,
        } => {
            let mut request = ClaimRequest::new(agent);
            request.task_id = id;
            let (model, host) = q_core::local_worker_identity(model, host);
            request.agent_model = model;
            request.agent_host = host;
            request.tags = split_caps(tag);
            request.max_failures = max_failures;
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
                    print_progress_hint(ui, task.task.id, &claim.token);
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
            activity,
        } => {
            let lease = match lease_minutes {
                Some(minutes) => Some(lease_from_minutes(minutes)?),
                None => None,
            };
            let claim = queue.heartbeat(HeartbeatRequest {
                task_id: id,
                claim_token,
                lease,
                activity,
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
                claim_token: claim_token.clone(),
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
                print_progress_hint(ui, detail.task.id, &claim_token);
            });
            Ok(())
        }
        Commands::Complete {
            id,
            claim_token,
            summary,
            status,
            artifact,
            attach,
        } => {
            let target = match status {
                Some(status) => Some(TaskStatus::parse(&status)?),
                None => None,
            };
            let artifacts = collect_artifacts(artifact, attach)?;
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
        Commands::Escalate {
            id,
            reason,
            claim_token,
        } => {
            let task = queue.escalate(q_core::EscalateRequest {
                task_id: id,
                claim_token,
                reason,
                actor: human_actor(),
            })?;
            emit(ui, &task, || {
                confirm(ui, "escalated", task.id, task.status.as_str(), &task.title);
                if let Some(reason) = &task.escalated_reason {
                    println!("reason: {reason}");
                }
            });
            Ok(())
        }
        Commands::Fail {
            id,
            note,
            claim_token,
        } => {
            let task = queue.fail(q_core::FailRequest {
                task_id: id,
                claim_token,
                note,
                actor: human_actor(),
            })?;
            emit(ui, &task, || {
                confirm(ui, "failed", task.id, task.status.as_str(), &task.title);
                println!("failures: {}", task.failure_count);
            });
            Ok(())
        }
        Commands::Note {
            id,
            message,
            claim_token,
        } => {
            let detail = queue.note(q_core::NoteRequest {
                task_id: id,
                claim_token,
                message: message.clone(),
                actor: human_actor(),
            })?;
            emit(ui, &detail, || {
                println!(
                    "noted {} {}",
                    ui.out.dim(&format!("#{}", detail.task.id)),
                    ui.out.bold(&message)
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
            let (origin_key, origin) = backend.json_fields();
            let body = serde_json::json!({
                origin_key: origin,
                "counts": status.counts,
                "active_claims": status.active_claims,
                "expired_claims": status.expired_claims,
            });
            emit(ui, &body, || print_status(backend, &status, ui.out));
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
        Commands::Reopen { ids } => for_each_task(
            ui,
            &ids,
            |id| Ok(queue.reopen(id, human_actor())?),
            |task| confirm(ui, "reopened", task.id, task.status.as_str(), &task.title),
        ),
        Commands::Log {
            id,
            message,
            claim_token,
            progress,
            artifact,
            attach,
        } => {
            let artifacts = collect_artifacts(artifact, attach)?;
            let message = message.filter(|text| !text.trim().is_empty());
            if message.is_none() && progress.is_none() && artifacts.is_empty() {
                let events = queue.events(id)?;
                emit(ui, &serde_json::json!({"events": events}), || {
                    print_events(&events, ui.out, "");
                });
                return Ok(());
            }
            let added = artifacts.len();
            let detail = queue.log(LogRequest {
                task_id: id,
                claim_token,
                message,
                progress,
                artifacts,
                actor: human_actor(),
            })?;
            emit(ui, &detail, || {
                let last = detail.events.last();
                println!(
                    "logged {} {}",
                    ui.out.dim(&format!("#{id}")),
                    match last {
                        Some(event) => describe_event(event, ui.out),
                        None => format!("{added} artifact(s)"),
                    }
                );
            });
            Ok(())
        }
        Commands::Artifact { id } => {
            let artifact = queue.artifact(id)?;
            emit(ui, &artifact, || match &artifact.content {
                Some(content) => {
                    print!("{content}");
                    if !content.ends_with('\n') {
                        println!();
                    }
                }
                None => println!(
                    "{}: {} {}",
                    artifact.artifact.kind,
                    artifact.artifact.value,
                    ui.out.dim("(reference only, no stored content)")
                ),
            });
            Ok(())
        }
        Commands::Feature { command } => dispatch_feature(queue, command, ui),
        Commands::Project { .. }
        | Commands::Mcp
        | Commands::Top { .. }
        | Commands::Serve { .. }
        | Commands::Token { .. }
        | Commands::Skill { .. }
        | Commands::Workers { .. }
        | Commands::Orbit { .. }
        | Commands::Exec { .. } => {
            unreachable!("handled before queue open")
        }
    }
}

fn dispatch_feature(
    queue: &dyn QueueService,
    command: FeatureCommand,
    ui: &Ui,
) -> Result<(), CliError> {
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
    Ok(ArtifactInput::reference(kind, artifact_value))
}

/// `--artifact kind=value` references plus `--attach [kind=]path` files whose
/// text is stored in the database.
fn collect_artifacts(
    references: Vec<String>,
    attachments: Vec<PathBuf>,
) -> Result<Vec<ArtifactInput>, CliError> {
    let mut artifacts = Vec::new();
    for item in references {
        artifacts.push(parse_artifact(&item)?);
    }
    for item in attachments {
        artifacts.push(read_attachment(&item)?);
    }
    Ok(artifacts)
}

const ATTACH_DEFAULT_KIND: &str = "report";

fn read_attachment(spec: &Path) -> Result<ArtifactInput, CliError> {
    let text = spec.to_string_lossy();
    let (kind, path) = match text.split_once('=') {
        Some((kind, path)) if !kind.trim().is_empty() && !kind.contains(['/', '\\']) => {
            (kind.trim().to_string(), PathBuf::from(path.trim()))
        }
        _ => (ATTACH_DEFAULT_KIND.to_string(), spec.to_path_buf()),
    };
    let content = fs::read_to_string(&path)
        .map_err(|err| CliError::message(format!("read {}: {err}", path.display())))?;
    Ok(ArtifactInput {
        kind,
        value: path.display().to_string(),
        content: Some(content),
    })
}

/// Turn the editor result into a body. `None` means no editor is configured;
/// a blank result means no body.
fn require_editor(edited: Option<String>) -> Result<Option<String>, CliError> {
    match edited {
        Some(body) if body.trim().is_empty() => Ok(None),
        Some(body) => Ok(Some(body)),
        None => Err(CliError::message(
            "--edit needs $VISUAL or $EDITOR to be set",
        )),
    }
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
        print_json(value);
    } else {
        human();
    }
}

fn print_json(value: &impl serde::Serialize) {
    serde_json::to_writer_pretty(io::stdout(), value).expect("write json");
    println!();
}

/// One failed id in a multi-id command, as reported under `errors` in `--json`.
#[derive(serde::Serialize)]
struct TaskError {
    id: i64,
    error: String,
}

/// Machine output for a lifecycle command given more than one id.
#[derive(serde::Serialize)]
struct BatchOutcome<T: serde::Serialize> {
    results: Vec<T>,
    errors: Vec<TaskError>,
}

/// Apply a lifecycle command to each id in order.
///
/// With exactly one id the behavior is unchanged: the outcome document is
/// printed (or the human confirmation), and an error propagates as before.
/// With several ids, each failure is reported for that id and the rest still
/// run. Human output prints one confirmation or error line per id as it
/// happens; `--json` prints a single `{"results":[...],"errors":[...]}`
/// document where each result has the single-id shape. The command exits
/// non-zero if any id failed.
fn for_each_task<T: serde::Serialize>(
    ui: &Ui,
    ids: &[i64],
    mut act: impl FnMut(i64) -> Result<T, CliError>,
    human: impl Fn(&T),
) -> Result<(), CliError> {
    let single = ids.len() == 1;
    let mut results = Vec::with_capacity(ids.len());
    let mut errors = Vec::new();
    for &id in ids {
        match act(id) {
            Ok(outcome) => {
                if !ui.json {
                    human(&outcome);
                }
                results.push(outcome);
            }
            Err(error) if single => return Err(error),
            Err(error) => {
                if !ui.json {
                    eprintln!(
                        "{} {} {error}",
                        ui.err.error_label(),
                        ui.err.dim(&format!("#{id}"))
                    );
                }
                errors.push(TaskError {
                    id,
                    error: error.to_string(),
                });
            }
        }
    }
    let failed = errors.len();
    if ui.json {
        match results.as_slice() {
            [only] if single => print_json(only),
            _ => print_json(&BatchOutcome { results, errors }),
        }
    }
    if failed == 0 {
        Ok(())
    } else {
        Err(CliError::message(format!(
            "{failed} of {} tasks failed",
            ids.len()
        )))
    }
}

/// Printed after a claim or start so an agent sees how to keep `PROG`
/// current without reading the skill. The command is ready to paste.
fn print_progress_hint(ui: &Ui, id: i64, token: &str) {
    println!(
        "{}",
        ui.out.dim(&format!(
            "report progress: q log {id} --progress <0-100> --claim-token {token}"
        ))
    );
}

fn confirm(ui: &Ui, verb: &str, id: i64, status: &str, title: &str) {
    println!(
        "{verb} {} [{}] {}",
        ui.out.dim(&format!("#{id}")),
        ui.out.status(status),
        ui.out.bold(title),
    );
}

/// Capture confirmation: a blank line, then exactly one line. The title is
/// collapsed to one line and truncated with an ellipsis like `q ls`.
fn confirm_capture(ui: &Ui, id: i64, status: &str, title: &str) {
    println!();
    println!("{}", capture_line(ui.out, id, status, title));
}

fn capture_line(paint: Paint, id: i64, status: &str, title: &str) -> String {
    format!(
        "captured {} [{}] {}",
        paint.dim(&format!("#{id}")),
        paint.status(status),
        paint.bold(&format_list_title(title)),
    )
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

/// Store maximum. `q top` fetches this many so completions and deletions
/// beyond the visible rows still show up as changes.
const TOP_FETCH_LIMIT: u32 = 500;
const TOP_CHANGE_ROWS: usize = 10;

struct TopOptions {
    interval: std::time::Duration,
    show_terminal: bool,
    limit: usize,
    once: bool,
    /// Redraw in place with ANSI clears. Off when stdout is not a terminal.
    screen: bool,
    /// Put the terminal in raw mode so typed keys are not echoed and `q`
    /// quits. Off when stdin or stdout is not a terminal; Ctrl-C quits then.
    keys: bool,
    /// Heartbeat older than this is flagged stale. The lease sweep is separate.
    stale_after: std::time::Duration,
}

/// One line in the recent-changes list, with the time it was noticed.
struct TopChange {
    at: OffsetDateTime,
    id: i64,
    /// Status before the change. `None` for a task seen for the first time.
    from: Option<TaskStatus>,
    /// Status after the change. `None` when the task was deleted.
    to: Option<TaskStatus>,
    /// Progress after the change, shown next to a non-terminal status.
    progress: Option<u8>,
    title: String,
}

const TOP_CHANGE_NEW: &str = "new";
const TOP_CHANGE_DELETED: &str = "deleted";

impl TopChange {
    fn before_label(&self) -> &str {
        self.from.map(TaskStatus::as_str).unwrap_or(TOP_CHANGE_NEW)
    }

    fn after_label(&self) -> String {
        match (self.to, self.progress) {
            (None, _) => TOP_CHANGE_DELETED.to_string(),
            (Some(status), Some(percent))
                if !matches!(status, TaskStatus::Done | TaskStatus::Cancelled) =>
            {
                format!("{} {percent}%", status.as_str())
            }
            (Some(status), _) => status.as_str().to_string(),
        }
    }
}

async fn run_top(
    queue: &dyn QueueService,
    backend: &Backend,
    filter: &ListFilter,
    options: &TopOptions,
    ui: &Ui,
) -> Result<(), CliError> {
    let mut previous: Option<std::collections::HashMap<i64, TaskSummary>> = None;
    let mut changes: std::collections::VecDeque<TopChange> = std::collections::VecDeque::new();
    let stdout = io::stdout();
    // The guard restores the terminal when it drops: on quit, on error, and
    // while unwinding from a panic. The last frame stays on screen so the
    // final state is still readable.
    let _terminal = TopTerminal::enter(options)?;
    let mut keys = options.keys.then(spawn_key_reader);
    loop {
        let frame = top_frame(
            queue,
            backend,
            filter,
            options,
            ui.out,
            &mut previous,
            &mut changes,
        )?;
        let bytes = if options.screen {
            // Raw mode turns off output post-processing, so a bare newline
            // no longer returns the carriage.
            screen_frame(&frame, options.keys)
        } else {
            frame.clone()
        };
        // One write and one flush per frame: a line-buffered print! would
        // hand the terminal the frame in pieces, which is what flickers.
        let mut out = stdout.lock();
        out.write_all(bytes.as_bytes())?;
        out.flush()?;
        if options.once {
            return Ok(());
        }
        // A key that is not a quit key must not postpone the next redraw, so
        // the deadline is fixed once per frame.
        let deadline = tokio::time::Instant::now() + options.interval;
        let quit = loop {
            tokio::select! {
                _ = tokio::time::sleep_until(deadline) => break false,
                _ = tokio::signal::ctrl_c() => break true,
                key = next_key(&mut keys) => {
                    if is_top_quit_key(&key) {
                        break true;
                    }
                }
            }
        };
        if quit {
            return Ok(());
        }
    }
}

/// Begin and end of synchronized output (DEC private mode 2026). A terminal
/// that supports it holds the frame and swaps it in at once; others ignore
/// the sequences.
const SYNC_BEGIN: &str = "\x1b[?2026h";
const SYNC_END: &str = "\x1b[?2026l";

/// A frame redrawn in place without clearing the screen first. The cursor
/// goes home, every line is overwritten and cleared to its end, and whatever
/// an earlier, taller frame left below is erased once at the end. Clearing
/// the whole screen before drawing is what made the old frames flicker: the
/// terminal showed blank between the clear and the repaint.
fn screen_frame(frame: &str, raw: bool) -> String {
    let newline = if raw { "\r\n" } else { "\n" };
    let mut out = String::with_capacity(frame.len() + 64);
    out.push_str(SYNC_BEGIN);
    out.push_str("\x1b[H");
    for line in frame.lines() {
        out.push_str(line.trim_end_matches('\r'));
        out.push_str("\x1b[K");
        out.push_str(newline);
    }
    out.push_str("\x1b[J");
    out.push_str(SYNC_END);
    out
}

/// Terminal state `q top` changes for the duration of the run: the hidden
/// cursor on a screen, and raw mode when keys are read. Dropping it puts
/// both back, so an error or a panic never leaves the shell without echo.
struct TopTerminal {
    screen: bool,
    raw: bool,
}

impl TopTerminal {
    fn enter(options: &TopOptions) -> Result<Self, CliError> {
        let mut terminal = Self {
            screen: false,
            raw: false,
        };
        if options.screen {
            print!("\x1b[?25l");
            terminal.screen = true;
        }
        if options.keys {
            // Set before the mode change so a failure still drops back
            // through disable_raw_mode, which is harmless when nothing changed.
            terminal.raw = true;
            crossterm::terminal::enable_raw_mode()
                .map_err(|err| CliError::message(format!("cannot read keys: {err}")))?;
        }
        Ok(terminal)
    }
}

impl Drop for TopTerminal {
    fn drop(&mut self) {
        if self.raw {
            let _ = crossterm::terminal::disable_raw_mode();
        }
        if self.screen {
            print!("\x1b[?25h");
        }
        let _ = io::stdout().flush();
    }
}

/// Forward key presses from the terminal to the redraw loop. Reads run on
/// their own thread because crossterm blocks; the thread stops within one
/// poll interval of the receiver going away.
fn spawn_key_reader() -> tokio::sync::mpsc::UnboundedReceiver<KeyEvent> {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    std::thread::spawn(move || {
        while !tx.is_closed() {
            match crossterm::event::poll(std::time::Duration::from_millis(100)) {
                Ok(true) => match crossterm::event::read() {
                    // Windows reports releases and repeats too; act on presses.
                    Ok(Event::Key(key)) if key.kind == KeyEventKind::Press => {
                        if tx.send(key).is_err() {
                            break;
                        }
                    }
                    Ok(_) => {}
                    Err(_) => break,
                },
                Ok(false) => {}
                Err(_) => break,
            }
        }
    });
    rx
}

/// The next key press, or a future that never resolves when keys are not
/// being read, so `select!` can always include it.
async fn next_key(keys: &mut Option<tokio::sync::mpsc::UnboundedReceiver<KeyEvent>>) -> KeyEvent {
    match keys {
        Some(rx) => match rx.recv().await {
            Some(key) => key,
            None => std::future::pending().await,
        },
        None => std::future::pending().await,
    }
}

/// `q` (either case), Esc, and Ctrl-C leave `q top`, like `top` itself.
/// In raw mode Ctrl-C arrives as a key, not a signal.
fn is_top_quit_key(key: &KeyEvent) -> bool {
    match key.code {
        KeyCode::Char('q') | KeyCode::Char('Q') => !key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT),
        KeyCode::Char('c') | KeyCode::Char('C') => key.modifiers.contains(KeyModifiers::CONTROL),
        KeyCode::Esc => true,
        _ => false,
    }
}

/// True when this list filter drops tasks that `q top --all` would still show.
fn top_filter_changes_rows(filter: &ListFilter) -> bool {
    filter.status.is_some()
        || filter.kind.is_some()
        || filter
            .feature
            .as_deref()
            .is_some_and(|text| !text.trim().is_empty())
        || !filter.tags.is_empty()
}

/// The `q top --all` query for this project and repo. Terminal tasks stay in,
/// and status, kind, feature, and tag filters do not.
fn top_layout_list_filter(filter: &ListFilter, limit: u32) -> ListFilter {
    ListFilter {
        status: None,
        project: filter.project.clone(),
        repo: filter.repo.clone(),
        kind: None,
        feature: None,
        limit,
        include_terminal: true,
        tags: Vec::new(),
    }
}

/// Rows that decide `q top` columns and widths: the same window `q top --all`
/// prints. When the active filter already is that query, reuse `tasks`.
fn top_layout_tasks(
    queue: &dyn QueueService,
    filter: &ListFilter,
    tasks: &[TaskSummary],
    limit: usize,
) -> Result<Vec<TaskSummary>, CliError> {
    if !top_filter_changes_rows(filter) {
        return Ok(tasks.iter().take(limit).cloned().collect());
    }
    let limit = u32::try_from(limit).unwrap_or(u32::MAX);
    Ok(queue.list(top_layout_list_filter(filter, limit))?)
}

/// Fetch the queue, record what changed since the last frame, and render.
fn top_frame(
    queue: &dyn QueueService,
    backend: &Backend,
    filter: &ListFilter,
    options: &TopOptions,
    paint: Paint,
    previous: &mut Option<std::collections::HashMap<i64, TaskSummary>>,
    changes: &mut std::collections::VecDeque<TopChange>,
) -> Result<String, CliError> {
    // Same recovery `q claim` and `q serve` use, so an expired lease does not
    // sit in the table as in progress until something else happens to claim.
    queue.recover_stale(q_core::RecoverRequest {
        to: None,
        actor: q_core::Actor::system(),
    })?;
    let status = queue.status()?;
    let tasks = queue.list(filter.clone())?;
    let now = OffsetDateTime::now_utc();
    let current: std::collections::HashMap<i64, TaskSummary> =
        tasks.iter().map(|task| (task.id, task.clone())).collect();
    if let Some(before) = previous.as_ref() {
        for change in top_changes(before, &current, &tasks, now) {
            changes.push_back(change);
        }
        while changes.len() > TOP_CHANGE_ROWS {
            changes.pop_front();
        }
    }
    *previous = Some(current);

    let visible: Vec<TaskSummary> = tasks
        .iter()
        .filter(|task| {
            options.show_terminal
                || !matches!(task.status, TaskStatus::Done | TaskStatus::Cancelled)
        })
        .take(options.limit)
        .cloned()
        .collect();
    // Columns and widths come from the rows `q top --all` would print, not
    // from this mode's subset. A filter then only changes which lines appear.
    let mut layout = top_layout_tasks(queue, filter, &tasks, options.limit)?;
    if layout.is_empty() {
        layout.clone_from(&visible);
    }

    let mut out = String::new();
    out.push_str(&format!(
        "{} {}  {}  every {}  {}\n",
        paint.bold("q top"),
        backend.describe(paint),
        paint.dim(&format_timestamp(now)),
        paint.dim(&format!("{:.1}s", options.interval.as_secs_f64())),
        paint.dim(if options.keys {
            "q quits"
        } else {
            "Ctrl-C quits"
        }),
    ));
    out.push_str(&format!("{}\n\n", render_top_counts(&status, paint)));
    if visible.is_empty() {
        out.push_str("no tasks\n");
    } else {
        out.push_str(&render_task_table_precise(
            &layout,
            &visible,
            paint,
            options.stale_after,
        ));
        out.push('\n');
    }
    out.push_str(&format!("\n{}\n", paint.bold("recent changes")));
    if changes.is_empty() {
        out.push_str(&format!("  {}\n", paint.dim("none yet")));
    }
    out.push_str(&render_top_changes(changes, paint));
    Ok(out)
}

/// Newest first, in aligned columns: time, id, from, to, title.
fn render_top_changes(changes: &std::collections::VecDeque<TopChange>, paint: Paint) -> String {
    let id_width = changes
        .iter()
        .map(|change| change.id.to_string().len() + 1)
        .max()
        .unwrap_or(0);
    let from_width = changes
        .iter()
        .map(|change| change.before_label().len())
        .max()
        .unwrap_or(0);
    let to_width = changes
        .iter()
        .map(|change| change.after_label().len())
        .max()
        .unwrap_or(0);
    let mut out = String::new();
    for change in changes.iter().rev() {
        let id = format!("#{}", change.id);
        let from = change.before_label();
        let to = change.after_label();
        let to = to.as_str();
        out.push_str(&format!(
            "  {}  {}{}  {}{}  {} {}{}  {}\n",
            paint.dim(&format_clock(change.at)),
            " ".repeat(id_width - id.len()),
            paint.dim(&id),
            paint_top_status(from, paint),
            " ".repeat(from_width - from.len()),
            paint.dim("->"),
            paint_top_status(to, paint),
            " ".repeat(to_width - to.len()),
            paint.bold(&change.title),
        ));
    }
    out
}

/// A status name is colored like the table; the `new` and `deleted`
/// pseudo-states are dim.
fn paint_top_status(label: &str, paint: Paint) -> String {
    if label == TOP_CHANGE_NEW || label == TOP_CHANGE_DELETED {
        paint.dim(label)
    } else {
        paint.status(label)
    }
}

fn render_top_counts(status: &q_core::QueueStatus, paint: Paint) -> String {
    let counts = &status.counts;
    let mut parts = Vec::new();
    for (label, count) in [
        ("held", counts.held),
        ("ready", counts.ready),
        ("claimed", counts.claimed),
        ("in_progress", counts.in_progress),
        ("review", counts.review),
        ("blocked", counts.blocked),
        ("escalated", counts.escalated),
        ("done", counts.done),
        ("cancelled", counts.cancelled),
    ] {
        parts.push(format!(
            "{} {}",
            paint.status(label),
            paint.bold(&count.to_string())
        ));
    }
    let expired = if status.expired_claims > 0 {
        paint.paint(
            Style::new().fg_color(Some(anstyle::AnsiColor::Red.into())),
            &format!("{} expired", status.expired_claims),
        )
    } else {
        format!("{} expired", status.expired_claims)
    };
    format!(
        "{}  {}  claims {} active, {}",
        parts.join("  "),
        paint.dim("|"),
        status.active_claims,
        expired
    )
}

/// Describe additions, status moves, and deletions between two fetches.
/// Ordered by the current list first, then deletions.
fn top_changes(
    before: &std::collections::HashMap<i64, TaskSummary>,
    after: &std::collections::HashMap<i64, TaskSummary>,
    order: &[TaskSummary],
    now: OffsetDateTime,
) -> Vec<TopChange> {
    let mut changes = Vec::new();
    for task in order {
        let from = match before.get(&task.id) {
            None => None,
            Some(old) if old.status != task.status || old.progress != task.progress => {
                Some(old.status)
            }
            Some(_) => continue,
        };
        changes.push(TopChange {
            at: now,
            id: task.id,
            from,
            to: Some(task.status),
            progress: task.progress,
            title: format_list_title(&task.title),
        });
    }
    let mut gone: Vec<&TaskSummary> = before
        .values()
        .filter(|task| !after.contains_key(&task.id))
        .collect();
    gone.sort_by_key(|task| task.id);
    for task in gone {
        changes.push(TopChange {
            at: now,
            id: task.id,
            from: Some(task.status),
            to: None,
            progress: None,
            title: format_list_title(&task.title),
        });
    }
    changes
}

fn format_clock(at: OffsetDateTime) -> String {
    let (h, m, s) = at.to_hms();
    format!("{h:02}:{m:02}:{s:02}")
}

fn print_status(backend: &Backend, status: &q_core::QueueStatus, paint: Paint) {
    println!("{}", backend.describe(paint));
    for (label, count) in [
        ("held", status.counts.held),
        ("ready", status.counts.ready),
        ("claimed", status.counts.claimed),
        ("in_progress", status.counts.in_progress),
        ("review", status.counts.review),
        ("blocked", status.counts.blocked),
        ("escalated", status.counts.escalated),
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

/// The task log: one line per event with time, event, who, and detail.
/// Columns are padded so the log reads as a table.
fn print_events(events: &[q_core::Event], paint: Paint, indent: &str) {
    let type_width = events
        .iter()
        .map(|event| event.event_type.len())
        .max()
        .unwrap_or(0);
    let who_width = events
        .iter()
        .map(|event| event_actor(event).len())
        .max()
        .unwrap_or(0);
    for event in events {
        let who = event_actor(event);
        let detail = event_detail(event, paint);
        println!(
            "{indent}{}  {}{}  {}{}  {}",
            paint.dim(&format_timestamp(event.created_at)),
            paint.bold(&event.event_type),
            " ".repeat(type_width - event.event_type.len()),
            paint.dim(&who),
            " ".repeat(who_width - who.len()),
            detail,
        );
    }
}

/// `agent:claude-01`, `human:pierric`, or `system:q`.
fn event_actor(event: &q_core::Event) -> String {
    match event.actor_id.as_deref().filter(|id| !id.is_empty()) {
        Some(id) => format!("{}:{id}", event.actor_type),
        None => event.actor_type.clone(),
    }
}

/// What the event did: a status move, a note, an artifact, or a branch.
fn event_detail(event: &q_core::Event, paint: Paint) -> String {
    let payload = &event.payload;
    let field = |key: &str| payload.get(key).and_then(|value| value.as_str());
    let mut parts = Vec::new();
    if let (Some(from), Some(to)) = (field("from"), field("to")) {
        parts.push(format!(
            "{} {} {}",
            paint.status(from),
            paint.dim("->"),
            paint.status(to)
        ));
    }
    if let Some(percent) = payload.get("progress").and_then(|value| value.as_u64()) {
        parts.push(format!("progress {percent}%"));
    }
    if let Some(message) = field("message").or_else(|| field("note")) {
        if !message.is_empty() {
            parts.push(message.split_whitespace().collect::<Vec<_>>().join(" "));
        }
    }
    if let Some(count) = payload
        .get("failure_count")
        .and_then(|value| value.as_u64())
    {
        parts.push(format!("failures {count}"));
    }
    if event.event_type == "task_escalated" {
        if let Some(reason) = field("reason") {
            if !reason.is_empty() {
                parts.push(reason.to_string());
            }
        }
    }
    if let (Some(kind), Some(value)) = (field("kind"), field("value")) {
        let id = payload
            .get("artifact_id")
            .and_then(|value| value.as_i64())
            .map(|id| paint.dim(&format!(" (artifact {id})")))
            .unwrap_or_default();
        parts.push(format!("{kind}: {value}{id}"));
    }
    if let Some(summary) = field("summary") {
        if !summary.is_empty() {
            parts.push(summary.to_string());
        }
    }
    if let Some(branch) = field("branch") {
        parts.push(format!("branch {branch}"));
    }
    if let Some(agent) = field("agent_id") {
        if event.actor_id.as_deref() != Some(agent) {
            parts.push(format!("agent {agent}"));
        }
    }
    parts.join("  ")
}

/// Short form used in confirmations: `event detail`.
fn describe_event(event: &q_core::Event, paint: Paint) -> String {
    format!("{} {}", event.event_type, event_detail(event, paint))
}

const TITLE_MAX_CHARS: usize = 64;
const UNASSIGNED_PROJECT: &str = "(none)";

#[derive(Clone)]
struct TaskListRow {
    id: String,
    status: String,
    feature: String,
    project: String,
    priority: String,
    /// Percent complete as `40%`, or blank.
    progress: String,
    updated: String,
    /// Newest `pr` artifact value. Shown as a clickable `PR` on a terminal.
    pr_url: Option<String>,
    tags: String,
    /// Blank when the task has never failed.
    fails: String,
    model: String,
    host: String,
    note: String,
    beat: String,
    /// `stale` when the active heartbeat is older than the threshold.
    stale: String,
    /// Who, when, and why, while the task is escalated. Blank otherwise.
    escalated: String,
    title: String,
    /// What the claiming agent last said it was doing, with its age. Blank
    /// when nothing was reported; the column only appears when some row
    /// has one.
    activity: String,
}

fn print_task_list(tasks: &[TaskSummary], paint: Paint) {
    if tasks.is_empty() {
        println!("no tasks");
        return;
    }
    println!("{}", render_task_table(tasks, paint));
}

/// The table as `q top` draws it: ages tick by the second.
/// `layout_tasks` fix the columns and widths (the `q top --all` window);
/// `tasks` are the rows drawn under that header.
fn render_task_table_precise(
    layout_tasks: &[TaskSummary],
    tasks: &[TaskSummary],
    paint: Paint,
    stale_after: std::time::Duration,
) -> String {
    let now = OffsetDateTime::now_utc();
    let layout_rows: Vec<TaskListRow> = layout_tasks
        .iter()
        .map(|task| task_list_row_with(task, now, true, Some(stale_after)))
        .collect();
    let rows: Vec<TaskListRow> = tasks
        .iter()
        .map(|task| task_list_row_with(task, now, true, Some(stale_after)))
        .collect();
    render_task_rows_with_layout(&layout_rows, &rows, paint, true)
}

fn render_task_table(tasks: &[TaskSummary], paint: Paint) -> String {
    let now = OffsetDateTime::now_utc();
    let rows: Vec<TaskListRow> = tasks.iter().map(|task| task_list_row(task, now)).collect();
    render_task_rows_painted(&rows, paint, false)
}

fn task_list_row(task: &TaskSummary, now: OffsetDateTime) -> TaskListRow {
    task_list_row_with(task, now, false, None)
}

/// `precise` shows ages to the second (`12s ago`) for a live view.
/// `stale_after` flags an active heartbeat older than that. `None` leaves the
/// stale cell blank (`q ls` does not flag workers).
fn task_list_row_with(
    task: &TaskSummary,
    now: OffsetDateTime,
    precise: bool,
    stale_after: Option<std::time::Duration>,
) -> TaskListRow {
    let updated = if precise {
        style::format_relative_precise(task.updated_at, now)
    } else {
        style::format_relative(task.updated_at, now)
    };
    let active = task.heartbeat_at.is_some();
    let (beat, stale) = heartbeat_cells(task.heartbeat_at, now, precise, stale_after);
    TaskListRow {
        id: task.id.to_string(),
        status: task.status.to_string(),
        feature: display_project(task.feature.as_deref()),
        project: display_project(task.project.as_deref()),
        priority: task.priority.to_string(),
        progress: format_progress(task.progress),
        updated,
        pr_url: task.pr_url.clone(),
        tags: truncate_chars(&task.tags.join(","), 32),
        fails: if task.failure_count == 0 {
            String::new()
        } else {
            task.failure_count.to_string()
        },
        model: if active {
            task.agent_model.clone().unwrap_or_default()
        } else {
            String::new()
        },
        host: if active {
            task.agent_host.clone().unwrap_or_default()
        } else {
            String::new()
        },
        note: if active {
            task.latest_note
                .as_deref()
                .map(|text| truncate_chars(&format_list_title(text), 40))
                .unwrap_or_default()
        } else {
            String::new()
        },
        beat,
        stale,
        escalated: escalation_cell(task, now, precise),
        title: format_list_title(&task.title),
        activity: format_activity(task.activity.as_deref(), task.activity_at, now),
    }
}

const ACTIVITY_MAX_CHARS: usize = 40;

/// `Bash: cargo test (12s)`: the agent's last reported activity and how long
/// ago it was reported. Blank when the active claim never reported one.
fn format_activity(
    activity: Option<&str>,
    at: Option<OffsetDateTime>,
    now: OffsetDateTime,
) -> String {
    match (activity, at) {
        (Some(text), Some(at)) => {
            let age = style::format_relative(at, now);
            let age = age.strip_suffix(" ago").unwrap_or(&age).to_string();
            format!("{} ({age})", truncate_chars(text, ACTIVITY_MAX_CHARS))
        }
        (Some(text), None) => truncate_chars(text, ACTIVITY_MAX_CHARS),
        _ => String::new(),
    }
}

/// `agent:bot · 3s ago · too big`, or blank when the task is not escalated.
fn escalation_cell(task: &TaskSummary, now: OffsetDateTime, precise: bool) -> String {
    if task.status != TaskStatus::Escalated {
        return String::new();
    }
    let who = task.escalated_by.as_deref().unwrap_or("-");
    let when = match task.escalated_at {
        Some(at) if precise => style::format_relative_precise(at, now),
        Some(at) => style::format_relative(at, now),
        None => "-".to_string(),
    };
    let reason = task
        .escalated_reason
        .as_deref()
        .map(format_list_title)
        .filter(|text| !text.is_empty())
        .unwrap_or_else(|| "-".to_string());
    truncate_chars(&format!("{who} · {when} · {reason}"), 48)
}

/// Heartbeat age, and `stale` when it is older than the threshold.
fn heartbeat_cells(
    heartbeat_at: Option<OffsetDateTime>,
    now: OffsetDateTime,
    precise: bool,
    stale_after: Option<std::time::Duration>,
) -> (String, String) {
    let Some(at) = heartbeat_at else {
        return (String::new(), String::new());
    };
    let beat = if precise {
        style::format_relative_precise(at, now)
    } else {
        style::format_relative(at, now)
    };
    let stale = match stale_after {
        Some(limit) => {
            let age = now.unix_timestamp().saturating_sub(at.unix_timestamp());
            if age >= i64::try_from(limit.as_secs()).unwrap_or(i64::MAX) {
                "stale".to_string()
            } else {
                String::new()
            }
        }
        None => String::new(),
    };
    (beat, stale)
}

const PR_LABEL: &str = "PR";

/// What the PR column shows: `PR` on a terminal (linked), the URL when piped.
fn pr_cell(row: &TaskListRow, paint: Paint) -> &str {
    match &row.pr_url {
        Some(url) => paint.link_text(PR_LABEL, url),
        None => "",
    }
}

fn format_progress(progress: Option<u8>) -> String {
    progress
        .map(|percent| format!("{percent}%"))
        .unwrap_or_default()
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
    render_task_rows_painted(rows, Paint::plain(), false)
}

/// One column of the task table. `fixed` columns always show; the others
/// appear only when some layout row has a real value, so a narrow terminal is not
/// spent on columns that are blank for every task in that set.
struct TaskColumn {
    header: &'static str,
    align_right: bool,
    fixed: bool,
    /// Values that count as empty for this column, besides the blank string.
    placeholders: &'static [&'static str],
}

const TASK_COLUMNS: &[TaskColumn] = &[
    TaskColumn {
        header: "ID",
        align_right: true,
        fixed: true,
        placeholders: &[],
    },
    TaskColumn {
        header: "STATUS",
        align_right: false,
        fixed: true,
        placeholders: &[],
    },
    TaskColumn {
        header: "FEATURE",
        align_right: false,
        fixed: false,
        placeholders: &[UNASSIGNED_PROJECT],
    },
    TaskColumn {
        header: "PROJECT",
        align_right: false,
        fixed: false,
        placeholders: &[UNASSIGNED_PROJECT],
    },
    TaskColumn {
        header: "PRI",
        align_right: true,
        fixed: false,
        placeholders: &["0"],
    },
    TaskColumn {
        header: "PROG",
        align_right: true,
        fixed: false,
        placeholders: &[],
    },
    TaskColumn {
        header: "UPDATED",
        align_right: false,
        fixed: true,
        placeholders: &[],
    },
    TaskColumn {
        header: "TITLE",
        align_right: false,
        fixed: true,
        placeholders: &[],
    },
    TaskColumn {
        header: "PR",
        align_right: false,
        fixed: false,
        placeholders: &[],
    },
    TaskColumn {
        header: "TAGS",
        align_right: false,
        fixed: false,
        placeholders: &[],
    },
    TaskColumn {
        header: "FAILS",
        align_right: true,
        fixed: false,
        placeholders: &["0"],
    },
    TaskColumn {
        header: "MODEL",
        align_right: false,
        fixed: false,
        placeholders: &[],
    },
    TaskColumn {
        header: "HOST",
        align_right: false,
        fixed: false,
        placeholders: &[],
    },
    TaskColumn {
        header: "NOTE",
        align_right: false,
        fixed: false,
        placeholders: &[],
    },
    TaskColumn {
        header: "BEAT",
        align_right: false,
        fixed: false,
        placeholders: &[],
    },
    TaskColumn {
        header: "STALE",
        align_right: false,
        fixed: false,
        placeholders: &[],
    },
    TaskColumn {
        header: "ESCALATED",
        align_right: false,
        fixed: false,
        placeholders: &[],
    },
    TaskColumn {
        header: "ACTIVITY",
        align_right: false,
        fixed: false,
        placeholders: &[],
    },
];

/// Columns that only the wide (`q top`) table considers.
const WIDE_ONLY: &[&str] = &[
    "FAILS",
    "MODEL",
    "HOST",
    "NOTE",
    "BEAT",
    "STALE",
    "ESCALATED",
];

/// A row's cells in `TASK_COLUMNS` order: text, style, and an optional link.
fn task_cells(row: &TaskListRow, paint: Paint) -> Vec<(&str, Style, Option<&str>)> {
    let dim = style::dim_style();
    let status = style::status_style(&row.status);
    let stale = Style::new()
        .bold()
        .fg_color(Some(anstyle::AnsiColor::Red.into()));
    vec![
        (row.id.as_str(), dim, None),
        (row.status.as_str(), status, None),
        (row.feature.as_str(), dim, None),
        (row.project.as_str(), dim, None),
        (row.priority.as_str(), dim, None),
        (row.progress.as_str(), status, None),
        (row.updated.as_str(), dim, None),
        (row.title.as_str(), style::bold_style(), None),
        (pr_cell(row, paint), Style::new(), row.pr_url.as_deref()),
        (row.tags.as_str(), dim, None),
        (row.fails.as_str(), dim, None),
        (row.model.as_str(), dim, None),
        (row.host.as_str(), dim, None),
        (row.note.as_str(), Style::new(), None),
        (row.beat.as_str(), dim, None),
        (row.stale.as_str(), stale, None),
        (
            row.escalated.as_str(),
            style::status_style("escalated"),
            None,
        ),
        (row.activity.as_str(), Style::new(), None),
    ]
}

fn render_task_rows_painted(rows: &[TaskListRow], paint: Paint, wide: bool) -> String {
    render_task_rows_with_layout(rows, rows, paint, wide)
}

/// `layout_rows` decide which columns exist and how wide they are. `rows` are
/// the lines printed under that header. `q ls` passes the same slice for both.
/// `q top` passes the `q top --all` rows as the layout and the filtered rows
/// as `rows`, so a mode cannot grow or drop a column on its own.
fn render_task_rows_with_layout(
    layout_rows: &[TaskListRow],
    rows: &[TaskListRow],
    paint: Paint,
    wide: bool,
) -> String {
    let layout: Vec<Vec<(&str, Style, Option<&str>)>> = layout_rows
        .iter()
        .map(|row| task_cells(row, paint))
        .collect();
    let table: Vec<Vec<(&str, Style, Option<&str>)>> =
        rows.iter().map(|row| task_cells(row, paint)).collect();
    let shown: Vec<usize> = TASK_COLUMNS
        .iter()
        .enumerate()
        .filter(|(index, column)| {
            if !wide && WIDE_ONLY.contains(&column.header) {
                return false;
            }
            column.fixed
                || layout.iter().any(|cells| {
                    let text = cells[*index].0;
                    !text.is_empty() && !column.placeholders.contains(&text)
                })
        })
        .map(|(index, _)| index)
        .collect();
    let headers: Vec<&str> = shown.iter().map(|&i| TASK_COLUMNS[i].header).collect();
    let align_right: Vec<bool> = shown.iter().map(|&i| TASK_COLUMNS[i].align_right).collect();
    let widths: Vec<usize> = shown
        .iter()
        .map(|&i| {
            column_width(
                TASK_COLUMNS[i].header,
                layout.iter().map(|cells| cells[i].0),
            )
        })
        .collect();
    let header_styles = vec![style::dim_style(); shown.len()];
    let mut lines = Vec::with_capacity(rows.len() + 1);
    lines.push(format_task_line(
        &headers,
        &header_styles,
        &vec![None; shown.len()],
        &widths,
        &align_right,
        paint,
    ));
    for cells in &table {
        let texts: Vec<&str> = shown.iter().map(|&i| cells[i].0).collect();
        let styles: Vec<Style> = shown.iter().map(|&i| cells[i].1).collect();
        let links: Vec<Option<&str>> = shown.iter().map(|&i| cells[i].2).collect();
        lines.push(format_task_line(
            &texts,
            &styles,
            &links,
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

/// One table line. A cell with a link is wrapped as a hyperlink instead of
/// styled; its visible text is still `cells[index]`, so padding is unchanged.
fn format_task_line(
    cells: &[&str],
    styles: &[Style],
    links: &[Option<&str>],
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
        let painted = match links.get(index).copied().flatten() {
            Some(url) => paint.link(cell, url),
            None => paint.paint(styles[index], cell),
        };
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
        format_task_line(
            &headers,
            &header_styles,
            &[None; 4],
            &widths,
            &align_right,
            paint
        )
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
                &[None; 4],
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
    if let Some(percent) = task.progress {
        meta(paint, &format!("progress: {percent}%"));
    }
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
    let tags = if task.tags.is_empty() {
        "-".to_string()
    } else {
        task.tags.join(", ")
    };
    meta(paint, &format!("tags: {tags}"));
    meta(paint, &format!("failures: {}", task.failure_count));
    if task.status == q_core::TaskStatus::Escalated {
        meta(
            paint,
            &format!(
                "escalated_reason: {}",
                task.escalated_reason.as_deref().unwrap_or("-")
            ),
        );
        meta(
            paint,
            &format!(
                "escalated_by: {}",
                task.escalated_by.as_deref().unwrap_or("-")
            ),
        );
        let when = task
            .escalated_at
            .map(format_timestamp)
            .unwrap_or_else(|| "-".to_string());
        meta(paint, &format!("escalated_at: {when}"));
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
                "model: {}  host: {}",
                claim.agent_model.as_deref().unwrap_or("-"),
                claim.agent_host.as_deref().unwrap_or("-"),
            ),
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
        if let Some(activity) = &claim.activity {
            meta(
                paint,
                &format!(
                    "activity: {activity} ({})",
                    style::format_relative(claim.heartbeat_at, OffsetDateTime::now_utc())
                ),
            );
        }
    }
    if !detail.artifacts.is_empty() {
        println!("\n{}", paint.bold("artifacts:"));
        for artifact in &detail.artifacts {
            let stored = match artifact.content_bytes {
                Some(bytes) => paint.dim(&format!(
                    "  ({bytes} bytes stored, q artifact {})",
                    artifact.id
                )),
                None => String::new(),
            };
            let value = if artifact.kind == "pr" || artifact.value.starts_with("http") {
                paint.link(&artifact.value, &artifact.value)
            } else {
                artifact.value.clone()
            };
            println!(
                "- {} {}: {value}{stored}",
                paint.dim(&format!("#{}", artifact.id)),
                artifact.kind,
            );
        }
    }
    let notes: Vec<&q_core::Event> = detail
        .events
        .iter()
        .filter(|event| event.event_type == q_core::NOTE_EVENT)
        .filter(|event| {
            event
                .payload
                .get("message")
                .and_then(|value| value.as_str())
                .is_some_and(|message| !message.trim().is_empty())
        })
        .collect();
    if !notes.is_empty() {
        println!("\n{}", paint.bold("notes:"));
        for event in notes {
            let message = event.payload["message"].as_str().unwrap_or_default();
            println!(
                "  {}  {}",
                paint.dim(&format_timestamp(event.created_at)),
                message
            );
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
        capture_line, display_project, format_elapsed, format_list_title, heartbeat_cells,
        is_top_quit_key, orbit_options, parse_age, render_task_rows, render_task_rows_painted,
        render_task_rows_with_layout, render_tree, render_tree_with, screen_frame, shell_join,
        truncate_chars, TaskListRow, TITLE_MAX_CHARS,
    };
    use crate::style::Paint;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use q_core::{TaskStatus, TaskTree, TreeFeature, TreeNode};

    #[test]
    fn capture_confirmation_is_one_truncated_line() {
        let short = capture_line(
            Paint::plain(),
            184,
            "held",
            "Benchmark trace encoding variants",
        );
        assert_eq!(
            short,
            "captured #184 [held] Benchmark trace encoding variants"
        );

        let long = format!("first line\nsecond {}", "x".repeat(TITLE_MAX_CHARS + 20));
        let line = capture_line(Paint::plain(), 7, "held", &long);
        assert_eq!(line.lines().count(), 1, "{line}");
        assert!(
            line.starts_with("captured #7 [held] first line second x"),
            "{line}"
        );
        assert!(line.ends_with('…'), "{line}");
        let title = line.trim_start_matches("captured #7 [held] ");
        assert_eq!(title.chars().count(), TITLE_MAX_CHARS);
    }

    #[test]
    fn top_quits_on_q_esc_and_ctrl_c_only() {
        let plain = |code| KeyEvent::new(code, KeyModifiers::NONE);
        assert!(is_top_quit_key(&plain(KeyCode::Char('q'))));
        assert!(is_top_quit_key(&KeyEvent::new(
            KeyCode::Char('Q'),
            KeyModifiers::SHIFT
        )));
        assert!(is_top_quit_key(&plain(KeyCode::Esc)));
        assert!(is_top_quit_key(&KeyEvent::new(
            KeyCode::Char('c'),
            KeyModifiers::CONTROL
        )));

        assert!(!is_top_quit_key(&plain(KeyCode::Char('c'))));
        assert!(!is_top_quit_key(&plain(KeyCode::Char('a'))));
        assert!(!is_top_quit_key(&plain(KeyCode::Enter)));
        assert!(!is_top_quit_key(&plain(KeyCode::Char(' '))));
        assert!(
            !is_top_quit_key(&KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL)),
            "Ctrl-Q is not a quit key"
        );
    }

    #[test]
    fn screen_frames_overwrite_in_place_instead_of_clearing() {
        let out = screen_frame("a\nbb\n\nc\n", false);
        assert!(out.starts_with("\x1b[?2026h\x1b[H"), "{out:?}");
        assert!(out.ends_with("\x1b[J\x1b[?2026l"), "{out:?}");
        assert!(!out.contains("\x1b[2J"), "no full-screen clear: {out:?}");
        assert_eq!(
            out,
            "\x1b[?2026h\x1b[Ha\x1b[K\nbb\x1b[K\n\x1b[K\nc\x1b[K\n\x1b[J\x1b[?2026l"
        );
        let raw = screen_frame("a\nb\n", true);
        assert!(raw.contains("a\x1b[K\r\nb\x1b[K\r\n"), "{raw:?}");
        assert!(!raw.contains("\n\n"), "{raw:?}");
    }

    #[test]
    fn exec_notes_quote_the_command_and_keep_the_duration_one_token() {
        let words = |args: &[&str]| args.iter().map(|a| a.to_string()).collect::<Vec<_>>();
        assert_eq!(
            shell_join(&words(&["cargo", "build", "--locked"])),
            "cargo build --locked"
        );
        assert_eq!(
            shell_join(&words(&["git", "commit", "-m", "Fix the parser", "--", ""])),
            "git commit -m 'Fix the parser' -- ''"
        );
        assert_eq!(
            shell_join(&words(&["sh", "-c", "echo it's $HOME"])),
            "sh -c \"echo it's \\$HOME\""
        );
        let ms = std::time::Duration::from_millis;
        assert_eq!(format_elapsed(ms(48)), "48ms");
        assert_eq!(format_elapsed(ms(1_240)), "1.2s");
        assert_eq!(format_elapsed(ms(59_960)), "60.0s");
        assert_eq!(format_elapsed(ms(187_000)), "3m07s");
        assert_eq!(format_elapsed(ms(3_720_000)), "1h02m");
        for text in [
            format_elapsed(ms(48)),
            format_elapsed(ms(187_000)),
            format_elapsed(ms(3_720_000)),
        ] {
            assert!(!text.contains(' '), "{text}");
        }
    }

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
                status: "held".into(),
                feature: "rollout".into(),
                project: "alpha".into(),
                priority: "0".into(),
                progress: "".into(),
                updated: "2026-09-22T20:00:00Z".into(),
                pr_url: None,
                tags: String::new(),
                fails: String::new(),
                model: String::new(),
                host: String::new(),
                note: String::new(),
                beat: String::new(),
                stale: String::new(),
                escalated: String::new(),
                title: "Short".into(),
                activity: String::new(),
            },
            TaskListRow {
                id: "3".into(),
                status: "in_progress".into(),
                feature: "(none)".into(),
                project: "(none)".into(),
                priority: "10".into(),
                progress: "".into(),
                updated: "2026-09-22T19:00:00Z".into(),
                pr_url: None,
                tags: String::new(),
                fails: String::new(),
                model: String::new(),
                host: String::new(),
                note: String::new(),
                beat: String::new(),
                stale: String::new(),
                escalated: String::new(),
                title: format_list_title(&long),
                activity: String::new(),
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
                status: "held".into(),
                feature: "(none)".into(),
                project: "alpha".into(),
                priority: "0".into(),
                progress: "".into(),
                updated: "3m ago".into(),
                pr_url: None,
                tags: String::new(),
                fails: String::new(),
                model: String::new(),
                host: String::new(),
                note: String::new(),
                beat: String::new(),
                stale: String::new(),
                escalated: String::new(),
                title: "Keep the held item".into(),
                activity: String::new(),
            },
            TaskListRow {
                id: "2".into(),
                status: "ready".into(),
                feature: "(none)".into(),
                project: "beta".into(),
                priority: "1".into(),
                progress: "".into(),
                updated: "1h ago".into(),
                pr_url: None,
                tags: String::new(),
                fails: String::new(),
                model: String::new(),
                host: String::new(),
                note: String::new(),
                beat: String::new(),
                stale: String::new(),
                escalated: String::new(),
                title: "Compare encodings".into(),
                activity: String::new(),
            },
            TaskListRow {
                id: "1".into(),
                status: "held".into(),
                feature: "(none)".into(),
                project: "(none)".into(),
                priority: "0".into(),
                progress: "".into(),
                updated: "2d ago".into(),
                pr_url: None,
                tags: String::new(),
                fails: String::new(),
                model: String::new(),
                host: String::new(),
                note: String::new(),
                beat: String::new(),
                stale: String::new(),
                escalated: String::new(),
                title: "Unassigned capture".into(),
                activity: String::new(),
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
ID  STATUS  PROJECT  PRI  UPDATED  TITLE
 4  held    alpha      0  3m ago   Keep the held item
 2  ready   beta       1  1h ago   Compare encodings
 1  held    (none)     0  2d ago   Unassigned capture"
        );
    }

    #[test]
    fn empty_columns_are_hidden_and_fixed_ones_stay() {
        let mut row = TaskListRow {
            id: "7".into(),
            status: "ready".into(),
            feature: "(none)".into(),
            project: "(none)".into(),
            priority: "0".into(),
            progress: String::new(),
            updated: "just now".into(),
            pr_url: None,
            tags: String::new(),
            fails: String::new(),
            model: String::new(),
            host: String::new(),
            note: String::new(),
            beat: String::new(),
            stale: String::new(),
            escalated: String::new(),
            title: "Bare".into(),
            activity: String::new(),
        };
        let bare = render_task_rows(std::slice::from_ref(&row));
        assert_eq!(
            bare.lines().next().unwrap().trim_end(),
            "ID  STATUS  UPDATED   TITLE"
        );
        row.priority = "2".into();
        row.feature = "Rollout".into();
        let some = render_task_rows(std::slice::from_ref(&row));
        assert_eq!(
            some.lines().next().unwrap().trim_end(),
            "ID  STATUS  FEATURE  PRI  UPDATED   TITLE"
        );
        // Wide-only columns never show in the ls table even when set.
        row.fails = "3".into();
        assert!(!render_task_rows(std::slice::from_ref(&row)).contains("FAILS"));
        assert!(
            render_task_rows_painted(std::slice::from_ref(&row), Paint::plain(), true)
                .contains("FAILS")
        );
    }

    #[test]
    fn pr_column_links_on_a_terminal_and_prints_the_url_when_plain() {
        let mut row = TaskListRow {
            id: "9".into(),
            status: "done".into(),
            feature: "(none)".into(),
            project: "alpha".into(),
            priority: "0".into(),
            progress: "100%".into(),
            updated: "1h ago".into(),
            pr_url: Some("https://example.com/pr/9".into()),
            tags: String::new(),
            fails: String::new(),
            model: String::new(),
            host: String::new(),
            note: String::new(),
            beat: String::new(),
            stale: String::new(),
            escalated: String::new(),
            title: "Shipped".into(),
            activity: String::new(),
        };
        let plain = render_task_rows(std::slice::from_ref(&row));
        assert!(plain.contains("UPDATED  TITLE    PR"), "{plain}");
        assert!(
            plain.contains("1h ago   Shipped  https://example.com/pr/9"),
            "{plain}"
        );
        let color = render_task_rows_painted(std::slice::from_ref(&row), Paint::color(), false);
        assert!(
            color.contains("\x1b]8;;https://example.com/pr/9\x1b\\"),
            "{color:?}"
        );
        let visible = anstream::adapter::strip_str(&color).to_string();
        assert!(visible.contains("UPDATED  TITLE    PR"), "{visible}");
        assert!(visible.contains("1h ago   Shipped  PR"), "{visible}");
        row.pr_url = None;
        let none = render_task_rows(std::slice::from_ref(&row));
        assert!(none.contains("1h ago   Shipped"), "{none}");
        assert!(
            !none
                .lines()
                .next()
                .unwrap()
                .split_whitespace()
                .any(|column| column == "PR"),
            "PR column hides without a link: {none}"
        );
    }

    #[test]
    fn top_table_flags_stale_heartbeats_and_shows_identity_notes_and_fails() {
        use time::OffsetDateTime;
        let now = OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap();
        let old = OffsetDateTime::from_unix_timestamp(1_700_000_000 - 180).unwrap();
        let recent = OffsetDateTime::from_unix_timestamp(1_700_000_000 - 30).unwrap();
        let limit = std::time::Duration::from_secs(120);
        let (beat, stale) = heartbeat_cells(Some(old), now, true, Some(limit));
        assert_eq!(stale, "stale");
        assert!(beat.contains("ago"), "{beat}");
        let (_, fresh) = heartbeat_cells(Some(recent), now, true, Some(limit));
        assert!(fresh.is_empty(), "{fresh}");
        let (_, unflagged) = heartbeat_cells(Some(old), now, false, None);
        assert!(unflagged.is_empty());

        let row = TaskListRow {
            id: "7".into(),
            status: "in_progress".into(),
            feature: "(none)".into(),
            project: "alpha".into(),
            priority: "0".into(),
            progress: "40%".into(),
            updated: "3m ago".into(),
            pr_url: None,
            tags: "rust".into(),
            fails: "2".into(),
            model: "opus".into(),
            host: "worker-a".into(),
            note: "running tests".into(),
            beat: "3m ago".into(),
            stale: "stale".into(),
            escalated: String::new(),
            title: "Fix the parser".into(),
            activity: String::new(),
        };
        let table = render_task_rows_painted(std::slice::from_ref(&row), Paint::plain(), true);
        let header = table.lines().next().unwrap();
        // The blank ESCALATED column is hidden; the rest keep their order.
        assert!(!header.contains("ESCALATED"), "{header}");
        for (left, right) in [
            ("TAGS", "FAILS"),
            ("FAILS", "MODEL"),
            ("MODEL", "HOST"),
            ("HOST", "NOTE"),
            ("NOTE", "BEAT"),
            ("BEAT", "STALE"),
            ("UPDATED", "TITLE"),
            ("TITLE", "TAGS"),
        ] {
            assert!(
                header.find(left).unwrap() < header.find(right).unwrap(),
                "{header}"
            );
        }
        assert!(table.contains("opus"), "{table}");
        assert!(table.contains("worker-a"), "{table}");
        assert!(table.contains("running tests"), "{table}");
        assert!(table.contains("stale"), "{table}");
        assert!(table.contains("  2  "), "{table}");
        let colored = render_task_rows_painted(std::slice::from_ref(&row), Paint::color(), true);
        assert!(colored.contains("\u{1b}["));
        assert!(anstream::adapter::strip_str(&colored)
            .to_string()
            .contains("stale"));
    }

    #[test]
    fn activity_column_appears_only_when_reported() {
        let mut row = TaskListRow {
            id: "3".into(),
            status: "in_progress".into(),
            feature: "(none)".into(),
            project: "alpha".into(),
            priority: "0".into(),
            progress: "40%".into(),
            updated: "just now".into(),
            pr_url: None,
            tags: String::new(),
            fails: String::new(),
            model: String::new(),
            host: String::new(),
            note: String::new(),
            beat: String::new(),
            stale: String::new(),
            escalated: String::new(),
            title: "Port the encoder".into(),
            activity: String::new(),
        };
        let quiet = render_task_rows(std::slice::from_ref(&row));
        assert!(!quiet.contains("ACTIVITY"), "{quiet}");
        row.activity = "Bash: cargo test (12s)".into();
        let busy = render_task_rows(std::slice::from_ref(&row));
        assert!(busy.contains("TITLE             ACTIVITY"), "{busy}");
        assert!(
            busy.contains("Port the encoder  Bash: cargo test (12s)"),
            "{busy}"
        );
        let now = time::macros::datetime!(2026-09-27 10:00:00 UTC);
        assert_eq!(
            super::format_activity(
                Some("Bash:   cargo   test"),
                Some(now - time::Duration::seconds(12)),
                now
            ),
            "Bash:   cargo   test (just now)"
        );
        assert_eq!(
            super::format_activity(
                Some("Read main.rs"),
                Some(now - time::Duration::minutes(3)),
                now
            ),
            "Read main.rs (3m)"
        );
        assert_eq!(super::format_activity(None, None, now), "");
    }

    #[test]
    fn color_does_not_change_visible_table_or_tree() {
        let rows = vec![TaskListRow {
            id: "2".into(),
            status: "ready".into(),
            feature: "(none)".into(),
            project: "beta".into(),
            priority: "1".into(),
            progress: "".into(),
            updated: "3m ago".into(),
            pr_url: None,
            tags: String::new(),
            fails: String::new(),
            model: String::new(),
            host: String::new(),
            note: String::new(),
            beat: String::new(),
            stale: String::new(),
            escalated: String::new(),
            title: "Compare encodings".into(),
            activity: String::new(),
        }];
        let plain = render_task_rows(&rows);
        let colored = render_task_rows_painted(&rows, crate::style::Paint::color(), false);
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
            status: TaskStatus::Held,
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
            status: TaskStatus::Held,
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
                status: TaskStatus::Held,
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
#3  held         Ship the rollout  [api]
├── #1  done         Shared schema  [db]  {Other}  (external)
│   └── #4  held         Add the types  [api]  (already shown)
└── #2  ready        Write the schema  [api]
    └── #4  held         Add the types  [api]

#5  blocked      Write the notes
└── #3  held         Ship the rollout  [api]  (cycle)"
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

    #[test]
    fn top_rows_use_the_all_layout_including_progress() {
        fn row(id: &str, status: &str, title: &str) -> TaskListRow {
            TaskListRow {
                id: id.into(),
                status: status.into(),
                feature: "(none)".into(),
                project: "alpha".into(),
                priority: "0".into(),
                progress: String::new(),
                updated: "1s ago".into(),
                pr_url: None,
                tags: String::new(),
                fails: String::new(),
                model: String::new(),
                host: String::new(),
                note: String::new(),
                beat: String::new(),
                stale: String::new(),
                escalated: String::new(),
                title: title.into(),
                activity: String::new(),
            }
        }

        let mut active = row("1", "in_progress", "Active work");
        active.progress = "40%".into();
        active.host = "cursor".into();
        active.note = "halfway".into();
        active.beat = "1s ago".into();
        let mut done = row("2", "done", "Finished work");
        done.progress = "100%".into();
        done.pr_url = Some("https://example.com/pr/2".into());
        let mut escalated = row("3", "escalated", "Needs a human");
        escalated.escalated = "agent:bot · 1s ago · too big".into();
        let layout = [escalated.clone(), done, active.clone()];
        let reference = render_task_rows_painted(&layout, Paint::plain(), true);
        let active_table = render_task_rows_with_layout(
            &layout,
            std::slice::from_ref(&active),
            Paint::plain(),
            true,
        );
        let escalated_table = render_task_rows_with_layout(
            &layout,
            std::slice::from_ref(&escalated),
            Paint::plain(),
            true,
        );
        let ref_lines: Vec<&str> = reference.lines().collect();
        assert_eq!(
            active_table.lines().next(),
            Some(ref_lines[0]),
            "{reference}"
        );
        assert_eq!(
            escalated_table.lines().next(),
            Some(ref_lines[0]),
            "{reference}"
        );
        assert!(ref_lines[0]
            .split_whitespace()
            .any(|column| column == "PROG"));
        assert!(ref_lines[0].split_whitespace().any(|column| column == "PR"));
        let ref_active = ref_lines
            .iter()
            .copied()
            .find(|line| line.contains("Active work"))
            .unwrap();
        let ref_escalated = ref_lines
            .iter()
            .copied()
            .find(|line| line.contains("Needs a human"))
            .unwrap();
        assert_eq!(active_table.lines().nth(1), Some(ref_active));
        assert_eq!(escalated_table.lines().nth(1), Some(ref_escalated));
        assert!(ref_active.contains("40%"), "{ref_active}");
        // No percent of its own, but the column stays because the done task is 100%.
        let prog_at = char_index(ref_lines[0], "PROG");
        let prog_cell: String = chars_at(ref_escalated, prog_at).chars().take(4).collect();
        assert_eq!(prog_cell.trim(), "", "{ref_escalated}");

        let alone = render_task_rows_painted(std::slice::from_ref(&active), Paint::plain(), true);
        let alone_header = alone.lines().next().unwrap();
        assert!(
            !alone_header.split_whitespace().any(|column| column == "PR"),
            "{alone_header}"
        );
        assert_ne!(Some(alone_header), Some(ref_lines[0]));

        let colored = render_task_rows_painted(&layout, Paint::color(), true);
        let colored_active = render_task_rows_with_layout(
            &layout,
            std::slice::from_ref(&active),
            Paint::color(),
            true,
        );
        let colored_line = colored
            .lines()
            .find(|line| line.contains("Active work"))
            .unwrap();
        assert_eq!(colored_active.lines().next(), colored.lines().next());
        assert_eq!(colored_active.lines().nth(1), Some(colored_line));
        assert!(colored_line.contains('\u{1b}'), "{colored_line:?}");
    }

    #[test]
    fn orbit_history_ages_parse() {
        let secs = |value: &str| parse_age(value).unwrap().map(|d| d.as_secs());
        assert_eq!(secs("0"), Some(0));
        assert_eq!(secs("90"), Some(90));
        assert_eq!(secs("30m"), Some(1800));
        assert_eq!(secs("1h"), Some(3600));
        assert_eq!(secs("2d"), Some(172_800));
        assert_eq!(secs("1.5h"), Some(5400));
        assert_eq!(secs(" all "), None);
        assert!(parse_age("soon").is_err());
        assert!(parse_age("1w").is_err());
        assert!(parse_age("-1h").is_err());
        let options = orbit_options(2.0, "1h", 0.0, true).unwrap();
        assert_eq!(options.mapper.segment, None);
        assert!(options.once);
        assert!(orbit_options(0.0, "1h", 10.0, false).is_err());
        assert!(orbit_options(2.0, "1h", -1.0, false).is_err());
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
