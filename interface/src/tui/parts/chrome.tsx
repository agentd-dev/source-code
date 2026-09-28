// SPDX-License-Identifier: AGPL-3.0-only
/**
 * The chrome: the top (header) edge renders the item list the app hands it —
 * the client's own layout (chrome.ts). What each item SAYS is chrome.ts's
 * `itemValue`, the one reading both display clients share; this file only
 * maps its tones onto the terminal palette. An item with nothing to say
 * (or a name this client does not know) takes no slot.
 */
import React from 'react';
import { Box, Text } from 'ink';
import { introspectionOn, itemValue } from '../../client/index.js';
import type { ChromeInput, Json, MirrorState, Tone } from '../../client/index.js';
import { theme } from '../theme.js';

export interface ChromeCtx {
  s: MirrorState;
  endpoint: string;
  screen: string;
  active: number;
}

/** Each tone in the terminal's palette; `bold` marks what must not be missed. */
export const TONE: Readonly<Record<Tone, { color: string; bold?: boolean }>> = Object.freeze({
  identity: { color: theme.accent },
  muted: { color: theme.dim },
  live: { color: theme.accent },
  caution: { color: theme.warn },
  alert: { color: theme.warn, bold: true },
  error: { color: theme.error, bold: true },
  value: { color: theme.command },
});

type Obj = { [k: string]: Json };

function obj(v: Json | undefined): Obj | undefined {
  return v !== null && v !== undefined && typeof v === 'object' && !Array.isArray(v) ? v : undefined;
}

/**
 * What the mirror knows, as chrome.ts reads it. The status document is the
 * bootstrap read overlaid with the live `status` events: the live one is
 * slim, so a fact only the bootstrap carries still shows.
 */
export function chromeInput(c: ChromeCtx): ChromeInput {
  const { s } = c;
  const live = obj(s.status);
  const boot = obj(s.bootstrap);
  const out: ChromeInput = {
    conn: s.conn,
    endpoint: c.endpoint,
    introspection: introspectionOn(s),
    draining: s.draining,
    paused: s.paused,
    active: c.active,
    counts: { runs: s.runs.size, subagents: s.subagents.size, conversations: s.conversations.size },
    screen: c.screen,
  };
  const card = s.session?.extended ?? s.session?.card;
  if (card !== undefined) out.card = card;
  if (live !== undefined || boot !== undefined) out.status = { ...boot, ...live };
  if (s.error !== undefined) out.error = s.error;
  return out;
}

/**
 * One item, rendered, or null when it has nothing to show. `inverse` draws it
 * as a block, for a state nobody should be able to miss.
 */
export function renderItem(name: string, input: ChromeInput, inverse = false): React.JSX.Element | null {
  const v = itemValue(name, input);
  if (v === null) return null;
  const tone = TONE[v.tone];
  return (
    <Text key={name} color={tone.color} bold={tone.bold === true || name === 'name'} inverse={inverse}>
      {inverse ? ` ${v.text} ` : v.text}
    </Text>
  );
}

/** A row of items with one column between them; the `inverse` ones drawn as blocks. */
export function ItemRow({
  items,
  input,
  inverse,
}: {
  items: string[];
  input: ChromeInput;
  inverse?: ReadonlySet<string>;
}): React.JSX.Element {
  const rendered = items
    .map((n) => renderItem(n, input, inverse?.has(n) === true))
    .filter((x): x is React.JSX.Element => x !== null);
  // Explicit per-item margins (not `gap`): ink collapses gap between bare Text
  // children in some layouts, gluing adjacent items.
  return (
    <Box flexDirection="row" flexWrap="wrap">
      {rendered.map((el) => (
        <Box key={el.key ?? undefined} marginRight={1}>
          {el}
        </Box>
      ))}
    </Box>
  );
}

/** The top edge. */
export function Edge({ items, ctx }: { items: string[]; ctx: ChromeCtx }): React.JSX.Element {
  return <ItemRow items={items} input={chromeInput(ctx)} />;
}
