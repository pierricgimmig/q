# q go-to-market plan

Status: proposal, 2026-09. Owner: Pierric. Scope: launch and first 90 days of an
open-source developer tool with one maintainer and no budget. Everything here
is a plan, not a result. No adoption numbers exist yet; targets are labelled as
targets.

## 1. Summary

- q is a local-first SQLite work queue that sits between a human's ideas and a
  fleet of coding agents. Humans capture and gate; agents claim, lease,
  heartbeat, and complete; one binary serves CLI, MCP, and HTTP.
- The wedge is one specific pain: people already run several Claude Code, Codex,
  or Cursor sessions in parallel and coordinate them with chat scrollback,
  TODO.md files, and memory. q replaces that with an atomic queue and a
  human-only `ready` gate.
- Launch is gated on three things this repo does not have yet: a `LICENSE`
  file, prebuilt binaries, and a 30-second demo gif. Ship those, then launch in
  one concentrated week, then publish one piece of content per week for 90 days.
- Success at day 90 is a tool people other than the author run daily, not a star
  count. Stars are a proxy; `q claim` calls from strangers' agents are the goal.

## 2. Positioning

### One-line pitch

> q is a local-first work queue for coding agents: you capture tasks, you mark
> them ready, idle agents claim them atomically over CLI or MCP.

### Elevator pitch (30 seconds)

> If you run more than one coding agent at a time, you already have a queue, it
> is just spread across chat windows and a TODO file. q makes it explicit. Type
> `q "fix the flaky test"` and it lands in an inbox. Run `q ready 12` and an
> idle agent can claim it, with a lease, a heartbeat, and a token so two agents
> never take the same task. It is one Rust binary over SQLite, it speaks MCP so
> Claude Code and Cursor use it natively, and `q serve` turns the same file into
> a shared authority for agents on other machines. It never launches agents,
> opens PRs, or touches GitHub. It is the queue, and only the queue.

### Candidate taglines

| Tagline | Use |
|---|---|
| The work queue for your coding agents. | README hero, repo description |
| Capture. Ready. Claim. | Logo lockup, terminal gif title card |
| Your agents are idle. Give them a queue. | Show HN, social |
| One binary. One SQLite file. No agent takes the same task twice. | Technical audiences, r/rust |
| Humans decide what is ready. Agents decide who does it. | Safety pillar, talks |

Recommendation: first one for the README, third for launch posts.

### What q is not (say this early and often)

- Not an orchestrator: it does not spawn agents, create worktrees, or merge.
- Not an issue tracker: no comments, assignees, sprints, or web UI.
- Not a sync service: `q serve` is one authority, not replicas.
- Not a hosted product: nothing phones home.

Stating the non-goals is a feature. It is what distinguishes q from every
"agent platform" and it is why a solo maintainer can keep it correct.

## 3. Problem and alternatives

The problem: multi-agent coding is now normal, and coordination is the missing
layer. People run 3 to 10 agent sessions and lose track of who is doing what,
what has been approved, and what got dropped when a session died.

| Alternative | What it does well | Where q wins | Honest gap for q |
|---|---|---|---|
| TODO.md / plain text | Zero setup, in-repo | Atomic claim, leases, event log, per-repo discovery | Text file is greppable and diffable in PRs |
| Claude Code TodoWrite / plan mode | Native, per-session | Survives the session, shared across agents and tools, human gate | Only useful once you run more than one session |
| Codex / Cursor background task queues | Integrated with the vendor's runner | Vendor neutral, local, one queue across Claude, Codex, Cursor, Grok | No runner: q does not start the agent for you |
| Beads and similar agent-native issue systems | Git-backed, rich task graph, agent memory | Single binary, SQLite atomicity, lease and token model, HTTP authority, no git merge conflicts on task state | Fewer fields, no git history of task state |
| GitHub Issues | Everyone has it, discussion, links to PRs | Local, sub-second, no rate limits, inbox that agents cannot see, private by default | No comments, no web UI, no cross-team visibility |
| Linear / Jira | Team process, reporting | Zero accounts, agents as first-class actors, risk gating | Not a team planning tool and should not try to be |
| Taskwarrior | Mature personal CLI, filters, recurrence | Multi-actor claims, MCP, agent safety model | Taskwarrior has 15 years of UX polish for humans |
| Celery / RabbitMQ / SQS | Real distributed queues | No broker, human inbox and ready gate, task bodies for agents | Not for high throughput and never will be |
| Custom scripts + `flock` | Works for one person | Already written, tested on three OSes, event log, recovery | Some people will always prefer their own 40 lines |

