// SPDX-License-Identifier: AGPL-3.0-only
/**
 * `AgentdClient` — agentd's declared extensions on top of the core A2A client.
 *
 * {@link A2aClient} speaks what any A2A 1.0 agent understands. This class adds
 * command/v2 (structured ops as a DataPart), events/v1 (the observation feed)
 * and task-annotations/v1 (agentd's facts about a task) — each only when the
 * card this session was opened from DECLARES it. A call the card does not
 * back is refused here, before anything reaches the wire: a command DataPart
 * sent to an agent that never promised to read one as a command is just a
 * message, and a model on the other end may act on it.
 *
 * Thin by design (RFC 0032): the daemon hosts state, tools and secrets; this
 * class only forwards intent and reads projections.
 */

import {
  ClientError,
  Endpoint,
  FeedEvent,
  FeedGoodbye,
  FeedHello,
  HistoryMessage,
  Json,
  TaskLink,
  TaskState,
  TaskView,
} from './types.js';
import { CallOptions, rpcStream } from './wire.js';
import { A2aClient, commandMessage, ListQuery, userMessage } from './a2a.js';
import type { Capabilities } from './discovery.js';
import { COMMAND_EXTENSION, EVENTS_EXTENSION, EVENTS_METHOD, OPS, TASK_ANNOTATIONS_EXTENSION } from './ext.js';

type Obj = { [k: string]: Json };

function obj(v: Json | undefined): Obj | undefined {
  return v !== null && v !== undefined && typeof v === 'object' && !Array.isArray(v) ? v : undefined;
}

function str(v: Json | undefined): string | undefined {
  return typeof v === 'string' && v.length > 0 ? v : undefined;
}

/** Epoch ms of an RFC 3339 timestamp (how ProtoJSON spells a Timestamp). */
function epochMs(v: Json | undefined): number | undefined {
  if (typeof v !== 'string') return undefined;
  const ms = Date.parse(v);
  return Number.isNaN(ms) ? undefined : ms;
}

/** The text parts of a part list, joined; and its data parts. */
function partsOf(parts: Json | undefined): { text: string[]; data: Json[] } {
  const text: string[] = [];
  const data: Json[] = [];
  for (const raw of Array.isArray(parts) ? parts : []) {
    const p = obj(raw);
    if (!p) continue;
    if (typeof p.text === 'string' && p.text.length > 0) text.push(p.text);
    else if (p.data !== undefined) data.push(p.data);
  }
  return { text, data };
}

const LINK_KINDS: ReadonlySet<string> = new Set(['run', 'subagent', 'turn']);

/** Read the task-annotations/v1 object into the view's annotation fields. */
function annotationsOf(a: Obj): Partial<TaskView> {
  const out: Partial<TaskView> = {};
  const link = obj(a.link);
  const kind = str(link?.kind);
  const id = str(link?.id);
  if (kind !== undefined && LINK_KINDS.has(kind) && id !== undefined) {
    out.link = { kind: kind as TaskLink['kind'], id };
  }
  const principal = str(a.principal);
  if (principal !== undefined) out.principal = principal;
  const command = str(a.command);
  if (command !== undefined) out.command = command;
  const created = epochMs(a.created);
  if (created !== undefined) out.created = created;
  if (Array.isArray(a.statusHistory)) {
    const hist: { state: TaskState; ts: number }[] = [];
    for (const raw of a.statusHistory) {
      const h = obj(raw);
      const state = str(h?.state);
      const ts = epochMs(h?.ts);
      if (state !== undefined && ts !== undefined) hist.push({ state: state as TaskState, ts });
    }
    out.statusHistory = hist;
  }
  if (a.askSchema !== undefined && a.askSchema !== null) out.askSchema = a.askSchema;
  return out;
}

/** How {@link normalizeTask} reads a task. */
export interface NormalizeOptions {
  /**
   * Read `metadata[task-annotations/v1]`. False when the reply showed the
   * extension was not active for the call: what sits under the key then was
   * not written under the extension's contract, so it is not read as such.
   */
  annotations?: boolean;
}

