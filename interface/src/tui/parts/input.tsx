// SPDX-License-Identifier: AGPL-3.0-only
/**
 * The composer's text field — **multiline**.
 *
 * `ink-text-input` is single-line: a pasted paragraph collapses and there is
 * no way to type a second line, which makes the TUI useless for the prompts
 * people actually write. This replaces it with a small editor whose entire
 * behaviour lives in the pure {@link applyKey} reducer, so the key semantics
 * are unit-tested without a terminal.
 *
 * How a newline is entered (all three, because terminals disagree):
 *   * **Ctrl+J** — the one that works everywhere: the terminal sends `\n`
 *     while plain Enter sends `\r`, so they are distinguishable at the tty.
 *   * **Alt/Option+Enter** — ESC-prefixed Enter (`key.meta`), what most
 *     terminals send and what other agent CLIs bind.
 *   * **a trailing `\`** — the shell idiom; typing Enter after a backslash
 *     continues the line instead of submitting.
 * Shift+Enter is deliberately absent: a terminal cannot distinguish it from
 * Enter without protocol extensions the emulator may not have enabled.
 *
 * Pasting is handled too — a chunk arriving with embedded newlines is
 * inserted verbatim rather than submitting on the first one.
 */
import React from 'react';
import { Text, useInput } from 'ink';
import { theme } from '../theme.js';

/** The editor's state: the buffer, and the cursor's index into it. */
export interface EditState {
  value: string;
  cursor: number;
}

/** The subset of Ink's key object the reducer cares about. */
export interface EditKey {
  return?: boolean;
  meta?: boolean;
  ctrl?: boolean;
  backspace?: boolean;
  delete?: boolean;
  leftArrow?: boolean;
  rightArrow?: boolean;
  upArrow?: boolean;
  downArrow?: boolean;
}

/** What the reducer decided: the new state, and whether to submit it. */
export interface EditResult {
  state: EditState;
  submit?: boolean;
  /** History navigation was requested (nothing to move within the buffer). */
  history?: 'prev' | 'next';
}

const clamp = (n: number, lo: number, hi: number): number => Math.max(lo, Math.min(hi, n));

/** The offset of the start of the line containing `i`. */
function lineStart(v: string, i: number): number {
  return v.lastIndexOf('\n', Math.max(0, i - 1)) + 1;
}
/** The offset of the end of the line containing `i` (the `\n`, or the end). */
function lineEnd(v: string, i: number): number {
  const n = v.indexOf('\n', i);
  return n === -1 ? v.length : n;
}

/** Insert `text` at the cursor. */
function insert(s: EditState, text: string): EditState {
  return {
    value: s.value.slice(0, s.cursor) + text + s.value.slice(s.cursor),
    cursor: s.cursor + text.length,
  };
}

/**
 * Apply one key/paste event. Pure — the component is a thin shell over this.
 */
export function applyKey(s: EditState, input: string, key: EditKey): EditResult {
  const cur = clamp(s.cursor, 0, s.value.length);
  const st: EditState = { value: s.value, cursor: cur };

  // A paste (or any multi-character chunk) carrying newlines: insert it.
  // Normalize CRLF/CR so the buffer only ever holds `\n`.
  if (input && input.length > 1 && /[\r\n]/.test(input)) {
    return { state: insert(st, input.replace(/\r\n?/g, '\n')) };
  }
  // Ctrl+J arrives as a bare `\n` (Enter is `\r` ⇒ key.return): a newline.
  if (input === '\n' && !key.return) return { state: insert(st, '\n') };

  if (key.return) {
    // Alt/Option+Enter → newline.
    if (key.meta) return { state: insert(st, '\n') };
    // Trailing backslash → continuation: drop the `\`, add a newline.
    if (st.value.slice(0, cur).endsWith('\\')) {
      const trimmed: EditState = {
        value: st.value.slice(0, cur - 1) + st.value.slice(cur),
        cursor: cur - 1,
      };
      return { state: insert(trimmed, '\n') };
    }
    return { state: st, submit: true };
  }

  if (key.backspace || key.delete) {
    if (cur === 0) return { state: st };
    return {
      state: { value: st.value.slice(0, cur - 1) + st.value.slice(cur), cursor: cur - 1 },
    };
  }

  if (key.leftArrow) return { state: { ...st, cursor: Math.max(0, cur - 1) } };
  if (key.rightArrow) return { state: { ...st, cursor: Math.min(st.value.length, cur + 1) } };

  // Vertical movement stays inside a multiline buffer; on the first/last line
  // it falls through to the caller as history navigation.
  if (key.upArrow) {
    const start = lineStart(st.value, cur);
    if (start === 0) return { state: st, history: 'prev' };
    const col = cur - start;
    const prevStart = lineStart(st.value, start - 1);
    const prevEnd = start - 1;
    return { state: { ...st, cursor: Math.min(prevStart + col, prevEnd) } };
  }
  if (key.downArrow) {
    const end = lineEnd(st.value, cur);
    if (end === st.value.length) return { state: st, history: 'next' };
    const col = cur - lineStart(st.value, cur);
    const nextStart = end + 1;
    return { state: { ...st, cursor: Math.min(nextStart + col, lineEnd(st.value, nextStart)) } };
  }

  // Ctrl-chords that are not text (Ctrl+C etc. are handled by the app).
  if (key.ctrl || !input) return { state: st };
  return { state: insert(st, input) };
}

export interface MultilineInputProps {
  value: string;
  cursor: number;
  onChange: (s: EditState) => void;
  onSubmit: (value: string) => void;
  onHistory?: (dir: 'prev' | 'next') => void;
  isActive?: boolean;
  /**
   * Let the app own ↑/↓ (it cycles the suggestion list). Ink runs every
   * registered handler, so the editor has to stand down explicitly rather
   * than rely on the app returning early.
   */
  ignoreVertical?: boolean;
  placeholder?: string;
}

/**
 * Render the buffer with a block cursor. Ink has no cursor of its own, so the
 * character under the cursor is drawn inverse (and a trailing cursor becomes
 * an inverse space).
 */
export function MultilineInput({
  value,
  cursor,
  onChange,
  onSubmit,
  onHistory,
  isActive = true,
  ignoreVertical = false,
  placeholder,
}: MultilineInputProps): React.JSX.Element {
  useInput(
    (input, key) => {
      if (ignoreVertical && (key.upArrow || key.downArrow)) return;
      const r = applyKey({ value, cursor }, input, key as EditKey);
      if (r.history && onHistory) {
        onHistory(r.history);
        return;
      }
      if (r.state.value !== value || r.state.cursor !== cursor) onChange(r.state);
      if (r.submit) onSubmit(r.state.value);
    },
    { isActive },
  );

  if (!value && placeholder) {
    return (
      <Text>
        <Text inverse>{placeholder.slice(0, 1)}</Text>
        <Text color={theme.dim}>{placeholder.slice(1)}</Text>
      </Text>
    );
  }

  const at = clamp(cursor, 0, value.length);
  const before = value.slice(0, at);
  const under = value.slice(at, at + 1);
  const after = value.slice(at + 1);
  return (
    <Text wrap="wrap">
      {before}
      <Text inverse>{under === '' || under === '\n' ? ' ' : under}</Text>
      {under === '\n' ? '\n' : ''}
      {after}
    </Text>
  );
}
