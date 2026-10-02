//! Spawn a job-stealing pool of `q` workers inside herdr.
//!
//! `q workers spawn N` does not dispatch tasks. It opens one tab named
//! `workers` in the current herdr workspace, tiles N agent panes in a roughly
//! square grid, and pins a full-width `q top` pane under that grid. Each agent
//! runs the worker loop on its own and claims the next ready task through the
//! existing atomic claim. Idle workers keep polling. The claim transaction is
//! the steal; there is no central assigner.
//!
//! herdr stays optional. This module shells out to the `herdr` CLI and never
//! links against it. Layout uses the documented CLI (`tab create`, `pane
//! split`, `pane rename`, `pane run`, `agent start`, `agent prompt`). Split
//! ratio is the fraction kept by the first child, and herdr clamps it to
//! `0.1..=0.9`. The original pane stays the first child; the new pane id is
//! `.result.pane.pane_id`. `tab create` exposes `.result.root_pane.pane_id`.

use std::collections::HashSet;
use std::path::Path;
use std::process::Command;

use q_project::ProjectContext;
use serde::Serialize;
use serde_json::Value;

use crate::cli::{Cli, Commands, WorkersCommand};

/// Largest pool `q workers spawn` will open. Beyond this, equal splits fall
/// through herdr's 0.1 ratio floor and the grid stops being even.
pub const MAX_WORKERS: u32 = 32;

/// Herdr `agent start --kind` values from the agent-automation docs.
pub const AGENT_KINDS: &[&str] = &[
    "pi",
    "claude",
    "codex",
    "gemini",
    "cursor",
    "devin",
    "agy",
    "cline",
    "omp",
    "mastracode",
    "opencode",
    "copilot",
    "kimi",
    "kiro",
    "droid",
    "amp",
    "grok",
    "hermes",
    "kilo",
    "qodercli",
    "qwen",
    "letta",
    "maki",
    "muse",
];

/// Claude Code. herdr starts it with `--kind claude`. Override with `--agent`
/// or `Q_WORKER_AGENT` (`codex`, `cursor`, and the rest of [`AGENT_KINDS`]).
pub const DEFAULT_AGENT_KIND: &str = "claude";

pub const DEFAULT_COLUMNS: u32 = 120;
pub const DEFAULT_ROWS: u32 = 40;

/// Terminal cells are about twice as tall as they are wide, so a pane looks
/// square when it has about twice as many columns as rows.
const CELL_HEIGHT_OVER_WIDTH: f64 = 2.0;

/// Preferred height of the `q top` strip, before the fraction clamps below.
const Q_TOP_PREFERRED_ROWS: f64 = 12.0;
const Q_TOP_MIN_FRACTION: f64 = 0.22;
const Q_TOP_MAX_FRACTION: f64 = 0.40;

const HERDR_MIN_RATIO: f64 = 0.1;
const HERDR_MAX_RATIO: f64 = 0.9;

/// How hard the terminal's aspect pulls the grid away from a square count.
const ASPECT_WEIGHT: f64 = 0.35;
/// Prefer a filled rectangle (8 is 4x2, not 3x3 with a gap).
const RAGGED_PENALTY: f64 = 0.55;
const EXTRA_CELL_PENALTY: f64 = 0.15;
/// A single row or column is a strip. Allowed for 2 or 3 workers, not for a pool.
const STRIP_PENALTY: f64 = 5.0;

const NOT_INSIDE: &str = "\
not running inside herdr: HERDR_ENV=1 and HERDR_WORKSPACE_ID must both be set. \
Open this project in a herdr workspace and run `q workers spawn` from a pane there \
(https://herdr.dev).";

const NOT_INSTALLED: &str = "\
herdr is not installed or not on PATH. Install it from https://herdr.dev, then run \
`q workers spawn` from a pane inside a herdr workspace.";

/// Row-major worker numbers. Row lengths differ by at most one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkerGrid {
    pub rows: Vec<Vec<u32>>,
}

impl WorkerGrid {
    pub fn columns(&self) -> usize {
        self.rows.iter().map(Vec::len).max().unwrap_or(0)
    }

    pub fn row_count(&self) -> usize {
        self.rows.len()
    }

