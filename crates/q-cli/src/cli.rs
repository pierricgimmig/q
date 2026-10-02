use std::path::PathBuf;

use clap::{Parser, Subcommand, ValueEnum};

/// Environment variable that stands in for the task id on `q log` and `q exec`.
pub const TASK_ID_ENV: &str = "Q_TASK_ID";
/// Environment variable that stands in for `--claim-token` on `q log` and `q exec`.
pub const CLAIM_TOKEN_ENV: &str = "Q_CLAIM_TOKEN";

pub fn preprocess(args: Vec<String>) -> Vec<String> {
    let task_env = std::env::var(TASK_ID_ENV)
        .ok()
        .filter(|value| !value.trim().is_empty());
    preprocess_with(args, task_env.as_deref())
}

/// Rewrite the raw arguments: a bare title becomes `add`, and `log` or
/// `exec` without a task id gets `task_env` (`$Q_TASK_ID`) inserted, so an
/// agent that exports its task can write `q log "note"` and `q exec -- cmd`.
pub fn preprocess_with(mut args: Vec<String>, task_env: Option<&str>) -> Vec<String> {
    if let Some(index) = first_positional(&args) {
        let word = &args[index];
        if word != "--" && is_command(word) {
            if let (true, Some(task)) = (takes_task_from_env(word), task_env) {
                let after = index + 1;
                let next = first_positional(&args[after..]).map(|offset| after + offset);
                let has_id = next
                    .map(|at| args[at].parse::<i64>().is_ok())
                    .unwrap_or(false);
                if !has_id {
                    args.insert(next.unwrap_or(args.len()), task.trim().to_string());
                }
            }
            return args;
        }
        args.insert(0, "add".into());
    }
    args
}

fn takes_task_from_env(word: &str) -> bool {
    matches!(word, "log" | "exec")
}

fn first_positional(args: &[String]) -> Option<usize> {
    let mut index = 0;
    while index < args.len() {
        let arg = &args[index];
        if arg == "--" {
            return Some(index);
        }
        if let Some(stripped) = arg.strip_prefix('-') {
            if arg.contains('=') || is_bool_flag(arg) {
                index += 1;
                continue;
            }
            if !stripped.starts_with('-') && arg.len() > 2 {
                index += 1;
                continue;
            }
            if is_value_flag(arg) {
                index += 2;
                continue;
            }
            index += 1;
            continue;
        }
        return Some(index);
    }
    None
}

fn is_command(word: &str) -> bool {
    matches!(
        word,
        "add"
            | "ls"
            | "list"
            | "top"
            | "show"
            | "tree"
            | "edit"
            | "ready"
            | "hold"
            | "block"
            | "cancel"
            | "delete"
            | "claim"
            | "heartbeat"
            | "start"
            | "complete"
            | "release"
            | "status"
            | "recover-stale"
            | "events"
            | "reopen"
            | "log"
            | "exec"
            | "artifact"
            | "project"
            | "feature"
            | "mcp"
            | "serve"
            | "token"
            | "orbit"
            | "skill"
            | "workers"
            | "fail"
            | "escalate"
            | "note"
            | "help"
            | "done"
            | "rm"
            | "recover"
            | "canceled"
    )
}

fn is_bool_flag(arg: &str) -> bool {
    matches!(
        arg,
        "--json"
            | "-j"
            | "--hold"
            | "-w"
            | "--wait"
            | "--edit"
            | "-e"
            | "--once"
            | "--yes"
            | "--force"
            | "--all"
            | "-a"
            | "--help"
            | "-h"
            | "--version"
            | "-V"
            | "--clear-project"
            | "--clear-repo"
            | "--clear-agent-pool"
            | "--clear-feature"
            | "--clear-tags"
            | "--escalated"
    )
}

fn is_value_flag(arg: &str) -> bool {
    matches!(
        arg,
        "--db"
            | "--server"
            | "--token"
            | "--bind"
            | "--auth"
            | "--public-url"
            | "--role"
            | "-C"
            | "--directory"
            | "--repo"
            | "--project"
            | "--kind"
            | "--priority"
            | "--risk"
            | "--status"
            | "--agent"
            | "--claim-token"
            | "--summary"
            | "--branch"
            | "--worktree"
            | "--lease-minutes"
            | "--max-risk"
            | "--body"
            | "--title"
            | "--agent-pool"
            | "--depends-on"
            | "--limit"
            | "--interval"
            | "--body-file"
            | "--to"
            | "--artifact"
            | "--attach"
            | "--progress"
            | "--activity"
            | "--target"
            | "--capability"
            | "--capabilities"
            | "--allowed-kind"
            | "--set-project"
            | "--set-repo"
            | "--feature"
            | "--color"
            | "--tag"
            | "--model"
            | "--host"
            | "--max-failures"
            | "--stale-after"
            | "--sweep-interval"
            | "--thread"
    )
}

/// When to color human stdout and stderr.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, ValueEnum)]
pub enum ColorMode {
    /// Color on a terminal. Off when piped, when `NO_COLOR` is set, or when `CLICOLOR=0`.
    #[default]
    Auto,
    /// ANSI color even when piped or when `NO_COLOR` is set.
    Always,
    /// Never emit ANSI color.
    Never,
}

#[derive(Debug, Parser)]
#[command(
    name = "q",
    version,
    about = "Local-first context-aware agent work queue",
    arg_required_else_help = true
)]
pub struct Cli {
    /// SQLite database path. Defaults to $XDG_DATA_HOME/q/queue.db. Ignored with --server.
    #[arg(long, global = true, value_name = "PATH")]
    pub db: Option<PathBuf>,

