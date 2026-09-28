// SPDX-License-Identifier: AGPL-3.0-only
// The observation driver against agents that are not agentd, or that say no:
// a stock A2A 1.0 agent is observed with core methods only; a failure that no
// retry can fix stops every loop; a declared feed is used, and one the agent
// does not serve degrades to core mode.
import test from 'node:test';
import assert from 'node:assert/strict';
import {
  A2aClient,
  COMMAND_EXTENSION,
  CORE_METHODS,
  EVENTS_EXTENSION,
  EVENTS_METHOD,
  Mirror,
  OPS,
  Observation,
  userMessage,
} from '../dist/client/index.js';
import { cardCache } from '../dist/client/discovery.js';
import { startFakeA2a } from './fake-a2a.mjs';

const at = (s) => new Date(Date.UTC(2026, 8, 1, 0, 0, s)).toISOString();
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

async function until(pred, ms = 3000, what = 'condition') {
  const end = Date.now() + ms;
  while (!pred()) {
    if (Date.now() > end) assert.fail(`timed out waiting for ${what}`);
    await sleep(5);
  }
}

/** Observe `fake`, stopping when the test ends. */
function observe(t, fake, opts = {}) {
  // Ports are reused across fakes, and the card cache is keyed by URL.
  cardCache.clear();
  const mirror = new Mirror();
  const got = { mirror, terminal: [] };
  const obs = new Observation(
    {
      configured: fake.url,
      pollMs: 40,
      backoffCapMs: 60,
      ...opts,
      onSession: (session, client) => Object.assign(got, { session, client }),
      onTerminal: (f) => got.terminal.push(f),
    },
    mirror,
  );
  got.obs = obs;
  obs.start();
  t.after(() => obs.stop());
  return got;
}

/** Every JSON-RPC request is a core method, and none activates an extension. */
function assertCoreOnly(fake) {
  const core = new Set(CORE_METHODS);
  for (const r of fake.rpcCalls()) {
    assert.ok(core.has(r.rpc), `${r.rpc} is not an A2A 1.0 core method`);
    assert.equal(r.headers['a2a-extensions'], undefined, `${r.rpc} activated an extension`);
    const parts = r.params?.message?.parts ?? [];
    assert.ok(!parts.some((p) => p.data !== undefined), `${r.rpc} sent a DataPart`);
  }
}

