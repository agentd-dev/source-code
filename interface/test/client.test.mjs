// SPDX-License-Identifier: AGPL-3.0-only
// Unit tests for the thin-client core: AgentdClient (card-gated extension
// calls, the messages and headers it sends, the replies it reads), task-shape
// normalization, the mirror's convergence behaviour — including the
// cross-client transcript — and a source scan that keeps the extension
// vocabulary and the JSON-RPC method names in one place.
import test from 'node:test';
import assert from 'node:assert/strict';
import { readdirSync, readFileSync, statSync } from 'node:fs';
import { dirname, join, relative } from 'node:path';
import { fileURLToPath } from 'node:url';
import {
  AgentdClient,
  ClientError,
  COMMAND_EXTENSION,
  CORE_METHODS,
  EVENTS_EXTENSION,
  EVENTS_METHOD,
  Mirror,
  OPS,
  TASK_ANNOTATIONS_EXTENSION,
  capabilitiesOf,
  commandReply,
  normalizeTask,
  sseParser,
} from '../dist/client/index.js';

const EP = { url: 'http://127.0.0.1:9/' };
const ANN = TASK_ANNOTATIONS_EXTENSION;

/**
 * Capabilities as discovery derives them from a card declaring `exts` (URI →
 * params); `streaming` is the card's own flag.
 */
function capsOf(exts = {}, { streaming = true, noExtensions = false } = {}) {
  const extensions = Object.entries(exts).map(([uri, params]) => ({ uri, params }));
  return capabilitiesOf({ capabilities: { streaming, extensions } }, null, { noExtensions });
}

const ALL_OPS = Object.values(OPS).map((op) => ({ op, reply: 'task' }));
/** Every agentd extension declared, every op offered. */
const FULL = () => capsOf({ [COMMAND_EXTENSION]: { ops: ALL_OPS }, [EVENTS_EXTENSION]: {}, [ANN]: {} });

/**
 * Replace fetch for one test. `reply(req)` gets {url, body, headers} and
 * returns {result} or {stream: [results]}, with optional response `headers`.
 */
function stubFetch(t, reply = () => ({ result: { task: { id: 't1', status: { state: 'TASK_STATE_WORKING' } } } })) {
  const calls = [];
  const orig = globalThis.fetch;
  globalThis.fetch = async (url, init) => {
    const req = { url: String(url), body: JSON.parse(init.body), headers: init.headers };
    calls.push(req);
    const r = reply(req);
    if (r.stream) {
      const text = r.stream.map((result) => `data: ${JSON.stringify({ jsonrpc: '2.0', id: req.body.id, result })}\r\n\r\n`).join('');
      return new Response(text, { status: 200, headers: { 'content-type': 'text/event-stream', ...(r.headers ?? {}) } });
    }
    return new Response(JSON.stringify({ jsonrpc: '2.0', id: req.body.id, result: r.result }), {
      status: 200,
      headers: { 'content-type': 'application/json', ...(r.headers ?? {}) },
    });
  };
  t.after(() => {
    globalThis.fetch = orig;
  });
  return calls;
}

async function rejects(p, kind) {
  let err;
  try {
    await p;
  } catch (e) {
    err = e;
  }
  assert.ok(err instanceof ClientError, `expected ClientError(${kind}), got ${err}`);
  assert.equal(err.kind, kind, err.message);
  return err;
}

// ---- what the client sends ------------------------------------------------------

