#!/bin/sh
# Claude Code PreToolUse hook: report the tool call an agent is about to run
# as the activity on its q claim, so `q top` shows it live.
#
# Install in .claude/settings.json (project or user):
#   { "hooks": { "PreToolUse": [ { "hooks": [ { "type": "command",
#       "command": "/absolute/path/to/q/tools/hooks/q-activity.sh" } ] } ] } }
#
# The hook needs to know which claim it reports for. After `q claim`, export:
#   export Q_TASK_ID=184 Q_CLAIM_TOKEN=<token>
# or write them to .q-claim in the working directory as two lines:
#   184
#   <token>
# The queue is the default database, Q_SERVER_URL/Q_SERVER_TOKEN when set,
# or the file named by Q_DB. Anything else on stdin is ignored. Nothing but
# the tool name and the agent's own one-line description of the call leaves
# the session.
set -eu
input=$(cat)
if [ -z "${Q_TASK_ID:-}" ] && [ -f .q-claim ]; then
    Q_TASK_ID=$(sed -n 1p .q-claim)
    Q_CLAIM_TOKEN=$(sed -n 2p .q-claim)
fi
[ -n "${Q_TASK_ID:-}" ] && [ -n "${Q_CLAIM_TOKEN:-}" ] || exit 0
command -v python3 >/dev/null 2>&1 || exit 0
activity=$(printf '%s' "$input" | python3 -c '
import json, sys
try:
    event = json.load(sys.stdin)
except Exception:
    sys.exit(0)
tool = event.get("tool_name") or ""
inp = event.get("tool_input") or {}
detail = inp.get("description") or inp.get("file_path") or inp.get("command") or inp.get("pattern") or ""
detail = " ".join(str(detail).split())
line = f"{tool}: {detail}" if detail else tool
print(line[:120])
') || exit 0
[ -n "$activity" ] || exit 0
if [ -n "${Q_DB:-}" ]; then set -- --db "$Q_DB"; else set --; fi
q "$@" heartbeat "$Q_TASK_ID" --claim-token "$Q_CLAIM_TOKEN" --activity "$activity" >/dev/null 2>&1 || true
exit 0
