// SPDX-License-Identifier: AGPL-3.0-only
// Render tests: the App is a pure projection of the Mirror — drive the mirror
// with daemon-shaped events (no network) and assert the frames.
import test from 'node:test';
import assert from 'node:assert/strict';
import React from 'react';
import { render } from 'ink-testing-library';
import { Mirror } from '@agentd/client';
import { App } from '../dist/app.js';

const tick = () => new Promise((r) => setTimeout(r, 30));

function boot() {
  const mirror = new Mirror();
  // A fake client: the App only calls it on user actions, none here.
  const client = {};
  const ui = render(
    React.createElement(App, {
      endpoint: 'http://127.0.0.1:1',
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
  mirror.setCard({ name: 'agentd' });
  mirror.setInfo({ enabled: true, debug: false, version: '9.9.9', instance: 'box-1', protocol: 1, feed: { ring: 1024, method: 'SubscribeToEvents' }, ops: [] });
  mirror.setConn('ready');
  await tick();
  const frame = ui.lastFrame();
  // The chrome renders the daemon-declared display items in order.
  assert.match(frame, /agentd 9\.9\.9 box-1/);
  assert.match(frame, /● live/);
  ui.unmount();
});

test('the daemon reshapes the chrome via interface.display / config events', async () => {
  const { mirror, ui } = boot();
  mirror.setConn('ready');
  mirror.setInfo({
    enabled: true, debug: false, version: '2.1.0', instance: 'box-2', model: 'mock-9', protocol: 1,
    feed: { ring: 1024, method: 'SubscribeToEvents' }, ops: [],
    display: { top: ['name', 'model'], bottom: ['conn', 'tokens'] },
  });
  mirror.bootstrap({ counters: { turns: 2, tokens_in: 11, tokens_out: 5 } });
  await tick();
  let frame = ui.lastFrame();
  assert.match(frame, /agentd mock-9/, 'top = name + model');
  assert.match(frame, /11\/5 tok/, 'bottom includes tokens');
  assert.doesNotMatch(frame, /tab:screens/, 'keys not in the configured bottom');
  // A runtime config.set from ANOTHER client re-shapes this one too.
  mirror.apply({ seq: 9, ts: 9, kind: 'config', data: { path: 'interface.display.bottom', value: ['conn', 'runs'] } });
  await tick();
  frame = ui.lastFrame();
  assert.match(frame, /0 runs/);
  assert.doesNotMatch(frame, /11\/5 tok/);
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
  // Another client's prompt arrives on the feed…
  mirror.apply({ seq: 1, ts: 10, kind: 'message', data: { messageId: 'm1', contextId: 'c1', taskId: 't1', principal: 'user:web', text: 'What is up?' } });
  mirror.apply({ seq: 2, ts: 20, kind: 'task', data: { task: { id: 't1', contextId: 'c1', status: { state: 'TASK_STATE_WORKING', timestamp: 20 } } } });
  await tick();
  let frame = ui.lastFrame();
  assert.match(frame, /you › What is up\?/);
  assert.match(frame, /working — 1 task live/);
  assert.match(frame, /1 active/);
  // …and the reply lands as the task's terminal artifact.
  mirror.apply({ seq: 3, ts: 30, kind: 'task', data: { task: { id: 't1', contextId: 'c1', updated: 30, status: { state: 'TASK_STATE_COMPLETED', timestamp: 30 }, artifacts: [{ parts: [{ text: 'All good.' }] }] } } });
  await tick();
  frame = ui.lastFrame();
  assert.match(frame, /agent › All good\./);
  assert.doesNotMatch(frame, /working —/);
  ui.unmount();
});

test('draining and input-required surface prominently', async () => {
  const { mirror, ui } = boot();
  mirror.setConn('ready');
  mirror.apply({ seq: 1, ts: 10, kind: 'task', data: { task: { id: 't2', contextId: 'c2', updated: 10, status: { state: 'TASK_STATE_INPUT_REQUIRED', timestamp: 10, message: { parts: [{ text: 'Which env?' }] } } } } });
  mirror.apply({ seq: 2, ts: 20, kind: 'lifecycle', data: { draining: true, reason: 'operator' } });
  await tick();
  const frame = ui.lastFrame();
  assert.match(frame, /Which env\?/);
  assert.match(frame, /\[reply to continue\]/);
  assert.match(frame, /DRAINING/);
  ui.unmount();
});
