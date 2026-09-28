// SPDX-License-Identifier: AGPL-3.0-only
/**
 * Composer affordances (RFC 0032 §15) — shared by the TUI and the web UI so
 * both surfaces behave identically:
 *
 *   `/`  commands — the SYSTEM set the card backs, plus every workflow this
 *        caller may run as a shortcut (`/deploy` ⇒ `workflow.run deploy`;
 *        system names win). `/set ` completes the paths the card lists as
 *        settable.
 *   `@`  skills — autocompletes the daemon's skill catalogue under the prefix
 *        the daemon publishes; the reference stays INLINE in the text (agentd
 *        preloads a referenced skill).
 *   `#`  targets — a LEADING `#<task-id|context-id>` routes the message: a
 *        task id answers/continues that task (input-required!), anything else
 *        addresses that conversation. Inline `#…` is left as plain text.
 *   `$`  values — `$model`, `$instance`, … interpolate live daemon state into
 *        the text before sending; unknown `$words` are left alone; `$$` ⇒ `$`.
 *
 * Everything offered here is read from what the agent published — its card
 * and its `status` document — never from what an agentd is configured with
 * by default: a client that guesses a daemon's configuration completes to
 * text that does nothing on the daemon it is actually talking to.
 */

import { Activity, inert, Json, MirrorState, TaskView, TERMINAL_STATES } from './types.js';
import { SLASH_OPS } from './ext.js';
import type { AgentdClient, RevokeTarget, SendOptions } from './client.js';

/** One completion the UI can apply. */
export interface Suggestion {
  /** What the user sees. */
  label: string;
  /** The replacement for the triggering token. */
  insert: string;
  /** A short right-hand hint. */
  hint: string;
}

/** A system slash command. */
export interface SystemCommand {
  name: string;
  hint: string;
  /**
   * The command/v2 op the command sends. It is offered only while the card
   * lists that op for this caller; a command without one is the client's own.
   */
  needs?: string;
}

/**
 * The system slash commands in the order help and suggestions list them.
 * Which op each one needs is NOT written here: it is {@link SLASH_OPS}, the
 * one table of slash words that are ops, so a command can never be offered
 * against an op other than the one it sends.
 */
const COMMANDS: ReadonlyArray<[string, string]> = [
  ['help', 'list commands'],
  ['new', 'start a fresh conversation'],
  ['tasks', 'the tasks screen'],
  ['subagents', 'the subagents screen'],
  ['debug', 'the debug screen'],
  ['chat', 'back to the conversation'],
  ['status', 'daemon status summary'],
  ['config', 'show the effective config (or one path)'],
  ['set', 'runtime-set a knob the agent lists as settable: /set <path> <value>'],
  ['workflow', 'run a workflow: /workflow <name>'],
  ['cancel', 'cancel a task (newest if none given)'],
  ['signal', 'fire a workflow signal: /signal <name> [run]'],
  ['send', 'message a warm subagent: /send <handle> <text>'],
  ['pause', 'pause a run, or the whole instance'],
  ['resume', 'resume a run / the instance'],
  ['plan', "a conversation's working plan"],
  ['conversations', 'list conversations (#<id> to address one)'],
  ['devices', 'device sign-ins waiting for approval'],
  ['approve', 'approve a device sign-in: /approve <code> <name> [operator]'],
  ['deny', 'refuse a device sign-in: /deny <code> | all'],
  ['sessions', 'the signed-in sessions'],
  ['revoke', 'end sessions: /revoke <sid> | name <name> | all'],
  ['drain', 'graceful drain'],
  ['layout', "this client's top and bottom edges"],
  ['login', 'sign in'],
  ['logout', 'sign out'],
  ['quit', 'leave the client'],
];

/** The system slash commands, shared by both UIs. */
export const SYSTEM_COMMANDS: ReadonlyArray<SystemCommand> = Object.freeze(
  COMMANDS.map(([name, hint]): SystemCommand =>
    Object.hasOwn(SLASH_OPS, name) ? { name, hint, needs: SLASH_OPS[name] } : { name, hint },
  ),
);

/** The ops the card offers this caller; empty before a session or without command/v2. */
function offeredOps(s: MirrorState): ReadonlySet<string> {
  return s.session?.caps.command?.ops ?? new Set();
}

