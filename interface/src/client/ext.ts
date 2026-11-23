// SPDX-License-Identifier: AGPL-3.0-only
/**
 * agentd's extension vocabulary, in one place: every extension URI, the
 * extension methods, the command envelope key and the op names the client
 * reaches for. This is their only home — a source scan in client.test.mjs
 * fails the build when another file under src/ spells one.
 *
 * Everything here is an A2A extension (or, for {@link UNIX_BINDING}, a custom
 * binding) that agentd DECLARES on its card. The client uses one only when the
 * card declares that exact URI.
 *
 * The specs live at the URIs themselves (https://agentd.dev/a2a/…); each must
 * equal the daemon's constant of the same name in `runtime/surface/ext.rs`.
 */

/**
 * command — the workload ops (start or signal a workflow, talk to or stop a
 * subagent), sent as one DataPart on SendMessage. A profile extension: it
 * adds no method, and a client that ignores it can still converse.
 * Spec: https://agentd.dev/a2a/ext/command
 */
export const COMMAND_EXTENSION = 'https://agentd.dev/a2a/ext/command';

/**
 * operator — the control plane and the reads, as JSON-RPC methods of their
 * own: `agentd.operator/<op>` ({@link operatorMethod}). A method extension:
 * a method is answered only while the request activates it.
 * Spec: https://agentd.dev/a2a/ext/operator
 */
export const OPERATOR_EXTENSION = 'https://agentd.dev/a2a/ext/operator';

/** What every operator method's name starts with. */
export const OPERATOR_METHOD_PREFIX = 'agentd.operator/';

/** The method that calls operator op `op`. */
export function operatorMethod(op: string): string {
  return `${OPERATOR_METHOD_PREFIX}${op}`;
}

/**
 * events — the instance-wide observation feed, a method extension
 * carrying {@link EVENTS_METHOD}. Declared only when the daemon runs with
 * the feed on. Spec: https://agentd.dev/a2a/ext/events
 */
export const EVENTS_EXTENSION = 'https://agentd.dev/a2a/ext/events';

/**
 * task-annotations — agentd's own facts about a task (what it is linked
 * to, its principal, its status history, a gate's answer schema), carried
 * under `metadata[<this URI>]` only while the extension is active.
 * Spec: https://agentd.dev/a2a/ext/task-annotations
 */
export const TASK_ANNOTATIONS_EXTENSION = 'https://agentd.dev/a2a/ext/task-annotations';

/**
 * The extensions this client implements. A card that marks any OTHER
 * extension `required` is refused: the client cannot honour what it does not
 * speak, and a required extension is a promise that it will.
 */
export const CLIENT_EXTENSIONS: ReadonlySet<string> = new Set([
  COMMAND_EXTENSION,
  OPERATOR_EXTENSION,
  EVENTS_EXTENSION,
  TASK_ANNOTATIONS_EXTENSION,
]);

/**
 * The protocol binding a unix-socket interface declares: JSON-RPC over a path
 * no browser or `fetch` can dial. Known here only so interface selection can
 * name it when it refuses it. Spec: https://agentd.dev/a2a/binding/jsonrpc-unix
 */
export const UNIX_BINDING = 'https://agentd.dev/a2a/binding/jsonrpc-unix';

/**
 * The feed's method, declared by {@link EVENTS_EXTENSION}. Namespaced so it
 * can never collide with a method the specification defines later — as the
 * operator methods are ({@link operatorMethod}).
 */
export const EVENTS_METHOD = 'agentd.events/SubscribeToEvents';

/** The key a command DataPart carries its `{op, …args}` under. */
export const COMMAND_DATA_KEY = 'agentd';

/**
 * Every op the client sends, keyed by the `AgentdClient` method that sends
 * it — a command op as a DataPart, an operator op ({@link OPERATOR_OPS}) as
 * its method. Whether an agent SERVES one is its card's business
 * (`params.ops`); this is only the spelling.
 */
export const OPS = Object.freeze({
  status: 'status',
  config: 'config',
  workflowRun: 'workflow.run',
  workflowCancel: 'workflow.cancel',
  workflowSignal: 'workflow.signal',
  subagentSend: 'subagent.send',
  subagentKill: 'subagent.kill',
  subagentStatus: 'subagent.status',
  subagentLog: 'subagent.log',
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
  companyStatus: 'company.status',
  companyUserIdReset: 'company.user_id.reset',
  companySignIn: 'company.sign_in',
  companySignOut: 'company.sign_out',
  delegationEnrol: 'delegation.enrol',
  delegationList: 'delegation.list',
  delegationApprove: 'delegation.approve',
  delegationDeny: 'delegation.deny',
  delegationRevoke: 'delegation.revoke',
  delegationForget: 'delegation.forget',
  delegationAudit: 'delegation.audit',
  paymentAllowances: 'payment.allowances',
} as const);

/** The ops the command extension carries: the workload. */
export const COMMAND_OPS: ReadonlySet<string> = new Set([
  OPS.workflowRun,
  OPS.workflowSignal,
  OPS.subagentSend,
  OPS.subagentKill,
]);

/**
 * The ops the operator extension carries, each as its method: every op of
 * {@link OPS} that is no command op.
 */
export const OPERATOR_OPS: ReadonlySet<string> = new Set(
  Object.values(OPS).filter((op) => !COMMAND_OPS.has(op)),
);

/**
 * The slash commands that are ops, keyed by the word a person
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
  enrol: OPS.delegationEnrol,
  consent: OPS.delegationList,
  allowances: OPS.paymentAllowances,
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
