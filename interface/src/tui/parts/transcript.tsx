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
 *
 * **Who said what is shown by treatment, not by a name.** Author labels
 * (`you ›` / `agent ›`) cost a column on every line and read as chat-log
 * boilerplate; instead the user's own lines are drawn **inverse** — the
 * terminal's own way of saying "this block is yours" — and everything else
 * gets a one-character gutter mark. Multi-line text keeps that gutter, so a
 * pasted paragraph stays visually attached to its speaker.
 *
 * The rows come from each task's core `history` (the mirror builds them): a
 * person's prompt from whichever client sent it, the agent's superseded
 * status messages, a command, and the task's own state last. A task waiting
 * on a person reads differently by what it waits for — an INPUT_REQUIRED
 * gate says how to answer it, an AUTH_REQUIRED one says no reply can.
 */
import React from 'react';
import { Box, Static, Text } from 'ink';
import { duration } from '../../client/index.js';
import type { TranscriptEntry } from '../../client/index.js';
import { SPINNER, theme } from '../theme.js';

/** The gutter is two columns wide for every kind — text lines up down the page. */
const GUTTER = 2;

/** Split on newlines, dropping a single trailing blank line. */
function lines(text: string): string[] {
  const out = text.replace(/\s+$/, '').split('\n');
  return out.length > 0 ? out : [''];
}

/**
 * One line word-wrapped to `width` columns, the break's space dropped. Ink's
 * own wrap keeps it, so a continuation after a line that filled the column
 * started with a space and was one column too wide: the terminal wrapped it
 * again and the frame lost a character and a row. A word longer than the
 * width is broken.
 */
export function wrapLine(line: string, width: number): string[] {
  const w = Math.max(1, width);
  const out: string[] = [];
  let cur = '';
  for (const word of line.split(' ')) {
    let rest = word;
    while ([...rest].length > w) {
      if (cur !== '') {
        out.push(cur);
        cur = '';
      }
      const cps = [...rest];
      out.push(cps.slice(0, w).join(''));
      rest = cps.slice(w).join('');
    }
    if (cur === '') cur = rest;
    else if ([...cur].length + 1 + [...rest].length <= w) cur += ' ' + rest;
    else {
      out.push(cur);
      cur = rest;
    }
  }
  out.push(cur);
  return out;
}

/**
 * A block of text with a one-character mark in the gutter: the mark on the
 * first line, blanks under it, so wrapped/multi-line bodies stay indented.
 */
function Marked({
  mark,
  markColor,
  color,
  dim,
  width,
  children,
}: {
  /** The text column's width, when the terminal's is known: measured beside
   * the gutter, a flex-grown column is judged wider than it is drawn, and a
   * wrapped line then loses its first character to the row before it. */
  width?: number;
  mark: string;
  markColor: string;
  color?: string;
  dim?: boolean;
  children: string;
}): React.JSX.Element {
  return (
    <Box flexDirection="row">
      <Box width={GUTTER} flexShrink={0}>
        <Text color={markColor}>{mark}</Text>
      </Box>
      <Box flexDirection="column" flexGrow={width === undefined ? 1 : 0} flexShrink={0} width={width}>
        {(width === undefined ? lines(children) : lines(children).flatMap((l) => wrapLine(l, width))).map((l, i) => (
          <Text key={i} color={color} dimColor={dim} wrap={width === undefined ? 'wrap' : 'truncate-end'}>
            {l === '' ? ' ' : l}
          </Text>
        ))}
      </Box>
    </Box>
  );
}

/**
 * The line under a waiting row: how to answer an INPUT_REQUIRED gate — a
 * plain reply when it is the gate this conversation's replies go to, else
 * addressed to its task — and nothing for AUTH_REQUIRED, whose text already
 * says it waits outside the conversation.
 */
function waitHint(e: TranscriptEntry, gate: string | undefined): string | undefined {
  if (!e.inputRequired) return undefined;
  return e.taskId !== undefined && e.taskId !== gate ? `⏎ answer with #${e.taskId} <reply>` : '⏎ reply to continue';
}

