# q

Local-first work queue for coding and research agents. Capture a task in one command and it is ready for an idle agent to claim through the CLI or an MCP server on stdio. Capture it with `--hold` instead to keep it held until a person marks it ready.

`q` is one binary. The human CLI and `q mcp` call the same service API. SQLite is the only store; queue SQL lives in the store crate and OAuth grant storage lives in the HTTP crate. To share one queue across machines, run `q serve` where the database lives and point every CLI and MCP client at it with `--server`.

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

Agents have no domain or IP allowlist: CLI and server-to-server MCP clients send their bearer token and do not need an `Origin` header. For browser requests that do include `Origin`, q accepts only its own origin (scheme, hostname, and port), rejecting other origins, including `null`, with HTTP 403. An origin is not an agent identity and does not replace authentication.

Set `--public-url https://q.example.com` behind a reverse proxy. This defines both the browser origin and OAuth server/resource URLs; request `Host` and forwarded headers cannot change them. Without it, q uses the listener's HTTP origin (the loopback address when binding all interfaces). No list of agent domains is needed.

### How sign-in works

`q serve` is its own small OAuth 2.1 server. Connectors discover it at `/.well-known/oauth-authorization-server`, register a client at `/oauth/register`, run the PKCE code flow through `/oauth/authorize`, and exchange the code at `/oauth/token`. The sign-in page asks for a token-file secret, and the connector gets that token's role.

Client ids, access tokens, and refresh tokens are HMAC-signed with a key in `oauth.key` next to the database, created on first start. Tokens for a principal are signed with a key derived from that principal's secret, so `q token revoke` invalidates every connector that signed in with it. Access tokens last a day and refresh tokens ninety days. OAuth requests may target only this server's `/mcp` resource, and issued tokens are bound to that resource and the registered client.

Refresh tokens rotate on each use. q records grant identifiers, current nonce hashes, expiration, and revocation in `oauth.db` next to the queue database. Reusing an old refresh token revokes that grant's refresh and access tokens, requiring a new sign-in; this protection survives restarts. Clients must send `client_id` when refreshing and retain the replacement refresh token. Keep `oauth.db` and `oauth.key` across upgrades; deleting the grant database requires reconnecting OAuth clients. Upgrading from the earlier stateless token format also requires a one-time OAuth sign-in. Raw token-file secrets used by agents are unaffected.

The service API is `POST /v1/<method>` with the request as JSON and the result as JSON. Errors are an `{"error": {"code", "message"}}` body with a 4xx or 5xx status, and the client turns them back into the same errors the local queue returns.

## Capture and discovery

A bare title is shorthand for `q add`. A captured task lands in **ready** and an idle agent may claim it at once. Add `--hold` (short `-w`, alias `--wait`) to land it in **held** instead: held tasks are never claimable until a person runs `q ready ID`. Use `--hold` for work that needs a human look first, such as a risky migration or an idea that is not yet specified.

```bash
q "Benchmark trace encoding variants"
q --hold "Rewrite the auth flow"
q "Compare KV-cache quantization approaches" --kind research
q -C ~/src/agent-orchestrator "Add stale-job recovery"
q --repo github.com/acme/agent-orchestrator "Add stale-job recovery"
q --project agent-orchestrator "Add stale-job recovery"
q add --feature "Cross-repo rollout" "Add the migration"
q "Write the parser" --body-file task.md
q -e "Write the parser"
```

Without `--json`, capture prints a blank line and then exactly one confirmation line: `captured #184 [ready] Benchmark trace encoding variants`. The title is collapsed to one line and, like `q ls`, truncated with an ellipsis after 64 characters. `--json` prints the full task instead.

