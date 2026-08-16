// SPDX-License-Identifier: AGPL-3.0-only
/**
 * The conversation view. Settled entries ride Ink's `<Static>` — written once
 * to terminal scrollback, never re-rendered (the flicker rule: the dynamic
 * region stays small) — while the in-flight rows render live below.
 */
import React from 'react';
import { Box, Static, Text } from 'ink';
import type { TranscriptEntry } from '@agentd/client';
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

export interface TranscriptProps {
  entries: TranscriptEntry[];
  /** A live "working" row under the settled history. */
  working?: { text: string; frame: number } | null;
}

export function Transcript({ entries, working }: TranscriptProps): React.JSX.Element {
  // Settled rows go to scrollback; anything still moving stays dynamic.
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
