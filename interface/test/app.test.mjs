// SPDX-License-Identifier: AGPL-3.0-only
// Render tests: the App is a pure projection of the Mirror — drive the mirror
// with daemon-shaped events (no network) and assert the frames.
import test from 'node:test';
import assert from 'node:assert/strict';
import React from 'react';
import { render } from 'ink-testing-library';
import { spawn } from 'node:child_process';
import { fileURLToPath } from 'node:url';
import { COMMAND_EXTENSION, Mirror, capabilitiesOf } from '../dist/client/index.js';
import { App, LAUNCH_SESSION_ENDED, terminalBanner } from '../dist/tui/app.js';
import { startFakeA2a } from './fake-a2a.mjs';

const CLI = fileURLToPath(new URL('../dist/tui/cli.js', import.meta.url));
const tick = (ms = 30) => new Promise((r) => setTimeout(r, ms));
/** An RFC 3339 timestamp `s` seconds into a fixed day. */
const at = (s) => new Date(Date.UTC(2026, 8, 1, 0, 0, s)).toISOString();

/**
 * What discovery settles for a public card declaring the command vocabulary:
 * whether introspection is on is not something that card can tell.
 */
function session(card = { name: 'agentd', capabilities: { extensions: [{ uri: COMMAND_EXTENSION, params: { ops: [] } }] } }) {
  return { cardUrl: 'http://127.0.0.1:1/.well-known/agent-card.json', card, extended: null, ep: { url: 'http://127.0.0.1:1/' }, caps: capabilitiesOf(card, null), warnings: [] };
}

function boot() {
  const mirror = new Mirror();
  // A fake client: the App only calls it on user actions, none here.
  const client = {};
  const ui = render(
    React.createElement(App, {
      configured: 'http://127.0.0.1:1',
      client,
      mirror,
      observe: false,
    }),
  );
  return { mirror, ui };
}

test('renders the connecting state, then daemon identity from the mirror', async () => {
  const { mirror, ui } = boot();
  await tick();
  assert.match(ui.lastFrame(), /connecting/);
  mirror.setSession(session());
  mirror.bootstrap({ version: '9.9.9', instance: 'box-1' });
  mirror.setConn('ready');
  await tick();
  const frame = ui.lastFrame();
  // The name comes from the card, the rest from the status document, in the
  // client's own default order.
  assert.match(frame, /agentd 9\.9\.9 box-1/);
  assert.match(frame, /● live/);
  ui.unmount();
});

test('the chrome is the client layout; the daemon only fills it in', async () => {
  const { mirror, ui } = boot();
  mirror.setConn('ready');
  mirror.setSession(session());
  mirror.bootstrap({ version: '2.1.0', instance: 'box-2', model: 'mock-9', counters: { turns: 2, tokens_in: 11, tokens_out: 5 } });
  await tick();
  let frame = ui.lastFrame();
  assert.match(frame, /agentd 2\.1\.0 box-2/, 'the default top');
  assert.doesNotMatch(frame, /mock-9/, 'model is not in the default layout');
  assert.match(frame, /11\/5 tok/, 'bottom includes tokens');
  assert.match(frame, /tab:screens/, 'and the key hints');
  assert.doesNotMatch(frame, /\bdebug\b/, 'no introspection known yet');
  // The cards cannot tell (no extended card): the feed's hello says the
  // introspection reads are on, so the badge shows.
  mirror.onHello({ seq: 0, resume: 0, resync: false, introspection: true, version: '2.1.0' });
  await tick();
  assert.match(ui.lastFrame(), /\bdebug\b/);
  // A config event from ANOTHER client is news, not a layout: nothing moves.
  mirror.apply({ seq: 9, ts: 9, kind: 'config', data: { paths: ['agent.approval'], source: 'admin.set' } });
  await tick();
  frame = ui.lastFrame();
  assert.match(frame, /11\/5 tok/);
  assert.doesNotMatch(frame, /\d+ runs/);
  ui.unmount();
});

