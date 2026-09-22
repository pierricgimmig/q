use std::path::PathBuf;

use clap::{Parser, Subcommand};

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
            | "skill"
            | "help"
    )
}

fn is_bool_flag(arg: &str) -> bool {
    matches!(
        arg,
        "--json"
            | "--yes"
            | "--force"
            | "--all"
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
    )
}

#[derive(Debug, Parser)]
#[command(
    name = "q",
    version,
    about = "Local-first context-aware agent work queue",
    arg_required_else_help = true
)]
pub struct Cli {
    /// SQLite database path. Defaults to $XDG_DATA_HOME/q/queue.db.
    #[arg(long, global = true, value_name = "PATH")]
    pub db: Option<PathBuf>,

    /// Print machine-readable JSON on stdout. Logs stay on stderr.
    #[arg(long, global = true)]
    pub json: bool,

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
        #[arg(long, value_name = "KIND")]
        kind: Option<String>,
        #[arg(long)]
        priority: Option<i32>,
        #[arg(long, value_name = "RISK")]
        risk: Option<String>,
        #[arg(long)]
        body: Option<String>,
        #[arg(long, value_name = "PATH")]
        body_file: Option<PathBuf>,
        #[arg(long = "capability")]
        capability: Vec<String>,
        #[arg(long, value_name = "POOL")]
        agent_pool: Option<String>,
        #[arg(long, value_name = "IDS")]
        depends_on: Option<String>,
        /// Feature id or unique title.
        #[arg(long, value_name = "ID|TITLE")]
        feature: Option<String>,
    },
    /// List tasks. Done and cancelled are hidden unless --all or --status is set.
    #[command(alias = "list")]
    Ls {
        /// Show only this status. Includes done or cancelled when that status is named.
        #[arg(long, value_name = "STATUS")]
        status: Option<String>,
        #[arg(long, value_name = "KIND")]
        kind: Option<String>,
        #[arg(long, default_value_t = 100)]
        limit: u32,
        /// Include done and cancelled tasks. Ignored when --status is set.
        #[arg(long)]
        all: bool,
        /// Show only tasks in this feature. Id or unique title.
        #[arg(long, value_name = "ID|TITLE")]
        feature: Option<String>,
    },
    /// Show one task, its claim, artifacts, and recent events.
    Show { id: i64 },
    /// Edit task fields. With no flags, open $VISUAL or $EDITOR on the body.
    Edit {
        id: i64,
        #[arg(long)]
        title: Option<String>,
        #[arg(long)]
        body: Option<String>,
        #[arg(long, value_name = "PATH")]
        body_file: Option<PathBuf>,
        #[arg(long, value_name = "KIND")]
        kind: Option<String>,
        #[arg(long)]
        priority: Option<i32>,
        #[arg(long, value_name = "RISK")]
        risk: Option<String>,
        #[arg(long = "capability")]
        capability: Vec<String>,
        #[arg(long, value_name = "POOL")]
        agent_pool: Option<String>,
        #[arg(long, value_name = "IDS")]
        depends_on: Option<String>,
        #[arg(long)]
        clear_project: bool,
        #[arg(long)]
        clear_repo: bool,
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
    Ready { id: i64 },
    /// Block a task. Claimed work also requires --claim-token.
    Block {
        id: i64,
        #[arg(long)]
        claim_token: Option<String>,
    },
    /// Cancel a task that is inbox, ready, or blocked. The row and its history stay.
    Cancel { id: i64 },
    /// Hard-delete a task and its claims, events, artifacts, and dependency rows.
    ///
    /// Unlike cancel, nothing remains in the queue database. Events cascade with
    /// the task and are not retained. An unexpired claim requires --force.
    Delete {
        id: i64,
        /// Delete even when an unexpired claim is held. Clears that claim in the same transaction.
        #[arg(long)]
        force: bool,
    },
    /// Atomically claim one eligible ready task.
    Claim {
        #[arg(long)]
        agent: String,
        #[arg(long = "capability")]
        capability: Vec<String>,
        #[arg(long = "kind")]
        kind: Vec<String>,
        /// Default medium. High and external_action are excluded unless raised explicitly.
        #[arg(long, default_value = "medium")]
        max_risk: String,
        #[arg(long)]
        lease_minutes: Option<u64>,
        #[arg(long, value_name = "POOL")]
        agent_pool: Option<String>,
    },
    /// Extend the lease for a matching, unexpired claim token.
    Heartbeat {
        id: i64,
        #[arg(long)]
        claim_token: String,
        #[arg(long)]
        lease_minutes: Option<u64>,
    },
    /// Mark claimed work in progress.
    Start {
        id: i64,
        #[arg(long)]
        claim_token: String,
        #[arg(long)]
        branch: Option<String>,
        #[arg(long)]
        worktree: Option<String>,
    },
    /// Complete claimed work, or accept a task that is already in review.
    Complete {
        id: i64,
        #[arg(long)]
        claim_token: Option<String>,
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
        id: i64,
        #[arg(long)]
        claim_token: String,
    },
    /// Show queue counts and claim lease health.
    Status,
    /// Requeue expired claims and record a recovery event.
    RecoverStale {
        /// ready or blocked. Defaults to each project's stale policy.
        #[arg(long, value_name = "ready|blocked")]
        to: Option<String>,
    },
    /// Show the append-only event log for a task.
    Events { id: i64 },
    /// Reopen done work to ready, or cancelled work to inbox.
    Reopen { id: i64 },
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
pub enum FeatureCommand {
    /// Create a feature.
    Create {
        /// Title words. Quoted text is the usual form.
        #[arg(required = true, num_args = 1.., value_name = "TITLE")]
        title: Vec<String>,
        #[arg(long)]
        body: Option<String>,
    },
    /// List features.
    #[command(alias = "list")]
    Ls,
    /// Show one feature.
    Show { id: i64 },
    /// Edit a feature title or body.
    Edit {
        id: i64,
        #[arg(long)]
        title: Option<String>,
        #[arg(long)]
        body: Option<String>,
    },
    /// Delete a feature. Tasks stay; their feature is cleared.
    Delete { id: i64 },
}

#[derive(Debug, Subcommand)]
pub enum ProjectCommand {
    /// Write .agentqueue.toml at the git root.
    Init {
        #[arg(long)]
        yes: bool,
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
    fn double_dash_forces_capture() {
        let args = preprocess(vec!["--".into(), "claim".into()]);
        assert_eq!(args, vec!["add", "--", "claim"]);
    }
}
