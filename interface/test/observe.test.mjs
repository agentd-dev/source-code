// SPDX-License-Identifier: AGPL-3.0-only
// The observation driver and the mirror it keeps: the feed cursor across a
// daemon restart, a bootstrap that replaces what it describes, a transcript
// built from core Task.history, and task stream frames folded in.
import test from 'node:test';
import assert from 'node:assert/strict';
import {
  COMMAND_EXTENSION,
  EVENTS_EXTENSION,
  EVENTS_METHOD,
  Mirror,
  Observation,
  TASK_ANNOTATIONS_EXTENSION,
  normalizeTask,
} from '../dist/client/index.js';
import { cardCache } from '../dist/client/discovery.js';
import { startFakeA2a } from './fake-a2a.mjs';

/** An RFC 3339 timestamp `s` seconds into a fixed day. */
const at = (s) => new Date(Date.UTC(2026, 8, 1, 0, 0, s)).toISOString();
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

/** Wait until `pred()` holds (or fail after `ms`). */
async function until(pred, ms = 3000, what = 'condition') {
  const end = Date.now() + ms;
  while (!pred()) {
    if (Date.now() > end) assert.fail(`timed out waiting for ${what}`);
    await sleep(5);
  }
}

const hello = (seq, resume, resync = false) => ({ hello: { seq, resume, resync, introspection: false, version: '1.17.0' } });
const event = (seq, kind, data) => ({ event: { seq, ts: 1000 + seq, kind, data } });

/** A fake agent whose card declares the events feed. */
async function feedAgent() {
  // Ports are reused across fakes, and the card cache is keyed by URL.
  cardCache.clear();
  const fake = await startFakeA2a();
  fake.card.capabilities.extensions = [{ uri: EVENTS_EXTENSION, params: {} }];
  return fake;
}

test('the feed cursor survives a daemon restart without replay loops', async (t) => {
  // The mirror on its own: a client holding a high cursor meets a restarted
  // daemon, whose hello says resync and replays from 1.
  const m = new Mirror();
  m.getState().lastSeq = 5000;
  m.onHello({ seq: 3, resume: 5000, resync: true, introspection: false, version: 'x' });
  assert.equal(m.getState().lastSeq, 0, 'the cursor is reset before the replay');
  assert.equal(m.getState().epoch, 1);
  for (let round = 0; round < 2; round++) {
    // The second round is a replay overlap: nothing is applied twice.
    for (const seq of [1, 2, 3]) m.apply({ seq, ts: seq, kind: 'lifecycle', data: { draining: true, reason: `r${seq}` } });
    m.apply({ seq: 4, ts: 4, kind: 'step', data: { run: 'r', step: 'fetch', phase: 'start' } });
    assert.deepEqual(
      m.getState().transcript.map((e) => e.key),
      ['feed-1-1', 'feed-1-2', 'feed-1-3'],
    );
    assert.equal(m.getState().steps.get('r').length, 1, 'a replayed step start is not a second row');
    assert.equal(m.getState().feedLog.length, 4);
  }

  // The driver, against an agent: the next fromSeq is the goodbye's cursor
  // (here past the last event this caller saw), then — after a stream that
  // ended with no goodbye — the highest seq applied; a `revoked` goodbye ends
  // observation for good.
  const fake = await feedAgent();
  t.after(() => fake.close());
  const from = [];
  fake.handle(EVENTS_METHOD, ({ fromSeq }) => {
    from.push(fromSeq);
    switch (from.length) {
      case 1:
        return { stream: [hello(3, 5000, true), event(1, 'lifecycle', { paused: true }), event(2, 'lifecycle', { paused: false }), event(3, 'run', { id: 'r1' }), { goodbye: { seq: 4, reason: 'deadline' } }] };
      case 2:
        return { stream: [hello(6, 4), event(5, 'run', { id: 'r2' }), event(6, 'run', { id: 'r3' })] };
      default:
        return { stream: [hello(7, 6), { goodbye: { seq: 7, reason: 'revoked' } }] };
    }
  });
  const mirror = new Mirror();
  mirror.getState().lastSeq = 5000;
  const terminal = [];
  const obs = new Observation({ configured: fake.url, backoffCapMs: 20, onTerminal: (f) => terminal.push(f) }, mirror);
  obs.start();
  t.after(() => obs.stop());
  await until(() => mirror.getState().conn === 'unauthenticated', 3000, 'the revoked goodbye');
  assert.deepEqual(from, [5000, 4, 6]);
  assert.equal(mirror.getState().epoch, 1);
  assert.equal(terminal.length, 1);
  assert.match(mirror.getState().error, /sign in again/);
  // The resync re-bootstrapped at once: the listing ran twice.
  assert.equal(fake.rpcCalls('ListTasks').length, 2 * 5);
  const notes = mirror.getState().transcript.filter((e) => e.key.startsWith('feed-')).map((e) => e.key);
  assert.deepEqual(notes, ['feed-1-1', 'feed-1-2']);
  await sleep(150);
  assert.equal(from.length, 3, 'nothing is retried after revocation');
});

