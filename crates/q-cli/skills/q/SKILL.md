---
name: q
description: Use the local-first q agent work queue (CLI + MCP) to capture ready or held tasks, claim/heartbeat/complete work with claim tokens, and respect the human gate plus risk/lease safety. Use when the user mentions q, the agent work queue, claiming tasks, the held/ready workflow, or starting the q worker (including a herdr pool via `q workers spawn`).
---

# q agent work queue

`q` is a local SQLite queue for coding and research agents. Use the `q` binary or the `q mcp` tools. Do not invent a second queue, task file, or status tracker for work that belongs here.

## Start

Say **start the q worker** to run this loop in the current session. There is no `q work` command: you do the work, and `q` only tracks it.

Say **start the q worker with N workers**, or **start the queue loop with N workers**, to spawn that many independent copies of this loop in herdr instead of running it here. Run `q workers spawn N`. See [Spawn a pool](#spawn-a-pool).

1. Claim one ready task with `q claim --agent ID` (MCP `queue_claim_next`). That claim is the mutual-exclusion step and it is atomic. Do not list tasks and then claim by id. If the claim finds nothing, another agent took the only eligible task or the queue is empty: wait about 30 seconds and try again.
2. Do that one task. Do not claim a second task while this one is open, and do not abandon a claim without `q complete`, `q escalate`, `q fail`, or `q release`.
3. While you work, heartbeat about every minute: `q heartbeat ID --claim-token TOKEN` (MCP `queue_heartbeat`). Heartbeat refreshes the lease, which starts at 30 minutes. `q top` flags a worker whose last heartbeat is older than 2 minutes (`--stale-after`) as stale. A claim with no heartbeat until the lease ends is released back to ready, locally on the next `q claim` or `q top`, and on `q serve` by the background sweep.
4. Post a short note at meaningful steps: `q note ID "running tests" --claim-token TOKEN` (MCP `queue_note`). `q top` shows the latest note on the active task. `q show` lists the notes.
5. When the work is done, `q complete ID --claim-token TOKEN`. If the task is too big, or you lack the tools or context to do it, `q escalate ID "why" --claim-token TOKEN` (MCP `queue_escalate`). Escalate releases the claim and parks the task as **escalated**. It is not claimable again until a human reviews it and runs `q ready ID`. Do not escalate by failing: `q fail` is only for a genuine execution failure, and it returns the task to ready so another agent can try. Then start the next cycle.

Escalated work is a human queue. Review it with `q ls --escalated` or `q top --escalated`. `q show` prints the reason, who escalated, and when. `q ready` (human-only on `q serve`) sends it back to ready.

A specialized agent claims only tagged work: `q claim --agent ID --tag rust` (repeat `--tag`; MCP and HTTP `tags`). The task must carry every tag. Omit the filter to take any eligible task. Set tags at capture with `q add --tag rust` or later with `q edit ID --tag rust`. `--max-failures N` skips tasks that have already failed at least N times; omit it for no cap.

`--model` or `$Q_AGENT_MODEL` records the model. The hostname is detected on this machine unless you pass `--host` or set `$Q_AGENT_HOST`. A remote `q serve` stores the host and model the client sends, not the server's hostname.

## Spawn a pool

`q workers spawn N` is a job-stealing pool. It does not dispatch tasks and it does not sit in the middle. It opens one herdr tab named `workers` in the current workspace (the pane's `HERDR_WORKSPACE_ID`) and tiles N panes in a roughly square grid, with one full-width pane named `q top` along the bottom. That bottom pane runs `q top` scoped to the current project (the same discovery as `q project show`: `--project`, then `-C`, then `.agentqueue.toml` and git). Each worker pane is named `worker 1` … `worker N`. herdr starts a coding agent in each one and sends it this loop. The herdr agent name and the claim `--agent` are `worker-1` … `worker-N`. The pane's `Q_AGENT_HOST` is the pane name (`worker 1`, with the space), and the prompt passes that same `--host`, so `q top` shows which pane is which.

Each worker claims the next ready task itself through `q claim` / `queue_claim_next`. That claim is the steal, and it is atomic. There is no central assigner. An idle worker waits about 30 seconds and claims again. Workers do not share a task and they do not wait for a dispatcher to hand them one.

```bash
q workers spawn 8
q workers spawn 8 --dry-run
q workers spawn 8 --agent codex
```

`--dry-run` prints the grid and the herdr commands and does not run them, so it works without herdr installed. A real spawn fails with a clear message when `herdr` is not on `PATH`, or when this process is not a herdr pane (`HERDR_ENV=1` and `HERDR_WORKSPACE_ID`). herdr is optional: `q` shells out to the CLI and does not link against it.

`--agent KIND` or `$Q_WORKER_AGENT` selects the herdr agent kind. The default is `claude` (`herdr agent start --kind claude`). `codex` and `cursor` are the other kinds named in the README; the full set is the one herdr documents for `agent start`. Repeat `--agent-arg` for arguments after `--` (for example `--agent-arg -m --agent-arg gpt-5.4`). `--columns` and `--rows` override the terminal size used to pick 4x2 versus 2x4. Eight workers is 4 columns by 2 rows on a wide terminal and 2 by 4 on a tall one. Nine is 3x3. Odd counts use rows whose lengths differ by at most one, with the short row at the bottom of the grid, still above `q top`.

The commands are `herdr tab create --label workers`, `herdr pane split --direction right|down --ratio`, `herdr pane rename`, `herdr pane run` for `q top`, then `herdr agent start` and `herdr agent prompt` in each worker pane. Pane ids come back in the JSON (`.result.root_pane.pane_id`, `.result.pane.pane_id`). The split ratio is the share kept by the first child.

## Rules

- Capture creates a **ready** task that any idle agent may claim at once. Pass `--hold` (`-w`, `--wait`) or MCP `hold: true` to create a **held** task instead. Held work is never claimable until a human runs `q ready ID`.
- Hold anything that should get a human look before an agent starts it: risky or destructive changes, external actions, or an idea that is not specified yet. When in doubt, hold.
- After every capture, tell the user in one line what was added: the task id, status, and title, for example `captured #184 [ready] Benchmark trace encoding variants`. The CLI prints this line itself without `--json` (after a blank line, with a long title truncated with an ellipsis); with `--json` or MCP `queue_capture`, relay it from the result. Never add a task silently.
- Only a human releases held work with `q ready ID` (or several at once: `q ready 11 12 13`) or takes ready work back with `q hold ID`. There is no MCP ready or hold tool. Do not mark held work ready yourself, and do not treat a captured task as permission to start it in the current session; claim it.
- Completion is still gated after a claim: `high` and `external_action` risk stay out of default claims, and a project with `require_pr` sends implementation work to `review` for a human to accept.
- Claim at most one task. `q claim --agent ID` takes the best eligible ready task. To work on a specific task, for example one the user pointed at, claim it by id: `q claim 184 --agent ID` (MCP `queue_claim_next` with `task_id`). That task must be ready and pass the same filters (risk, capabilities, pool, dependencies, project cap); otherwise the claim fails and the error says why, for example `cannot move from held to claimed` or `not eligible for this claim: risk high is above the claim's maximum medium`. Never bypass that by editing the task. Keep the opaque claim token and send it with heartbeat, start, block, complete, escalate, fail, note, release, and log.
- Keep the task's log current while you work. `q log ID "message" --claim-token TOKEN` (MCP `queue_log`) records a timestamped, agent-attributed note: what you are about to do, what you found, what you decided. Publish reports with `--attach report=PATH` (MCP artifact `content`) so the text is stored in the database, and link PRs with `--artifact pr=URL`. Every state change is logged automatically with your agent id. `q log ID` prints the log.
- When you open a pull request for a task, reference the task as `(Q task#184)` in the title or the first line of the body, and attach the PR with `--artifact pr=URL` on `q log` or `q complete`. Never write `Closes q task #184`: closing is not what happens, and a bare `#184` makes GitHub link an unrelated issue.
- Say what you are doing: `q heartbeat ID --claim-token TOKEN --activity "Bash: cargo test"` (MCP `queue_heartbeat` with `activity`) shows the current step live in `q top`. Claude Code users can install `tools/hooks/q-activity.sh` as a `PreToolUse` hook with `Q_TASK_ID` and `Q_CLAIM_TOKEN` exported, and it is sent for every tool call automatically.
- Report progress, starting right after `q start`: `q log ID --progress 10 --claim-token TOKEN` (MCP `queue_log` with `progress`), then again at each milestone (plan made, code written, tests green, PR open), optionally with a message. The `PROG` column in `q top` and `q ls` stays blank until you do, and humans rely on it to see that work is moving. `q claim` and `q start` print the exact command. Use your honest estimate; do not report 100, completing the task sets that.
- Humans may watch the queue in the Orbit profiler with `q orbit`. Your log notes are what it draws: write `@begin label` before a piece of work and `@end` after it to get a span on your thread, and prefix notes from a parallel sub-agent with `[name]` (`[explore] @begin Survey the API`, `[explore] @end`) so it gets its own thread. Plain notes are marks. Do not run `q orbit` yourself.
- `q cancel` keeps the task and its history. `q delete` hard-deletes the task and cascaded claims, events, artifacts, and dependency rows. Delete is allowed from any status. An unexpired claim is rejected unless `--force` (CLI) or `force: true` (MCP) is set, which clears that claim in the same transaction.
- `block`, `cancel`, `release`, `recover-stale`, and `delete` do not take a reason. Claim tokens are still required for claimed work.
- `q ready`, `q cancel`, `q reopen`, and `q delete` accept one or more ids, processed in order. One failing id is reported and the rest still run; the exit status is non-zero if any failed. With `--json`, one id prints the usual single-task document and several ids print `{"results":[...],"errors":[{"id":..,"error":..}]}`.
- Default claim risk is `medium`. `high` and `external_action` are excluded unless the claim sets `--max-risk` or MCP `maximum_risk`. `external_action` also needs the project flag `allow_external_actions`, which defaults to false.
- If the claim sets an agent pool, the task must have that exact pool. Omit the pool to leave pool filtering unrestricted.
- Required capabilities must be a subset of the worker capabilities. Dependencies must be `done`. Empty repo, project, and kind filters mean unrestricted.
- Default lease is 30 minutes (minimum 1 minute, maximum 24 hours), measured from the last heartbeat. Heartbeat extends only a matching, unexpired token; `--lease-minutes` sets a new length. An expired claim is requeued by the next `q claim`, by `q top`, by `q recover-stale` (`--to ready|blocked`, default per project `stale_disposition`), and by the `q serve` sweep (default every 15 seconds, `--sweep-interval`, 0 disables it). The recovery event stores `reason` `lease_expired`. A project with `stale_disposition = blocked` still parks expired claims as blocked.
- No eligible work is success, not an error: `found` is false and `reason` is `no_eligible_ready_tasks`.
- Prefer `q --json` for machine output. Logs belong on stderr. In MCP mode, stdout is protocol only.
- If `Q_SERVER_URL` is set, every `q` command and `q mcp` talk to a shared `q serve` authority with the bearer token in `Q_SERVER_TOKEN`. Do not pass `--db` in that case. Agent tokens cannot run `q ready` or `q reopen`; the server rejects them. Clients that speak MCP over HTTP can use `$Q_SERVER_URL/mcp` with the same bearer token instead of `q mcp`.
- The queue does not create worktrees, open pull requests, merge, or deploy. It does not launch agents either, except `q workers spawn`, which only asks herdr to start them and still does not dispatch tasks.
- A **feature** is an optional group of tasks that may span repos. Each task keeps its own repo and project. Pass a feature id or unique title to `q add --feature`, `q edit --feature`, `q ls --feature`, `q tree --feature`, or MCP `feature`. `q edit --clear-feature` detaches a task. Deleting a feature clears that link and keeps the tasks.
- Every task carries a **repo** and **project** found at capture time: `--repo`/`--project` win, then `-C DIR`, then the current directory's git remote and `HEAD`, then `.agentqueue.toml` at the git root (with path rules), then the global map in `$XDG_CONFIG_HOME/q/path-map.toml`. Capture never fails outside git; the task is just unassigned. `q project show` prints what discovery found; `q project init` writes `.agentqueue.toml` (keys: `project`, `repo`, `default_kind`, `default_agent_pool`, `max_parallel_jobs`, `require_pr`, `allow_external_actions`, `stale_disposition` = `ready|blocked`, and `[[paths]]` rules). `max_parallel_jobs` counts claimed plus in-progress tasks per project.
- Give a captured task enough to work from: `--kind` (implementation, research, review, benchmark, documentation, other), `--priority N` (higher first), `--risk` (low, medium, high, external_action), a Markdown `--body` or `--body-file PATH` with the goal, scope, deliverable and acceptance criteria (`-e` opens `$EDITOR` on a template), `--depends-on IDS`, `--capability NAME`, `--agent-pool POOL`, `--tag NAME` (repeatable). `q edit ID` changes any of these later, and `--clear-project`, `--clear-repo`, `--clear-agent-pool`, `--clear-feature`, `--clear-tags` unset them.
- `q tree ID` prints the tasks that must be done before that task. Children are dependencies. `q tree --feature` does the same for every task in a feature. A repeated task is marked already shown. A dependency outside the feature is marked external.

## CLI

```bash
q "Benchmark trace encoding variants"
q --hold "Rewrite the auth flow"
q ls
q ls --all
q ls --status held
q top
q workers spawn 8 --dry-run
q show 184
q tree 184
q tree --feature "Cross-repo rollout"
q add --tag rust "Fix the parser"
q claim --agent codex-local-01 --capability rust --tag rust --model opus --json
q claim 184 --agent codex-local-01 --json
q heartbeat 184 --claim-token TOKEN
q note 184 "running tests" --claim-token TOKEN
q fail 184 "tests failed" --claim-token TOKEN
q start 184 --claim-token TOKEN --branch agent/task-184-trace-encoding
q complete 184 --claim-token TOKEN --summary "Benchmark report committed" --artifact report=./docs/benchmarks/trace-encoding.md
q log 184 "Encoder read; varint path is the slow one" --claim-token TOKEN
q log 184 --progress 40 --claim-token TOKEN
q log 184 "[bench] @begin Run the encoder benchmarks" --claim-token TOKEN
q log 184 "[bench] @end" --claim-token TOKEN
q log 184 --claim-token TOKEN --attach report=./docs/benchmarks/trace-encoding.md --artifact pr=https://github.com/acme/x/pull/7
q log 184
q artifact 7
q release 184 --claim-token TOKEN
q block 184 --claim-token TOKEN
q ready 11 12 13
q cancel 184
q delete 184
q feature create "Cross-repo rollout" --body "Ship the queue across services"
q add --feature "Cross-repo rollout" "Add the migration"
q ls --feature "Cross-repo rollout"
q edit 12 --clear-feature
```

`q top` redraws counts, the table, and recent changes every second (ages tick by the second) until `q` (or Esc or Ctrl-C) is pressed; it is for humans watching the queue, not for agents. `q ls` (alias `q list`) omits `done` and `cancelled`. `q ls --all` (`-a`) includes them. `q ls --status done` or `q ls --status cancelled` shows that status without `--all`. The human table has feature, project, priority, and a relative `UPDATED` time, and it is colored on a terminal. Prefer `q --json` (`-j`) when reading tasks: JSON timestamps stay absolute and are never colored. Tasks with no feature or project are `(none)` and sort last. Order is feature, then project, then newest update. `q done` is `q complete`. `q rm` is `q delete`. `q recover` is `q recover-stale`.

### Every command

Global flags go before the subcommand: `--db PATH`, `--server URL`, `--token TOKEN`, `-j`/`--json`, `--color auto|always|never`, `-C DIR`, `--repo REPO`, `--project NAME`.

| Command | What it does |
|---|---|
| `q "title"` / `q add TITLE` | Capture. `--hold` (`-w`, `--wait`), `--kind`, `--priority`, `--risk`, `--body`, `--body-file PATH`, `-e`/`--edit`, `--capability` (repeatable), `--agent-pool`, `--depends-on IDS`, `--feature ID\|TITLE`, `--tag` (repeatable). |
| `q ls` (`list`) | Table of open tasks. `--status`, `--escalated`, `--kind`, `--feature`, `--tag` (every tag must match), `-a`/`--all`, `-n N`. Columns: id, status, feature, project, priority, `PROG`, updated, `PR` (a clickable link on a terminal, the URL when piped), `TAGS`, title. |
| `q top` | Live view for humans: counts, table, recent changes. `-i SECONDS`, `--once`, `--stale-after SECONDS` (default 120), `--escalated`, `--tag`, plus the other `q ls` filters. The wide table adds `FAILS`, `MODEL`, `HOST`, `NOTE`, `BEAT`, `STALE`, and `ESCALATED` (who, when, and why). A worker whose last heartbeat is older than the threshold is marked `stale`. `q`, Esc, or Ctrl-C quits. |
| `q workers spawn N` | Open a herdr tab named `workers` with N agent panes in a grid and a full-width `q top` pane under it, scoped to the current project. Job-stealing: each worker claims on its own. `--dry-run`, `--agent KIND` (`$Q_WORKER_AGENT`, default `claude`), `--agent-arg`, `--columns`, `--rows`. |
| `q show ID` | One task with body, acceptance criteria, claim, artifacts (with ids and stored sizes), recent events. |
| `q tree ID` / `q tree --feature X` | What must be done first. |
| `q edit ID` | `--title`, `--body`, `--body-file`, `-e`, `--kind`, `--priority`, `--risk`, `--capability`, `--agent-pool`, `--depends-on`, `--feature`, `--tag`, `--clear-tags`, `--clear-*`. With no flags, opens the editor on the body. |
| `q ready IDS` / `q hold ID` | Human gate: release held or blocked work; take ready or blocked work back. |
| `q claim --agent ID` | Claim one eligible ready task. `--capability` (repeatable), `--kind`, `--max-risk`, `--lease-minutes` (default 30), `--agent-pool`, `--model`, `--host`, `--tag` (every tag must match), `--max-failures N` (omit for no cap); global `--repo`/`--project` restrict the pool. Prints the token and the progress command. |
| `q fail ID [NOTE] --claim-token T` | Release the claim, record the optional note, increment the failure count, and return the task to ready. Use this for a genuine execution failure, not because the task is too big. |
| `q escalate ID REASON --claim-token T` | Release the claim and park the task as `escalated` for a human. Not claimable until `q ready`. |
| `q note ID MESSAGE --claim-token T` | Append a short status line to the claimed task. |
| `q heartbeat ID --claim-token T` | Extend the lease (`--lease-minutes`, default 30). Send one about every minute while working. |
| `q start ID --claim-token T` | Move to in_progress; `--branch` and `--worktree` are recorded on the claim. |
| `q log ID [MESSAGE]` | Append a note; `--progress N`, `--artifact kind=value`, `--attach [KIND=]PATH` (stores the file's text), `--claim-token T` for attribution. With nothing to add, prints the log. |
| `q artifact ID` | Print an artifact's stored content. |
| `q complete ID` (`done`) | `--claim-token T --summary TEXT`, `--artifact`, `--attach`, `--status review\|done`. Without a token, a human accepts a task in review. |
| `q release ID --claim-token T` / `q block ID --claim-token T` | Give the task back to ready, or park it as blocked. |
| `q cancel IDS` (`canceled`) / `q reopen IDS` / `q delete IDS` (`rm`, `--force`) | Cancel keeps history; reopen brings done back to ready and cancelled back to held; delete removes everything. |
| `q events ID` | The full event log (same as `q log ID`). |
| `q status` | Counts per status, active and expired claims. |
| `q recover-stale` (`recover`) | Requeue expired claims, `--to ready\|blocked`. |
| `q feature create\|ls\|show\|edit\|delete` | Manage features (`--title`, `--body`). |
| `q project init\|show` | Write or print `.agentqueue.toml` (`--yes`, `--force`). |
| `q serve` / `q token create\|ls\|revoke` | Run the shared authority (`--bind`, `--auth FILE`, `--public-url`, `--sweep-interval SECONDS`) and manage its token file. Operators run these, not agents. The sweep releases expired leases. |
| `q orbit` | Follow the queue live in the Orbit profiler (`--url`, `--history`, `--segment`, `--once`). Humans run this; agents do not. |
| `q mcp` | The stdio MCP server. |
| `q skill [install --target NAME]` | Print or install this skill. |

`q complete` of claimed work moves through `in_progress`, then to `done`. If the project sets `require_pr` and the kind is `implementation`, completion lands in `review` instead. A human accepts review with `q complete ID` and no claim token. `q reopen ID` moves done work back to ready, or cancelled work back to held.

## MCP

`q mcp` serves newline-delimited JSON-RPC on stdio and does not open a network port. Tools:

- `queue_capture` — create a ready task, or a held one with `hold: true`. Optional `feature` is an id or unique title
- `queue_feature_create`, `queue_feature_list`, `queue_feature_get` — named task groups
- `queue_list` — list bounded summaries. Omits `done` and `cancelled` unless `status` is set or `include_terminal` / `all` is true. Optional `feature` filters by id or unique title
- `queue_get` — fetch one task, its claim, artifacts, and recent events
- `queue_tree` — dependency tree for `task_id`, or a forest for `feature` (id or unique title). Children must be done first
- `queue_claim_next` — atomically claim one eligible ready task, or return no work. With `task_id`, claim that task instead; it must be ready and eligible, or the call is a domain error. Optional `tags` (every tag must match), `max_failures`, `agent_model`, and `agent_host`
- `queue_fail` — release the claim, record an optional `note`, increment the failure count, and return the task to ready. For a genuine execution failure
- `queue_escalate` — release the claim and park the task as `escalated` with a required `reason`, when it is too big or you lack the tools or context. A human returns it with `q ready`
- `queue_note` — append a short status `message` to a claimed task
- `queue_heartbeat` — extend a lease with the task id and claim token. Send one about every minute while working
- `queue_start` — mark a claim in progress and record a branch or worktree
- `queue_block` — block claimed work with the claim token
- `queue_complete` — complete or send to review, with a summary and artifacts
- `queue_log` — append a note, a `progress` percent (0 to 100), and/or artifacts to the task log; pass `claim_token` so the entry carries your agent id. An artifact `{kind, value, content}` stores `content` in the database
- `queue_artifact` — fetch one artifact by `artifact_id` with its stored content
- `queue_release` — return a claim to ready with the claim token
- `queue_status` — counts per status plus active and expired claims
- `queue_edit` — edit task fields; omitted fields are unchanged, an empty array clears capabilities or dependencies, `clear_*` flags unset project, repo, agent pool, or feature
- `queue_cancel` — cancel a task and keep its history
- `queue_delete` — hard-delete a task (`task_id`, optional `force`). Unlike cancel, the row and its events are removed
- `queue_ready` and `queue_reopen` — only offered to human tokens over `q serve` (chat connectors signed in as a person). A stdio session or an agent token never sees them.

`queue_claim_next` takes `agent_id`, optional `task_id` (or `id`), `capabilities`, `allowed_repos`, `allowed_projects`, `allowed_kinds`, `maximum_risk`, `lease_minutes`, `agent_pool`, `agent_model`, `agent_host`, `tags`, and `max_failures`. On stdio, a missing model falls back to `Q_AGENT_MODEL` and a missing host to this machine. Over `q serve`, only the values the client sends are stored. `queue_list` takes the same `tags` filter as `q ls --tag`. Unknown argument keys are rejected. Invalid arguments are JSON-RPC `-32602`. Domain errors are a successful `tools/call` with `isError` true.

## Install

Install this skill for local agents with `q skill install`.
