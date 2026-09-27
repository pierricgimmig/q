# Following the queue in Orbit

`q orbit` shows what the queue is doing on the Orbit profiler's live timeline. Each task is a process, each agent working it is a thread of that process, and the log entries agents write become spans and marks on those threads. It is optional and off by default: nothing in `q` talks to Orbit unless you run this command.

This document describes the mapping, the mechanism on both sides, how to run it, and what is not done.

## The mapping

Orbit's vocabulary is processes, threads, scopes (spans stacked by depth on a thread), instants, and value lanes. The bridge uses it like this:

| Queue | Orbit |
|---|---|
| The queue | Process `q queue` (pid `0x7100_0000`) with one value lane, `active claims` |
| Task `#id` | Process `#id title` (pid `0x7100_0000 + id * 256`), main thread `task` (tid = pid) |
| Task status | Spans on the main thread at depth 0: `ready`, `claimed`, `in_progress`, `review`, `blocked`, ... one after the other. `done` and `cancelled` are marks. |
| Human and system events | Marks on the main thread at depth 1: `created`, `ready`, `edited`, a human `note: ...`, `artifact kind: value` |
| A claim by agent `A` | Thread `A` (tid = pid + slot) with a `claimed` span at depth 0 for the whole lease |
| `q start` | Span `in_progress <branch>` at depth 1 inside the claim |
| Heartbeats, agent notes, artifacts | Marks at depth 2 on the agent thread: `heartbeat`, `note: ...`, `artifact pr: https://...` |
| Claim end | A mark on the agent thread: `completed: <summary>`, `released`, `blocked`, `recovered` |
| Nested work in a note: `@begin label` ... `@end` | A span named `label` on the agent thread, at depth 2 and deeper as they nest |
| A sub-agent in a note: `[name] ...` | Its own thread `name` inside the task process, so parallel sub-agents are parallel threads |
| Reported `progress` on a note | Value lane `progress %` on the task |
| Task dependency `#a` depends on `#b` | Span `waits on #b` on `#a`'s main thread at depth 2+, from `#a`'s creation until `#b` is done |

An agent, or a sub-agent it drives, uses the note convention with the ordinary log command:

```bash
q log 21 "[explore] @begin Survey the Orbit API" --claim-token TOKEN
q log 21 "[explore] Reading orbit-live-server" --claim-token TOKEN
q log 21 "[explore] @end" --claim-token TOKEN
q log 21 "@begin Write the mapper" --claim-token TOKEN     # on the agent's own thread
q log 21 "@end" --claim-token TOKEN
```

The convention is parsed by the bridge only; the log stores the text as written, and `q log ID` prints it unchanged. A `[name]` prefix needs a name with no spaces, so `[not a marker] text` stays plain text. `@end` with nothing open becomes a mark saying so.

Thread slots are stable per task: the same agent id, or the same `[name]`, gets the same thread if it comes back after a release. Up to 255 threads per task.

Timestamps are the log's UTC seconds. Two events in the same second have the same timestamp; a phase that opens and closes within one second is drawn as a mark.

### Open spans

Orbit's ring is append-only and a span needs a duration, so work that is still running cannot be drawn as one span until it ends. The bridge draws it as adjacent **segments**: every `--segment` seconds (default 10) each open span gets a segment from where the previous one ended to now, and the close emits the remainder. On the timeline that reads as a bar that grows while the agent works. `--segment 0` turns this off and draws each span once, when it ends.

## The mechanism

### Orbit side: `POST /api/events`

Orbit's Rust service (`orbit-live-server`, embedded in `orbit-service`) already had `POST /api/scope`, a one-scope-at-a-time interface that files everything under a single synthetic `agents` process. That cannot name processes or threads, so the Orbit PR adds `POST /api/events` in `src/OrbitLiveViewer/crates/orbit-live-server/src/ingest.rs`:

```json
{
  "clock": "unix_ns",
  "processes": [{"pid": 1895825664, "name": "#21 Link q with Orbit"}],
  "threads":   [{"pid": 1895825664, "tid": 1895825665, "name": "claude-fable-worker-8"}],
  "spans":     [{"pid": 1895825664, "tid": 1895825665, "name": "claimed", "start_ns": 1790481600000000000, "duration_ns": 70000000000, "depth": 0}],
  "instants":  [{"pid": 1895825664, "tid": 1895825665, "name": "heartbeat", "timestamp_ns": 1790481620000000000, "depth": 2}],
  "values":    [{"pid": 1895825664, "tid": 1895825664, "name": "progress %", "timestamp_ns": 1790481640000000000, "value": 40}]
}
```

