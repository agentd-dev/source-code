// SPDX-License-Identifier: AGPL-3.0-only
/**
 * Wire + view types for the display clients (RFC 0032, RFC 0043 §15).
 *
 * agentd is the single source of truth: everything here is a *projection* of
 * daemon state — the client never derives truth of its own.
 */

import type { Session } from './discovery.js';

/** A JSON value (what the wire carries). */
export type Json = null | boolean | number | string | Json[] | { [k: string]: Json };

/** Connection settings for one agentd instance. */
export interface Endpoint {
  /** `http(s)://host:port` — the A2A listener (`a2a.listen`). */
  url: string;
  /** Bearer for a listener with `a2a.bearer` / a `bearer_ref` principal. */
  bearer?: string;
  /**
   * The `tenant` the selected card interface declares. When set it is written
   * into the params of every request; the spec requires exactly that value.
   */
  tenant?: string;
}

/** A2A 1.0 `TaskState` values. */
export type TaskState =
  | 'TASK_STATE_UNSPECIFIED'
  | 'TASK_STATE_SUBMITTED'
  | 'TASK_STATE_WORKING'
  | 'TASK_STATE_INPUT_REQUIRED'
  | 'TASK_STATE_AUTH_REQUIRED'
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

/**
 * The states in which a task has stopped to wait for someone: an answer
 * (input) or a credential (auth). Neither is terminal, and neither is working.
 */
export const INTERRUPTED_STATES: ReadonlySet<TaskState> = new Set([
  'TASK_STATE_INPUT_REQUIRED',
  'TASK_STATE_AUTH_REQUIRED',
]);

/** What a task is attached to (task-annotations/v1 `link`). */
export interface TaskLink {
  kind: 'run' | 'subagent' | 'turn';
  id: string;
}

/** One message of a task's core `history`, flattened. */
export interface HistoryMessage {
  messageId: string;
  role: 'ROLE_USER' | 'ROLE_AGENT';
  /** Every text part, joined with '\n'. */
  text: string;
  /** Every data part's `data`. */
  data: Json[];
}

/**
 * The client's view of one A2A `Task`, flattened once at the edge. The core
 * fields come from the Task itself; agentd's own facts (`link` … `askSchema`)
 * come ONLY from `metadata[task-annotations/v1]`, and only while that
 * extension is active — there is no other key and no flat fallback.
 */
export interface TaskView {
  id: string;
  contextId: string;
  state: TaskState;
  /** Every text part of the status message, joined with '\n'. */
  message?: string;
  /** The artifacts' text parts (the reply, a string result). */
  artifacts: string[];
  /** The artifacts' data parts (a command's structured result). */
  artifactData: Json[];
  /** The core `Task.history`: who said what, including other clients. */
  history: HistoryMessage[];
  link?: TaskLink;
  principal?: string;
  /** The command/v2 op this task runs, when a command opened it. */
  command?: string;
  /** Epoch ms the task was created. */
  created?: number;
  /** Each state the task passed through, with when (epoch ms). */
  statusHistory?: { state: TaskState; ts: number }[];
  /** The shape a gate's answer must take, if the gate declared one. */
  askSchema?: Json;
  /** Epoch ms of `status.timestamp`; 0 when the task carries none. */
  updated: number;
}

/** One events/v1 feed event. */
export interface FeedEvent {
  seq: number;
  ts: number;
  kind: string; // task | run | step | conversation | subagent | child | activity | status | lifecycle | config | audit | auth | *.removed
  data: Json;
}

/** The feed's opening frame. */
export interface FeedHello {
  seq: number;
  resume: number;
  /** The cursor predates the replay window — re-bootstrap via `status`. */
  resync: boolean;
  /** The daemon serves the introspection reads right now. */
  introspection: boolean;
  /** The agentd build, not a protocol number: the version is in the URI. */
  version: string;
}

/** Why the feed ended, and where to resume. */
export interface FeedGoodbye {
  seq: number;
  reason: string;
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
  /**
   * How long the turn took, once it is over.
   *
   * The live row already counts up while the agent works; this is what the
   * count settled at. Without it the number vanishes at exactly the moment it
   * became a fact — and "how long did that take?" is the question people ask
   * about an agent more than any other.
   */
  ms?: number;
}

/**
 * Connection lifecycle of the observation channel. The last three are
 * terminal: retrying cannot help, so the client stops and says why.
 */
export type ConnState =
  | 'connecting'
  | 'ready'
  | 'polling'
  | 'error'
  | 'closed'
  | 'unauthenticated'
  | 'forbidden'
  | 'incompatible';