    /// URL of a `q serve` authority, for example http://127.0.0.1:7777. Falls back to $Q_SERVER_URL.
    #[arg(long, global = true, value_name = "URL")]
    pub server: Option<String>,

    /// Bearer token for --server. Falls back to $Q_SERVER_TOKEN.
    #[arg(long, global = true, value_name = "TOKEN")]
    pub token: Option<String>,

    /// Print machine-readable JSON on stdout. Logs stay on stderr.
    ///
    /// JSON and `q mcp` are never colored.
    #[arg(short = 'j', long, global = true)]
    pub json: bool,

    /// Color for human output: auto, always, or never.
    ///
    /// auto colors a terminal and turns color off when stdout is piped, when
    /// NO_COLOR is non-empty, or when CLICOLOR=0. CLICOLOR_FORCE turns color on
    /// even through a pipe. always forces ANSI, including over NO_COLOR. never
    /// disables color. JSON and `q mcp` ignore this flag.
    #[arg(long, global = true, value_enum, default_value_t, value_name = "WHEN")]
    pub color: ColorMode,

    /// Directory used for project discovery. Does not change the parent shell.
    #[arg(short = 'C', long = "directory", global = true, value_name = "DIR")]
    pub directory: Option<PathBuf>,

    /// Explicit repository identity. Overrides discovery. On `claim`/`ls`, filters by repo.
    #[arg(long, global = true, value_name = "REPO")]
    pub repo: Option<String>,

    /// Explicit project name. Overrides discovery. On `claim`/`ls`, filters by project.
    #[arg(long, global = true, value_name = "NAME")]
    pub project: Option<String>,

    #[command(subcommand)]
    pub command: Commands,
}

