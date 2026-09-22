---
name: q
description: Use the local-first q agent work queue (CLI + MCP) to capture inbox tasks, wait for human ready, claim/heartbeat/complete work with claim tokens, and respect risk/lease safety. Use when the user mentions q, the agent work queue, claiming tasks, or inbox/ready workflow.
---

# q agent work queue

`q` is a local SQLite queue for coding and research agents. Use the `q` binary or the `q mcp` tools. Do not invent a second queue, task file, or status tracker for work that belongs here.

## Rules

- Capture creates an **inbox** task. Inbox work is never claimable.
- A human must run `q ready ID` before an agent may claim the task. There is no MCP ready tool. Do not mark inbox work ready yourself, and do not treat a captured task as permission to start.
- Claim at most one task. Keep the opaque claim token and send it with heartbeat, start, block, complete, and release.
- `block`, `release`, `cancel`, and `recover-stale` need a nonempty reason.
- `q cancel` keeps the task and its history. `q delete` hard-deletes the task and cascaded claims, events, artifacts, and dependency rows. Delete is allowed from any status and does not take a reason. An unexpired claim is rejected unless `--force` (CLI) or `force: true` (MCP) is set, which clears that claim in the same transaction.
- Default claim risk is `medium`. `high` and `external_action` are excluded unless the claim sets `--max-risk` or MCP `maximum_risk`. `external_action` also needs the project flag `allow_external_actions`, which defaults to false.
- If the claim sets an agent pool, the task must have that exact pool. Omit the pool to leave pool filtering unrestricted.
- Required capabilities must be a subset of the worker capabilities. Dependencies must be `done`. Empty repo, project, and kind filters mean unrestricted.
- Default lease is 45 minutes (minimum 1 minute, maximum 24 hours). Heartbeat extends only a matching, unexpired token.
- No eligible work is success, not an error: `found` is false and `reason` is `no_eligible_ready_tasks`.
- Prefer `q --json` for machine output. Logs belong on stderr. In MCP mode, stdout is protocol only.
- The queue does not launch agents, create worktrees, open pull requests, merge, or deploy.

## CLI

```bash
q "Benchmark trace encoding variants"
q ls
q ls --all
q ls --status inbox
q show 184
q claim --agent codex-local-01 --capability rust --json
q heartbeat 184 --claim-token TOKEN
q start 184 --claim-token TOKEN --branch agent/task-184-trace-encoding
q complete 184 --claim-token TOKEN --summary "Benchmark report committed" --artifact report=./docs/benchmarks/trace-encoding.md
q release 184 --claim-token TOKEN --reason "Missing credentials for benchmark host"
q block 184 --claim-token TOKEN --reason "Need a storage-format decision"
q cancel 184 --reason "Superseded by task 212"
q delete 184
```

`q ls` (alias `q list`) omits `done` and `cancelled`. `q ls --all` includes them. `q ls --status done` or `q ls --status cancelled` shows that status without `--all`. The table has project, priority, and `updated_at`. Tasks with no project are `(none)` and sort last. Within a project, the newest update is first.

`q complete` of claimed work moves through `in_progress`, then to `done`. If the project sets `require_pr` and the kind is `implementation`, completion lands in `review` instead. A human accepts review with `q complete ID` and no claim token. `q reopen ID` moves done work back to ready.

## MCP

`q mcp` serves newline-delimited JSON-RPC on stdio and does not open a network port. Tools:

- `queue_capture` — create an inbox task
- `queue_list` — list bounded summaries. Omits `done` and `cancelled` unless `status` is set or `include_terminal` / `all` is true
- `queue_get` — fetch one task, its claim, artifacts, and recent events
- `queue_claim_next` — atomically claim one eligible ready task, or return no work
- `queue_heartbeat` — extend a lease with the task id and claim token
- `queue_start` — mark a claim in progress and record a branch or worktree
- `queue_block` — block claimed work with a reason
- `queue_complete` — complete or send to review, with a summary and artifacts
- `queue_release` — return a claim to ready with a reason
- `queue_delete` — hard-delete a task (`task_id`, optional `force`). Unlike cancel, the row and its events are removed.

`queue_claim_next` takes `agent_id`, `capabilities`, `allowed_repos`, `allowed_projects`, `allowed_kinds`, `maximum_risk`, `lease_minutes`, and `agent_pool`. Unknown argument keys are rejected. Invalid arguments are JSON-RPC `-32602`. Domain errors are a successful `tools/call` with `isError` true.

## Install

Install this skill for local agents with `q skill install`.