test('the subagents screen lists live subagents from the feed', async () => {
  const { mirror, ui } = boot();
  mirror.setConn('ready');
  mirror.apply({ seq: 1, ts: 10, kind: 'subagent', data: { handle: 'sub-researcher', mode: 'warm', status: 'running', tokens: 1200, updated: Date.now() } });
  // Navigate: tab → tasks, tab → subagents.
  ui.stdin.write('\t');
  await tick();
  ui.stdin.write('\t');
  await tick();
  const frame = ui.lastFrame();
  assert.match(frame, /sub-researcher/);
  assert.match(frame, /warm/);
  assert.match(frame, /running/);
  assert.match(frame, /enter details/);
  ui.unmount();
});

test('a cross-client conversation renders: prompt, working, reply', async () => {
  const { mirror, ui } = boot();
  mirror.setConn('ready');
  // Another client's prompt arrives on the feed, in the task's history…
  const history = [{ role: 'ROLE_USER', messageId: 'm1', parts: [{ text: 'What is up?' }] }];
  mirror.apply({ seq: 2, ts: 20, kind: 'task', data: { task: { id: 't1', contextId: 'c1', status: { state: 'TASK_STATE_WORKING', timestamp: at(20) }, history } } });
  await tick();
  let frame = ui.lastFrame();
  // Authorship is treatment, not a label: the user's line carries the
  // inverse block's gutter bar, the agent's a bullet.
  assert.match(frame, /▌\s+What is up\?/);
  assert.doesNotMatch(frame, /you ›/, 'no author labels');
  // With no activity record yet, the working row degrades to a bare label.
  assert.match(frame, /working/);
  assert.match(frame, /1 active/);
  // The daemon's activity says what it is DOING — phase, elapsed, tokens.
  mirror.apply({
    seq: 3, ts: 25, kind: 'activity',
    data: { id: '7', task: 't1', ctx: 'c1', phase: 'thinking', tool: null, round: 1, tokens_in: 900, tokens_out: 300, started_ms: Date.now() - 12_000, updated_ms: Date.now() },
  });
  await tick();
  frame = ui.lastFrame();
  assert.match(frame, /thinking/);
  assert.match(frame, /1[12]s/, 'elapsed ticks locally from started_ms');
  assert.match(frame, /1\.2k tok/);
  // A tool call names itself.
  mirror.apply({
    seq: 4, ts: 26, kind: 'activity',
    data: { id: '7', task: 't1', phase: 'tool', tool: 'read_file', round: 1, tokens_in: 900, tokens_out: 300, started_ms: Date.now() - 12_000, updated_ms: Date.now() },
  });
  await tick();
  assert.match(ui.lastFrame(), /read_file/);
  // …and the reply lands as the task's terminal artifact.
  mirror.apply({ seq: 5, ts: 30, kind: 'task', data: { task: { id: 't1', contextId: 'c1', status: { state: 'TASK_STATE_COMPLETED', timestamp: at(30) }, history, artifacts: [{ parts: [{ text: 'All good.' }] }] } } });
  await tick();
  frame = ui.lastFrame();
  assert.match(frame, /● All good\./);
  assert.doesNotMatch(frame, /working —/);
  ui.unmount();
});