#[derive(Debug, Subcommand)]
pub enum Commands {
    /// Capture a task. It is ready for agents at once unless --hold is set.
    Add {
        /// Title words. Quoted text is the usual form: q "Fix the bug".
        #[arg(required = true, num_args = 1.., value_name = "TITLE")]
        title: Vec<String>,
        /// implementation, research, review, benchmark, documentation, or other.
        #[arg(long, value_name = "KIND")]
        kind: Option<String>,
        /// Higher numbers are more urgent. Default 0.
        #[arg(long)]
        priority: Option<i32>,
        /// low, medium, high, or external_action. Default low.
        #[arg(long, value_name = "RISK")]
        risk: Option<String>,
        /// Markdown body. Any shape is accepted; no sections are required.
        #[arg(long)]
        body: Option<String>,
        /// Read the body from a file instead of --body.
        #[arg(long, value_name = "PATH")]
        body_file: Option<PathBuf>,
        /// Open $VISUAL or $EDITOR on the body before capture.
        ///
        /// Starts from --body or --body-file when given, otherwise from a
        /// template of suggested sections.
        #[arg(short = 'e', long)]
        edit: bool,
        /// Required capability. Repeatable. Comma-separated values are split.
        #[arg(long = "capability")]
        capability: Vec<String>,
        /// Only agents in this pool may claim the task.
        #[arg(long, value_name = "POOL")]
        agent_pool: Option<String>,
        /// Comma-separated task ids that must be done first.
        #[arg(long, value_name = "IDS")]
        depends_on: Option<String>,
        /// Feature id or unique title.
        #[arg(long, value_name = "ID|TITLE")]
        feature: Option<String>,
        /// Label stored on the task. Repeatable. Commas are split.
        #[arg(long = "tag", value_name = "TAG")]
        tag: Vec<String>,
        /// Keep the task held, out of the claimable pool, until `q ready ID`.
        ///
        /// Use this for work that needs a human look before an agent may
        /// start it. `-w` and `--wait` are aliases.
        #[arg(short = 'w', long, visible_alias = "wait")]
        hold: bool,
    },
    /// List tasks. Done and cancelled are hidden unless --all or --status is set.
    #[command(visible_alias = "list")]
    Ls {
        /// Show only this status. Includes done or cancelled when that status is named.
        ///
        /// Statuses: held, ready, claimed, in_progress, review, blocked, escalated, done, cancelled.
        #[arg(long, value_name = "STATUS", conflicts_with = "escalated")]
        status: Option<String>,
        /// Show only tasks a worker escalated for human review.
        #[arg(long)]
        escalated: bool,
        /// implementation, research, review, benchmark, documentation, or other.
        #[arg(long, value_name = "KIND")]
        kind: Option<String>,
        /// Maximum rows.
        #[arg(short = 'n', long, default_value_t = 100)]
        limit: u32,
        /// Include done and cancelled tasks. Ignored when --status is set.
        #[arg(short = 'a', long)]
        all: bool,
        /// Show only tasks in this feature. Id or unique title.
        #[arg(long, value_name = "ID|TITLE")]
        feature: Option<String>,
        /// Keep tasks that carry every one of these tags. Repeatable.
        #[arg(long = "tag", value_name = "TAG")]
        tag: Vec<String>,
    },
    /// Watch the queue. Redraws counts, the task table, and recent changes; press q to quit.
    Top {
        /// Seconds between refreshes. Default 1, so ages tick by the second.
        #[arg(short = 'i', long, default_value_t = 1.0, value_name = "SECONDS")]
        interval: f64,
        /// Show only this status. Includes done or cancelled when that status is named.
        #[arg(long, value_name = "STATUS", conflicts_with = "escalated")]
        status: Option<String>,
        /// Show only tasks a worker escalated for human review.
        #[arg(long)]
        escalated: bool,
        /// implementation, research, review, benchmark, documentation, or other.
        #[arg(long, value_name = "KIND")]
        kind: Option<String>,
        /// Maximum rows in the table.
        #[arg(short = 'n', long, default_value_t = 30)]
        limit: u32,
        /// Include done and cancelled tasks in the table. Ignored when --status is set.
        #[arg(short = 'a', long)]
        all: bool,
        /// Show only tasks in this feature. Id or unique title.
        #[arg(long, value_name = "ID|TITLE")]
        feature: Option<String>,
        /// Keep tasks that carry every one of these tags. Repeatable.
        #[arg(long = "tag", value_name = "TAG")]
        tag: Vec<String>,
        /// Flag a worker whose last heartbeat is older than this many seconds. Default 120.
        #[arg(long, default_value_t = q_core::DEFAULT_STALE_HEARTBEAT_SECS, value_name = "SECONDS")]
        stale_after: u64,
        /// Draw one frame and exit instead of refreshing.
        #[arg(long)]
        once: bool,
    },
    /// Show one task, its claim, artifacts, and recent events.
    Show {
        /// Task id.
        id: i64,
    },
    /// Show what must be done before a task, or before the tasks in a feature.
    ///
    /// Children are dependencies (do these first). A repeated node is marked
    /// already shown. With --feature, tasks outside that feature are marked external.
    Tree {
        /// Task to root the tree at.
        #[arg(value_name = "TASK_ID", required_unless_present = "feature")]
        id: Option<i64>,
        /// Feature id or unique title. One tree per task that nothing else in the feature depends on.
        #[arg(
            long,
            value_name = "ID|TITLE",
            required_unless_present = "id",
            conflicts_with = "id"
        )]
        feature: Option<String>,
    },
    /// Edit task fields. With no flags, open $VISUAL or $EDITOR on the body.
    Edit {
        /// Task id.
        id: i64,
        /// Replace the title.
        #[arg(long)]
        title: Option<String>,
        /// Replace the body.
        #[arg(long)]
        body: Option<String>,
        /// Read a replacement body from a file.
        #[arg(long, value_name = "PATH")]
        body_file: Option<PathBuf>,
        /// Open $VISUAL or $EDITOR on the body, even when other flags are set.
        #[arg(short = 'e', long)]
        edit: bool,
        /// implementation, research, review, benchmark, documentation, or other.
        #[arg(long, value_name = "KIND")]
        kind: Option<String>,
        /// Replace the priority.
        #[arg(long)]
        priority: Option<i32>,
        /// low, medium, high, or external_action.
        #[arg(long, value_name = "RISK")]
        risk: Option<String>,
        /// Replace required capabilities. Repeatable. Commas are split.
        #[arg(long = "capability")]
        capability: Vec<String>,
        /// Replace the agent pool.
        #[arg(long, value_name = "POOL")]
        agent_pool: Option<String>,
        /// Replace dependencies with these comma-separated task ids.
        #[arg(long, value_name = "IDS")]
        depends_on: Option<String>,
        /// Clear the project name.
        #[arg(long)]
        clear_project: bool,
        /// Clear the repository identity.
        #[arg(long)]
        clear_repo: bool,
        /// Clear the agent pool.
        #[arg(long)]
        clear_agent_pool: bool,
        /// Feature id or unique title.
        #[arg(long, value_name = "ID|TITLE")]
        feature: Option<String>,
        /// Detach the task from its feature.
        #[arg(long)]
        clear_feature: bool,
        /// Replace tags. Repeatable. Commas are split. An empty value is ignored; use --clear-tags.
        #[arg(long = "tag", value_name = "TAG")]
        tag: Vec<String>,
        /// Remove every tag.
        #[arg(long)]
        clear_tags: bool,
    },
    /// Move one or more tasks to ready so agents may claim them. Releases held work.
    ///
    /// Ids are processed in order. A failure on one id is reported and the
    /// rest still run; the exit status is non-zero if any id failed.
    Ready {
        /// Task ids.
        #[arg(required = true, value_name = "ID", num_args = 1..)]
        ids: Vec<i64>,
    },
    /// Move a ready or blocked task back to held so agents cannot claim it.
    Hold {
        /// Task id.
        id: i64,
    },
    /// Block a task. Claimed work also requires --claim-token.
    Block {
        /// Task id.
        id: i64,
        /// Token from `q claim`. Required when the task is claimed or in progress.
        #[arg(long)]
        claim_token: Option<String>,
    },
    /// Cancel tasks that are held, ready, or blocked. The rows and their history stay.
    ///
    /// Ids are processed in order; one failure does not stop the rest.
    #[command(visible_alias = "canceled")]
    Cancel {
        /// Task ids.
        #[arg(required = true, value_name = "ID", num_args = 1..)]
        ids: Vec<i64>,
    },
    /// Hard-delete tasks and their claims, events, artifacts, and dependency rows.
    ///
    /// Unlike cancel, nothing remains in the queue database. Events cascade with
    /// the task and are not retained. An unexpired claim requires --force.
    /// Ids are processed in order; one failure does not stop the rest.
    #[command(visible_alias = "rm")]
    Delete {
        /// Task ids.
        #[arg(required = true, value_name = "ID", num_args = 1..)]
        ids: Vec<i64>,
        /// Delete even when an unexpired claim is held. Clears that claim in the same transaction.
        #[arg(long)]
        force: bool,
    },
    /// Atomically claim one eligible ready task, or a specific one by id.
    Claim {
        /// Claim this task instead of the best eligible one. It must be ready
        /// and pass the same filters; otherwise the command fails and says why.
        id: Option<i64>,
        /// Worker id recorded on the claim.
        #[arg(long)]
        agent: String,
        /// Capability this worker has. Repeatable. Commas are split.
        #[arg(long = "capability")]
        capability: Vec<String>,
        /// Kind this worker accepts. Repeatable. Default: any kind.
        #[arg(long = "kind")]
        kind: Vec<String>,
        /// Default medium. High and external_action are excluded unless raised explicitly.
        #[arg(long, default_value = "medium")]
        max_risk: String,
        /// Lease length in minutes, measured from the last heartbeat. Default 30. Minimum 1, maximum 1440.
        #[arg(long)]
        lease_minutes: Option<u64>,
        /// Only claim a task in this pool.
        #[arg(long, value_name = "POOL")]
        agent_pool: Option<String>,
        /// Model name recorded on the claim. Falls back to $Q_AGENT_MODEL.
        #[arg(long, value_name = "NAME")]
        model: Option<String>,
        /// Hostname recorded on the claim. Defaults to this machine. $Q_AGENT_HOST overrides that.
        #[arg(long, value_name = "HOST")]
        host: Option<String>,
        /// Claim only tasks that carry every one of these tags. Repeatable.
        #[arg(long = "tag", value_name = "TAG")]
        tag: Vec<String>,
        /// Skip tasks that have already failed this many times. Omit for no cap.
        #[arg(long, value_name = "N")]
        max_failures: Option<u32>,
    },
    /// Release the claim and park the task for a human. It is not claimable until `q ready`.
    ///
    /// Use this when the task is too big, or you lack the tools or context.
    /// A genuine execution failure uses `q fail` instead. The reason is required.
    Escalate {
        /// Task id.
        id: i64,
        /// Why a human should look at this before another agent claims it.
        #[arg(value_name = "REASON")]
        reason: String,
        /// Token printed by `q claim`.
        #[arg(long)]
        claim_token: String,
    },
    /// Release a claim, record an optional note, and return the task to ready.
    ///
    /// The failure count increments. Another agent can claim the task. The
    /// claim token is required. The note is optional.
    Fail {
        /// Task id.
        id: i64,
        /// Optional short note stored on the failure event.
        #[arg(value_name = "NOTE")]
        note: Option<String>,
        /// Token printed by `q claim`.
        #[arg(long)]
        claim_token: String,
    },
    /// Append a short status line to a claimed task.
    Note {
        /// Task id.
        id: i64,
        /// Status line, such as `running tests`.
        #[arg(value_name = "MESSAGE")]
        message: String,
        /// Token printed by `q claim`.
        #[arg(long)]
        claim_token: String,
    },
    /// Extend the lease for a matching, unexpired claim token.
    Heartbeat {
        /// Task id.
        id: i64,
        /// Token printed by `q claim`.
        #[arg(long)]
        claim_token: String,
        /// New lease length in minutes. Default 30.
        #[arg(long)]
        lease_minutes: Option<u64>,
        /// What you are doing right now, for example "Bash: cargo test".
        /// Shown in q top and q show until the next heartbeat replaces it.
        #[arg(long, value_name = "TEXT")]
        activity: Option<String>,
    },
    /// Mark claimed work in progress.
    Start {
        /// Task id.
        id: i64,
        /// Token printed by `q claim`.
        #[arg(long)]
        claim_token: String,
        /// Branch name to record on the claim.
        #[arg(long)]
        branch: Option<String>,
        /// Worktree path to record on the claim.
        #[arg(long)]
        worktree: Option<String>,
    },
    /// Complete claimed work, or accept a task that is already in review.
    #[command(visible_alias = "done")]
    Complete {
        /// Task id.
        id: i64,
        /// Token printed by `q claim`. Omit when accepting a task already in review.
        #[arg(long)]
        claim_token: Option<String>,
        /// Short note stored on the completion event.
        #[arg(long, default_value = "")]
        summary: String,
        #[arg(long, value_name = "review|done")]
        status: Option<String>,
        /// Repeatable kind=value artifact, for example --artifact pr=https://...
        #[arg(long = "artifact")]
        artifact: Vec<String>,
        /// Store a file's text in the database as an artifact. Repeatable.
        /// KIND=PATH or PATH (kind defaults to report).
        #[arg(long = "attach", value_name = "[KIND=]PATH")]
        attach: Vec<PathBuf>,
    },
    /// Return claimed work to ready.
    Release {
        /// Task id.
        id: i64,
        /// Token printed by `q claim`.
        #[arg(long)]
        claim_token: String,
    },
    /// Show queue counts and claim lease health.
    Status,
    /// Requeue expired claims and record a recovery event.
    #[command(visible_alias = "recover")]
    RecoverStale {
        /// ready or blocked. Defaults to each project's stale policy.
        #[arg(long, value_name = "ready|blocked")]
        to: Option<String>,
    },
    /// Show the append-only event log for a task.
    Events {
        /// Task id.
        id: i64,
    },
    /// Reopen done work to ready, or cancelled work to held.
    ///
    /// Ids are processed in order; one failure does not stop the rest.
    Reopen {
        /// Task ids.
        #[arg(required = true, value_name = "ID", num_args = 1..)]
        ids: Vec<i64>,
    },
    /// Append a note or artifacts to a task's log, or print the log.
    ///
    /// With a message or --artifact/--attach, append an entry. With neither,
    /// print every event for the task, oldest first: time, event, who, detail.
    /// The id may be left out when $Q_TASK_ID is set.
    Log {
        /// Task id. Defaults to $Q_TASK_ID.
        id: i64,
        /// Note text, such as a thinking step or progress update.
        #[arg(value_name = "MESSAGE")]
        message: Option<String>,
        /// Token printed by `q claim`. Attributes the entry to that agent.
        #[arg(long, env = CLAIM_TOKEN_ENV, hide_env_values = true)]
        claim_token: Option<String>,
        /// Percent complete, 0 to 100. Shown in the PROG column of q ls and q top.
        #[arg(long, value_name = "PERCENT", value_parser = clap::value_parser!(u8).range(0..=100))]
        progress: Option<u8>,
        /// Repeatable kind=value artifact, for example --artifact pr=https://...
        #[arg(long = "artifact")]
        artifact: Vec<String>,
        /// Store a file's text in the database as an artifact. Repeatable.
        /// KIND=PATH or PATH (kind defaults to report).
        #[arg(long = "attach", value_name = "[KIND=]PATH")]
        attach: Vec<PathBuf>,
    },
    /// Run a command and record it on the task's log as a launched process.
    ///
    /// Writes `@exec <command line>` before the command runs and `@exit <code>
    /// (<duration>) <command line>` after it ends, so `q orbit` draws the
    /// process as a scope under the agent's thread. The command's stdin,
    /// stdout, and stderr pass through unchanged and its exit status is
    /// returned. The id may be left out when $Q_TASK_ID is set; put `--`
    /// before the command.
    Exec {
        /// Task id. Defaults to $Q_TASK_ID.
        id: i64,
        /// Token printed by `q claim`. Attributes the entries to that agent.
        #[arg(long, env = CLAIM_TOKEN_ENV, hide_env_values = true)]
        claim_token: Option<String>,
        /// File the process under a sub-thread of the agent, as `[NAME]` does
        /// in a note. No spaces.
        #[arg(long, value_name = "NAME")]
        thread: Option<String>,
        /// The command and its arguments.
        #[arg(
            required = true,
            trailing_var_arg = true,
            allow_hyphen_values = true,
            value_name = "COMMAND"
        )]
        command: Vec<String>,
    },
    /// Print an artifact's stored content. Ids are shown by `q show`.
    Artifact {
        /// Artifact id.
        id: i64,
    },
    /// Named groups of tasks that may span repos.
    Feature {
        #[command(subcommand)]
        command: FeatureCommand,
    },
    /// Project discovery and .agentqueue.toml.
    Project {
        #[command(subcommand)]
        command: ProjectCommand,
    },
    /// Serve the queue as an MCP server on stdio.
    Mcp,
    /// Serve the local database over HTTP for remote agents and chat connectors.
    Serve {
        /// Address to listen on. Non-loopback addresses require a token file.
        #[arg(long, default_value = "127.0.0.1:7777", value_name = "ADDR")]
        bind: String,
        /// Token file with `[[tokens]]` entries. Defaults to tokens.toml next to the database when that file exists.
        #[arg(long, value_name = "FILE")]
        auth: Option<PathBuf>,
        /// Public origin, for example `https://q.example.com`. Used in OAuth metadata and redirects.
        /// Also defines the accepted browser Origin. Defaults to the listener origin;
        /// set this explicitly behind a reverse proxy. Agents need no Origin header.
        #[arg(long, value_name = "URL")]
        public_url: Option<String>,
        /// Seconds between sweeps that release expired claim leases. Default 15.
        /// Zero disables the sweep. The lease length itself is `--lease-minutes` on claim.
        #[arg(long, default_value_t = q_core::DEFAULT_LEASE_SWEEP_SECS, value_name = "SECONDS")]
        sweep_interval: u64,
    },
    /// Manage the token file that q serve authenticates with.
    Token {
        #[command(subcommand)]
        command: TokenCommand,
    },
    /// Spawn a job-stealing pool of q workers in the current herdr workspace.
    ///
    /// There is no dispatcher. Each worker claims the next ready task on its own.
    Workers {
        #[command(subcommand)]
        command: WorkersCommand,
    },
    /// Follow queue activity live in the Orbit profiler.
    ///
    /// Tails the event log and posts tasks as processes, agents as threads,
    /// and events as spans to a running Orbit service (POST /api/events).
    Orbit {
        /// Orbit service URL. Falls back to $Q_ORBIT_URL, then http://127.0.0.1:44766.
        #[arg(long, value_name = "URL")]
        url: Option<String>,
        /// Seconds between polls of the queue.
        #[arg(short = 'i', long, default_value_t = 2.0, value_name = "SECONDS")]
        interval: f64,
        /// Draw this much history at start: a duration such as 1h, 30m, 2d, or all.
        ///
        /// Older events still build the state, so open work is drawn from its
        /// real start. 0 draws nothing from the past.
        #[arg(long, default_value = "1h", value_name = "AGE")]
        history: String,
        /// Seconds between segments of a span that is still open. 0 draws a
        /// span only when it ends.
        #[arg(long, default_value_t = 10.0, value_name = "SECONDS")]
        segment: f64,
        /// Do one pass (replay, push) and exit.
        #[arg(long)]
        once: bool,
    },
    /// Print or install the agent skill for q.
    Skill {
        #[command(subcommand)]
        command: Option<SkillCommand>,
    },
}

