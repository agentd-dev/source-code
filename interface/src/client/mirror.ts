// SPDX-License-Identifier: AGPL-3.0-only
/**
 * The **mirror** — an event-sourced projection of agent state that renderers
 * (Ink, DOM) read and subscribe to. Writes come ONLY from the agent: the
 * bootstrap `status` document, feed events, task reads and task stream
 * frames. The one local exception is the optimistic echo of a just-sent
 * prompt, reconciled when the task's core `history` carries the same
 * messageId — so N clients converge on the same transcript, each rendering
 * independently, and that holds in core mode too: history is an A2A field,
 * not an agentd one.
 */

import { normalizeTask } from './client.js';
import { COMMAND_DATA_KEY } from './ext.js';
import {
  Activity,
  ConnState,
  FeedEvent,
  FeedHello,
  inert,
  Json,
  MirrorState,
  StepRow,
  TERMINAL_STATES,
  TaskView,
  TranscriptEntry,
} from './types.js';
import type { Session } from './discovery.js';

const FEED_LOG_CAP = 500;
const TRANSCRIPT_CAP = 1000;

/** What an AUTH_REQUIRED row says: no reply in the conversation answers it. */
const AUTH_REQUIRED_TEXT = 'authorization required — complete it out of band, then continue';

type Obj = { [k: string]: Json };

function obj(v: Json | undefined): Obj | undefined {
  return v !== null && v !== undefined && typeof v === 'object' && !Array.isArray(v) ? v : undefined;
}

function str(v: Json | undefined): string | undefined {
  return typeof v === 'string' && v.length > 0 ? v : undefined;
}

/** A value from the feed, as inert text: a device's client_id is whatever its requester chose. */
function shown(v: Json | undefined): string {
  return inert(str(v) ?? '?');
}

/**
 * A terminal state, in words. A task that ends without an artifact or a message
 * still owes the reader an explanation, and `TASK_STATE_FAILED` is not one — it
 * is the wire enum leaking into the conversation.
 */
function whyItEnded(state: string): string {
  switch (state) {
    case 'TASK_STATE_FAILED':
      return 'the task failed (no reason reported — check the daemon log)';
    case 'TASK_STATE_CANCELED':
      return 'canceled';
    case 'TASK_STATE_REJECTED':
      return 'rejected before it ran';
    default:
      return state.replace(/^TASK_STATE_/, '').toLowerCase().replace(/_/g, ' ');
  }
}

/** One artifact as a task stream built it (text chunks joined, data parts kept). */
interface StreamArtifact {
  text: string;
  data: Json[];
}

/** The text and data parts of one artifact. */
function artifactParts(a: Obj): StreamArtifact {
  const v = normalizeTask({ id: '_', artifacts: [a] }, { annotations: false });
  return { text: (v?.artifacts ?? []).join('\n'), data: v?.artifactData ?? [] };
}

export class Mirror {
  private state: MirrorState = {
    conn: 'connecting',
    draining: false,
    paused: false,
    tasks: new Map(),
    runs: new Map(),
    steps: new Map(),
    conversations: new Map(),
    subagents: new Map(),
    children: new Map(),
    activity: new Map(),
    transcript: [],
    feedLog: [],
    lastSeq: 0,
    epoch: 0,
  };
  private listeners = new Set<() => void>();
  private configListeners = new Set<() => void>();
  private version = 0;
  private notes = 0;
  /**
   * Artifacts a task stream is assembling, by task then artifactId. A stream
   * sends an artifact in chunks (`append`), which the flattened TaskView
   * cannot express; the pieces live here until the task ends.
   */
  private streamArtifacts = new Map<string, Map<string, StreamArtifact>>();

  /** Subscribe to changes (returns the unsubscriber). */
  subscribe = (fn: () => void): (() => void) => {
    this.listeners.add(fn);
    return () => this.listeners.delete(fn);
  };

  /** A monotonically increasing change stamp (for useSyncExternalStore). */
  getVersion = (): number => this.version;

  /** The current state (mutated in place; use `getVersion` for change detection). */
  getState = (): MirrorState => this.state;

  private bump(): void {
    this.version++;
    for (const fn of this.listeners) fn();
  }

  // ---- connection lifecycle ---------------------------------------------

