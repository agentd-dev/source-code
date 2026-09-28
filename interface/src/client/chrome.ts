// SPDX-License-Identifier: AGPL-3.0-only
/**
 * The chrome — the top and bottom edges both display clients draw around the
 * conversation — and what each item in it shows.
 *
 * The layout is the CLIENT's business. The daemon used to own the item
 * vocabulary, the defaults and even their validation, although half the
 * items (`conn`, `endpoint`, `screen`, `keys`, `clock`) are client state no
 * daemon can know. Now the vocabulary lives here, once, for the TUI and the
 * web UI alike; each surface picks its own default; and a person reshapes it
 * locally (`--top`/`--bottom` and `/layout` in the TUI, `/layout` in the
 * browser). The one thing only the daemon can supply is a value a workflow
 * keeps in memory, which it publishes as `status.values` for the keys the
 * operator listed in `observability.status_values` — `memory:<key>` reads it.
 *
 * Nothing here renders: {@link itemValue} says what an item reads and in
 * which tone, and each surface maps the tone to its own colours.
 */

import { CONTROL, type Json } from './types.js';

/** Which client draws a chrome. */
export type Surface = 'tui' | 'web';

/** One item of the chrome vocabulary. */
export interface DisplayItem {
  /** Where the item means something: `screen` and `keys` describe the TUI. */
  surfaces: readonly Surface[];
  /** One line for `/layout` help. */
  describe: string;
}

/**
 * The prefix of a memory item: `memory:deploy.state` shows `status.values`
 * under `deploy.state`. Any key parses — whether the daemon publishes it is
 * the operator's `observability.status_values`, and an unpublished key simply
 * shows nothing.
 */
export const MEMORY_PREFIX = 'memory:';

const BOTH: readonly Surface[] = ['tui', 'web'];

/** The chrome vocabulary: every item name a layout may use (plus `memory:<key>`). */
export const DISPLAY_ITEMS: Readonly<Record<string, DisplayItem>> = Object.freeze({
  name: { surfaces: BOTH, describe: "the agent's name, from its card" },
  version: { surfaces: BOTH, describe: "the daemon's build version" },
  instance: { surfaces: BOTH, describe: 'the instance id' },
  model: { surfaces: BOTH, describe: 'the model in use' },
  endpoint: { surfaces: BOTH, describe: 'the endpoint this client talks to' },
  conn: { surfaces: BOTH, describe: 'the connection state' },
  debug: { surfaces: BOTH, describe: 'a badge while the daemon offers introspection' },
  draining: { surfaces: BOTH, describe: 'DRAINING or PAUSED while the daemon is' },
  active: { surfaces: BOTH, describe: 'how many units are working' },
  turns: { surfaces: BOTH, describe: 'model turns so far (operators)' },
  tokens: { surfaces: BOTH, describe: 'tokens in/out so far (operators)' },
  tool_calls: { surfaces: BOTH, describe: 'tool calls so far (operators)' },
  runs: { surfaces: BOTH, describe: 'workflow runs this client can see' },
  subagents: { surfaces: BOTH, describe: 'subagents this client can see' },
  conversations: { surfaces: BOTH, describe: 'conversations this client can see' },
  clock: { surfaces: BOTH, describe: 'the local time' },
  screen: { surfaces: ['tui'], describe: 'the current screen' },
  keys: { surfaces: ['tui'], describe: 'the key hints' },
});

/** A chrome layout: the items of the top and bottom edges, in order. */
export interface Layout {
  top: string[];
  bottom: string[];
}

/**
 * What each surface shows until its user says otherwise. The browser has no
 * screen switcher or key hints to show, so its edges are shorter.
 */
export const DEFAULT_LAYOUT: Readonly<Record<Surface, Readonly<Layout>>> = Object.freeze({
  tui: Object.freeze({
    top: ['name', 'version', 'instance', 'debug'],
    bottom: ['conn', 'endpoint', 'draining', 'active', 'turns', 'tokens', 'screen', 'keys'],
  }),
  web: Object.freeze({
    top: ['name', 'version', 'instance', 'debug'],
    bottom: ['conn', 'endpoint', 'draining', 'active', 'turns', 'tokens'],
  }),
});