test('a task event without history is read once with GetTask', async (t) => {
  const fake = await feedAgent();
  t.after(() => fake.close());
  const task = {
    id: 'tx',
    contextId: 'cx',
    status: { state: 'TASK_STATE_WORKING', timestamp: at(1) },
    history: [{ role: 'ROLE_USER', messageId: 'mx', parts: [{ text: 'from another client' }] }],
  };
  fake.putTask(task);
  // The feed carries the task without its history, twice.
  const bare = { id: 'tx', contextId: 'cx', status: task.status };
  let n = 0;
  fake.handle(EVENTS_METHOD, () => {
    n++;
    return { stream: [hello(2, 0), event(n * 2 - 1, 'task', { task: bare }), event(n * 2, 'task', { task: bare }), { goodbye: { seq: n * 2, reason: 'deadline' } }], hold: sleep(30) };
  });
  // ListTasks does not see it (a listing that lags): only the feed does.
  fake.handle('ListTasks', () => ({ tasks: [], nextPageToken: '' }));
  const mirror = new Mirror();
  const obs = new Observation({ configured: fake.url, backoffCapMs: 20 }, mirror);
  obs.start();
  t.after(() => obs.stop());
  await until(() => mirror.getState().transcript.some((e) => e.text === 'from another client'), 3000, 'the prompt row');
  await until(() => n >= 3, 3000, 'a few feed streams');
  const gets = fake.rpcCalls('GetTask');
  assert.equal(gets.length, 1, 'one lazy read per task');
  assert.deepEqual(gets[0].params, { id: 'tx', historyLength: 10 });
});

test('a live config event re-reads the card; a replayed one does not', async (t) => {
  const fake = await feedAgent();
  t.after(() => fake.close());
  let n = 0;
  fake.handle(EVENTS_METHOD, ({ fromSeq }) => {
    n++;
    // Every stream opens with a config event at the hello's own seq — one
    // that happened before this subscription, replayed. The third stream
    // also carries one past the hello: a change happening now.
    const frames = [hello(fromSeq + 1, fromSeq), event(fromSeq + 1, 'config', { paths: ['a.b'], source: 'reload' })];
    if (n === 3) frames.push(event(fromSeq + 2, 'config', { paths: ['agent.approval'], source: 'admin.set' }));
    frames.push({ goodbye: { seq: fromSeq + (n === 3 ? 2 : 1), reason: 'deadline' } });
    return { stream: frames, hold: sleep(10) };
  });
  const mirror = new Mirror();
  const obs = new Observation({ configured: fake.url, backoffCapMs: 20 }, mirror);
  obs.start();
  t.after(() => obs.stop());
  await until(() => n >= 7, 3000, 'several subscriptions');
  // Discovery, then one refresh's discovery with the cache bypassed (the
  // card's max-age would otherwise have answered from memory) — and none for
  // the replayed events.
  assert.equal(fake.cardGets().length, 2);
});