    pub fn row_lengths(&self) -> Vec<usize> {
        self.rows.iter().map(Vec::len).collect()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkerSpec {
    pub index: u32,
    pub agent_id: String,
    pub pane_label: String,
    pub pane: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Right,
    Down,
}

impl Direction {
    fn as_str(self) -> &'static str {
        match self {
            Self::Right => "right",
            Self::Down => "down",
        }
    }
}

/// One herdr invocation. Pane names (`root`, `qtop`, `p1`, ...) are filled in
/// after the previous command's JSON comes back.
#[derive(Debug, Clone, PartialEq)]
pub enum Step {
    TabCreate {
        cwd: String,
        env: Vec<(String, String)>,
    },
    Split {
        target: String,
        direction: Direction,
        ratio: f64,
        cwd: String,
        env: Vec<(String, String)>,
        new_pane: String,
    },
    Rename {
        pane: String,
        label: String,
    },
    PaneRun {
        pane: String,
        command: String,
    },
    AgentStart {
        name: String,
        kind: String,
        pane: String,
        args: Vec<String>,
    },
    AgentPrompt {
        name: String,
        prompt: String,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub struct SpawnPlan {
    pub worker_count: u32,
    pub columns: usize,
    pub row_count: usize,
    pub row_lengths: Vec<usize>,
    pub grid: Vec<Vec<u32>>,
    pub worker_height_ratio: f64,
    pub term_columns: u32,
    pub term_rows: u32,
    pub size_source: String,
    pub agent_kind: String,
    pub agent_args: Vec<String>,
    pub project: Option<String>,
    pub cwd: String,
    pub workers: Vec<WorkerSpec>,
    pub steps: Vec<Step>,
}

pub struct SpawnRequest {
    pub workers: u32,
    pub columns: u32,
    pub rows: u32,
    pub size_source: String,
    pub agent_kind: String,
    pub agent_args: Vec<String>,
    pub project: Option<String>,
    pub cwd: String,
    pub q_bin: String,
    pub global_args: Vec<String>,
    pub forwarded_env: Vec<(String, String)>,
}

pub fn parse_worker_count(raw: &str) -> Result<u32, String> {
    let value: u32 = raw
        .parse()
        .map_err(|_| format!("worker count '{raw}' is not an integer"))?;
    if !(1..=MAX_WORKERS).contains(&value) {
        return Err(format!("worker count must be from 1 to {MAX_WORKERS}"));
    }
    Ok(value)
}

pub fn parse_term_extent(raw: &str) -> Result<u32, String> {
    let value: u32 = raw
        .parse()
        .map_err(|_| format!("'{raw}' is not an integer"))?;
    if value == 0 {
        return Err("must be at least 1".into());
    }
    Ok(value)
}

pub fn worker_agent_id(index: u32) -> String {
    format!("worker-{index}")
}

pub fn worker_pane_label(index: u32) -> String {
    format!("worker {index}")
}

pub fn resolve_agent_kind(flag: Option<&str>, env: Option<&str>) -> Result<String, String> {
    let chosen = flag
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .or_else(|| env.map(str::trim).filter(|text| !text.is_empty()))
        .unwrap_or(DEFAULT_AGENT_KIND);
    if !AGENT_KINDS.contains(&chosen) {
        return Err(format!(
            "unknown herdr agent kind '{chosen}'. --agent and Q_WORKER_AGENT accept: {}",
            AGENT_KINDS.join(", ")
        ));
    }
    Ok(chosen.to_string())
}

/// Workspace id when this process is a herdr pane. `HERDR_ENV=1` is set inside
/// managed panes; `HERDR_WORKSPACE_ID` names the workspace of that pane.
pub fn require_herdr_pane(
    herdr_env: Option<&str>,
    workspace_id: Option<&str>,
) -> Result<String, String> {
    let workspace = workspace_id.map(str::trim).filter(|text| !text.is_empty());
    if herdr_env == Some("1") {
        if let Some(workspace) = workspace {
            return Ok(workspace.to_string());
        }
    }
    Err(NOT_INSIDE.to_string())
}

pub fn resolve_terminal_size(
    flag_columns: Option<u32>,
    flag_rows: Option<u32>,
    herdr_area: Option<(u32, u32)>,
    env_columns: Option<u32>,
    env_rows: Option<u32>,
) -> (u32, u32, String) {
    let (columns, column_source) = pick_extent(
        flag_columns,
        herdr_area.map(|area| area.0),
        env_columns,
        DEFAULT_COLUMNS,
    );
    let (rows, row_source) = pick_extent(
        flag_rows,
        herdr_area.map(|area| area.1),
        env_rows,
        DEFAULT_ROWS,
    );
    let source = if column_source == row_source {
        column_source.to_string()
    } else {
        format!("columns {column_source}, rows {row_source}")
    };
    (columns, rows, source)
}

fn pick_extent(
    flag: Option<u32>,
    herdr: Option<u32>,
    env: Option<u32>,
    default: u32,
) -> (u32, &'static str) {
    if let Some(value) = positive(flag) {
        return (value, "flag");
    }
    if let Some(value) = positive(herdr) {
        return (value, "herdr");
    }
    if let Some(value) = positive(env) {
        return (value, "env");
    }
    (default, "default")
}

fn positive(value: Option<u32>) -> Option<u32> {
    value.filter(|number| *number > 0)
}

/// Fraction of the tab kept by the worker grid (the first child of the root
/// down-split). The `q top` pane gets the rest. About 12 rows, clamped to
/// 22%–40% of the tab, then into herdr's ratio range.
pub fn worker_height_ratio(term_rows: u32) -> f64 {
    let rows = f64::from(term_rows.max(1));
    let fraction = (Q_TOP_PREFERRED_ROWS / rows).clamp(Q_TOP_MIN_FRACTION, Q_TOP_MAX_FRACTION);
    round_ratio(1.0 - fraction)
}

pub fn plan_grid(workers: u32, term_columns: u32, term_rows: u32) -> WorkerGrid {
    let ratio = worker_height_ratio(term_rows);
    let worker_rows = (f64::from(term_rows.max(1)) * ratio).max(1.0);
    let target = f64::from(term_columns.max(1)) / (worker_rows * CELL_HEIGHT_OVER_WIDTH);
    choose_grid(workers.max(1), target.max(0.05))
}

fn choose_grid(workers: u32, target_aspect: f64) -> WorkerGrid {
    let count = workers as usize;
    let ideal = (count as f64).sqrt();
    let mut best: Option<(f64, usize, usize)> = None;
    for row_count in 1..=count {
        let cols = count.div_ceil(row_count);
        if cols == 0 || cols * (row_count - 1) >= count {
            continue;
        }
        let aspect = cols as f64 / row_count as f64;
        let shape = (cols as f64 / ideal).ln().abs() + (row_count as f64 / ideal).ln().abs();
        let orientation = (aspect.ln() - target_aspect.ln()).abs() * ASPECT_WEIGHT;
        let slack = cols * row_count - count;
        let ragged = if slack == 0 {
            0.0
        } else {
            RAGGED_PENALTY + slack as f64 * EXTRA_CELL_PENALTY
        };
        let strip = if count > 3 && (cols == 1 || row_count == 1) {
            STRIP_PENALTY
        } else {
            0.0
        };
        let cost = shape + orientation + ragged + strip;
        let replace = match best {
            None => true,
            Some((best_cost, best_cols, best_rows)) => {
                if (cost - best_cost).abs() <= 1e-9 {
                    cols > best_cols || (cols == best_cols && row_count < best_rows)
                } else {
                    cost < best_cost
                }
            }
        };
        if replace {
            best = Some((cost, cols, row_count));
        }
    }
    let (_, cols, row_count) = best.expect("at least one grid shape");
    let long_rows = if count.is_multiple_of(row_count) {
        row_count
    } else {
        count % row_count
    };
    let short_len = if count.is_multiple_of(row_count) {
        cols
    } else {
        cols - 1
    };
    let mut rows = Vec::with_capacity(row_count);
    let mut next = 1u32;
    for index in 0..row_count {
        let len = if index < long_rows { cols } else { short_len };
        let mut row = Vec::with_capacity(len);
        for _ in 0..len {
            row.push(next);
            next += 1;
        }
        rows.push(row);
    }
    WorkerGrid { rows }
}

pub fn plan_spawn(request: SpawnRequest) -> Result<SpawnPlan, String> {
    if !(1..=MAX_WORKERS).contains(&request.workers) {
        return Err(format!("worker count must be from 1 to {MAX_WORKERS}"));
    }
    if request.columns == 0 || request.rows == 0 {
        return Err("terminal size must be at least 1 column and 1 row".into());
    }
    if request.cwd.trim().is_empty() {
        return Err("worker cwd is empty".into());
    }
    let agent_kind = resolve_agent_kind(Some(&request.agent_kind), None)?;
    let grid = plan_grid(request.workers, request.columns, request.rows);
    let ratio = worker_height_ratio(request.rows);
    let (steps, workers) = build_steps(&request, &grid, ratio, &agent_kind);
    let plan = SpawnPlan {
        worker_count: request.workers,
        columns: grid.columns(),
        row_count: grid.row_count(),
        row_lengths: grid.row_lengths(),
        grid: grid.rows,
        worker_height_ratio: ratio,
        term_columns: request.columns,
        term_rows: request.rows,
        size_source: request.size_source,
        agent_kind,
        agent_args: request.agent_args,
        project: request.project,
        cwd: request.cwd,
        workers,
        steps,
    };
    check_plan(&plan)?;
    Ok(plan)
}

fn build_steps(
    request: &SpawnRequest,
    grid: &WorkerGrid,
    worker_ratio: f64,
    agent_kind: &str,
) -> (Vec<Step>, Vec<WorkerSpec>) {
    let mut builder = LayoutBuilder {
        steps: Vec::new(),
        next_id: 0,
        base_env: request.forwarded_env.clone(),
        cwd: request.cwd.clone(),
        workers: Vec::new(),
    };
    builder.steps.push(Step::TabCreate {
        cwd: request.cwd.clone(),
        env: builder.env_for(Some(1)),
    });
    let qtop = "qtop".to_string();
    builder.steps.push(Step::Split {
        target: "root".into(),
        direction: Direction::Down,
        ratio: worker_ratio,
        cwd: request.cwd.clone(),
        env: builder.env_for(None),
        new_pane: qtop.clone(),
    });
    builder.steps.push(Step::Rename {
        pane: qtop.clone(),
        label: "q top".into(),
    });
    builder.split_rows("root", &grid.rows);
    builder.workers.sort_by_key(|worker| worker.index);
    let workers = builder.workers.clone();
    let prefix = q_prefix(&request.q_bin, &request.global_args);
    builder.steps.push(Step::PaneRun {
        pane: qtop,
        command: format!("{prefix} top"),
    });
    for worker in &workers {
        builder.steps.push(Step::AgentStart {
            name: worker.agent_id.clone(),
            kind: agent_kind.to_string(),
            pane: worker.pane.clone(),
            args: request.agent_args.clone(),
        });
        builder.steps.push(Step::AgentPrompt {
            name: worker.agent_id.clone(),
            prompt: worker_prompt(worker, &prefix),
        });
    }
    (builder.steps, workers)
}

struct LayoutBuilder {
    steps: Vec<Step>,
    next_id: u32,
    base_env: Vec<(String, String)>,
    cwd: String,
    workers: Vec<WorkerSpec>,
}

impl LayoutBuilder {
    fn fresh(&mut self) -> String {
        self.next_id += 1;
        format!("p{}", self.next_id)
    }

    fn env_for(&self, worker_index: Option<u32>) -> Vec<(String, String)> {
        let mut env = self.base_env.clone();
        if let Some(index) = worker_index {
            env.push(("Q_AGENT_HOST".into(), worker_pane_label(index)));
        }
        env
    }

    fn split_rows(&mut self, pane: &str, rows: &[Vec<u32>]) {
        if rows.len() <= 1 {
            if let Some(row) = rows.first() {
                self.split_cols(pane, row);
            }
            return;
        }
        let below = self.fresh();
        let host = rows[1].first().copied();
        self.steps.push(Step::Split {
            target: pane.to_string(),
            direction: Direction::Down,
            ratio: even_ratio(rows.len()),
            cwd: self.cwd.clone(),
            env: self.env_for(host),
            new_pane: below.clone(),
        });
        self.split_cols(pane, &rows[0]);
        self.split_rows(&below, &rows[1..]);
    }

    fn split_cols(&mut self, pane: &str, row: &[u32]) {
        let Some((first, rest)) = row.split_first() else {
            return;
        };
        if rest.is_empty() {
            self.finish_worker(pane, *first);
            return;
        }
        let right = self.fresh();
        let host = rest.first().copied();
        self.steps.push(Step::Split {
            target: pane.to_string(),
            direction: Direction::Right,
            ratio: even_ratio(row.len()),
            cwd: self.cwd.clone(),
            env: self.env_for(host),
            new_pane: right.clone(),
        });
        self.finish_worker(pane, *first);
        self.split_cols(&right, rest);
    }

    fn finish_worker(&mut self, pane: &str, index: u32) {
        let label = worker_pane_label(index);
        self.steps.push(Step::Rename {
            pane: pane.to_string(),
            label: label.clone(),
        });
        self.workers.push(WorkerSpec {
            index,
            agent_id: worker_agent_id(index),
            pane_label: label,
            pane: pane.to_string(),
        });
    }
}

fn even_ratio(parts: usize) -> f64 {
    round_ratio(1.0 / parts as f64)
}

fn round_ratio(ratio: f64) -> f64 {
    let clamped = ratio.clamp(HERDR_MIN_RATIO, HERDR_MAX_RATIO);
    (clamped * 1000.0).round() / 1000.0
}

pub fn format_ratio(ratio: f64) -> String {
    let rounded = round_ratio(ratio);
    let text = format!("{rounded:.3}");
    let trimmed = text.trim_end_matches('0').trim_end_matches('.');
    if trimmed.contains('.') {
        trimmed.to_string()
    } else {
        format!("{trimmed}.0")
    }
}

fn worker_prompt(worker: &WorkerSpec, q_prefix: &str) -> String {
    let id = &worker.agent_id;
    let pane = &worker.pane_label;
    let claim = format!("{q_prefix} claim --agent {id} --host \"{pane}\"");
    format!(
        "You are {id} in herdr pane \"{pane}\", one worker in a q job-stealing pool. \
There is no dispatcher and no central assigner. You and the other workers each claim \
the next ready task yourself. The claim is atomic, so two workers cannot take the same \
task. When nothing is eligible, wait about 30 seconds and claim again. Idle workers keep \
polling. Do not exit this loop, and do not claim a second task while one is open.\n\
\n\
This pane sets Q_AGENT_HOST to \"{pane}\" so `q top` shows which pane is which. \
Keep --agent {id} on every claim. Do not pass a different --host.\n\
\n\
1. Claim one ready task: `{claim}`\n\
   If the claim finds nothing, wait about 30 seconds and try again.\n\
2. Do that one task. Heartbeat about every minute: `{q_prefix} heartbeat ID --claim-token TOKEN`.\n\
3. Post a short note at meaningful steps: `{q_prefix} note ID \"what you are doing\" --claim-token TOKEN`.\n\
4. When it is done, `{q_prefix} complete ID --claim-token TOKEN`. If the task is too big or you \
lack the tools or context, `{q_prefix} escalate ID \"why\" --claim-token TOKEN`. Use \
`{q_prefix} fail ID \"why\" --claim-token TOKEN` only for a genuine execution failure, which \
returns the task to ready. Then go back to step 1.\n\
\n\
This is the \"start the q worker\" loop. Pass --model or set Q_AGENT_MODEL to the model you are. \
Follow the q skill for risk, hold, and progress."
    )
}

fn q_prefix(q_bin: &str, global_args: &[String]) -> String {
    let mut parts = Vec::with_capacity(1 + global_args.len());
    parts.push(shell_quote(q_bin));
    for arg in global_args {
        parts.push(shell_quote(arg));
    }
    parts.join(" ")
}

fn check_plan(plan: &SpawnPlan) -> Result<(), String> {
    let mut known = HashSet::new();
    for step in &plan.steps {
        match step {
            Step::TabCreate { .. } => {
                known.insert("root".to_string());
            }
            Step::Split {
                target,
                new_pane,
                ratio,
                ..
            } => {
                if !known.contains(target) {
                    return Err(format!("internal error: split ${target} before it exists"));
                }
                if !(HERDR_MIN_RATIO..=HERDR_MAX_RATIO).contains(ratio) {
                    return Err(format!(
                        "internal error: ratio {ratio} is outside 0.1..=0.9"
                    ));
                }
                known.insert(new_pane.clone());
            }
            Step::Rename { pane, .. }
            | Step::PaneRun { pane, .. }
            | Step::AgentStart { pane, .. } => {
                if !known.contains(pane) {
                    return Err(format!("internal error: ${pane} was not created"));
                }
            }
            Step::AgentPrompt { name, .. } => {
                if !plan.workers.iter().any(|worker| worker.agent_id == *name) {
                    return Err(format!("internal error: prompt for unknown agent {name}"));
                }
            }
        }
    }
    if !known.contains("qtop") {
        return Err("internal error: q top pane was not created".into());
    }
    if plan.workers.len() != plan.worker_count as usize {
        return Err("internal error: worker count does not match the grid".into());
    }
    Ok(())
}

pub fn render_dry_run(plan: &SpawnPlan) -> String {
    let mut out = String::new();
    push_line(&mut out, format!("# q workers spawn {}", plan.worker_count));
    push_line(
        &mut out,
        "# job-stealing pool: each worker claims the next ready task on its own; there is no dispatcher",
    );
    push_line(&mut out, format!("# grid: {}", grid_label(plan)));
    for row in &plan.grid {
        let cells = row
            .iter()
            .map(|index| format!("worker {index}"))
            .collect::<Vec<_>>()
            .join(" | ");
        push_line(&mut out, format!("#   {cells}"));
    }
    push_line(
        &mut out,
        format!(
            "#   q top (full width, below the grid; worker area ratio {})",
            format_ratio(plan.worker_height_ratio)
        ),
    );
    match &plan.project {
        Some(project) => push_line(&mut out, format!("# project: {project}")),
        None => push_line(&mut out, "# project: (none)"),
    }
    push_line(&mut out, format!("# agent: {}", plan.agent_kind));
    if !plan.agent_args.is_empty() {
        let args = plan
            .agent_args
            .iter()
            .map(|arg| shell_quote(arg))
            .collect::<Vec<_>>()
            .join(" ");
        push_line(&mut out, format!("# agent args: {args}"));
    }
    push_line(&mut out, format!("# cwd: {}", plan.cwd));
    push_line(
        &mut out,
        format!(
            "# terminal: {}x{} ({})",
            plan.term_columns, plan.term_rows, plan.size_source
        ),
    );
    push_line(&mut out, "#");
    push_line(
        &mut out,
        "# $root is the tab's root pane (.result.root_pane.pane_id) and becomes worker 1.",
    );
    push_line(
        &mut out,
        "# A split keeps that pane as the first child. The new pane is .result.pane.pane_id.",
    );
    push_line(
        &mut out,
        "# Ratio is the fraction kept by the first child. herdr clamps ratios to 0.1..=0.9.",
    );
    out.push('\n');
    for step in &plan.steps {
        for line in render_step(step) {
            push_line(&mut out, line);
        }
    }
    out
}

fn grid_label(plan: &SpawnPlan) -> String {
    if plan.row_lengths.iter().all(|len| *len == plan.columns) {
        format!("{}x{}", plan.columns, plan.row_count)
    } else {
        let parts = plan
            .row_lengths
            .iter()
            .map(|len| len.to_string())
            .collect::<Vec<_>>()
            .join("+");
        format!("{}x{} ({parts})", plan.columns, plan.row_count)
    }
}

fn render_step(step: &Step) -> Vec<String> {
    match step {
        Step::TabCreate { cwd, env } => vec![
            format!(
                "herdr tab create --workspace \"$HERDR_WORKSPACE_ID\" --label workers --cwd {} --focus{}",
                shell_quote(cwd),
                render_env_suffix(env),
            ),
            "# $root = .result.root_pane.pane_id".into(),
        ],
        Step::Split {
            target,
            direction,
            ratio,
            cwd,
            env,
            new_pane,
        } => vec![
            format!(
                "herdr pane split ${target} --direction {} --ratio {} --cwd {} --no-focus{}",
                direction.as_str(),
                format_ratio(*ratio),
                shell_quote(cwd),
                render_env_suffix(env),
            ),
            format!("# ${new_pane} = .result.pane.pane_id"),
        ],
        Step::Rename { pane, label } => {
            vec![format!(
                "herdr pane rename ${pane} {}",
                shell_quote(label)
            )]
        }
        Step::PaneRun { pane, command } => {
            vec![format!(
                "herdr pane run ${pane} {}",
                shell_quote(command)
            )]
        }
        Step::AgentStart {
            name,
            kind,
            pane,
            args,
        } => vec![render_agent_start(name, kind, pane, args)],
        Step::AgentPrompt { name, prompt } => {
            vec![format!(
                "herdr agent prompt {name} {}",
                shell_quote(prompt)
            )]
        }
    }
}

fn render_agent_start(name: &str, kind: &str, pane: &str, args: &[String]) -> String {
    let mut line = format!("herdr agent start {name} --kind {kind} --pane ${pane}");
    if !args.is_empty() {
        line.push_str(" --");
        for arg in args {
            line.push(' ');
            line.push_str(&shell_quote(arg));
        }
    }
    line
}

fn render_env_suffix(env: &[(String, String)]) -> String {
    let mut out = String::new();
    for (key, value) in env {
        out.push(' ');
        out.push_str(&render_env_flag(key, value));
    }
    out
}

fn render_env_flag(key: &str, value: &str) -> String {
    if key == "Q_SERVER_TOKEN" {
        "--env Q_SERVER_TOKEN=$Q_SERVER_TOKEN".into()
    } else {
        format!("--env {}", shell_quote(&format!("{key}={value}")))
    }
}

pub fn command_lines(plan: &SpawnPlan) -> Vec<String> {
    render_dry_run(plan)
        .lines()
        .filter(|line| line.starts_with("herdr "))
        .map(str::to_string)
        .collect()
}

fn shell_quote(text: &str) -> String {
    if text.is_empty() {
        return "''".to_string();
    }
    let safe = text.chars().all(|ch| {
        ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.' | '/' | ':' | ',' | '=')
    });
    if safe {
        text.to_string()
    } else {
        format!("'{}'", text.replace('\'', "'\\''"))
    }
}

fn push_line(out: &mut String, line: impl AsRef<str>) {
    out.push_str(line.as_ref());
    out.push('\n');
}

pub fn run(cli: &Cli, context: &ProjectContext, cwd: &Path) -> Result<(), String> {
    let Commands::Workers { command } = &cli.command else {
        return Err("internal error: workers::run without a workers command".into());
    };
    let WorkersCommand::Spawn {
        count,
        dry_run,
        agent,
        agent_arg,
        columns,
        rows,
    } = command;
    let kind = resolve_agent_kind(
        agent.as_deref(),
        std::env::var("Q_WORKER_AGENT").ok().as_deref(),
    )?;
    let workspace_id = if *dry_run {
        None
    } else {
        ensure_herdr_installed()?;
        Some(require_herdr_pane(
            std::env::var("HERDR_ENV").ok().as_deref(),
            std::env::var("HERDR_WORKSPACE_ID").ok().as_deref(),
        )?)
    };
    let area = if *dry_run || (columns.is_some() && rows.is_some()) {
        None
    } else {
        read_tab_area()
    };
    let (term_columns, term_rows, size_source) = resolve_terminal_size(
        *columns,
        *rows,
        area,
        env_positive("COLUMNS"),
        env_positive("LINES"),
    );
    let project = context
        .project
        .as_deref()
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(str::to_string);
    let (global_args, forwarded_env) = invocation_scope(cli, project.as_deref());
    let plan = plan_spawn(SpawnRequest {
        workers: *count,
        columns: term_columns,
        rows: term_rows,
        size_source,
        agent_kind: kind,
        agent_args: agent_arg.clone(),
        project,
        cwd: display_path(cwd),
        q_bin: q_bin(),
        global_args,
        forwarded_env,
    })?;
    if *dry_run {
        if cli.json {
            println!(
                "{}",
                serde_json::to_string_pretty(&dry_run_json(&plan)).map_err(|err| err.to_string())?
            );
        } else {
            print!("{}", render_dry_run(&plan));
        }
        return Ok(());
    }
    let workspace_id = workspace_id.ok_or_else(|| NOT_INSIDE.to_string())?;
    let panes = execute(&plan, &workspace_id)?;
    if cli.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&spawned_json(&plan, &panes))
                .map_err(|err| err.to_string())?
        );
    } else {
        println!("{}", success_text(&plan));
    }
    Ok(())
}