#[derive(Debug, Subcommand)]
pub enum WorkersCommand {
    /// Open a herdr tab named "workers": N agent panes, and q top along the bottom.
    ///
    /// The panes form a roughly square grid. A full-width `q top` pane sits under
    /// that grid, filtered to the current project. Each agent runs the worker loop
    /// on its own (`worker-1` .. `worker-N`). Requires a herdr pane unless `--dry-run`.
    Spawn {
        /// How many workers to start.
        #[arg(value_name = "N", value_parser = crate::workers::parse_worker_count)]
        count: u32,
        /// Print the herdr commands and the grid. Does not require herdr.
        #[arg(long)]
        dry_run: bool,
        /// Herdr agent kind (`herdr agent start --kind`). Defaults to $Q_WORKER_AGENT or claude.
        #[arg(long, value_name = "KIND")]
        agent: Option<String>,
        /// Extra argument for the agent executable, after `--`. Repeatable.
        #[arg(long = "agent-arg", value_name = "ARG", allow_hyphen_values = true)]
        agent_arg: Vec<String>,
        /// Terminal width in columns, used to shape the grid.
        ///
        /// Default on a real run: the herdr tab area, else $COLUMNS, else 120.
        /// `--dry-run` skips herdr and uses $COLUMNS or 120 unless this is set.
        #[arg(long, value_name = "COLS", value_parser = crate::workers::parse_term_extent)]
        columns: Option<u32>,
        /// Terminal height in rows, used to shape the grid.
        ///
        /// Default on a real run: the herdr tab area, else $LINES, else 40.
        /// `--dry-run` skips herdr and uses $LINES or 40 unless this is set.
        #[arg(long, value_name = "ROWS", value_parser = crate::workers::parse_term_extent)]
        rows: Option<u32>,
    },
}