test('messages carry ROLE_USER and a taskId is never paired with a contextId', async (t) => {
  const calls = stubFetch(t);
  const c = new AgentdClient(EP, FULL());

  // Answering a gate names the task only: the server infers the context, so
  // two ids that could disagree are never sent.
  await c.send('the blue one', { contextId: 'c1', taskId: 't1' });
  await c.send('hello', { contextId: 'c1' });
  const [answer, hello] = calls.map((r) => r.body.params);
  assert.equal(answer.message.role, 'ROLE_USER');
  assert.equal(answer.message.taskId, 't1');
  assert.ok(!('contextId' in answer.message), 'a taskId never travels with a contextId');
  assert.deepEqual(answer.message.parts, [{ text: 'the blue one' }]);
  assert.equal(hello.message.contextId, 'c1');
  assert.ok(!('taskId' in hello.message));
  // returnImmediately is explicit on every send: absent means "wait for the
  // whole turn" in 1.0, and `blocking` is not a 1.0 field at all.
  assert.equal(answer.configuration.returnImmediately, true);

  // A command: one DataPart, marked with the extension, never a taskId, and
  // `op` wins over an arg that happens to be named `op`.
  calls.length = 0;
  await c.command(OPS.adminPause, { run: 'r1', op: 'admin.drain' }, { contextId: 'c1' });
  const cmd = calls[0].body.params;
  assert.equal(cmd.message.role, 'ROLE_USER');
  assert.deepEqual(cmd.message.extensions, [COMMAND_EXTENSION]);
  assert.ok(!('taskId' in cmd.message));
  assert.deepEqual(cmd.message.parts, [{ data: { agentd: { run: 'r1', op: OPS.adminPause } }, mediaType: 'application/json' }]);
  assert.equal(cmd.configuration.returnImmediately, false);

  // The typed wrappers speak the canonical argument names.
  calls.length = 0;
  await c.workflowRun('deploy', { env: 'prod' });
  await c.adminSet('agent.approval', 'ask');
  await c.authDeviceApprove('ABCD-EFGH', 'alice');
  await c.authDeviceApprove('ABCD-EFGH', 'bob', 'operator');
  await c.authDeviceDeny({ userCode: 'WXYZ-BCDF' });
  await c.authDeviceDeny({ all: true });
  await c.authSessionsRevoke({ sid: 'ds_1' });
  await c.authSessionsRevoke({ name: 'alice' });
  await c.authSessionsRevoke({ all: true });
  const sent = calls.map((r) => r.body.params.message.parts[0].data.agentd);
  assert.deepEqual(sent, [
    { workflow: 'deploy', inputs: { env: 'prod' }, op: OPS.workflowRun },
    { path: 'agent.approval', value: 'ask', op: OPS.adminSet },
    { user_code: 'ABCD-EFGH', as: 'alice', op: OPS.authDeviceApprove },
    { user_code: 'ABCD-EFGH', as: 'bob', scope: 'operator', op: OPS.authDeviceApprove },
    { user_code: 'WXYZ-BCDF', op: OPS.authDeviceDeny },
    { all: true, op: OPS.authDeviceDeny },
    { sid: 'ds_1', op: OPS.authSessionsRevoke },
    { name: 'alice', op: OPS.authSessionsRevoke },
    { all: true, op: OPS.authSessionsRevoke },
  ]);
  // A workflow run hands back the WORKING task; a mutation waits for its result.
  assert.equal(calls[0].body.params.configuration.returnImmediately, true);
  assert.equal(calls[1].body.params.configuration.returnImmediately, false);
  for (const r of calls) assert.equal(JSON.stringify(r.body).includes('blocking'), false);
});

test('extensions are refused locally when undeclared', async (t) => {
  const calls = stubFetch(t);

  // A stock A2A agent: no extension, no streaming.
  const bare = new AgentdClient(EP, capsOf({}, { streaming: false }));
  await rejects(bare.command(OPS.status), 'extension-not-declared');
  await rejects(bare.status(), 'extension-not-declared');
  await rejects(bare.pause(), 'extension-not-declared');
  await rejects(bare.subscribeEvents(0, () => {}, () => {}), 'extension-not-declared');
  await rejects(bare.subscribeTask('t1', () => {}), 'op-not-offered');
  assert.equal(bare.offers(OPS.status), false);

  // agentd's extensions declared, but this card offers only `status`.
  const narrow = new AgentdClient(EP, capsOf({ [COMMAND_EXTENSION]: { ops: [{ op: OPS.status, reply: 'message' }] } }));
  await rejects(narrow.pause(), 'op-not-offered');
  await rejects(narrow.adminSet('agent.approval', 'ask'), 'op-not-offered');
  await rejects(narrow.debugEvents(), 'op-not-offered');
  await rejects(narrow.subscribeEvents(0, () => {}, () => {}), 'extension-not-declared');

  // `--no-extensions`: a card declaring everything still gets core only.
  const off = new AgentdClient(EP, capsOf({ [COMMAND_EXTENSION]: { ops: ALL_OPS }, [EVENTS_EXTENSION]: {} }, { noExtensions: true }));
  await rejects(off.status(), 'extension-not-declared');
  await rejects(off.subscribeEvents(0, () => {}, () => {}), 'extension-not-declared');

  // An approval without the name it signs in as is refused before the wire.
  await rejects(new AgentdClient(EP, FULL()).authDeviceApprove('ABCD-EFGH', ' '), 'invalid-argument');

  assert.equal(calls.length, 0, 'nothing reached the network');
});

