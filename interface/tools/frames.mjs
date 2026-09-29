// SPDX-License-Identifier: AGPL-3.0-only
/**
 * Render real TUI frames for the documentation.
 *
 * The frames are produced by the actual App against a Mirror driven with
 * daemon-shaped events — the same harness the render tests use, and the same
 * fake agent's card (test/fake-a2a.mjs), here declaring agentd's extensions
 * the way agentd's own card does. That matters: a screenshot mocked up by hand
 * drifts from the product the moment either changes, while these are
 * regenerated from the code that ships.
 *
 *   node tools/frames.mjs > ../docs/_generated/tui-frames.json
 */
import './_force_color.mjs';
import React from 'react';
import { render } from 'ink-testing-library';
import {
  COMMAND_EXTENSION,
  EVENTS_EXTENSION,
  INTROSPECTION_OPS,
  Mirror,
  OPS,
  TASK_ANNOTATIONS_EXTENSION,
  capabilitiesOf,
} from '../dist/client/index.js';
import { App } from '../dist/tui/app.js';
import { defaultCard } from '../test/fake-a2a.mjs';

const tick = (ms = 40) => new Promise((r) => setTimeout(r, ms));
const ENDPOINT = 'http://127.0.0.1:8420';
/** An RFC 3339 timestamp `s` seconds into a fixed day. */
const at = (s) => new Date(Date.UTC(2026, 8, 1, 9, 0, s)).toISOString();

/**
 * The ops that answer with a Message — the reads, as the daemon's op table
 * has them. Every other op answers with a Task.
 */
const READS = new Set([
  OPS.status,
  OPS.config,
  OPS.workflowStatus,
  OPS.subagentStatus,
  OPS.planGet,
  ...INTROSPECTION_OPS,
  OPS.authDevicePending,
  OPS.authSessions,
]);

/**
 * The card: the fake agent's, declaring what an agentd with the feed and
 * introspection on declares to its operator.
 */
function card() {
  const c = defaultCard(ENDPOINT);
  c.name = 'agentd';
  const ops = Object.values(OPS).map((op) => ({ op, reply: READS.has(op) ? 'message' : 'task' }));
  c.capabilities.extensions = [
    { uri: COMMAND_EXTENSION, params: { ops, settable: [] } },
    { uri: EVENTS_EXTENSION, params: { ring: 1024, kinds: ['task', 'run', 'step', 'subagent'] } },
    { uri: TASK_ANNOTATIONS_EXTENSION, params: {} },
  ];
  return c;
}

function session() {
  const c = card();
  // Read as an operator's extended card, so the introspection ops are known on.
  const caps = capabilitiesOf(c, c);
  if (!INTROSPECTION_OPS.every((op) => caps.command?.ops.has(op))) throw new Error('the fixture card lost the introspection ops');
  return { cardUrl: `${ENDPOINT}/.well-known/agent-card.json`, card: c, extended: c, ep: { url: `${ENDPOINT}/` }, caps, warnings: [] };
}

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
  // A prompt typed into the composer opens a task in conversation c1.
  send: async (text) => ({
    task: {
      id: 't-ship', contextId: 'c1', state: 'TASK_STATE_WORKING', artifacts: [], artifactData: [], updated: Date.now(),
      history: [{ messageId: 'typed', role: 'ROLE_USER', text, data: [] }],
    },
    messageId: 'typed',
  }),
};

function boot(cols = 92, rows = 26) {
  const mirror = new Mirror();
  const ui = render(
    React.createElement(App, { configured: ENDPOINT, client, mirror, observe: false }),
    { columns: cols, rows },
  );
  return { mirror, ui };
}

/** The feed's sequence: every event is numbered after the one before it, as the daemon numbers them. */
let seq = 0;
const feed = (m, kind, data) => m.apply({ seq: ++seq, ts: seq, kind, data });

/** A daemon mid-flight: a run stepping, two subagents, one in trouble. */
function populate(mirror) {
  seq = 0;
  mirror.setSession(session());
  mirror.bootstrap({ version: '1.17.0', instance: 'triage-1', model: 'gpt-5.1' });
  mirror.onHello({ seq: 0, resume: 0, resync: false, introspection: true, version: '1.17.0' });
  mirror.setConn('ready');
  feed(mirror, 'run', { id: 'pipeline-01M0C0', workflow: 'pipeline', status: 'running', steps: '3/7' });
  const step = (s, extra = {}) => feed(mirror, 'step', { run: 'pipeline-01M0C0', step: s, ...extra });
  // Durations are measured from the events the client sees, so the fixture has
  // to actually take time — otherwise every step documents itself as `0ms` and
  // the column looks broken rather than fast.
  step('fetch', { kind: 'mcp.tool', phase: 'start' });
  step('triage', { kind: 'extract', phase: 'start', attempt: 1 });
  step('notify', { kind: 'a2a.send', phase: 'start' });
  return mirror;
}