function Row({ e, columns, gate }: { e: TranscriptEntry; columns?: number; gate?: string }): React.JSX.Element {
  const textWidth = columns === undefined ? undefined : Math.max(1, columns - GUTTER);
  switch (e.kind) {
    case 'user': {
      // Inverse: the user's own words, in the terminal's own idiom for
      // "selected/mine". Each line is padded so the block reads as a block
      // rather than as ragged highlighted words.
      const width = Math.max(20, (columns ?? 80) - GUTTER - 1);
      return (
        <Box flexDirection="row" marginTop={1}>
          <Box width={GUTTER} flexShrink={0}>
            <Text color={theme.user}>{'▌'}</Text>
          </Box>
          <Box flexDirection="column" flexGrow={1}>
            {lines(e.text).map((l, i) => (
              <Text key={i} color={theme.user} inverse wrap="truncate-end">
                {` ${l} `.padEnd(width, ' ')}
              </Text>
            ))}
            {e.principal && e.principal !== 'operator' ? (
              <Text color={theme.dim}>{`  ${e.principal}`}</Text>
            ) : null}
          </Box>
        </Box>
      );
    }
    case 'agent': {
      const hint = waitHint(e, gate);
      return (
        <Box flexDirection="column" marginTop={1}>
          {e.authRequired ? (
            <Marked mark="⚿" markColor={theme.warn} color={theme.warn} width={textWidth}>
              {e.text}
            </Marked>
          ) : (
            <Marked mark="●" markColor={theme.agent} width={textWidth}>
              {e.text}
            </Marked>
          )}
          {hint !== undefined ? (
            <Box marginLeft={GUTTER}>
              <Text color={theme.warn}>{hint}</Text>
            </Box>
          ) : null}
          {/* What the live counter settled at. Without it the number vanishes
              at the moment it became a fact, and "how long did that take?" is
              the question people ask about an agent more than any other. */}
          {e.ms !== undefined && !e.inputRequired && !e.authRequired ? (
            <Box marginLeft={GUTTER}>
              <Text color={theme.dim}>{duration(e.ms)}</Text>
            </Box>
          ) : null}
        </Box>
      );
    }
    case 'command':
      return (
        <Marked mark="▸" markColor={theme.command} color={theme.command} width={textWidth}>
          {e.text + (e.principal && e.principal !== 'operator' ? ` (${e.principal})` : '')}
        </Marked>
      );
    case 'error':
      return (
        <Box flexDirection="column" marginTop={1}>
          <Marked mark="✗" markColor={theme.error} color={theme.error} width={textWidth}>
            {e.text}
          </Marked>
          {e.ms !== undefined ? (
            <Box marginLeft={GUTTER}>
              <Text color={theme.dim}>{duration(e.ms)}</Text>
            </Box>
          ) : null}
        </Box>
      );
    default:
      return (
        <Marked mark="·" markColor={theme.dim} dim width={textWidth}>
          {e.text}
        </Marked>
      );
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
  /** The task a plain reply in this conversation answers (`currentGate`). */
  gate?: string;
}

/**
 * Roughly how many terminal rows an entry occupies once wrapped — every
 * newline is a row, plus the blank separator line the spaced kinds carry.
 */
function heightOf(e: TranscriptEntry, columns: number): number {
  // The rows Marked draws: the same wrap, at the same width.
  const width = Math.max(1, columns - GUTTER);
  const body = lines(e.text).reduce((n, line) => n + wrapLine(line, width).length, 0);
  const spacer = e.kind === 'user' || e.kind === 'agent' || e.kind === 'error' ? 1 : 0;
  const hint = e.kind === 'agent' && e.inputRequired ? 1 : 0;
  return body + spacer + hint;
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
  const fit = (budget: number): { visible: TranscriptEntry[]; above: number } => {
    const visible: TranscriptEntry[] = [];
    let used = 0;
    let i = end - 1;
    for (; i >= 0; i--) {
      const h = heightOf(entries[i], columns);
      // The last entry always renders, even if it alone overflows — a window
      // that fits nothing would be a blank screen.
      if (used + h > budget && visible.length > 0) break;
      used += h;
      visible.unshift(entries[i]);
    }
    return { visible, above: i + 1 };
  };
  const first = fit(rows);
  // The "↑ N earlier messages" hint occupies a row of the same box, so when it
  // is going to render the window has one row less to work with. Re-fitting
  // can only push MORE entries above the fold, so this settles in one pass.
  return first.above > 0 ? fit(Math.max(1, rows - 1)) : first;
}

export function Transcript({ entries, working, viewport, gate }: TranscriptProps): React.JSX.Element {
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
          <Row key={e.key} e={e} columns={viewport.columns} gate={gate} />
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
  // A waiting row changes when the task moves on, so it is not settled yet.
  const moving = (e: TranscriptEntry): boolean => e.pending === true || e.inputRequired === true || e.authRequired === true;
  const settled = entries.filter((e) => !moving(e));
  const live = entries.filter(moving);
  return (
    <Box flexDirection="column">
      <Static items={settled}>{(e) => <Row key={e.key} e={e} />}</Static>
      {live.map((e) => (
        <Row key={e.key} e={e} gate={gate} />
      ))}
      {working ? (
        <Text color={theme.accent}>
          {SPINNER[working.frame % SPINNER.length]} {working.text}
        </Text>
      ) : null}
    </Box>
  );
}
