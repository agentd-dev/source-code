// SPDX-License-Identifier: Apache-2.0
/**
 * Render real TUI frames for the documentation.
 *
 * The frames are produced by the actual App against a Mirror driven with
 * daemon-shaped events — the same harness the render tests use. That matters:
 * a screenshot mocked up by hand drifts from the product the moment either
 * changes, while these are regenerated from the code that ships.
 *
 *   node tools/frames.mjs > ../docs/_tui-frames.json
 */
import React from 'react';
import { render } from 'ink-testing-library';
import { Mirror } from '../dist/client/index.js';
import { App } from '../dist/tui/app.js';

const tick = (ms = 40) => new Promise((r) => setTimeout(r, ms));

/** A stand-in daemon: the App calls these on user actions. */
const client = {
  subagentGet: async (handle) => ({
    handle,
    status: handle === 'sa-lint' ? 'failed' : 'running',
    mode: handle === 'sa-lint' ? 'detached' : 'supervised',
    attempt: 1,
    tokens: handle === 'sa-lint' ? 260 : 4120,
    instruction: 'Review the diff for correctness regressions and report findings.',
    result: null,
    error: handle === 'sa-lint' ? 'lint server refused the connection' : null,
    requested_by: 'operator',
  }),
  subagentKill: async () => ({ ok: true }),
  subagentSend: async () => ({ ok: true }),
};

function boot(cols = 92, rows = 26) {
  const mirror = new Mirror();
  const ui = render(
    React.createElement(App, { endpoint: 'http://127.0.0.1:8420', client, mirror, observe: false }),
    { columns: cols, rows },
  );
  return { mirror, ui };
}

const INFO = {
  enabled: true, debug: true, version: '2.3.0', instance: 'triage-1', model: 'gpt-5.1',
  protocol: 1, feed: { ring: 1024, method: 'SubscribeToEvents' }, ops: [],
};

/** A daemon mid-flight: a run stepping, two subagents, one in trouble. */
function populate(mirror) {
  mirror.setCard({ name: 'agentd' });
  mirror.setInfo(INFO);
  mirror.setConn('ready');
  mirror.apply({ seq: 1, ts: 1, kind: 'run', data: { id: 'pipeline-01M0C0', workflow: 'pipeline', status: 'running', steps: '3/7' } });
  const step = (n, s, extra = {}) => mirror.apply({ seq: n, ts: n, kind: 'step', data: { run: 'pipeline-01M0C0', step: s, ...extra } });
  step(2, 'fetch', { kind: 'mcp.tool', phase: 'start' });
  step(3, 'fetch', { phase: 'done', status: 'done', tokens: 0 });
  step(4, 'triage', { kind: 'extract', phase: 'start', attempt: 1 });
  step(5, 'triage', { phase: 'done', status: 'done', tokens: 1840 });
  step(6, 'notify', { kind: 'a2a.send', phase: 'start' });
  mirror.apply({ seq: 7, ts: 7, kind: 'subagent', data: { handle: 'sa-review', mode: 'supervised', status: 'running', tokens: 4120, updated: Date.now() } });
  mirror.apply({ seq: 8, ts: 8, kind: 'subagent', data: { handle: 'sa-lint', mode: 'detached', status: 'failed', tokens: 260, updated: Date.now() } });
  return mirror;
}

const frames = {};
async function capture(name, setup, cols = 92, rows = 26) {
  const { mirror, ui } = boot(cols, rows);
  await setup(mirror, ui);
  await tick();
  frames[name] = (ui.lastFrame() ?? '').replace(/\s+$/gm, '');
  ui.unmount();
}

await capture('chat', async (m) => {
  populate(m);
  m.apply({ seq: 20, ts: 20, kind: 'message', data: { messageId: 'm1', contextId: 'c1', principal: 'operator', text: 'Triage the newest issue' } });
  m.apply({ seq: 21, ts: 21, kind: 'task', data: { task: { id: 't1', contextId: 'c1', status: { state: 'TASK_STATE_WORKING', timestamp: 21 } } } });
  await tick();
});

await capture('subagents', async (m, ui) => {
  populate(m);
  await tick();
  ui.stdin.write('\t');           // chat -> tasks
  await tick();
  ui.stdin.write('\t');           // tasks -> subagents
  await tick();
});

// The detail view, where the control verbs live.
await capture('subagent-detail', async (m, ui) => {
  populate(m);
  await tick();
  ui.stdin.write('\t'); await tick();
  ui.stdin.write('\t'); await tick();
  ui.stdin.write('\r'); await tick(120);   // enter -> detail
});

// The same view asking to confirm a stop.
await capture('subagent-stop', async (m, ui) => {
  populate(m);
  await tick();
  ui.stdin.write('\t'); await tick();
  ui.stdin.write('\t'); await tick();
  ui.stdin.write('\r'); await tick(120);
  ui.stdin.write('k'); await tick(200);
});

await capture('debug', async (m, ui) => {
  populate(m);
  await tick();
  for (let i = 0; i < 3; i++) { ui.stdin.write('\t'); await tick(); }
});

process.stdout.write(JSON.stringify(frames, null, 2));
