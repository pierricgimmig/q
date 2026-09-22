# q

Local-first work queue for coding and research agents. Capture an idea in one command, keep it in an inbox until a person marks it ready, then let an idle agent claim it through the CLI or an MCP server on stdio.

`q` is one binary. The human CLI and `q mcp` call the same service API. SQLite is the only store, and the only SQL lives in the store crate.

## Install

Requires a stable Rust toolchain (1.88 or newer; this repo pins `stable` in `rust-toolchain.toml`).

```bash
cargo build --release
cargo install --path crates/q-cli --locked
```

The binary is named `q`. `cargo install` places it on your Cargo bin path. For MCP clients, use the absolute path of that binary.

## Database

The default database is `$XDG_DATA_HOME/q/queue.db`.

When `XDG_DATA_HOME` is unset:

| Platform | Path |
|---|---|
| Linux | `~/.local/share/q/queue.db` |
| macOS | `~/Library/Application Support/q/queue.db` |
| Windows | `%LOCALAPPDATA%\q\queue.db` |

Every connection sets WAL mode, foreign keys, and a 5 second busy timeout. Override the file with a global `--db PATH` flag. The parent directory is created if needed.

```bash
q --db /tmp/queue.db status
```

## Capture and discovery

A bare title is shorthand for `q add` and always lands in **inbox**. Inbox tasks are never claimable.

```bash
q "Benchmark trace encoding variants"
q "Compare KV-cache quantization approaches" --kind research
q -C ~/src/agent-orchestrator "Add stale-job recovery"
q --repo github.com/acme/agent-orchestrator "Add stale-job recovery"
q --project agent-orchestrator "Add stale-job recovery"
```

Discovery precedence:

1. Explicit `--repo` or `--project`.
2. `-C DIRECTORY` (does not change the parent shell).
3. The current directory, using `git -C` for the worktree root, `origin` remote, and `HEAD`.
4. `.agentqueue.toml` at the git root, including path rules.
5. A global prefix map at `$XDG_CONFIG_HOME/q/path-map.toml` (or `~/.config/q/path-map.toml`).
6. An unassigned task with an absolute capture path. Capture does not fail outside git.

Remote URLs such as `git@github.com:acme/profiler-core.git` and `https://github.com/acme/profiler-core.git` are stored as `github.com/acme/profiler-core`.

```bash
q project init --yes
q project show
```

`q project init` writes `.agentqueue.toml` at the git root. Without `--yes` it asks for confirmation on a terminal. `--force` overwrites an existing file.

## Triage

```bash
q ls
q ls --all
q ls --status inbox
q ls --status cancelled
q show 184
q edit 184 --body-file task.md
q ready 184
q block 184 --reason "Need storage-format decision first"
q cancel 184 --reason "Superseded by task 212"
q delete 184
```

`q ls` (alias `q list`) hides `done` and `cancelled`. `--all` includes them. `--status` shows only that status, including `done` or `cancelled`, and does not require `--all`.

Human output is an aligned table: `ID`, `STATUS`, `PROJECT`, `PRI`, `UPDATED`, `TITLE`. A task with no project is shown as `(none)` and sorted after named projects. Projects are ordered by name, case-insensitively. Within a project, the newest `updated_at` is first. Titles longer than 64 characters are truncated with an ellipsis. `--json` prints the same rows as `{"tasks":[...]}`.

```text
ID  STATUS  PROJECT  PRI  UPDATED               TITLE
 4  inbox   alpha      0  2026-09-22T20:04:00Z  Keep the inbox item
 2  ready   beta       1  2026-09-22T20:02:00Z  Compare encodings
 1  inbox   (none)     0  2026-09-22T20:01:00Z  Unassigned capture
```

`q ready` is the permission boundary. Any task the state machine allows can be marked ready, including a sparse inbox body. Recommended sections (Goal, Scope, Deliverable, Acceptance criteria, Repository/target, Constraints, and Dependencies) are warnings only and do not block the transition. The original capture text is kept after later edits.

`q cancel` is a status change. The task row, claims, and event history stay, and `q reopen` can bring a cancelled task back to inbox. `q delete` is a hard delete: one `BEGIN IMMEDIATE` transaction removes the task and the rows that reference it (claims, events, artifacts, and dependency edges, which the schema cascades). It is allowed from any status. An unexpired claim is rejected unless `--force` is passed; `--force` clears that claim in the same transaction. Delete does not take a reason. Events cascade with the task, so nothing is written to the event log.

## Claim lifecycle

```bash
q claim --agent codex-local-01
q claim --agent claude-local-01 --repo github.com/acme/profiler-core --json
q heartbeat 184 --claim-token TOKEN
q start 184 --claim-token TOKEN --branch agent/task-184-trace-encoding
q complete 184 --claim-token TOKEN --summary "Benchmark report committed" \
  --artifact report=./docs/benchmarks/trace-encoding.md
q release 184 --claim-token TOKEN --reason "Missing credentials for benchmark host"
```

`q claim` runs inside one `BEGIN IMMEDIATE` transaction: recover expired claims, select one eligible ready task, mark it claimed, insert an opaque token and lease, and record `task_claimed`. No eligible work is success, not an error:

```json
{"found": false, "reason": "no_eligible_ready_tasks"}
```

