# q

Local-first work queue for coding and research agents. Capture an idea in one command, keep it in an inbox until a person marks it ready, then let an idle agent claim it through the CLI or an MCP server on stdio.

`q` is one binary. The human CLI and `q mcp` call the same service API. The working store is a local SQLite file. When a Turso URL is set, that file syncs with a libsql server that is the central authority for claims.

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

### Turso / libsql

With no remote URL, `q` stays local-only. Claims, leases, and `BEGIN IMMEDIATE` behave as they do today. `q status` reports `link: local_only`.

Set a remote to share one queue across machines:

```bash
export Q_TURSO_URL="libsql://q-queue-your-org.turso.io"
export Q_TURSO_AUTH_TOKEN="..."
q sync
q status
```

The same values can be passed as `--turso-url` and `--turso-auth-token`. URL lookup order is the flag, then `Q_TURSO_URL`, `LIBSQL_URL`, `TURSO_DATABASE_URL`. Token lookup is the flag, then `Q_TURSO_AUTH_TOKEN`, `LIBSQL_AUTH_TOKEN`, `TURSO_AUTH_TOKEN`. An empty value is skipped. `Queue::open` (used by tests) ignores these variables; the CLI and MCP server opt in.

The local file is still the database the CLI reads and writes. `libsql` is used only as an HTTP client (`Builder::new_remote`) to the authority. Embedded replicas that forward every write to the primary cannot accept offline writes, and stock libsql sync is last-push-wins, which would let two completions overwrite each other. q therefore syncs by `public_id` (tasks, features, events, artifacts) and `claim_token` (claims). Integer ids stay local.

`q sync`, and a sync attempt on open, pull remote rows and push dirty local rows. `q status` adds `link` (`local_only`, `online`, or `offline`) and `remote_configured`. A failed probe is offline, not a hard error: the local file keeps working.

### Offline claim policy

Creating a task does not consult that gate. An agent may capture inbox work while offline; the row is stored locally and pushed on the next successful sync. Claiming is separate: while the link is offline, an agent may claim only a task it created itself during that outage. The row must have `origin = local_unsynced`, `created_offline = 1`, and `creator_agent_id` equal to the claiming agent. Tasks pulled from the authority (`synced_from_remote`), tasks already pushed (`synced_local`), tasks created while online, tasks created by a human, and tasks created by a different agent stay unclaimable until the link returns. That is what stops two machines from taking the same ready task.

`created_offline` is set only at capture, and only when a remote is configured and the probe fails. Later edits do not change it. MCP `queue_capture` takes an optional `agent_id` (default `mcp`) so the creator matches `queue_claim_next`.

When the link is online, claim still runs in one local `BEGIN IMMEDIATE` transaction, and the authority must accept the claim before the token is returned. The remote update is a compare-and-swap on `public_id` and status `ready`. If another machine already claimed it, the local transaction rolls back and the claim result is no eligible work.

### Offline create and idempotency

Offline create and the offline claim gate are different rules. Capture always writes the local database, online or offline. Claim, while offline, still accepts only a task this agent just created locally.

Each new task stores an idempotency key so the same intent is not queued twice when machines reconnect:

- Pass `--idempotency-key` (CLI) or `idempotency_key` (MCP `queue_capture`) to name the intent. A repeat capture with that key returns the existing task.
- When the key is omitted, q derives `content:<sha256>` from the title, body, kind, repo, and project (whitespace collapsed). The same content on two machines is one task.
- The key is unique when it is set. On sync, if the authority already has that key under a different `public_id`, the authority's row is kept. The local duplicate is deleted, its `public_id` is tombstoned, and a `task_deduped` event records the discarded id. This is content dedup, not a second claim rule.

```bash
q add --idempotency-key rollout-7 "Add the migration"
```

### Reconciliation

Prevention is the offline gate. A leftover conflict is possible if a task was claimed locally and the authority also has a different claim (for example a crash after the authority accepted a claim, or a row that was synced and then claimed on two sides before the gate existed). On sync:

- Tasks are matched by `public_id`, claims by `claim_token`.
- If the latest claim tokens differ and the authority has a claim, or the authority has moved to claimed / in progress / review / done, the remote row wins.
- The local claim is retired with `release_reason = superseded` and `superseded_reason = remote_authority`. A `claim_conflict` event is appended. If the losing local status was `review` or `done`, the event is `completion_conflict` and that local completion is not pushed. Completions are never applied twice in silence.
- The same token (this agent's own heartbeat or completion catching up) is not a conflict.
- A task the authority has never seen is inserted. Deletes travel as rows in `sync_tombstones`.

## Capture and discovery

A bare title is shorthand for `q add` and always lands in **inbox**. Inbox tasks are never claimable.

```bash
q "Benchmark trace encoding variants"
q "Compare KV-cache quantization approaches" --kind research
q -C ~/src/agent-orchestrator "Add stale-job recovery"
q --repo github.com/acme/agent-orchestrator "Add stale-job recovery"
q --project agent-orchestrator "Add stale-job recovery"
q add --feature "Cross-repo rollout" "Add the migration"
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

## Features

A feature is an optional parent label. It groups tasks that may span repos. Each task still has its own `repo` and `project` from discovery. Deleting a feature clears `feature_id` on those tasks and leaves the tasks in place.

```bash
q feature create "Cross-repo rollout" --body "Ship the queue across services"
q feature ls
q feature show 1
q feature edit 1 --title "Rollout"
q add --feature "Cross-repo rollout" "Add the migration"
q edit 12 --feature 1
q edit 12 --clear-feature
q ls --feature "Cross-repo rollout"
q tree --feature "Cross-repo rollout"
q feature delete 1
```

`--feature` accepts an id or a unique title (case-insensitive). A title that matches more than one feature is an error; pass the id. `q ls` sorts by feature (unset last), then project (unset last), then newest `updated_at`.

## Triage

```bash
q ls
q ls --all
q ls --status inbox
q ls --status cancelled
q show 184
q tree 184
q edit 184 --body-file task.md
q ready 184
q block 184
q cancel 184
q delete 184
```

`q ls` (alias `q list`) hides `done` and `cancelled`. `--all` includes them. `--status` shows only that status, including `done` or `cancelled`, and does not require `--all`.

Human output is an aligned table: `ID`, `STATUS`, `FEATURE`, `PROJECT`, `PRI`, `UPDATED`, `TITLE`. A task with no feature or project is shown as `(none)`. Rows are ordered by feature title, case-insensitively, with unset features last; then by project name the same way; then by newest `updated_at`. Titles longer than 64 characters are truncated with an ellipsis. `--json` prints the same rows as `{"tasks":[...]}`.

```text
ID  STATUS  FEATURE  PROJECT  PRI  UPDATED               TITLE
 4  inbox   (none)   alpha      0  2026-09-22T20:04:00Z  Keep the inbox item
 2  ready   (none)   beta       1  2026-09-22T20:02:00Z  Compare encodings
 1  inbox   (none)   (none)     0  2026-09-22T20:01:00Z  Unassigned capture
```

`q ready` is the permission boundary. Any task the state machine allows can be marked ready, including a sparse inbox body. Recommended sections (Goal, Scope, Deliverable, Acceptance criteria, Repository/target, Constraints, and Dependencies) are warnings only and do not block the transition. The original capture text is kept after later edits.

`q cancel` is a status change. The task row, claims, and event history stay, and `q reopen` can bring a cancelled task back to inbox. `q delete` is a hard delete: one `BEGIN IMMEDIATE` transaction removes the task and the rows that reference it (claims, events, artifacts, and dependency edges, which the schema cascades). It is allowed from any status. An unexpired claim is rejected unless `--force` is passed; `--force` clears that claim in the same transaction. Events cascade with the task, so nothing is written to the event log. None of these commands take a reason.

## Dependency tree

`q tree` shows what has to be finished first. Each child is a task the parent depends on. Read downward.

```bash
q tree 12
q tree --feature "Cross-repo rollout"
q tree 12 --json
```

A task with no dependencies is one line. `--feature` prints every task in that feature. Roots are the tasks that no other task in the feature depends on, including tasks that depend on nothing. A dependency outside the feature is marked `(external)` and still expanded. A feature name in `{braces}` appears when it is not the feature you asked for. If a task would show up twice, the later copy says `(already shown)` and is not expanded again. An empty feature prints `no tasks in <title>`.

Titles use the same 64-character ellipsis as `q ls`.

```text
#3  inbox        Ship the rollout  [api]
├── #1  done         Shared schema  [db]  {Other}  (external)
└── #2  ready        Write the schema  [api]
    └── #4  inbox        Add the types  [api]
```

`--json` prints `{"roots":[...]}`. A feature tree also includes `feature`. Each node has `id`, `status`, `title`, and `depends_on`. `project`, `feature`, `external`, `already_shown`, and `cycle` are left out when they would be empty or false.

## Claim lifecycle

```bash
q claim --agent codex-local-01
q claim --agent claude-local-01 --repo github.com/acme/profiler-core --json
q heartbeat 184 --claim-token TOKEN
q start 184 --claim-token TOKEN --branch agent/task-184-trace-encoding
q complete 184 --claim-token TOKEN --summary "Benchmark report committed" \
  --artifact report=./docs/benchmarks/trace-encoding.md
q release 184 --claim-token TOKEN
```

`q claim` runs inside one `BEGIN IMMEDIATE` transaction: recover expired claims, select one eligible ready task, mark it claimed, insert an opaque token and lease, and record `task_claimed`. When a Turso URL is configured and the link is online, the authority accepts that claim before the token is returned. When the link is offline, the offline claim policy above applies. No eligible work is success, not an error:

```json
{"found": false, "reason": "no_eligible_ready_tasks"}
```

Default lease is 45 minutes (minimum 1 minute, maximum 24 hours). Heartbeat extends only a matching, unexpired token. `block`, `release`, and `complete` of claimed or in-progress work require that token. A matching claim is retired rather than deleted, so branch and worktree history stay in the database.

Default `--max-risk` is `medium`. High and `external_action` tasks are not selected unless the claim raises the ceiling. External-action tasks also require `allow_external_actions` on the project, which defaults to false. Empty repo, project, and kind filters mean unrestricted. Required capabilities must be a subset of the worker's capabilities. Dependencies must be `done`. A project's `max_parallel_jobs` counts claimed and in-progress tasks.

If the project sets `require_pr` and the task kind is implementation, `complete` lands in `review` even when the requested target is `done`. A human can then accept it with `q complete ID` and no claim token. `q reopen ID` moves done work back to ready so it can be claimed again, or cancelled work back to inbox.

## Admin

```bash
q status
q sync
q recover-stale
q recover-stale --to blocked
q events 184
```

Without `--to`, each task follows its project's stale policy (`ready` by default). Claim also recovers expired leases in the same transaction. There is no background daemon in v1. `block`, `cancel`, `release`, `recover-stale`, and `delete` do not take a reason, and events do not store one.

## JSON output

`--json` is global. Entity output is pretty-printed JSON on stdout. Logs and errors stay on stderr. The default tracing level is `error`; set `RUST_LOG=info` to see claim and open messages on stderr only.

```bash
q claim --agent codex-local-01 --json
q show 184 --json
q ls --status ready --json
q tree --feature "Cross-repo rollout" --json
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
| `queue_capture` | Create an inbox task. Optional repo, project, path, kind, priority, risk, and feature (id or unique title). |
| `queue_list` | Bounded summaries with status, project, repo, kind, and feature filters. Omits `done` and `cancelled` unless `status` is set or `include_terminal` (alias `all`) is true. |
| `queue_feature_create` | Create a feature (title, optional body). |
| `queue_feature_list` | List features. |
| `queue_feature_get` | Fetch one feature by id. |
| `queue_get` | One task plus claim, artifacts, and recent events. |
| `queue_tree` | Dependency tree for a task id, or a forest for a feature id or unique title. Children are tasks that must be done first. |
| `queue_claim_next` | Atomically claim one eligible ready task, or return no work. |
| `queue_heartbeat` | Extend a lease with task id and claim token. |
| `queue_start` | Mark a claim in progress and record branch or worktree. |
| `queue_block` | Block claimed work. Requires the claim token. |
| `queue_complete` | Complete or send to review, with summary and artifacts. |
| `queue_release` | Return a claim to ready. Requires the claim token. |
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
- `delete` removes the task from the database. `cancel` keeps the task. An active claim blocks `delete` unless `--force` is set.
- `block`, `cancel`, `release`, `recover-stale`, and `delete` do not take a reason.
- The queue does not launch agents, create worktrees, open pull requests, merge, deploy, or call GitHub or Linear.

## Layout

```text
crates/q-core       types, transitions, readiness checks, QueueService
crates/q-dispatch   eligibility rules used inside the claim transaction
crates/q-project    git discovery and .agentqueue.toml
crates/q-store      SQLite schema, migrations, Turso sync, and the service implementation
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