fn dry_run_json(plan: &SpawnPlan) -> DryRunJson {
    DryRunJson {
        dry_run: true,
        workers: plan.worker_count,
        columns: plan.columns,
        rows: plan.row_count,
        row_lengths: plan.row_lengths.clone(),
        worker_height_ratio: plan.worker_height_ratio,
        agent: plan.agent_kind.clone(),
        project: plan.project.clone(),
        cwd: plan.cwd.clone(),
        terminal_columns: plan.term_columns,
        terminal_rows: plan.term_rows,
        terminal_source: plan.size_source.clone(),
        grid: plan.grid.clone(),
        commands: command_lines(plan),
        script: render_dry_run(plan),
    }
}

fn spawned_json(plan: &SpawnPlan, panes: &[SpawnedPane]) -> SpawnedJson {
    SpawnedJson {
        dry_run: false,
        workers: plan.worker_count,
        columns: plan.columns,
        rows: plan.row_count,
        row_lengths: plan.row_lengths.clone(),
        worker_height_ratio: plan.worker_height_ratio,
        agent: plan.agent_kind.clone(),
        project: plan.project.clone(),
        panes: panes
            .iter()
            .map(|pane| PaneOut {
                label: pane.label.clone(),
                agent_id: pane.agent_id.clone(),
                pane_id: pane.pane_id.clone(),
            })
            .collect(),
    }
}