Positioning against each: never argue q is better in general. Argue it is the
right size for one specific job, and name the alternative's job honestly.

## 4. Target audiences

| Segment | Who | Pain today | Entry point | Message |
|---|---|---|---|---|
| A. Parallel-agent developer | Solo dev running 2 to 10 Claude Code / Codex / Cursor sessions | Coordination lives in their head; dead sessions drop work | `cargo install`, `q skill install`, MCP config | "Stop being the queue for your agents." |
| B. Small team on shared agents | 2 to 6 engineers with a shared VPS running agents | Two agents grab the same task; no audit of what an agent did | `q serve` with token roles | "One authority. Human tokens gate, agent tokens work." |
| C. Agent-fleet builder | People building their own runners, harnesses, and loops | Reinventing leases, heartbeats, and stale recovery | `QueueService` trait, `--json`, HTTP wire format | "The claim transaction you were about to write, already tested." |
| D. Rust CLI community | People who read r/rust and cargo release notes | Curiosity, not pain | Code layout, `BEGIN IMMEDIATE`, one SQL crate | "A small, boring, correct Rust workspace." |

Priority order: A, then C, then B. Segment A is the fastest to reach and the
most likely to file useful issues. Segment B needs a shared server and TLS,
which is a bigger ask. Segment D gives stars but few daily users; treat it as
amplification, not as the target.

## 5. Messaging pillars

1. Human gate. Inbox is never claimable. Only `q ready` makes work claimable,
   and the server enforces that agent tokens cannot call it. Lead with this;
   it is the line that makes people trust the tool with agents.
2. Atomic by construction. One `BEGIN IMMEDIATE` transaction recovers expired
   leases, selects, claims, and logs. No two agents take one task. Show the
   transaction, not a diagram.
3. One binary, one file. CLI, MCP, and HTTP call the same service trait. The
   only SQL is in `q-store`. Nothing to deploy for the local case.
4. Vendor neutral. Skill install targets `claude`, `cursor`, `codex`, and
   `agents`. MCP works with anything that speaks stdio JSON-RPC.
5. Knows its limits. Does not launch agents, open PRs, or call GitHub. Risk
   levels and `allow_external_actions` default to off. Say this as a strength.

Every post, tweet, and README section should map to one pillar. If a piece of
content does not, cut it.

## 6. Naming and branding notes

- `q` is a strong CLI name and a weak brand name. Collisions: Amazon Q (AWS
  assistant, dominates search), kdb+/q (language), `q` text-as-data tool (has
  the Homebrew `q` formula), `q` shell aliases in many dotfiles.
- Keep the binary `q`. Do not rename it; typing it 50 times a day is the point.
- Give the project a searchable long name for titles, packages, and URLs.
  Candidates: `q-queue`, `qagent`, `agentq`, `qwork`. Recommend `q-queue`
  for crates.io, Homebrew tap, and GitHub description: "q-queue: the `q` work
  queue for coding agents". Check crates.io availability before deciding; a
  one-letter crate name is almost certainly unavailable.
- Consistent phrasing everywhere: "work queue for coding agents". Never "task
  manager", never "orchestrator", never "platform".