/**
 * Flatten an A2A `Task` into the client view. Core fields come from the Task;
 * agentd's own facts only from `metadata[<task-annotations/v1 URI>]`. The old
 * `agentd/*` metadata keys and the flat top-level fields are not read: a
 * v1.17 daemon writes neither, and reading them would keep a private shape
 * alive in every client that copied this one.
 */
export function normalizeTask(t: Json, o: NormalizeOptions = {}): TaskView | null {
  const task = obj(t);
  const id = str(task?.id);
  if (!task || id === undefined) return null;
  const status = obj(task.status);
  const artifacts: string[] = [];
  const artifactData: Json[] = [];
  for (const raw of Array.isArray(task.artifacts) ? task.artifacts : []) {
    const p = partsOf(obj(raw)?.parts);
    artifacts.push(...p.text);
    artifactData.push(...p.data);
  }
  const history: HistoryMessage[] = [];
  for (const raw of Array.isArray(task.history) ? task.history : []) {
    const m = obj(raw);
    const role = m?.role;
    const messageId = str(m?.messageId);
    if (!m || messageId === undefined || (role !== 'ROLE_USER' && role !== 'ROLE_AGENT')) continue;
    const p = partsOf(m.parts);
    history.push({ messageId, role, text: p.text.join('\n'), data: p.data });
  }
  const message = partsOf(obj(status?.message)?.parts).text;
  const view: TaskView = {
    id,
    contextId: str(task.contextId) ?? '',
    state: (str(status?.state) ?? 'TASK_STATE_UNSPECIFIED') as TaskState,
    artifacts,
    artifactData,
    history,
    updated: epochMs(status?.timestamp) ?? 0,
  };
  if (message.length > 0) view.message = message.join('\n');
  const ann = o.annotations === false ? undefined : obj(obj(task.metadata)?.[TASK_ANNOTATIONS_EXTENSION]);
  return ann ? { ...view, ...annotationsOf(ann) } : view;
}

/** A command's reply: a read answers with a Message, work with a Task. */
export type CommandReply =
  | { kind: 'message'; message: Json; data: Json | undefined }
  | { kind: 'task'; task: TaskView; data: Json | undefined };

/** The first DataPart's `data` in a part list. */
function firstData(parts: Json | undefined): Json | undefined {
  for (const raw of Array.isArray(parts) ? parts : []) {
    const p = obj(raw);
    if (p && p.data !== undefined) return p.data;
  }
  return undefined;
}

/**
 * Read a command's `SendMessageResponse`, which is exactly one of:
 * - `{message}` — a read op's answer: `data` is its first DataPart;
 * - `{task}` — work: `data` is the first DataPart of THIS task's result
 *   artifact (`<task id>.result`), and nothing else. A task still working has
 *   none yet, and any other artifact (a model's own data, another task's
 *   result) is not the command's answer, so `data` is then undefined.
 * Anything else is not a command reply, and is refused rather than guessed at.
 */
export function commandReply(result: Json, o: NormalizeOptions = {}): CommandReply {
  const r = obj(result);
  const message = obj(r?.message);
  if (message) return { kind: 'message', message, data: firstData(message.parts) };
  const task = normalizeTask(r?.task ?? null, o);
  if (task) {
    const arts = Array.isArray(obj(r?.task)?.artifacts) ? (obj(r?.task)?.artifacts as Json[]) : [];
    const result = arts.map(obj).find((a) => a?.artifactId === `${task.id}.result`);
    return { kind: 'task', task, data: firstData(result?.parts) };
  }
  throw new ClientError('invalid-response', 'command reply is neither a Task nor a Message');
}

/** The message envelope for a natural-language send. */
export interface SendOptions {
  /** Continue this conversation (omit to open a new one). Ignored with `taskId`. */
  contextId?: string;
  /** Answer this task's input-required gate / continue it. */
  taskId?: string;
  /** Client-chosen message id (defaults to a fresh one). */
  messageId?: string;
  /**
   * Hand back the task at once (default true): progress arrives on the feed
   * or a task stream, and a display client must not freeze for a whole turn.
   */
  returnImmediately?: boolean;
}

/** Who `authSessionsRevoke` ends. */
export type RevokeTarget = { sid: string } | { name: string } | { all: true };