fn success_text(plan: &SpawnPlan) -> String {
    let project = plan.project.as_deref().unwrap_or("(none)");
    let range = if plan.worker_count == 1 {
        "worker-1".to_string()
    } else {
        format!("worker-1..worker-{}", plan.worker_count)
    };
    format!(
        "spawned {} q workers in herdr tab \"workers\" (grid {}x{})\n\
q top is the full-width pane along the bottom, project {project}\n\
agent: {} ({range})",
        plan.worker_count, plan.columns, plan.row_count, plan.agent_kind
    )
}

#[derive(Serialize)]
struct DryRunJson {
    dry_run: bool,
    workers: u32,
    columns: usize,
    rows: usize,
    row_lengths: Vec<usize>,
    worker_height_ratio: f64,
    agent: String,
    project: Option<String>,
    cwd: String,
    terminal_columns: u32,
    terminal_rows: u32,
    terminal_source: String,
    grid: Vec<Vec<u32>>,
    commands: Vec<String>,
    script: String,
}

#[derive(Serialize)]
struct SpawnedJson {
    dry_run: bool,
    workers: u32,
    columns: usize,
    rows: usize,
    row_lengths: Vec<usize>,
    worker_height_ratio: f64,
    agent: String,
    project: Option<String>,
    panes: Vec<PaneOut>,
}