#[derive(Debug, Subcommand)]
pub enum SkillCommand {
    /// Install the q skill into common agent skill directories.
    Install {
        /// Limit install to agents, claude, cursor, codex, or all. Repeatable. Default: all.
        #[arg(long = "target", value_name = "NAME")]
        target: Vec<String>,
        /// Accepted for scripts. Existing SKILL.md files are overwritten either way.
        #[arg(long)]
        force: bool,
    },
}

#[derive(Debug, Subcommand)]
pub enum TokenCommand {
    /// Add a token with a fresh secret and print the secret once.
    Create {
        /// Who this token is for, for example pierric or codex-vps.
        name: String,
        /// human or agent. Agents cannot mark work ready.
        #[arg(long, default_value = "agent", value_name = "ROLE")]
        role: String,
        /// Token file. Defaults to tokens.toml next to the database.
        #[arg(long, value_name = "FILE")]
        auth: Option<PathBuf>,
    },
    /// List token names and roles. Secrets are never printed.
    Ls {
        #[arg(long, value_name = "FILE")]
        auth: Option<PathBuf>,
    },
    /// Remove a token. Connectors signed in with it stop working.
    Revoke {
        name: String,
        #[arg(long, value_name = "FILE")]
        auth: Option<PathBuf>,
    },
}