test('draining and input-required surface prominently', async () => {
  const { mirror, ui } = boot();
  try {
    mirror.setConn('ready');
    mirror.apply({ seq: 1, ts: 10, kind: 'task', data: { task: { id: 't2', contextId: 'c2', status: { state: 'TASK_STATE_INPUT_REQUIRED', timestamp: at(10), message: { parts: [{ text: 'Which env?' }] } } } } });
    mirror.apply({ seq: 2, ts: 20, kind: 'lifecycle', data: { draining: true, reason: 'operator' } });
    await tick();
    const frame = ui.lastFrame();
    assert.match(frame, /Which env\?/);
    // The gate is in another conversation than this (fresh) console's, so a
    // plain reply would not reach it: the row says how to address it.
    assert.match(frame, /⏎ answer with #t2 <reply>/);
    assert.match(frame, /DRAINING/);
  } finally {
    ui.unmount();
  }
});

/** A card declaring the command vocabulary with exactly `ops`. */
function opsSession(ops, settable = []) {
  return session({ name: 'agentd', capabilities: { extensions: [{ uri: COMMAND_EXTENSION, params: { ops, settable } }] } });
}

test("a reply answers only this conversation's gate, and a Message answer is shown", async () => {
  const mirror = new Mirror();
  const sends = [];
  const client = {
    send: async (text, opts) => {
      sends.push({ text, opts });
      if (sends.length === 1) {
        // The agent answers directly, naming the conversation.
        return { task: null, reply: { messageId: 'r1', contextId: 'c7', role: 'ROLE_AGENT', parts: [{ text: 'pong' }] }, messageId: 'u1' };
      }
      return { task: null, messageId: `u${sends.length}` };
    },
  };
  const ui = render(React.createElement(App, { configured: 'http://127.0.0.1:1', client, mirror, observe: false }));
  try {
    mirror.setSession(opsSession([]));
    mirror.setConn('ready');
    await tick();
    ui.stdin.write('ping');
    await tick();
    ui.stdin.write('\r');
    await tick(60);
    assert.deepEqual(sends[0].opts, {}, 'a fresh console opens a new conversation');
    let frame = ui.lastFrame();
    assert.match(frame, /ping/);
    assert.match(frame, /● pong/, 'the Message reply is the answer');
    // A gate opens in ANOTHER conversation, then one in this one.
    mirror.apply({ seq: 1, ts: 1, kind: 'task', data: { task: { id: 'g-other', contextId: 'c9', status: { state: 'TASK_STATE_INPUT_REQUIRED', timestamp: at(1), message: { parts: [{ text: 'elsewhere?' }] } } } } });
    await tick();
    ui.stdin.write('first');
    await tick();
    ui.stdin.write('\r');
    await tick(60);
    assert.deepEqual(sends[1].opts, { contextId: 'c7' }, "another conversation's gate is not answered");
    mirror.apply({ seq: 2, ts: 2, kind: 'task', data: { task: { id: 'g-here', contextId: 'c7', status: { state: 'TASK_STATE_INPUT_REQUIRED', timestamp: at(2), message: { parts: [{ text: 'which?' }] } } } } });
    await tick();
    frame = ui.lastFrame();
    assert.match(frame, /which\?\s+⏎ reply to continue/);
    ui.stdin.write('this one');
    await tick();
    ui.stdin.write('\r');
    await tick(60);
    assert.deepEqual(sends[2].opts, { taskId: 'g-here' }, 'the gate, and no contextId with it');
    // AUTH_REQUIRED is shown, and not answerable from the composer.
    mirror.apply({ seq: 3, ts: 3, kind: 'task', data: { task: { id: 'g-here', contextId: 'c7', status: { state: 'TASK_STATE_AUTH_REQUIRED', timestamp: at(3) } } } });
    await tick();
    frame = ui.lastFrame();
    assert.match(frame, /⚿ authorization required/);
    assert.match(frame, /a reply cannot answer it/);
    assert.doesNotMatch(frame, /reply to continue/);
  } finally {
    ui.unmount();
  }
});

test('slash commands are gated on the card, and an approval asks first', async () => {
  const mirror = new Mirror();
  const approvals = [];
  const client = {
    authDeviceApprove: async (userCode, name, scope) => {
      approvals.push([userCode, name, scope]);
      return { approved: { user_code: userCode, principal: `user:${name}`, scope: scope ?? 'user' } };
    },
  };
  const ui = render(React.createElement(App, { configured: 'http://127.0.0.1:1', client, mirror, observe: false }));
  const type = async (line) => {
    ui.stdin.write(line);
    await tick();
    ui.stdin.write('\r');
    await tick(60);
  };
  try {
    mirror.setSession(opsSession([{ op: 'auth.device.approve', reply: 'task' }]));
    mirror.setConn('ready');
    await tick();
    await type('/set agent.approval never');
    assert.match(ui.lastFrame(), /\/set is not offered by this agent to you \(its card does not list admin\.set\)/);
    await type('/approve ABCD-EFGH alice');
    assert.match(ui.lastFrame(), /\? approve device ABCD-EFGH as alice\? \(y\/N\)/);
    ui.stdin.write('n');
    await tick(60);
    assert.equal(approvals.length, 0, 'anything but y is no');
    assert.match(ui.lastFrame(), /not approved: ABCD-EFGH/);
    await type('/approve ABCD-EFGH alice operator');
    ui.stdin.write('y');
    await tick(60);
    assert.deepEqual(approvals, [['ABCD-EFGH', 'alice', 'operator']]);
    assert.match(ui.lastFrame(), /approved ABCD-EFGH as user:alice \(operator\)/);
    // The name is required, and checked before anything is sent.
    await type('/approve ABCD-EFGH');
    assert.match(ui.lastFrame(), /usage: \/approve <code> <name> \[operator\]/);
    assert.equal(approvals.length, 1);
  } finally {
    ui.unmount();
  }
});

test('chrome from the client layout, status values, and terminal banners', async () => {
  const mirror = new Mirror();
  const ui = render(
    React.createElement(App, {
      configured: 'http://127.0.0.1:1', client: {}, mirror, observe: false,
      layout: { top: ['name', 'memory:deploy.state'], bottom: ['conn', 'endpoint'] },
    }),
  );
  try {
    mirror.setSession(session());
    mirror.bootstrap({ version: '2.1.0', instance: 'box-3', values: { 'deploy.state': 'green' } });
    mirror.setConn('ready');
    await tick();
    let frame = ui.lastFrame();
    assert.match(frame, /agentd green/, 'the layout it was given, with the published value');
    assert.doesNotMatch(frame, /2\.1\.0|box-3|tab:screens/, 'nothing the layout leaves out');
    // A live status event carries the value as it is now.
    mirror.apply({ seq: 1, ts: 1, kind: 'status', data: { values: { 'deploy.state': 'red' } } });
    await tick();
    assert.match(ui.lastFrame(), /agentd red/);
    // /layout reshapes it locally, and refuses what it does not know.
    ui.stdin.write('/layout top name,instance');
    await tick();
    ui.stdin.write('\r');
    await tick(60);
    assert.match(ui.lastFrame(), /^agentd box-3/m);
    // Each terminal state names itself on the status bar — never "closed".
    for (const [state, text] of [['unauthenticated', '✗ unauthenticated'], ['forbidden', '✗ forbidden'], ['incompatible', '✗ incompatible']]) {
      mirror.setConn(state, 'why');
      await tick();
      frame = ui.lastFrame();
      assert.ok(frame.includes(text), `${state}: ${frame}`);
      assert.doesNotMatch(frame, /closed/);
    }
  } finally {
    ui.unmount();
  }

  // The banner, through a real observation: a launch session the agent no
  // longer accepts ends with the one thing to do about it.
  const fake = await startFakeA2a({ bearer: 'the-live-token' });
  const m2 = new Mirror();
  const ui2 = render(
    React.createElement(App, {
      configured: fake.url, mirror: m2, credential: { token: 'a-revoked-token' }, signIn: 'launch', fullscreen: false,
    }),
  );
  try {
    for (let i = 0; i < 100 && m2.getState().conn !== 'unauthenticated'; i++) await tick(20);
    await tick();
    const frame = ui2.lastFrame();
    assert.ok(frame.includes(LAUNCH_SESSION_ENDED), frame);
    assert.ok(frame.includes('✗ unauthenticated'), frame);
  } finally {
    ui2.unmount();
    await fake.close();
  }
  // Each kind of ending says what fixes it.
  const device = [{ method: 'device', flow: {} }];
  assert.deepEqual(terminalBanner({ kind: 'unauthenticated', message: 'm' }, 'launch', device), [LAUNCH_SESSION_ENDED]);
  assert.match(terminalBanner({ kind: 'unauthenticated', message: 'm' }, 'device', device).join('\n'), /\/login to sign in again/);
  assert.match(terminalBanner({ kind: 'unauthenticated', message: 'm' }, undefined, []).join('\n'), /--bearer-file/);
  assert.match(terminalBanner({ kind: 'incompatible', message: 'm' }, undefined, []).join('\n'), /incompatible agent: m/);
});

test('the TUI sends no Origin header', async () => {
  // The real program, launched the way `agentd tui` launches it: the code on
  // fd 3, then everything a session does against a real HTTP server.
  const fake = await startFakeA2a();
  fake.launch.code = 'agentd_lc_origin';
  fake.bearer = fake.launch.token;
  const child = spawn(process.execPath, [CLI, '--endpoint', fake.url, '--launch-fd', '3'], {
    env: { PATH: process.env.PATH },
    stdio: ['ignore', 'pipe', 'pipe', 'pipe'],
  });
  let stderr = '';
  child.stderr.on('data', (d) => (stderr += d));
  child.stdout.resume();
  child.stdio[3].end('agentd_lc_origin\n');
  try {
    // Wait until the launched session has made authenticated JSON-RPC calls.
    const authed = () => fake.requests.filter((r) => r.rpc !== undefined && r.headers.authorization === `Bearer ${fake.launch.token}`);
    for (let i = 0; i < 250 && authed().length < 2; i++) await tick(20);
    assert.ok(authed().length >= 2, `the launch session reached JSON-RPC: ${stderr}`);
    const exchange = fake.requests.filter((r) => r.path === '/oauth2/token');
    assert.equal(exchange.length, 1);
    assert.equal(fake.cardGets().length >= 1, true);
    for (const r of fake.requests) {
      assert.equal(r.headers.origin, undefined, `${r.method} ${r.path}${r.rpc ? ` ${r.rpc}` : ''} carried an Origin header`);
    }
  } finally {
    child.kill('SIGKILL');
    await fake.close();
  }
});

test('fullscreen windows the transcript from the bottom and reports what is above', async () => {
  const { windowEntries } = await import('../dist/tui/parts/transcript.js');
  const entries = Array.from({ length: 20 }, (_, i) => ({
    key: `k${i}`, ctx: 'c', ts: i, kind: 'user', text: `line ${i}`,
  }));
  // Following the tail: the last N that fit, nothing hidden below.
  // Each message costs its body plus the blank separator row that spaces the
  // conversation out, so a 5-row window holds two of them.
  const bottom = windowEntries(entries, { rows: 5, columns: 80, offset: 0 });
  assert.equal(bottom.visible.length, 2);
  assert.equal(bottom.visible.at(-1).text, 'line 19', 'anchored at the live end');
  assert.equal(bottom.above, 18);
  // Scrolled up by 10: the window moves back, the tail is hidden below.
  const up = windowEntries(entries, { rows: 5, columns: 80, offset: 10 });
  assert.equal(up.visible.at(-1).text, 'line 9');
  assert.equal(up.above, 8);
  // Wrapped entries take more rows, so fewer fit.
  const long = [{ key: 'l', ctx: 'c', ts: 0, kind: 'user', text: 'x'.repeat(300) }, ...entries];
  const wrapped = windowEntries(long, { rows: 4, columns: 40, offset: 20 });
  assert.equal(wrapped.visible.length, 1, 'one 300-char entry fills a 40-col window');
  // A single entry taller than the window still renders (never a blank screen).
  const huge = windowEntries([{ key: 'h', ctx: 'c', ts: 0, kind: 'user', text: 'y'.repeat(1000) }], { rows: 2, columns: 40, offset: 0 });
  assert.equal(huge.visible.length, 1);
  assert.equal(huge.above, 0);
});

test('fullscreen renders a scroll hint instead of terminal scrollback', async () => {
  const mirror = new Mirror();
  const ui = render(
    React.createElement(App, {
      configured: 'http://127.0.0.1:1', client: {}, mirror, observe: false, fullscreen: true,
    }),
  );
  mirror.setConn('ready');
  for (let i = 0; i < 40; i++) {
    const task = { id: `t${i}`, contextId: 'c', status: { state: 'TASK_STATE_COMPLETED', timestamp: at(i) }, history: [{ role: 'ROLE_USER', messageId: `m${i}`, parts: [{ text: `msg ${i}` }] }] };
    mirror.apply({ seq: i + 1, ts: i, kind: 'task', data: { task } });
  }
  await tick();
  const frame = ui.lastFrame();
  assert.match(frame, /msg 39/, 'anchored at the newest message');
  assert.match(frame, /earlier messages/, 'says how much is above the fold');
  assert.doesNotMatch(frame, /msg 0\b/, 'older messages are outside the viewport');
  ui.unmount();
});
