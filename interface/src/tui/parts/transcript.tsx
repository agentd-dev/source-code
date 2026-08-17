// SPDX-License-Identifier: AGPL-3.0-only
/**
 * The conversation view, in two modes.
 *
 * **Fullscreen** (the default): a bottom-anchored, clipped viewport the app
 * scrolls itself (PgUp/PgDn) — the alternate screen has no scrollback of its
 * own, so the terminal cannot do it for us.
 *
 * **Inline** (`--inline`): settled entries ride Ink's `<Static>` — written
 * once into the terminal's real scrollback, never re-rendered (the flicker
 * rule: the dynamic region stays small) — while in-flight rows render below.
 * The conversation survives quitting.
 */
import React from 'react';
import { Box, Static, Text } from 'ink';
import type { TranscriptEntry } from '../../client/index.js';
import { SPINNER, theme } from '../theme.js';

function Row({ e }: { e: TranscriptEntry }): React.JSX.Element {
  switch (e.kind) {
    case 'user':
      return (
        <Box flexDirection="row">
          <Text color={theme.user} bold>
            {'you › '}
          </Text>
          <Text wrap="wrap">{e.text}</Text>
          {e.principal && e.principal !== 'operator' ? (
            <Text color={theme.dim}> ({e.principal})</Text>
          ) : null}
        </Box>
      );
    case 'agent':
      return (
        <Box flexDirection="row">
          <Text color={theme.agent} bold>
            {'agent › '}
          </Text>
          <Text wrap="wrap">
            {e.text}
            {e.inputRequired ? '' : ''}
          </Text>
          {e.inputRequired ? <Text color={theme.warn}> [reply to continue]</Text> : null}
        </Box>
      );
    case 'command':
      return (
        <Text color={theme.command}>
          {'▸ '}
          {e.text}
          {e.principal && e.principal !== 'operator' ? ` (${e.principal})` : ''}
        </Text>
      );
    case 'error':
      return (
        <Box flexDirection="row">
          <Text color={theme.error} bold>
            {'agent ✗ '}
          </Text>
          <Text color={theme.error} wrap="wrap">
            {e.text}
          </Text>
        </Box>
      );
    default:
      return <Text color={theme.info}>{`· ${e.text}`}</Text>;
  }
}

/** A viewport for fullscreen mode (the alternate screen has no scrollback). */
export interface Viewport {
  /** Rows the transcript may occupy. */
  rows: number;
  /** Terminal width, for estimating wrapped heights. */
  columns: number;
  /** Entries hidden below the bottom (0 = following the live end). */
  offset: number;
}

export interface TranscriptProps {
  entries: TranscriptEntry[];
  /** A live "working" row under the settled history. */
  working?: { text: string; frame: number } | null;
  /** Set in fullscreen: render a windowed viewport instead of scrollback. */
  viewport?: Viewport;
}

/** Roughly how many terminal rows an entry occupies once wrapped. */
function heightOf(e: TranscriptEntry, columns: number): number {
  const prefix = e.kind === 'user' || e.kind === 'agent' || e.kind === 'error' ? 8 : 2;
  const width = Math.max(20, columns - prefix);
  return e.text.split('\n').reduce((n, line) => n + Math.max(1, Math.ceil(line.length / width)), 0);
}

/**
 * The entries that fit `rows`, counting from the bottom and skipping `offset`
 * entries below the window. Returns the slice plus how many are hidden above
 * (for the scroll hint).
 */
export function windowEntries(
  entries: TranscriptEntry[],
  { rows, columns, offset }: Viewport,
): { visible: TranscriptEntry[]; above: number } {
  const end = Math.max(0, entries.length - offset);
  const visible: TranscriptEntry[] = [];
  let used = 0;
  let i = end - 1;
  for (; i >= 0; i--) {
    const h = heightOf(entries[i], columns);
    if (used + h > rows && visible.length > 0) break;
    used += h;
    visible.unshift(entries[i]);
  }
  return { visible, above: i + 1 };
}

export function Transcript({ entries, working, viewport }: TranscriptProps): React.JSX.Element {
  // Fullscreen: a windowed viewport, bottom-anchored, clipped — the alternate
  // screen has no scrollback of its own, so the app owns the scroll.
  if (viewport) {
    const { visible, above } = windowEntries(entries, viewport);
    return (
      <Box flexDirection="column" height={viewport.rows} overflow="hidden">
        <Box flexGrow={1} />
        {above > 0 ? (
          <Text color={theme.dim}>
            ↑ {above} earlier {above === 1 ? 'message' : 'messages'}
            {viewport.offset > 0 ? ' · PgDn to follow' : ' · PgUp to scroll'}
          </Text>
        ) : null}
        {visible.map((e) => (
          <Row key={e.key} e={e} />
        ))}
        {working ? (
          <Text color={theme.accent}>
            {SPINNER[working.frame % SPINNER.length]} {working.text}
          </Text>
        ) : null}
      </Box>
    );
  }
  // Inline: settled rows go to terminal scrollback (never re-rendered);
  // anything still moving stays in the dynamic region.
  const settled = entries.filter((e) => !e.pending && !e.inputRequired);
  const live = entries.filter((e) => e.pending || e.inputRequired);
  return (
    <Box flexDirection="column">
      <Static items={settled}>{(e) => <Row key={e.key} e={e} />}</Static>
      {live.map((e) => (
        <Row key={e.key} e={e} />
      ))}
      {working ? (
        <Text color={theme.accent}>
          {SPINNER[working.frame % SPINNER.length]} {working.text}
        </Text>
      ) : null}
    </Box>
  );
}
