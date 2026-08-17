// SPDX-License-Identifier: AGPL-3.0-only
// The composer's multiline editing rules. All of the behaviour lives in the
// pure `applyKey` reducer, so this exercises the real key semantics without a
// terminal — the thing that is otherwise only testable by hand.
import test from 'node:test';
import assert from 'node:assert/strict';
import { applyKey } from '../dist/tui/parts/input.js';

const st = (value, cursor = value.length) => ({ value, cursor });
const type = (s, text) => text.split('').reduce((acc, ch) => applyKey(acc, ch, {}).state, s);

test('typing, cursor movement and backspace edit the buffer', () => {
  let s = type(st(''), 'hello');
  assert.deepEqual(s, { value: 'hello', cursor: 5 });
  // ← ← then a character inserts at the cursor, not at the end.
  s = applyKey(s, '', { leftArrow: true }).state;
  s = applyKey(s, '', { leftArrow: true }).state;
  s = applyKey(s, 'X', {}).state;
  assert.equal(s.value, 'helXlo');
  assert.equal(s.cursor, 4);
  // Backspace removes the character before the cursor.
  s = applyKey(s, '', { backspace: true }).state;
  assert.equal(s.value, 'hello');
  // Backspace at the start is a no-op, never a crash.
  const start = applyKey(st('abc', 0), '', { backspace: true });
  assert.deepEqual(start.state, { value: 'abc', cursor: 0 });
  // → stops at the end.
  assert.equal(applyKey(st('ab', 2), '', { rightArrow: true }).state.cursor, 2);
});

test('Enter submits — and the three newline idioms do not', () => {
  // Plain Enter submits the buffer.
  const submitted = applyKey(st('ship it'), '', { return: true });
  assert.equal(submitted.submit, true);
  assert.equal(submitted.state.value, 'ship it');

  // Alt/Option+Enter inserts a newline instead.
  const alt = applyKey(st('line one'), '', { return: true, meta: true });
  assert.ok(!alt.submit);
  assert.equal(alt.state.value, 'line one\n');

  // Ctrl+J arrives as a bare "\n" with no `return` flag: a newline.
  const ctrlJ = applyKey(st('line one'), '\n', {});
  assert.ok(!ctrlJ.submit);
  assert.equal(ctrlJ.state.value, 'line one\n');

  // A trailing backslash continues the line: the `\` is consumed.
  const cont = applyKey(st('line one\\'), '', { return: true });
  assert.ok(!cont.submit);
  assert.equal(cont.state.value, 'line one\n');
  assert.equal(cont.state.cursor, 9);

  // A backslash elsewhere in the text does not defeat submitting.
  const midway = applyKey(st('a\\b'), '', { return: true });
  assert.equal(midway.submit, true);
});

test('a pasted block keeps its newlines instead of submitting on the first', () => {
  const pasted = applyKey(st(''), 'first\r\nsecond\rthird\nfourth', {});
  assert.ok(!pasted.submit, 'a paste never submits');
  assert.equal(pasted.state.value, 'first\nsecond\nthird\nfourth', 'CRLF and CR normalize to LF');
  assert.equal(pasted.state.cursor, pasted.state.value.length);
  // Pasting into the middle of existing text splices it in.
  const spliced = applyKey(st('ab', 1), 'X\nY', {});
  assert.equal(spliced.state.value, 'aX\nYb');
});

test('↑/↓ move between lines, and fall through to history at the edges', () => {
  const buf = 'alpha\nbravo\ncharlie';
  // From the middle line, ↑ keeps the column.
  const up = applyKey(st(buf, 8), '', { upArrow: true }); // col 2 of "bravo"
  assert.equal(up.state.cursor, 2, 'same column on the previous line');
  assert.ok(!up.history);
  // ↓ from the middle line, likewise.
  const down = applyKey(st(buf, 8), '', { downArrow: true });
  assert.equal(down.state.cursor, 14, 'same column on the next line');
  // A shorter target line clamps to its end rather than overshooting.
  const clamp = applyKey(st('a\nlonger', 7), '', { upArrow: true });
  assert.equal(clamp.state.cursor, 1, 'clamped to the end of the short line');
  // On the first line ↑ is history; on the last line ↓ is history.
  assert.equal(applyKey(st(buf, 2), '', { upArrow: true }).history, 'prev');
  assert.equal(applyKey(st(buf, 15), '', { downArrow: true }).history, 'next');
  // A single-line buffer: both edges are history.
  assert.equal(applyKey(st('one', 1), '', { upArrow: true }).history, 'prev');
  assert.equal(applyKey(st('one', 1), '', { downArrow: true }).history, 'next');
});

test('control chords and empty input never corrupt the buffer', () => {
  const s = st('keep me');
  assert.deepEqual(applyKey(s, 'c', { ctrl: true }).state, s, 'Ctrl+C is the app’s, not text');
  assert.deepEqual(applyKey(s, '', {}).state, s);
  // A cursor out of range is clamped rather than producing undefined slices.
  const wild = applyKey({ value: 'abc', cursor: 99 }, 'd', {});
  assert.equal(wild.state.value, 'abcd');
});
