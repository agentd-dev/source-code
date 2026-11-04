// SPDX-License-Identifier: AGPL-3.0-only
// The composer's completion (ghost text, → to accept) and the subagents
// detail view's message field and log pane, rendered by the real App over a
// Mirror and a stand-in client.
import test from 'node:test';
import assert from 'node:assert/strict';
import React from 'react';
import { render } from 'ink-testing-library';
import { COMMAND_EXTENSION, Mirror, OPS, capabilitiesOf } from '../dist/client/index.js';
import { App } from '../dist/tui/app.js';
import { SubagentLog, compactLogLine } from '../dist/tui/parts/subagents.js';

const tick = (ms = 40) => new Promise((r) => setTimeout(r, ms));
const RIGHT = '\u001b[C';
const UP = '\u001b[A';
const ESC = '\u001b';
// eslint-disable-next-line no-control-regex
const plain = (s) => s.replace(/\u001b\[[0-9;?]*[A-Za-z]/g, '');

/** A session whose card lists `ops` for this caller. */
function session(ops = []) {
  const card = {
    name: 'agentd',
    capabilities: { extensions: [{ uri: COMMAND_EXTENSION, params: { ops: ops.map((op) => ({ op, reply: 'message' })) } }] },
  };
  return { cardUrl: 'http://127.0.0.1:1/.well-known/agent-card.json', card, extended: null, ep: { url: 'http://127.0.0.1:1/' }, caps: capabilitiesOf(card, null), warnings: [] };
}

function boot(client = {}, ops = []) {
  const mirror = new Mirror();
  const ui = render(React.createElement(App, { configured: 'http://127.0.0.1:1', client, mirror, observe: false }));
  mirror.setSession(session(ops));
  mirror.setConn('ready');
  return { mirror, ui };
}

/** Tab to the subagents screen, and open the first one's detail. */
async function openDetail(ui) {
  ui.stdin.write('\t');
  await tick();
  ui.stdin.write('\t');
  await tick();
  ui.stdin.write('\r');
  await tick();
}

const sub = (data) => ({ seq: 1, ts: 10, kind: 'subagent', data: { mode: 'warm', status: 'running', tokens: 0, error: null, updated: 1000, template: null, tier: 'flat', reports_to: null, ...data } });

// ---- autocomplete ------------------------------------------------------------

test('the composer shows the suggestion ahead of the cursor, and → takes it', async () => {
  const { ui } = boot();
  await tick();
  for (const ch of '/he') ui.stdin.write(ch);
  await tick();
  assert.match(ui.lastFrame(), /› \/help/, 'the ghost completes the word after the cursor');
  ui.stdin.write(RIGHT);
  await tick();
  ui.stdin.write('\r');
  await tick();
  assert.match(ui.lastFrame(), /· \/help · \/new/, '/help ran: → accepted the suggestion');
  ui.unmount();
});

test('the ghost is not sent: Enter sends what was typed', async () => {
  const { ui } = boot();
  await tick();
  for (const ch of '/he') ui.stdin.write(ch);
  await tick();
  ui.stdin.write('\r');
  await tick();
  assert.match(ui.lastFrame(), /unknown command \/he\b/);
  ui.unmount();
});

test('→ mid-line moves the cursor, and with no suggestion → at the end does nothing', async () => {
  const { ui } = boot();
  await tick();
  for (const ch of '/hex') ui.stdin.write(ch);
  await tick();
  // No command starts "hex": nothing is offered, → at the end is a no-op.
  ui.stdin.write(RIGHT);
  await tick();
  ui.stdin.write('\u001b[D');
  ui.stdin.write('\u001b[D');
  await tick();
  ui.stdin.write(RIGHT);
  await tick();
  ui.stdin.write('Y');
  await tick();
  ui.stdin.write('\r');
  await tick();
  assert.match(ui.lastFrame(), /unknown command \/heYx\b/);
  ui.unmount();
});

// A narrow pane, with Ink's real renderer (the testing library lays out at
// 100 columns, which cannot show a row that wraps): the ghost is cut at the
// cursor's row, and the composer keeps its one row.
test('in a narrow pane the ghost is cut to its row and the composer stays one row', async () => {
  const { render: inkRender } = await import('ink');
  const { PassThrough } = await import('node:stream');
  const cols = 12;
  const stdout = new PassThrough();
  Object.assign(stdout, { isTTY: true, columns: cols, rows: 20 });
  let screen = '';
  stdout.on('data', (d) => (screen += d.toString()));
  const stdin = new PassThrough();
  Object.assign(stdin, { isTTY: true, setRawMode() {}, ref() {}, unref() {} });
  const mirror = new Mirror();
  const app = inkRender(React.createElement(App, { configured: 'http://127.0.0.1:1', client: {}, mirror, observe: false }), {
    stdout,
    stdin,
    debug: true,
    exitOnCtrlC: false,
    patchConsole: false,
  });
  try {
    mirror.setSession(session());
    mirror.setConn('ready');
    await tick();
    stdin.write('/s');
    await tick(80);
    const frames = plain(screen).split('╭');
    const last = ('╭' + frames[frames.length - 1]).split('\n');
    const bottom = last.findIndex((l) => l.includes('╰'));
    const inside = last.slice(1, bottom);
    assert.equal(inside.length, 1, `the composer is one row: ${JSON.stringify(inside)}`);
    assert.match(inside[0], /^│.*│$/);
    // 6 columns for the text: "/s" and four of "ubagents ".
    assert.match(inside[0], /\/subag(?!e)/, JSON.stringify(inside[0]));
  } finally {
    app.unmount();
  }
});

// ---- messaging a subagent from its detail view --------------------------------

test('a warm worker is messaged from its detail view, and its reply is shown there', async () => {
  const sent = [];
  let result = null;
  const client = {
    subagentGet: async () => ({}),
    subagentStatus: async (handle) => ({ handle, status: 'running', mode: 'warm', result }),
    subagentSend: async (handle, message) => {
      sent.push([handle, message]);
      setTimeout(() => (result = 'pong from the worker'), 100);
      return { ok: true, handle };
    },
  };
  const { mirror, ui } = boot(client);
  mirror.apply(sub({ handle: 'sub-w' }));
  await openDetail(ui);
  assert.match(ui.lastFrame(), /subagent sub-w/);
  assert.match(ui.lastFrame(), /m\/enter message/);
  ui.stdin.write('\r');
  await tick();
  assert.match(ui.lastFrame(), /message sub-w/, 'the field opened in the detail view');
  for (const ch of 'ping') ui.stdin.write(ch);
  await tick();
  ui.stdin.write('\r');
  await tick(100);
  assert.deepEqual(sent, [['sub-w', 'ping']]);
  let frame = ui.lastFrame();
  assert.match(frame, /you › ping/);
  assert.match(frame, /waiting for sub-w's reply/);
  assert.match(frame, /subagent sub-w/, 'still the detail view, not the chat');
  await tick(1700);
  frame = ui.lastFrame();
  assert.match(frame, /sub-w › pong from the worker/);
  assert.doesNotMatch(frame, /waiting for/);
  // Esc leaves the field; the exchange stays.
  ui.stdin.write(ESC);
  await tick();
  frame = ui.lastFrame();
  assert.doesNotMatch(frame, /message sub-w/);
  assert.match(frame, /you › ping/);
  ui.unmount();
});

test("an instance child's answer is its reply, not its own result", async () => {
  let reply;
  const client = {
    subagentGet: async () => ({}),
    // Its `result` is the child's own report, and does not move.
    subagentStatus: async (handle) => ({ handle, status: 'running', mode: 'detached', result: { status: 'completed', output: 'booted' }, ...(reply ? { reply } : {}) }),
    subagentSend: async () => {
      setTimeout(() => (reply = 'the room is listening'), 100);
      return { ok: true, delivery: 'async' };
    },
  };
  const { mirror, ui } = boot(client);
  mirror.apply(sub({ handle: 'inst-9', mode: 'detached', tier: 'instance' }));
  await openDetail(ui);
  ui.stdin.write('m');
  await tick();
  for (const ch of 'anyone there?') ui.stdin.write(ch);
  ui.stdin.write('\r');
  await tick(1800);
  const frame = ui.lastFrame();
  assert.match(frame, /inst-9 › the room is listening/);
  assert.doesNotMatch(frame, /booted/);
  ui.unmount();
});

test('a child that cannot take a message says so in the field, and nothing is sent', async () => {
  const sent = [];
  const client = { subagentGet: async () => ({}), subagentStatus: async () => ({}), subagentSend: async (...a) => sent.push(a) };
  const { mirror, ui } = boot(client);
  mirror.apply(sub({ handle: 'sub-s', mode: 'sync' }));
  await openDetail(ui);
  ui.stdin.write('m');
  await tick();
  assert.match(ui.lastFrame(), /not a warm subagent \(sync\)/);
  for (const ch of 'hi') ui.stdin.write(ch);
  ui.stdin.write('\r');
  await tick();
  assert.deepEqual(sent, []);
  ui.unmount();
});

test('a send the daemon refuses is said plainly, with no stack', async () => {
  const client = {
    subagentGet: async () => ({}),
    subagentStatus: async () => ({}),
    subagentSend: async () => {
      throw new Error('instance "inst-1" is not running yet: its respawn waits for 4242 to exit');
    },
  };
  const { mirror, ui } = boot(client);
  mirror.apply(sub({ handle: 'inst-1', mode: 'detached', tier: 'instance' }));
  await openDetail(ui);
  ui.stdin.write('m');
  await tick();
  for (const ch of 'hello') ui.stdin.write(ch);
  ui.stdin.write('\r');
  await tick(80);
  const frame = ui.lastFrame();
  assert.match(frame, /not sent: instance "inst-1" is not running yet/);
  assert.doesNotMatch(frame, /\bat .*\.js:\d+/, 'no stack');
  ui.unmount();
});

test('/send in chat still messages a subagent', async () => {
  const sent = [];
  const client = { subagentSend: async (h, m) => sent.push([h, m]) };
  const { ui } = boot(client, [OPS.subagentSend]);
  await tick();
  for (const ch of '/send sub-w left please') ui.stdin.write(ch);
  ui.stdin.write('\r');
  await tick(60);
  assert.deepEqual(sent, [['sub-w', 'left please']]);
  assert.match(ui.lastFrame(), /sent to sub-w/);
  ui.unmount();
});

// ---- a subagent's log --------------------------------------------------------

const line = (event, fields = {}, level = 'info', s = 12) =>
  JSON.stringify({ ts: `2026-11-04T10:11:${String(s).padStart(2, '0')}.345Z`, level, event, run_id: 'r', agent_id: 'a', agent_path: '0', comp: 'supervisor', pid: 7, ...fields });

test('l opens the log pane: the tail, then what is written since, read by the cursor', async () => {
  const calls = [];
  const client = {
    subagentGet: async () => ({}),
    subagentLog: async (handle, lines, after) => {
      calls.push([handle, lines, after]);
      if (after === undefined) return { handle, source: 'file', lines: [line('proc.start', { version: '1.24.0' }), line('a2a.listen', { addr: 'unix:///s' })], cursor: 100 };
      if (after === 100) return { handle, source: 'file', lines: [line('loop.step', { step: 1 }, 'warn', 13)], cursor: 160 };
      return { handle, source: 'file', lines: [], cursor: after };
    },
  };
  const { mirror, ui } = boot(client, [OPS.subagentLog]);
  mirror.apply(sub({ handle: 'inst-2', tier: 'instance' }));
  await openDetail(ui);
  assert.match(ui.lastFrame(), /l log/);
  ui.stdin.write('l');
  await tick(80);
  let frame = ui.lastFrame();
  assert.match(frame, /log inst-2/);
  assert.match(frame, /10:11:12 info +proc\.start version=1\.24\.0/);
  assert.match(frame, /a2a\.listen addr=unix:\/\/\/s/);
  assert.doesNotMatch(frame, /run_id|agent_path|pid=/, 'the correlation fields are left out');
  await tick(1600);
  frame = ui.lastFrame();
  assert.match(frame, /10:11:13 warn +loop\.step step=1/, 'followed with the cursor');
  assert.deepEqual(calls.slice(0, 2), [
    ['inst-2', 200, undefined],
    ['inst-2', 200, 100],
  ]);
  // Scrolled up, the pane holds still and says what is below.
  ui.stdin.write(UP);
  await tick();
  assert.match(ui.lastFrame(), /1 newer below/);
  // Esc closes the pane, back to the detail.
  ui.stdin.write(ESC);
  await tick();
  frame = ui.lastFrame();
  assert.doesNotMatch(frame, /log inst-2/);
  assert.match(frame, /subagent inst-2/);
  ui.unmount();
});

test('l is offered only when the card lists subagent.log', async () => {
  let asked = 0;
  const client = { subagentGet: async () => ({}), subagentLog: async () => (asked++, { lines: [], cursor: 0 }) };
  const { mirror, ui } = boot(client, []);
  mirror.apply(sub({ handle: 'sub-q' }));
  await openDetail(ui);
  assert.doesNotMatch(ui.lastFrame(), /l log/);
  ui.stdin.write('l');
  await tick(60);
  assert.equal(asked, 0);
  assert.doesNotMatch(ui.lastFrame(), /log sub-q/);
  ui.unmount();
});

test('a log line renders compact and inert', () => {
  const l = compactLogLine(line('tool.call', { tool: 'echo', err: 'bad\u001b]0;owned\u0007' }, 'error'));
  assert.equal(l.time, '10:11:12');
  assert.equal(l.level, 'error');
  assert.equal(l.event, 'tool.call');
  assert.equal(l.rest, 'tool=echo err=bad\\u001b]0;owned\\u0007');
  const raw = compactLogLine('not json \u001b[2J');
  assert.equal(raw.rest, 'not json \\u001b[2J');
  assert.equal(raw.event, '');
});

// The pane in a narrow terminal: one row per line, cut at the width, and as
// many rows as it was given — a log line that wrapped would push the frame.
test('the log pane keeps to its width and its rows in a narrow pane', async () => {
  const { render: inkRender, Box } = await import('ink');
  const { PassThrough } = await import('node:stream');
  const cols = 30;
  const stdout = new PassThrough();
  Object.assign(stdout, { isTTY: true, columns: cols, rows: 20 });
  let screen = '';
  stdout.on('data', (d) => (screen += d.toString()));
  const lines = Array.from({ length: 12 }, (_, i) => line(`ev${i}`, { note: 'a long field value that would wrap '.repeat(3) }));
  const app = inkRender(
    React.createElement(Box, { flexDirection: 'column' }, React.createElement(SubagentLog, { handle: 'inst-3', lines, scroll: 0, rows: 8, width: cols })),
    { stdout, debug: true, exitOnCtrlC: false, patchConsole: false },
  );
  try {
    await tick(60);
    const frame = plain(screen).split('log inst-3');
    const rows = ('log inst-3' + frame[frame.length - 1]).replace(/\n+$/, '').split('\n');
    assert.equal(rows.length, 8, JSON.stringify(rows));
    for (const r of rows) assert.ok([...r].length <= cols, `${JSON.stringify(r)} is wider than the pane`);
    assert.match(rows[rows.length - 2], /ev11 /, 'the newest line is last');
  } finally {
    app.unmount();
  }
});