  setConn(conn: ConnState, error?: string): void {
    this.state.conn = conn;
    this.state.error = error;
    this.bump();
  }

  /** Adopt what discovery settled: the card(s) and what they declare. */
  setSession(session: Session): void {
    this.state.session = session;
    this.bump();
  }

  /**
   * Call `fn` when the agent reports a live config change — what the card
   * offers may have moved with it. Returns the unsubscriber.
   */
  onConfig(fn: () => void): () => void {
    this.configListeners.add(fn);
    return () => this.configListeners.delete(fn);
  }

  /**
   * The feed's opening frame. `resync` means the cursor this client resumed
   * from no longer names a place in the agent's feed — it was evicted, or the
   * daemon restarted and numbers from 1 again — and the agent replays from
   * the start of its window. So the cursor goes back to 0 BEFORE any replayed
   * event is applied (otherwise every one of them would look like a replay of
   * something already seen and be dropped), and the epoch moves on so rows
   * keyed by the old numbering cannot collide with the new.
   */
  onHello(h: FeedHello): void {
    if (h.resync) {
      this.state.lastSeq = 0;
      this.state.epoch++;
    }
    this.state.hello = h;
    this.bump();
  }

  // ---- bootstrap ---------------------------------------------------------

  /**
   * Adopt the full `status` document (connect, resync, a poll). Each section
   * it carries REPLACES the mirror's, and each it leaves out is CLEARED: a
   * re-bootstrap after a daemon restart must not keep the previous run's
   * sections, and a caller-scoped document omits what the caller may not
   * see, which must then show as nothing rather than as stale data. Tasks are
   * not touched: they come from ListTasks, which is paged.
   */
  bootstrap(status: Json): void {
    const s = obj(status) ?? {};
    this.state.bootstrap = status;
    // The live `status` events restart from this document.
    this.state.status = undefined;
    this.state.draining = s.draining === true;
    this.state.paused = s.paused === true;
    const keyed = (list: Json | undefined, key: (o: Obj) => string | undefined): Map<string, Json> => {
      const m = new Map<string, Json>();
      for (const raw of Array.isArray(list) ? list : []) {
        const o = obj(raw);
        const k = o ? key(o) : undefined;
        if (o && k !== undefined) m.set(k, raw);
      }
      return m;
    };
    this.state.runs = keyed(s.runs, (o) => str(o.id));
    this.state.conversations = keyed(s.conversations, (o) => str(o.id));
    this.state.subagents = keyed(s.subagents, (o) => str(o.handle));
    this.state.children = keyed(s.children, (o) => (o.node !== undefined && o.node !== null ? String(o.node) : undefined));
    this.state.activity = new Map();
    for (const a of Array.isArray(s.activity) ? s.activity : []) {
      const rec = a as unknown as Activity;
      if (rec?.id) this.state.activity.set(rec.id, rec);
    }
    // Step rows belong to runs; a run that is gone takes its steps with it.
    for (const run of [...this.state.steps.keys()]) {
      if (!this.state.runs.has(run)) this.state.steps.delete(run);
    }
    this.bump();
  }

  /** Adopt a task list / single task read. */
  adoptTasks(tasks: TaskView[]): void {
    for (const t of tasks) this.putTask(t);
    this.bump();
  }

  /** Forget a task the agent no longer knows (or no longer shows this caller). */
  removeTask(id: string): void {
    this.state.tasks.delete(id);
    this.streamArtifacts.delete(id);
    this.bump();
  }

  // ---- the optimistic echo ----------------------------------------------

  /** Echo a just-sent prompt (reconciled by messageId when the task's history carries it). */
  localEcho(messageId: string, ctx: string | undefined, text: string, taskId?: string): void {
    this.upsertEntry({
      key: `echo-${messageId}`,
      ctx: ctx ?? '',
      ts: Date.now(),
      kind: 'user',
      text,
      taskId,
      pending: true,
    });
    this.bump();
  }

  /** A local client-side note (errors, hints) — never sent anywhere. */
  note(text: string, kind: 'info' | 'error' = 'info'): void {
    this.upsertEntry({
      key: `note-${Date.now().toString(36)}-${this.notes++}`,
      ctx: '',
      ts: Date.now(),
      kind,
      text,
    });
    this.bump();
  }

