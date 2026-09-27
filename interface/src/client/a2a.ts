// SPDX-License-Identifier: AGPL-3.0-only
/**
 * A2A 1.0 core, and nothing else: the method names the specification defines,
 * the two message builders every send goes through, and `A2aClient`, which
 * calls the core methods an interactive client needs.
 *
 * This layer never activates an extension on its own. A caller that wants
 * one passes it in `CallOptions` — and the only caller that does is the
 * agentd layer above, which first checks that the card declares it. So what
 * this file sends is what any A2A 1.0 agent understands.
 */

import { ClientError, Endpoint, Json } from './types.js';
import { call, CallOptions, rpcStream } from './wire.js';
import { COMMAND_DATA_KEY, COMMAND_EXTENSION } from './ext.js';

/**
 * The eleven JSON-RPC methods of A2A 1.0 (the `A2AService` rpcs). The client
 * emits a subset of them; every method string it sends is typed against this
 * list, so a name from an older protocol or a private dialect cannot compile.
 */
export const CORE_METHODS = [
  'SendMessage',
  'SendStreamingMessage',
  'GetTask',
  'ListTasks',
  'CancelTask',
  'SubscribeToTask',
  'CreateTaskPushNotificationConfig',
  'GetTaskPushNotificationConfig',
  'ListTaskPushNotificationConfigs',
  'DeleteTaskPushNotificationConfig',
  'GetExtendedAgentCard',
] as const;

export type CoreMethod = (typeof CORE_METHODS)[number];

/** ListTasks asks for the largest page the spec allows (1..100)… */
export const LIST_PAGE_SIZE = 100;
/** …and follows at most this many pages before reporting `truncated`. */
export const LIST_MAX_PAGES = 10;

type Obj = { [k: string]: Json };

let nextMsg = 1;

/** A fresh client-chosen `messageId`. */
export function newMessageId(): string {
  return `ui-${Date.now().toString(36)}-${nextMsg++}`;
}

/** Where a natural-language message goes. */
export interface MessageTarget {
  messageId?: string;
  /** Continue this conversation; ignored when `taskId` is set. */
  contextId?: string;
  /** Answer or continue this task. */
  taskId?: string;
}

/**
 * A natural-language message. `role` is required (the official SDKs refuse a
 * message without one). A `taskId` is never sent with a `contextId`: the
 * server infers the context from the task, and two ids that could disagree
 * are a mismatch waiting to happen — so the pair is impossible here rather
 * than checked somewhere else.
 */
export function userMessage(text: string, to: MessageTarget = {}): Obj {
  const m: Obj = { role: 'ROLE_USER', messageId: to.messageId ?? newMessageId(), parts: [{ text }] };
  if (to.taskId) m.taskId = to.taskId;
  else if (to.contextId) m.contextId = to.contextId;
  return m;
}

/**
 * A command/v2 message: exactly one application/json DataPart holding
 * `{agentd: {op, …args}}`, marked in `Message.extensions` so the peer knows the
 * DataPart is a command and not data for a model to read. A command opens its
 * own task, so it never carries a `taskId`.
 */
export function commandMessage(op: string, args: Obj = {}, to: { messageId?: string; contextId?: string } = {}): Obj {
  const m: Obj = {
    role: 'ROLE_USER',
    messageId: to.messageId ?? newMessageId(),
    parts: [{ data: { [COMMAND_DATA_KEY]: { op, ...args } }, mediaType: 'application/json' }],
    extensions: [COMMAND_EXTENSION],
  };
  if (to.contextId) m.contextId = to.contextId;
  return m;
}

/**
 * `SendMessageConfiguration`. `returnImmediately` is always explicit: in 1.0
 * an absent value means "wait for the task to finish", and a display client
 * that waited by accident would freeze for a whole turn.
 */
export interface SendConfig {
  returnImmediately: boolean;
  historyLength?: number;
}

/** A ListTasks filter; the paging fields are this client's own. */
export interface ListQuery {
  contextId?: string;
  status?: string;
  /** RFC 3339; the server returns tasks updated at or after it. */
  statusTimestampAfter?: string;
  historyLength?: number;
}