export class AgentdClient {
  readonly ep: Endpoint;
  /** What the card this client was opened from declares. */
  readonly caps: Capabilities;
  private readonly a2a: A2aClient;

  constructor(ep: Endpoint, caps: Capabilities) {
    this.ep = ep;
    this.caps = caps;
    this.a2a = new A2aClient(ep);
  }

  /** The card offers `op` to this caller (a built-in op or a workflow command). */
  offers(op: string): boolean {
    const c = this.caps.command;
    return c !== null && (c.ops.has(op) || c.commands.some((x) => x.op === op));
  }

  /**
   * task-annotations/v1 rides every call that returns tasks, when declared —
   * it is what carries a task's link, principal and gate schema.
   */
  private annotated(): string[] {
    return this.caps.annotations ? [TASK_ANNOTATIONS_EXTENSION] : [];
  }

  /** Options for a core task call: annotations only, depended on by nothing. */
  private core(signal?: AbortSignal): CallOptions {
    return { exts: this.annotated(), signal };
  }

  /**
   * Whether a reply may be read for annotations: yes unless the server
   * echoed `A2A-Extensions` and left the extension out of it. An absent echo
   * is advisory (it is a SHOULD), and then the card governs.
   */
  private annotationsIn(echo: string[] | null): NormalizeOptions {
    return { annotations: this.caps.annotations && (echo === null || echo.includes(TASK_ANNOTATIONS_EXTENSION)) };
  }

  // ---- conversation ------------------------------------------------------

  /**
   * Send a natural-language message. An agent may answer with a task (the
   * usual case) or directly with a message (`reply`).
   */
  async send(
    text: string,
    opts: SendOptions = {},
  ): Promise<{ task: TaskView | null; reply?: Json; messageId: string }> {
    const message = userMessage(text, opts);
    const messageId = message.messageId as string;
    const r = await this.a2a.sendMessage(message, { returnImmediately: opts.returnImmediately ?? true }, this.core());
    const task = r.task ? normalizeTask(r.task, this.annotationsIn(r.echo)) : null;
    return r.message ? { task, reply: r.message, messageId } : { task, messageId };
  }

  /**
   * Send one command/v2 op. Refused locally — no request is made — when the
   * card declares no command extension or does not offer `op`. The call
   * activates command/v2 and REQUIRES it: a present echo without it means the
   * agent did not run this as a command, and its answer is not trusted.
   */
  async command(
    op: string,
    args: Obj = {},
    o: { contextId?: string; returnImmediately?: boolean; signal?: AbortSignal } = {},
  ): Promise<CommandReply> {
    if (this.caps.command === null) {
      throw new ClientError('extension-not-declared', `${op}: this agent does not declare ${COMMAND_EXTENSION}`);
    }
    if (!this.offers(op)) {
      throw new ClientError('op-not-offered', `${op} is not offered by this agent (its card does not list the op)`);
    }
    const message = commandMessage(op, args, { contextId: o.contextId });
    const r = await this.a2a.sendMessage(
      message,
      { returnImmediately: o.returnImmediately ?? false },
      { exts: [COMMAND_EXTENSION, ...this.annotated()], require: [COMMAND_EXTENSION], signal: o.signal },
    );
    return commandReply(r.task ? { task: r.task } : { message: r.message ?? null }, this.annotationsIn(r.echo));
  }

  /** A command's structured answer: the reply's DataPart, or null. */
  private async data(op: string, args: Obj = {}): Promise<Json> {
    return (await this.command(op, args)).data ?? null;
  }

  // ---- tasks -------------------------------------------------------------

  async getTask(id: string, historyLength?: number): Promise<TaskView | null> {
    const r = await this.a2a.getTask(id, historyLength, this.core());
    return normalizeTask(r.result, this.annotationsIn(r.echo));
  }

  /** Every page of ListTasks (see {@link A2aClient.listTasks}). */
  async listTasks(q: ListQuery = {}): Promise<{ tasks: TaskView[]; truncated: boolean }> {
    const r = await this.a2a.listTasks(q, this.core());
    const tasks = r.tasks.map(({ task, echo }) => normalizeTask(task, this.annotationsIn(echo)));
    return { tasks: tasks.filter((t): t is TaskView => t !== null), truncated: r.truncated };
  }