`--body` and `--body-file` set the Markdown body at capture. `-e` (`--edit`) opens `$VISUAL` or `$EDITOR` on it first, seeded with that text or, when neither is given, with a template of suggested sections (Goal, Repository / target, Scope, Deliverable, Acceptance criteria, Constraints / do not do, Dependencies). None of them are required. A body left blank is stored as no body. `--edit` is an error when no editor is set.

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
q ls --status held
q ls --status cancelled
q show 184
q tree 184
q edit 184 --body-file task.md
q edit 184
q edit 184 --priority 2 -e
q hold 184
q ready 184
q ready 11 12 13
q block 184
q cancel 184
q cancel 11 12 13
q delete 184
q reopen 184
```

`q edit` with no flags opens `$VISUAL` or `$EDITOR` on the body. `-e` (`--edit`) does the same alongside other flags, seeded with `--body` or `--body-file` when given. `q ls` (alias `q list`) hides `done` and `cancelled`. `--all` (short `-a`) includes them. `--status` shows only that status, including `done` or `cancelled`, and does not require `--all`. Statuses are `held`, `ready`, `claimed`, `in_progress`, `review`, `blocked`, `done`, and `cancelled`. `-n` sets the row limit (default 100).

Human output is an aligned table: `ID`, `STATUS`, `FEATURE`, `PROJECT`, `PRI`, `PROG`, `UPDATED`, `PR`, `TITLE`. `PR` is the task's newest `pr` artifact (attached with `--artifact pr=URL` on `q log` or `q complete`): on a terminal it is a clickable `PR` label (an OSC 8 hyperlink, supported by most modern terminals), and when output is piped or color is off the full URL is printed instead. `q top` uses the same table, so a finished task's pull request is one click away there and in `q ls --all`; `q show` links the artifact too. `PROG` is the percent complete last reported by the working agent (`q log ID --progress 40`), blank until reported, `100%` once done. A task with no feature or project is shown as `(none)`. Rows are ordered by feature title, case-insensitively, with unset features last; then by project name the same way; then by newest `updated_at`. Titles longer than 64 characters are truncated with an ellipsis. `UPDATED` is a relative time (`3m ago`, `just now`). `q show` keeps the full UTC timestamp, along with the claim, artifacts, and recent events. `--json` prints the same rows as `{"tasks":[...]}` with absolute timestamps and no color.

On a terminal, status is colored: `held` blue, `ready` green, `claimed` yellow, `in_progress` cyan, `review` magenta, `blocked` red, `done` bright green, `cancelled` dim strikethrough gray. Ids, projects, features, and times are dim. Titles are bold. Color follows `NO_COLOR`, `CLICOLOR`, and `CLICOLOR_FORCE`, and turns off when stdout is not a terminal. `--color auto|always|never` overrides that (`always` wins over `NO_COLOR`). `--json` and `q mcp` are never colored. `-j` is short for `--json`.

```text
ID  STATUS       FEATURE  PROJECT  PRI  PROG  UPDATED  PR  TITLE
 5  done         (none)   alpha      0  100%  5m ago   PR  Ship the parser
 4  held         (none)   alpha      0        3m ago       Keep the held item
 3  in_progress  (none)   alpha      0   40%  1m ago       Port the encoder
 2  ready        (none)   beta       1        1h ago       Compare encodings
 1  held         (none)   (none)     0        2d ago       Unassigned capture
```

`q hold` and `q ready` are the human gate. `q hold ID` moves a ready or blocked task to `held`, where no agent can claim it. `q ready ID` releases a held or blocked task. Any task the state machine allows can be marked ready, including a sparse body; the body's shape is never checked. The only readiness warning is for `high` or `external_action` risk, since default claims skip those tasks. The original capture text is kept after later edits. Neither command is exposed as an MCP tool, and a `q serve` agent token cannot call `ready`.

`q ready`, `q cancel`, `q reopen`, and `q delete` take one or more ids: `q ready 11 12 13`. Ids are processed in order, each one in its own transaction. A failure on one id (not found, wrong status, active claim) is reported for that id and the remaining ids still run; the command exits non-zero at the end if any id failed. Human output prints the usual confirmation or error line per id as it happens. With `--json` and exactly one id the output is the same document as before, so existing callers do not change. With `--json` and several ids the output is a single `{"results":[...],"errors":[{"id":13,"error":"..."}]}` document, where each entry in `results` has the single-id shape (`{"task":...,"warnings":[...]}` for ready, the task for cancel and reopen, and the delete outcome for delete).

`q cancel` is a status change. The task row, claims, and event history stay, and `q reopen` can bring a cancelled task back to held for another look. `q delete` is a hard delete: one `BEGIN IMMEDIATE` transaction removes the task and the rows that reference it (claims, events, artifacts, and dependency edges, which the schema cascades). It is allowed from any status. An unexpired claim is rejected unless `--force` is passed; `--force` clears that claim in the same transaction. Events cascade with the task, so nothing is written to the event log. None of these commands take a reason.

## Watching the queue

```bash
q top
q top -i 5 -a
q top --feature "Cross-repo rollout"
q top --once
```

`q top` redraws the queue counts, the task table, and a list of recent changes every two seconds until you press `q` (Esc and Ctrl-C also quit). While it runs the terminal is in raw mode, so keys you type are not echoed into the shell, and the mode is restored when it exits, including on an error. `-i` (`--interval`) sets the seconds between refreshes. The table takes the same filters as `q ls` (`--status`, `--kind`, `--feature`, `-a`, and `-n`, default 30 rows). Recent changes are noticed between refreshes and listed newest first, up to ten, in aligned columns: time, id, the status before, the status after, and the title. A task seen for the first time comes from `new`, and a deleted task goes to `deleted`. The last frame stays on screen after quitting. `--once` draws a single frame and exits, and output that is not a terminal gets plain frames with no escape codes and no key handling, so Ctrl-C quits there. `--json` is not supported; use `q ls --json` or `q status --json`.

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
#3  held         Ship the rollout  [api]
├── #1  done         Shared schema  [db]  {Other}  (external)
└── #2  ready        Write the schema  [api]
    └── #4  held         Add the types  [api]
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
q log 184 "Encoder read; the varint path is the slow one" --claim-token TOKEN
q log 184 --progress 40 --claim-token TOKEN
q log 184 --claim-token TOKEN --attach report=./docs/benchmarks/trace-encoding.md
q log 184
q artifact 7
```