test('activation headers and echo verification', async (t) => {
  let echo;
  let result = { task: { id: 't1', status: { state: 'TASK_STATE_COMPLETED' }, metadata: { [ANN]: { principal: 'user:alice' } } } };
  const calls = stubFetch(t, (req) => {
    const headers = echo === undefined ? {} : { 'a2a-extensions': echo };
    if (req.body.method === EVENTS_METHOD) return { stream: [{ hello: { seq: 0 } }, { goodbye: { seq: 7, reason: 'deadline' } }], headers };
    if (req.body.method === 'ListTasks') return { result: { tasks: result.task ? [result.task] : [], nextPageToken: '' }, headers };
    // GetTask and CancelTask answer with the Task itself.
    if (req.body.method === 'GetTask' || req.body.method === 'CancelTask') return { result: result.task ?? null, headers };
    return { result, headers };
  });
  const header = (i) => calls[i].headers['a2a-extensions'];

  // Declared: a command activates command/v2 (+ annotations), the feed
  // events/v1 (+ annotations), a core task call the annotations alone.
  const c = new AgentdClient(EP, FULL());
  await c.status();
  const bye = await c.subscribeEvents(3, () => {}, () => {});
  await c.send('hi');
  await c.getTask('t1');
  await c.listTasks();
  assert.equal(header(0), `${COMMAND_EXTENSION}, ${ANN}`);
  assert.equal(header(1), `${EVENTS_EXTENSION}, ${ANN}`);
  assert.equal(calls[1].body.method, EVENTS_METHOD);
  assert.deepEqual(calls[1].body.params, { fromSeq: 3 });
  assert.deepEqual(bye, { seq: 7, reason: 'deadline' });
  for (const i of [2, 3, 4]) assert.equal(header(i), ANN, calls[i].body.method);

  // Not declared: nothing is claimed that is not used.
  calls.length = 0;
  const plain = new AgentdClient(EP, capsOf({ [COMMAND_EXTENSION]: { ops: ALL_OPS } }));
  await plain.status();
  await plain.send('hi');
  assert.equal(header(0), COMMAND_EXTENSION);
  assert.equal(header(1), undefined);
  // A card without task-annotations/v1 gets no annotations read, whatever
  // the reply carries under that key.
  assert.equal((await plain.send('hi')).task.principal, undefined);

  // No echo: advisory, the card governs — annotations are read.
  assert.equal((await c.send('hi')).task.principal, 'user:alice');
  // An echo that lists the annotations: read.
  echo = `${COMMAND_EXTENSION}, ${ANN}`;
  assert.equal((await c.command(OPS.adminPause)).task.principal, 'user:alice');
  // An echo WITHOUT the annotations: the key was not written under the
  // extension's contract, so the task is read without it.
  echo = COMMAND_EXTENSION;
  assert.equal((await c.command(OPS.adminPause)).task.principal, undefined);
  // The same rule on every core task call, not only on sends: the echo of
  // GetTask, ListTasks and CancelTask governs the task that call returned.
  echo = 'urn:x:other';
  assert.equal((await c.getTask('t1')).principal, undefined, 'GetTask');
  assert.equal((await c.listTasks()).tasks[0].principal, undefined, 'ListTasks');
  assert.equal((await c.cancelTask('t1')).principal, undefined, 'CancelTask');
  echo = ANN;
  assert.equal((await c.getTask('t1')).principal, 'user:alice', 'GetTask');
  assert.equal((await c.listTasks()).tasks[0].principal, 'user:alice', 'ListTasks');
  assert.equal((await c.cancelTask('t1')).principal, 'user:alice', 'CancelTask');
  // An echo without the command extension: the agent did not run this as a
  // command (a model may have read it as prose), so the answer is refused.
  echo = ANN;
  await rejects(c.command(OPS.adminPause), 'extension-not-activated');
  await rejects(c.status(), 'extension-not-activated');
  // …and a feed opened without events/v1 is refused before a frame lands.
  let frames = 0;
  await rejects(c.subscribeEvents(0, () => frames++, () => frames++), 'extension-not-activated');
  assert.equal(frames, 0);
  result = { message: { role: 'ROLE_AGENT', parts: [{ data: {} }] } };
  echo = '';
  await rejects(c.status(), 'extension-not-activated');
});

