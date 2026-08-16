// SPDX-License-Identifier: Apache-2.0
/**
 * The **mirror** — an event-sourced projection of daemon state that renderers
 * (Ink, DOM) read and subscribe to. Writes come ONLY from the daemon: the
 * bootstrap `status` document, feed events, and task reads. The one local
 * exception is the optimistic echo of a just-sent prompt, reconciled when its
 * `message` event arrives (matched by messageId) — so N clients converge on
 * the same transcript, each rendering independently.
 */

import { normalizeTask } from './client.js';
import {
  ConnState,
  FeedEvent,
  FeedHello,
  InterfaceInfo,
  Json,
  MirrorState,
  TERMINAL_STATES,
  TaskView,
  TranscriptEntry,
} from './types.js';

const FEED_LOG_CAP = 500;
const TRANSCRIPT_CAP = 1000;

export class Mirror {
  private state: MirrorState = {
    conn: 'connecting',
    draining: false,
    tasks: new Map(),
    runs: new Map(),
    conversations: new Map(),
    subagents: new Map(),
    children: new Map(),
    transcript: [],
    feedLog: [],
    lastSeq: 0,
  };
  private listeners = new Set<() => void>();
  private version = 0;

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

  setInfo(info: InterfaceInfo): void {
    this.state.info = info;
    this.bump();
  }

  setCard(card: Json): void {
    this.state.card = card;
    this.bump();
  }

  onHello(h: FeedHello): void {
    this.state.hello = h;
    this.bump();
  }

  // ---- bootstrap ---------------------------------------------------------

  /** Adopt the full `status` document (connect / resync). */
  bootstrap(status: Json): void {
    const s = status as { [k: string]: Json };
    this.state.bootstrap = status;
    this.state.draining = s.draining === true;
    if (Array.isArray(s.runs)) {
      for (const r of s.runs) {
        const id = (r as { [k: string]: Json }).id;
        if (typeof id === 'string') this.state.runs.set(id, r);
      }
    }
    if (Array.isArray(s.conversations)) {
      for (const c of s.conversations) {
        const id = (c as { [k: string]: Json }).id;
        if (typeof id === 'string') this.state.conversations.set(id, c);
      }
    }
    if (Array.isArray(s.subagents)) {
      for (const sub of s.subagents) {
        const h = (sub as { [k: string]: Json }).handle;
        if (typeof h === 'string') this.state.subagents.set(h, sub);
      }
    }
    if (Array.isArray(s.children)) {
      for (const c of s.children) {
        const n = (c as { [k: string]: Json }).node;
        if (n !== undefined && n !== null) this.state.children.set(String(n), c);
      }
    }
    this.bump();
  }

  /** Adopt a task list / single task read. */
  adoptTasks(tasks: TaskView[]): void {
    for (const t of tasks) this.putTask(t);
    this.bump();
  }

  // ---- the optimistic echo ----------------------------------------------

  /** Echo a just-sent prompt (reconciled by messageId when its event lands). */
  localEcho(messageId: string, ctx: string | undefined, text: string, taskId?: string): void {
    this.upsertEntry({
      key: messageId,
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
      key: `note-${Date.now().toString(36)}-${this.state.transcript.length}`,
      ctx: '',
      ts: Date.now(),
      kind,
      text,
    });
    this.bump();
  }

  // ---- the feed ----------------------------------------------------------