  /**
   * A note derived from a feed event, keyed by the event itself: applying the
   * same event twice (a replay the seq check did not catch) updates the same
   * row instead of adding another. `n` tells apart two notes of one event.
   */
  private feedNote(ev: FeedEvent, text: string, n = 0): void {
    this.upsertEntry({
      key: `feed-${this.state.epoch}-${ev.seq}${n > 0 ? `-${n}` : ''}`,
      ctx: '',
      ts: ev.ts,
      kind: 'info',
      text,
    });
  }

  /** An agent's direct `Message` reply (a `{message}` send result or stream frame). */
  agentMessage(message: Json): void {
    const m = obj(message);
    const id = str(m?.messageId);
    if (!m || id === undefined) return;
    const v = normalizeTask({ id: '_', history: [{ ...m, role: 'ROLE_AGENT' }] }, { annotations: false });
    const h = v?.history[0];
    const text = h && h.text.length > 0 ? h.text : h && h.data.length > 0 ? JSON.stringify(h.data[0]) : '';
    if (text.length === 0) return;
    this.upsertEntry({
      key: `msg-${id}`,
      ctx: str(m.contextId) ?? '',
      ts: Date.now(),
      kind: 'agent',
      text,
      taskId: str(m.taskId),
    });
    this.bump();
  }

  // ---- task streams (SendStreamingMessage, SubscribeToTask) ---------------

  /**
   * Fold one `StreamResponse` in: `{task}`, `{message}`, `{statusUpdate}` or
   * `{artifactUpdate}`. This is how a client with no feed still sees a task
   * move — the spec's own per-task stream.
   */
  applyStream(frame: Json): void {
    const f = obj(frame);
    if (!f) return;
    const annotations = this.state.session?.caps.annotations ?? true;
    if (f.task !== undefined) {
      const raw = obj(f.task);
      const t = normalizeTask(f.task, { annotations });
      if (!raw || !t) return;
      const arts = new Map<string, StreamArtifact>();
      for (const a of Array.isArray(raw.artifacts) ? raw.artifacts : []) {
        const o = obj(a);
        const id = str(o?.artifactId);
        if (o && id !== undefined) arts.set(id, artifactParts(o));
      }
      this.streamArtifacts.set(t.id, arts);
      this.putStreamed(t);
    } else if (f.message !== undefined) {
      this.agentMessage(f.message);
      return;
    } else if (f.statusUpdate !== undefined) {
      const u = obj(f.statusUpdate);
      const id = str(u?.taskId);
      if (!u || id === undefined) return;
      // The status alone, read the way a whole task's status is read.
      const s = normalizeTask({ id, contextId: u.contextId ?? '', status: u.status ?? null }, { annotations: false });
      if (!s) return;
      const prev = this.state.tasks.get(id) ?? { ...s, message: undefined };
      const next: TaskView = { ...prev, state: s.state, updated: s.updated || prev.updated };
      if (!next.contextId) next.contextId = s.contextId;
      if (s.message !== undefined) next.message = s.message;
      else delete next.message;
      this.putStreamed(next);
    } else if (f.artifactUpdate !== undefined) {
      const u = obj(f.artifactUpdate);
      const id = str(u?.taskId);
      const a = obj(u?.artifact);
      const artifactId = str(a?.artifactId);
      if (!u || !a || id === undefined || artifactId === undefined) return;
      const arts = this.streamArtifacts.get(id) ?? new Map<string, StreamArtifact>();
      this.streamArtifacts.set(id, arts);
      const piece = artifactParts(a);
      const had = arts.get(artifactId);
      // `append` continues the artifact with this id; without it the artifact
      // is sent whole and replaces what was there.
      arts.set(artifactId, u.append === true && had ? { text: had.text + piece.text, data: [...had.data, ...piece.data] } : piece);
      const prev: TaskView = this.state.tasks.get(id) ?? {
        id,
        contextId: str(u.contextId) ?? '',
        state: 'TASK_STATE_WORKING',
        artifacts: [],
        artifactData: [],
        history: [],
        updated: 0,
      };
      this.putStreamed(prev);
    } else {
      return;
    }
    this.bump();
  }