test('against a stock A2A 1.0 agent the client uses only core methods', async (t) => {
  const fake = await startFakeA2a();
  t.after(() => fake.close());
  // More finished tasks than one page holds, and one task still working.
  for (let i = 0; i < 150; i++) {
    fake.putTask({ id: `old-${i}`, contextId: 'c-old', status: { state: 'TASK_STATE_COMPLETED', timestamp: at(0) }, artifacts: [] });
  }
  const w1 = fake.putTask({
    id: 'w1',
    contextId: 'c1',
    status: { state: 'TASK_STATE_WORKING', timestamp: at(1) },
    history: [{ role: 'ROLE_USER', messageId: 'mw', parts: [{ text: 'long job' }] }],
    artifacts: [],
  });
  // The follower's first stream ends with the task still working; the
  // resume carries Last-Event-ID and sees it finish.
  fake.handle('SubscribeToTask', ({ id }, rec) => {
    if (id !== 'w1') return { stream: [{ task: fake.tasks.get(id) }] };
    if (rec.headers['last-event-id'] === undefined) return { stream: [{ task: w1 }] };
    w1.status = { state: 'TASK_STATE_COMPLETED', timestamp: at(9) };
    w1.artifacts = [{ artifactId: 'w1.out', parts: [{ text: 'job done' }] }];
    return {
      stream: [
        { artifactUpdate: { taskId: 'w1', contextId: 'c1', artifact: w1.artifacts[0] } },
        { statusUpdate: { taskId: 'w1', contextId: 'c1', status: w1.status } },
      ],
    };
  });
  // A streaming send: the reply arrives in chunks, then the task completes.
  fake.handle('SendStreamingMessage', ({ message }) => {
    const first = { id: 's1', contextId: 'c2', status: { state: 'TASK_STATE_WORKING', timestamp: at(2) }, history: [message], artifacts: [] };
    const done = { state: 'TASK_STATE_COMPLETED', timestamp: at(3) };
    fake.putTask({ ...first, status: done, artifacts: [{ artifactId: 's1.out', parts: [{ text: 'Hello!' }] }] });
    const art = (text, append) => ({ artifactUpdate: { taskId: 's1', contextId: 'c2', artifact: { artifactId: 's1.out', parts: [{ text }] }, append } });
    return { stream: [{ task: first }, art('Hel', false), art('lo!', true), { statusUpdate: { taskId: 's1', contextId: 'c2', status: done } }] };
  });

  const got = observe(t, fake);
  const { mirror } = got;
  await until(() => mirror.getState().conn === 'polling', 3000, 'core mode');
  await until(() => mirror.getState().transcript.some((e) => e.text === 'job done'), 3000, 'the followed task to finish');
  const subs = fake.rpcCalls('SubscribeToTask').filter((r) => r.params.id === 'w1');
  assert.equal(subs.length, 2);
  assert.equal(subs[1].headers['last-event-id'], '1', 'the follower resumed after the last SSE id');

  // This client's own send streams; the CRLF-framed frames fold in.
  const a2a = new A2aClient(got.session.ep);
  await a2a.sendStreamingMessage(userMessage('hi', { messageId: 'm-hi' }), { returnImmediately: false }, (f) => mirror.applyStream(f));
  const rows = mirror.getState().transcript.filter((e) => e.taskId === 's1').map((e) => [e.kind, e.text]);
  assert.deepEqual(rows, [['user', 'hi'], ['agent', 'Hello!']]);
  // …and a cancel.
  fake.putTask({ id: 'x1', contextId: 'c3', status: { state: 'TASK_STATE_WORKING', timestamp: at(4) } });
  await got.client.cancelTask('x1');
  await sleep(100);

  assertCoreOnly(fake);
  // ListTasks: pages of 100 with artifacts and history, following the token.
  const lists = fake.rpcCalls('ListTasks');
  for (const r of lists) {
    assert.equal(r.params.pageSize, 100);
    assert.equal(r.params.includeArtifacts, true);
  }
  assert.ok(lists.some((r) => r.params.pageToken === '100'), 'the second page was read');
  assert.ok(lists.some((r) => r.params.status === 'TASK_STATE_INPUT_REQUIRED'), 'gates are listed on their own');
  assert.ok(lists.some((r) => typeof r.params.statusTimestampAfter === 'string'), 'polls ask only for what changed');
  assert.ok(mirror.getState().tasks.has('old-120'), 'a task past page one is mirrored');
  assert.equal(mirror.getState().conn, 'polling');
});

test('core mode holds at most 8 task streams in Node, this client\'s own first', async (t) => {
  const fake = await startFakeA2a();
  t.after(() => fake.close());
  for (let i = 0; i < 12; i++) {
    // `own` is the oldest: without a preference it would be followed last.
    fake.putTask({ id: i === 0 ? 'own' : `w${i}`, contextId: 'c', status: { state: 'TASK_STATE_WORKING', timestamp: at(i) } });
  }
  let release;
  const held = new Promise((r) => (release = r));
  t.after(() => release());
  fake.handle('SubscribeToTask', ({ id }) => ({ stream: [{ task: fake.tasks.get(id) }], hold: held }));
  const got = observe(t, fake, { pollMs: 1000 });
  got.obs.track('own');
  await until(() => fake.open.SubscribeToTask === 8, 3000, 'eight streams');
  await sleep(100);
  assert.equal(fake.maxOpen.SubscribeToTask, 8);
  const followed = fake.rpcCalls('SubscribeToTask').map((r) => r.params.id);
  assert.ok(followed.includes('own'), `own task followed (got ${followed.join(', ')})`);
});

