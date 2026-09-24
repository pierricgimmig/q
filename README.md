# q

Local-first work queue for coding and research agents. Capture an idea in one command, keep it in an inbox until a person marks it ready, then let an idle agent claim it through the CLI or an MCP server on stdio.

`q` is one binary. The human CLI and `q mcp` call the same service API. SQLite is the only store, and the only SQL lives in the store crate. To share one queue across machines, run `q serve` where the database lives and point every CLI and MCP client at it with `--server`.

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

## Remote server

`q serve` turns the local database into the single authority for a fleet of agents and for chat apps. One process owns the SQLite file and answers three things over HTTP: the service API for remote `q` and `q mcp` clients, MCP over HTTP at `/mcp` for chat connectors and IDE agents, and the OAuth endpoints chat connectors sign in with. Claims stay serialized by the same `BEGIN IMMEDIATE` transaction they use locally. There is no replica and no sync: two machines cannot take the same task because there is only one place to take it from.

### VPS in five commands

```bash
# on the VPS, with the q binary in the current directory
sudo ./deploy/install.sh q.example.com          # user, systemd unit, Caddy TLS
sudo -u q q --db /var/lib/q/queue.db token create pierric --role human
sudo -u q q --db /var/lib/q/queue.db token create codex-vps --role agent
```

`deploy/install.sh` installs the binary, creates a `q` system user, writes `deploy/q.service` and `deploy/Caddyfile` with your hostname, and starts both. Without Caddy, put any TLS proxy in front of `127.0.0.1:7777` and pass `--public-url https://your.host` to `q serve` so OAuth redirects use the right origin.

`q token create` prints the secret once and writes it to `tokens.toml` next to the database. A server started with a token file picks up new and revoked tokens without a restart. An empty token file denies all access; revoking the last token keeps this file in place. Missing, unreadable, or invalid token files deny access until repaired. `q token ls` and `q token revoke NAME` manage the file. Secrets are random; nothing else is stored.

### Connect a chat app (Grok, Claude, ChatGPT)

1. In the app, add a custom connector with the URL `https://q.example.com/mcp`.
2. The app discovers the OAuth endpoints and sends you to the q sign-in page.
3. Paste your human token from `q token create` and click Allow.

The connector now acts as you: it can capture, list, edit, cancel, and mark tasks ready. The `queue_ready` and `queue_reopen` tools only appear for human tokens. A connector signed in with an agent token never sees them, and the server refuses them anyway.

Grok Bot and other clients that take a URL plus a static header instead of a sign-in flow work too: send `Authorization: Bearer <secret>` with a token-file secret.

### Connect an agent (Codex, Claude Code, scripts)

Agents that run on a machine use the `q` binary there. Two environment variables switch every command, including `q mcp`, to the server:

```bash
export Q_SERVER_URL="https://q.example.com"
export Q_SERVER_TOKEN="<that agent's secret>"
q claim --agent codex-vps-01 --json
```

For an MCP client config, the same values go in `env`:

```json
{
  "mcpServers": {
    "q": {
      "command": "/absolute/path/to/q",
      "args": ["mcp"],
      "env": { "Q_SERVER_URL": "https://q.example.com", "Q_SERVER_TOKEN": "..." }
    }
  }
}
```

Clients that speak MCP over HTTP directly can skip the local binary and use `https://q.example.com/mcp` with the bearer header.

`--server URL` and `--token TOKEN` are global flags that override `Q_SERVER_URL` and `Q_SERVER_TOKEN`. When a server is set, `--db` is ignored.

### Tokens and roles

Tokens live in a TOML file, by default `tokens.toml` next to the database:

```toml
[[tokens]]
name = "pierric"
role = "human"
secret = "..."

[[tokens]]
name = "codex-vps"
role = "agent"
secret = "..."
```

Secrets must be at least 16 characters. `human` tokens may call everything. `agent` tokens cannot call `ready` or `reopen`, so an agent cannot make work claimable, and any actor an agent sends is recorded as an agent. The rule that only humans mark work ready is enforced by the server, not by convention.

Without a token file the server accepts every request as an anonymous human and refuses to bind anything but a loopback address. This local mode requires a restart to enable authentication after creating the first token. `--public-url` requires a token file. The systemd service always passes `--auth`, and the installer creates an empty token file before starting it. `GET /v1/health` and the OAuth discovery endpoints need no token.

### How sign-in works