  /** Store a task a stream moved, with the artifacts the stream assembled. */
  private putStreamed(t: TaskView): void {
    const arts = this.streamArtifacts.get(t.id);
    const view =
      arts && arts.size > 0
        ? {
            ...t,
            artifacts: [...arts.values()].map((a) => a.text).filter((x) => x.length > 0),
            artifactData: [...arts.values()].flatMap((a) => a.data),
          }
        : t;
    this.putTask(view);
    // A finished task's stream is over: its pieces are in the view now.
    if (TERMINAL_STATES.has(view.state)) this.streamArtifacts.delete(t.id);
  }

  // ---- the feed ----------------------------------------------------------

  /** Fold one feed event in. This is the convergence path for EVERY client. */
  apply(ev: FeedEvent): void {
    // A replay overlap (the agent resends from a cursor this client already
    // passed) must not apply an event twice: a step would grow a second row,
    // a lifecycle change a second note.
    if (ev.seq <= this.state.lastSeq) return;
    this.state.lastSeq = ev.seq;
    this.state.feedLog.push(ev);
    if (this.state.feedLog.length > FEED_LOG_CAP) this.state.feedLog.shift();
    const data = obj(ev.data) ?? {};
    switch (ev.kind) {
      case 'task': {
        // The feed activates the annotations only when the card declares
        // them; without that, what sits under the key was not written under
        // the extension's contract. A mirror with no session yet has no card
        // to ask, and reads them.
        const t = normalizeTask(data.task ?? null, {
          annotations: this.state.session?.caps.annotations ?? true,
        });
        if (t) this.putTask(t);
        break;
      }
      case 'task.removed': {
        if (typeof data.id === 'string') {
          this.state.tasks.delete(data.id);
          this.streamArtifacts.delete(data.id);
        }
        break;
      }
      case 'run': {
        const id = data.id;
        if (typeof id === 'string') this.state.runs.set(id, ev.data);
        break;
      }
      case 'run.removed': {
        if (typeof data.id === 'string') {
          this.state.runs.delete(data.id);
          this.state.steps.delete(data.id);
        }
        break;
      }
      case 'step': {
        // A step's life is two events. Collapse them onto one row so the UI
        // shows "what is happening" rather than a scrolling pair per step —
        // `start` creates the row, `done` completes it in place.
        const run = data.run as string | undefined;
        const step = data.step as string | undefined;
        if (!run || !step) break;
        const rows = this.state.steps.get(run) ?? [];
        const phase = data.phase === 'done' ? 'done' : 'start';
        const existing = rows.findIndex((r) => r.step === step && r.phase === 'start');
        const now = Date.now();
        const startedAt = phase === 'done' ? rows[existing]?.startedAt : now;
        const row: StepRow = {
          step,
          kind: (data.kind as string) ?? rows[existing]?.kind,
          phase,
          status: data.status as string | undefined,
          attempt: data.attempt as number | undefined,
          tokens: data.tokens as number | undefined,
          err: (data.err as string) ?? undefined,
          at: now,
          startedAt,
          // How long the step took. Measured from the events the client
          // actually saw rather than from daemon clocks, so it is honest about
          // being the observer's view — and a step whose start was missed
          // (joined mid-run) reports no duration rather than a wrong one.
          ms: phase === 'done' && startedAt !== undefined ? now - startedAt : undefined,
        };
        if (phase === 'done' && existing >= 0) rows[existing] = row;
        else rows.push(row);
        // A long run should not grow without bound in a client's memory.
        this.state.steps.set(run, rows.slice(-200));
        break;
      }
      case 'conversation': {
        const id = data.id;
        if (typeof id === 'string') this.state.conversations.set(id, ev.data);
        break;
      }
      case 'conversation.removed': {
        if (typeof data.id === 'string') this.state.conversations.delete(data.id);
        break;
      }
      case 'subagent': {
        const h = data.handle;
        if (typeof h === 'string') this.state.subagents.set(h, ev.data);
        break;
      }
      case 'subagent.removed': {
        if (typeof data.id === 'string') this.state.subagents.delete(data.id);
        break;
      }
      case 'child': {
        const n = data.node;
        if (n !== undefined && n !== null) this.state.children.set(String(n), ev.data);
        break;
      }
      case 'child.removed': {
        if (typeof data.id === 'string') this.state.children.delete(data.id);
        break;
      }
      case 'activity': {
        const rec = ev.data as unknown as Activity;
        if (rec?.id) this.state.activity.set(rec.id, rec);
        break;
      }
      case 'activity.removed': {
        if (typeof data.id === 'string') this.state.activity.delete(data.id);
        break;
      }
      case 'status': {
        this.state.status = ev.data;
        this.state.draining = data.draining === true;
        if (typeof data.paused === 'boolean') this.state.paused = data.paused;
        break;
      }
      case 'lifecycle': {
        let n = 0;
        if (data.draining === true) {
          this.state.draining = true;
          this.feedNote(ev, `agentd is draining (${shown(data.reason)})`, n++);
        }
        if (typeof data.paused === 'boolean') {
          this.state.paused = data.paused;
          this.feedNote(ev, data.paused ? `agentd paused (${inert(str(data.reason) ?? 'operator')})` : 'agentd resumed', n++);
        }
        break;
      }
      case 'config': {
        // A runtime `admin.set` or a reload (possibly from ANOTHER client).
        // The layout is this client's own, so nothing here reshapes it — but
        // what the card offers may have changed, so whoever holds the session
        // re-reads it. Only for a live change: a replayed one is history the
        // session was opened after.
        const paths = Array.isArray(data.paths) ? data.paths.filter((p): p is string => typeof p === 'string') : [];
        const source = str(data.source);
        this.feedNote(
          ev,
          `config changed${paths.length > 0 ? `: ${paths.map(inert).join(', ')}` : ''}${source ? ` (${inert(source)})` : ''}`,
        );
        if (ev.seq > (this.state.hello?.seq ?? 0)) for (const fn of this.configListeners) fn();
        break;
      }
      case 'auth': {
        // Operator-only: sign-ins and sessions. The client_id and user code
        // come from whoever asked for a device sign-in, so they are shown
        // inert, never as terminal input.
        const code = shown(data.user_code);
        const scope = shown(data.scope);
        switch (data.event) {
          case 'pending':
            this.feedNote(
              ev,
              `device ${code} (${shown(data.client_id)}${data.peer !== undefined ? `, ${shown(data.peer)}` : ''}) ` +
                `requests ${scope} access — /approve ${code} <name>`,
            );
            break;
          case 'approved':
            this.feedNote(ev, `device ${code} approved as ${shown(data.name)} (${scope})`);
            break;
          case 'denied':
            this.feedNote(ev, `device ${code} denied`);
            break;
          case 'revoked':
            this.feedNote(ev, `session ${shown(data.sid)}${data.name !== undefined ? ` (${shown(data.name)})` : ''} revoked`);
            break;
          case 'launch':
            this.feedNote(ev, `${shown(data.client_id)} signed in through the launcher (session ${shown(data.sid)})`);
            break;
          default:
            this.feedNote(ev, `auth: ${shown(data.event)}`);
        }
        break;
      }
      default:
        // audit + future kinds land in feedLog only.
        break;
    }
    this.bump();
  }