test('an expired session ends observation', async (t) => {
  const fake = await feedAgent();
  t.after(() => fake.close());
  fake.card.capabilities.extensions = [];
  const mirror = new Mirror();
  const terminal = [];
  const credential = { token: 'agentd_at_x', expiresAt: Date.now() + 150 };
  const obs = new Observation({ configured: fake.url, credential, pollMs: 20, onTerminal: (f) => terminal.push(f) }, mirror);
  obs.start();
  t.after(() => obs.stop());
  await until(() => mirror.getState().conn === 'unauthenticated', 3000, 'the expiry');
  assert.equal(terminal.length, 1);
  assert.ok(mirror.getState().transcript.some((e) => /expires in a minute/.test(e.text)), 'warned first');
  const seen = fake.requests.length;
  await sleep(150);
  assert.equal(fake.requests.length, seen, 'polling stopped');
});

test('bootstrap replaces sections and clears the ones a scoped status omits', () => {
  const m = new Mirror();
  const s = m.getState();
  m.bootstrap({
    draining: true,
    runs: [{ id: 'r1' }],
    conversations: [{ id: 'c1' }],
    subagents: [{ handle: 'h1' }],
    children: [{ node: 7 }],
    activity: [{ id: '7', phase: 'thinking', round: 1, tokens_in: 0, tokens_out: 0, started_ms: 1, updated_ms: 1 }],
  });
  m.apply({ seq: 1, ts: 1, kind: 'step', data: { run: 'r1', step: 'fetch', phase: 'start' } });
  m.apply({ seq: 2, ts: 2, kind: 'status', data: { model: 'old' } });
  // A caller-scoped document (or a restarted daemon): only what it lists.
  m.bootstrap({ runs: [{ id: 'r2' }] });
  assert.deepEqual([...s.runs.keys()], ['r2']);
  for (const k of ['conversations', 'subagents', 'children', 'activity']) assert.equal(s[k].size, 0, k);
  assert.equal(s.steps.has('r1'), false, 'a run that is gone takes its steps with it');
  assert.equal(s.draining, false);
  assert.equal(s.status, undefined, 'the live status restarts from the new document');
  assert.deepEqual(m.workflows(), []);
  m.bootstrap({ workflows: [{ name: 'deploy' }, { name: 'triage' }] });
  assert.equal(s.runs.size, 0, 'a document that omits runs clears them too');
  assert.deepEqual(m.workflows(), ['deploy', 'triage'], 'without an extended card, status names the workflows');
});

