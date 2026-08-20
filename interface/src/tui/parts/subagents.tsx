// SPDX-License-Identifier: Apache-2.0
/**
 * The subagents screen: the live list (feed-driven), a selectable row, and a
 * drill-down detail view (`subagent.get` — instruction, result, attempts) with
 * a way back. Details need `interface.debug`; without it the list still shows
 * the summary the feed carries.
 */
import React from 'react';
import { Box, Text } from 'ink';
import type { Json, MirrorState } from '../../client/index.js';
import { ago, theme } from '../theme.js';

/** A fixed-width column: pad short, TRUNCATE long.
 *
 * `padEnd` alone only pads — a value wider than its column overflows into the
 * next one, which is how `supervised` + `running` rendered as
 * `supervisedrunning`. A terminal table has no wrapping to save it, so the
 * column has to enforce its own width, and always leave one space so adjacent
 * values cannot touch even at exactly the limit. */
function col(v: unknown, width: number): string {
  const s = String(v ?? '');
  const w = width - 1;
  return (s.length > w ? `${s.slice(0, Math.max(1, w - 1))}…` : s).padEnd(width);
}

function statusColor(status: string): string {
  switch (status) {
    case 'running':
    case 'spawning':
      return theme.accent;
    case 'completed':
      return theme.agent;
    case 'failed':
    case 'crashed':
    case 'killed':
      return theme.error;
    default:
      return theme.dim;
  }
}

export function SubagentList({
  s,
  selected,
}: {
  s: MirrorState;
  selected: number;
}): React.JSX.Element {
  const subs = [...s.subagents.values()] as { [k: string]: Json }[];
  if (subs.length === 0) {
    return <Text color={theme.dim}>no subagents yet — the agent spawns them as it delegates</Text>;
  }
  return (
    <Box flexDirection="column">
      <Text color={theme.dim} bold>
        {`  ${col('handle', 21)}${col('mode', 12)}${col('status', 12)}${col('tokens', 9)}updated`}
      </Text>
      {subs.slice(0, 20).map((x, i) => {
        const status = String(x.status ?? '');
        return (
          <Box key={String(x.handle ?? i)} flexDirection="row">
            <Text color={i === selected ? theme.accent : undefined} bold={i === selected}>
              {i === selected ? '▸ ' : '  '}
              {col(x.handle, 21)}
            </Text>
            <Text color={theme.dim}>{col(x.mode, 12)}</Text>
            <Text color={statusColor(status)}>{col(status, 12)}</Text>
            <Text color={theme.dim}>{col(x.tokens ?? 0, 9)}</Text>
            <Text color={theme.dim}>{x.updated ? ago(Number(x.updated)) : ''}</Text>
          </Box>
        );
      })}
      <Text color={theme.dim}>{'\n↑/↓ select · enter details · tab next screen'}</Text>
    </Box>
  );
}

export function SubagentDetail({
  handle,
  summary,
  detail,
  debug,
  killAsk = false,
}: {
  handle: string;
  summary: { [k: string]: Json } | undefined;
  detail: { [k: string]: Json } | null;
  debug: boolean;
  /** Waiting for confirmation that this subagent should be stopped. */
  killAsk?: boolean;
}): React.JSX.Element {
  const d = detail ?? summary ?? {};
  const line = (label: string, v: Json | undefined, color?: string) =>
    v === undefined || v === null ? null : (
      <Box key={label} flexDirection="row">
        <Text color={theme.dim}>{label.padEnd(14)}</Text>
        <Text color={color} wrap="wrap">
          {typeof v === 'string' ? v : JSON.stringify(v)}
        </Text>
      </Box>
    );
  return (
    <Box flexDirection="column">
      <Text color={theme.accent} bold>
        subagent {handle}
      </Text>
      {line('status', d.status, statusColor(String(d.status ?? '')))}
      {!['running', 'spawning'].includes(String(d.status ?? '')) ? (
        <Text color={theme.dim}>{'(not running — messaging and stopping apply to a live subagent)'}</Text>
      ) : null}
      {line('mode', d.mode)}
      {line('attempt', d.attempt)}
      {line('tokens', d.tokens)}
      {line('instruction', d.instruction)}
      {line('result', d.result)}
      {line('error', d.error, theme.error)}
      {line('requested_by', d.requested_by)}
      {!debug ? (
        <Text color={theme.dim}>
          (summary only — enable interface.debug, or /set interface.debug true, for instruction/result)
        </Text>
      ) : null}
      {killAsk ? (
        <Text color={theme.error} bold>
          {'\nstop '}
          {handle}
          {'? y to confirm, any other key to cancel'}
        </Text>
      ) : (
        <Text color={theme.dim}>
          {'\nm message · k stop · esc/backspace back · tab next screen'}
        </Text>
      )}
    </Box>
  );
}
