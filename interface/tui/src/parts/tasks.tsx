// SPDX-License-Identifier: Apache-2.0
/** The tasks screen: every task the principal may see, selectable, cancelable. */
import React from 'react';
import { Box, Text } from 'ink';
import type { TaskView } from '@agentd/client';
import { ago, shortId, stateLabel, theme } from '../theme.js';

export function TaskList({
  tasks,
  selected,
}: {
  tasks: TaskView[];
  selected: number;
}): React.JSX.Element {
  if (tasks.length === 0) {
    return <Text color={theme.dim}>no tasks yet — send a message or run a workflow</Text>;
  }
  const rows = tasks.slice(0, 20);
  return (
    <Box flexDirection="column">
      <Text color={theme.dim} bold>
        {'  id           state        link              updated'}
      </Text>
      {rows.map((t, i) => {
        const st = stateLabel(t.state);
        const link = t.link
          ? 'run' in t.link
            ? `run ${shortId(t.link.run.id, 12)}`
            : 'subagent' in t.link
              ? `sub ${shortId(t.link.subagent.handle, 12)}`
              : `turn ${shortId((t.link as { turn: { ctx: string } }).turn.ctx, 12)}`
          : '';
        return (
          <Box key={t.id} flexDirection="row">
            <Text color={i === selected ? theme.accent : undefined} bold={i === selected}>
              {i === selected ? '▸ ' : '  '}
              {shortId(t.id, 12).padEnd(13)}
            </Text>
            <Text color={st.color}>{st.label.padEnd(13)}</Text>
            <Text color={theme.dim}>{link.padEnd(18)}</Text>
            <Text color={theme.dim}>{ago(t.updated)}</Text>
          </Box>
        );
      })}
      <Text color={theme.dim}>{'\n↑/↓ select · c cancel · enter view result · tab next screen'}</Text>
    </Box>
  );
}