/** Is `name` an item this surface can draw? */
export function isDisplayItem(name: string, surface: Surface): boolean {
  if (name.startsWith(MEMORY_PREFIX)) return name.length > MEMORY_PREFIX.length;
  // `hasOwn`, not `in`: `constructor` or `toString` is not an item.
  return Object.hasOwn(DISPLAY_ITEMS, name) && DISPLAY_ITEMS[name].surfaces.includes(surface);
}

/**
 * Read one edge of a layout as a person wrote it — `name,version,memory:pr`
 * from a flag or an environment variable, or a list from browser storage.
 * Unknown names are REPORTED rather than silently dropped, so a typo in
 * `--top` says so instead of just not showing; they never reach `items`.
 */
export function parseLayout(input: string | readonly string[], surface: Surface): { items: string[]; unknown: string[] } {
  const raw = typeof input === 'string' ? input.split(',') : input;
  const items: string[] = [];
  const unknown: string[] = [];
  for (const r of raw) {
    if (typeof r !== 'string') continue;
    const name = r.trim();
    if (name.length === 0) continue;
    const into = isDisplayItem(name, surface) ? items : unknown;
    if (!into.includes(name)) into.push(name);
  }
  return { items, unknown };
}

/**
 * How an item reads, by meaning rather than colour: each surface maps these
 * to its own palette (the TUI's theme, the web UI's classes).
 */
export type Tone =
  /** Who this is: the agent's name, the instance. */
  | 'identity'
  /** Secondary facts. */
  | 'muted'
  /** Healthy and moving: live, working. */
  | 'live'
  /** Worth a glance: polling, introspection on. */
  | 'caution'
  /** The daemon is not taking work: DRAINING, PAUSED. */
  | 'alert'
  | 'error'
  /** A value a workflow keeps (`memory:<key>`). */
  | 'value';

/** What one item shows: its text and tone. */
export interface ItemValue {
  text: string;
  tone: Tone;
}

/** Everything an item may read. Every field is optional except what a client always knows. */
export interface ChromeInput {
  /** The public Agent Card. */
  card?: Json;
  /** The latest `status` document (the bootstrap, then live updates). */
  status?: Json;
  /** The connection state, as the client names it. */
  conn: string;
  /** The last connection error, for `conn` in state `error`. */
  error?: string;
  endpoint: string;
  /** The daemon offers the introspection ops (the card says so). */
  introspection?: boolean;
  /** Live lifecycle state; when absent, the status document's flags are read. */
  draining?: boolean;
  paused?: boolean;
  /** Units working right now. */
  active?: number;
  /** How many of each the client currently mirrors. */
  counts?: { runs?: number; subagents?: number; conversations?: number };
  /** The TUI's current screen. */
  screen?: string;
  /** Epoch ms for `clock`; the current time when absent. */
  now?: number;
}

/** The connection states and how each reads. Anything else reads as itself, muted. */
const CONN: Readonly<Record<string, ItemValue>> = Object.freeze({
  ready: { text: '● live', tone: 'live' },
  polling: { text: '◐ polling', tone: 'caution' },
  connecting: { text: '○ connecting', tone: 'muted' },
  closed: { text: '○ closed', tone: 'muted' },
  // Each stop reason reads as itself: "closed" for a refused credential sent
  // people looking for a network fault when the fix was to sign in again.
  unauthenticated: { text: '✗ unauthenticated', tone: 'error' },
  forbidden: { text: '✗ forbidden', tone: 'error' },
  incompatible: { text: '✗ incompatible', tone: 'error' },
});

function obj(v: Json | undefined): { [k: string]: Json } | undefined {
  return v !== null && typeof v === 'object' && !Array.isArray(v) ? v : undefined;
}

function str(v: Json | undefined): string | undefined {
  return typeof v === 'string' && v.length > 0 ? v : undefined;
}

function num(v: Json | undefined): number | undefined {
  return typeof v === 'number' && Number.isFinite(v) ? v : undefined;
}