test('the transcript comes from Task.history and reconciles the local echo', () => {
  const m = new Mirror();
  const rows = () => m.getState().transcript;
  const task = (t) => normalizeTask(t);

  // 1. This client's prompt, echoed before the agent answered.
  m.localEcho('m1', 'c', 'hi', 't1');
  assert.equal(rows().length, 1);
  assert.equal(rows()[0].pending, true);

  // 2. The task's history carries the same messageId: the same row, settled,
  // plus the reply.
  m.adoptTasks([task({
    id: 't1', contextId: 'c',
    status: { state: 'TASK_STATE_COMPLETED', timestamp: at(2) },
    history: [{ role: 'ROLE_USER', messageId: 'm1', parts: [{ text: 'hi' }] }],
    artifacts: [{ artifactId: 't1.result', parts: [{ text: 'Hello' }] }],
  })]);
  assert.deepEqual(rows().map((e) => [e.key, e.kind, e.text, e.pending ?? false]), [
    ['h-t1-m1', 'user', 'hi', false],
    ['task-t1', 'agent', 'Hello', false],
  ]);

  // 3. Another client's prompt, still working: its row is here too.
  m.adoptTasks([task({
    id: 't2', contextId: 'c',
    status: { state: 'TASK_STATE_WORKING', timestamp: at(3) },
    history: [{ role: 'ROLE_USER', messageId: 'm9', parts: [{ text: 'from web' }] }],
  })]);
  assert.ok(rows().some((e) => e.key === 'h-t2-m9' && e.kind === 'user' && e.text === 'from web'));

  // 4. A command: a user message that is only data is one `command` row,
  // named by the annotation, and no user row.
  const ann = { [TASK_ANNOTATIONS_EXTENSION]: { command: 'admin.pause' } };
  m.adoptTasks([task({
    id: 't3', contextId: 'c',
    status: { state: 'TASK_STATE_WORKING', timestamp: at(4) },
    history: [{ role: 'ROLE_USER', messageId: 'm3', parts: [{ data: { agentd: { op: 'admin.pause' } } }] }],
    metadata: ann,
  })]);
  const cmd = rows().filter((e) => e.taskId === 't3');
  assert.deepEqual(cmd.map((e) => [e.key, e.kind, e.text]), [['cmd-t3', 'command', 'admin.pause']]);

  // 5. A gate asks; the answer goes in; the question is now history.
  m.adoptTasks([task({
    id: 't4', contextId: 'c',
    status: { state: 'TASK_STATE_INPUT_REQUIRED', timestamp: at(5), message: { messageId: 't4.status.1', role: 'ROLE_AGENT', parts: [{ text: 'Which region?' }] } },
    history: [{ role: 'ROLE_USER', messageId: 'm4', parts: [{ text: 'deploy' }] }],
  })]);
  assert.ok(rows().some((e) => e.key === 'task-t4' && e.inputRequired && e.text === 'Which region?'));
  m.adoptTasks([task({
    id: 't4', contextId: 'c',
    status: { state: 'TASK_STATE_WORKING', timestamp: at(6) },
    history: [
      { role: 'ROLE_USER', messageId: 'm4', parts: [{ text: 'deploy' }] },
      { role: 'ROLE_AGENT', messageId: 't4.status.1', parts: [{ text: 'Which region?' }] },
      { role: 'ROLE_USER', messageId: 'm5', parts: [{ text: 'eu' }] },
    ],
  })]);
  assert.deepEqual(rows().filter((e) => e.taskId === 't4').map((e) => [e.key, e.text, e.inputRequired ?? false]), [
    ['h-t4-m4', 'deploy', false],
    ['h-t4-t4.status.1', 'Which region?', false],
    ['h-t4-m5', 'eu', false],
  ]);

  // 6. AUTH_REQUIRED is shown, and is not a gate a reply answers.
  m.adoptTasks([task({ id: 't5', contextId: 'c', status: { state: 'TASK_STATE_AUTH_REQUIRED', timestamp: at(7) } })]);
  const auth = rows().find((e) => e.key === 'task-t5');
  assert.match(auth.text, /authorization required/);
  assert.equal(auth.authRequired, true);
  assert.equal(auth.inputRequired, undefined);
});