// ---- what the client reads ------------------------------------------------------

test('commandReply reads Message and Task DataParts', () => {
  // A read answers with a Message: its first DataPart is the document.
  const read = commandReply({
    message: { role: 'ROLE_AGENT', messageId: 'msg-1', parts: [{ text: 'status' }, { data: { runs: [] }, mediaType: 'application/json' }] },
  });
  assert.equal(read.kind, 'message');
  assert.deepEqual(read.data, { runs: [] });

  // Work answers with a Task: the result artifact's DataPart, found by id.
  const work = commandReply({
    task: {
      id: 't9',
      contextId: 'c',
      status: { state: 'TASK_STATE_COMPLETED', timestamp: '2026-09-01T00:00:00Z' },
      artifacts: [
        { artifactId: 't9.log', parts: [{ data: { lines: 3 } }] },
        { artifactId: 't9.result', parts: [{ data: { path: 'agent.approval', value: 'ask' } }] },
      ],
    },
  });
  assert.equal(work.kind, 'task');
  assert.equal(work.task.id, 't9');
  assert.deepEqual(work.data, { path: 'agent.approval', value: 'ask' });
  // Only THIS task's result artifact is the answer: another artifact's data
  // (a model's own, another task's result) is not, and is not guessed at.
  assert.equal(commandReply({ task: { id: 't', artifacts: [{ artifactId: 'x', parts: [{ data: 1 }] }] } }).data, undefined);
  assert.equal(commandReply({ task: { id: 't', artifacts: [{ artifactId: 'u.result', parts: [{ data: 1 }] }] } }).data, undefined);
  // A task still working has no result yet.
  assert.equal(commandReply({ task: { id: 't', status: { state: 'TASK_STATE_WORKING' } } }).data, undefined);

  // Nothing else is a command reply — not the bare objects of the old
  // surface, and not a result artifact's text parsed as JSON.
  for (const bad of [{ interface: {} }, { set: { value: 1 } }, null, 'status', { task: { noId: true } }]) {
    assert.throws(() => commandReply(bad), (e) => e instanceof ClientError && e.kind === 'invalid-response');
  }
  assert.equal(commandReply({ task: { id: 't', artifacts: [{ artifactId: 't.result', parts: [{ text: '{"a":1}' }] }] } }).data, undefined);
});

test('the mirror reads feed annotations only when the card declares them', () => {
  const task = { id: 't1', contextId: 'c', status: { state: 'TASK_STATE_WORKING' }, metadata: { [ANN]: { principal: 'user:alice' } } };
  const session = (caps) => ({ cardUrl: 'x', card: {}, extended: null, ep: EP, caps, warnings: [] });
  const principal = (caps) => {
    const m = new Mirror();
    if (caps) m.setSession(session(caps));
    m.apply({ seq: 1, ts: 1, kind: 'task', data: { task } });
    return m.getState().tasks.get('t1').principal;
  };
  assert.equal(principal(capsOf({ [EVENTS_EXTENSION]: {} })), undefined, 'not declared: not read');
  assert.equal(principal(FULL()), 'user:alice', 'declared: read');
  assert.equal(principal(null), 'user:alice', 'no session yet: read');
});