#[derive(Debug, Subcommand)]
pub enum FeatureCommand {
    /// Create a feature.
    Create {
        /// Title words. Quoted text is the usual form.
        #[arg(required = true, num_args = 1.., value_name = "TITLE")]
        title: Vec<String>,
        /// Optional description.
        #[arg(long)]
        body: Option<String>,
    },
    /// List features.
    #[command(visible_alias = "list")]
    Ls,
    /// Show one feature.
    Show {
        /// Feature id.
        id: i64,
    },
    /// Edit a feature title or body.
    Edit {
        /// Feature id.
        id: i64,
        /// Replace the title.
        #[arg(long)]
        title: Option<String>,
        /// Replace the body.
        #[arg(long)]
        body: Option<String>,
    },
    /// Delete a feature. Tasks stay; their feature is cleared.
    Delete {
        /// Feature id.
        id: i64,
    },
}

#[derive(Debug, Subcommand)]
pub enum ProjectCommand {
    /// Write .agentqueue.toml at the git root.
    Init {
        /// Write the file without prompting. Required when stdin is not a terminal.
        #[arg(long)]
        yes: bool,
        /// Overwrite an existing .agentqueue.toml.
        #[arg(long)]
        force: bool,
    },
    /// Print the project context discovered from the working directory.
    Show,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bare_title_becomes_add() {
        let args = preprocess(vec![
            "--json".into(),
            "Fix trace timestamp overflow".into(),
            "--kind".into(),
            "research".into(),
        ]);
        assert_eq!(
            args,
            vec![
                "add",
                "--json",
                "Fix trace timestamp overflow",
                "--kind",
                "research"
            ]
        );
    }

    #[test]
    fn workers_spawn_is_not_rewritten_as_capture() {
        let args = preprocess(vec![
            "workers".into(),
            "spawn".into(),
            "8".into(),
            "--dry-run".into(),
        ]);
        assert_eq!(args, vec!["workers", "spawn", "8", "--dry-run"]);
    }

    #[test]
    fn skill_is_not_rewritten_as_capture() {
        let args = preprocess(vec![
            "skill".into(),
            "install".into(),
            "--target".into(),
            "agents".into(),
        ]);
        assert_eq!(args, vec!["skill", "install", "--target", "agents"]);
    }