function obj(v: Json | undefined): Obj | undefined {
  return v !== null && v !== undefined && typeof v === 'object' && !Array.isArray(v) ? v : undefined;
}

function configOf(c: SendConfig): Obj {
  const out: Obj = { returnImmediately: c.returnImmediately };
  if (c.historyLength !== undefined) out.historyLength = c.historyLength;
  return out;
}

/** The core methods an interactive client calls, over one endpoint. */
export class A2aClient {
  readonly ep: Endpoint;
  constructor(ep: Endpoint) {
    this.ep = ep;
  }

  private call(method: CoreMethod, params: Obj, o?: CallOptions) {
    return call(this.ep, method, params, o);
  }

  /**
   * SendMessage. The result is a `SendMessageResponse`: `{task}` or
   * `{message}`, never both; anything else is not a reply we can use.
   */
  async sendMessage(
    message: Obj,
    config: SendConfig,
    o?: CallOptions,
  ): Promise<{ task?: Obj; message?: Obj; echo: string[] | null }> {
    const { result, echo } = await this.call('SendMessage', { message, configuration: configOf(config) }, o);
    const r = obj(result);
    const task = obj(r?.task);
    const reply = obj(r?.message);
    if (!task && !reply) throw new ClientError('invalid-response', 'SendMessage answered neither a task nor a message');
    return { ...(task ? { task } : {}), ...(reply ? { message: reply } : {}), echo };
  }

  /** SendStreamingMessage: each `StreamResponse` frame goes to `onFrame`. */
  async sendStreamingMessage(
    message: Obj,
    config: SendConfig,
    onFrame: (frame: Json, sseId?: string) => void,
    o?: CallOptions,
  ): Promise<{ echo: string[] | null }> {
    const method: CoreMethod = 'SendStreamingMessage';
    return rpcStream(this.ep, method, { message, configuration: configOf(config) }, onFrame, o);
  }

  async getTask(id: string, historyLength?: number, o?: CallOptions): Promise<Json> {
    const params: Obj = { id };
    if (historyLength !== undefined) params.historyLength = historyLength;
    return (await this.call('GetTask', params, o)).result;
  }

  /**
   * ListTasks, every page. Unasked, a server picks its own page size and
   * leaves artifacts out (`include_artifacts` defaults to false), which hid
   * every gate past page one and every reply text; so each request asks for
   * {@link LIST_PAGE_SIZE} with artifacts, and
   * `nextPageToken` is followed until it runs out — or until `maxPages`, when
   * the listing is reported `truncated` instead of silently cut.
   */
  async listTasks(
    q: ListQuery = {},
    o?: CallOptions,
    maxPages = LIST_MAX_PAGES,
  ): Promise<{ tasks: Json[]; truncated: boolean }> {
    const tasks: Json[] = [];
    let pageToken: string | undefined;
    for (let page = 0; page < maxPages; page++) {
      const params: Obj = { ...q, pageSize: LIST_PAGE_SIZE, includeArtifacts: true };
      if (pageToken !== undefined) params.pageToken = pageToken;
      const r = obj((await this.call('ListTasks', params, o)).result);
      if (!r) throw new ClientError('invalid-response', 'ListTasks answered something other than an object');
      if (Array.isArray(r.tasks)) tasks.push(...r.tasks);
      const next = r.nextPageToken;
      if (typeof next !== 'string' || next === '') return { tasks, truncated: false };
      pageToken = next;
    }
    return { tasks, truncated: true };
  }

  async cancelTask(id: string, o?: CallOptions): Promise<Json> {
    return (await this.call('CancelTask', { id }, o)).result;
  }

  /** SubscribeToTask; `o.lastEventId` resumes after that SSE id. */
  async subscribeToTask(
    id: string,
    onFrame: (frame: Json, sseId?: string) => void,
    o?: CallOptions,
  ): Promise<{ echo: string[] | null }> {
    const method: CoreMethod = 'SubscribeToTask';
    return rpcStream(this.ep, method, { id }, onFrame, o);
  }

  /** GetExtendedAgentCard: the card as the authenticated caller may see it. */
  async getExtendedAgentCard(o?: CallOptions): Promise<Json> {
    return (await this.call('GetExtendedAgentCard', {}, o)).result;
  }
}