/**
 * The system commands this caller can use: the client's own, and each op
 * command whose op the card lists. A command the agent would refuse is not
 * suggested, so nobody learns it is refused by trying it.
 */
export function availableCommands(s: MirrorState): SystemCommand[] {
  const ops = offeredOps(s);
  return SYSTEM_COMMANDS.filter((c) => c.needs === undefined || ops.has(c.needs));
}

/** One line of help: the commands {@link availableCommands} allows. */
export function commandHelp(s: MirrorState): string {
  return `${availableCommands(s)
    .map((c) => `/${c.name}`)
    .join(' · ')} — plus @skill, #target, $value in messages`;
}

function field(d: Json | undefined, key: string): Json | undefined {
  return d !== null && d !== undefined && typeof d === 'object' && !Array.isArray(d) ? d[key] : undefined;
}

function text(v: Json | undefined): string | undefined {
  return typeof v === 'string' && v.length > 0 ? v : undefined;
}

/**
 * A fact of the `status` document: the live one (feed `status` events), else
 * the bootstrap read. The live one wins because it is what a reload changed.
 */
function statusFact(s: MirrorState, key: string): Json | undefined {
  return field(s.status, key) ?? field(s.bootstrap, key);
}

/** A string fact of the card this session was opened from (the extended one when read). */
function cardFact(s: MirrorState, key: string): string | undefined {
  const card = s.session?.extended ?? s.session?.card;
  return text(field(card as Json | undefined, key));
}

type Counters = { turns?: number; tokens_in?: number; tokens_out?: number };

/** The usage counters; only an operator's `status` carries them. */
function counters(s: MirrorState): Counters | undefined {
  const c = statusFact(s, 'counters');
  return c !== null && typeof c === 'object' && !Array.isArray(c) ? (c as Counters) : undefined;
}

/**
 * The `$` values a client can interpolate, with their reader. A reader
 * answers `undefined` when the agent did not publish the value to this
 * caller — then the `$word` is left as typed rather than replaced with a
 * made-up `0` or an empty string.
 */
const DOLLAR_VARS: ReadonlyArray<[string, (s: MirrorState) => string | undefined]> = [
  ['name', (s) => cardFact(s, 'name')],
  ['model', (s) => text(statusFact(s, 'model'))],
  ['instance', (s) => text(statusFact(s, 'instance'))],
  ['version', (s) => text(statusFact(s, 'version')) ?? cardFact(s, 'version')],
  ['turns', (s) => (counters(s)?.turns !== undefined ? String(counters(s)?.turns) : undefined)],
  [
    'tokens',
    (s) => {
      const c = counters(s);
      return c?.tokens_in !== undefined || c?.tokens_out !== undefined ? `${c.tokens_in ?? 0}/${c.tokens_out ?? 0}` : undefined;
    },
  ],
  ['tasks', (s) => String(s.tasks.size)],
];

/**
 * The workflows this caller may run: the extended card lists them per caller
 * (skills tagged `workflow`, ids `workflow:<name>` — discovery strips the
 * prefix), and without one the `status` document's `workflows` is the one
 * other place an agent names them.
 */
export function workflowNames(s: MirrorState): string[] {
  const caps = s.session?.caps;
  if (caps?.extendedCard) return [...caps.workflows];
  const wfs = statusFact(s, 'workflows');
  return (Array.isArray(wfs) ? wfs : []).map((w) => text(field(w, 'name'))).filter((n): n is string => n !== undefined);
}

/** The skill names the daemon serves. */
export function skillNames(s: MirrorState): string[] {
  const sk = statusFact(s, 'skills');
  return (Array.isArray(sk) ? sk : []).filter((n): n is string => typeof n === 'string');
}

/**
 * The prefix that makes the daemon preload a skill (`skills.reference_prefix`),
 * as its `status` document publishes it. There is no default here: an agent
 * that publishes no prefix gets no `@` completions, because a guessed prefix
 * completes to text that silently preloads nothing.
 */
export function skillPrefix(s: MirrorState): string | undefined {
  return text(statusFact(s, 'skill_prefix'));
}

