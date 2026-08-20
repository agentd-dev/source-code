// SPDX-License-Identifier: AGPL-3.0-only
// Unit tests for the thin-client core: the SSE parser (chunk boundaries,
// keep-alives), task-shape normalization (wrapped/nested vs flat), and the
// mirror's convergence behavior — including the cross-client transcript.
import test from 'node:test';
import assert from 'node:assert/strict';
import { sseParser, normalizeTask, Mirror } from '../dist/client/index.js';

test('sse parser handles chunk boundaries, multi-line data and comments', () => {
  const got = [];
  const feed = sseParser((d) => got.push(d));
  // A frame split across chunks, a keep-alive comment, then two frames at once.
  feed('data: {"a"');
  feed(':1}\n');
  feed('\n');
  feed(': keep-alive\n\n');
  feed('data: one\n\ndata: two\n\n');
  assert.deepEqual(got, ['{"a":1}', 'one', 'two']);
  // Multi-line data concatenates with newlines (SSE spec).
  feed('data: l1\ndata: l2\n\n');
  assert.equal(got[3], 'l1\nl2');
});

test('normalizeTask flattens the A2A task the daemon actually sends', () => {
  // One shape on every path now: state under `status`, an RFC 3339 timestamp,
  // and agentd's own facts under `metadata` (proto3's extension point).
  const full = normalizeTask({
    id: 't1',
    contextId: 'c1',
    status: {
      state: 'TASK_STATE_COMPLETED',
      timestamp: '2026-08-17T13:41:27.824Z',
      message: { role: 'ROLE_AGENT', parts: [{ text: 'done' }] },
    },
    artifacts: [{ artifactId: 't1.result', parts: [{ text: 'The answer' }] }],
    metadata: {
      'agentd/principal': 'operator',
      'agentd/link': { run: { id: 'r1' } },
      'agentd/statusHistory': [{ state: 'TASK_STATE_SUBMITTED', ts: 1 }],
    },
  });
  assert.equal(full.state, 'TASK_STATE_COMPLETED');
  assert.equal(full.message, 'done');
  assert.deepEqual(full.artifacts, ['The answer']);
  assert.equal(full.principal, 'operator');
  assert.deepEqual(full.link, { run: { id: 'r1' } });
  assert.equal(full.history.length, 1);
  // The timestamp is RFC 3339 on the wire and epoch ms in the view — the TUI
  // sorts and subtracts it, so a string would silently produce NaN.
  assert.equal(full.updated, Date.parse('2026-08-17T13:41:27.824Z'));

  // The listing is the same shape without artifacts.
  const listed = normalizeTask({
    id: 't2',
    contextId: 'c2',
    status: { state: 'TASK_STATE_WORKING', timestamp: '2026-08-17T13:41:28.000Z' },
    metadata: { 'agentd/principal': 'operator' },
  });
  assert.equal(listed.state, 'TASK_STATE_WORKING');
  assert.deepEqual(listed.artifacts, []);

  // A daemon predating that move still reads: the flat fields are the fallback.
  const old = normalizeTask({ id: 't3', contextId: 'c3', state: 'TASK_STATE_WORKING', principal: 'operator', updated: 9 });
  assert.equal(old.state, 'TASK_STATE_WORKING');
  assert.equal(old.updated, 9);
  assert.equal(old.principal, 'operator');

  assert.equal(normalizeTask({ noId: true }), null);
});

test('the mirror converges tasks, sections and the cross-client transcript', () => {
  const m = new Mirror();
  let notified = 0;
  m.subscribe(() => notified++);

  // Bootstrap adopts the status document sections.
  m.bootstrap({
    draining: false,
    runs: [{ id: 'r1', workflow: 'greet', status: 'running' }],
    conversations: [{ id: 'c1', messages: 2 }],
    subagents: [{ handle: 'h1', status: 'running' }],
    children: [{ node: 7, pid: 123 }],
  });
  const s = m.getState();
  assert.equal(s.runs.get('r1').workflow, 'greet');
  assert.equal(s.children.get('7').pid, 123);

  // ANOTHER client's prompt arrives on the feed → transcript entry.
  m.apply({ seq: 1, ts: 100, kind: 'message', data: { messageId: 'mA', contextId: 'c9', taskId: 't9', principal: 'operator', text: 'Hello from the web UI' } });
  assert.equal(s.transcript.length, 1);
  assert.equal(s.transcript[0].kind, 'user');
  assert.match(s.transcript[0].text, /web UI/);

  // The task works, then completes with the reply → agent entry, prompt settles.
  m.apply({ seq: 2, ts: 110, kind: 'task', data: { task: { id: 't9', contextId: 'c9', status: { state: 'TASK_STATE_WORKING', timestamp: 110 } }, principal: 'operator' } });
  assert.equal(m.activeTasks().length, 1);
  m.apply({ seq: 3, ts: 120, kind: 'task', data: { task: { id: 't9', contextId: 'c9', updated: 120, status: { state: 'TASK_STATE_COMPLETED', timestamp: 120 }, artifacts: [{ parts: [{ text: 'Hi!' }] }] } } });
  assert.equal(m.activeTasks().length, 0);
  const agent = s.transcript.find((e) => e.kind === 'agent');
  assert.equal(agent.text, 'Hi!');
  assert.equal(agent.taskId, 't9');
  assert.equal(s.lastSeq, 3);

  // Sections update + departure events.
  m.apply({ seq: 4, ts: 130, kind: 'run', data: { id: 'r1', workflow: 'greet', status: 'completed' } });
  assert.equal(s.runs.get('r1').status, 'completed');
  m.apply({ seq: 5, ts: 140, kind: 'subagent.removed', data: { id: 'h1' } });
  assert.equal(s.subagents.has('h1'), false);
  m.apply({ seq: 6, ts: 150, kind: 'lifecycle', data: { draining: true, reason: 'test' } });
  assert.equal(s.draining, true);
  assert.ok(notified > 3, 'listeners fire');
});

