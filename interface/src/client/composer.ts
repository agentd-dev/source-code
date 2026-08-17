// SPDX-License-Identifier: Apache-2.0
/**
 * Composer affordances (RFC 0032 §15) — shared by the TUI and the web UI so
 * both surfaces behave identically:
 *
 *   `/`  commands — the SYSTEM set, plus every daemon workflow as a shortcut
 *        (`/deploy` ⇒ `workflow.run deploy`; system names win).
 *   `@`  skills — autocompletes the daemon's skill catalogue; the reference
 *        stays INLINE in the text (agentd preloads a referenced skill).
 *   `#`  targets — a LEADING `#<task-id|context-id>` routes the message: a
 *        task id answers/continues that task (input-required!), anything else
 *        addresses that conversation. Inline `#…` is left as plain text.
 *   `$`  values — `$model`, `$instance`, … interpolate live daemon state into
 *        the text before sending; unknown `$words` are left alone; `$$` ⇒ `$`.
 */

import { Activity, MirrorState, TERMINAL_STATES } from './types.js';

/** One completion the UI can apply. */
export interface Suggestion {
  /** What the user sees. */
  label: string;
  /** The replacement for the triggering token. */
  insert: string;
  /** A short right-hand hint. */
  hint: string;
}

/** The system slash commands (name → hint), shared by both UIs. */
export const SYSTEM_COMMANDS: ReadonlyArray<[string, string]> = [
  ['help', 'list commands'],
  ['new', 'start a fresh conversation'],
  ['tasks', 'the tasks screen'],
  ['subagents', 'the subagents screen'],
  ['debug', 'the debug screen'],
  ['chat', 'back to the conversation'],
  ['status', 'daemon status summary'],
  ['config', 'show the effective config (or one path)'],
  ['set', 'runtime-set a knob: /set interface.debug true'],
  ['workflow', 'run a workflow: /workflow <name>'],
  ['cancel', 'cancel a task (newest if none given)'],
  ['signal', 'fire a workflow signal: /signal <name> [run]'],
  ['send', 'message a warm subagent: /send <handle> <text>'],
  ['pause', 'pause a run, or the whole instance'],
  ['resume', 'resume a run / the instance'],
  ['plan', "a conversation's working plan"],
  ['conversations', 'list conversations (#<id> to address one)'],
  ['pair', 'show the pairing code (operator)'],
  ['drain', 'graceful drain'],
  ['quit', 'leave the client'],
];

/** The `$` values a client can interpolate, with their reader. */
const DOLLAR_VARS: ReadonlyArray<[string, (s: MirrorState) => string]> = [
  ['model', (s) => s.info?.model ?? ''],
  ['instance', (s) => s.info?.instance ?? ''],
  ['version', (s) => s.info?.version ?? ''],
  ['turns', (s) => String(counters(s)?.turns ?? 0)],
  ['tokens', (s) => `${counters(s)?.tokens_in ?? 0}/${counters(s)?.tokens_out ?? 0}`],
  ['tasks', (s) => String(s.tasks.size)],
];

function counters(s: MirrorState):
  | { turns?: number; tokens_in?: number; tokens_out?: number }
  | undefined {
  return ((s.status ?? s.bootstrap) as { counters?: { turns?: number; tokens_in?: number; tokens_out?: number } } | undefined)
    ?.counters;
}

/** The workflow names the daemon serves (from the bootstrap status doc). */
export function workflowNames(s: MirrorState): string[] {
  const wfs = (s.bootstrap as { workflows?: { name?: string }[] } | undefined)?.workflows;
  return (wfs ?? []).map((w) => w.name ?? '').filter((n) => n.length > 0);
}

/** The skill names the daemon serves. */
export function skillNames(s: MirrorState): string[] {
  const sk = (s.bootstrap as { skills?: string[] } | undefined)?.skills;
  return (sk ?? []).filter((n) => typeof n === 'string');
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

/** Completions for the current input (empty when no trigger / no match). */
export function suggest(input: string, s: MirrorState, max = 6): Suggestion[] {
  const t = triggerToken(input);
  if (!t) return [];
  const q = t.query.toLowerCase();
  const starts = (name: string) => name.toLowerCase().startsWith(q);
  switch (t.trigger) {
    case '/': {
      const sys: Suggestion[] = SYSTEM_COMMANDS.filter(([n]) => starts(n)).map(([n, hint]) => ({
        label: `/${n}`,
        insert: `/${n} `,
        hint,
      }));
      const wf: Suggestion[] = workflowNames(s)
        .filter((n) => starts(n) && !SYSTEM_COMMANDS.some(([c]) => c === n))
        .map((n) => ({ label: `/${n}`, insert: `/${n} `, hint: 'workflow' }));
      return [...sys, ...wf].slice(0, max);
    }
    case '@':
      return skillNames(s)
        .filter(starts)
        .map((n) => ({ label: `@${n}`, insert: `@${n} `, hint: 'skill' }))
        .slice(0, max);
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
        .map(([n, read]) => ({ label: `$${n}`, insert: `$${n} `, hint: read(s) || 'value' }))
        .slice(0, max);
  }
}

/** Replace the triggering token with a chosen suggestion. */
export function applySuggestion(input: string, sug: Suggestion): string {
  const t = triggerToken(input);
  if (!t) return input;
  return input.slice(0, t.start) + sug.insert;
}

/** A message prepared for sending: routed and interpolated. */
export interface Prepared {
  text: string;
  /** A LEADING `#task-…` target — answer/continue that task. */
  taskId?: string;
  /** A LEADING `#<ctx>` target — address that conversation. */
  contextId?: string;
}

/** Apply the `#` routing and `$` interpolation to an outgoing message. */
export function prepare(input: string, s: MirrorState): Prepared {
  let text = input.trim();
  const out: Prepared = { text };
  const m = /^#(\S+)\s+(.*)$/s.exec(text);
  if (m) {
    const id = m[1];
    if (s.tasks.has(id) || id.startsWith('task-')) out.taskId = id;
    else out.contextId = id;
    text = m[2];
  }
  // `$name` for KNOWN names only; `$$` escapes a literal dollar.
  text = text.replace(/\$(\$|[a-z_]+)/g, (whole, name: string) => {
    if (name === '$') return '$';
    const hit = DOLLAR_VARS.find(([n]) => n === name);
    return hit ? hit[1](s) : whole;
  });
  out.text = text;
  return out;
}


/** A compact human duration (`8s`, `1m14s`, `2h03m`). */
export function elapsed(sinceMs: number, nowMs: number = Date.now()): string {
  const s = Math.max(0, Math.floor((nowMs - sinceMs) / 1000));
  if (s < 60) return `${s}s`;
  const m = Math.floor(s / 60);
  if (m < 60) return `${m}m${String(s % 60).padStart(2, '0')}s`;
  return `${Math.floor(m / 60)}h${String(m % 60).padStart(2, '0')}m`;
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
