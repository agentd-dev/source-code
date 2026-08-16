// SPDX-License-Identifier: Apache-2.0
// Unit tests for the thin-client core: the SSE parser (chunk boundaries,
// keep-alives), task-shape normalization (wrapped/nested vs flat), and the
// mirror's convergence behavior — including the cross-client transcript.
import test from 'node:test';
import assert from 'node:assert/strict';
import { sseParser, normalizeTask, Mirror } from '../dist/index.js';

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

test('normalizeTask folds both wire shapes into one view', () => {
  // The nested shape (GetTask / SendMessage / feed).
  const full = normalizeTask({
    id: 't1',
    contextId: 'c1',
    status: { state: 'TASK_STATE_COMPLETED', timestamp: 5, message: { role: 'agent', parts: [{ text: 'done' }] } },
    artifacts: [{ artifactId: 't1.result', parts: [{ text: 'The answer' }] }],
  });
  assert.equal(full.state, 'TASK_STATE_COMPLETED');
  assert.equal(full.message, 'done');
  assert.deepEqual(full.artifacts, ['The answer']);
  // The flat summary shape (ListTasks).
  const flat = normalizeTask({ id: 't2', contextId: 'c2', state: 'TASK_STATE_WORKING', principal: 'operator', updated: 9 });
  assert.equal(flat.state, 'TASK_STATE_WORKING');
  assert.equal(flat.updated, 9);
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