`q serve` is its own small OAuth 2.1 server. Connectors discover it at `/.well-known/oauth-authorization-server`, register a client at `/oauth/register`, run the PKCE code flow through `/oauth/authorize`, and exchange the code at `/oauth/token`. The sign-in page asks for a token-file secret, and the connector gets that token's role.

There is no session database. Client ids, access tokens, and refresh tokens are HMAC-signed with a key in `oauth.key` next to the database, created on first start. Tokens for a principal are signed with a key derived from that principal's secret, so `q token revoke` invalidates every connector that signed in with it. Access tokens last a day and refresh tokens ninety days.

The service API is `POST /v1/<method>` with the request as JSON and the result as JSON. Errors are an `{"error": {"code", "message"}}` body with a 4xx or 5xx status, and the client turns them back into the same errors the local queue returns.

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
q ls -a
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

`q ls` (alias `q list`) hides `done` and `cancelled`. `--all` (short `-a`) includes them. `--status` shows only that status, including `done` or `cancelled`, and does not require `--all`. `-n` sets the row limit (default 100).

Human output is an aligned table: `ID`, `STATUS`, `FEATURE`, `PROJECT`, `PRI`, `UPDATED`, `TITLE`. A task with no feature or project is shown as `(none)`. Rows are ordered by feature title, case-insensitively, with unset features last; then by project name the same way; then by newest `updated_at`. Titles longer than 64 characters are truncated with an ellipsis. `UPDATED` is a relative time (`3m ago`, `just now`). `q show` keeps the full UTC timestamp, along with the claim, artifacts, and recent events. `--json` prints the same rows as `{"tasks":[...]}` with absolute timestamps and no color.

On a terminal, status is colored: `inbox` blue, `ready` green, `claimed` yellow, `in_progress` cyan, `review` magenta, `blocked` red, `done` dim green, `cancelled` dim strikethrough gray. Ids, projects, features, and times are dim. Titles are bold. Color follows `NO_COLOR`, `CLICOLOR`, and `CLICOLOR_FORCE`, and turns off when stdout is not a terminal. `--color auto|always|never` overrides that (`always` wins over `NO_COLOR`). `--json` and `q mcp` are never colored. `-j` is short for `--json`.

```text
ID  STATUS  FEATURE  PROJECT  PRI  UPDATED  TITLE
 4  inbox   (none)   alpha      0  3m ago   Keep the inbox item
 2  ready   (none)   beta       1  1h ago   Compare encodings
 1  inbox   (none)   (none)     0  2d ago   Unassigned capture
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

Titles use the same 64-character ellipsis as `q ls`. Status words use the same colors as the list. Connectors stay the box-drawing characters below.

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

`q mcp` speaks newline-delimited JSON-RPC on stdio. It does not open a network port. Protocol messages are the only bytes on stdout. Logs go to stderr. With `--server` or `Q_SERVER_URL` set it forwards every tool call to a `q serve` authority instead of opening a local file.

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
| `queue_status` | Counts per status plus active and expired claims. |
| `queue_edit` | Edit task fields, including feature, dependencies, and `clear_*` flags. |
| `queue_cancel` | Cancel a task, keeping its history. |
| `queue_ready` | Move a task to ready. Only listed for human tokens over `q serve`. |
| `queue_reopen` | Move a done task back to ready. Only listed for human tokens over `q serve`. |

Unknown argument keys are rejected. Invalid tool arguments are JSON-RPC `-32602`. Domain errors are a successful `tools/call` with `isError: true`. Over stdio there is no ready tool: a local agent cannot make work claimable. Over `q serve`, the ready and reopen tools appear only for human tokens.

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
crates/q-store      SQLite schema, migrations, and the service implementation
crates/q-mcp        stdio JSON-RPC adapter
crates/q-http       q serve: service API, MCP over HTTP, OAuth sign-in; and RemoteQueue, the HTTP client
deploy/             systemd unit, Caddyfile, and install script for a VPS
crates/q-cli        the q binary
```

CLI, MCP, and the HTTP transport depend on the service trait. They do not run SQL.

## Tests

CI (`.github/workflows/ci.yml`) runs these on every pull request and push to `main`. Build and test run on Linux, macOS, and Windows.

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --locked
cargo build --workspace --all-targets --locked
cargo test --workspace --locked
cargo deny --all-features --locked check   # cargo install --locked cargo-deny
```

`--locked` fails if `Cargo.lock` is out of date, so run `cargo update -p <crate>` or a plain `cargo build` first when you change dependencies. The dependency policy (advisories, licenses, duplicate versions, sources) lives in `deny.toml`. A scheduled weekly run re-checks advisories against the current lockfile.