  /** Fold one feed event in. This is the convergence path for EVERY client. */
  apply(ev: FeedEvent): void {
    if (ev.seq > this.state.lastSeq) this.state.lastSeq = ev.seq;
    this.state.feedLog.push(ev);
    if (this.state.feedLog.length > FEED_LOG_CAP) this.state.feedLog.shift();
    const data = (ev.data ?? {}) as { [k: string]: Json };
    switch (ev.kind) {
      case 'task': {
        const t = normalizeTask((data.task as Json) ?? null);
        if (t) {
          if (!t.link && data.link) t.link = data.link as TaskView['link'];
          if (!t.principal && typeof data.principal === 'string') t.principal = data.principal;
          this.putTask(t);
        }
        break;
      }
      case 'task.removed': {
        if (typeof data.id === 'string') this.state.tasks.delete(data.id);
        break;
      }
      case 'message': {
        // A prompt (possibly from ANOTHER client): upsert by messageId — the
        // local echo reconciles here.
        const key = (data.messageId as string) ?? `msg-${ev.seq}`;
        this.upsertEntry({
          key,
          ctx: (data.contextId as string) ?? '',
          ts: ev.ts,
          kind: 'user',
          text: (data.text as string) ?? '',
          taskId: (data.taskId as string) ?? undefined,
          principal: (data.principal as string) ?? undefined,
          pending: false,
        });
        break;
      }
      case 'command': {
        this.upsertEntry({
          key: `cmd-${ev.seq}`,
          ctx: (data.contextId as string) ?? '',
          ts: ev.ts,
          kind: 'command',
          text: (data.op as string) ?? 'command',
          principal: (data.principal as string) ?? undefined,
        });
        break;
      }
      case 'run': {
        const id = data.id;
        if (typeof id === 'string') this.state.runs.set(id, ev.data);
        break;
      }
      case 'run.removed': {
        if (typeof data.id === 'string') this.state.runs.delete(data.id);
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
      case 'status': {
        this.state.status = ev.data;
        this.state.draining = data.draining === true;
        break;
      }
      case 'lifecycle': {
        if (data.draining === true) {
          this.state.draining = true;
          this.note(`agentd is draining (${(data.reason as string) ?? ''})`);
        }
        break;
      }
      case 'config': {
        // A runtime `config.set` (possibly from ANOTHER client) — fold it into
        // the live info so every surface re-renders its chrome/debug panes.
        const path = data.path as string;
        const value = data.value;
        const info = this.state.info;
        if (info) {
          if (path === 'interface.debug' && typeof value === 'boolean') info.debug = value;
          if (path === 'interface.display.top' && Array.isArray(value))
            (info.display ??= { top: [], bottom: [] }).top = value as string[];
          if (path === 'interface.display.bottom' && Array.isArray(value))
            (info.display ??= { top: [], bottom: [] }).bottom = value as string[];
        }
        this.note(`config: ${path} = ${JSON.stringify(value)}`);
        break;
      }
      case 'pairing': {
        this.note(`a client paired (${(data.sessions as number) ?? '?'} live sessions)`);
        break;
      }
      default:
        // audit + future kinds land in feedLog only.
        break;
    }
    this.bump();
  }

  // ---- internals ---------------------------------------------------------

  /** Store a task and derive its transcript consequences. */
  private putTask(t: TaskView): void {
    this.state.tasks.set(t.id, t);
    const terminal = TERMINAL_STATES.has(t.state);
    const link = t.link as { turn?: { ctx: string } } | undefined;
    const isTurn = link?.turn !== undefined || t.contextId.length > 0;
    if (!isTurn) return;
    // A task becomes a transcript row only when its PROMPT is known (a user
    // entry / message event carries its taskId, or the row already exists).
    // That keeps command-result tasks (status/config/…) and pre-attach history
    // out of the conversation — they live on the Tasks screen instead. The one
    // exception is input-required: a client attaching mid-gate must see it.
    const known =
      this.state.transcript.some((e) => e.key === `task-${t.id}` || e.taskId === t.id);
    // The agent's reply rides the terminal artifact; input-required rides the
    // status message. Keyed by taskId so progress updates the same row.
    if (t.state === 'TASK_STATE_INPUT_REQUIRED') {
      this.upsertEntry({
        key: `task-${t.id}`,
        ctx: t.contextId,
        ts: t.updated,
        kind: 'agent',
        text: t.message ?? 'input required',
        taskId: t.id,
        inputRequired: true,
      });
    } else if (terminal && known) {
      const failed = t.state !== 'TASK_STATE_COMPLETED';
      const text = t.artifacts[0] ?? t.message ?? (failed ? t.state : '');
      if (text.length > 0) {
        this.upsertEntry({
          key: `task-${t.id}`,
          ctx: t.contextId,
          ts: t.updated,
          kind: failed ? 'error' : 'agent',
          text,
          taskId: t.id,
        });
      }
      // The user prompt that started it is no longer pending.
      for (const e of this.state.transcript) {
        if (e.taskId === t.id && e.kind === 'user') e.pending = false;
      }
    }
  }

  /** Insert-or-update a transcript entry by key, keeping order by ts. */
  private upsertEntry(entry: TranscriptEntry): void {
    const i = this.state.transcript.findIndex((e) => e.key === entry.key);
    if (i >= 0) {
      const prev = this.state.transcript[i];
      this.state.transcript[i] = { ...prev, ...entry, ts: prev.ts || entry.ts };
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

  /** All tasks, newest first. */
  allTasks(): TaskView[] {
    return [...this.state.tasks.values()].sort((a, b) => b.updated - a.updated);
  }
}