  // ---- internals ---------------------------------------------------------

  /**
   * Store a task and derive its transcript rows. The conversation comes from
   * the task's core `history` — who said what, from whichever client — and
   * the task's own state: the gate it stopped at, or how it ended.
   */
  private putTask(t: TaskView): void {
    this.state.tasks.set(t.id, t);
    const isTurn = t.link?.kind === 'turn' || t.contextId.length > 0;
    if (!isTurn) return;
    this.historyRows(t);
    // A task gets a reply row only when its PROMPT is known — a history row,
    // or the local echo carries its taskId. That keeps tasks whose start this
    // client never saw off the conversation; they live on the Tasks screen.
    // The exception is a gate: a client attaching mid-gate must see it.
    const known = this.state.transcript.some((e) => e.taskId === t.id && (e.kind === 'user' || e.kind === 'command'));
    const key = `task-${t.id}`;
    // The task's own state reads after everything its history says, whatever
    // the clocks: an echo is stamped by this client, the rest by the agent.
    let floor = -Infinity;
    for (const e of this.state.transcript) if (e.taskId === t.id && e.key !== key) floor = Math.max(floor, e.ts);
    const ts = Math.max(this.state.transcript.find((e) => e.key === key)?.ts ?? t.updated, floor + 1);
    if (t.state === 'TASK_STATE_INPUT_REQUIRED') {
      this.upsertEntry({
        key,
        ctx: t.contextId,
        ts,
        kind: 'agent',
        text: t.message ?? 'input required',
        taskId: t.id,
        inputRequired: true,
        authRequired: undefined,
      }, true);
    } else if (t.state === 'TASK_STATE_AUTH_REQUIRED') {
      this.upsertEntry({
        key,
        ctx: t.contextId,
        ts,
        kind: 'agent',
        text: t.message ? `${AUTH_REQUIRED_TEXT}\n${t.message}` : AUTH_REQUIRED_TEXT,
        taskId: t.id,
        inputRequired: undefined,
        authRequired: true,
      }, true);
    } else if (TERMINAL_STATES.has(t.state) && known) {
      const failed = t.state !== 'TASK_STATE_COMPLETED';
      // When the work began: the prompt that started it, else the task's own
      // first transition. A turn adopted mid-flight has neither and reports no
      // duration rather than a number measured from when we happened to look.
      const started =
        this.state.transcript.find((e) => e.taskId === t.id && e.kind === 'user' && e.key !== key)?.ts ??
        t.statusHistory?.[0]?.ts ??
        t.created ??
        0;
      const data = t.artifactData.length > 0 ? JSON.stringify(t.artifactData[0]) : undefined;
      const text = t.artifacts[0] ?? data ?? t.message ?? (failed ? whyItEnded(t.state) : '');
      if (text.length > 0) {
        this.upsertEntry({
          key,
          ctx: t.contextId,
          ts,
          kind: failed ? 'error' : 'agent',
          text,
          taskId: t.id,
          inputRequired: undefined,
          authRequired: undefined,
          // Measured across the task's own life, so it is the agent's view of
          // the work rather than the client's view of the network.
          ms: t.updated > 0 && started > 0 ? Math.max(0, t.updated - started) : undefined,
        }, true);
      }
      // The prompt that started it is no longer pending.
      for (const e of this.state.transcript) {
        if (e.taskId === t.id && e.kind === 'user') e.pending = false;
      }
    } else {
      this.leftGate(t);
    }
  }

