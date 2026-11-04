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

test('keys that arrive before a re-render all land, and Enter sends them all', async () => {
  const React = (await import('react')).default;
  const { render } = await import('ink-testing-library');
  const { MultilineInput } = await import('../dist/tui/parts/input.js');
  const sent = [];
  function Host() {
    const [s, set] = React.useState({ value: '', cursor: 0 });
    return React.createElement(MultilineInput, {
      value: s.value,
      cursor: s.cursor,
      onChange: set,
      onSubmit: (v) => sent.push(v),
    });
  }
  const r = render(React.createElement(Host));
  await new Promise((ok) => setTimeout(ok, 20));
  // One keystroke per write, with no render between them: what a fast
  // typist or `tmux send-keys` delivers.
  for (const ch of '#01 ok') r.stdin.write(ch);
  r.stdin.write('\r');
  await new Promise((ok) => setTimeout(ok, 50));
  r.unmount();
  assert.deepEqual(sent, ['#01 ok']);
});

test('the ghost fits the cursor row: whole, cut there, or not at all', async () => {
  const { ghostFits } = await import('../dist/tui/parts/input.js');
  assert.equal(ghostFits('/he', 'lp ', 20), 'lp ', 'room for all of it');
  assert.equal(ghostFits('/s', 'ubagents ', 6), 'ubag', 'cut at the row');
  assert.equal(ghostFits('abcdef', 'x', 6), '', 'the line fills its row');
  assert.equal(ghostFits('a long line that wraps', 'xy', 6), '', 'a wrapped line shows none');
  // Only the last line counts: earlier ones are rows of their own.
  assert.equal(ghostFits('a long first line\n/he', 'lp', 6), 'lp');
  assert.equal(ghostFits('/he', 'lp', undefined), '', 'no width to judge by');
  assert.equal(ghostFits('/he', '', 20), '');
  // Columns, not code points: a wide character takes two.
  assert.equal(ghostFits('@', '日本語日本語', 6), '日本', 'wide characters cut by their columns');
  assert.equal(ghostFits('日本語', 'x', 6), '', 'a line of wide characters fills its row');
  assert.equal(ghostFits('@', '🚀🚀🚀', 6), '🚀🚀', 'an emoji takes two columns');
});

test('→ at the end accepts the completion, keys faster than a redraw included', async () => {
  const React = (await import('react')).default;
  const { render } = await import('ink-testing-library');
  const { MultilineInput } = await import('../dist/tui/parts/input.js');
  const sent = [];
  const asked = [];
  function Host() {
    const [s, set] = React.useState({ value: '', cursor: 0 });
    return React.createElement(MultilineInput, {
      value: s.value,
      cursor: s.cursor,
      onChange: set,
      onSubmit: (v) => sent.push(v),
      complete: (v) => {
        asked.push(v);
        return v === '/he' ? '/help ' : null;
      },
    });
  }
  const r = render(React.createElement(Host));
  await new Promise((ok) => setTimeout(ok, 20));
  // No render between them: → completes what the keys left, not the last frame.
  for (const ch of '/he') r.stdin.write(ch);
  r.stdin.write('\u001b[C');
  r.stdin.write('x');
  r.stdin.write('\r');
  await new Promise((ok) => setTimeout(ok, 50));
  r.unmount();
  assert.deepEqual(asked, ['/he']);
  assert.deepEqual(sent, ['/help x']);
});

test('→ before the end moves the cursor and completes nothing', async () => {
  const React = (await import('react')).default;
  const { render } = await import('ink-testing-library');
  const { MultilineInput } = await import('../dist/tui/parts/input.js');
  const sent = [];
  let asked = 0;
  function Host() {
    const [s, set] = React.useState({ value: '', cursor: 0 });
    return React.createElement(MultilineInput, {
      value: s.value,
      cursor: s.cursor,
      onChange: set,
      onSubmit: (v) => sent.push(v),
      complete: () => {
        asked += 1;
        return '/help ';
      },
    });
  }
  const r = render(React.createElement(Host));
  await new Promise((ok) => setTimeout(ok, 20));
  for (const ch of 'abc') r.stdin.write(ch);
  r.stdin.write('\u001b[D');
  r.stdin.write('\u001b[D');
  r.stdin.write('\u001b[C');
  r.stdin.write('X');
  r.stdin.write('\r');
  await new Promise((ok) => setTimeout(ok, 50));
  r.unmount();
  assert.equal(asked, 0, 'mid-line, → is a move');
  assert.deepEqual(sent, ['abXc']);
});
