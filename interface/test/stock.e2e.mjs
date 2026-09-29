// SPDX-License-Identifier: AGPL-3.0-only
// INTEROP: the TypeScript client against someone else's reading of A2A 1.0 —
// the official hello-world sample on the official Python SDK (a2a-sdk
// 1.1.5), not agentd and not this repository's fake. The fake answers the
// way its author read the spec; the sample answers the way the SDK's
// authors did, and where the two readings differ this is the test that
// says so.
//
// The client must discover the sample from its card, observe it with core
// methods alone (it declares no extension), converse with it, and read what
// the SDK sends back — while sending nothing a stock server would not
// expect: the A2A-Version header on every call, and no extension, no
// DataPart and no credential.
//
//   AGENTD_STOCK_PYTHON=/path/to/venv/bin/python node --test test/stock.e2e.mjs
import test from 'node:test';
import assert from 'node:assert/strict';
import {
  A2A_VERSION,
  A2aClient,
  AgentdClient,
  CORE_METHODS,
  Mirror,
  Observation,
  RpcError,
  TERMINAL_STATES,
  openSession,
  userMessage,
} from '../dist/client/index.js';
import { cardCache } from '../dist/client/discovery.js';
import { skipUnless, startHelloAgent, stockUnavailable, until } from './stock/harness.mjs';

const skip = skipUnless(stockUnavailable());

test('stock: the client discovers, observes and converses with the official hello-world agent', { skip }, async (t) => {
  const hello = await startHelloAgent();
  t.after(() => hello.stop());
  cardCache.clear();

  // Discovery: the sample's card, its one JSON-RPC 1.0 interface, and no
  // extension — so no agentd capability is assumed.
  const session = await openSession(hello.url);
  assert.equal(session.card.name, 'Hello World Agent');
  assert.equal(session.ep.url.replace(/\/$/, ''), hello.url);
  assert.equal(session.caps.command, null);
  assert.equal(session.caps.events, null);
  assert.equal(session.caps.annotations, false);
  assert.equal(session.caps.streaming, true);

  // An observing client, in core mode: with no feed declared it polls
  // ListTasks and follows what moves.
  const mirror = new Mirror();
  const obs = new Observation({ configured: hello.url, pollMs: 100 }, mirror);
  obs.start();
  t.after(() => obs.stop());
  await until(() => mirror.getState().conn === 'polling', 10000, 'the observer polling');

  // A second client converses: a blocking send is answered with the
  // finished task, the sample's artifact on it.
  const client = new AgentdClient(session.ep, session.caps);
  const sent = await client.send('hi', { returnImmediately: false });
  const task = sent.task;
  assert.ok(task, JSON.stringify(sent));
  assert.equal(task.state, 'TASK_STATE_COMPLETED');
  assert.deepEqual(task.artifacts, ['Hello, World! I have received your request (hi)']);
  assert.ok(task.history.some((m) => m.role === 'ROLE_USER' && m.text === 'hi'), 'the user turn is in Task.history');
  assert.equal(task.principal, undefined, 'no annotations are read from a peer that declares none');

  // GetTask and ListTasks read the same task back.
  const got = await client.getTask(task.id);
  assert.equal(got?.state, 'TASK_STATE_COMPLETED');
  assert.deepEqual(got?.artifacts, task.artifacts);
  const listed = await client.listTasks();
  assert.ok(listed.tasks.some((x) => x.id === task.id), 'ListTasks lists it');

  // The observer converges on the other client's task, from core methods.
  await until(() => mirror.getState().tasks.get(task.id)?.state === 'TASK_STATE_COMPLETED', 10000, 'the task in the mirror');
  assert.ok(
    mirror.getState().transcript.some((e) => e.kind === 'user' && e.taskId === task.id && e.text === 'hi'),
    "the other client's turn on the observer's transcript",
  );

  // A streaming send: frames until a terminal status.
  const frames = [];
  await new A2aClient(session.ep).sendStreamingMessage(userMessage('stream please'), { returnImmediately: false }, (f) => frames.push(f));
  const states = frames.map((f) => f.task?.status?.state ?? f.statusUpdate?.status?.state).filter(Boolean);
  assert.ok(states.length > 0, JSON.stringify(frames));
  assert.ok(TERMINAL_STATES.has(states.at(-1)), `the stream ends terminal: ${states}`);
  assert.ok(
    frames.some((f) => f.artifactUpdate?.artifact?.parts?.some((p) => p.text?.includes('stream please'))),
    `an artifact frame carries the reply: ${JSON.stringify(frames)}`,
  );

  // Refusals are JSON-RPC errors the client can tell apart from a failure
  // of the wire: a finished task can neither be canceled nor followed.
  await assert.rejects(client.cancelTask(task.id), (e) => e instanceof RpcError && e.code === -32002, 'TaskNotCancelableError');
  await assert.rejects(client.subscribeTask(task.id, () => {}), (e) => e instanceof RpcError, 'no stream for a finished task');

  // What the client sent the stock server: core methods only, each with
  // the version header, and nothing a stock server was not told to expect.
  obs.stop();
  const calls = hello.rpc();
  assert.ok(calls.length > 0);
  const core = new Set(CORE_METHODS);
  for (const r of calls) {
    assert.ok(core.has(r.body.method), `${r.body.method} is not an A2A 1.0 core method`);
    assert.equal(r['a2a-version'], A2A_VERSION, `${r.body.method} without A2A-Version ${A2A_VERSION}`);
    assert.equal(r['a2a-extensions'], null, `${r.body.method} activated an extension`);
    assert.equal(r.authorization, false, `${r.body.method} carried a credential nobody asked for`);
    const parts = r.body.params?.message?.parts ?? [];
    assert.ok(!parts.some((p) => p.data !== undefined), `${r.body.method} sent a DataPart`);
  }
  const seen = new Set(calls.map((r) => r.body.method));
  for (const m of ['SendMessage', 'SendStreamingMessage', 'GetTask', 'ListTasks', 'CancelTask', 'SubscribeToTask']) {
    assert.ok(seen.has(m), `the run exercised ${m}: ${[...seen]}`);
  }
});