#[derive(Serialize)]
struct PaneOut {
    label: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    agent_id: Option<String>,
    pane_id: String,
}

struct SpawnedPane {
    label: String,
    agent_id: Option<String>,
    pane_id: String,
}

fn invocation_scope(cli: &Cli, project: Option<&str>) -> (Vec<String>, Vec<(String, String)>) {
    let mut args = Vec::new();
    let mut env = Vec::new();
    if let Some(url) = flag_or_env(cli.server.as_deref(), "Q_SERVER_URL") {
        env.push(("Q_SERVER_URL".into(), url));
    } else if let Some(db) = &cli.db {
        args.push("--db".into());
        args.push(display_path(db));
    }
    if let Some(token) = flag_or_env(cli.token.as_deref(), "Q_SERVER_TOKEN") {
        env.push(("Q_SERVER_TOKEN".into(), token));
    }
    if let Some(project) = project {
        args.push("--project".into());
        args.push(project.to_string());
    }
    if let Some(repo) = cli
        .repo
        .as_deref()
        .map(str::trim)
        .filter(|text| !text.is_empty())
    {
        args.push("--repo".into());
        args.push(repo.to_string());
    }
    (args, env)
}

fn flag_or_env(flag: Option<&str>, key: &str) -> Option<String> {
    flag.map(str::trim)
        .filter(|text| !text.is_empty())
        .map(str::to_string)
        .or_else(|| {
            std::env::var(key)
                .ok()
                .map(|text| text.trim().to_string())
                .filter(|text| !text.is_empty())
        })
}

fn env_positive(key: &str) -> Option<u32> {
    std::env::var(key)
        .ok()
        .and_then(|raw| raw.trim().parse().ok())
        .filter(|number: &u32| *number > 0)
}

fn display_path(path: &Path) -> String {
    std::path::absolute(path)
        .unwrap_or_else(|_| path.to_path_buf())
        .display()
        .to_string()
}

fn q_bin() -> String {
    std::env::current_exe()
        .map(|path| path.display().to_string())
        .unwrap_or_else(|_| "q".into())
}

