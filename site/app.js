/* q landing page: an in-browser simulation of the q CLI.
 *
 * Structure:
 *   1. model    — Queue: the in-memory state machine (mirrors q-core/q-store rules)
 *   2. render   — turns queue data into the same text the real CLI prints, with color spans
 *   3. shell    — tokenizer, flag parser, and one handler per `q` subcommand
 *   4. agents   — fake workers that poll `claim`, start, heartbeat, and complete on timers
 *   5. ui       — terminal widget, history, tab completion, hints, counts, agent panel
 *
 * No frameworks, no build step, no network. Everything lives in this file.
 */
(() => {
  'use strict';

  // ───────────────────────────── 1. model ─────────────────────────────

  const STATUSES = ['inbox', 'ready', 'claimed', 'in_progress', 'review', 'blocked', 'done', 'cancelled'];
  const KINDS = ['implementation', 'research', 'review', 'benchmark', 'documentation', 'other'];
  const RISKS = ['low', 'medium', 'high', 'external_action'];
  const RISK_RANK = { low: 0, medium: 1, high: 2, external_action: 3 };
  const DEFAULT_LEASE_MIN = 45;
  const TITLE_MAX_CHARS = 64;
  const TREE_STATUS_WIDTH = 11;

  // crates/q-core/src/transition.rs
  const TRANSITIONS = new Set([
    'inbox>ready', 'inbox>blocked', 'inbox>cancelled',
    'ready>claimed', 'ready>blocked', 'ready>cancelled',
    'claimed>in_progress', 'claimed>ready', 'claimed>blocked',
    'in_progress>review', 'in_progress>done', 'in_progress>blocked', 'in_progress>ready',
    'review>done', 'review>in_progress', 'review>blocked',
    'blocked>inbox', 'blocked>ready', 'blocked>cancelled',
    'done>ready', 'cancelled>inbox',
  ]);

  class QueueError extends Error {
    constructor(code, message) { super(message); this.code = code; }
    static notFound(id) { return new QueueError('not_found', `task ${id} not found`); }
    static transition(from, to) { return new QueueError('invalid_transition', `invalid transition from ${from} to ${to}`); }
    static token() { return new QueueError('token_mismatch', 'claim token does not match an active claim'); }
    static expired() { return new QueueError('claim_expired', 'claim lease has expired'); }
    static input(msg) { return new QueueError('invalid_input', msg); }
    static conflict(msg) { return new QueueError('conflict', msg); }
  }

  const uuid = () => (crypto.randomUUID ? crypto.randomUUID()
    : 'xxxxxxxx-xxxx-4xxx-yxxx-xxxxxxxxxxxx'.replace(/[xy]/g, (c) => {
      const r = Math.random() * 16 | 0;
      return (c === 'x' ? r : (r & 0x3) | 0x8).toString(16);
    }));

  const iso = (d) => new Date(d).toISOString().replace(/\.\d{3}Z$/, 'Z');

  // crates/q-cli/src/style.rs format_relative
  function relative(then, now) {
    const delta = Math.floor((now - then) / 1000);
    const future = delta < 0;
    const s = Math.abs(delta);
    if (s < 45) return 'just now';
    let body;
    if (s < 60) body = `${s}s`;
    else if (s < 3600) body = `${Math.floor(s / 60)}m`;
    else if (s < 86400) body = `${Math.floor(s / 3600)}h`;
    else if (s < 30 * 86400) body = `${Math.floor(s / 86400)}d`;
    else if (s < 365 * 86400) body = `${Math.floor(s / (30 * 86400))}mo`;
    else body = `${Math.floor(s / (365 * 86400))}y`;
    return future ? `in ${body}` : `${body} ago`;
  }

  function truncateChars(text, max) {
    const chars = Array.from(text);
    if (chars.length <= max) return text;
    return chars.slice(0, max - 1).join('') + '…';
  }

  const flatTitle = (title) => truncateChars(title.split(/\s+/).filter(Boolean).join(' '), TITLE_MAX_CHARS);

  const slug = (title) => title.toLowerCase().replace(/[^a-z0-9]+/g, '-').replace(/^-+|-+$/g, '').split('-').slice(0, 4).join('-');

  class Queue {
    constructor() { this.reset(); }

    reset() {
      this.tasks = new Map();
      this.features = new Map();
      this.nextTaskId = 1;
      this.nextEventId = 1;
      this.nextClaimId = 1;
      this.nextFeatureId = 1;
      this.listeners = this.listeners || [];
      this.recent = []; // for `q top` recent changes
    }

    onChange(fn) { this.listeners.push(fn); }
    changed(kind, task, extra = {}) {
      const entry = { at: Date.now(), kind, id: task ? task.id : null, title: task ? task.title : '', ...extra };
      this.recent.unshift(entry);
      this.recent.length = Math.min(this.recent.length, 10);
      for (const fn of this.listeners) fn(entry);
    }

    now() { return Date.now(); }

    // ── features
    createFeature({ title, body = null }) {
      const id = this.nextFeatureId++;
      const f = { id, title, body, created_at: this.now() };
      this.features.set(id, f);
      return f;
    }
    findFeature(ref) {
      if (ref == null) return null;
      const asId = Number(ref);
      if (Number.isInteger(asId) && this.features.has(asId)) return this.features.get(asId);
      const matches = [...this.features.values()].filter((f) => f.title.toLowerCase() === String(ref).toLowerCase());
      if (matches.length === 1) return matches[0];
      if (matches.length > 1) throw QueueError.input(`feature title "${ref}" matches ${matches.length} features; pass the id`);
      throw new QueueError('not_found', `feature not found: ${ref}`);
    }

    // ── tasks
    get(id) {
      const t = this.tasks.get(Number(id));
      if (!t) throw QueueError.notFound(id);
      return t;
    }

    event(task, type, actorType, actorId, payload = {}) {
      const ev = { id: this.nextEventId++, task_id: task.id, event_type: type, actor_type: actorType, actor_id: actorId, payload, created_at: this.now() };
      task.events.push(ev);
      return ev;
    }

    transition(task, to, eventType, actor, payload = {}) {
      const from = task.status;
      if (!TRANSITIONS.has(`${from}>${to}`)) throw QueueError.transition(from, to);
      task.status = to;
      task.updated_at = this.now();
      this.event(task, eventType, actor.type, actor.id, { from, to, ...payload });
      this.changed(`[${from}] -> [${to}]`, task);
      return task;
    }

    capture({ title, kind = 'implementation', risk = 'low', priority = 0, project = 'q', repo = 'github.com/pierricgimmig/q', feature = null, body = null, dependsOn = [], actor = HUMAN }) {
      title = title.trim();
      if (!title) throw QueueError.input('title must not be empty');
      if (!KINDS.includes(kind)) throw QueueError.input(`unknown kind: ${kind} (expected one of ${KINDS.join(', ')})`);
      if (!RISKS.includes(risk)) throw QueueError.input(`unknown risk: ${risk} (expected one of ${RISKS.join(', ')})`);
      const f = feature == null ? null : this.findFeature(feature);
      for (const dep of dependsOn) this.get(dep);
      const id = this.nextTaskId++;
      const now = this.now();
      const task = {
        id, public_id: uuid(), title, original_capture: title, body, status: 'inbox', kind, priority, risk,
        project, repo, feature_id: f ? f.id : null, capture_path: '/home/you/src/' + (project || 'scratch'),
        depends_on: [...dependsOn], required_capabilities: [], created_at: now, updated_at: now,
        claim: null, retired_claims: [], artifacts: [], events: [], summary: null,
      };
      this.tasks.set(id, task);
      this.event(task, 'task_created', actor.type, actor.id, { status: 'inbox', kind, risk, project, repo, title });
      this.changed('added', task);
      return task;
    }

    ready(id, actor = HUMAN) {
      const task = this.get(id);
      return this.transition(task, 'ready', 'task_ready', actor);
    }

    edit(id, patch) {
      const task = this.get(id);
      if (patch.priority != null) task.priority = patch.priority;
      if (patch.risk != null) { if (!RISKS.includes(patch.risk)) throw QueueError.input(`unknown risk: ${patch.risk}`); task.risk = patch.risk; }
      if (patch.kind != null) { if (!KINDS.includes(patch.kind)) throw QueueError.input(`unknown kind: ${patch.kind}`); task.kind = patch.kind; }
      if (patch.title != null) task.title = patch.title;
      if (patch.body !== undefined) task.body = patch.body;
      if (patch.feature !== undefined) task.feature_id = patch.feature == null ? null : this.findFeature(patch.feature).id;
      if (patch.dependsOn) { for (const d of patch.dependsOn) { this.get(d); if (d === task.id) throw QueueError.input('a task cannot depend on itself'); } task.depends_on = [...new Set(patch.dependsOn)]; }
      task.updated_at = this.now();
      this.event(task, 'task_edited', HUMAN.type, HUMAN.id, {});
      this.changed('edited', task);
      return task;
    }

    requireToken(task, token) {
      if (!task.claim) throw QueueError.token();
      if (task.claim.token !== token) throw QueueError.token();
      if (task.claim.lease_expires_at <= this.now()) throw QueueError.expired();
      return task.claim;
    }

    retireClaim(task) {
      if (task.claim) { task.claim.active = false; task.retired_claims.push(task.claim); task.claim = null; }
    }

    block(id, token, actor = HUMAN) {
      const task = this.get(id);
      if (task.claim) { this.requireToken(task, token); this.retireClaim(task); }
      return this.transition(task, 'blocked', 'task_blocked', actor);
    }

    cancel(id, actor = HUMAN) {
      const task = this.get(id);
      return this.transition(task, 'cancelled', 'task_cancelled', actor);
    }

    delete(id, force = false) {
      const task = this.get(id);
      const activeClaim = task.claim && task.claim.lease_expires_at > this.now();
      if (activeClaim && !force) throw QueueError.conflict(`task ${id} has an unexpired claim; pass --force to clear it`);
      let deps = 0;
      for (const other of this.tasks.values()) {
        const before = other.depends_on.length;
        other.depends_on = other.depends_on.filter((d) => d !== task.id);
        deps += before - other.depends_on.length;
      }
      this.tasks.delete(task.id);
      this.changed('deleted', task);
      return {
        task_id: task.id, title: task.title, status: task.status, active_claim_cleared: !!activeClaim,
        claims_removed: task.retired_claims.length + (task.claim ? 1 : 0), events_removed: task.events.length,
        artifacts_removed: task.artifacts.length, dependencies_removed: task.depends_on.length + deps,
      };
    }

    reopen(id, actor = HUMAN) {
      const task = this.get(id);
      if (task.status === 'done') return this.transition(task, 'ready', 'task_reopened', actor);
      if (task.status === 'cancelled') return this.transition(task, 'inbox', 'task_reopened', actor);
      throw QueueError.transition(task.status, task.status === 'done' ? 'ready' : 'inbox');
    }

    recoverStale(to = 'ready', actor = HUMAN) {
      const out = [];
      for (const task of this.tasks.values()) {
        if (task.claim && task.claim.lease_expires_at <= this.now() && (task.status === 'claimed' || task.status === 'in_progress')) {
          const previous = task.status;
          this.retireClaim(task);
          this.transition(task, to, 'claim_recovered', actor, { reason: 'lease_expired' });
          out.push({ task_id: task.id, previous_status: previous, new_status: to });
        }
      }
      return out;
    }

    ineligibleReason(task, req) {
      if (task.status !== 'ready') return 'not_ready';
      if (RISK_RANK[task.risk] > RISK_RANK[req.maxRisk]) return `risk ${task.risk} exceeds max-risk ${req.maxRisk}`;
      if (req.kinds.length && !req.kinds.includes(task.kind)) return 'kind filtered';
      if (req.repo && task.repo !== req.repo) return 'repo filtered';
      if (req.project && task.project !== req.project) return 'project filtered';
      if (!task.required_capabilities.every((c) => req.capabilities.includes(c))) return 'missing capability';
      for (const d of task.depends_on) { const dep = this.tasks.get(d); if (!dep || dep.status !== 'done') return `depends on #${d} (${dep ? dep.status : 'deleted'})`; }
      return null;
    }

    claimNext({ agent, capabilities = [], kinds = [], maxRisk = 'medium', leaseMinutes = DEFAULT_LEASE_MIN, repo = null, project = null }) {
      if (!agent) throw QueueError.input('agent id must not be empty');
      if (!RISKS.includes(maxRisk)) throw QueueError.input(`unknown risk: ${maxRisk}`);
      if (leaseMinutes < 1 || leaseMinutes > 1440) throw QueueError.input('lease must be between 1 and 1440 minutes');
      this.recoverStale();
      const req = { capabilities, kinds, maxRisk, repo, project };
      const candidates = [...this.tasks.values()]
        .filter((t) => this.ineligibleReason(t, req) === null)
        .sort((a, b) => b.priority - a.priority || a.updated_at - b.updated_at || a.id - b.id);
      if (!candidates.length) return { found: false, reason: 'no_eligible_ready_tasks' };
      const task = candidates[0];
      const now = this.now();
      const claim = {
        id: this.nextClaimId++, task_id: task.id, agent_id: agent, token: uuid(), claimed_at: now, heartbeat_at: now,
        lease_expires_at: now + leaseMinutes * 60000, lease_minutes: leaseMinutes, active: true, branch: null, worktree: null,
      };
      task.claim = claim;
      this.transition(task, 'claimed', 'task_claimed', { type: 'agent', id: agent }, { agent_id: agent, lease_expires_at: iso(claim.lease_expires_at) });
      return { found: true, task, claim };
    }

    heartbeat(id, token, leaseMinutes = null, actor = null) {
      const task = this.get(id);
      const claim = this.requireToken(task, token);
      const minutes = leaseMinutes == null ? claim.lease_minutes : leaseMinutes;
      if (minutes < 1 || minutes > 1440) throw QueueError.input('lease must be between 1 and 1440 minutes');
      claim.heartbeat_at = this.now();
      claim.lease_expires_at = claim.heartbeat_at + minutes * 60000;
      this.event(task, 'claim_heartbeat', actor ? actor.type : 'agent', actor ? actor.id : claim.agent_id, { lease_expires_at: iso(claim.lease_expires_at) });
      this.changed('heartbeat', task, { quiet: true });
      return claim;
    }

    start(id, token, { branch = null, worktree = null } = {}, actor = null) {
      const task = this.get(id);
      const claim = this.requireToken(task, token);
      if (branch) claim.branch = branch;
      if (worktree) claim.worktree = worktree;
      return this.transition(task, 'in_progress', 'task_started', actor || { type: 'agent', id: claim.agent_id }, { branch, worktree });
    }

    complete(id, token, { summary = null, artifacts = [], target = null } = {}, actor = null) {
      const task = this.get(id);
      if (task.status === 'review' && token == null) {
        return this.transition(task, 'done', 'task_completed', HUMAN, { summary });
      }
      const claim = this.requireToken(task, token);
      const who = actor || { type: 'agent', id: claim.agent_id };
      if (task.status === 'claimed') this.transition(task, 'in_progress', 'task_started', who, {});
      const to = target || 'done';
      if (!['done', 'review'].includes(to)) throw QueueError.input(`complete target must be done or review, got ${to}`);
      for (const a of artifacts) task.artifacts.push(a);
      if (summary) task.summary = summary;
      this.retireClaim(task);
      return this.transition(task, to, 'task_completed', who, { summary, artifacts: artifacts.length });
    }

    release(id, token, actor = null) {
      const task = this.get(id);
      const claim = this.requireToken(task, token);
      this.retireClaim(task);
      return this.transition(task, 'ready', 'task_released', actor || { type: 'agent', id: claim.agent_id });
    }

    // ── read side
    featureTitle(task) { const f = task.feature_id && this.features.get(task.feature_id); return f ? f.title : null; }

    list({ status = null, kind = null, all = false, feature = null, limit = 100 } = {}) {
      let rows = [...this.tasks.values()];
      if (status) { if (!STATUSES.includes(status)) throw QueueError.input(`unknown status: ${status}`); rows = rows.filter((t) => t.status === status); }
      else if (!all) rows = rows.filter((t) => t.status !== 'done' && t.status !== 'cancelled');
      if (kind) rows = rows.filter((t) => t.kind === kind);
      if (feature != null) { const f = this.findFeature(feature); rows = rows.filter((t) => t.feature_id === f.id); }
      const key = (s) => (s == null ? null : s.toLowerCase());
      const cmpOpt = (a, b) => { if (a === b) return 0; if (a == null) return 1; if (b == null) return -1; return a < b ? -1 : 1; };
      rows.sort((a, b) => cmpOpt(key(this.featureTitle(a)), key(this.featureTitle(b)))
        || cmpOpt(key(a.project), key(b.project)) || b.updated_at - a.updated_at || b.id - a.id);
      return rows.slice(0, limit);
    }

    status() {
      const counts = Object.fromEntries(STATUSES.map((s) => [s, 0]));
      let active = 0; let expired = 0;
      for (const t of this.tasks.values()) {
        counts[t.status]++;
        if (t.claim) { if (t.claim.lease_expires_at > this.now()) active++; else expired++; }
      }
      return { counts, active_claims: active, expired_claims: expired };
    }

    tree(id) {
      const root = this.get(id);
      const seen = new Set();
      const build = (task, path) => {
        const node = { id: task.id, status: task.status, title: task.title, project: task.project, feature_id: task.feature_id, children: [] };
        if (path.has(task.id)) { node.cycle = true; return node; }
        if (seen.has(task.id)) { node.already_shown = true; return node; }
        seen.add(task.id);
        const next = new Set(path); next.add(task.id);
        for (const d of task.depends_on) { const dep = this.tasks.get(d); if (dep) node.children.push(build(dep, next)); }
        return node;
      };
      return { roots: [build(root, new Set())], feature: null };
    }

    treeFeature(ref) {
      const f = this.findFeature(ref);
      const members = [...this.tasks.values()].filter((t) => t.feature_id === f.id);
      const dependedOn = new Set(members.flatMap((t) => t.depends_on));
      const rootTasks = members.filter((t) => !dependedOn.has(t.id));
      const seen = new Set();
      const build = (task, path) => {
        const node = { id: task.id, status: task.status, title: task.title, project: task.project, feature_id: task.feature_id, external: task.feature_id !== f.id, children: [] };
        if (path.has(task.id)) { node.cycle = true; return node; }
        if (seen.has(task.id)) { node.already_shown = true; return node; }
        seen.add(task.id);
        const next = new Set(path); next.add(task.id);
        for (const d of task.depends_on) { const dep = this.tasks.get(d); if (dep) node.children.push(build(dep, next)); }
        return node;
      };
      return { roots: rootTasks.map((t) => build(t, new Set())), feature: f };
    }
  }

  const HUMAN = { type: 'human', id: 'you' };

  // ───────────────────────────── 2. render ─────────────────────────────

  const esc = (s) => String(s).replace(/[&<>"']/g, (c) => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' }[c]));
  const dim = (s) => `<span class="dim">${esc(s)}</span>`;
  const bold = (s) => `<span class="bold">${esc(s)}</span>`;
  const st = (s) => `<span class="st-${esc(s)}">${esc(s)}</span>`;
  const padEnd = (s, n) => s + ' '.repeat(Math.max(0, n - Array.from(s).length));
  const padStart = (s, n) => ' '.repeat(Math.max(0, n - Array.from(s).length)) + s;

  const R = {
    confirm: (verb, task) => `${esc(verb)} ${dim('#' + task.id)} [${st(task.status)}] ${bold(task.title)}`,

    table(queue, tasks) {
      if (!tasks.length) return 'no tasks';
      const now = Date.now();
      const rows = tasks.map((t) => ({
        id: String(t.id), status: t.status, feature: queue.featureTitle(t) || '(none)', project: t.project || '(none)',
        pri: String(t.priority), updated: relative(t.updated_at, now), title: flatTitle(t.title),
      }));
      const heads = ['ID', 'STATUS', 'FEATURE', 'PROJECT', 'PRI', 'UPDATED', 'TITLE'];
      const keys = ['id', 'status', 'feature', 'project', 'pri', 'updated', 'title'];
      const right = [true, false, false, false, true, false, false];
      const widths = keys.map((k, i) => Math.max(heads[i].length, ...rows.map((r) => Array.from(r[k]).length)));
      const cell = (text, i, painted) => (right[i] ? padStart(text, widths[i]) : padEnd(text, widths[i])).replace(text, painted || esc(text));
      const lines = [heads.map((h, i) => (right[i] ? padStart(h, widths[i]) : padEnd(h, widths[i]))).join('  ')];
      for (const r of rows) {
        lines.push([
          cell(r.id, 0, dim(r.id)), cell(r.status, 1, st(r.status)), cell(r.feature, 2, dim(r.feature)), cell(r.project, 3, dim(r.project)),
          cell(r.pri, 4), cell(r.updated, 5, dim(r.updated)), cell(r.title, 6, bold(r.title)),
        ].join('  '));
      }
      return lines.join('\n');
    },

    detail(queue, task) {
      const out = [];
      out.push(`${dim('#' + task.id)} ${bold(task.title)} [${st(task.status)}]`);
      out.push(dim(`kind: ${task.kind}  risk: ${task.risk}  priority: ${task.priority}`));
      out.push(dim(`project: ${task.project || '-'}`));
      out.push(dim(`repo: ${task.repo || '-'}`));
      out.push(dim(`feature: ${queue.featureTitle(task) || '-'}`));
      if (task.depends_on.length) out.push(dim(`depends_on: ${task.depends_on.join(', ')}`));
      out.push(dim(`created_at: ${iso(task.created_at)}`));
      out.push(dim(`updated_at: ${iso(task.updated_at)}`));
      out.push(dim(`public_id: ${task.public_id}`));
      out.push(dim(`capture_path: ${task.capture_path}`));
      out.push(dim(`original_capture: ${task.original_capture}`));
      if (task.body) out.push('', esc(task.body));
      // Like the real store: a matching claim is retired, not deleted, so the latest one still shows.
      const c = task.claim || task.retired_claims[task.retired_claims.length - 1];
      if (c) {
        out.push('', `claim: agent=${esc(c.agent_id)} active=${c.active} token=<span class="tok">${esc(c.token)}</span>`);
        out.push(dim(`lease_expires_at: ${iso(c.lease_expires_at)}`));
        if (c.branch) out.push(dim(`branch: ${c.branch}`));
      }
      if (task.artifacts.length) {
        out.push('', bold('artifacts:'));
        for (const a of task.artifacts) out.push(`- ${esc(a.kind)}: ${esc(a.value)}`);
      }
      if (task.events.length) {
        out.push('', bold('events:'));
        out.push(R.events(task.events.slice(-12), '  '));
      }
      return out.join('\n');
    },

    events: (events, indent = '') => events.map((e) => `${indent}${dim('#' + e.id)} ${esc(e.event_type)} ${dim(e.actor_type)} ${dim(iso(e.created_at))}`).join('\n'),

    status(status) {
      const out = ['database: /home/you/.local/share/q/queue.db'];
      for (const s of STATUSES) out.push(`${st(s)}: ${status.counts[s]}`);
      out.push(`active claims: ${status.active_claims}`);
      out.push(status.expired_claims > 0 ? `<span class="err">expired claims</span>: ${status.expired_claims}` : `expired claims: ${status.expired_claims}`);
      return out.join('\n');
    },

    tree(queue, tree) {
      if (!tree.roots.length) return tree.feature ? `no tasks in ${esc(tree.feature.title)}` : 'no tasks';
      const anchor = tree.feature ? tree.feature.id : tree.roots[0].feature_id;
      const node = (n, prefix, isRoot, isLast) => {
        const connector = isRoot ? '' : (isLast ? '└── ' : '├── ');
        const status = st(n.status) + ' '.repeat(Math.max(0, TREE_STATUS_WIDTH - n.status.length));
        let line = `${dim(prefix)}${dim(connector)}${dim('#' + n.id)}  ${status}  ${bold(flatTitle(n.title))}`;
        if (n.project) line += `  ${dim('[' + n.project + ']')}`;
        const f = n.feature_id && queue.features.get(n.feature_id);
        if (f && n.feature_id !== anchor) line += `  ${dim('{' + f.title + '}')}`;
        if (n.external) line += `  ${dim('(external)')}`;
        if (n.already_shown) line += `  ${dim('(already shown)')}`;
        if (n.cycle) line += `  <span class="err">(cycle)</span>`;
        const lines = [line];
        const childPrefix = isRoot ? '' : prefix + (isLast ? '    ' : '│   ');
        n.children.forEach((c, i) => lines.push(node(c, childPrefix, false, i === n.children.length - 1)));
        return lines.join('\n');
      };
      return tree.roots.map((r) => node(r, '', true, true)).join('\n\n');
    },

    top(queue, tasks) {
      const s = queue.status();
      const head = `q top database: /home/you/.local/share/q/queue.db  ${iso(Date.now())}  every 2.0s  Ctrl-C quits`;
      const counts = STATUSES.map((k) => `${st(k)} ${s.counts[k]}`).join('  ') + `  |  claims ${s.active_claims} active, ${s.expired_claims} expired`;
      const recent = queue.recent.filter((e) => !e.quiet).slice(0, 10);
      const changes = recent.length ? recent.map((e) => `  ${dim('#' + e.id)} ${esc(e.kind)} ${bold(flatTitle(e.title))}`).join('\n') : '  none yet';
      return `${dim(head)}\n${counts}\n\n${R.table(queue, tasks)}\n\nrecent changes\n${changes}`;
    },

    json: (value) => `<span class="json">${esc(JSON.stringify(value, null, 2))}</span>`,

    taskJson(queue, t) {
      const c = t.claim || t.retired_claims[t.retired_claims.length - 1] || null;
      return {
        id: t.id, public_id: t.public_id, title: t.title, status: t.status, kind: t.kind, priority: t.priority, risk: t.risk,
        project: t.project, repo: t.repo, feature: queue.featureTitle(t), dependencies: t.depends_on,
        created_at: iso(t.created_at), updated_at: iso(t.updated_at),
        claim: c ? { agent_id: c.agent_id, token: c.token, lease_expires_at: iso(c.lease_expires_at), branch: c.branch, active: c.active } : null,
        artifacts: t.artifacts,
      };
    },
  };

  // ───────────────────────────── 3. shell ─────────────────────────────

  function tokenize(line) {
    const out = [];
    let cur = null; let quote = null;
    for (let i = 0; i < line.length; i++) {
      const c = line[i];
      if (quote) {
        if (c === quote) quote = null;
        else if (c === '\\' && quote === '"' && i + 1 < line.length) cur += line[++i];
        else cur += c;
      } else if (c === '"' || c === "'") { quote = c; cur = cur ?? ''; }
      else if (c === '\\' && i + 1 < line.length) { cur = (cur ?? '') + line[++i]; }
      else if (/\s/.test(c)) { if (cur !== null) { out.push(cur); cur = null; } }
      else cur = (cur ?? '') + c;
    }
    if (quote) throw new ShellError(`unterminated quote ${quote}`);
    if (cur !== null) out.push(cur);
    return out;
  }

  class ShellError extends Error {}

  // Global flags accepted anywhere, like clap's global = true.
  const GLOBAL_FLAGS = { '--json': 'bool', '-j': 'bool', '--db': 'value', '--server': 'value', '--token': 'value', '--color': 'value', '-C': 'value', '--directory': 'value', '--repo': 'value', '--project': 'value' };

  // Parse `tokens` against `spec` = { '--flag': 'bool' | 'value' | 'multi' }. Returns { flags, positionals }.
  function parseArgs(tokens, spec, aliases = {}) {
    const flags = {}; const positionals = [];
    const all = { ...GLOBAL_FLAGS, ...spec };
    for (let i = 0; i < tokens.length; i++) {
      let tok = tokens[i];
      if (tok === '--') { positionals.push(...tokens.slice(i + 1)); break; }
      if (tok.startsWith('-') && tok.length > 1 && !/^-\d+$/.test(tok)) {
        let inline = null;
        if (tok.startsWith('--') && tok.includes('=')) { [tok, inline] = [tok.slice(0, tok.indexOf('=')), tok.slice(tok.indexOf('=') + 1)]; }
        const name = aliases[tok] || tok;
        const kind = all[name];
        if (!kind) throw new ShellError(`unexpected argument '${tok}' found\n\n  tip: add --help to the command to see valid arguments`);
        if (kind === 'bool') { flags[name] = true; continue; }
        const value = inline ?? tokens[++i];
        if (value === undefined) throw new ShellError(`a value is required for '${tok} <${name.replace(/^-+/, '').toUpperCase()}>' but none was supplied`);
        if (kind === 'multi') (flags[name] = flags[name] || []).push(value);
        else flags[name] = value;
      } else positionals.push(tok);
    }
    return { flags, positionals };
  }

  const requireId = (positionals, what = 'ID') => {
    if (!positionals.length) throw new ShellError(`the following required arguments were not provided:\n  <${what}>`);
    const id = Number(positionals[0]);
    if (!Number.isInteger(id) || id < 1) throw new ShellError(`invalid value '${positionals[0]}' for '<${what}>': invalid digit found in string`);
    return id;
  };
  const requireFlag = (flags, name, label) => {
    if (flags[name] === undefined) throw new ShellError(`the following required arguments were not provided:\n  ${name} <${label}>`);
    return flags[name];
  };
  const intFlag = (flags, name, fallback) => {
    if (flags[name] === undefined) return fallback;
    const n = Number(flags[name]);
    if (!Number.isInteger(n)) throw new ShellError(`invalid value '${flags[name]}' for '${name}': invalid digit found in string`);
    return n;
  };

  const HELP = {
    main: `Local-first context-aware agent work queue

Usage: q [OPTIONS] <COMMAND>

Commands:
  add            Capture a task into the inbox (a bare title is shorthand: q "title")
  ls             List tasks. Done and cancelled are hidden unless --all or --status is set [alias: list]
  top            Watch the queue: counts, the task table, and recent changes (one frame here)
  show           Show one task, its claim, artifacts, and recent events
  tree           Show what must be done before a task, or before the tasks in a feature
  edit           Edit task fields (--priority, --risk, --kind, --title, --depends-on, --feature)
  ready          Move a task to ready so agents may claim it
  block          Block a task. Claimed work also requires --claim-token
  cancel         Cancel a task that is inbox, ready, or blocked. The row and its history stay
  delete         Hard-delete a task and its claims, events, artifacts, and dependency rows [alias: rm]
  claim          Atomically claim one eligible ready task
  heartbeat      Extend the lease for a matching, unexpired claim token
  start          Mark claimed work in progress
  complete       Complete claimed work, or accept a task that is already in review [alias: done]
  release        Return claimed work to ready
  status         Show queue counts and claim lease health
  recover-stale  Requeue expired claims and record a recovery event [alias: recover]
  events         Show the append-only event log for a task
  reopen         Reopen done work to ready, or cancelled work to inbox
  feature        Named groups of tasks that may span repos (create, ls, show)
  mcp            Serve the queue as an MCP server on stdio
  serve          Serve the local database over HTTP as the single authority for remote agents
  skill          Print the agent skill for q
  help           Print this message or the help of the given subcommand(s)

Options:
  -j, --json           Print machine-readable JSON on stdout
      --repo <REPO>    Explicit repository identity. On claim/ls, filters by repo
      --project <NAME> Explicit project name. On claim/ls, filters by project
  -h, --help           Print help
  -V, --version        Print version

Shell extras in this demo: help, clear, agents, reset. Use ↑/↓ for history and Tab to complete.`,
    add: `Capture a task into the inbox\n\nUsage: q add [OPTIONS] <TITLE>...\n       q "<TITLE>"\n\nOptions:\n      --kind <KIND>            implementation, research, review, benchmark, documentation, or other\n      --priority <N>           Higher claims first [default: 0]\n      --risk <RISK>            low, medium, high, or external_action [default: low]\n      --feature <ID|TITLE>     Attach to a feature\n      --depends-on <ID>        Task that must be done first. Repeatable\n      --body <TEXT>            Markdown body`,
    ls: `List tasks. Done and cancelled are hidden unless --all or --status is set\n\nUsage: q ls [OPTIONS]\n\nOptions:\n      --status <STATUS>     inbox, ready, claimed, in_progress, review, blocked, done, cancelled\n      --kind <KIND>         implementation, research, review, benchmark, documentation, or other\n      --feature <ID|TITLE>  Show only tasks in this feature\n  -n, --limit <LIMIT>       Maximum rows [default: 100]\n  -a, --all                 Include done and cancelled tasks\n  -j, --json                Print machine-readable JSON`,
    top: `Watch the queue. Redraws counts, the task table, and recent changes until Ctrl-C\n\nUsage: q top [OPTIONS]\n\nOptions:\n  -i, --interval <SECONDS>  Seconds between refreshes [default: 2]\n      --status <STATUS>     Show only this status\n  -n, --limit <LIMIT>       Maximum rows in the table [default: 30]\n  -a, --all                 Include done and cancelled tasks\n      --once                Draw one frame and exit (this demo always draws one frame)`,
    show: `Show one task, its claim, artifacts, and recent events\n\nUsage: q show [OPTIONS] <ID>`,
    tree: `Show what must be done before a task, or before the tasks in a feature.\n\nChildren are dependencies (do these first).\n\nUsage: q tree [OPTIONS] [TASK_ID]\n\nOptions:\n      --feature <ID|TITLE>  One tree per task that nothing else in the feature depends on`,
    edit: `Edit task fields\n\nUsage: q edit [OPTIONS] <ID>\n\nOptions:\n      --title <TITLE>        New title\n      --priority <N>         Higher claims first\n      --risk <RISK>          low, medium, high, or external_action\n      --kind <KIND>          implementation, research, review, benchmark, documentation, or other\n      --feature <ID|TITLE>   Attach to a feature\n      --clear-feature        Detach from its feature\n      --depends-on <ID>      Replace dependencies. Repeatable\n      --body <TEXT>          Markdown body`,
    ready: `Move a task to ready so agents may claim it\n\nUsage: q ready [OPTIONS] <ID>`,
    block: `Block a task. Claimed work also requires --claim-token\n\nUsage: q block [OPTIONS] <ID>\n\nOptions:\n      --claim-token <CLAIM_TOKEN>  Token printed by 'q claim'`,
    cancel: `Cancel a task that is inbox, ready, or blocked. The row and its history stay\n\nUsage: q cancel [OPTIONS] <ID>`,
    delete: `Hard-delete a task and its claims, events, artifacts, and dependency rows\n\nUsage: q delete [OPTIONS] <ID>\n\nOptions:\n      --force  Clear an unexpired claim in the same transaction`,
    claim: `Atomically claim one eligible ready task\n\nUsage: q claim [OPTIONS] --agent <AGENT>\n\nOptions:\n      --agent <AGENT>                  Worker id recorded on the claim\n      --capability <CAPABILITY>        Capability this worker has. Repeatable\n      --kind <KIND>                    Kind this worker accepts. Repeatable. Default: any kind\n      --max-risk <MAX_RISK>            Default medium. High and external_action are excluded unless raised explicitly\n      --lease-minutes <LEASE_MINUTES>  Lease length. Default 45. Minimum 1, maximum 1440\n      --repo <REPO>                    Only claim tasks in this repo\n      --project <NAME>                 Only claim tasks in this project\n  -j, --json                           Print machine-readable JSON`,
    heartbeat: `Extend the lease for a matching, unexpired claim token\n\nUsage: q heartbeat [OPTIONS] --claim-token <CLAIM_TOKEN> <ID>\n\nOptions:\n      --claim-token <CLAIM_TOKEN>      Token printed by 'q claim'\n      --lease-minutes <LEASE_MINUTES>  New lease length in minutes. Default 45`,
    start: `Mark claimed work in progress\n\nUsage: q start [OPTIONS] --claim-token <CLAIM_TOKEN> <ID>\n\nOptions:\n      --claim-token <CLAIM_TOKEN>  Token printed by 'q claim'\n      --branch <BRANCH>            Branch name to record on the claim\n      --worktree <WORKTREE>        Worktree path to record on the claim`,
    complete: `Complete claimed work, or accept a task that is already in review\n\nUsage: q complete [OPTIONS] <ID>\n\nOptions:\n      --claim-token <CLAIM_TOKEN>  Token printed by 'q claim'. Not needed to accept review\n      --summary <SUMMARY>          One line about what was done\n      --status <STATUS>            done (default) or review\n      --artifact <KIND=VALUE>      Repeatable, e.g. --artifact pr=https://github.com/acme/x/pull/1`,
    release: `Return claimed work to ready\n\nUsage: q release [OPTIONS] --claim-token <CLAIM_TOKEN> <ID>`,
    status: `Show queue counts and claim lease health\n\nUsage: q status [OPTIONS]`,
    'recover-stale': `Requeue expired claims and record a recovery event\n\nUsage: q recover-stale [OPTIONS]\n\nOptions:\n      --to <ready|blocked>  Where recovered tasks go [default: ready]`,
    events: `Show the append-only event log for a task\n\nUsage: q events [OPTIONS] <ID>`,
    reopen: `Reopen done work to ready, or cancelled work to inbox\n\nUsage: q reopen [OPTIONS] <ID>`,
    feature: `Named groups of tasks that may span repos\n\nUsage: q feature <COMMAND>\n\nCommands:\n  create <TITLE>...  Create a feature (--body TEXT)\n  ls                 List features\n  show <ID>          Show a feature and its tasks`,
    mcp: `Serve the queue as an MCP server on stdio\n\nUsage: q mcp [--db PATH | --server URL --token TOKEN]\n\nSpeaks newline-delimited JSON-RPC on stdio. No network port. Tools: queue_capture, queue_list, queue_get,\nqueue_tree, queue_claim_next, queue_heartbeat, queue_start, queue_block, queue_complete, queue_release, queue_delete,\nqueue_feature_create, queue_feature_list, queue_feature_get. There is no ready tool: only humans mark work ready.`,
    serve: `Serve the local database over HTTP as the single authority for remote agents\n\nUsage: q serve [--bind ADDR] [--db PATH] [--auth tokens.toml]\n\nEvery CLI and MCP client points at it with --server URL (or Q_SERVER_URL). Claims stay serialized by the same\nBEGIN IMMEDIATE transaction they use locally. Agent tokens cannot call ready or reopen; the server refuses them.`,
    skill: `Print or install the agent skill for q\n\nUsage: q skill [install --target agents|claude|cursor|codex|all]`,
  };
  const ALIASES = { list: 'ls', canceled: 'cancel', rm: 'delete', done: 'complete', recover: 'recover-stale' };
  const SUBCOMMANDS = Object.keys(HELP).filter((k) => k !== 'main');

  const artifactFromFlag = (s) => {
    const i = s.indexOf('=');
    if (i <= 0) throw new ShellError(`artifact must be KIND=VALUE, got '${s}'`);
    return { kind: s.slice(0, i), value: s.slice(i + 1) };
  };

  // Every handler returns an HTML string (or '') and may throw ShellError / QueueError.
  function makeCommands(queue, ctx) {
    const emit = (flags, value, human) => (flags['--json'] || flags['-j'] ? R.json(typeof value === 'function' ? value() : value) : human());
    const wantsHelp = (tokens) => tokens.includes('--help') || tokens.includes('-h');

    const commands = {
      add(tokens) {
        const { flags, positionals } = parseArgs(tokens, { '--kind': 'value', '--priority': 'value', '--risk': 'value', '--feature': 'value', '--depends-on': 'multi', '--body': 'value', '--body-file': 'value', '-e': 'bool', '--edit': 'bool' });
        if (!positionals.length) throw new ShellError('the following required arguments were not provided:\n  <TITLE>...');
        if (flags['-e'] || flags['--edit']) throw new ShellError('no editor is set ($VISUAL or $EDITOR); this demo has no editor anyway');
        const task = queue.capture({
          title: positionals.join(' '), kind: flags['--kind'] || 'implementation', risk: flags['--risk'] || 'low',
          priority: intFlag(flags, '--priority', 0), project: flags['--project'] || 'q', repo: flags['--repo'] || 'github.com/pierricgimmig/q',
          feature: flags['--feature'] ?? null, body: flags['--body'] ?? null, dependsOn: (flags['--depends-on'] || []).map((d) => requireId([d], 'ID')),
        });
        ctx.log('human', `q add → ${R.confirm('captured', task)}`);
        return emit(flags, () => R.taskJson(queue, task), () => R.confirm('captured', task));
      },
      ls(tokens) {
        const { flags } = parseArgs(tokens, { '--status': 'value', '--kind': 'value', '--feature': 'value', '-n': 'value', '--limit': 'value', '-a': 'bool', '--all': 'bool' }, { '-n': '--limit' });
        const tasks = queue.list({ status: flags['--status'] || null, kind: flags['--kind'] || null, all: !!(flags['-a'] || flags['--all']), feature: flags['--feature'] ?? null, limit: intFlag(flags, '--limit', 100) });
        return emit(flags, () => ({ tasks: tasks.map((t) => R.taskJson(queue, t)) }), () => R.table(queue, tasks));
      },
      top(tokens) {
        const { flags } = parseArgs(tokens, { '--status': 'value', '--kind': 'value', '--feature': 'value', '-n': 'value', '--limit': 'value', '-a': 'bool', '--all': 'bool', '-i': 'value', '--interval': 'value', '--once': 'bool' }, { '-n': '--limit', '-i': '--interval' });
        if (flags['--json'] || flags['-j']) throw new ShellError('--json is not supported by q top; use q ls --json or q status --json');
        const tasks = queue.list({ status: flags['--status'] || null, kind: flags['--kind'] || null, all: !!(flags['-a'] || flags['--all']), feature: flags['--feature'] ?? null, limit: intFlag(flags, '--limit', 30) });
        const note = flags['--once'] ? '' : `\n<span class="note">(the real q top keeps redrawing until Ctrl-C; this demo draws one frame)</span>`;
        return R.top(queue, tasks) + note;
      },
      show(tokens) {
        const { flags, positionals } = parseArgs(tokens, {});
        const task = queue.get(requireId(positionals));
        return emit(flags, () => ({ ...R.taskJson(queue, task), events: task.events.map((e) => ({ id: e.id, event_type: e.event_type, actor_type: e.actor_type, actor_id: e.actor_id, created_at: iso(e.created_at) })) }), () => R.detail(queue, task));
      },
      tree(tokens) {
        const { flags, positionals } = parseArgs(tokens, { '--feature': 'value' });
        if (flags['--feature'] == null && !positionals.length) throw new ShellError('the following required arguments were not provided:\n  --feature <ID|TITLE>\n  <TASK_ID>');
        const tree = flags['--feature'] != null ? queue.treeFeature(flags['--feature']) : queue.tree(requireId(positionals, 'TASK_ID'));
        const toJson = (n) => ({ id: n.id, status: n.status, title: n.title, project: n.project, depends_on: n.children.map(toJson), ...(n.external ? { external: true } : {}), ...(n.already_shown ? { already_shown: true } : {}) });
        return emit(flags, () => ({ ...(tree.feature ? { feature: { id: tree.feature.id, title: tree.feature.title } } : {}), roots: tree.roots.map(toJson) }), () => R.tree(queue, tree));
      },
      edit(tokens) {
        const { flags, positionals } = parseArgs(tokens, { '--title': 'value', '--priority': 'value', '--risk': 'value', '--kind': 'value', '--feature': 'value', '--clear-feature': 'bool', '--depends-on': 'multi', '--body': 'value', '-e': 'bool', '--edit': 'bool' });
        const id = requireId(positionals);
        const patch = {};
        if (flags['--title'] != null) patch.title = flags['--title'];
        if (flags['--priority'] != null) patch.priority = intFlag(flags, '--priority');
        if (flags['--risk'] != null) patch.risk = flags['--risk'];
        if (flags['--kind'] != null) patch.kind = flags['--kind'];
        if (flags['--body'] != null) patch.body = flags['--body'];
        if (flags['--feature'] != null) patch.feature = flags['--feature'];
        if (flags['--clear-feature']) patch.feature = null;
        if (flags['--depends-on']) patch.dependsOn = flags['--depends-on'].map((d) => requireId([d]));
        if (!Object.keys(patch).length) throw new ShellError('no changes specified; this demo has no $EDITOR, pass flags such as --priority 2 or --risk high');
        const task = queue.edit(id, patch);
        return emit(flags, () => R.taskJson(queue, task), () => R.confirm('updated', task));
      },
      ready(tokens) {
        const { flags, positionals } = parseArgs(tokens, {});
        const task = queue.ready(requireId(positionals));
        ctx.log('human', `q ready ${task.id} → [${st('ready')}] ${esc(flatTitle(task.title))}`);
        return emit(flags, () => R.taskJson(queue, task), () => R.confirm('ready', task));
      },
      block(tokens) {
        const { flags, positionals } = parseArgs(tokens, { '--claim-token': 'value' });
        const task = queue.block(requireId(positionals), flags['--claim-token']);
        return emit(flags, () => R.taskJson(queue, task), () => R.confirm('blocked', task));
      },
      cancel(tokens) {
        const { flags, positionals } = parseArgs(tokens, {});
        const task = queue.cancel(requireId(positionals));
        return emit(flags, () => R.taskJson(queue, task), () => R.confirm('cancelled', task));
      },
      delete(tokens) {
        const { flags, positionals } = parseArgs(tokens, { '--force': 'bool' });
        const o = queue.delete(requireId(positionals), !!flags['--force']);
        return emit(flags, o, () => [
          `deleted ${dim('#' + o.task_id)} [${st(o.status)}] ${bold(o.title)}`,
          o.active_claim_cleared ? 'cleared active claim' : null,
          dim(`removed claims=${o.claims_removed} events=${o.events_removed} artifacts=${o.artifacts_removed} dependencies=${o.dependencies_removed}`),
        ].filter(Boolean).join('\n'));
      },
      claim(tokens) {
        const { flags } = parseArgs(tokens, { '--agent': 'value', '--capability': 'multi', '--kind': 'multi', '--max-risk': 'value', '--lease-minutes': 'value', '--agent-pool': 'value' });
        const agent = requireFlag(flags, '--agent', 'AGENT');
        const o = queue.claimNext({
          agent, capabilities: (flags['--capability'] || []).flatMap((c) => c.split(',')), kinds: flags['--kind'] || [],
          maxRisk: flags['--max-risk'] || 'medium', leaseMinutes: intFlag(flags, '--lease-minutes', DEFAULT_LEASE_MIN), repo: flags['--repo'] || null, project: flags['--project'] || null,
        });
        if (o.found) ctx.log('human', `q claim --agent ${esc(agent)} → claimed #${o.task.id}`);
        return emit(flags,
          () => (o.found ? { found: true, task: R.taskJson(queue, o.task), claim: { token: o.claim.token, agent_id: o.claim.agent_id, lease_expires_at: iso(o.claim.lease_expires_at) } } : o),
          () => (o.found ? `${R.confirm('claimed', o.task)}\ntoken: <span class="tok">${esc(o.claim.token)}</span>\n${dim('lease_expires_at: ' + iso(o.claim.lease_expires_at))}` : 'no eligible ready tasks'));
      },
      heartbeat(tokens) {
        const { flags, positionals } = parseArgs(tokens, { '--claim-token': 'value', '--lease-minutes': 'value' });
        const id = requireId(positionals);
        const claim = queue.heartbeat(id, requireFlag(flags, '--claim-token', 'CLAIM_TOKEN'), flags['--lease-minutes'] == null ? null : intFlag(flags, '--lease-minutes'), HUMAN);
        return emit(flags, { task_id: id, agent_id: claim.agent_id, lease_expires_at: iso(claim.lease_expires_at) }, () => `heartbeat ${dim('#' + id)} until ${dim(iso(claim.lease_expires_at))}`);
      },
      start(tokens) {
        const { flags, positionals } = parseArgs(tokens, { '--claim-token': 'value', '--branch': 'value', '--worktree': 'value' });
        const id = requireId(positionals);
        const task = queue.start(id, requireFlag(flags, '--claim-token', 'CLAIM_TOKEN'), { branch: flags['--branch'] || null, worktree: flags['--worktree'] || null }, HUMAN);
        return emit(flags, () => R.taskJson(queue, task), () => R.confirm('started', task));
      },
      complete(tokens) {
        const { flags, positionals } = parseArgs(tokens, { '--claim-token': 'value', '--summary': 'value', '--status': 'value', '--artifact': 'multi' });
        const id = requireId(positionals);
        const before = queue.get(id);
        if (before.status !== 'review' && flags['--claim-token'] == null) throw new ShellError('the following required arguments were not provided:\n  --claim-token <CLAIM_TOKEN>\n\n  (a claim token is only optional when accepting work that is in review)');
        const task = queue.complete(id, flags['--claim-token'] ?? null, { summary: flags['--summary'] || null, target: flags['--status'] || null, artifacts: (flags['--artifact'] || []).map(artifactFromFlag) }, HUMAN);
        ctx.log('human', `q complete ${id} → [${st(task.status)}] ${esc(flatTitle(task.title))}`);
        return emit(flags, () => R.taskJson(queue, task), () => R.confirm('completed', task));
      },
      release(tokens) {
        const { flags, positionals } = parseArgs(tokens, { '--claim-token': 'value' });
        const id = requireId(positionals);
        const task = queue.release(id, requireFlag(flags, '--claim-token', 'CLAIM_TOKEN'), HUMAN);
        return emit(flags, () => R.taskJson(queue, task), () => R.confirm('released', task));
      },
      status(tokens) {
        const { flags } = parseArgs(tokens, {});
        const s = queue.status();
        return emit(flags, { database: '/home/you/.local/share/q/queue.db', ...s }, () => R.status(s));
      },
      'recover-stale'(tokens) {
        const { flags } = parseArgs(tokens, { '--to': 'value' });
        const to = flags['--to'] || 'ready';
        if (!['ready', 'blocked'].includes(to)) throw new ShellError(`--to must be ready or blocked, got '${to}'`);
        const recovered = queue.recoverStale(to);
        return emit(flags, { recovered }, () => (recovered.length ? recovered.map((r) => `recovered ${dim('#' + r.task_id)} ${st(r.previous_status)} -> ${st(r.new_status)}`).join('\n') : 'no expired claims'));
      },
      events(tokens) {
        const { flags, positionals } = parseArgs(tokens, {});
        const task = queue.get(requireId(positionals));
        return emit(flags, { events: task.events.map((e) => ({ id: e.id, event_type: e.event_type, actor_type: e.actor_type, actor_id: e.actor_id, payload: e.payload, created_at: iso(e.created_at) })) }, () => R.events(task.events));
      },
      reopen(tokens) {
        const { flags, positionals } = parseArgs(tokens, {});
        const task = queue.reopen(requireId(positionals));
        return emit(flags, () => R.taskJson(queue, task), () => R.confirm('reopened', task));
      },
      feature(tokens) {
        const sub = tokens[0];
        if (!sub || wantsHelp(tokens)) return esc(HELP.feature);
        const rest = tokens.slice(1);
        if (sub === 'create') {
          const { flags, positionals } = parseArgs(rest, { '--body': 'value' });
          if (!positionals.length) throw new ShellError('the following required arguments were not provided:\n  <TITLE>...');
          const f = queue.createFeature({ title: positionals.join(' '), body: flags['--body'] ?? null });
          return emit(flags, f, () => `created feature ${dim('#' + f.id)} ${bold(f.title)}`);
        }
        if (sub === 'ls' || sub === 'list') {
          const { flags } = parseArgs(rest, {});
          const fs = [...queue.features.values()];
          return emit(flags, { features: fs }, () => (fs.length ? fs.map((f) => `${dim(padStart('#' + f.id, 4))}  ${bold(f.title)}  ${dim(`(${[...queue.tasks.values()].filter((t) => t.feature_id === f.id).length} tasks)`)}`).join('\n') : 'no features'));
        }
        if (sub === 'show') {
          const { flags, positionals } = parseArgs(rest, {});
          const f = queue.findFeature(requireId(positionals));
          const members = queue.list({ all: true, feature: f.id });
          return emit(flags, { ...f, tasks: members.map((t) => R.taskJson(queue, t)) }, () => `${dim('#' + f.id)} ${bold(f.title)}\n${f.body ? esc(f.body) + '\n' : ''}\n${R.table(queue, members)}`);
        }
        throw new ShellError(`unrecognized subcommand '${sub}'\n\n  tip: q feature --help`);
      },
      mcp() { return esc(HELP.mcp) + `\n<span class="note">(this demo has no stdio; a real 'q mcp' would now wait for JSON-RPC on stdin)</span>`; },
      serve() { return esc(HELP.serve) + `\n<span class="note">(this demo has no network; a real 'q serve' would bind 127.0.0.1:7777)</span>`; },
      skill() { return esc(HELP.skill) + `\n\n<span class="note">The real command prints a SKILL.md telling agents: capture lands in inbox, wait for a human 'q ready', claim at most one task,\nkeep the claim token, heartbeat, and never mark your own work ready. See github.com/pierricgimmig/q for the full text.</span>`; },
      project(tokens) {
        if (tokens[0] === 'init') return `wrote ${dim('/home/you/src/q/.agentqueue.toml')}\n<span class="note">(simulated)</span>`;
        return dim('source: git\nproject: q\nrepo: github.com/pierricgimmig/q\ngit_root: /home/you/src/q');
      },
    };

    function runQ(tokens) {
      if (!tokens.length || (tokens.length === 1 && wantsHelp(tokens))) return esc(HELP.main);
      if (tokens[0] === '--version' || tokens[0] === '-V') return 'q 0.1.0 (browser demo)';
      // Peel leading global flags so `q --json ls` and `q -C dir "title"` work.
      const leading = [];
      let i = 0;
      while (i < tokens.length && tokens[i].startsWith('-')) {
        const kind = GLOBAL_FLAGS[tokens[i]];
        if (!kind) break;
        leading.push(tokens[i]); if (kind === 'value') leading.push(tokens[++i]); i++;
      }
      if (i >= tokens.length) return esc(HELP.main);
      let name = tokens[i]; const rest = [...leading, ...tokens.slice(i + 1)];
      if (name === 'help') { const topic = ALIASES[tokens[i + 1]] || tokens[i + 1]; return esc(HELP[topic] || HELP.main); }
      name = ALIASES[name] || name;
      if (!commands[name]) return commands.add([...leading, ...tokens.slice(i)]); // bare title shorthand
      if (wantsHelp(rest)) return esc(HELP[name] || HELP.main);
      return commands[name](rest);
    }

    return { runQ, commands };
  }

  // ───────────────────────────── 4. agents ─────────────────────────────

  const WORK_LINES = {
    implementation: ['reading crates/ and the README', 'writing the change', 'cargo build --workspace', 'cargo test --workspace', 'cargo clippy -- -D warnings', 'committing on the branch'],
    research: ['collecting sources', 'reading papers and issues', 'drafting comparison table', 'writing findings.md'],
    review: ['checking out the branch', 'reading the diff', 'running the test suite', 'writing review notes'],
    benchmark: ['building release binary', 'warming up', 'running 5 iterations', 'collecting p50/p99', 'writing report'],
    documentation: ['reading the code paths', 'drafting the doc', 'checking examples compile', 'proofreading'],
    other: ['planning', 'doing the thing', 'checking the result'],
  };
  const SUMMARIES = {
    implementation: ['Implemented and covered by tests', 'Change landed with tests and docs', 'Done; clippy and tests green'],
    research: ['Findings written up with recommendation', 'Comparison table and notes committed'],
    review: ['Reviewed; two suggestions, no blockers', 'Approved with minor comments'],
    benchmark: ['Report committed with p50/p99 and variance', 'Benchmarks run 5x, results in docs/'],
    documentation: ['Docs written and cross-linked', 'Guide added with runnable examples'],
    other: ['Done', 'Completed as described'],
  };
  const pick = (arr) => arr[Math.floor(Math.random() * arr.length)];
  const jitter = (base, spread) => base + Math.random() * spread;

  class Agent {
    constructor(name, { pace = 1, maxRisk = 'medium', kinds = [], color = 'a0' }) {
      Object.assign(this, { name, pace, maxRisk, kinds, color });
      this.state = 'idle'; this.task = null; this.token = null; this.plan = []; this.nextAt = Date.now() + jitter(1500, 3000);
      this.nextHeartbeat = 0; this.finishAt = 0; this.claimedAt = 0;
    }
  }

  class Scheduler {
    constructor(queue, onLog, onChange) {
      this.queue = queue; this.log = onLog; this.onChange = onChange;
      this.agents = [
        new Agent('claude-01', { pace: 1.0, color: 'a0' }),
        new Agent('codex-02', { pace: 1.35, color: 'a1' }),
        new Agent('gemini-03', { pace: 0.8, kinds: ['research', 'documentation', 'benchmark', 'review'], color: 'a2' }),
      ];
      this.timer = setInterval(() => this.tick(), 500);
    }

    reset() { for (const a of this.agents) { a.state = 'idle'; a.task = null; a.token = null; a.plan = []; a.nextAt = Date.now() + jitter(1500, 3000); } this.onChange(); }

    tick() {
      const now = Date.now();
      for (const a of this.agents) {
        try { this.step(a, now); } catch (err) { this.log(a, `<span class="err">error:</span> ${esc(err.message)}`); this.idle(a, now, 4000); }
      }
    }

    idle(agent, now, delay = 3000) { agent.state = 'idle'; agent.task = null; agent.token = null; agent.plan = []; agent.nextAt = now + jitter(delay, 3000); this.onChange(); }

    step(agent, now) {
      if (agent.state === 'idle') {
        if (now < agent.nextAt) return;
        const outcome = this.queue.claimNext({ agent: agent.name, maxRisk: agent.maxRisk, kinds: agent.kinds });
        if (!outcome.found) { agent.nextAt = now + jitter(3000, 3000); return; }
        agent.state = 'claimed'; agent.task = outcome.task; agent.token = outcome.claim.token; agent.claimedAt = now;
        agent.nextAt = now + jitter(1500, 1500) / agent.pace;
        this.log(agent, `q claim --agent ${agent.name} → claimed ${dim('#' + agent.task.id)} ${bold(flatTitle(agent.task.title))} ${dim('token ' + agent.token.slice(0, 8) + '…')}`);
        this.onChange();
        return;
      }
      const task = this.queue.tasks.get(agent.task.id);
      if (!task || !task.claim || task.claim.token !== agent.token) {
        // A human released, blocked, deleted, or completed our task under us.
        this.log(agent, `${dim('#' + agent.task.id)} is no longer ours (${task ? task.status : 'deleted'}); back to polling`);
        this.idle(agent, now, 2000);
        return;
      }
      if (agent.state === 'claimed') {
        if (now < agent.nextAt) return;
        const branch = `agent/task-${task.id}-${slug(task.title) || 'work'}`;
        this.queue.start(task.id, agent.token, { branch });
        agent.state = 'working';
        const lines = WORK_LINES[task.kind] || WORK_LINES.other;
        const total = jitter(14000, 12000) / agent.pace;
        agent.plan = lines.map((text, i) => ({ at: now + (total * (i + 1)) / (lines.length + 1), text }));
        agent.finishAt = now + total;
        agent.nextHeartbeat = now + 6000;
        this.log(agent, `q start ${task.id} --branch ${esc(branch)} → [${st('in_progress')}]`);
        this.onChange();
        return;
      }
      if (agent.state === 'working') {
        while (agent.plan.length && agent.plan[0].at <= now) this.log(agent, dim(agent.plan.shift().text));
        if (now >= agent.nextHeartbeat && now < agent.finishAt) {
          const claim = this.queue.heartbeat(task.id, agent.token);
          agent.nextHeartbeat = now + 6000;
          this.log(agent, `q heartbeat ${task.id} → ${dim('lease until ' + iso(claim.lease_expires_at).slice(11, 19) + 'Z')}`);
          this.onChange();
        }
        if (now >= agent.finishAt) {
          const summary = pick(SUMMARIES[task.kind] || SUMMARIES.other);
          const artifact = task.kind === 'implementation' || task.kind === 'review'
            ? { kind: 'pr', value: `https://github.com/pierricgimmig/q/pull/${100 + task.id}` }
            : { kind: 'report', value: `docs/${slug(task.title) || 'report'}.md` };
          const done = this.queue.complete(task.id, agent.token, { summary, artifacts: [artifact] });
          this.log(agent, `q complete ${task.id} --summary "${esc(summary)}" → [${st(done.status)}] ${dim(artifact.kind + '=' + artifact.value)}`);
          this.idle(agent, now, 2500);
        }
      }
    }
  }

  // ───────────────────────────── 5. ui ─────────────────────────────

  const $ = (sel) => document.querySelector(sel);

  function seed(queue) {
    const rollout = queue.createFeature({ title: 'Rollout', body: 'Ship the queue across services' });
    queue.capture({ title: 'Benchmark trace encoding variants', kind: 'benchmark', project: 'profiler', repo: 'github.com/acme/profiler' });
    const schema = queue.capture({ title: 'Write the schema migration', project: 'api', repo: 'github.com/acme/api', feature: rollout.id });
    const recovery = queue.capture({ title: 'Add stale-job recovery', project: 'orchestrator', repo: 'github.com/acme/orchestrator', feature: rollout.id });
    queue.capture({ title: 'Ship the rollout behind a flag', project: 'api', repo: 'github.com/acme/api', feature: rollout.id, dependsOn: [schema.id, recovery.id], priority: 1 });
    queue.capture({ title: 'Rotate the production API keys', kind: 'other', risk: 'high', project: 'api', repo: 'github.com/acme/api' });
    queue.capture({ title: 'Compare KV-cache quantization', kind: 'research', project: 'q', repo: 'github.com/pierricgimmig/q' });
    // A little history so `q ls -a` and `q reopen` have something to show.
    const oldDone = queue.capture({ title: 'Color the q ls status column', project: 'q', repo: 'github.com/pierricgimmig/q' });
    queue.ready(oldDone.id);
    const c = queue.claimNext({ agent: 'codex-02' });
    queue.start(c.task.id, c.claim.token, { branch: 'agent/task-7-color-status' });
    queue.complete(c.task.id, c.claim.token, { summary: 'Status colors follow NO_COLOR and CLICOLOR', artifacts: [{ kind: 'pr', value: 'https://github.com/pierricgimmig/q/pull/10' }] });
    const ago = (t, ms) => { t.created_at -= ms; t.updated_at -= ms; for (const e of t.events) e.created_at -= ms; };
    const minutesAgo = [26, 21, 19, 12, 9, 6, 90];
    let i = 0;
    for (const t of queue.tasks.values()) ago(t, (minutesAgo[i++] || 0) * 60000);
    queue.recent = [];
  }

  class Terminal {
    constructor(queue, scheduler) {
      this.queue = queue; this.scheduler = scheduler;
      this.out = $('#term-out'); this.body = $('#term-body'); this.input = $('#term-input'); this.form = $('#term-form');
      this.history = []; this.histIdx = -1; this.draft = '';
      this.log = (who, msg) => appendLog(who, msg);
      this.shell = makeCommands(queue, { log: this.log });
      this.bind();
      this.banner();
    }

    bind() {
      this.form.addEventListener('submit', (e) => { e.preventDefault(); this.submit(); });
      this.body.addEventListener('click', (e) => { if (e.target === this.body || e.target === this.out || this.out.contains(e.target)) { if (!window.getSelection().toString()) this.input.focus(); } });
      this.input.addEventListener('keydown', (e) => {
        if (e.key === 'ArrowUp') { e.preventDefault(); this.recall(-1); }
        else if (e.key === 'ArrowDown') { e.preventDefault(); this.recall(1); }
        else if (e.key === 'Tab') { e.preventDefault(); this.complete(); }
        else if (e.key === 'l' && e.ctrlKey) { e.preventDefault(); this.clear(); }
        else if (e.key === 'c' && e.ctrlKey && !window.getSelection().toString()) { e.preventDefault(); this.input.value = ''; this.print(`<span class="cmd"><span class="prompt">$</span> ^C</span>`); }
      });
    }

    print(html) { const div = document.createElement('div'); div.innerHTML = html; this.out.appendChild(div); this.body.scrollTop = this.body.scrollHeight; }
    echo(line) { this.print(`<div class="cmd"><span class="prompt">$</span>${esc(line)}</div>`); }
    clear() { this.out.innerHTML = ''; }

    banner() {
      this.print([
        dim('q 0.1.0 · in-browser demo · 3 fake agents are polling this queue'),
        dim('The sample tasks below are in inbox. Mark one ready and watch the agents panel.'),
        '',
        this.shell.runQ(['ls']),
        '',
      ].join('\n'));
    }

    recall(dir) {
      if (!this.history.length) return;
      if (this.histIdx === -1) { if (dir > 0) return; this.draft = this.input.value; this.histIdx = this.history.length - 1; }
      else { this.histIdx += dir; }
      if (this.histIdx >= this.history.length) { this.histIdx = -1; this.input.value = this.draft; return; }
      if (this.histIdx < 0) this.histIdx = 0;
      this.input.value = this.history[this.histIdx];
      requestAnimationFrame(() => this.input.setSelectionRange(this.input.value.length, this.input.value.length));
    }

    complete() {
      const value = this.input.value;
      const before = value.slice(0, this.input.selectionStart);
      const m = before.match(/^(\s*q\s+)([a-z-]*)$/);
      if (!m) {
        if (/^\s*[a-z]*$/.test(before)) {
          const word = before.trim();
          const c = ['q', 'help', 'clear', 'agents', 'reset'].filter((w) => w.startsWith(word));
          if (c.length === 1) this.input.value = c[0] + ' ' + value.slice(before.length);
          else if (c.length > 1) this.print(dim(c.join('  ')));
        }
        return;
      }
      const cands = SUBCOMMANDS.concat(Object.keys(ALIASES)).filter((s) => s.startsWith(m[2]));
      if (cands.length === 1) this.input.value = m[1] + cands[0] + ' ' + value.slice(before.length);
      else if (cands.length > 1) {
        let common = cands[0];
        for (const c of cands) while (!c.startsWith(common)) common = common.slice(0, -1);
        this.input.value = m[1] + common + value.slice(before.length);
        this.echo(value); this.print(dim(cands.join('  ')));
      }
    }

    submit() {
      const line = this.input.value;
      this.input.value = ''; this.histIdx = -1; this.draft = '';
      if (line.trim()) { if (this.history[this.history.length - 1] !== line) this.history.push(line); if (this.history.length > 200) this.history.shift(); }
      this.echo(line);
      const output = this.run(line);
      if (output) this.print(output);
      updateHint();
    }

    run(line) {
      let tokens;
      try { tokens = tokenize(line); } catch (e) { return `<span class="err">error:</span> ${esc(e.message)}`; }
      if (!tokens.length) return '';
      const [head, ...rest] = tokens;
      try {
        switch (head) {
          case 'q': return this.shell.runQ(rest);
          case 'help': case '?': return esc(HELP.main);
          case 'clear': this.clear(); return '';
          case 'agents': return this.scheduler.agents.map((a) => `${padEnd(a.name, 11)} ${padEnd(a.state, 8)} ${a.task ? dim('#' + a.task.id + ' ' + flatTitle(a.task.title)) : dim('polling q claim every few seconds')}  ${dim('max-risk ' + a.maxRisk + (a.kinds.length ? ' kinds ' + a.kinds.join(',') : ''))}`).join('\n');
          case 'reset': resetDemo(); return '';
          case 'ls': case 'pwd': case 'cd': case 'cat': case 'git': case 'cargo': case 'echo':
            return head === 'echo' ? esc(rest.join(' ')) : `<span class="err">${esc(head)}:</span> this shell only knows q (and help, clear, agents, reset)`;
          default: return `<span class="err">${esc(head)}:</span> command not found. Try <span class="bold">help</span>.`;
        }
      } catch (e) {
        if (e instanceof ShellError) return `<span class="err">error:</span> ${esc(e.message)}`;
        if (e instanceof QueueError) return `<span class="err">error:</span> ${esc(e.message)}`;
        console.error(e);
        return `<span class="err">error:</span> ${esc(e.message || String(e))}`;
      }
    }
  }

  // ── agents panel

  function appendLog(who, msg) {
    const list = $('#agent-log');
    const empty = list.querySelector('.empty'); if (empty) empty.remove();
    const li = document.createElement('li');
    const cls = typeof who === 'string' ? who : who.color;
    const name = typeof who === 'string' ? 'you' : who.name;
    li.innerHTML = `<span class="t">${iso(Date.now()).slice(11, 19)}</span><span class="who ${cls}">${esc(name)}</span><span class="msg">${msg}</span>`;
    list.appendChild(li);
    while (list.children.length > 80) list.removeChild(list.firstChild);
    list.scrollTop = list.scrollHeight;
  }

  function renderAgents(scheduler) {
    const list = $('#agent-list');
    const now = Date.now();
    list.innerHTML = scheduler.agents.map((a) => {
      const t = a.task && scheduler.queue.tasks.get(a.task.id);
      let state = 'idle · polling q claim';
      let lease = 0;
      if (a.state === 'claimed' && t) state = `claimed #${t.id} · starting…`;
      if (a.state === 'working' && t) {
        state = `in_progress #${t.id} · ${flatTitle(t.title)}`;
        const total = a.finishAt - (a.claimedAt || now);
        lease = Math.min(100, Math.max(0, ((now - (a.claimedAt || now)) / total) * 100));
      }
      return `<li class="agent ${a.state}"><span class="agent-dot"></span><span class="agent-name">${esc(a.name)}<small>${a.kinds.length ? 'research pool' : 'max-risk ' + a.maxRisk}</small></span><span class="agent-state">${esc(state)}</span>${a.state === 'working' ? `<span class="agent-lease"><i style="width:${lease.toFixed(0)}%"></i></span>` : ''}</li>`;
    }).join('');
  }

  function renderCounts(queue) {
    const s = queue.status();
    $('#counts').innerHTML = STATUSES.map((k) => `<span>${st(k)} <b>${s.counts[k]}</b></span>`).join('') + `<span class="sep">|</span><span>claims <b>${s.active_claims}</b> active</span>`;
  }

  // ── hints

  let hintIdx = -1;
  function hintsFor(queue) {
    const tasks = [...queue.tasks.values()];
    const inbox = tasks.filter((t) => t.status === 'inbox');
    const ready = tasks.filter((t) => t.status === 'ready');
    const active = tasks.filter((t) => t.status === 'claimed' || t.status === 'in_progress');
    const done = tasks.filter((t) => t.status === 'done');
    const high = tasks.find((t) => t.risk === 'high' && t.status === 'ready');
    const withDeps = tasks.find((t) => t.depends_on.length);
    const hints = [];
    if (inbox.length) hints.push(`q ready ${inbox[0].id}`);
    if (!tasks.length || tasks.length < 12) hints.push('q "Write the parser for .agentqueue.toml"');
    if (active.length) hints.push(`q show ${active[0].id}`);
    if (high) hints.push('q claim --agent me --max-risk high');
    if (ready.length && !high) hints.push('q claim --agent me');
    if (withDeps) hints.push(`q tree ${withDeps.id}`);
    hints.push('q ls');
    if (done.length) hints.push('q ls -a', `q reopen ${done[done.length - 1].id}`);
    hints.push('q status', 'q top', 'q tree --feature Rollout', 'q "Draft release notes" --kind documentation', 'help');
    return hints;
  }
  function updateHint() {
    const hints = hintsFor(state.queue);
    hintIdx = (hintIdx + 1) % hints.length;
    $('#hint-cmd').textContent = hints[hintIdx];
  }

  // ── wiring

  const state = {};

  function resetDemo() {
    state.queue.reset();
    seed(state.queue);
    state.scheduler.reset();
    $('#agent-log').innerHTML = '<li class="empty">agents are idle; mark a task ready to give them work</li>';
    state.term.clear();
    state.term.banner();
    renderCounts(state.queue); renderAgents(state.scheduler); updateHint();
  }

  function init() {
    const queue = new Queue();
    seed(queue);
    const scheduler = new Scheduler(queue, appendLog, () => renderAgents(scheduler));
    const term = new Terminal(queue, scheduler);
    Object.assign(state, { queue, scheduler, term });
    queue.onChange(() => { renderCounts(queue); renderAgents(scheduler); });
    $('#agent-log').innerHTML = '<li class="empty">agents are idle; mark a task ready to give them work</li>';
    renderCounts(queue); renderAgents(scheduler); updateHint();
    setInterval(() => renderAgents(scheduler), 1000);
    setInterval(updateHint, 9000);

    $('#hint-cmd').addEventListener('click', () => { term.input.value = $('#hint-cmd').textContent; term.input.focus(); });
    $('#reset').addEventListener('click', resetDemo);
    document.querySelectorAll('.copy').forEach((btn) => btn.addEventListener('click', async () => {
      const text = document.querySelector(btn.dataset.copy).textContent;
      try { await navigator.clipboard.writeText(text); btn.textContent = 'copied'; btn.classList.add('copied'); }
      catch { btn.textContent = 'select it'; }
      setTimeout(() => { btn.textContent = 'copy'; btn.classList.remove('copied'); }, 1600);
    }));

    // Debug/test hook. Not used by the page itself.
    window.qdemo = { queue, scheduler, term, run: (line) => term.run(line), Queue, tokenize };
  }

  if (document.readyState === 'loading') document.addEventListener('DOMContentLoaded', init); else init();
})();