  async cancelTask(id: string): Promise<TaskView | null> {
    const r = await this.a2a.cancelTask(id, this.core());
    return normalizeTask(r.result, this.annotationsIn(r.echo));
  }

  // ---- reads (Message replies; no task is created) -----------------------

  /** The `status` document (the bootstrap read). */
  async status(): Promise<Json> {
    return this.data(OPS.status);
  }

  /** The effective config (operator). */
  async config(): Promise<Json> {
    return this.data(OPS.config);
  }

  async workflowStatus(run?: string): Promise<Json> {
    return this.data(OPS.workflowStatus, run ? { run } : {});
  }

  /** A subagent's status (served without introspection, unlike `.get`). */
  async subagentStatus(handle: string): Promise<Json> {
    return this.data(OPS.subagentStatus, { handle });
  }

  /** A conversation's working plan. */
  async planGet(id?: string): Promise<Json> {
    return this.data(OPS.planGet, id ? { id } : {});
  }

  // Introspection: served only while the daemon has it on.

  /** A conversation's stored transcript. */
  async conversationGet(id: string, limit?: number): Promise<Json> {
    return this.data(OPS.conversationGet, limit ? { id, limit } : { id });
  }

  /** A run with per-step detail. */
  async runGet(run: string): Promise<Json> {
    return this.data(OPS.runGet, { run });
  }

  /** One subagent's detail (instruction, result, attempts…). */
  async subagentGet(handle: string): Promise<Json> {
    return this.data(OPS.subagentGet, { handle });
  }

  /** The live log ring, cursored. */
  async debugEvents(after = 0, limit = 200, level?: string): Promise<Json> {
    const args: Obj = { after, limit };
    if (level) args.level = level;
    return this.data(OPS.debugEvents, args);
  }

  // ---- work (Task replies) -----------------------------------------------

  /**
   * Run a workflow. It hands back the WORKING task at once: a run can take
   * minutes, and its progress arrives on the feed or a task stream.
   */
  async workflowRun(workflow: string, inputs?: Json): Promise<{ task: TaskView | null }> {
    const r = await this.command(OPS.workflowRun, inputs !== undefined ? { workflow, inputs } : { workflow }, {
      returnImmediately: true,
    });
    return { task: r.kind === 'task' ? r.task : null };
  }

  async workflowCancel(run: string): Promise<Json> {
    return this.data(OPS.workflowCancel, { run });
  }

  /** Fire a named workflow signal (resumes `wait: {on: signal}` steps). */
  async signal(name: string, payload?: Json, run?: string): Promise<Json> {
    const args: Obj = { name };
    if (payload !== undefined) args.payload = payload;
    if (run) args.run = run;
    return this.data(OPS.workflowSignal, args);
  }

  /** Inject a message into a WARM subagent. */
  async subagentSend(handle: string, message: string): Promise<Json> {
    return this.data(OPS.subagentSend, { handle, message });
  }

  /**
   * Stop a subagent.
   *
   * The supervisor owns the process group, so this is a real kill rather than
   * a request the child can decline — which is the point of being able to do
   * it from a UI at all.
   */
  async subagentKill(handle: string, reason?: string): Promise<Json> {
    return this.data(OPS.subagentKill, reason ? { handle, reason } : { handle });
  }

  // ---- admin (operator) --------------------------------------------------

  async drain(reason = 'requested from the interface'): Promise<Json> {
    return this.data(OPS.adminDrain, { reason });
  }

  /** Pause one run, or (no arg) hold the whole instance. Reversible. */
  async pause(run?: string): Promise<Json> {
    return this.data(OPS.adminPause, run ? { run } : {});
  }

  /** Resume a paused run / the instance. */
  async resume(run?: string): Promise<Json> {
    return this.data(OPS.adminResume, run ? { run } : {});
  }

  /** Cancel one run by id. */
  async cancelRun(run: string, reason?: string): Promise<Json> {
    return this.data(OPS.adminCancel, reason ? { run, reason } : { run });
  }

