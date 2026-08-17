// SPDX-License-Identifier: Apache-2.0
/**
 * `AgentdClient` — every operation a display client can ask of agentd, and
 * nothing else. Thin by design (RFC 0032): the daemon hosts state, tools and
 * secrets; this class only forwards intent and reads projections.
 */

import {
  Endpoint,
  FeedEvent,
  FeedHello,
  InterfaceInfo,
  Json,
  PairedSession,
  PairingCode,
  RpcError,
  TaskState,
  TaskView,
} from './types.js';
import { rpc, rpcStream, StreamFrame } from './wire.js';

/** Normalize either wire task shape into the client view. */
export function normalizeTask(t: Json): TaskView | null {
  if (t === null || typeof t !== 'object' || Array.isArray(t)) return null;
  const o = t as { [k: string]: Json };
  const id = typeof o.id === 'string' ? o.id : '';
  if (!id) return null;
  const status = (o.status ?? null) as { [k: string]: Json } | null;
  const state = ((status?.state as string) ?? (o.state as string) ?? 'TASK_STATE_SUBMITTED') as TaskState;
  const msgParts = (status?.message as { [k: string]: Json } | undefined)?.parts;
  const message = Array.isArray(msgParts)
    ? ((msgParts[0] as { [k: string]: Json } | undefined)?.text as string | undefined)
    : undefined;
  const artifacts: string[] = [];
  if (Array.isArray(o.artifacts)) {
    for (const a of o.artifacts) {
      const parts = (a as { [k: string]: Json }).parts;
      if (Array.isArray(parts)) {
        for (const p of parts) {
          const text = (p as { [k: string]: Json }).text;
          if (typeof text === 'string' && text.length > 0) artifacts.push(text);
        }
      }
    }
  }
  return {
    id,
    contextId: (o.contextId as string) ?? '',
    state,
    message,
    artifacts,
    link: (o.link as TaskView['link']) ?? undefined,
    principal: (o.principal as string) ?? undefined,
    updated: (o.updated as number) ?? ((status?.timestamp as number) ?? 0),
    history: Array.isArray(o.history) ? o.history : undefined,
  };
}

/** The message envelope for a natural-language send. */
export interface SendOptions {
  /** Continue this conversation (omit to open a new one). */
  contextId?: string;
  /** Answer this task's input-required gate / continue it. */
  taskId?: string;
  /** Client-chosen message id (defaults to a fresh one). */
  messageId?: string;
  /** Block until terminal (default FALSE here — the feed carries progress). */
  blocking?: boolean;
}

export class AgentdClient {
  readonly ep: Endpoint;
  constructor(ep: Endpoint) {
    this.ep = ep;
  }

  private nextMsg = 1;
  private msgId(): string {
    return `ui-${Date.now().toString(36)}-${this.nextMsg++}`;
  }

  // ---- discovery ---------------------------------------------------------

  /** The public agent card. */
  async agentCard(): Promise<Json> {
    return rpc(this.ep, 'GetAgentCard', {});
  }

  /** `interface.info` — throws UNSUPPORTED_OPERATION when the surface is off. */
  async interfaceInfo(): Promise<InterfaceInfo> {
    const r = (await this.command('interface.info', {})) as { [k: string]: Json };
    return r.interface as unknown as InterfaceInfo;
  }

  // ---- conversation ------------------------------------------------------

  /**
   * Send a natural-language message. Returns the created/continued task
   * immediately (`blocking` defaults to false — watch the feed or the task).
   */
  async send(text: string, opts: SendOptions = {}): Promise<{ task: TaskView | null; messageId: string }> {
    const messageId = opts.messageId ?? this.msgId();
    const message: { [k: string]: Json } = { messageId, parts: [{ text }] };
    if (opts.contextId) message.contextId = opts.contextId;
    if (opts.taskId) message.taskId = opts.taskId;
    const r = (await rpc(this.ep, 'SendMessage', {
      message,
      configuration: { blocking: opts.blocking ?? false },
    })) as { [k: string]: Json };
    return { task: normalizeTask(r.task ?? null), messageId };
  }

  /** Send a command DataPart (`{op, …args}`); returns the raw result. */
  async command(op: string, args: { [k: string]: Json }, contextId?: string): Promise<Json> {
    const message: { [k: string]: Json } = {
      messageId: this.msgId(),
      parts: [{ data: { agentd: { op, ...args } } }],
    };
    if (contextId) message.contextId = contextId;
    return rpc(this.ep, 'SendMessage', { message });
  }

  /**
   * A command whose result rides the task's terminal artifact as JSON text
   * (`status`, `config`, `workflow.status`): parse it back out.
   */
  async commandResult(op: string, args: { [k: string]: Json } = {}): Promise<Json> {
    const r = (await this.command(op, args)) as { [k: string]: Json };
    const task = normalizeTask(r.task ?? null);
    const text = task?.artifacts[0];
    if (typeof text !== 'string') return r;
    try {
      return JSON.parse(text) as Json;
    } catch {
      return text;
    }
  }

  // ---- tasks -------------------------------------------------------------

  async getTask(id: string): Promise<TaskView | null> {
    return normalizeTask(await rpc(this.ep, 'GetTask', { id }));
  }

  async listTasks(): Promise<TaskView[]> {
    const r = (await rpc(this.ep, 'ListTasks', {})) as { [k: string]: Json };
    const tasks = Array.isArray(r.tasks) ? r.tasks : [];
    return tasks.map(normalizeTask).filter((t): t is TaskView => t !== null);
  }

  async cancelTask(id: string): Promise<TaskView | null> {
    return normalizeTask(await rpc(this.ep, 'CancelTask', { id }));
  }