Default lease is 45 minutes (minimum 1 minute, maximum 24 hours). Heartbeat extends only a matching, unexpired token. `block`, `release`, and `complete` of claimed or in-progress work require that token. A matching claim is retired rather than deleted, so branch and worktree history stay in the database.

Default `--max-risk` is `medium`. High and `external_action` tasks are not selected unless the claim raises the ceiling. External-action tasks also require `allow_external_actions` on the project, which defaults to false. Empty repo, project, and kind filters mean unrestricted. Required capabilities must be a subset of the worker's capabilities. Dependencies must be `done`. A project's `max_parallel_jobs` counts claimed and in-progress tasks.

If the project sets `require_pr` and the task kind is implementation, `complete` lands in `review` even when the requested target is `done`. A human can then accept it with `q complete ID` and no claim token. `q reopen ID` moves done work back to ready so it can be claimed again, or cancelled work back to inbox.

## Admin

```bash
q status
q recover-stale --reason "lease expired after agent restart"
q recover-stale --reason "host went away" --to blocked
q events 184
```

`recover-stale` requires a nonempty reason. Without `--to`, each task follows its project's stale policy (`ready` by default). Claim also recovers expired leases in the same transaction, using the reason `lease expired`. There is no background daemon in v1.

## JSON output

`--json` is global. Entity output is pretty-printed JSON on stdout. Logs and errors stay on stderr. The default tracing level is `error`; set `RUST_LOG=info` to see claim and open messages on stderr only.

```bash
q claim --agent codex-local-01 --json
q show 184 --json
q ls --status ready --json
```

## MCP

`q mcp` speaks newline-delimited JSON-RPC on stdio. It does not open a network port. Protocol messages are the only bytes on stdout. Logs go to stderr.

```json
{
  "mcpServers": {
    "q": {
      "command": "/absolute/path/to/q",
      "args": ["mcp", "--db", "/absolute/path/to/queue.db"]
    }
  }
}
```

Tools, all backed by the same service methods as the CLI:

| Tool | Purpose |
|---|---|
| `queue_capture` | Create an inbox task. Optional repo, project, path, kind, priority, risk. |
| `queue_list` | Bounded summaries with status, project, repo, and kind filters. Omits `done` and `cancelled` unless `status` is set or `include_terminal` (alias `all`) is true. |
| `queue_get` | One task plus claim, artifacts, and recent events. |
| `queue_claim_next` | Atomically claim one eligible ready task, or return no work. |
| `queue_heartbeat` | Extend a lease with task id and claim token. |
| `queue_start` | Mark a claim in progress and record branch or worktree. |
| `queue_block` | Block claimed work with a reason. |
| `queue_complete` | Complete or send to review, with summary and artifacts. |
| `queue_release` | Return a claim to ready with a reason. |
| `queue_delete` | Hard-delete a task. `force` clears an unexpired claim. |

Unknown argument keys are rejected. Invalid tool arguments are JSON-RPC `-32602`. Domain errors are a successful `tools/call` with `isError: true`. There is no free-form update tool.

`queue_claim_next` example:

```json
{
  "agent_id": "codex-local-01",
  "capabilities": ["rust", "benchmarking"],
  "allowed_repos": ["github.com/acme/profiler-core"],
  "allowed_kinds": ["implementation", "research", "benchmark"],
  "maximum_risk": "low",
  "lease_minutes": 45
}
```

## Agent skill

`q skill` prints the agent skill on stdout, then a short install guide. `q skill --json` prints `skill`, `install_targets`, and `install_help`.

```bash
q skill
q skill install
q skill install --target cursor --target agents
```

`q skill install` writes `SKILL.md` into the user-level skill directories, creating parents as needed. Existing files are overwritten (`created` or `updated`). Unwritable targets are skipped. The command fails only when every selected target was skipped.

| Target | Path |
|---|---|
| `agents` | `~/.agents/skills/q/SKILL.md` |
| `claude` | `~/.claude/skills/q/SKILL.md` |
| `cursor` | `~/.cursor/skills/q/SKILL.md` |
| `codex` | `~/.codex/skills/q/SKILL.md` |

Grok and similar agents that read Cursor skills or `~/.agents/skills` are covered by those two directories. `--target` accepts `agents`, `claude`, `cursor`, `codex`, or `all` (the default). `--force` is accepted; overwrite does not depend on it.

## Safety defaults

- Capture is local and always creates an inbox task at low risk unless you set a higher risk.
- Only an explicit ready transition makes work claimable.
- High-risk and external-action tasks are excluded from default claims.
- External action also needs the project policy flag.
- `block`, `release`, `cancel`, and `recover-stale` require a nonempty reason.
- `delete` removes the task from the database and does not take a reason. `cancel` keeps the task. An active claim blocks `delete` unless `--force` is set.
- The queue does not launch agents, create worktrees, open pull requests, merge, deploy, or call GitHub or Linear.

## Layout

```text
crates/q-core       types, transitions, readiness checks, QueueService
crates/q-dispatch   eligibility rules used inside the claim transaction
crates/q-project    git discovery and .agentqueue.toml
crates/q-store      SQLite schema, migrations, and the service implementation
crates/q-mcp        stdio JSON-RPC adapter
crates/q-cli        the q binary
```

CLI and MCP depend on the service trait. They do not run SQL.

## Tests

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo build --release
```