    #[test]
    fn delete_is_not_rewritten_as_capture() {
        let args = preprocess(vec!["delete".into(), "12".into(), "--force".into()]);
        assert_eq!(args, vec!["delete", "12", "--force"]);
    }

    #[test]
    fn known_commands_are_left_alone() {
        let args = preprocess(vec![
            "--db".into(),
            "/tmp/q.db".into(),
            "ls".into(),
            "--status".into(),
            "held".into(),
        ]);
        assert_eq!(args, vec!["--db", "/tmp/q.db", "ls", "--status", "held"]);
    }

    #[test]
    fn hold_flag_does_not_hide_the_bare_title() {
        let args = preprocess(vec!["--hold".into(), "Risky migration".into()]);
        assert_eq!(args, vec!["add", "--hold", "Risky migration"]);
        let args = preprocess(vec!["-w".into(), "Risky migration".into()]);
        assert_eq!(args, vec!["add", "-w", "Risky migration"]);
        let args = preprocess(vec!["hold".into(), "12".into()]);
        assert_eq!(args, vec!["hold", "12"]);
    }

    #[test]
    fn list_all_is_not_rewritten_as_capture() {
        let args = preprocess(vec!["list".into(), "--all".into()]);
        assert_eq!(args, vec!["list", "--all"]);
    }

    #[test]
    fn feature_is_not_rewritten_as_capture() {
        let args = preprocess(vec![
            "feature".into(),
            "create".into(),
            "Cross-repo rollout".into(),
            "--body".into(),
            "Ship it".into(),
        ]);
        assert_eq!(
            args,
            vec![
                "feature",
                "create",
                "Cross-repo rollout",
                "--body",
                "Ship it"
            ]
        );
    }

    #[test]
    fn tree_is_not_rewritten_as_capture() {
        let args = preprocess(vec![
            "tree".into(),
            "--feature".into(),
            "Cross-repo rollout".into(),
        ]);
        assert_eq!(args, vec!["tree", "--feature", "Cross-repo rollout"]);
    }

    #[test]
    fn tree_accepts_a_task_or_a_feature() {
        let task = Cli::try_parse_from(["q", "tree", "12"]).unwrap();
        match task.command {
            Commands::Tree { id, feature } => {
                assert_eq!(id, Some(12));
                assert!(feature.is_none());
            }
            other => panic!("unexpected {other:?}"),
        }
        let feature = Cli::try_parse_from(["q", "tree", "--feature", "Rollout"]).unwrap();
        match feature.command {
            Commands::Tree { id, feature } => {
                assert!(id.is_none());
                assert_eq!(feature.as_deref(), Some("Rollout"));
            }
            other => panic!("unexpected {other:?}"),
        }
        assert!(Cli::try_parse_from(["q", "tree"]).is_err());
        assert!(Cli::try_parse_from(["q", "tree", "12", "--feature", "Rollout"]).is_err());
    }