test('applyStream folds stream frames', () => {
  const m = new Mirror();
  const s = m.getState();
  m.localEcho('m1', 'c', 'hi', 't');
  m.applyStream({ task: { id: 't', contextId: 'c', status: { state: 'TASK_STATE_SUBMITTED' }, history: [{ role: 'ROLE_USER', messageId: 'm1', parts: [{ text: 'hi' }] }] } });
  assert.equal(s.tasks.get('t').state, 'TASK_STATE_SUBMITTED');
  m.applyStream({ statusUpdate: { taskId: 't', contextId: 'c', status: { state: 'TASK_STATE_WORKING', timestamp: at(1) } } });
  assert.equal(s.tasks.get('t').state, 'TASK_STATE_WORKING');
  assert.equal(s.tasks.get('t').updated, Date.parse(at(1)));
  // An artifact in chunks: `append` continues the artifact with that id.
  m.applyStream({ artifactUpdate: { taskId: 't', contextId: 'c', artifact: { artifactId: 'a', parts: [{ text: 'He' }] } } });
  m.applyStream({ artifactUpdate: { taskId: 't', contextId: 'c', artifact: { artifactId: 'a', parts: [{ text: 'llo' }] }, append: true, lastChunk: true } });
  assert.deepEqual(s.tasks.get('t').artifacts, ['Hello']);
  // Without `append` an artifact is sent whole and replaces.
  m.applyStream({ artifactUpdate: { taskId: 't', contextId: 'c', artifact: { artifactId: 'b', parts: [{ text: 'draft' }] } } });
  m.applyStream({ artifactUpdate: { taskId: 't', contextId: 'c', artifact: { artifactId: 'b', parts: [{ text: 'final' }] } } });
  assert.deepEqual(s.tasks.get('t').artifacts, ['Hello', 'final']);
  m.applyStream({ statusUpdate: { taskId: 't', contextId: 'c', status: { state: 'TASK_STATE_COMPLETED', timestamp: at(2) } } });
  assert.deepEqual(s.transcript.map((e) => [e.key, e.kind, e.text]), [
    ['h-t-m1', 'user', 'hi'],
    ['task-t', 'agent', 'Hello'],
  ]);
  // A direct Message reply: an agent row keyed by its messageId.
  m.applyStream({ message: { role: 'ROLE_AGENT', messageId: 'r1', contextId: 'c', parts: [{ text: 'a direct answer' }] } });
  assert.ok(s.transcript.some((e) => e.key === 'msg-r1' && e.kind === 'agent' && e.text === 'a direct answer'));
  // A status update for a task this client never saw makes a stub.
  m.applyStream({ statusUpdate: { taskId: 'u', contextId: 'c2', status: { state: 'TASK_STATE_WORKING', timestamp: at(3) } } });
  assert.equal(s.tasks.get('u').contextId, 'c2');
});

test('a history message cannot name another row', () => {
  // Another principal's task, whose sender chose messageIds that are the
  // client's own row keys: a feed note, this client's gate, an echo.
  const m = new Mirror();
  const rows = () => m.getState().transcript;
  m.apply({ seq: 1, ts: 1, kind: 'lifecycle', data: { draining: true, reason: 'maintenance' } });
  const note = rows().find((e) => e.key === 'feed-0-1');
  assert.ok(note, 'the note is there');
  m.adoptTasks([normalizeTask({
    id: 'mine', contextId: 'c',
    status: { state: 'TASK_STATE_INPUT_REQUIRED', timestamp: at(1), message: { messageId: 'q', role: 'ROLE_AGENT', parts: [{ text: 'Approve the deploy?' }] } },
    history: [{ role: 'ROLE_USER', messageId: 'p1', parts: [{ text: 'deploy' }] }],
  })]);
  m.localEcho('m-own', 'c', 'my prompt', 'mine2');
  const before = JSON.stringify(rows().filter((e) => !e.key.startsWith('h-')));

  m.adoptTasks([normalizeTask({
    id: 'theirs', contextId: 'c',
    status: { state: 'TASK_STATE_WORKING', timestamp: at(2) },
    history: [
      { role: 'ROLE_USER', messageId: 'feed-0-1', parts: [{ text: 'spoofed note' }] },
      { role: 'ROLE_USER', messageId: 'task-mine', parts: [{ text: 'spoofed gate' }] },
      { role: 'ROLE_AGENT', messageId: 'cmd-mine', parts: [{ text: 'spoofed command' }] },
      { role: 'ROLE_USER', messageId: 'm-own', parts: [{ text: 'spoofed echo' }] },
    ],
  })]);
  // Every row the client made is exactly as it was…
  assert.equal(JSON.stringify(rows().filter((e) => !e.key.startsWith('h-') || e.taskId === 'mine')).includes('spoofed'), false);
  assert.equal(JSON.stringify(rows().filter((e) => !e.key.startsWith('h-'))), before);
  const gate = rows().find((e) => e.key === 'task-mine');
  assert.equal(gate.text, 'Approve the deploy?');
  assert.equal(gate.inputRequired, true);
  assert.equal(gate.taskId, 'mine');
  assert.equal(rows().find((e) => e.key === 'echo-m-own').pending, true, 'another task does not settle my echo');
  // …and the foreign messages are that task's own rows, carrying none of
  // the fields of the rows they named.
  const theirs = rows().filter((e) => e.taskId === 'theirs');
  assert.deepEqual(theirs.map((e) => e.key), ['h-theirs-feed-0-1', 'h-theirs-task-mine', 'h-theirs-cmd-mine', 'h-theirs-m-own']);
  assert.ok(theirs.every((e) => e.inputRequired === undefined));

  // The echo's own task does claim it: one row, settled, in the echo's place.
  const echoTs = rows().find((e) => e.key === 'echo-m-own').ts;
  m.adoptTasks([normalizeTask({
    id: 'mine2', contextId: 'c',
    status: { state: 'TASK_STATE_WORKING', timestamp: at(3) },
    history: [{ role: 'ROLE_USER', messageId: 'm-own', parts: [{ text: 'my prompt' }] }],
  })]);
  assert.equal(rows().some((e) => e.key === 'echo-m-own'), false);
  const settled = rows().find((e) => e.key === 'h-mine2-m-own');
  assert.equal(settled.pending, false);
  assert.equal(settled.ts, echoTs);
});