  // ---- the reads (RFC 0032 §5; taskless) ---------------------------------

  /** The full `status` document (the bootstrap read). */
  async status(): Promise<Json> {
    return this.commandResult('status');
  }

  /** The effective config (operator). */
  async config(): Promise<Json> {
    return this.commandResult('config');
  }

  async workflowRun(name: string, inputs?: Json): Promise<{ task: TaskView | null }> {
    const r = (await this.command('workflow.run', inputs !== undefined ? { name, inputs } : { name })) as {
      [k: string]: Json;
    };
    return { task: normalizeTask(r.task ?? null) };
  }

  async workflowStatus(run?: string): Promise<Json> {
    return this.commandResult('workflow.status', run ? { run } : {});
  }

  async workflowCancel(run: string): Promise<Json> {
    return this.command('workflow.cancel', { run });
  }

  /** Debug: a conversation's transcript (message bodies — `interface.debug`). */
  async conversationGet(id: string, limit?: number): Promise<Json> {
    const r = (await this.command('conversation.get', limit ? { id, limit } : { id })) as {
      [k: string]: Json;
    };
    return r.conversation ?? null;
  }

  /** Debug: a run with per-step detail. */
  async runGet(run: string): Promise<Json> {
    const r = (await this.command('run.get', { run })) as { [k: string]: Json };
    return r.run ?? null;
  }

  /** Debug: the live log ring, cursored. */
  async debugEvents(after = 0, limit = 200, level?: string): Promise<Json> {
    const args: { [k: string]: Json } = { after, limit };
    if (level) args.level = level;
    return this.command('debug.events', args);
  }

  /** Debug: one subagent's detail (instruction, result, attempts…). */
  async subagentGet(handle: string): Promise<Json> {
    const r = (await this.command('subagent.get', { handle })) as { [k: string]: Json };
    return r.subagent ?? null;
  }

  /** Operator: runtime-set a whitelisted config knob (RFC 0032 §14). */
  async configSet(path: string, value: Json): Promise<Json> {
    return this.command('config.set', { path, value });
  }

  // ---- pairing (RFC 0032 §13) --------------------------------------------

  /** Operator: the current rotating pairing code (read it out to a joiner). */
  async pairingCode(): Promise<PairingCode> {
    const r = (await this.command('pairing.code', {})) as { [k: string]: Json };
    return r.pairing as unknown as PairingCode;
  }

  /**
   * Exchange a pairing code for a session token (works UNAUTHENTICATED —
   * this IS the login). Use the returned token as the endpoint bearer.
   */
  async pair(code: string): Promise<PairedSession> {
    return (await rpc(this.ep, 'Pair', { code })) as unknown as PairedSession;
  }

  // ---- steering (RFC 0029 §5/§7) -----------------------------------------

  /** Fire a named workflow signal (resumes `wait: {on: signal}` steps). */
  async signal(name: string, payload?: Json, run?: string): Promise<Json> {
    const args: { [k: string]: Json } = { name };
    if (payload !== undefined) args.payload = payload;
    if (run) args.run = run;
    return this.commandResult('workflow.signal', args);
  }

  /** Inject a message into a WARM subagent. */
  async subagentSend(handle: string, message: string): Promise<Json> {
    return this.commandResult('subagent.send', { handle, message });
  }

  /** A conversation's working plan. */
  async planGet(id?: string): Promise<Json> {
    return this.commandResult('plan.get', id ? { id } : {});
  }

  // ---- admin -------------------------------------------------------------

  async drain(reason = 'requested from the interface'): Promise<Json> {
    return rpc(this.ep, 'a2a.drain', { reason });
  }

  /** Pause one run, or (no arg) hold the whole instance. Reversible. */
  async pause(run?: string): Promise<Json> {
    return rpc(this.ep, 'a2a.pause', run ? { run } : {});
  }

  /** Resume a paused run / the instance. */
  async resume(run?: string): Promise<Json> {
    return rpc(this.ep, 'a2a.resume', run ? { run } : {});
  }

  // ---- streams -----------------------------------------------------------

  /**
   * Attach to the global observation feed. `onHello`/`onEvent` fire as frames
   * land; resolves with the goodbye cursor when the server ends the stream
   * (deadline — reconnect with `fromSeq`), rejects on transport errors or a
   * server error frame.
   */
  async subscribeEvents(
    fromSeq: number,
    onHello: (h: FeedHello) => void,
    onEvent: (e: FeedEvent) => void,
    signal?: AbortSignal,
  ): Promise<{ seq: number }> {
    let goodbye: { seq: number } = { seq: fromSeq };
    let errorFrame: { code: number; message: string } | undefined;
    await rpcStream(
      this.ep,
      'SubscribeToEvents',
      { fromSeq },
      (frame: StreamFrame) => {
        if (frame.error) {
          errorFrame = frame.error;
          return;
        }
        const r = frame.result as { [k: string]: Json } | undefined;
        if (!r) return;
        if (r.hello) onHello(r.hello as unknown as FeedHello);
        else if (r.event) onEvent(r.event as unknown as FeedEvent);
        else if (r.goodbye) goodbye = { seq: ((r.goodbye as { [k: string]: Json }).seq as number) ?? fromSeq };
      },
      signal,
    );
    if (errorFrame) throw new RpcError(errorFrame.code, errorFrame.message);
    return goodbye;
  }

  /** Attach to one task's stream (status/artifact frames until terminal). */
  async subscribeTask(
    id: string,
    onFrame: (frame: Json) => void,
    signal?: AbortSignal,
  ): Promise<void> {
    await rpcStream(
      this.ep,
      'SubscribeToTask',
      { id },
      (frame) => {
        if (frame.result) onFrame(frame.result);
      },
      signal,
    );
  }
}