/** The mirror's full state — everything a renderer needs, nothing it owns. */
export interface MirrorState {
  conn: ConnState;
  /** Last connection error (conn === 'error'). */
  error?: string;
  hello?: FeedHello;
  /** What discovery settled: the card(s), the interface and the capabilities. */
  session?: Session;
  /** The full `status` command document from the last bootstrap. */
  bootstrap?: Json;
  /** The slim live status (feed `status` events). */
  status?: Json;
  draining: boolean;
  paused: boolean;
  tasks: Map<string, TaskView>;
  runs: Map<string, Json>;
  /** Per-run step detail, newest last, keyed by run id.
   *
   * The feed used to carry run-level counts only — "3 done, 1 running" — so a
   * client could see that a run was moving but never WHAT was moving. The
   * daemon now emits a `step` event per transition; this is where they land. */
  steps: Map<string, StepRow[]>;
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

/**
 * A parsed `WWW-Authenticate` challenge (RFC 7235 / RFC 6750 §3). `error` is
 * what tells a revoked or expired session (`invalid_token`) from a request
 * that never carried a credential.
 */
export interface BearerChallenge {
  scheme: string;
  realm?: string;
  error?: string;
  errorDescription?: string;
  /** RFC 9728 protected-resource metadata URL. */
  resourceMetadata?: string;
}

/** What an error carries beyond its code and message. */
export interface RpcErrorExtra {
  /** The JSON-RPC `error.data`, verbatim (the `@type`d google.rpc details). */
  data?: Json;
  /** The HTTP status the error arrived with. */
  status?: number;
  challenge?: BearerChallenge;
  /** `Retry-After`, in milliseconds. */
  retryAfterMs?: number;
}

/**
 * A JSON-RPC error surfaced to the caller. An HTTP error with no JSON-RPC
 * body keeps `code = -status`.
 */
export class RpcError extends Error {
  code: number;
  data?: Json;
  status?: number;
  challenge?: BearerChallenge;
  retryAfterMs?: number;
  constructor(code: number, message: string, extra: RpcErrorExtra = {}) {
    super(message);
    this.code = code;
    this.name = 'RpcError';
    if (extra.data !== undefined) this.data = extra.data;
    if (extra.status !== undefined) this.status = extra.status;
    if (extra.challenge !== undefined) this.challenge = extra.challenge;
    if (extra.retryAfterMs !== undefined) this.retryAfterMs = extra.retryAfterMs;
  }
}

/**
 * A failure this client decided on its own, without (or before) a JSON-RPC
 * error from the agent: discovery refusals, extension checks, and replies it
 * cannot trust.
 */
export type ClientErrorKind =
  | 'discovery'
  | 'no-interface'
  | 'cross-origin'
  | 'required-extension'
  | 'extension-not-declared'
  | 'extension-not-activated'
  | 'op-not-offered'
  /** A call the client refuses to make as asked: a required argument is missing. */
  | 'invalid-argument'
  | 'invalid-response'
  | 'unsupported-scheme'
  /** A credential would travel in the clear: plain http to a non-loopback host. */
  | 'insecure-endpoint'
  /** The feed ended with goodbye `revoked`: the session is gone. */
  | 'session-revoked'
  /** The credential's own expiry passed (there is no refresh token). */
  | 'session-expired';

/**
 * C0 and C1 control characters, DEL included. Text that reaches a terminal
 * from a place someone else writes — a card, a memory value, an error message
 * — must not carry them: a terminal obeys what it is sent, and an ESC
 * sequence could retitle the window, set the clipboard (OSC 52) or dress a
 * link as another (OSC 8).
 */
export const CONTROL = /[\u0000-\u001f\u007f-\u009f]+/g;

/**
 * `s` as inert text for an error or warning: every control character spelled
 * as its `\uXXXX` escape, so what a hostile value tried is visible and does
 * nothing.
 */
export function inert(s: string): string {
  return s.replace(CONTROL, (run) => [...run].map((c) => `\\u${c.charCodeAt(0).toString(16).padStart(4, '0')}`).join(''));
}

export class ClientError extends Error {
  kind: ClientErrorKind;
  constructor(kind: ClientErrorKind, message: string) {
    super(message);
    this.kind = kind;
    this.name = 'ClientError';
  }
}

// The JSON-RPC and A2A error codes the client acts on (A2A 1.0 §5.4; the
// -314xx pair is agentd's HTTP-status mirror, outside the reserved range).
export const PARSE_ERROR = -32700;
export const INVALID_REQUEST = -32600;
export const METHOD_NOT_FOUND = -32601;
export const INVALID_PARAMS = -32602;
export const INTERNAL_ERROR = -32603;
export const TASK_NOT_FOUND = -32001;
/** The server's "this surface is off" code. */
export const UNSUPPORTED_OPERATION = -32004;
/** The agent declares an extended card but has none configured (A2A 1.0). */
export const EXTENDED_CARD_NOT_CONFIGURED = -32007;
export const CONTENT_TYPE_NOT_SUPPORTED = -32005;
export const EXTENSION_SUPPORT_REQUIRED = -32008;
export const VERSION_NOT_SUPPORTED = -32009;
export const UNAUTHENTICATED = -31401;
export const PERMISSION_DENIED = -31403;

/** One step transition, as the observation feed reports it. */
export type StepRow = {
  step: string;
  kind?: string;
  /** `start` then `done` — a step is running while it has no terminal status. */
  phase: 'start' | 'done';
  status?: string;
  attempt?: number;
  tokens?: number;
  err?: string;
  at: number;
  /** When the step started, so a finished row can show how long it took. */
  startedAt?: number;
  /** Milliseconds from start to done — present once the step finished. */
  ms?: number;
};