- Visual identity: none needed beyond a terminal gif with the real colored
  `q ls` output and a monospace wordmark. Do not commission a logo before 1k
  users.
- Domain: only if the landing page (task #17) ships. `qqueue.dev` or a GitHub
  Pages URL is fine. Do not buy `q.*` anything.
- Repo description and topics: `agent-work-queue`, `mcp-server`, `claude-code`,
  `codex`, `cursor`, `sqlite`, `rust-cli`, `local-first`.

## 7. Pre-launch checklist

Nothing below is optional. Launching without these wastes the one-shot channels
(Show HN, r/rust).

| Item | Why | Effort |
|---|---|---|
| Add `LICENSE` (MIT, matches Cargo.toml) | Repo currently has no license file; many people will not install | 5 min |
| Release workflow producing binaries for linux x86_64/aarch64, macOS arm64/x86_64, Windows x86_64 | "cargo build from source" loses most of segment A | 2 to 4 h with `cargo-dist` or a hand-written matrix |
| `cargo-binstall` metadata in `q-cli/Cargo.toml` | One-command install without compiling | 30 min once releases exist |
| Tag `v0.1.0` and write release notes | People link to versions, not commits | 30 min |
| README hero rewrite (Appendix A) | Current README opens with architecture, not with the problem | 1 h |
| 30-second gif: `q "..."`, `q ready`, `q claim --json` from two terminals, `q top` | The single most-shared asset | 2 h with `vhs` or `asciinema` + `agg` |
| Quickstart: install to first claim in five commands | Time-to-first-claim under 3 min | 1 h |
| GitHub Discussions enabled, 3 issue templates, 5 `good first issue` labels | Somewhere for people to land | 1 h |
| Landing page (task #17) if ready; not a blocker | Nice for social previews | separate task |

Do the gif last. The gif should show the real product after the README has
been tightened.

## 8. Launch plan by channel

Launch in one week, Tuesday to Thursday, after the checklist is green.

| Channel | Format | Timing | Notes |
|---|---|---|---|
| GitHub README | Hero, gif, quickstart, comparison table | Day 0 | Everything else links here |
| Show HN | "Show HN: q, a local-first work queue for coding agents (Rust, SQLite, MCP)" | Tue 8 to 9 am ET | Body: the problem in 3 sentences, what it does not do, one gif link. Reply to every comment within the hour for 6 hours |
| r/rust | "q: a single-binary SQLite work queue for coding agents" | Wed | Lead with architecture: service trait, one SQL crate, `BEGIN IMMEDIATE`. r/rust rewards code talk, not pitch |
| r/ClaudeAI, r/ChatGPTCoding, r/cursor | "How I run N Claude Code sessions without them stepping on each other" | Thu | Lead with the workflow, mention q in the second paragraph. Do not lead with the tool |
| X/Twitter | Thread: gif, 5 tweets, one per pillar | Tue after HN post | Tag nothing. Reply to people who quote it |
| Bluesky | Same thread, plainer wording | Tue | Rust and OSS audience is disproportionately there |
| dev.to / personal blog | Long post: "Why a queue, not an orchestrator" | Wed | Canonical URL on the blog, cross-post to dev.to |
| Hacker News comments elsewhere | Mention when relevant in threads about agent coordination | Ongoing | Only when it answers the question asked |
| MCP registries | Submit to the official MCP registry, `mcp.so`, `glama.ai`, Smithery, PulseMCP, `awesome-mcp-servers` | Week 1 | Each has a form or PR. Include the `q mcp` config block verbatim |
| Claude Code plugin / skill marketplace | Package `q skill` as a Claude Code plugin; publish a marketplace repo | Week 1 to 2 | Verify current plugin format before building |
| awesome-lists | `awesome-rust` (Applications > Utilities), `awesome-claude-code`, `awesome-cursorrules` or equivalents, `awesome-ai-agents` | Week 2 | One PR each, exact format of each list |
| Orbit network | Profiler users and game-dev tooling followers already know the author | Day 0 | One honest post: "same author, different problem". This is the warmest audience |
| Rust newsletters | This Week in Rust "Crate of the Week" nomination; Rust Trending | Week 2 | Nominate yourself; it is allowed |
| Podcasts / streams | Offer a 20-minute live demo to Rust and AI-coding streams | Month 2 to 3 | Only after a stable release |

What not to do at launch: Product Hunt (wrong audience for a CLI), paid
anything, cold DMs, and announcing on more than three channels the same day.

### Show HN draft

Title: Show HN: q, a local-first work queue for coding agents (Rust, SQLite, MCP)

> I run several Claude Code and Codex sessions in parallel and kept losing
> track of who was doing what. q is the queue I wanted: `q "title"` puts a
> task in an inbox, `q ready ID` makes it claimable, and any idle agent runs
> `q claim` or calls the `queue_claim_next` MCP tool. Claims are one
> `BEGIN IMMEDIATE` SQLite transaction with a lease, a heartbeat, and an
> opaque token, so two agents cannot take the same task. `q serve` exposes
> the same file over HTTP for agents on other machines, with human and agent
> token roles so an agent can never mark work ready. It deliberately does not
> launch agents, open PRs, or call GitHub. One Rust binary, MIT.

## 9. 90-day content calendar

One piece per week. Each maps to a pillar and a segment. Formats rotate so
the same story is not told the same way twice.

| Week | Piece | Pillar | Segment | Channel |
|---|---|---|---|---|
| 0 | Launch: README, gif, Show HN, r/rust, thread | all | A, D | HN, Reddit, X, Bluesky |
| 1 | "Five commands from install to first claim" | 3 | A | Blog, dev.to |
| 2 | "Why agents should never be able to mark work ready" | 1 | A, B | Blog, X |
| 3 | "The claim transaction, annotated" (walk through `q-store` claim code) | 2 | C, D | Blog, r/rust |
| 4 | Video: two Claude Code sessions sharing one queue via MCP | 4 | A | YouTube short, X |
| 5 | "q serve: one authority for a VPS full of agents" with Caddy config | 3 | B | Blog |
| 6 | Comparison post: TODO.md, Beads, GitHub Issues, q, when to use which | all | A | Blog, HN comment fodder |
| 7 | "Risk levels and external_action: letting agents near production safely" | 5 | B, C | Blog |
| 8 | Release `v0.2` with the top three community requests; release notes as a post | 3 | all | GitHub, X |
| 9 | Guest workflow post: how a user (with consent) runs q; if none exist yet, the author's own Orbit workflow | 4 | A | Blog |
| 10 | "Features and `q tree`: dependency graphs agents can actually follow" | 2 | A, C | Blog, gif |
| 11 | Talk proposal submitted to one Rust meetup and one AI-engineering meetup | all | D | Meetup |
| 12 | 90-day retrospective: what was used, what was ignored, what is next | all | all | Blog, HN |

Rules: every post has one runnable code block, one screenshot or gif, and one
line on what q does not do. Under 900 words. No post about AI in general.

## 10. Community

- GitHub Discussions on, with categories: Q&A, Show your setup, Ideas. Close
  issues that are questions and move them to Discussions politely.
- Issue templates: bug (with `q --version`, OS, `q status` output), feature
  (with "which alternative did you consider"), agent-integration (which client,
  MCP config redacted).
- Labels: `good first issue`, `help wanted`, `agent-client`, `q-serve`,
  `docs`, `needs-repro`. Seed at least five `good first issue` items before
  launch: examples in the README for each skill target, a `q ls --sort` flag,
  shell completions, a `CONTRIBUTING.md`, Windows path tests.
- Response target: acknowledge every issue and PR within 48 hours for the
  first 90 days. Say so in `CONTRIBUTING.md`; it is a differentiator for solo
  projects.
- No Discord until there are more than ~20 weekly-active users. Discord is a
  time sink for one maintainer and hides knowledge from search. GitHub
  Discussions is enough. Revisit at day 90.
- Recognize contributors in release notes by handle. Cheap and remembered.
- `CODE_OF_CONDUCT.md`: Contributor Covenant, unchanged.

## 11. Distribution

| Channel | Priority | Work | Status |
|---|---|---|---|
| GitHub Releases with prebuilt binaries | 1 | `cargo-dist` or a release matrix in `.github/workflows` | Missing |
| `cargo binstall q-queue` | 1 | `[package.metadata.binstall]` pointing at release assets | Missing |
| `cargo install q-queue` (crates.io) | 2 | Rename publishable crate, drop `publish = false`, keep binary `q` via `[[bin]] name = "q"` | Missing; name TBD |
| Homebrew tap `pierricgimmig/tap` | 2 | Formula generated by `cargo-dist` or hand-written; `q` core formula is taken | Missing |
| Shell installer `curl ... \| sh` | 3 | `cargo-dist` generates one | Missing |
| Nix flake | 3 | Community will likely send it; accept the PR | Missing |
| Scoop / winget | 3 | After Windows binaries exist and someone asks | Missing |
| Docker image for `q serve` | 3 | Small distroless image, `ghcr.io/pierricgimmig/q` | Missing |
| Claude Code plugin / skill marketplace entry | 2 | Packages `q skill` and MCP config | Missing |
| MCP registries | 2 | Listing PRs and forms | Missing |

Versioning: semver, `0.x` until the wire format and MCP tool schemas are
frozen. State clearly in the README what is stable (CLI flags, MCP tool names,
`/v1` HTTP wire) and what is not (SQLite schema, Rust crate APIs).

## 12. Metrics

Track weekly in a private spreadsheet. None of these exist today; all
baselines are zero.

| Metric | Source | Why |
|---|---|---|
| Release downloads per asset | GitHub API | Only true install signal |
| `cargo binstall` / crates.io downloads | crates.io | Second install signal |
| Stars, forks, watchers | GitHub | Reach proxy, not usage |
| Unique cloners | GitHub Insights (14-day window; record weekly) | Developer interest |
| Issues and Discussions opened by non-author | GitHub | Real users hit real edges |
| PRs by non-author | GitHub | Contributor funnel |
| MCP registry listing views / installs where exposed | Each registry | Agent-client channel health |
| Referrers to the README | GitHub Insights traffic | Which channels work |
| Posts mentioning q by others | Search, HN Algolia | Word of mouth |

There is no telemetry in q and there should not be. Do not add it. Usage is
inferred from downloads and conversations only.

Day-90 targets (goals, not predictions): binaries downloaded from at least
three OSes; at least ten issues or discussions from people other than the
author; at least three external PRs merged; at least one public write-up by
someone else; one confirmed team using `q serve`.

## 13. Risks

| Risk | Likelihood | Impact | Mitigation |
|---|---|---|---|
| Agent vendors ship a native cross-session queue | High | High | Stay vendor-neutral and local; the human gate and HTTP authority are the moat. Integrate with theirs rather than compete |
| Name collision with Amazon Q buries search | High | Medium | Long name `q-queue` in titles, packages, and URLs; consistent phrase "work queue for coding agents" |
| Solo-maintainer bus factor scares teams | Medium | Medium | MIT, small codebase, clear architecture doc, tests on three OSes. Say "small enough to fork" out loud |
| Launch before binaries exist | Medium | High | Section 7 is a hard gate |
| Feature-request sprawl toward an issue tracker | High | Medium | Keep the "what q is not" list in the README; close with a link to it |
| Schema or wire changes break early adopters | Medium | Medium | Migrations already exist; document stability tiers; changelog every release |
| HN or Reddit lands flat | Medium | Low | The 90-day calendar does not depend on one post. Repost to r/rust after v0.2 with new material |
| Security report on `q serve` | Low | High | `SECURITY.md` with a private contact; TLS-behind-proxy stance already documented |
| Maintainer burnout from support | Medium | High | 48-hour acknowledge, not 48-hour fix. No Discord. One content piece per week, not more |
| Windows edge cases (paths, editor, colors) | Medium | Low | CI already runs Windows; ask for Windows testers in the launch post |

## 14. Decisions this plan asks for

1. Long name for packages and titles (recommend `q-queue`).
2. `cargo-dist` versus hand-written release matrix (recommend `cargo-dist`).
3. Launch week, contingent on section 7.
4. Whether the landing page (task #17) blocks launch (recommend no).

---

## Appendix A: suggested README hero rewrite

Replace everything above the current `## Install` heading with the block
below. Keep every existing section after it. README.md itself is not changed
by this document.

````markdown
# q

**The work queue for your coding agents.**

You capture tasks. You mark them ready. Idle agents claim them, atomically,
over the CLI or MCP. One Rust binary, one SQLite file, nothing to deploy.

[gif: two terminals; left runs `q "Benchmark trace encoding"` then `q ready 12`;
right runs `q claim --agent codex-01 --json` and receives the task; `q top` below]

```bash
cargo binstall q-queue          # or: brew install pierricgimmig/tap/q
q "Benchmark trace encoding variants"       # lands in inbox
q ready 1                                    # now claimable
q claim --agent claude-local-01 --json       # from any agent, any machine
q skill install                              # teach Claude Code, Cursor, Codex
```

## Why

Running several Claude Code, Codex, or Cursor sessions means you are the queue:
you remember what is approved, who took what, and what died with a closed
terminal. q makes that explicit and safe.

- **Humans gate.** Inbox is never claimable. Only `q ready` makes work
  claimable, and agent tokens cannot call it.
- **Claims are atomic.** One `BEGIN IMMEDIATE` transaction recovers stale
  leases, picks one task, and issues a token. Two agents never take the same
  task.
- **One binary, three surfaces.** CLI, `q mcp` (stdio), and `q serve` (HTTP)
  call the same service. The only SQL lives in one crate.
- **Vendor neutral.** `q skill install` targets Claude, Cursor, Codex, and
  `~/.agents`. Any MCP client works.
- **Knows its limits.** q does not launch agents, create worktrees, open PRs,
  merge, or call GitHub. Risky and external-action tasks are excluded from
  claims by default.

## What q is not

Not an orchestrator, not an issue tracker, not a sync service, not hosted.
See [Compared to alternatives](#compared-to-alternatives).

## Quickstart

<five commands from install to first `q complete`, with expected output>
````

Then the existing sections follow: Install (build from source moves below the
binary install), Database, Remote server, Capture and discovery, and so on. Add
a short "Compared to alternatives" section using the table in section 3 of this
document, trimmed to the five rows most readers ask about: TODO.md, Claude
Code's TodoWrite, GitHub Issues, Beads, Taskwarrior.

## Appendix B: social thread draft (X / Bluesky)

1. Your agents are idle. Give them a queue. q is a local-first work queue for
   coding agents: capture, ready, claim. One Rust binary over SQLite. [gif]
2. The rule that matters: inbox is never claimable. You run `q ready`. Agents
   cannot, even over HTTP with an agent token.
3. Claims are one `BEGIN IMMEDIATE` transaction with a lease and a token. Two
   Claude Code sessions, one task, zero collisions.
4. `q mcp` speaks stdio to Claude Code, Cursor, Codex, anything. `q skill
   install` drops the skill into all of them.
5. `q serve` turns the same file into a single authority for agents on other
   machines. Human and agent token roles. No replicas, no sync.
6. It does not launch agents, open PRs, or call GitHub. That is the point.
   MIT. github.com/pierricgimmig/q