    #[test]
    fn color_flag_and_aliases_parse() {
        let listed =
            Cli::try_parse_from(["q", "--color", "always", "-j", "ls", "-a", "-n", "10"]).unwrap();
        assert_eq!(listed.color, ColorMode::Always);
        assert!(listed.json);
        match listed.command {
            Commands::Ls { all, limit, .. } => {
                assert!(all);
                assert_eq!(limit, 10);
            }
            other => panic!("unexpected {other:?}"),
        }
        assert!(Cli::try_parse_from(["q", "--color", "rainbow", "ls"]).is_err());

        let top =
            Cli::try_parse_from(["q", "top", "-i", "0.5", "-n", "5", "-a", "--once"]).unwrap();
        match top.command {
            Commands::Top {
                interval,
                limit,
                all,
                once,
                ..
            } => {
                assert_eq!(interval, 0.5);
                assert_eq!(limit, 5);
                assert!(all);
                assert!(once);
            }
            other => panic!("unexpected {other:?}"),
        }
        assert_eq!(
            preprocess(vec!["top".into(), "--interval".into(), "1".into()]),
            vec!["top", "--interval", "1"]
        );

        let done = Cli::try_parse_from(["q", "done", "4", "--summary", "ok"]).unwrap();
        match done.command {
            Commands::Complete { id, summary, .. } => {
                assert_eq!(id, 4);
                assert_eq!(summary, "ok");
            }
            other => panic!("unexpected {other:?}"),
        }
        let removed = Cli::try_parse_from(["q", "rm", "9", "--force"]).unwrap();
        match removed.command {
            Commands::Delete { ids, force } => {
                assert_eq!(ids, vec![9]);
                assert!(force);
            }
            other => panic!("unexpected {other:?}"),
        }
        assert!(matches!(
            Cli::try_parse_from(["q", "recover"]).unwrap().command,
            Commands::RecoverStale { .. }
        ));
        match Cli::try_parse_from(["q", "canceled", "3"]).unwrap().command {
            Commands::Cancel { ids } => assert_eq!(ids, vec![3]),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn lifecycle_commands_accept_one_or_more_ids() {
        match Cli::try_parse_from(["q", "ready", "11", "12", "13"])
            .unwrap()
            .command
        {
            Commands::Ready { ids } => assert_eq!(ids, vec![11, 12, 13]),
            other => panic!("unexpected {other:?}"),
        }
        match Cli::try_parse_from(["q", "ready", "11"]).unwrap().command {
            Commands::Ready { ids } => assert_eq!(ids, vec![11]),
            other => panic!("unexpected {other:?}"),
        }
        assert!(Cli::try_parse_from(["q", "ready"]).is_err());
        assert!(Cli::try_parse_from(["q", "ready", "11", "twelve"]).is_err());

        match Cli::try_parse_from(["q", "cancel", "4", "5"])
            .unwrap()
            .command
        {
            Commands::Cancel { ids } => assert_eq!(ids, vec![4, 5]),
            other => panic!("unexpected {other:?}"),
        }
        match Cli::try_parse_from(["q", "reopen", "6", "7"])
            .unwrap()
            .command
        {
            Commands::Reopen { ids } => assert_eq!(ids, vec![6, 7]),
            other => panic!("unexpected {other:?}"),
        }
        match Cli::try_parse_from(["q", "delete", "8", "9", "--force"])
            .unwrap()
            .command
        {
            Commands::Delete { ids, force } => {
                assert_eq!(ids, vec![8, 9]);
                assert!(force);
            }
            other => panic!("unexpected {other:?}"),
        }
        match Cli::try_parse_from(["q", "delete", "--force", "8", "9"])
            .unwrap()
            .command
        {
            Commands::Delete { ids, force } => {
                assert_eq!(ids, vec![8, 9]);
                assert!(force);
            }
            other => panic!("unexpected {other:?}"),
        }
        assert_eq!(
            preprocess(vec!["ready".into(), "11".into(), "12".into(), "13".into()]),
            vec!["ready", "11", "12", "13"]
        );
    }

    #[test]
    fn color_and_aliases_are_not_rewritten_as_capture() {
        let args = preprocess(vec![
            "--color".into(),
            "never".into(),
            "rm".into(),
            "3".into(),
        ]);
        assert_eq!(args, vec!["--color", "never", "rm", "3"]);
        assert_eq!(
            preprocess(vec!["done".into(), "3".into()]),
            vec!["done", "3"]
        );
        assert_eq!(preprocess(vec!["recover".into()]), vec!["recover"]);
        assert_eq!(
            preprocess(vec!["-j".into(), "Fix the bug".into()]),
            vec!["add", "-j", "Fix the bug"]
        );
    }

    #[test]
    fn double_dash_forces_capture() {
        let args = preprocess(vec!["--".into(), "claim".into()]);
        assert_eq!(args, vec!["add", "--", "claim"]);
    }

    fn words(args: &[&str]) -> Vec<String> {
        args.iter().map(|arg| arg.to_string()).collect()
    }

    #[test]
    fn log_and_exec_take_the_task_id_from_the_environment() {
        // Missing id: inserted before the first positional, or appended.
        assert_eq!(
            preprocess_with(words(&["log", "a note"]), Some("22")),
            vec!["log", "22", "a note"]
        );
        assert_eq!(
            preprocess_with(words(&["log"]), Some("22")),
            vec!["log", "22"]
        );
        assert_eq!(
            preprocess_with(words(&["log", "--claim-token", "T", "a note"]), Some("22")),
            vec!["log", "--claim-token", "T", "22", "a note"]
        );
        assert_eq!(
            preprocess_with(words(&["log", "--artifact", "pr=x"]), Some(" 22 ")),
            vec!["log", "--artifact", "pr=x", "22"]
        );
        assert_eq!(
            preprocess_with(words(&["exec", "--", "cargo", "build"]), Some("22")),
            vec!["exec", "22", "--", "cargo", "build"]
        );
        assert_eq!(
            preprocess_with(words(&["-j", "exec", "cargo", "build"]), Some("22")),
            vec!["-j", "exec", "22", "cargo", "build"]
        );
        assert_eq!(
            preprocess_with(
                words(&["exec", "--thread", "bench", "--", "cargo", "bench"]),
                Some("22")
            ),
            vec!["exec", "--thread", "bench", "22", "--", "cargo", "bench"]
        );
        // An explicit id wins; other commands and a bare title are untouched.
        assert_eq!(
            preprocess_with(words(&["log", "7", "a note"]), Some("22")),
            vec!["log", "7", "a note"]
        );
        assert_eq!(
            preprocess_with(words(&["exec", "7", "--", "true"]), Some("22")),
            vec!["exec", "7", "--", "true"]
        );
        assert_eq!(
            preprocess_with(words(&["heartbeat", "--claim-token", "T"]), Some("22")),
            vec!["heartbeat", "--claim-token", "T"]
        );
        assert_eq!(
            preprocess_with(words(&["exec the plan"]), Some("22")),
            vec!["add", "exec the plan"]
        );
        // Without the variable nothing changes.
        assert_eq!(
            preprocess_with(words(&["log", "a note"]), None),
            vec!["log", "a note"]
        );

        let parsed = Cli::try_parse_from([
            "q", "exec", "22", "--thread", "bench", "--", "cargo", "bench", "--locked",
        ])
        .unwrap();
        match parsed.command {
            Commands::Exec {
                id,
                thread,
                command,
                ..
            } => {
                assert_eq!(id, 22);
                assert_eq!(thread.as_deref(), Some("bench"));
                assert_eq!(command, vec!["cargo", "bench", "--locked"]);
            }
            other => panic!("unexpected {other:?}"),
        }
        let parsed =
            Cli::try_parse_from(["q", "exec", "22", "cargo", "build", "--locked"]).unwrap();
        match parsed.command {
            Commands::Exec { command, .. } => {
                assert_eq!(command, vec!["cargo", "build", "--locked"]);
            }
            other => panic!("unexpected {other:?}"),
        }
        assert!(Cli::try_parse_from(["q", "exec", "22"]).is_err());
    }
}
