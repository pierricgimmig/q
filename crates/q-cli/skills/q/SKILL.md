---
name: q
description: Use the local-first q agent work queue (CLI + MCP) to capture ready or held tasks, claim/heartbeat/complete work with claim tokens, and respect the human gate plus risk/lease safety. Use when the user mentions q, the agent work queue, claiming tasks, or the held/ready workflow.
---

# q agent work queue

`q` is a local SQLite queue for coding and research agents. Use the `q` binary or the `q mcp` tools. Do not invent a second queue, task file, or status tracker for work that belongs here.

## Rules

- Capture creates a **ready** task that any idle agent may claim at once. Pass `--hold` (`-w`, `--wait`) or MCP `hold: true` to create a **held** task instead. Held work is never claimable until a human runs `q ready ID`.
- Hold anything that should get a human look before an agent starts it: risky or destructive changes, external actions, or an idea that is not specified yet. When in doubt, hold.
- After every capture, tell the user in one line what was added: the task id, status, and title, for example `captured #184 [ready] Benchmark trace encoding variants`. The CLI prints this line itself without `--json` (after a blank line, with a long title truncated with an ellipsis); with `--json` or MCP `queue_capture`, relay it from the result. Never add a task silently.
- Only a human releases held work with `q ready ID` (or several at once: `q ready 11 12 13`) or takes ready work back with `q hold ID`. There is no MCP ready or hold tool. Do not mark held work ready yourself, and do not treat a captured task as permission to start it in the current session; claim it.
- Completion is still gated after a claim: `high` and `external_action` risk stay out of default claims, and a project with `require_pr` sends implementation work to `review` for a human to accept.
- Claim at most one task. Keep the opaque claim token and send it with heartbeat, start, block, complete, release, and log.
- Keep the task's log current while you work. `q log ID "message" --claim-token TOKEN` (MCP `queue_log`) records a timestamped, agent-attributed note: what you are about to do, what you found, what you decided. Publish reports with `--attach report=PATH` (MCP artifact `content`) so the text is stored in the database, and link PRs with `--artifact pr=URL`. Every state change is logged automatically with your agent id. `q log ID` prints the log.
- When you open a pull request for a task, reference the task as `(Q task#184)` in the title or the first line of the body, and attach the PR with `--artifact pr=URL` on `q log` or `q complete`. Never write `Closes q task #184`: closing is not what happens, and a bare `#184` makes GitHub link an unrelated issue.
- Report progress, starting right after `q start`: `q log ID --progress 10 --claim-token TOKEN` (MCP `queue_log` with `progress`), then again at each milestone (plan made, code written, tests green, PR open), optionally with a message. The `PROG` column in `q top` and `q ls` stays blank until you do, and humans rely on it to see that work is moving. `q claim` and `q start` print the exact command. Use your honest estimate; do not report 100, completing the task sets that.
- `q cancel` keeps the task and its history. `q delete` hard-deletes the task and cascaded claims, events, artifacts, and dependency rows. Delete is allowed from any status. An unexpired claim is rejected unless `--force` (CLI) or `force: true` (MCP) is set, which clears that claim in the same transaction.
- `block`, `cancel`, `release`, `recover-stale`, and `delete` do not take a reason. Claim tokens are still required for claimed work.
- `q ready`, `q cancel`, `q reopen`, and `q delete` accept one or more ids, processed in order. One failing id is reported and the rest still run; the exit status is non-zero if any failed. With `--json`, one id prints the usual single-task document and several ids print `{"results":[...],"errors":[{"id":..,"error":..}]}`.
- Default claim risk is `medium`. `high` and `external_action` are excluded unless the claim sets `--max-risk` or MCP `maximum_risk`. `external_action` also needs the project flag `allow_external_actions`, which defaults to false.
- If the claim sets an agent pool, the task must have that exact pool. Omit the pool to leave pool filtering unrestricted.
- Required capabilities must be a subset of the worker capabilities. Dependencies must be `done`. Empty repo, project, and kind filters mean unrestricted.
- Default lease is 45 minutes (minimum 1 minute, maximum 24 hours). Heartbeat extends only a matching, unexpired token.
- No eligible work is success, not an error: `found` is false and `reason` is `no_eligible_ready_tasks`.
- Prefer `q --json` for machine output. Logs belong on stderr. In MCP mode, stdout is protocol only.
- If `Q_SERVER_URL` is set, every `q` command and `q mcp` talk to a shared `q serve` authority with the bearer token in `Q_SERVER_TOKEN`. Do not pass `--db` in that case. Agent tokens cannot run `q ready` or `q reopen`; the server rejects them. Clients that speak MCP over HTTP can use `$Q_SERVER_URL/mcp` with the same bearer token instead of `q mcp`.
- The queue does not launch agents, create worktrees, open pull requests, merge, or deploy.
- A **feature** is an optional group of tasks that may span repos. Each task keeps its own repo and project. Pass a feature id or unique title to `q add --feature`, `q edit --feature`, `q ls --feature`, `q tree --feature`, or MCP `feature`. `q edit --clear-feature` detaches a task. Deleting a feature clears that link and keeps the tasks.
- `q tree ID` prints the tasks that must be done before that task. Children are dependencies. `q tree --feature` does the same for every task in a feature. A repeated task is marked already shown. A dependency outside the feature is marked external.