test('a resync whose re-bootstrap is refused ends observation', async (t) => {
  const fake = await feedAgent();
  t.after(() => fake.close());
  let subs = 0;
  fake.handle(EVENTS_METHOD, () => {
    subs++;
    // The second stream says resync, and by then the agent refuses the
    // caller: the re-bootstrap's listing answers 403.
    if (subs === 2) fake.fail('ListTasks', { code: -31403, message: 'not yours', status: 403 });
    return { stream: [hello(1, 0, subs === 2), { goodbye: { seq: 1, reason: 'deadline' } }], hold: sleep(subs === 2 ? 5000 : 10) };
  });
  const mirror = new Mirror();
  const terminal = [];
  const obs = new Observation({ configured: fake.url, backoffCapMs: 20, onTerminal: (f) => terminal.push(f) }, mirror);
  obs.start();
  t.after(() => obs.stop());
  // At once — not when the stream next ends and a reconnect bootstraps.
  await until(() => terminal.length > 0, 1500, 'the terminal refusal');
  assert.equal(mirror.getState().conn, 'forbidden');
  const seen = fake.requests.length;
  await sleep(150);
  assert.equal(fake.requests.length, seen, 'nothing after the refusal');
  assert.equal(subs, 2);
});

test('feed events that arrive during a resync re-bootstrap are applied after it', async (t) => {
  const fake = await feedAgent();
  t.after(() => fake.close());
  let release;
  const slow = new Promise((r) => (release = r));
  t.after(() => release());
  let subs = 0;
  fake.handle(EVENTS_METHOD, () => {
    subs++;
    if (subs > 1) return { stream: [hello(3, 3)], hold: sleep(2000) };
    // A restarted agent: resync, then a run the snapshot does not know yet.
    return { stream: [hello(2, 0, true), event(1, 'run', { id: 'r-new', status: 'running' })], hold: sleep(2000) };
  });
  // The status snapshot is slow, and older than the run event.
  fake.card.capabilities.extensions.push({ uri: COMMAND_EXTENSION, params: { ops: [{ op: 'status', reply: 'message' }] } });
  let statusReads = 0;
  fake.handle('SendMessage', async () => {
    statusReads++;
    if (statusReads > 1) await slow;
    return { message: { role: 'ROLE_AGENT', messageId: `s${statusReads}`, parts: [{ data: { runs: [] }, mediaType: 'application/json' }] } };
  });
  const mirror = new Mirror();
  const obs = new Observation({ configured: fake.url, backoffCapMs: 20 }, mirror);
  obs.start();
  t.after(() => obs.stop());
  await until(() => statusReads >= 2, 3000, 'the resync bootstrap');
  await sleep(50);
  assert.equal(mirror.getState().runs.has('r-new'), false, 'held until the snapshot lands');
  release();
  await until(() => mirror.getState().runs.has('r-new'), 3000, 'the held event, applied after the snapshot');
});