test('the local echo reconciles with its feed message (no duplicate rows)', () => {
  const m = new Mirror();
  m.localEcho('m-1', 'ctx', 'my prompt', 't-1');
  assert.equal(m.getState().transcript.length, 1);
  assert.equal(m.getState().transcript[0].pending, true);
  // The daemon's message event for the SAME messageId lands (as every other
  // client sees it) — same row, now settled, not a duplicate.
  m.apply({ seq: 10, ts: Date.now(), kind: 'message', data: { messageId: 'm-1', contextId: 'ctx', taskId: 't-1', text: 'my prompt' } });
  assert.equal(m.getState().transcript.length, 1);
  assert.equal(m.getState().transcript[0].pending, false);
});

test('command-result tasks stay OFF the transcript (no prompt → no row)', () => {
  const m = new Mirror();
  // A `status` command completes as a task with a JSON artifact — but no
  // `message` event ever carried its taskId, so the conversation stays clean.
  m.apply({ seq: 1, ts: 10, kind: 'task', data: { task: { id: 't-cmd', contextId: 'a2a-7', updated: 10, status: { state: 'TASK_STATE_COMPLETED', timestamp: 10 }, artifacts: [{ parts: [{ text: '{"runs":[],"huge":"status blob"}' }] }] } } });
  assert.equal(m.getState().transcript.length, 0);
  assert.ok(m.getState().tasks.has('t-cmd'), 'still on the Tasks screen');
  // Whereas a task WITH a known prompt renders its reply.
  m.apply({ seq: 2, ts: 20, kind: 'message', data: { messageId: 'm1', contextId: 'c', taskId: 't-nl', text: 'hi' } });
  m.apply({ seq: 3, ts: 30, kind: 'task', data: { task: { id: 't-nl', contextId: 'c', updated: 30, status: { state: 'TASK_STATE_COMPLETED', timestamp: 30 }, artifacts: [{ parts: [{ text: 'hello' }] }] } } });
  assert.equal(m.getState().transcript.filter((e) => e.kind === 'agent').length, 1);
});

test('input-required surfaces as an answerable agent row', () => {
  const m = new Mirror();
  m.apply({ seq: 1, ts: 10, kind: 'task', data: { task: { id: 't5', contextId: 'c5', updated: 10, status: { state: 'TASK_STATE_INPUT_REQUIRED', timestamp: 10, message: { parts: [{ text: 'Which region?' }] } } } } });
  const row = m.getState().transcript.find((e) => e.inputRequired);
  assert.equal(row.text, 'Which region?');
  assert.equal(row.taskId, 't5');
});

test('step events collapse into one row per step and carry state', () => {
  const m = new Mirror();
  // A step's life is two feed events. The UI wants ONE row that changes state,
  // not a scrolling pair — otherwise a run of twenty steps reads as forty
  // lines and the thing you are looking for is buried.
  m.apply({ seq: 1, ts: 10, kind: 'step', data: { run: 'r1', step: 'fetch', kind: 'assign', phase: 'start', attempt: 1 } });
  let rows = m.state.steps.get('r1');
  assert.equal(rows.length, 1);
  assert.equal(rows[0].phase, 'start');
  assert.equal(rows[0].kind, 'assign');

  m.apply({ seq: 2, ts: 20, kind: 'step', data: { run: 'r1', step: 'fetch', phase: 'done', status: 'done', tokens: 12 } });
  rows = m.state.steps.get('r1');
  assert.equal(rows.length, 1, 'the done event completes the row in place');
  assert.equal(rows[0].phase, 'done');
  assert.equal(rows[0].status, 'done');
  // The kind came from the start event and must survive the completion.
  assert.equal(rows[0].kind, 'assign');

  // A failure keeps its error where the UI can show it.
  m.apply({ seq: 3, ts: 30, kind: 'step', data: { run: 'r1', step: 'boom', kind: 'fail', phase: 'start' } });
  m.apply({ seq: 4, ts: 40, kind: 'step', data: { run: 'r1', step: 'boom', phase: 'done', status: 'failed', err: 'downstream refused' } });
  rows = m.state.steps.get('r1');
  assert.equal(rows.length, 2);
  assert.equal(rows[1].status, 'failed');
  assert.equal(rows[1].err, 'downstream refused');

  // Steps are per-run, and a removed run takes its steps with it rather than
  // leaking for the lifetime of the client.
  m.apply({ seq: 5, ts: 50, kind: 'step', data: { run: 'r2', step: 'other', phase: 'start' } });
  assert.equal(m.state.steps.get('r2').length, 1);
  m.apply({ seq: 6, ts: 60, kind: 'run.removed', data: { id: 'r1' } });
  assert.equal(m.state.steps.has('r1'), false);
  assert.equal(m.state.steps.get('r2').length, 1, 'another run is untouched');
});

test('a run with many steps stays bounded in client memory', () => {
  const m = new Mirror();
  for (let i = 0; i < 300; i++) {
    m.apply({ seq: i, ts: i, kind: 'step', data: { run: 'big', step: `s${i}`, phase: 'start' } });
  }
  assert.ok(m.state.steps.get('big').length <= 200, 'the ring is capped');
});
