// SPDX-License-Identifier: AGPL-3.0-only
/**
 * What the composer offers while a task in this conversation waits.
 *
 * An INPUT_REQUIRED gate with a schema is a numbered list: a terminal has no
 * radio buttons, but it has numbers — and a number is faster than typing an
 * option's wording. `1`–`9` pick, `enter` submits, and the composer still
 * takes free text for anything the form cannot express.
 *
 * An AUTH_REQUIRED task is NOT answerable here. It waits for a credential
 * given out of band (A2A 1.0 §4.1.3), so there is nothing to pick and a
 * reply would only start a new message; the prompt says so instead of
 * offering an answer the task cannot take.
 */
import React from 'react';
import { Box, Text } from 'ink';
import type { AskForm, TaskState } from '../../client/index.js';
import { theme } from '../theme.js';

/** The options a form offers, as the rows the number keys pick from. */
export function gateOptions(form: AskForm): string[] {
  const options = form.kind === 'bool' ? ['yes', 'no'] : form.kind === 'one' || form.kind === 'many' ? form.options : [];
  return (form.kind === 'one' || form.kind === 'many') && form.other ? [...options, '__other__'] : options;
}

/** Whether a waiting task takes an answer from the composer at all. */
export function answerable(state: TaskState): boolean {
  return state === 'TASK_STATE_INPUT_REQUIRED';
}

/** The rows {@link GatePrompt} occupies, so the transcript leaves them free. */
export function gateRows(state: TaskState, form: AskForm, picked: readonly string[]): number {
  if (!answerable(state)) return 1;
  if (form.kind === 'text') return 0;
  return gateOptions(form).length + 1 + (picked.includes('__other__') ? 1 : 0);
}

export function GatePrompt({
  state,
  form,
  picked,
  other,
}: {
  /** The waiting task's state. */
  state: TaskState;
  form: AskForm;
  picked: string[];
  /** The free-text answer being typed, when `other…` is selected. */
  other: string;
}): React.JSX.Element | null {
  if (!answerable(state)) {
    return (
      // One row, always: the layout budgets exactly one for it.
      <Text color={theme.warn} wrap="truncate-end">
        {'  waiting for authorization given outside the conversation — a reply cannot answer it'}
      </Text>
    );
  }
  if (form.kind === 'text') return null;
  const multi = form.kind === 'many';
  const rows = gateOptions(form);

  return (
    <Box flexDirection="column">
      {rows.map((key, i) => {
        const on = picked.includes(key);
        return (
          <Text key={key} color={on ? theme.accent : undefined}>
            {'  '}
            <Text color={theme.dim}>{i + 1}</Text>
            {/* A filled marker reads as chosen without the colour being the
                only signal — terminals vary, and some people cannot see it. */}
            {on ? (multi ? ' [x] ' : ' (•) ') : multi ? ' [ ] ' : ' ( ) '}
            {key === '__other__' ? 'other…' : key}
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
