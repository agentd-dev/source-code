// SPDX-License-Identifier: AGPL-3.0-only
/**
 * agentd's extension vocabulary, in one place: every extension URI, the one
 * extension method, the command envelope key and the op names the client
 * reaches for. This is their only home — a URI or op that changes version
 * changes here and nowhere else, and a source scan in client.test.mjs fails
 * the build when another file under src/ spells one.
 *
 * Everything here is an A2A extension (or, for {@link UNIX_BINDING}, a custom
 * binding) that agentd DECLARES on its card. The client uses one only when the
 * card declares that exact URI: the version is part of the URI, and there is
 * no fallback from one version to another.
 *
 * The specs live at the URIs themselves (https://agentd.dev/a2a/…); each must
 * equal the daemon's constant of the same name in `runtime/surface/ext.rs`.
 */

/**
 * command/v2 — structured operations sent as one DataPart on SendMessage.
 * A profile extension: it adds no method, and a client that ignores it can
 * still converse. Spec: https://agentd.dev/a2a/ext/command/v2
 */
export const COMMAND_EXTENSION = 'https://agentd.dev/a2a/ext/command/v2';

/**
 * events/v1 — the instance-wide observation feed, a method extension
 * carrying {@link EVENTS_METHOD}. Declared only when the daemon runs with
 * the feed on. Spec: https://agentd.dev/a2a/ext/events/v1
 */
export const EVENTS_EXTENSION = 'https://agentd.dev/a2a/ext/events/v1';

/**
 * task-annotations/v1 — agentd's own facts about a task (what it is linked
 * to, its principal, its status history, a gate's answer schema), carried
 * under `metadata[<this URI>]` only while the extension is active.
 * Spec: https://agentd.dev/a2a/ext/task-annotations/v1
 */
export const TASK_ANNOTATIONS_EXTENSION = 'https://agentd.dev/a2a/ext/task-annotations/v1';

/**
 * The extensions this client implements. A card that marks any OTHER
 * extension `required` is refused: the client cannot honour what it does not
 * speak, and a required extension is a promise that it will.
 */
export const CLIENT_EXTENSIONS: ReadonlySet<string> = new Set([
  COMMAND_EXTENSION,
  EVENTS_EXTENSION,
  TASK_ANNOTATIONS_EXTENSION,
]);

/**
 * The protocol binding a unix-socket interface declares: JSON-RPC over a path
 * no browser or `fetch` can dial. Known here only so interface selection can
 * name it when it refuses it. Spec: https://agentd.dev/a2a/binding/jsonrpc-unix/v1
 */
export const UNIX_BINDING = 'https://agentd.dev/a2a/binding/jsonrpc-unix/v1';

/**
 * The one method outside A2A core the client may call, declared by
 * {@link EVENTS_EXTENSION}. Namespaced so it can never collide with a method
 * the specification defines later.
 */
export const EVENTS_METHOD = 'agentd.events/SubscribeToEvents';

/** The key a command DataPart carries its `{op, …args}` under. */
export const COMMAND_DATA_KEY = 'agentd';

/**
 * Every command/v2 op the client sends, keyed by the `AgentdClient` method
 * that sends it. Whether an agent SERVES one is its card's business
 * (`params.ops`); this is only the spelling.
 */
export const OPS = Object.freeze({
  status: 'status',
  config: 'config',
  workflowRun: 'workflow.run',
  workflowStatus: 'workflow.status',
  workflowCancel: 'workflow.cancel',
  workflowSignal: 'workflow.signal',
  subagentSend: 'subagent.send',
  subagentKill: 'subagent.kill',
  subagentStatus: 'subagent.status',
  planGet: 'plan.get',
  conversationGet: 'conversation.get',
  runGet: 'run.get',
  subagentGet: 'subagent.get',
  debugEvents: 'debug.events',
  adminDrain: 'admin.drain',
  adminPause: 'admin.pause',
  adminResume: 'admin.resume',
  adminCancel: 'admin.cancel',
  adminSet: 'admin.set',
  authDevicePending: 'auth.device.pending',
  authDeviceApprove: 'auth.device.approve',
  authDeviceDeny: 'auth.device.deny',
  authSessions: 'auth.sessions',
  authSessionsRevoke: 'auth.sessions.revoke',
} as const);

/**
 * The slash commands that are command/v2 ops, keyed by the word a person
 * types. A slash command is offered only while the card lists its op.
 */
export const SLASH_OPS: Readonly<Record<string, string>> = Object.freeze({
  status: OPS.status,
  config: OPS.config,
  set: OPS.adminSet,
  workflow: OPS.workflowRun,
  signal: OPS.workflowSignal,
  send: OPS.subagentSend,
  pause: OPS.adminPause,
  resume: OPS.adminResume,
  plan: OPS.planGet,
  drain: OPS.adminDrain,
  approve: OPS.authDeviceApprove,
  deny: OPS.authDeviceDeny,
  devices: OPS.authDevicePending,
  sessions: OPS.authSessions,
  revoke: OPS.authSessionsRevoke,
});

/**
 * The read ops behind the debug views. They back client methods, not slash
 * commands, and the daemon serves them only while `a2a.introspection.enabled`
 * is on — so the card listing any of them is what turns those views on.
 */
export const INTROSPECTION_OPS: readonly string[] = Object.freeze([
  OPS.conversationGet,
  OPS.runGet,
  OPS.subagentGet,
  OPS.debugEvents,
]);