test('normalizeTask reads only URI-keyed annotations and core history', () => {
  const full = normalizeTask({
    id: 't1',
    contextId: 'c1',
    status: {
      state: 'TASK_STATE_INPUT_REQUIRED',
      timestamp: '2026-08-17T13:41:27.824Z',
      message: { role: 'ROLE_AGENT', parts: [{ text: 'Which' }, { text: 'region?' }] },
    },
    artifacts: [{ artifactId: 't1.result', parts: [{ text: 'The answer' }, { data: { n: 1 } }] }],
    history: [
      { role: 'ROLE_USER', messageId: 'm1', parts: [{ text: 'deploy it' }] },
      { role: 'ROLE_USER', messageId: 'm2', parts: [{ data: { agentd: { op: 'status' } } }] },
      { role: 'ROLE_AGENT', messageId: 't1.status.1', parts: [{ text: 'working on it' }] },
      { role: 'ROLE_UNSPECIFIED', messageId: 'm9', parts: [{ text: 'dropped' }] },
    ],
    metadata: {
      [ANN]: {
        link: { kind: 'run', id: 'r1' },
        principal: 'user:alice',
        command: 'workflow.run',
        created: '2026-08-17T13:41:00Z',
        statusHistory: [{ state: 'TASK_STATE_SUBMITTED', ts: '2026-08-17T13:41:00Z' }],
        askSchema: { type: 'string', enum: ['eu', 'us'] },
      },
    },
  });
  assert.equal(full.state, 'TASK_STATE_INPUT_REQUIRED');
  assert.equal(full.message, 'Which\nregion?');
  assert.deepEqual(full.artifacts, ['The answer']);
  assert.deepEqual(full.artifactData, [{ n: 1 }]);
  assert.deepEqual(full.history, [
    { messageId: 'm1', role: 'ROLE_USER', text: 'deploy it', data: [] },
    { messageId: 'm2', role: 'ROLE_USER', text: '', data: [{ agentd: { op: 'status' } }] },
    { messageId: 't1.status.1', role: 'ROLE_AGENT', text: 'working on it', data: [] },
  ]);
  assert.deepEqual(full.link, { kind: 'run', id: 'r1' });
  assert.equal(full.principal, 'user:alice');
  assert.equal(full.command, 'workflow.run');
  assert.equal(full.created, Date.parse('2026-08-17T13:41:00Z'));
  assert.deepEqual(full.statusHistory, [{ state: 'TASK_STATE_SUBMITTED', ts: Date.parse('2026-08-17T13:41:00Z') }]);
  assert.deepEqual(full.askSchema, { type: 'string', enum: ['eu', 'us'] });
  // RFC 3339 on the wire, epoch ms in the view: the TUI sorts and subtracts it.
  assert.equal(full.updated, Date.parse('2026-08-17T13:41:27.824Z'));

  // Read with the annotations off (the echo left them out): core fields only.
  const bare = normalizeTask({ id: 't1', status: { state: 'TASK_STATE_WORKING' }, metadata: { [ANN]: { principal: 'x' } } }, { annotations: false });
  assert.equal(bare.principal, undefined);
  assert.equal(bare.state, 'TASK_STATE_WORKING');

  // The pre-1.17 shapes are not read: `agentd/*` metadata keys, flat
  // top-level fields, a numeric timestamp.
  const old = normalizeTask({
    id: 't3',
    contextId: 'c3',
    state: 'TASK_STATE_WORKING',
    principal: 'operator',
    link: { run: { id: 'r1' } },
    updated: 9,
    status: { timestamp: 9 },
    metadata: {
      'agentd/principal': 'operator',
      'agentd/link': { run: { id: 'r1' } },
      'agentd/statusHistory': [{ state: 'TASK_STATE_SUBMITTED', ts: 1 }],
      'agentd/ask_schema': { type: 'string' },
    },
  });
  assert.equal(old.state, 'TASK_STATE_UNSPECIFIED');
  assert.equal(old.updated, 0);
  for (const k of ['principal', 'link', 'statusHistory', 'askSchema', 'message']) assert.equal(old[k], undefined, k);
  assert.deepEqual(old.history, []);

  // A link of a kind the extension does not define is not a link.
  assert.equal(normalizeTask({ id: 't', metadata: { [ANN]: { link: { kind: 'bogus', id: 'x' } } } }).link, undefined);
  assert.equal(normalizeTask({ noId: true }), null);
});

// ---- one home for the vocabulary ----------------------------------------------

const SRC = join(dirname(fileURLToPath(import.meta.url)), '..', 'src');

function sources(dir = SRC) {
  const out = [];
  for (const name of readdirSync(dir)) {
    const p = join(dir, name);
    if (statSync(p).isDirectory()) out.push(...sources(p));
    else if (/\.(ts|tsx|mjs|js)$/.test(name) && !/\.test\./.test(name)) {
      out.push({ path: relative(SRC, p).split('\\').join('/'), text: readFileSync(p, 'utf8') });
    }
  }
  return out;
}

