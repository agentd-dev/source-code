// SPDX-License-Identifier: AGPL-3.0-only
/** The one-line status bar: connection, counters, hints. Pure projection. */
import React from 'react';
import { Box, Text } from 'ink';
import type { MirrorState } from '@agentd/client';
import { theme } from '../theme.js';

function connDot(s: MirrorState): { dot: string; color: string; label: string } {
  switch (s.conn) {
    case 'ready':
      return { dot: '●', color: theme.accent, label: 'live' };
    case 'polling':
      return { dot: '◐', color: theme.warn, label: 'polling' };
    case 'connecting':
      return { dot: '○', color: theme.dim, label: 'connecting' };
    case 'error':
      return { dot: '✗', color: theme.error, label: s.error ?? 'error' };
    default:
      return { dot: '○', color: theme.dim, label: 'closed' };
  }
}

export function StatusBar({
  s,
  endpoint,
  screen,
  active,
}: {
  s: MirrorState;
  endpoint: string;
  screen: string;
  active: number;
}): React.JSX.Element {
  const c = connDot(s);
  const counters = ((s.status ?? s.bootstrap) as { counters?: { tokens_in?: number; tokens_out?: number; turns?: number } } | undefined)
    ?.counters;
  return (
    <Box flexDirection="row" gap={1}>
      <Text color={c.color}>
        {c.dot} {c.label}
      </Text>
      <Text color={theme.dim}>{endpoint}</Text>
      {s.draining ? <Text color={theme.warn}>DRAINING</Text> : null}
      {active > 0 ? <Text color={theme.accent}>{active} active</Text> : null}
      {counters ? (
        <Text color={theme.dim}>
          {counters.turns ?? 0} turns · {counters.tokens_in ?? 0}/{counters.tokens_out ?? 0} tok
        </Text>
      ) : null}
      <Text color={theme.dim}>[{screen}]</Text>
      <Text color={theme.dim}>tab:screens esc:cancel /:cmd ^c:quit</Text>
    </Box>
  );
}