/**
 * What `name` shows, or `null` for nothing at all. An item with nothing to say
 * (an unset memory key, counters a non-operator is not shown, no active
 * units) takes no slot rather than an empty one, because a blank reads as
 * broken. An unknown name is `null` too, so a layout written for a newer
 * client still draws the rest.
 *
 * Every item is one line of inert text: control characters ({@link CONTROL} —
 * chrome text comes from places a model can write, and a newline breaks a
 * one-line bar) become a space, and an item left with nothing visible takes
 * no slot.
 */
export function itemValue(name: string, c: ChromeInput): ItemValue | null {
  const v = rawItemValue(name, c);
  if (v === null) return null;
  const text = v.text.replace(CONTROL, ' ');
  if (text.trim() === '') return null;
  return text === v.text ? v : { ...v, text };
}

function rawItemValue(name: string, c: ChromeInput): ItemValue | null {
  const status = obj(c.status);
  if (name.startsWith(MEMORY_PREFIX)) {
    const key = name.slice(MEMORY_PREFIX.length);
    const values = obj(status?.values);
    // `hasOwn`: a memory key named like an Object.prototype member is still
    // just a key.
    const v = values && Object.hasOwn(values, key) ? values[key] : undefined;
    if (v === undefined || v === null || v === '') return null;
    return { text: typeof v === 'string' ? v : JSON.stringify(v), tone: 'value' };
  }
  const counters = obj(status?.counters);
  switch (name) {
    case 'name':
      return { text: str(obj(c.card)?.name) ?? 'agentd', tone: 'identity' };
    case 'version': {
      const v = str(status?.version);
      return v ? { text: v, tone: 'muted' } : null;
    }
    case 'instance': {
      const v = str(status?.instance);
      return v ? { text: v, tone: 'identity' } : null;
    }
    case 'model': {
      const v = str(status?.model);
      return v ? { text: v, tone: 'muted' } : null;
    }
    case 'endpoint':
      return { text: c.endpoint, tone: 'muted' };
    case 'conn':
      if (c.conn === 'error') return { text: `✗ ${c.error ?? 'error'}`, tone: 'error' };
      return Object.hasOwn(CONN, c.conn) ? CONN[c.conn] : { text: c.conn, tone: 'muted' };
    case 'debug':
      return c.introspection ? { text: 'debug', tone: 'caution' } : null;
    case 'draining': {
      if (c.draining ?? status?.draining === true) return { text: 'DRAINING', tone: 'alert' };
      if (c.paused ?? status?.paused === true) return { text: 'PAUSED', tone: 'alert' };
      return null;
    }
    case 'active':
      return c.active !== undefined && c.active > 0 ? { text: `${c.active} active`, tone: 'live' } : null;
    // The counters are in the operator's status document only: another
    // principal is not shown the daemon's totals, so the item stays away
    // rather than claiming zero.
    case 'turns': {
      const n = num(counters?.turns);
      return n !== undefined ? { text: `${n} turns`, tone: 'muted' } : null;
    }
    case 'tokens':
      return counters
        ? { text: `${num(counters.tokens_in) ?? 0}/${num(counters.tokens_out) ?? 0} tok`, tone: 'muted' }
        : null;
    case 'tool_calls': {
      const n = num(counters?.tool_calls);
      return n !== undefined ? { text: `${n} tools`, tone: 'muted' } : null;
    }
    case 'runs':
      return c.counts?.runs !== undefined ? { text: `${c.counts.runs} runs`, tone: 'muted' } : null;
    case 'subagents':
      return c.counts?.subagents !== undefined ? { text: `${c.counts.subagents} subagents`, tone: 'muted' } : null;
    case 'conversations':
      return c.counts?.conversations !== undefined ? { text: `${c.counts.conversations} conv`, tone: 'muted' } : null;
    case 'screen':
      return c.screen !== undefined ? { text: `[${c.screen}]`, tone: 'muted' } : null;
    case 'keys':
      return { text: 'tab:screens esc:cancel /:cmd ^c:quit', tone: 'muted' };
    case 'clock':
      return { text: new Date(c.now ?? Date.now()).toLocaleTimeString(), tone: 'muted' };
    default:
      return null;
  }
}