  /**
   * One row per history message: a person's text and an agent's superseded
   * status, each keyed by its task AND its messageId, and a command — a user
   * message that is only data — as one `command` row for the task.
   *
   * The sender chooses a messageId, so it is never a key on its own: a
   * message whose id was `task-<id>` or `feed-0-1` would otherwise overwrite
   * that row — another principal's gate, or a note — and inherit whatever
   * the history row does not set. The local echo becomes the history row
   * only by the explicit reconciliation in {@link claimEcho}.
   */
  private historyRows(t: TaskView): void {
    const base = t.statusHistory?.[0]?.ts ?? t.created ?? t.updated;
    // History carries no timestamps. A row keeps the time it already has (the
    // echo's), else the task's start plus its position; either way it goes
    // after the message before it, so a task's rows stay in the order the
    // agent recorded them even when two clocks disagree.
    let floor = -Infinity;
    const place = (entry: Omit<TranscriptEntry, 'ts'>, i: number): void => {
      const ts = Math.max(this.state.transcript.find((e) => e.key === entry.key)?.ts ?? base + i, floor + 1);
      floor = ts;
      this.upsertEntry({ ...entry, ts }, true);
    };
    t.history.forEach((m, i) => {
      const key = `h-${t.id}-${m.messageId}`;
      if (m.role === 'ROLE_USER' && m.text.length > 0) {
        this.claimEcho(m.messageId, t.id, key);
        place({ key, ctx: t.contextId, kind: 'user', text: m.text, taskId: t.id, principal: t.principal, pending: false }, i);
      } else if (m.role === 'ROLE_USER' && m.data.length > 0) {
        const env = obj(obj(m.data[0])?.[COMMAND_DATA_KEY]);
        place(
          {
            key: `cmd-${t.id}`,
            ctx: t.contextId,
            kind: 'command',
            text: t.command ?? str(env?.op) ?? 'command',
            taskId: t.id,
            principal: t.principal,
          },
          i,
        );
      } else if (m.role === 'ROLE_AGENT' && m.text.length > 0) {
        place({ key, ctx: t.contextId, kind: 'agent', text: m.text, taskId: t.id }, i);
      }
    });
  }