/** The trailing trigger token of the input, if any. */
export function triggerToken(input: string): { trigger: '/' | '@' | '#' | '$'; query: string; start: number } | null {
  // `/` only triggers at the very start (it's a command line, not a word).
  if (input.startsWith('/') && !input.includes(' ')) {
    return { trigger: '/', query: input.slice(1), start: 0 };
  }
  const m = /(^|\s)([@#$])([\w./-]*)$/.exec(input);
  if (!m) return null;
  const trigger = m[2] as '@' | '#' | '$';
  return { trigger, query: m[3] ?? '', start: input.length - (m[3]?.length ?? 0) - 1 };
}

/** The path being typed after `/set `, if that is what the input is. */
function setPathToken(input: string): { query: string; start: number } | null {
  const m = /^\/set\s+([^\s]*)$/.exec(input);
  if (!m) return null;
  return { query: m[1], start: input.length - m[1].length };
}

/** Completions for the current input (empty when no trigger / no match). */
export function suggest(input: string, s: MirrorState, max = 6): Suggestion[] {
  const t = triggerToken(input);
  if (!t) {
    const set = setPathToken(input);
    if (!set || !offeredOps(s).has(SLASH_OPS.set)) return [];
    // Only the paths the card says THIS caller may set: the daemon refuses
    // every other one, so offering it would be offering a refusal.
    return (s.session?.caps.command?.settable ?? [])
      .filter((p) => p.startsWith(set.query))
      .map((p) => ({ label: p, insert: `${p} `, hint: 'settable' }))
      .slice(0, max);
  }
  const q = t.query.toLowerCase();
  const starts = (name: string) => name.toLowerCase().startsWith(q);
  switch (t.trigger) {
    case '/': {
      const sys: Suggestion[] = availableCommands(s)
        .filter((c) => starts(c.name))
        .map((c) => ({ label: `/${c.name}`, insert: `/${c.name} `, hint: c.hint }));
      // A shortcut runs `workflow.run`, so it is offered only with that op;
      // a system name wins even while its command is not offered.
      const wf: Suggestion[] = offeredOps(s).has(SLASH_OPS.workflow)
        ? workflowNames(s)
            .filter((n) => starts(n) && !SYSTEM_COMMANDS.some((c) => c.name === n))
            .map((n) => ({ label: `/${n}`, insert: `/${n} `, hint: 'workflow' }))
        : [];
      return [...sys, ...wf].slice(0, max);
    }
    case '@': {
      // Inserts the FULL reference under the published prefix, not a bare
      // `@name`: the daemon preloads a skill only when the text carries its
      // `skills.reference_prefix`, so a bare `@release-notes` would complete
      // to text that silently preloads nothing. Bare `@name` is left free for
      // whatever a deployment means by it conversationally.
      const prefix = skillPrefix(s);
      if (prefix === undefined) return [];
      return skillNames(s)
        .filter(starts)
        .map((n) => ({ label: `${prefix}${n}`, insert: `${prefix}${n} `, hint: 'skill' }))
        .slice(0, max);
    }
    case '#': {
      const tasks = [...s.tasks.values()]
        .sort((a, b) => b.updated - a.updated)
        .filter((tk) => starts(tk.id))
        .map((tk) => ({
          label: `#${tk.id}`,
          insert: `#${tk.id} `,
          hint: tk.state === 'TASK_STATE_INPUT_REQUIRED' ? 'answer this task' : TERMINAL_STATES.has(tk.state) ? 'continue task' : 'task',
        }));
      const ctxs = [...s.conversations.keys()]
        .filter(starts)
        .map((id) => ({ label: `#${id}`, insert: `#${id} `, hint: 'conversation' }));
      return [...tasks, ...ctxs].slice(0, max);
    }
    case '$':
      return DOLLAR_VARS.filter(([n]) => starts(n))
        .map(([n, read]) => ({ label: `$${n}`, insert: `$${n} `, hint: read(s) ?? 'value' }))
        .slice(0, max);
  }
}

/** Replace the triggering token with a chosen suggestion. */
export function applySuggestion(input: string, sug: Suggestion): string {
  const t = triggerToken(input) ?? setPathToken(input);
  if (!t) return input;
  return input.slice(0, t.start) + sug.insert;
}

/** A message prepared for sending: routed and interpolated. */
export interface Prepared {
  text: string;
  /** A LEADING `#<task id>` target — answer/continue that task. */
  taskId?: string;
  /** A LEADING `#<ctx>` target — address that conversation. */
  contextId?: string;
}

/**
 * The one shape a server mints a task id in: a UUIDv4. A client never names
 * a task — it only continues one — so an id in this shape that the mirror
 * has not seen (an older task, one another client opened) is a task, while
 * a free-form name is a conversation the message may start.
 */
const SERVER_TASK_ID = /^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/i;

/**
 * Where a leading `#<id>` goes: a task the mirror holds, a conversation it
 * holds, else by shape — a server-minted id is taken for a task (the
 * caller's conversations are all in the mirror, its older tasks need not be),
 * and anything else names a conversation. A task id that turns out not to be
 * one is refused by the server as not found, never sent somewhere else.
 */
function targetOf(id: string, s: MirrorState): Pick<Prepared, 'taskId' | 'contextId'> {
  if (s.tasks.has(id)) return { taskId: id };
  if (s.conversations.has(id)) return { contextId: id };
  return SERVER_TASK_ID.test(id) ? { taskId: id } : { contextId: id };
}

/** Apply the `#` routing and `$` interpolation to an outgoing message. */
export function prepare(input: string, s: MirrorState): Prepared {
  let text = input.trim();
  const out: Prepared = { text };
  const m = /^#(\S+)\s+(.*)$/s.exec(text);
  if (m) {
    Object.assign(out, targetOf(m[1], s));
    text = m[2];
  }
  // `$name` for KNOWN, published names only; `$$` escapes a literal dollar.
  text = text.replace(/\$(\$|[a-z_]+)/g, (whole, name: string) => {
    if (name === '$') return '$';
    const hit = DOLLAR_VARS.find(([n]) => n === name);
    return hit?.[1](s) ?? whole;
  });
  out.text = text;
  return out;
}

/**
 * The input-required gate a plain message in conversation `contextId`
 * answers: the newest `INPUT_REQUIRED` task OF THAT CONVERSATION. None
 * without a conversation — a fresh one has no gates — and never a gate of
 * another conversation, however recent: an answer typed into one chat is not
 * an answer to a question asked in another. `AUTH_REQUIRED` is not a gate a
 * message can answer; the person must sign in, not reply.
 */
export function currentGate(s: MirrorState, contextId: string | undefined): TaskView | undefined {
  // No conversation matches no task: a task never has an undefined context.
  let gate: TaskView | undefined;
  for (const t of s.tasks.values()) {
    if (t.state !== 'TASK_STATE_INPUT_REQUIRED' || t.contextId !== contextId) continue;
    if (gate === undefined || t.updated > gate.updated) gate = t;
  }
  return gate;
}

/**
 * Where a prepared message goes, given the conversation the person is in:
 * - a leading `#task` target → that task (the person named it);
 * - otherwise the conversation — the `#ctx` target, else `current` — and,
 *   when that conversation has an open gate ({@link currentGate}), the gate.
 *
 * The answer never carries a `taskId` and a `contextId` together: the server
 * reads the context from the task, and two ids that could disagree are how a
 * reply once landed in another conversation's gate.
 */
export function routeSend(p: Prepared, s: MirrorState, current: string | undefined): Pick<SendOptions, 'taskId' | 'contextId'> {
  if (p.taskId !== undefined) return { taskId: p.taskId };
  const contextId = p.contextId ?? current;
  const gate = currentGate(s, contextId);
  if (gate) return { taskId: gate.id };
  return contextId !== undefined && contextId !== '' ? { contextId } : {};
}

// ---- sign-in administration (operator) ------------------------------------

/**
 * The name a device is approved as. The daemon is the authority (it also
 * refuses reserved names and names a configured principal already holds);
 * checking the shape here only lets a mistyped name get its usage line
 * before anything is sent. Lowercase, so two spellings never name two
 * principals.
 */
export const APPROVAL_NAME = /^[a-z0-9][a-z0-9._-]{0,63}$/;

/** A parsed `/approve`, `/deny`, `/devices`, `/sessions` or `/revoke`. */
export type AuthCommand =
  | { cmd: 'approve'; userCode: string; name: string; scope?: 'operator' }
  | { cmd: 'deny'; target: { userCode: string } | { all: true } }
  | { cmd: 'devices' }
  | { cmd: 'sessions' }
  | { cmd: 'revoke'; target: RevokeTarget };

const AUTH_USAGE: Readonly<Record<AuthCommand['cmd'], string>> = Object.freeze({
  approve: 'usage: /approve <code> <name> [operator] — the name the device signs in as (a-z, 0-9, . _ -)',
  deny: 'usage: /deny <code> | all',
  devices: 'usage: /devices',
  sessions: 'usage: /sessions',
  revoke: 'usage: /revoke <sid> | name <name> | all',
});

/**
 * Read an operator sign-in command. `null` when `cmd` is not one; `{usage}`
 * when it is one written wrong — nothing is sent for that.
 */
export function parseAuthCommand(cmd: string, args: readonly string[]): AuthCommand | { usage: string } | null {
  const usage = (c: AuthCommand['cmd']) => ({ usage: AUTH_USAGE[c] });
  switch (cmd) {
    case 'approve': {
      const [userCode, name, scope, ...extra] = args;
      // The name is required: every session approved under one name is one
      // principal and shares its tasks, so it is chosen, never defaulted.
      if (!userCode || !name || !APPROVAL_NAME.test(name) || extra.length > 0) return usage('approve');
      if (scope === undefined) return { cmd, userCode, name };
      return scope === 'operator' ? { cmd, userCode, name, scope } : usage('approve');
    }
    case 'deny':
      if (args.length !== 1) return usage('deny');
      return { cmd, target: args[0] === 'all' ? { all: true } : { userCode: args[0] } };
    case 'devices':
    case 'sessions':
      return args.length === 0 ? { cmd } : usage(cmd);
    case 'revoke':
      // A lone `name` is `/revoke name <name>` missing its name, not a sid.
      if (args.length === 1 && args[0] !== 'name') return { cmd, target: args[0] === 'all' ? { all: true } : { sid: args[0] } };
      if (args.length === 2 && args[0] === 'name') return { cmd, target: { name: args[1] } };
      return usage('revoke');
    default:
      return null;
  }
}

/** The client methods the sign-in commands call. */
export type AuthClient = Pick<
  AgentdClient,
  'authDeviceApprove' | 'authDeviceDeny' | 'authDevicePending' | 'authSessions' | 'authSessionsRevoke'
>;

/** What a sign-in command did, as a line for the transcript. */
export interface AuthOutcome {
  text: string;
  error?: boolean;
}

/** A value from a sign-in reply, as inert text: a device's client_id is whatever its requester chose. */
function shown(v: Json | undefined): string {
  return inert(text(v) ?? (typeof v === 'number' || typeof v === 'boolean' ? String(v) : '?'));
}

function rows(v: Json | undefined): Json[] {
  return Array.isArray(v) ? v : [];
}

/**
 * Run an operator sign-in command line (`/approve …` split into `cmd` and
 * `args`). A usage error answers locally and sends nothing. `confirm`, when
 * given, is asked before an approval — approving hands a device a principal.
 * Errors from the call propagate; the caller shows them as it shows any.
 */
export async function runAuthCommand(
  cmd: string,
  args: readonly string[],
  client: AuthClient,
  confirm?: (question: string) => Promise<boolean>,
): Promise<AuthOutcome | null> {
  const c = parseAuthCommand(cmd, args);
  if (c === null) return null;
  if ('usage' in c) return { text: c.usage, error: true };
  switch (c.cmd) {
    case 'approve': {
      const as = c.scope === 'operator' ? `${c.name} with OPERATOR scope` : c.name;
      if (confirm && !(await confirm(`approve device ${inert(c.userCode)} as ${as}?`))) {
        return { text: `not approved: ${inert(c.userCode)}` };
      }
      const r = field(await client.authDeviceApprove(c.userCode, c.name, c.scope), 'approved');
      const note =
        field(r, 'existing') === true
          ? ` — ${c.name} signed in before: this device shares every task, run and conversation that name owns`
          : '';
      return { text: `approved ${shown(field(r, 'user_code'))} as ${shown(field(r, 'principal'))} (${shown(field(r, 'scope'))})${note}` };
    }
    case 'deny': {
      const r = await client.authDeviceDeny(c.target);
      return { text: `denied ${shown(field(r, 'denied'))} sign-in(s)` };
    }
    case 'devices': {
      const pending = rows(field(await client.authDevicePending(), 'pending'));
      if (pending.length === 0) return { text: 'no device sign-ins waiting' };
      const lines = pending.map(
        (p) =>
          `${shown(field(p, 'user_code'))}  ${shown(field(p, 'client_id'))} · ${shown(field(p, 'scope'))}` +
          `${field(p, 'peer') !== undefined ? ` · from ${shown(field(p, 'peer'))}` : ''}`,
      );
      return { text: `waiting:\n${lines.join('\n')}\n/approve <code> <name> · /deny <code>` };
    }
    case 'sessions': {
      const sessions = rows(field(await client.authSessions(), 'sessions'));
      if (sessions.length === 0) return { text: 'no sessions' };
      const lines = sessions.map(
        (x) =>
          `${shown(field(x, 'sid'))}  ${shown(field(x, 'kind'))} · ${shown(field(x, 'principal'))}` +
          `${field(x, 'name') !== undefined ? ` (${shown(field(x, 'name'))})` : ''} · ${shown(field(x, 'client_id'))}`,
      );
      return { text: `sessions:\n${lines.join('\n')}\n/revoke <sid> | name <name> | all` };
    }
    case 'revoke': {
      const r = await client.authSessionsRevoke(c.target);
      return { text: `revoked ${shown(field(r, 'revoked'))} session(s)` };
    }
  }
}

/** A compact human duration (`8s`, `1m14s`, `2h03m`). */
export function elapsed(sinceMs: number, nowMs: number = Date.now()): string {
  const s = Math.max(0, Math.floor((nowMs - sinceMs) / 1000));
  if (s < 60) return `${s}s`;
  const m = Math.floor(s / 60);
  if (m < 60) return `${m}m${String(s % 60).padStart(2, '0')}s`;
  return `${Math.floor(m / 60)}h${String(m % 60).padStart(2, '0')}m`;
}

/**
 * A duration, at the precision the number deserves (`120ms`, `1.4s`, `2m03s`).
 *
 * Distinct from `elapsed`, which counts whole seconds because it is watching a
 * clock tick. Most steps finish in milliseconds, and rendering those as `0s`
 * throws away the only interesting thing about them — that they were fast, and
 * which one of them was not.
 */
export function duration(ms: number): string {
  if (ms < 1000) return `${Math.max(0, Math.round(ms))}ms`;
  const s = ms / 1000;
  if (s < 60) return `${s.toFixed(s < 10 ? 1 : 0)}s`;
  const m = Math.floor(s / 60);
  return `${m}m${String(Math.floor(s % 60)).padStart(2, '0')}s`;
}

/** Compact token count (`940`, `1.2k`, `18k`). */
export function tokens(n: number): string {
  if (n < 1000) return String(n);
  if (n < 100_000) return `${(n / 1000).toFixed(1).replace(/\.0$/, '')}k`;
  return `${Math.round(n / 1000)}k`;
}

/**
 * The live working line (RFC 0032 §17) — what the agent is doing, how long it
 * has been at it, and what it has spent: `thinking · 12s · 1.2k tok · round 2`
 * or `read_file · 3s · 1.2k tok`. Elapsed ticks locally from `started_ms`, so
 * the daemon emits nothing while a long think runs.
 */
export function activityLine(a: Activity | undefined, nowMs: number = Date.now()): string {
  if (!a) return 'working';
  const what =
    a.phase === 'tool'
      ? (a.tool ?? 'tool')
      : a.phase === 'waiting'
        ? `waiting · ${a.tool ?? 'wait'}`
        : 'thinking';
  const parts = [what, elapsed(a.started_ms, nowMs)];
  const spent = a.tokens_in + a.tokens_out;
  if (spent > 0) parts.push(`${tokens(spent)} tok`);
  if (a.round > 1) parts.push(`round ${a.round}`);
  return parts.join(' · ');
}