test('core mode without streaming sends with returnImmediately and polls the task', async (t) => {
  const fake = await startFakeA2a();
  t.after(() => fake.close());
  fake.card.capabilities.streaming = false;
  // A newer task: the listing's `statusTimestampAfter` then leaves out the
  // one sent below, as a listing that lags would.
  fake.putTask({ id: 'recent', contextId: 'c0', status: { state: 'TASK_STATE_COMPLETED', timestamp: at(5) } });
  const got = observe(t, fake);
  await until(() => got.mirror.getState().conn === 'polling', 3000, 'core mode');
  const sent = await got.client.send('hello');
  got.obs.track(sent.task.id);
  got.mirror.localEcho(sent.messageId, sent.task.contextId, 'hello', sent.task.id);
  // The agent finishes it without touching the timestamp a listing filters on.
  const stored = fake.tasks.get(sent.task.id);
  const reads = fake.rpcCalls('GetTask').length;
  stored.status = { ...stored.status, state: 'TASK_STATE_COMPLETED' };
  stored.artifacts = [{ artifactId: 'r', parts: [{ text: 'hi there' }] }];
  await until(() => got.mirror.getState().transcript.some((e) => e.text === 'hi there'), 3000, 'the reply');
  assert.ok(fake.rpcCalls('GetTask').length > reads, 'the own task was read with GetTask');
  assert.equal(fake.rpcCalls('SendMessage')[0].params.configuration.returnImmediately, true);
  assert.equal(fake.rpcCalls('SubscribeToTask').length, 0, 'no stream on an agent that does not stream');
  assertCoreOnly(fake);
});

test('--no-extensions speaks core only to an agent that declares extensions', async (t) => {
  const fake = await startFakeA2a();
  t.after(() => fake.close());
  fake.card.capabilities.extensions = [
    { uri: EVENTS_EXTENSION, params: {} },
    { uri: COMMAND_EXTENSION, params: { ops: [{ op: OPS.status, reply: 'message' }] } },
  ];
  const got = observe(t, fake, { noExtensions: true });
  await until(() => got.mirror.getState().conn === 'polling', 3000, 'core mode');
  await sleep(150);
  assert.equal(got.mirror.getState().conn, 'polling');
  assert.equal(fake.rpcCalls(EVENTS_METHOD).length, 0);
  assert.equal(fake.rpcCalls('SendMessage').length, 0, 'no status read');
  assertCoreOnly(fake);
});

test('terminal failures stop every loop', async (t) => {
  const cases = [
    {
      fail: { code: -31401, message: 'unauthenticated', status: 401, headers: { 'www-authenticate': 'Bearer error="invalid_token"' } },
      conn: 'unauthenticated',
      message: /sign in again/,
    },
    { fail: { code: -31403, message: 'not yours', status: 403 }, conn: 'forbidden', message: /not permitted/ },
    { fail: { code: -32009, message: 'A2A-Version 2.0 required' }, conn: 'incompatible', message: /1\.0/ },
  ];
  for (const c of cases) {
    const fake = await startFakeA2a();
    t.after(() => fake.close());
    fake.fail('*', c.fail);
    // A retry would come after one backoff (200 ms); the bootstrap's other
    // parallel reads, sent before the answer, may land just after it.
    const got = observe(t, fake, { backoffCapMs: 200 });
    await until(() => got.mirror.getState().conn === c.conn, 3000, c.conn);
    const ended = Date.now();
    assert.match(got.mirror.getState().error, c.message);
    await sleep(600);
    const after = fake.requests.filter((r) => r.at > ended + 100);
    assert.deepEqual(after.map((r) => r.rpc ?? r.path), [], `${c.conn}: no request after the terminal answer`);
    assert.equal(got.terminal.length, 1, `${c.conn}: onTerminal once`);
    assert.equal(got.terminal[0].kind, c.conn);
  }
});