fn ensure_herdr_installed() -> Result<(), String> {
    match Command::new("herdr").arg("--version").output() {
        Ok(output) if output.status.success() => Ok(()),
        Ok(output) => {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let stdout = String::from_utf8_lossy(&output.stdout);
            Err(format!(
                "herdr --version failed ({}). {stderr}{stdout}",
                output.status
            ))
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Err(NOT_INSTALLED.to_string()),
        Err(err) => Err(format!("{NOT_INSTALLED} ({err})")),
    }
}

fn read_tab_area() -> Option<(u32, u32)> {
    let output = Command::new("herdr")
        .args(["pane", "layout", "--current"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let value = parse_herdr_json(&output.stdout).ok()?;
    let width = json_u32(
        &value,
        &[
            "/result/layout/area/width",
            "/layout/area/width",
            "/result/area/width",
        ],
    )?;
    let height = json_u32(
        &value,
        &[
            "/result/layout/area/height",
            "/layout/area/height",
            "/result/area/height",
        ],
    )?;
    if width == 0 || height == 0 {
        None
    } else {
        Some((width, height))
    }
}

fn execute(plan: &SpawnPlan, workspace_id: &str) -> Result<Vec<SpawnedPane>, String> {
    let mut panes: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    let worker_total = plan.worker_count;
    for step in &plan.steps {
        match step {
            Step::TabCreate { cwd, env } => {
                eprintln!("opening herdr tab \"workers\"");
                let mut args = vec![
                    "tab".into(),
                    "create".into(),
                    "--workspace".into(),
                    workspace_id.to_string(),
                    "--label".into(),
                    "workers".into(),
                    "--cwd".into(),
                    cwd.clone(),
                    "--focus".into(),
                ];
                push_env(&mut args, env);
                let value = run_herdr(&args)?;
                let id = required_string(
                    &value,
                    &["/result/root_pane/pane_id", "/root_pane/pane_id"],
                    "tab create did not return .result.root_pane.pane_id",
                )?;
                panes.insert("root".into(), id);
            }
            Step::Split {
                target,
                direction,
                ratio,
                cwd,
                env,
                new_pane,
            } => {
                let target_id = pane_id(&panes, target)?;
                let mut args = vec![
                    "pane".into(),
                    "split".into(),
                    target_id,
                    "--direction".into(),
                    direction.as_str().to_string(),
                    "--ratio".into(),
                    format_ratio(*ratio),
                    "--cwd".into(),
                    cwd.clone(),
                    "--no-focus".into(),
                ];
                push_env(&mut args, env);
                let value = run_herdr(&args)?;
                let id = required_string(
                    &value,
                    &["/result/pane/pane_id", "/pane/pane_id"],
                    "pane split did not return .result.pane.pane_id",
                )?;
                panes.insert(new_pane.clone(), id);
            }
            Step::Rename { pane, label } => {
                let id = pane_id(&panes, pane)?;
                run_herdr(&["pane".into(), "rename".into(), id, label.clone()])?;
            }
            Step::PaneRun { pane, command } => {
                eprintln!("starting q top");
                let id = pane_id(&panes, pane)?;
                run_herdr(&["pane".into(), "run".into(), id, command.clone()])?;
            }
            Step::AgentStart {
                name,
                kind,
                pane,
                args: extra,
            } => {
                let index = plan
                    .workers
                    .iter()
                    .find(|worker| worker.agent_id == *name)
                    .map(|worker| worker.index)
                    .unwrap_or(0);
                eprintln!("starting {name} ({index}/{worker_total})");
                let id = pane_id(&panes, pane)?;
                let mut args = vec![
                    "agent".into(),
                    "start".into(),
                    name.clone(),
                    "--kind".into(),
                    kind.clone(),
                    "--pane".into(),
                    id,
                ];
                if !extra.is_empty() {
                    args.push("--".into());
                    args.extend(extra.iter().cloned());
                }
                run_herdr(&args).map_err(|err| format!("{name}: {err}"))?;
            }
            Step::AgentPrompt { name, prompt } => {
                run_herdr(&[
                    "agent".into(),
                    "prompt".into(),
                    name.clone(),
                    prompt.clone(),
                ])
                .map_err(|err| format!("{name}: {err}"))?;
            }
        }
    }
    let mut spawned = Vec::new();
    for worker in &plan.workers {
        spawned.push(SpawnedPane {
            label: worker.pane_label.clone(),
            agent_id: Some(worker.agent_id.clone()),
            pane_id: pane_id(&panes, &worker.pane)?,
        });
    }
    spawned.push(SpawnedPane {
        label: "q top".into(),
        agent_id: None,
        pane_id: pane_id(&panes, "qtop")?,
    });
    Ok(spawned)
}

fn pane_id(
    panes: &std::collections::HashMap<String, String>,
    symbol: &str,
) -> Result<String, String> {
    panes
        .get(symbol)
        .cloned()
        .ok_or_else(|| format!("internal error: pane ${symbol} was not created"))
}

fn push_env(args: &mut Vec<String>, env: &[(String, String)]) {
    for (key, value) in env {
        args.push("--env".into());
        args.push(format!("{key}={value}"));
    }
}

fn run_herdr(args: &[String]) -> Result<Value, String> {
    let output = Command::new("herdr").args(args).output().map_err(|err| {
        if err.kind() == std::io::ErrorKind::NotFound {
            NOT_INSTALLED.to_string()
        } else {
            format!("failed to run herdr: {err}")
        }
    })?;
    if !output.status.success() {
        return Err(format!(
            "herdr {} failed: {}",
            display_args(args),
            command_detail(&output)
        ));
    }
    if output.stdout.iter().all(|byte| byte.is_ascii_whitespace()) {
        return Ok(Value::Null);
    }
    parse_herdr_json(&output.stdout).map_err(|err| format!("herdr {}: {err}", display_args(args)))
}

fn command_detail(output: &std::process::Output) -> String {
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    match (stderr.is_empty(), stdout.is_empty()) {
        (false, false) => format!("{stderr}\n{stdout}"),
        (false, true) => stderr,
        (true, false) => stdout,
        (true, true) => format!("exit status {}", output.status),
    }
}

fn display_args(args: &[String]) -> String {
    args.iter()
        .map(|arg| {
            if arg.starts_with("Q_SERVER_TOKEN=") {
                "Q_SERVER_TOKEN=(redacted)".to_string()
            } else if arg.chars().any(|ch| ch.is_whitespace()) {
                shell_quote(arg)
            } else {
                arg.clone()
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn parse_herdr_json(stdout: &[u8]) -> Result<Value, String> {
    if let Ok(value) = serde_json::from_slice::<Value>(stdout) {
        return Ok(value);
    }
    let text = String::from_utf8_lossy(stdout);
    let Some(start) = text.find('{') else {
        return Err(format!("herdr returned no JSON: {text}"));
    };
    serde_json::from_str(&text[start..])
        .map_err(|err| format!("herdr returned invalid JSON: {err}: {text}"))
}

fn required_string(value: &Value, pointers: &[&str], missing: &str) -> Result<String, String> {
    json_string(value, pointers).ok_or_else(|| format!("{missing}: {value}"))
}

fn json_string(value: &Value, pointers: &[&str]) -> Option<String> {
    pointers.iter().find_map(|pointer| {
        value
            .pointer(pointer)
            .and_then(|node| node.as_str())
            .map(str::to_string)
    })
}

fn json_u32(value: &Value, pointers: &[&str]) -> Option<u32> {
    for pointer in pointers {
        let Some(node) = value.pointer(pointer) else {
            continue;
        };
        if let Some(number) = node.as_u64() {
            return u32::try_from(number).ok().filter(|number| *number > 0);
        }
        if let Some(number) = node.as_f64() {
            if number.is_finite() && number > 0.0 && number <= f64::from(u32::MAX) {
                return Some(number.round() as u32);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn is_herdr_agent_name(name: &str) -> bool {
        let mut chars = name.chars();
        let Some(first) = chars.next() else {
            return false;
        };
        first.is_ascii_lowercase()
            && name.len() <= 32
            && chars
                .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '_' || ch == '-')
    }

    fn sample(workers: u32, columns: u32, rows: u32) -> SpawnPlan {
        plan_spawn(SpawnRequest {
            workers,
            columns,
            rows,
            size_source: "flag".into(),
            agent_kind: "claude".into(),
            agent_args: vec![],
            project: Some("demo".into()),
            cwd: "/repo".into(),
            q_bin: "q".into(),
            global_args: vec!["--project".into(), "demo".into()],
            forwarded_env: vec![],
        })
        .unwrap()
    }

    #[test]
    fn eight_wide_is_four_by_two_and_nine_is_three_by_three() {
        let eight = plan_grid(8, 160, 40);
        assert_eq!(eight.rows, vec![vec![1, 2, 3, 4], vec![5, 6, 7, 8]]);
        assert_eq!(eight.columns(), 4);
        assert_eq!(eight.row_count(), 2);
        let nine = plan_grid(9, 160, 40);
        assert_eq!(nine.rows, vec![vec![1, 2, 3], vec![4, 5, 6], vec![7, 8, 9]]);
        let nine_wide = plan_grid(9, 400, 40);
        assert_eq!(nine_wide.rows, nine.rows);
        let eight_wide = plan_grid(8, 400, 40);
        assert_eq!(eight_wide.rows, eight.rows);
    }

    #[test]
    fn eight_tall_is_two_by_four_and_nine_stays_square() {
        let eight = plan_grid(8, 80, 100);
        assert_eq!(
            eight.rows,
            vec![vec![1, 2], vec![3, 4], vec![5, 6], vec![7, 8]]
        );
        let nine = plan_grid(9, 80, 100);
        assert_eq!(nine.rows, vec![vec![1, 2, 3], vec![4, 5, 6], vec![7, 8, 9]]);
    }

    #[test]
    fn odd_counts_fill_every_cell_with_a_short_last_row() {
        let wide = plan_grid(5, 160, 40);
        assert_eq!(wide.rows, vec![vec![1, 2, 3], vec![4, 5]]);
        let tall = plan_grid(5, 80, 100);
        assert_eq!(tall.rows, vec![vec![1, 2], vec![3, 4], vec![5]]);
        for count in 1..=MAX_WORKERS {
            for (columns, rows) in [(160, 40), (80, 100), (120, 40)] {
                let grid = plan_grid(count, columns, rows);
                let lengths = grid.row_lengths();
                assert!(lengths.iter().all(|len| *len > 0), "{count} {lengths:?}");
                let max = *lengths.iter().max().unwrap();
                let min = *lengths.iter().min().unwrap();
                assert!(max - min <= 1, "n={count} {lengths:?}");
                assert_eq!(lengths.iter().sum::<usize>(), count as usize);
                let flat: Vec<u32> = grid.rows.iter().flatten().copied().collect();
                assert_eq!(flat, (1..=count).collect::<Vec<_>>());
            }
        }
    }

    #[test]
    fn one_worker_is_a_single_pane_above_q_top() {
        let plan = sample(1, 120, 40);
        assert_eq!(plan.grid, vec![vec![1]]);
        let splits: Vec<_> = plan
            .steps
            .iter()
            .filter_map(|step| match step {
                Step::Split {
                    direction, ratio, ..
                } => Some((*direction, format_ratio(*ratio))),
                _ => None,
            })
            .collect();
        assert_eq!(splits, vec![(Direction::Down, "0.7".into())]);
    }

    #[test]
    fn eight_and_nine_split_evenly_above_the_dashboard() {
        let eight = sample(8, 160, 40);
        assert_eq!(format_ratio(eight.worker_height_ratio), "0.7");
        assert_eq!(
            split_ratios(&eight),
            ratios(&[
                ("down", "0.7"),
                ("down", "0.5"),
                ("right", "0.25"),
                ("right", "0.333"),
                ("right", "0.5"),
                ("right", "0.25"),
                ("right", "0.333"),
                ("right", "0.5"),
            ])
        );
        let nine = sample(9, 120, 40);
        assert_eq!(nine.columns, 3);
        assert_eq!(nine.row_count, 3);
        assert_eq!(
            split_ratios(&nine),
            ratios(&[
                ("down", "0.7"),
                ("down", "0.333"),
                ("right", "0.333"),
                ("right", "0.5"),
                ("down", "0.5"),
                ("right", "0.333"),
                ("right", "0.5"),
                ("right", "0.333"),
                ("right", "0.5"),
            ])
        );
    }

    fn ratios(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(direction, ratio)| ((*direction).to_string(), (*ratio).to_string()))
            .collect()
    }

    fn split_ratios(plan: &SpawnPlan) -> Vec<(String, String)> {
        plan.steps
            .iter()
            .filter_map(|step| match step {
                Step::Split {
                    direction, ratio, ..
                } => Some((direction.as_str().to_string(), format_ratio(*ratio))),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn ratios_stay_inside_herdr_clamp() {
        for count in 1..=MAX_WORKERS {
            for (columns, rows) in [(160, 40), (80, 100), (40, 200)] {
                let plan = sample_at(count, columns, rows);
                for step in &plan.steps {
                    if let Step::Split { ratio, .. } = step {
                        assert!(
                            (0.1..=0.9).contains(ratio),
                            "n={count} {columns}x{rows} ratio={ratio}"
                        );
                    }
                }
            }
        }
    }

    fn sample_at(workers: u32, columns: u32, rows: u32) -> SpawnPlan {
        sample(workers, columns, rows)
    }

    #[test]
    fn names_are_worker_n_and_herdr_agent_ids() {
        let plan = sample(8, 160, 40);
        assert_eq!(plan.workers[0].pane, "root");
        for (offset, worker) in plan.workers.iter().enumerate() {
            let index = (offset as u32) + 1;
            assert_eq!(worker.index, index);
            assert_eq!(worker.agent_id, format!("worker-{index}"));
            assert_eq!(worker.pane_label, format!("worker {index}"));
            assert!(is_herdr_agent_name(&worker.agent_id));
        }
    }

    #[test]
    fn dry_run_for_eight_and_nine_lists_herdr_commands() {
        let eight = render_dry_run(&sample(8, 160, 40));
        assert!(eight.contains("job-stealing pool"), "{eight}");
        assert!(eight.contains("# grid: 4x2\n"), "{eight}");
        assert!(
            eight.contains("worker 1 | worker 2 | worker 3 | worker 4"),
            "{eight}"
        );
        assert!(
            eight.contains("worker 5 | worker 6 | worker 7 | worker 8"),
            "{eight}"
        );
        assert!(
            eight.contains("q top (full width, below the grid; worker area ratio 0.7)"),
            "{eight}"
        );
        assert!(eight.contains("# project: demo"), "{eight}");
        assert!(eight.contains("# agent: claude"), "{eight}");
        assert_eq!(eight.matches("herdr tab create").count(), 1, "{eight}");
        assert!(
            eight.contains(
                "herdr tab create --workspace \"$HERDR_WORKSPACE_ID\" --label workers --cwd /repo --focus --env 'Q_AGENT_HOST=worker 1'"
            ),
            "{eight}"
        );
        assert!(
            eight.contains("# $root = .result.root_pane.pane_id"),
            "{eight}"
        );
        assert!(
            eight.contains(
                "herdr pane split $root --direction down --ratio 0.7 --cwd /repo --no-focus"
            ),
            "{eight}"
        );
        assert!(eight.contains("# $qtop = .result.pane.pane_id"), "{eight}");
        assert!(eight.contains("herdr pane rename $qtop 'q top'"), "{eight}");
        assert!(!eight.contains("--label 'q top'"), "{eight}");
        assert!(!eight.contains("--label \"q top\""), "{eight}");
        assert!(
            eight.contains("herdr pane rename $root 'worker 1'"),
            "{eight}"
        );
        assert!(
            eight.contains("herdr pane rename $p7 'worker 8'") || eight.contains("'worker 8'"),
            "{eight}"
        );
        assert!(eight.contains("herdr pane run $qtop "), "{eight}");
        assert!(eight.contains("q --project demo top"), "{eight}");
        assert!(
            eight.contains("herdr agent start worker-1 --kind claude --pane $root"),
            "{eight}"
        );
        assert!(
            eight.contains("herdr agent start worker-8 --kind claude"),
            "{eight}"
        );
        assert!(eight.contains("herdr agent prompt worker-1 "), "{eight}");
        assert!(eight.contains("herdr agent prompt worker-8 "), "{eight}");
        assert!(eight.contains("--agent worker-1"), "{eight}");
        assert!(eight.contains("--host \"worker 1\""), "{eight}");
        assert!(eight.contains("Q_AGENT_HOST=worker 8"), "{eight}");
        assert!(eight.contains("job-stealing"), "{eight}");
        assert!(!eight.contains("jq "), "{eight}");
        assert_eq!(command_lines(&sample(8, 160, 40)).len() > 10, true);

        let nine = render_dry_run(&sample(9, 120, 40));
        assert!(nine.contains("# grid: 3x3\n"), "{nine}");
        assert!(nine.contains("worker 7 | worker 8 | worker 9"), "{nine}");
        assert!(
            nine.contains("herdr agent start worker-9 --kind claude"),
            "{nine}"
        );
        assert!(nine.contains("--host \"worker 9\""), "{nine}");
        assert_eq!(nine.matches("herdr tab create").count(), 1, "{nine}");
    }

    #[test]
    fn dry_run_redacts_the_server_token_and_passes_agent_args() {
        let plan = plan_spawn(SpawnRequest {
            workers: 2,
            columns: 160,
            rows: 40,
            size_source: "flag".into(),
            agent_kind: "codex".into(),
            agent_args: vec!["-m".into(), "gpt-5.4".into()],
            project: None,
            cwd: "/repo".into(),
            q_bin: "q".into(),
            global_args: vec![],
            forwarded_env: vec![
                ("Q_SERVER_URL".into(), "http://127.0.0.1:7777".into()),
                ("Q_SERVER_TOKEN".into(), "secret-token".into()),
            ],
        })
        .unwrap();
        let script = render_dry_run(&plan);
        assert!(script.contains("# project: (none)"), "{script}");
        assert!(script.contains("# agent: codex"), "{script}");
        assert!(
            script.contains("Q_SERVER_URL=http://127.0.0.1:7777"),
            "{script}"
        );
        assert!(
            script.contains("Q_SERVER_TOKEN=$Q_SERVER_TOKEN"),
            "{script}"
        );
        assert!(!script.contains("secret-token"), "{script}");
        assert!(
            script.contains("herdr agent start worker-1 --kind codex --pane $root -- -m gpt-5.4"),
            "{script}"
        );
        assert!(script.contains("grid: 2x1"), "{script}");
    }

    #[test]
    fn agent_kind_flag_wins_and_unknown_kinds_fail() {
        assert_eq!(resolve_agent_kind(None, None).unwrap(), "claude");
        assert_eq!(resolve_agent_kind(None, Some("cursor")).unwrap(), "cursor");
        assert_eq!(
            resolve_agent_kind(Some("codex"), Some("cursor")).unwrap(),
            "codex"
        );
        let err = resolve_agent_kind(Some("cursor-agent"), None).unwrap_err();
        assert!(err.contains("unknown herdr agent kind"), "{err}");
        assert!(err.contains("claude"), "{err}");
    }

    #[test]
    fn herdr_pane_check_requires_env_and_workspace() {
        assert!(require_herdr_pane(None, None)
            .unwrap_err()
            .contains("not running inside herdr"));
        assert!(require_herdr_pane(Some("1"), None).is_err());
        assert!(require_herdr_pane(Some("1"), Some("  ")).is_err());
        assert!(require_herdr_pane(Some("0"), Some("w1")).is_err());
        assert_eq!(require_herdr_pane(Some("1"), Some(" w1 ")).unwrap(), "w1");
    }

    #[test]
    fn terminal_size_prefers_flags_then_herdr_then_env() {
        let (columns, rows, source) =
            resolve_terminal_size(Some(200), Some(50), Some((80, 24)), Some(100), Some(30));
        assert_eq!((columns, rows, source.as_str()), (200, 50, "flag"));
        let (columns, rows, source) =
            resolve_terminal_size(None, None, Some((80, 24)), Some(100), Some(30));
        assert_eq!((columns, rows, source.as_str()), (80, 24, "herdr"));
        let (columns, rows, source) = resolve_terminal_size(None, None, None, Some(100), Some(30));
        assert_eq!((columns, rows, source.as_str()), (100, 30, "env"));
        let (columns, rows, source) = resolve_terminal_size(None, None, None, None, None);
        assert_eq!((columns, rows, source.as_str()), (120, 40, "default"));
    }

    #[test]
    fn worker_count_parser_rejects_zero_and_the_cap() {
        assert_eq!(parse_worker_count("8").unwrap(), 8);
        assert!(parse_worker_count("0").is_err());
        assert!(parse_worker_count("33").is_err());
        assert!(parse_term_extent("0").is_err());
    }
}