  /**
   * Runtime-set one of the paths the card lists as settable (until the next
   * reload). Answers `{path, value}` as the daemon parsed it.
   */
  async adminSet(path: string, value: Json): Promise<Json> {
    return this.data(OPS.adminSet, { path, value });
  }

  // ---- sign-in administration (operator) ---------------------------------

  /** Device sign-ins waiting for an operator's decision. */
  async authDevicePending(): Promise<Json> {
    return this.data(OPS.authDevicePending);
  }

  /**
   * Approve a device sign-in as `name`. The name is required: it becomes the
   * principal the device acts as (`user:<name>`), and every session approved
   * under one name shares that principal's tasks — so it is chosen on
   * purpose, never defaulted. `scope` raises it to another configured scope.
   */
  async authDeviceApprove(userCode: string, name: string, scope?: string): Promise<Json> {
    if (name.trim() === '') {
      throw new ClientError('invalid-argument', `approving ${userCode} needs the name it signs in as`);
    }
    const args: Obj = { user_code: userCode, as: name };
    if (scope !== undefined) args.scope = scope;
    return this.data(OPS.authDeviceApprove, args);
  }

  /** Refuse one pending device sign-in, or all of them. */
  async authDeviceDeny(target: { userCode: string } | { all: true }): Promise<Json> {
    return this.data(OPS.authDeviceDeny, 'all' in target ? { all: true } : { user_code: target.userCode });
  }

  /** The live browser/terminal sessions (device and launch). */
  async authSessions(): Promise<Json> {
    return this.data(OPS.authSessions);
  }

  /** End one session by id, every session approved under a name, or all. */
  async authSessionsRevoke(target: RevokeTarget): Promise<Json> {
    const args: Obj = 'sid' in target ? { sid: target.sid } : 'name' in target ? { name: target.name } : { all: true };
    return this.data(OPS.authSessionsRevoke, args);
  }

  // ---- streams -----------------------------------------------------------

  /**
   * Attach to the events/v1 observation feed. `onHello`/`onEvent` fire as
   * frames land. Resolves with the goodbye — the cursor to resume from and
   * why the server ended the stream — or `undefined` when the stream ended
   * without one. Rejects on a transport or server error (the transport
   * throws both), and locally, with no request, when the card declares no
   * feed.
   */
  async subscribeEvents(
    fromSeq: number,
    onHello: (h: FeedHello) => void,
    onEvent: (e: FeedEvent) => void,
    signal?: AbortSignal,
  ): Promise<FeedGoodbye | undefined> {
    if (this.caps.events === null) {
      throw new ClientError('extension-not-declared', `this agent does not declare ${EVENTS_EXTENSION}`);
    }
    let goodbye: FeedGoodbye | undefined;
    await rpcStream(
      this.ep,
      EVENTS_METHOD,
      { fromSeq },
      (result) => {
        const r = obj(result);
        if (!r) return;
        if (r.hello) onHello(r.hello as unknown as FeedHello);
        else if (r.event) onEvent(r.event as unknown as FeedEvent);
        else if (r.goodbye) {
          const g = obj(r.goodbye);
          if (typeof g?.seq === 'number') goodbye = { seq: g.seq, reason: str(g.reason) ?? '' };
        }
      },
      { signal, exts: [EVENTS_EXTENSION, ...this.annotated()], require: [EVENTS_EXTENSION] },
    );
    return goodbye;
  }

  /**
   * Attach to one task's stream (status/artifact frames until it stops).
   * Rejects on an error — a terminal or unknown task is an answer, not an
   * empty stream — and locally when the card does not declare streaming.
   * `lastEventId` resumes after that SSE id.
   */
  async subscribeTask(
    id: string,
    onFrame: (frame: Json, sseId?: string) => void,
    signal?: AbortSignal,
    lastEventId?: string,
  ): Promise<void> {
    if (!this.caps.streaming) {
      throw new ClientError('op-not-offered', 'this agent does not stream (its card leaves capabilities.streaming off)');
    }
    const o = this.core(signal);
    if (lastEventId !== undefined) o.lastEventId = lastEventId;
    await this.a2a.subscribeToTask(id, onFrame, o);
  }
}