## CLI

```bash
q "Benchmark trace encoding variants"
q --hold "Rewrite the auth flow"
q ls
q ls --all
q ls --status held
q top
q show 184
q tree 184
q tree --feature "Cross-repo rollout"
q claim --agent codex-local-01 --capability rust --json
q heartbeat 184 --claim-token TOKEN
q start 184 --claim-token TOKEN --branch agent/task-184-trace-encoding
q complete 184 --claim-token TOKEN --summary "Benchmark report committed" --artifact report=./docs/benchmarks/trace-encoding.md
q log 184 "Encoder read; varint path is the slow one" --claim-token TOKEN
q log 184 --progress 40 --claim-token TOKEN
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

`q top` redraws counts, the table, and recent changes every two seconds until `q` (or Esc or Ctrl-C) is pressed; it is for humans watching the queue, not for agents. `q ls` (alias `q list`) omits `done` and `cancelled`. `q ls --all` (`-a`) includes them. `q ls --status done` or `q ls --status cancelled` shows that status without `--all`. The human table has feature, project, priority, and a relative `UPDATED` time, and it is colored on a terminal. Prefer `q --json` (`-j`) when reading tasks: JSON timestamps stay absolute and are never colored. Tasks with no feature or project are `(none)` and sort last. Order is feature, then project, then newest update. `q done` is `q complete`. `q rm` is `q delete`. `q recover` is `q recover-stale`.

`q complete` of claimed work moves through `in_progress`, then to `done`. If the project sets `require_pr` and the kind is `implementation`, completion lands in `review` instead. A human accepts review with `q complete ID` and no claim token. `q reopen ID` moves done work back to ready, or cancelled work back to held.

## MCP

`q mcp` serves newline-delimited JSON-RPC on stdio and does not open a network port. Tools:

- `queue_capture` — create a ready task, or a held one with `hold: true`. Optional `feature` is an id or unique title
- `queue_feature_create`, `queue_feature_list`, `queue_feature_get` — named task groups
- `queue_list` — list bounded summaries. Omits `done` and `cancelled` unless `status` is set or `include_terminal` / `all` is true. Optional `feature` filters by id or unique title
- `queue_get` — fetch one task, its claim, artifacts, and recent events
- `queue_tree` — dependency tree for `task_id`, or a forest for `feature` (id or unique title). Children must be done first
- `queue_claim_next` — atomically claim one eligible ready task, or return no work
- `queue_heartbeat` — extend a lease with the task id and claim token
- `queue_start` — mark a claim in progress and record a branch or worktree
- `queue_block` — block claimed work with the claim token
- `queue_complete` — complete or send to review, with a summary and artifacts
- `queue_log` — append a note, a `progress` percent (0 to 100), and/or artifacts to the task log; pass `claim_token` so the entry carries your agent id. An artifact `{kind, value, content}` stores `content` in the database
- `queue_artifact` — fetch one artifact by `artifact_id` with its stored content
- `queue_release` — return a claim to ready with the claim token
- `queue_delete` — hard-delete a task (`task_id`, optional `force`). Unlike cancel, the row and its events are removed.

`queue_claim_next` takes `agent_id`, `capabilities`, `allowed_repos`, `allowed_projects`, `allowed_kinds`, `maximum_risk`, `lease_minutes`, and `agent_pool`. Unknown argument keys are rejected. Invalid arguments are JSON-RPC `-32602`. Domain errors are a successful `tools/call` with `isError` true.

## Install

Install this skill for local agents with `q skill install`.