Every list is optional. Names go to the service's process and thread name tables (and are replayed to viewers that connect later). Spans become `API_SCOPE` events (or `API_TRACK` with `"track": "async"`), instants are zero-length scopes, values are `VALUE` samples. Names are interned once per request. The reply is `{"accepted", "dropped_before_start", "named", "monotonic_now_ns", "capture_start_ns"}`.

Orbit timestamps are `CLOCK_MONOTONIC`. With `"clock": "unix_ns"` the service converts wall-clock timestamps using the offset between the two clocks at the time of the request, so the bridge does not need to know when the Orbit machine booted and can run on another host. A timestamp from before boot becomes 0.

Two things about Orbit to know: events that start before a running capture began are refused (`dropped_before_start` counts them, and the bridge prints that count), and pressing **Record** clears the ring and the names. The bridge re-sends every known name every 30 seconds, so rows get their names back; events sent before Record are gone, as they are for `/api/scope`.

The existing viewer renders all of this without changes: process and thread names, stacked scopes, value lanes. No viewer rebuild is needed.

### q side

- `QueueService::events_since(after_id, limit)` is a new method: the queue-wide event feed, oldest first, with the event id as cursor. It is implemented by the SQLite store and by `RemoteQueue`, and served by `q serve` as `POST /v1/events_since`, so the bridge works against a local file or a remote authority the same way.
- `crates/q-orbit` holds the bridge. `mapper.rs` is pure (events in, records out, with the open-span state); `wire.rs` is the request body; `client.rs` posts it with `ureq`; `bridge.rs` runs the poll loop, fetches a task's title and dependencies with `get` the first time it sees the task, and keeps an unsent batch when Orbit is down.
- `q orbit` in the CLI wires it up.

## Running it

Start Orbit's Rust service (from the Orbit repo, on a branch that has `POST /api/events`):

```bash
./rust.sh                        # http://127.0.0.1:44766/
```

Open the viewer in a browser, then start the bridge from any machine that can reach both:

```bash
q orbit                                  # local database, Orbit at http://127.0.0.1:44766
q orbit --url http://orbit-host:44766    # or Q_ORBIT_URL=...
q --server https://q.example.com orbit   # tail a remote q serve
q orbit --history all                    # draw the whole log, not just the last hour
q orbit --history 0 --segment 5 -i 1     # only what happens from now on, tight segments
q orbit --once --json                    # one pass, machine-readable summary
```

Flags: `--url` (falls back to `$Q_ORBIT_URL`, then `http://127.0.0.1:44766`), `-i/--interval` seconds between polls (default 2), `--history` how much of the past to draw at start (`1h` default; `30m`, `2d`, `all`, or `0`), `--segment` seconds between segments of an open span (default 10; `0` disables), `--once`. Events older than `--history` still build the state, so a claim that started three hours ago is drawn from its real start on the first segment.

The command prints one line per push (`pushed 12 events, 4 names (accepted 12)  3 open spans, 1 active claims`) and a summary on exit. `--json` prints only the summary. A bad URL or a dead service fails at start; an outage after that is logged and the batch is retried on the next poll, bounded at 100,000 pending events.

Verified end to end on 2026-09-27: a scratch database driven through capture, ready, claim, start, heartbeat, `[explore] @begin` / `@end` notes, an artifact and completion, pushed by `q orbit --once` to an `orbit-service` built from the Orbit PR on port 44799. Orbit accepted 11 events on the first pass and 18 on the second (`events_live` 29, `dropped_before_start` 0), and named the task process, the `task`, `claude-e2e-1` and `explore` threads, and the `q queue` process.

## Not done

- **Dependency edges as arrows.** Orbit's wire protocol has no edge frame and the live viewer only draws flow arrows for Chrome JSON traces loaded in the browser. Dependencies are shown as `waits on #dep` spans instead. Arrows need a new `LiveFrame` in `orbit-live-protocol`, storage and replay in `LiveService`, a decoder path in the viewer, and a WASM rebuild (`src/OrbitLiveViewer/build_wasm.sh`, which installs a nightly toolchain).
- **Open spans as one box.** Segments are the append-only workaround. Orbit could add an "amend duration" event or let the viewer merge adjacent same-name segments.
- **Sub-second timestamps.** The event log keeps whole seconds, so bursts collapse onto one instant. Storing fractional seconds in `events.created_at` would spread them out.
- **First-class sub-agents.** The `[name]` and `@begin`/`@end` convention lives in the bridge. A `--thread` / `--begin` / `--end` form of `q log` (or a `queue_log` field) would make it explicit and validated.
- **No MCP surface.** `q orbit` is a CLI command for a human's screen; agents do not need to call it.
- **Reverse direction.** Nothing from Orbit flows back into the queue (for example a Record press could be logged on the task).
