use std::path::PathBuf;

use clap::{Parser, Subcommand, ValueEnum};

pub fn preprocess(mut args: Vec<String>) -> Vec<String> {
    if let Some(index) = first_positional(&args) {
        let word = &args[index];
        if word != "--" && is_command(word) {
            return args;
        }
        args.insert(0, "add".into());
    }
    args
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
            | "show"
            | "tree"
            | "edit"
            | "ready"
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
            | "project"
            | "feature"
            | "mcp"
            | "serve"
            | "token"
            | "skill"
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
            | "--body-file"
            | "--to"
            | "--artifact"
            | "--target"
            | "--capability"
            | "--capabilities"
            | "--allowed-kind"
            | "--set-project"
            | "--set-repo"
            | "--feature"
            | "--color"
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
    /// Capture a task into the inbox.
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
        /// Markdown body. Missing sections warn on ready; they do not block it.
        #[arg(long)]
        body: Option<String>,
        /// Read the body from a file instead of --body.
        #[arg(long, value_name = "PATH")]
        body_file: Option<PathBuf>,
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
    },
    /// List tasks. Done and cancelled are hidden unless --all or --status is set.
    #[command(visible_alias = "list")]
    Ls {
        /// Show only this status. Includes done or cancelled when that status is named.
        ///
        /// Statuses: inbox, ready, claimed, in_progress, review, blocked, done, cancelled.
        #[arg(long, value_name = "STATUS")]
        status: Option<String>,
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
    },
    /// Move a task to ready so agents may claim it.
    Ready {
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
    /// Cancel a task that is inbox, ready, or blocked. The row and its history stay.
    #[command(visible_alias = "canceled")]
    Cancel {
        /// Task id.
        id: i64,
    },
    /// Hard-delete a task and its claims, events, artifacts, and dependency rows.
    ///
    /// Unlike cancel, nothing remains in the queue database. Events cascade with
    /// the task and are not retained. An unexpired claim requires --force.
    #[command(visible_alias = "rm")]
    Delete {
        /// Task id.
        id: i64,
        /// Delete even when an unexpired claim is held. Clears that claim in the same transaction.
        #[arg(long)]
        force: bool,
    },
    /// Atomically claim one eligible ready task.
    Claim {
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
        /// Lease length. Default 45. Minimum 1, maximum 1440.
        #[arg(long)]
        lease_minutes: Option<u64>,
        /// Only claim a task in this pool.
        #[arg(long, value_name = "POOL")]
        agent_pool: Option<String>,
    },
    /// Extend the lease for a matching, unexpired claim token.
    Heartbeat {
        /// Task id.
        id: i64,
        /// Token printed by `q claim`.
        #[arg(long)]
        claim_token: String,
        /// New lease length in minutes. Default 45.
        #[arg(long)]
        lease_minutes: Option<u64>,
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
    /// Reopen done work to ready, or cancelled work to inbox.
    Reopen {
        /// Task id.
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
        /// Defaults to the reverse proxy's X-Forwarded-Proto and Host headers.
        #[arg(long, value_name = "URL")]
        public_url: Option<String>,
    },
    /// Manage the token file that q serve authenticates with.
    Token {
        #[command(subcommand)]
        command: TokenCommand,
    },
    /// Print or install the agent skill for q.
    Skill {
        #[command(subcommand)]
        command: Option<SkillCommand>,
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
            "inbox".into(),
        ]);
        assert_eq!(args, vec!["--db", "/tmp/q.db", "ls", "--status", "inbox"]);
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
            Commands::Delete { id, force } => {
                assert_eq!(id, 9);
                assert!(force);
            }
            other => panic!("unexpected {other:?}"),
        }
        assert!(matches!(
            Cli::try_parse_from(["q", "recover"]).unwrap().command,
            Commands::RecoverStale { .. }
        ));
        assert!(matches!(
            Cli::try_parse_from(["q", "canceled", "3"]).unwrap().command,
            Commands::Cancel { id: 3 }
        ));
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
}
