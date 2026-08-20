// SPDX-License-Identifier: AGPL-3.0-only
/**
 * A gate's options, as a numbered list.
 *
 * A terminal has no radio buttons, but it has numbers — and a number is faster
 * than typing an option's wording, which is what the person had to do before
 * the schema travelled with the gate. `1`–`9` pick, `enter` submits, and the
 * composer still takes free text for anything the form cannot express.
 */
import React from 'react';
import { Box, Text } from 'ink';
import type { AskForm } from '../../client/index.js';
import { theme } from '../theme.js';

export function GatePrompt({
  form,
  picked,
  other,
}: {
  form: AskForm;
  picked: string[];
  /** The free-text answer being typed, when `other…` is selected. */
  other: string;
}): React.JSX.Element | null {
  if (form.kind === 'text') return null;
  const options =
    form.kind === 'bool'
      ? ['yes', 'no']
      : form.kind === 'one' || form.kind === 'many'
        ? form.options
        : [];
  const allowOther = (form.kind === 'one' || form.kind === 'many') && form.other;
  const multi = form.kind === 'many';
  const rows = allowOther ? [...options, 'other…'] : options;

  return (
    <Box flexDirection="column">
      {rows.map((o, i) => {
        const key = o === 'other…' ? '__other__' : o;
        const on = picked.includes(key);
        return (
          <Text key={o} color={on ? theme.accent : undefined}>
            {'  '}
            <Text color={theme.dim}>{i + 1}</Text>
            {/* A filled marker reads as chosen without the colour being the
                only signal — terminals vary, and some people cannot see it. */}
            {on ? (multi ? ' [x] ' : ' (•) ') : multi ? ' [ ] ' : ' ( ) '}
            {o}
          </Text>
        );
      })}
      {picked.includes('__other__') ? (
        <Text>
          {'  '}
          <Text color={theme.dim}>your answer: </Text>
          {other}
          <Text color={theme.accent}>▌</Text>
        </Text>
      ) : null}
      <Text color={theme.dim}>
        {'  '}
        {multi ? '1–9 toggle · enter answers' : '1–9 pick · enter answers'}
        {' · or just type a reply'}
      </Text>
    </Box>
  );
}