/** Finish the two steps that complete, after real elapsed time. */
async function settle(mirror) {
  const step = (s, extra = {}) => feed(mirror, 'step', { run: 'pipeline-01M0C0', step: s, ...extra });
  await tick(140);
  step('fetch', { phase: 'done', status: 'done', tokens: 0 });
  await tick(900);
  step('triage', { phase: 'done', status: 'done', tokens: 1840 });
  feed(mirror, 'subagent', { handle: 'sa-review', mode: 'supervised', status: 'running', tokens: 4120, updated: Date.now() });
  feed(mirror, 'subagent', { handle: 'sa-lint', mode: 'detached', status: 'failed', tokens: 260, updated: Date.now() });
}

/** Type a line into the composer and send it. */
async function type(ui, line) {
  ui.stdin.write(line);
  await tick();
  ui.stdin.write('\r');
  await tick(80);
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
  await settle(m);
  // Another client's prompt, as the task's history carries it.
  feed(m, 'task', { task: {
    id: 't1', contextId: 'c1',
    status: { state: 'TASK_STATE_WORKING', timestamp: at(21) },
    history: [{ role: 'ROLE_USER', messageId: 'm1', parts: [{ text: 'Triage the newest issue' }] }],
  } });
  await tick();
});

await capture('subagents', async (m, ui) => {
  populate(m);
  await settle(m);
  await tick();
  ui.stdin.write('\t');           // chat -> tasks
  await tick();
  ui.stdin.write('\t');           // tasks -> subagents
  await tick();
});

// The detail view, where the control verbs live.
await capture('subagent-detail', async (m, ui) => {
  populate(m);
  await settle(m);
  await tick();
  ui.stdin.write('\t'); await tick();
  ui.stdin.write('\t'); await tick();
  ui.stdin.write('\r'); await tick(120);   // enter -> detail
});

// The same view asking to confirm a stop.
await capture('subagent-stop', async (m, ui) => {
  populate(m);
  await settle(m);
  await tick();
  ui.stdin.write('\t'); await tick();
  ui.stdin.write('\t'); await tick();
  ui.stdin.write('\r'); await tick(120);
  ui.stdin.write('k'); await tick(200);
});

await capture('debug', async (m, ui) => {
  populate(m);
  await settle(m);
  await tick();
  for (let i = 0; i < 3; i++) { ui.stdin.write('\t'); await tick(); }
});

/**
 * A gate in the conversation this console is in: the task the person's prompt
 * opened stops to ask. Stamped after the prompt, as the daemon would.
 */
async function gate(m, ui, text, askSchema) {
  await type(ui, 'Ship the release?');
  feed(m, 'task', { task: {
    id: 't-ship', contextId: 'c1',
    status: {
      state: 'TASK_STATE_INPUT_REQUIRED',
      timestamp: new Date(Date.now() + 1000).toISOString(),
      message: { role: 'ROLE_AGENT', messageId: 'ask-1', parts: [{ text }] },
    },
    history: [{ role: 'ROLE_USER', messageId: 'typed', parts: [{ text: 'Ship the release?' }] }],
    metadata: { [TASK_ANNOTATIONS_EXTENSION]: { askSchema } },
  } });
  await tick(80);
}

// A gate whose schema says "one of these three" — the form, not a text box.
await capture('gate-choice', async (m, ui) => {
  populate(m);
  await settle(m);
  await gate(m, ui, 'Candidate sa-review found 3 regressions. How should I proceed?', {
    type: 'string', enum: ['ship anyway', 'hold for fixes', 'roll back'],
  });
});

// A multi-select with an "other" escape hatch.
await capture('gate-multi', async (m, ui) => {
  populate(m);
  await settle(m);
  await gate(m, ui, 'Which checks should I run before merging?', {
    type: 'array', items: { anyOf: [{ enum: ['unit', 'integration', 'e2e'] }, { type: 'string' }] },
  });
});

process.stdout.write(JSON.stringify(frames, null, 2));