test('extension URIs, op names and JSON-RPC methods have one home', () => {
  const files = sources();
  assert.ok(files.some((f) => f.path === 'client/client.ts'), 'the scan reads src/');

  // Every agentd extension URI is spelled in ext.ts and nowhere else.
  const uriHomes = files.filter((f) => f.text.includes('https://agentd.dev/a2a/')).map((f) => f.path);
  assert.deepEqual(uriHomes, ['client/ext.ts']);

  // So is every dotted command op, as a string literal.
  const dotted = Object.values(OPS).filter((op) => op.includes('.'));
  for (const f of files.filter((x) => x.path !== 'client/ext.ts')) {
    for (const op of dotted) {
      assert.ok(!f.text.includes(`'${op}'`) && !f.text.includes(`"${op}"`), `${f.path} spells the op ${op}`);
    }
  }

  // Every method string handed to the transport is an A2A 1.0 core method or
  // the one extension method.
  const allowed = new Set([...CORE_METHODS, EVENTS_METHOD]);
  const patterns = [
    /\b(?:rpc|rpcStream|call)\(\s*[\w.]+\s*,\s*(['"`])([^'"`]+)\1/g,
    /\bthis\.call\(\s*(['"`])([^'"`]+)\1/g,
    /\bCoreMethod\s*=\s*(['"`])([^'"`]+)\1/g,
    // A JSON-RPC body built by hand.
    /\bjsonrpc\s*:\s*(['"])2\.0\1\s*,[^}]*?\bmethod\s*:\s*(['"`])([^'"`]+)\2/g,
  ];
  const seen = new Set();
  for (const f of files) {
    for (const re of patterns) {
      for (const m of f.text.matchAll(re)) {
        const method = m[m.length - 1];
        seen.add(method);
        assert.ok(allowed.has(method), `${f.path} calls the JSON-RPC method ${method}`);
      }
    }
  }
  for (const m of ['SendMessage', 'ListTasks', 'SubscribeToTask']) assert.ok(seen.has(m), `the scan saw ${m}`);

  // The removed surface stays removed.
  for (const token of ['GetAgentCard', "'Pair'", '"Pair"', '"blocking"', "'blocking'", 'blocking:', 'agentd/link']) {
    const hits = files.filter((f) => f.text.includes(token)).map((f) => f.path);
    assert.deepEqual(hits, [], `${token} appears in ${hits.join(', ')}`);
  }
});

// ---- the SSE parser and the mirror ----------------------------------------------

test('sse parser handles chunk boundaries, multi-line data and comments', () => {
  const got = [];
  const feed = sseParser((ev) => got.push(ev.data));
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

/** An RFC 3339 timestamp `s` seconds into a fixed day. */
const at = (s) => new Date(Date.UTC(2026, 8, 1, 0, 0, s)).toISOString();

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
  m.apply({ seq: 2, ts: 110, kind: 'task', data: { task: { id: 't9', contextId: 'c9', status: { state: 'TASK_STATE_WORKING', timestamp: at(1) } } } });
  assert.equal(m.activeTasks().length, 1);
  m.apply({ seq: 3, ts: 120, kind: 'task', data: { task: { id: 't9', contextId: 'c9', status: { state: 'TASK_STATE_COMPLETED', timestamp: at(2) }, artifacts: [{ parts: [{ text: 'Hi!' }] }] } } });
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
  // A command completes as a task with a result artifact — but no prompt
  // ever carried its taskId, so the conversation stays clean.
  m.apply({ seq: 1, ts: 10, kind: 'task', data: { task: { id: 't-cmd', contextId: 'a2a-7', status: { state: 'TASK_STATE_COMPLETED', timestamp: at(1) }, artifacts: [{ artifactId: 't-cmd.result', parts: [{ text: 'paused' }] }] } } });
  assert.equal(m.getState().transcript.length, 0);
  assert.ok(m.getState().tasks.has('t-cmd'), 'still on the Tasks screen');
  // Whereas a task WITH a known prompt renders its reply.
  m.apply({ seq: 2, ts: 20, kind: 'message', data: { messageId: 'm1', contextId: 'c', taskId: 't-nl', text: 'hi' } });
  m.apply({ seq: 3, ts: 30, kind: 'task', data: { task: { id: 't-nl', contextId: 'c', status: { state: 'TASK_STATE_COMPLETED', timestamp: at(3) }, artifacts: [{ parts: [{ text: 'hello' }] }] } } });
  assert.equal(m.getState().transcript.filter((e) => e.kind === 'agent').length, 1);
});

test('input-required surfaces as an answerable agent row', () => {
  const m = new Mirror();
  m.apply({ seq: 1, ts: 10, kind: 'task', data: { task: { id: 't5', contextId: 'c5', status: { state: 'TASK_STATE_INPUT_REQUIRED', timestamp: at(1), message: { parts: [{ text: 'Which region?' }] } } } } });
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

test('a gate schema becomes the form a person can actually answer', async () => {
  const { askForm, askAnswer } = await import('../dist/client/index.js');

  // Single choice. The gate says "one of these three"; the client should offer
  // three options rather than a text box the person guesses the wording for.
  const one = askForm({ type: 'string', enum: ['file', 'hold', 'reject'] });
  assert.equal(one.kind, 'one');
  assert.deepEqual(one.options, ['file', 'hold', 'reject']);
  assert.equal(one.other, false, 'no free text unless the schema allows it');
  assert.equal(askAnswer(one, ['hold'], ''), 'hold');

  // The common gate shape is a one-property object; the person should be asked
  // the question, not shown a JSON envelope.
  const wrapped = askForm({
    type: 'object',
    properties: { decision: { type: 'string', enum: ['approve', 'deny'] } },
  });
  assert.equal(wrapped.kind, 'one');
  assert.deepEqual(wrapped.options, ['approve', 'deny']);

  // Multi-select.
  const many = askForm({ type: 'array', items: { enum: ['a', 'b', 'c'] } });
  assert.equal(many.kind, 'many');
  assert.deepEqual(askAnswer(many, ['a', 'c'], ''), ['a', 'c']);

  // "Other" is offered ONLY when the schema says a value outside the list is
  // acceptable — otherwise a free-text box invites an answer that is then
  // rejected, which is worse than not offering it.
  const withOther = askForm({
    anyOf: [{ enum: ['red', 'green'] }, { type: 'string' }],
  });
  assert.equal(withOther.kind, 'one');
  assert.equal(withOther.other, true);
  assert.equal(askAnswer(withOther, ['__other__'], 'chartreuse'), 'chartreuse');

  const manyOther = askForm({ type: 'array', items: { anyOf: [{ enum: ['x'] }, { type: 'string' }] } });
  assert.equal(manyOther.other, true);
  assert.deepEqual(askAnswer(manyOther, ['x', '__other__'], 'y'), ['x', 'y']);

  // Booleans, and the fallback that has always existed.
  assert.equal(askForm({ type: 'boolean' }).kind, 'bool');
  assert.equal(askAnswer(askForm({ type: 'boolean' }), ['yes'], ''), true);
  assert.equal(askForm(undefined).kind, 'text');
  assert.equal(askForm({ type: 'string' }).kind, 'text');
  // A default the schema declares is carried so the client can preselect it.
  assert.equal(askForm({ type: 'string', enum: ['a', 'b'], default: 'b' }).def, 'b');
});

test('durations are measured, formatted at the right precision, and honest about gaps', async () => {
  const { duration, Mirror } = await import('../dist/client/index.js');

  // Most steps finish in milliseconds; rendering those as "0s" throws away the
  // only interesting thing about them.
  assert.equal(duration(120), '120ms');
  assert.equal(duration(1400), '1.4s');
  assert.equal(duration(12_000), '12s');
  assert.equal(duration(125_000), '2m05s');
  assert.equal(duration(-5), '0ms', 'a clock skew must not render as negative');

  // A step's duration is measured across the two events the client saw.
  const m = new Mirror();
  m.apply({ seq: 1, ts: 1, kind: 'step', data: { run: 'r', step: 's1', kind: 'noop', phase: 'start' } });
  await new Promise((r) => setTimeout(r, 25));
  m.apply({ seq: 2, ts: 2, kind: 'step', data: { run: 'r', step: 's1', phase: 'done', status: 'done' } });
  const row = m.state.steps.get('r')[0];
  assert.ok(row.ms >= 20, `expected a measured duration, got ${row.ms}`);

  // A step whose START was never seen (client attached mid-run) reports NO
  // duration rather than one measured from when we happened to look.
  m.apply({ seq: 3, ts: 3, kind: 'step', data: { run: 'r', step: 'late', phase: 'done', status: 'done' } });
  const late = m.state.steps.get('r').find((x) => x.step === 'late');
  assert.equal(late.ms, undefined, 'an unobserved start must not be invented');
});