`q claim` runs inside one `BEGIN IMMEDIATE` transaction: recover expired claims, select one eligible ready task, mark it claimed, insert an opaque token and lease, and record `task_claimed`. No eligible work is success, not an error:

```json
{"found": false, "reason": "no_eligible_ready_tasks"}
```

Default lease is 45 minutes (minimum 1 minute, maximum 24 hours). Heartbeat extends only a matching, unexpired token. `block`, `release`, and `complete` of claimed or in-progress work require that token. A matching claim is retired rather than deleted, so branch and worktree history stay in the database.

Default `--max-risk` is `medium`. High and `external_action` tasks are not selected unless the claim raises the ceiling. External-action tasks also require `allow_external_actions` on the project, which defaults to false. Empty repo, project, and kind filters mean unrestricted. Required capabilities must be a subset of the worker's capabilities. Dependencies must be `done`. A project's `max_parallel_jobs` counts claimed and in-progress tasks.

If the project sets `require_pr` and the task kind is implementation, `complete` lands in `review` even when the requested target is `done`. A human can then accept it with `q complete ID` and no claim token. `q reopen ID` moves done work back to ready so it can be claimed again, or cancelled work back to held.

## Task log and artifacts

Every task has a log: the append-only event list. Each entry has a UTC timestamp, the event, who did it, and what changed. State changes are recorded by the queue itself (`task_created`, `task_ready`, `task_claimed` with the agent id, `task_started`, `task_completed`, and so on, each with the `from -> to` move). Agents add their own entries while they work:

- `q log ID "message" --claim-token TOKEN` appends a `task_note`. The token attributes the note to the claiming agent; a human runs it without a token and is recorded by `$USER`.
- `q log ID --progress 40 --claim-token TOKEN` reports percent complete, alone or with a message. It is stored on the task, shown in the `PROG` column of `q ls` and `q top`, and appears in the `q top` change list as `[in_progress 40%]`. Completion sets it to `100%`; `q reopen` clears it.
- `q log ID --artifact kind=value` records a reference such as a PR URL or a path. `--attach [KIND=]PATH` reads a file and stores its text in the database (kind defaults to `report`), so a Markdown or HTML report survives even if the file goes away. Both work on `q complete` too.
- `q log ID` with nothing to add prints the log, oldest first. `q events ID` is the same list.
- `q show ID` lists artifacts with their ids and the stored size. `q artifact ARTIFACT_ID` prints the stored content; `--json` returns the artifact with `content`.

```text
2026-09-27T04:27:43Z  task_claimed    agent:claude-fable-01  ready -> claimed
2026-09-27T04:27:43Z  task_started    agent:claude-fable-01  claimed -> in_progress  branch agent/task-1
2026-09-27T04:27:43Z  task_note       agent:claude-fable-01  Reading the encoder; suspect varint path
2026-09-27T04:27:43Z  artifact_added  agent:claude-fable-01  report: report.md (artifact 2)
2026-09-27T04:27:43Z  task_note       human:pierric          reviewer: looks right
2026-09-27T04:27:43Z  task_completed  agent:claude-fable-01  in_progress -> done  Report committed
```

Logging never changes a task's status. A wrong or expired token is rejected the same way as on `q complete`.

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
| `queue_capture` | Create a ready task, or a held one with `hold: true`. Optional repo, project, path, kind, priority, risk, and feature (id or unique title). |
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
| `queue_log` | Append a note, a `progress` percent, and/or artifacts to a task's log. Artifacts may carry `content` to store in the database. |
| `queue_artifact` | Fetch one artifact by id with its stored content. |
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

- Capture is local and creates a `ready` task at low risk unless you set a higher risk. Agents may claim it at once.
- `q add --hold` (MCP `hold: true`) creates a `held` task instead. Held work is never claimable, and only a human `q ready` releases it. That is the safe capture for work you want to look at first.
- Completion is still gated: high and `external_action` risk stay out of default claims, and a project with `require_pr` sends implementation work to `review` for a human to accept.
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
