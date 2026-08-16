// SPDX-License-Identifier: Apache-2.0
/**
 * Wire + view types for the agentd interface surface (RFC 0032).
 *
 * agentd is the single source of truth: everything here is a *projection* of
 * daemon state — the client never derives truth of its own.
 */

/** A JSON value (what the wire carries). */
export type Json = null | boolean | number | string | Json[] | { [k: string]: Json };

/** Connection settings for one agentd instance. */
export interface Endpoint {
  /** `http(s)://host:port` — the A2A listener (`a2a.listen`). */
  url: string;
  /** Bearer for a listener with `a2a.bearer` / a `bearer_ref` principal. */
  bearer?: string;
}

/** A2A `Task.status.state` values (RFC 0029 §4). */
export type TaskState =
  | 'TASK_STATE_SUBMITTED'
  | 'TASK_STATE_WORKING'
  | 'TASK_STATE_INPUT_REQUIRED'
  | 'TASK_STATE_COMPLETED'
  | 'TASK_STATE_FAILED'
  | 'TASK_STATE_CANCELED'
  | 'TASK_STATE_REJECTED';

export const TERMINAL_STATES: ReadonlySet<TaskState> = new Set([
  'TASK_STATE_COMPLETED',
  'TASK_STATE_FAILED',
  'TASK_STATE_CANCELED',
  'TASK_STATE_REJECTED',
]);

/** What a task is attached to. */
export type TaskLink =
  | { run: { id: string } }
  | { subagent: { handle: string } }
  | { turn: { ctx: string } };

/**
 * The client's normalized task view. The wire has TWO shapes — the full task
 * (nested `status.state`, from GetTask/SendMessage/feed) and the flat summary
 * (top-level `state`, from ListTasks) — normalized here once, at the edge.
 */
export interface TaskView {
  id: string;
  contextId: string;
  state: TaskState;
  /** The status message (input-required prompts, terminal explanations). */
  message?: string;
  /** The terminal artifact texts (the reply / command result). */
  artifacts: string[];
  link?: TaskLink;
  principal?: string;
  updated: number;
  history?: Json[];
}

/** One `SubscribeToEvents` feed event (RFC 0032 §4). */
export interface FeedEvent {
  seq: number;
  ts: number;
  kind: string; // task | task.removed | message | command | run | conversation | subagent | child | status | lifecycle | audit | *.removed
  data: Json;
}

/** The feed's opening frame. */
export interface FeedHello {
  seq: number;
  resume: number;
  /** The cursor predates the replay window — re-bootstrap via `status`. */
  resync: boolean;
  debug: boolean;
  version: string;
}

/** `interface.info` (RFC 0032 §5). */
export interface InterfaceInfo {
  enabled: boolean;
  debug: boolean;
  version: string;
  instance: string;
  model?: string;
  protocol: number;
  feed: { ring: number; method: string };
  ops: string[];
  /** The daemon-decided chrome layout (RFC 0032 §12). */
  display?: { top: string[]; bottom: string[] };
  pairing?: { enabled: boolean };
}

/** `pairing.code` (operator; RFC 0032 §13). */
export interface PairingCode {
  code: string;
  expires_in_ms: number;
  window_ms: number;
  role: string;
  sessions: number;
  url?: string;
}

/** `Pair` result: the minted session credential. */
export interface PairedSession {
  token: string;
  expiresAt: number;
  role: string;
  agent: { name: string; instance: string; version: string };
}

/**
 * What a working unit is doing right now (RFC 0032 §17). Elapsed time is NOT
 * streamed — tick it locally from `started_ms`.
 */
export interface Activity {
  /** The child node id (the record's key). */
  id: string;
  /** The A2A task this unit answers, when it has one. */
  task?: string;
  ctx?: string;
  phase: 'thinking' | 'tool' | 'waiting';
  /** The tool executing (phase `tool`) or the wait it parked on (`waiting`). */
  tool?: string;
  round: number;
  tokens_in: number;
  tokens_out: number;
  started_ms: number;
  updated_ms: number;
}

/** One entry of the rendered conversation transcript (a client-side view). */
export interface TranscriptEntry {
  /** Stable key: the messageId (user) or taskId (agent/command). */
  key: string;
  ctx: string;
  ts: number;
  kind: 'user' | 'agent' | 'command' | 'info' | 'error';
  text: string;
  taskId?: string;
  principal?: string;
  /** Still working server-side (renders as the live row). */
  pending?: boolean;
  /** The task stopped at input-required — answer it to continue. */
  inputRequired?: boolean;
}

/** Connection lifecycle of the observation channel. */
export type ConnState = 'connecting' | 'ready' | 'polling' | 'error' | 'closed';

/** The mirror's full state — everything a renderer needs, nothing it owns. */
export interface MirrorState {
  conn: ConnState;
  /** Last connection error (conn === 'error'). */
  error?: string;
  hello?: FeedHello;
  info?: InterfaceInfo;
  card?: Json;
  /** The full `status` command document from the last bootstrap. */
  bootstrap?: Json;
  /** The slim live status (feed `status` events). */
  status?: Json;
  draining: boolean;
  paused: boolean;
  tasks: Map<string, TaskView>;
  runs: Map<string, Json>;
  conversations: Map<string, Json>;
  subagents: Map<string, Json>;
  children: Map<string, Json>;
  /** Live per-unit activity (RFC 0032 §17), keyed by unit id. */
  activity: Map<string, Activity>;
  transcript: TranscriptEntry[];
  /** Bounded feed tail for the debug pane. */
  feedLog: FeedEvent[];
  /** The resume cursor (highest feed seq seen). */
  lastSeq: number;
}

/** A JSON-RPC error surfaced to the caller. */
export class RpcError extends Error {
  code: number;
  constructor(code: number, message: string) {
    super(message);
    this.code = code;
    this.name = 'RpcError';
  }
}

/** The server's "this surface is off" code (UNSUPPORTED_OPERATION). */
export const UNSUPPORTED_OPERATION = -32004;