  /**
   * This client's pending prompt, now in task `taskId`'s history as `key`:
   * the echo row becomes that row, keeping its place. Only an echo sent for
   * that task (or before its task was known) is claimed — never one another
   * task's history happens to name.
   */
  private claimEcho(messageId: string, taskId: string, key: string): void {
    const i = this.state.transcript.findIndex((e) => e.key === `echo-${messageId}`);
    const echo = this.state.transcript[i];
    if (!echo || (echo.taskId !== undefined && echo.taskId !== taskId)) return;
    if (this.state.transcript.some((e) => e.key === key)) this.state.transcript.splice(i, 1);
    else this.state.transcript[i] = { ...echo, key };
  }

  /**
   * A task that was at a gate and is working again. Its question now sits in
   * history as a superseded status message; when that row is there, the gate
   * row would only repeat it, so it goes. Otherwise it stays as what was
   * asked, no longer answerable.
   */
  private leftGate(t: TaskView): void {
    const i = this.state.transcript.findIndex((e) => e.key === `task-${t.id}`);
    const row = this.state.transcript[i];
    if (!row || (!row.inputRequired && !row.authRequired)) return;
    if (t.history.some((m) => m.role === 'ROLE_AGENT' && m.text === row.text)) {
      this.state.transcript.splice(i, 1);
      return;
    }
    this.state.transcript[i] = { ...row, inputRequired: undefined, authRequired: undefined };
  }

  /**
   * Insert-or-update a transcript entry by key, keeping order by ts. An
   * update keeps the row's time unless `placed` says the caller chose it.
   */
  private upsertEntry(entry: TranscriptEntry, placed = false): void {
    const i = this.state.transcript.findIndex((e) => e.key === entry.key);
    if (i >= 0) {
      const prev = this.state.transcript[i];
      const ts = placed ? entry.ts : prev.ts || entry.ts;
      this.state.transcript[i] = { ...prev, ...entry, ts };
      if (ts !== prev.ts) this.state.transcript.sort((a, b) => a.ts - b.ts);
      return;
    }
    this.state.transcript.push(entry);
    if (this.state.transcript.length > TRANSCRIPT_CAP) this.state.transcript.shift();
    // Keep chronological order (events can arrive slightly out of order).
    this.state.transcript.sort((a, b) => a.ts - b.ts);
  }

  // ---- selectors ---------------------------------------------------------

  /** Tasks still working / awaiting input, newest first. */
  activeTasks(): TaskView[] {
    return [...this.state.tasks.values()]
      .filter((t) => !TERMINAL_STATES.has(t.state))
      .sort((a, b) => b.updated - a.updated);
  }

  /** The live activity for a task (or the newest overall when none given). */
  activityFor(taskId?: string): Activity | undefined {
    const all = [...this.state.activity.values()];
    if (taskId) {
      const hit = all.find((a) => a.task === taskId);
      if (hit) return hit;
    }
    return all.sort((a, b) => b.updated_ms - a.updated_ms)[0];
  }

  /** All tasks, newest first. */
  allTasks(): TaskView[] {
    return [...this.state.tasks.values()].sort((a, b) => b.updated - a.updated);
  }

  /**
   * The workflows this caller may run: from the extended card when one was
   * read (it lists them per caller), else from the `status` document's
   * `workflows` — the one other place an agent names them.
   */
  workflows(): string[] {
    const caps = this.state.session?.caps;
    if (caps?.extendedCard) return caps.workflows;
    const list = obj(this.state.bootstrap)?.workflows;
    return (Array.isArray(list) ? list : []).map((w) => str(obj(w)?.name)).filter((n): n is string => n !== undefined);
  }
}

/**
 * The introspection reads are on for this client. The extended card says so
 * for this caller when one was read; when the cards cannot tell (`null` — an
 * agent with no listener auth serves no extended card) the feed's
 * `hello.introspection` decides. `null` is "unknown", not "off".
 */
export function introspectionOn(s: MirrorState): boolean {
  const caps = s.session?.caps.introspection;
  if (caps === true || caps === false) return caps;
  return s.hello?.introspection === true;
}