test('a rate limit waits out Retry-After; 5xx and a card that is not ready are retried', async (t) => {
  // 429: the next request comes no sooner than Retry-After.
  const limited = await startFakeA2a();
  t.after(() => limited.close());
  limited.fail('*', { code: -32603, message: 'slow down', status: 429, headers: { 'retry-after': '1' }, times: 1 });
  const a = observe(t, limited);
  await until(() => a.mirror.getState().conn === 'polling', 4000, 'recovery after the rate limit');
  const times = limited.rpcCalls().map((r) => r.at);
  const first = times[0];
  const later = times.find((x) => x - first > 100);
  assert.ok(later - first >= 950, `retried after ${later - first} ms`);
  assert.equal(a.terminal.length, 0);

  // 502 twice: backed off and retried, never terminal.
  const flaky = await startFakeA2a();
  t.after(() => flaky.close());
  flaky.fail('*', { code: -32603, message: 'bad gateway', status: 502, times: 2 });
  const b = observe(t, flaky);
  await until(() => b.mirror.getState().conn === 'polling', 3000, 'recovery after 502s');
  assert.equal(b.terminal.length, 0);

  // A card endpoint answering 503 is an agent that is not ready yet, not an
  // incompatible one.
  const starting = await startFakeA2a();
  t.after(() => starting.close());
  starting.cardStatus = 503;
  const c = observe(t, starting);
  await until(() => starting.cardGets().length >= 2, 3000, 'a second card read');
  assert.equal(c.terminal.length, 0, 'not ready is not incompatible');
  starting.cardStatus = undefined;
  await until(() => c.mirror.getState().conn === 'polling', 3000, 'the card once it is ready');
  assert.equal(c.terminal.length, 0);
});

test('events/v1 when declared, core mode on -32601', async (t) => {
  // Declared and served: the feed, one stream at a time, resumed after each
  // goodbye, with the extension activated and echoed.
  const feed = await startFakeA2a();
  t.after(() => feed.close());
  feed.card.capabilities.extensions = [{ uri: EVENTS_EXTENSION, params: {} }];
  feed.echo = true;
  let n = 0;
  feed.handle(EVENTS_METHOD, ({ fromSeq }) => {
    n++;
    return {
      stream: [
        { hello: { seq: fromSeq, resume: fromSeq, resync: false, introspection: false, version: 'x' } },
        { event: { seq: fromSeq + 1, ts: 1, kind: 'run', data: { id: `r${n}` } } },
        { goodbye: { seq: fromSeq + 1, reason: 'deadline' } },
      ],
      hold: sleep(30),
    };
  });
  const a = observe(t, feed);
  await until(() => n >= 4, 3000, 'resumed feed streams');
  assert.equal(a.mirror.getState().conn, 'ready');
  assert.equal(feed.maxOpen[EVENTS_METHOD], 1, 'one feed stream at a time');
  const subs = feed.rpcCalls(EVENTS_METHOD);
  assert.deepEqual(subs.slice(0, 4).map((r) => r.params.fromSeq), [0, 1, 2, 3]);
  for (const r of subs) assert.equal(r.headers['a2a-extensions'], EVENTS_EXTENSION);
  assert.equal(feed.rpcCalls('SubscribeToTask').length, 0, 'the feed replaces task streams');

  // Declared but not served: core mode, and the feed is not asked again.
  const liar = await startFakeA2a();
  t.after(() => liar.close());
  liar.card.capabilities.extensions = [{ uri: EVENTS_EXTENSION, params: {} }];
  const b = observe(t, liar, { pollMs: 50 });
  await until(() => b.mirror.getState().conn === 'polling', 3000, 'core mode');
  await sleep(300);
  assert.equal(b.mirror.getState().conn, 'polling');
  assert.equal(liar.rpcCalls(EVENTS_METHOD).length, 1);
  assert.ok(liar.rpcCalls('ListTasks').length > 5, 'polling');
});
