// SPDX-License-Identifier: AGPL-3.0-only
/**
 * The tasks screen: every task the principal may see, selectable, cancelable.
 * The `link` column is what task-annotations says the task belongs to — a
 * workflow run, a subagent, or a conversation turn — spelled as the daemon
 * spells the kind, so it can be matched against the other screens and `/…`
 * commands; without the extension the column is empty, not guessed.
 */
import React from 'react';
import { Box, Text } from 'ink';
import type { TaskView } from '../../client/index.js';
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
        {'  id           state        link                   updated'}
      </Text>
      {rows.map((t, i) => {
        const st = stateLabel(t.state);
        const link = t.link ? `${t.link.kind} ${shortId(t.link.id, 12)}` : '';
        return (
          <Box key={t.id} flexDirection="row">
            <Text color={i === selected ? theme.accent : undefined} bold={i === selected}>
              {i === selected ? '▸ ' : '  '}
              {shortId(t.id, 12).padEnd(13)}
            </Text>
            <Text color={st.color}>{st.label.padEnd(13)}</Text>
            <Text color={theme.dim}>{link.padEnd(23)}</Text>
            <Text color={theme.dim}>{ago(t.updated)}</Text>
          </Box>
        );
      })}
      <Text color={theme.dim}>{'\n↑/↓ select · c cancel · enter view result · tab next screen'}</Text>
    </Box>
  );
}
