// SPDX-License-Identifier: AGPL-3.0-only
// MONOREPO-ONLY: this test reads what agentd.dev publishes for each extension
// URI (web/lib/extensions.json, web/public/a2a/…) and the daemon's Rust
// constants (crates/agentd/src/runtime/surface/ext.rs), neither of which the
// npm package ships. It runs wherever `npm test` runs — inside the agentd tree.
//
// The client and the daemon ship separately, so what holds them together has
// to be a contract both are tested against — not the daemon's internals, and
// not the client's own reading of them. That contract is the published one:
// the registry, the schema bundle at `<uri>/schema.json`, and the golden
// examples beside it. The Rust side (contract_e2e.rs) validates the same
// files against the running daemon; this side holds the client to them:
//
// - every golden example validates against its bundle, with a validator that
//   is not agentd's (ajv), so the two sides cannot share one misreading;
// - the extension identifiers the client speaks are the daemon's, and carry
//   no version;
// - every command envelope the client can send validates, and for a golden
//   example it is exactly the example;
// - the feed frames, fed through the client's own frame reader, land in the
//   Mirror as the documents say they mean; and a command's reply is read
//   from where the extension puts it.
import test from 'node:test';
import assert from 'node:assert/strict';
import * as ext from '../dist/client/ext.js';
import {
  AgentdClient,
  COMMAND_DATA_KEY,
  COMMAND_EXTENSION,
  EVENTS_EXTENSION,
  Mirror,
  OPS,
  TASK_ANNOTATIONS_EXTENSION,
  capabilitiesOf,
  commandReply,
} from '../dist/client/index.js';
import { REGISTRY, bundleOf, check, examplesOf, readRepoJson, readRepoText, validator } from './schemas.mjs';

// ---- the published files ----------------------------------------------------

test('every golden example validates against the bundle published beside it', () => {
  const extensions = REGISTRY.filter((e) => e.kind === 'extension');
  assert.ok(extensions.length >= 3, JSON.stringify(REGISTRY));
  for (const entry of extensions) {
    const bundle = readRepoJson('web', 'public', entry.path, 'schema.json');
    assert.equal(bundle.$id, entry.schema, `${entry.path}: the bundle names its own address`);
    const validate = validator(entry.uri);
    const examples = examplesOf(entry.uri);
    assert.ok(examples.length > 0, `${entry.uri} publishes no example`);
    for (const [f, doc] of examples) check(validate, doc, `${entry.path}/examples/${f}`);
  }
});

test('the bundles refuse what they do not describe', () => {
  // A validator that accepts everything would pass every test above.
  const command = validator(COMMAND_EXTENSION);
  // (An op outside the vocabulary is not refused by the bundle: a workflow's
  // `kind: a2a` trigger declares a command of its own name.)
  assert.equal(command({ [COMMAND_DATA_KEY]: { op: OPS.status, extra: 1 } }), false, 'an argument the op does not take');
  assert.equal(command({ [COMMAND_DATA_KEY]: { op: OPS.runGet } }), false, 'a missing required argument');
  const events = validator(EVENTS_EXTENSION);
  assert.equal(events({ hello: { seq: 1 }, goodbye: { seq: 1, reason: 'deadline' } }), false, 'two frames in one');
  const ann = validator(TASK_ANNOTATIONS_EXTENSION);
  assert.equal(ann({ link: { kind: 'run', id: 'r' }, created: 'yesterday', statusHistory: [], principal: 'x' }), false);
});

// ---- the identifiers ----------------------------------------------------------

/** The value of `pub const <name>: &str = "…";` in ext.rs. */
function rustConst(src, name) {
  const m = new RegExp(`pub const ${name}: &str =\\s*"([^"\\\\]*)";`).exec(src);
  assert.ok(m, `pub const ${name} not found in ext.rs`);
  return m[1];
}

test("the client's extension identifiers are the daemon's, and none carries a version", () => {
  const src = readRepoText('crates', 'agentd', 'src', 'runtime', 'surface', 'ext.rs');
  const names = ['COMMAND_EXTENSION', 'EVENTS_EXTENSION', 'TASK_ANNOTATIONS_EXTENSION', 'UNIX_BINDING', 'EVENTS_METHOD'];
  for (const name of names) {
    assert.equal(typeof ext[name], 'string', `dist/client/ext.js exports ${name}`);
    assert.equal(ext[name], rustConst(src, name), `${name}: the client and the daemon disagree`);
    // An agentd-owned identifier names one thing; a version in it is a second
    // name for the same thing, and an incompatible change takes a new name.
    assert.doesNotMatch(ext[name], /\/v\d+$/, `${name} carries a version`);
  }
  // Every URI is one agentd.dev publishes, and every one it publishes the
  // client knows.
  const uris = names.filter((n) => n !== 'EVENTS_METHOD').map((n) => ext[n]);
  assert.deepEqual([...uris].sort(), REGISTRY.map((e) => e.uri).sort());
});

// ---- what the client sends ----------------------------------------------------

/**
 * How each op is sent through the client's typed wrapper, given the envelope
 * it should produce. Keyed by the op, so the table covers the published
 * vocabulary rather than whatever this file remembers of it.
 */
const SEND = {
  [OPS.status]: (c) => c.status(),
  [OPS.config]: (c) => c.config(),
  [OPS.workflowRun]: (c, e) => c.workflowRun(e.workflow, e.inputs),
  [OPS.workflowStatus]: (c, e) => c.workflowStatus(e.run),
  [OPS.workflowCancel]: (c, e) => c.workflowCancel(e.run),
  [OPS.workflowSignal]: (c, e) => c.signal(e.name, e.payload, e.run),
  [OPS.subagentSend]: (c, e) => c.subagentSend(e.handle, e.message),
  [OPS.subagentKill]: (c, e) => c.subagentKill(e.handle, e.reason),
  [OPS.subagentStatus]: (c, e) => c.subagentStatus(e.handle),
  [OPS.planGet]: (c, e) => c.planGet(e.id),
  [OPS.conversationGet]: (c, e) => c.conversationGet(e.id, e.limit),
  [OPS.runGet]: (c, e) => c.runGet(e.run),
  [OPS.subagentGet]: (c, e) => c.subagentGet(e.handle),
  [OPS.debugEvents]: (c, e) => c.debugEvents(e.after, e.limit, e.level),
  [OPS.adminDrain]: (c, e) => c.drain(e.reason),
  [OPS.adminPause]: (c, e) => c.pause(e.run),
  [OPS.adminResume]: (c, e) => c.resume(e.run),
  [OPS.adminCancel]: (c, e) => c.cancelRun(e.run, e.reason),
  [OPS.adminSet]: (c, e) => c.adminSet(e.path, e.value),
  [OPS.authDevicePending]: (c) => c.authDevicePending(),
  [OPS.authDeviceApprove]: (c, e) => c.authDeviceApprove(e.user_code, e.as, e.scope),
  [OPS.authDeviceDeny]: (c, e) => c.authDeviceDeny(e.all ? { all: true } : { userCode: e.user_code }),
  [OPS.authSessions]: (c) => c.authSessions(),
  [OPS.authSessionsRevoke]: (c, e) => c.authSessionsRevoke(e.sid ? { sid: e.sid } : e.name ? { name: e.name } : { all: true }),
};

/**
 * Envelopes beyond the golden examples, so every op — and each optional
 * argument a wrapper can leave out or pass — is sent at least once.
 */
const MORE = [
  { op: OPS.config },
  { op: OPS.workflowRun, workflow: 'greet' },
  { op: OPS.workflowStatus },
  { op: OPS.workflowStatus, run: 'greet-01' },
  { op: OPS.workflowCancel, run: 'greet-01' },
  { op: OPS.workflowSignal, name: 'go' },
  { op: OPS.workflowSignal, name: 'go', payload: { ok: true }, run: 'greet-01' },
  { op: OPS.subagentSend, handle: 'h1', message: 'hi' },
  { op: OPS.subagentKill, handle: 'h1' },
  { op: OPS.subagentKill, handle: 'h1', reason: 'done' },
  { op: OPS.subagentStatus, handle: 'h1' },
  { op: OPS.planGet },
  { op: OPS.planGet, id: 'chat1' },
  { op: OPS.conversationGet, id: 'chat1' },
  { op: OPS.conversationGet, id: 'chat1', limit: 20 },
  { op: OPS.runGet, run: 'greet-01' },
  { op: OPS.subagentGet, handle: 'h1' },
  { op: OPS.debugEvents, after: 0, limit: 50 },
  { op: OPS.debugEvents, after: 3, limit: 50, level: 'warn' },
  { op: OPS.adminDrain, reason: 'maintenance' },
  { op: OPS.adminPause },
  { op: OPS.adminPause, run: 'greet-01' },
  { op: OPS.adminResume },
  { op: OPS.adminResume, run: 'greet-01' },
  { op: OPS.adminCancel, run: 'greet-01' },
  { op: OPS.adminCancel, run: 'greet-01', reason: 'stuck' },
  { op: OPS.adminSet, path: 'agent.approval', value: 'ask' },
  { op: OPS.authDevicePending },
  { op: OPS.authDeviceApprove, user_code: 'WDJB-MJHT', as: 'bob' },
  { op: OPS.authDeviceDeny, user_code: 'WDJB-MJHT' },
  { op: OPS.authDeviceDeny, all: true },
  { op: OPS.authSessions },
  { op: OPS.authSessionsRevoke, sid: 's-1' },
  { op: OPS.authSessionsRevoke, name: 'alice.laptop' },
  { op: OPS.authSessionsRevoke, all: true },
];

/** Replace fetch for one test: record each JSON-RPC body, answer `reply(body)` with the echo `echo`. */
function stubFetch(t, reply, echo) {
  const calls = [];
  const orig = globalThis.fetch;
  globalThis.fetch = async (_url, init) => {
    const body = JSON.parse(init.body);
    calls.push(body);
    const r = reply(body);
    const headers = { 'content-type': r.stream ? 'text/event-stream' : 'application/json' };
    if (echo) headers['A2A-Extensions'] = echo.join(', ');
    if (r.stream) {
      const text = r.stream.map((result) => `data: ${JSON.stringify({ jsonrpc: '2.0', id: body.id, result })}\r\n\r\n`).join('');
      return new Response(text, { status: 200, headers });
    }
    return new Response(JSON.stringify({ jsonrpc: '2.0', id: body.id, result: r.result }), { status: 200, headers });
  };
  t.after(() => {
    globalThis.fetch = orig;
  });
  return calls;
}

/** A client of a card declaring every agentd extension and offering every op the client speaks. */
function fullClient() {
  const ops = Object.values(OPS).map((op) => ({ op, reply: 'task' }));
  const extensions = [
    { uri: COMMAND_EXTENSION, params: { ops } },
    { uri: EVENTS_EXTENSION, params: {} },
    { uri: TASK_ANNOTATIONS_EXTENSION, params: {} },
  ];
  return new AgentdClient({ url: 'http://127.0.0.1:9/' }, capabilitiesOf({ capabilities: { streaming: true, extensions } }, null));
}

test('the client speaks the published command vocabulary, and nothing outside it', () => {
  const bundle = bundleOf(COMMAND_EXTENSION);
  const published = Object.keys(bundle.$defs.ops).sort();
  assert.deepEqual(Object.values(OPS).sort(), published, 'OPS is the published op list');
  for (const op of Object.keys(bundle.$defs.reserved)) {
    assert.ok(!Object.values(OPS).includes(op), `${op} is reserved: no client sends it`);
  }
  assert.deepEqual(Object.keys(SEND).sort(), published, 'every op has a way to send it');
});

test('every envelope the client sends validates, and a golden example is sent exactly', async (t) => {
  const validate = validator(COMMAND_EXTENSION);
  const calls = stubFetch(
    t,
    () => ({ result: { task: { id: 't1', contextId: 'c1', status: { state: 'TASK_STATE_COMPLETED' } } } }),
    [COMMAND_EXTENSION, TASK_ANNOTATIONS_EXTENSION],
  );
  const c = fullClient();
  const golden = examplesOf(COMMAND_EXTENSION).map(([f, doc]) => [`examples/${f}`, doc[COMMAND_DATA_KEY]]);
  const all = [...golden, ...MORE.map((e, i) => [`MORE[${i}]`, e])];
  const sentOps = new Set();
  for (const [what, envelope] of all) {
    const send = SEND[envelope.op];
    assert.ok(send, `${what}: no wrapper sends ${envelope.op}`);
    calls.length = 0;
    await send(c, envelope);
    assert.equal(calls.length, 1, what);
    const { message } = calls[0].params;
    assert.deepEqual(message.extensions, [COMMAND_EXTENSION], `${what}: the message is marked as a command`);
    assert.equal(message.parts.length, 1, `${what}: one DataPart`);
    const data = message.parts[0].data;
    check(validate, data, `${what} as sent`);
    // Key order is not part of JSON; compare as documents.
    assert.deepEqual(data, { [COMMAND_DATA_KEY]: envelope }, `${what}: the client sent another envelope`);
    sentOps.add(envelope.op);
  }
  assert.deepEqual([...sentOps].sort(), Object.values(OPS).sort(), 'every op was sent at least once');
});

// ---- what the client reads -----------------------------------------------------

test('the feed frames, through the client’s frame reader, land in the Mirror as documented', async (t) => {
  const frames = new Map(examplesOf(EVENTS_EXTENSION));
  const order = ['hello.json', 'event-task.json', 'event-step.json', 'event-config.json', 'event-auth.json', 'goodbye.json'];
  assert.deepEqual([...frames.keys()].sort(), [...order].sort(), 'a new golden frame needs its effect asserted here');
  stubFetch(t, () => ({ stream: order.map((f) => frames.get(f)) }), [EVENTS_EXTENSION, TASK_ANNOTATIONS_EXTENSION]);
  const c = fullClient();
  const mirror = new Mirror();
  const goodbye = await c.subscribeEvents(
    0,
    (h) => mirror.onHello(h),
    (e) => mirror.apply(e),
  );
  const s = mirror.getState();

  // hello: the build and whether the introspection reads are served.
  assert.deepEqual(s.hello, frames.get('hello.json').hello);
  // goodbye: where to resume, and why the stream ended.
  assert.deepEqual(goodbye, frames.get('goodbye.json').goodbye);
  assert.equal(s.lastSeq, frames.get('event-auth.json').event.seq, 'the highest seq applied');

  // task: the Task, its core history on the transcript, and the annotations
  // read from under their URI.
  const wire = frames.get('event-task.json').event.data.task;
  const ann = wire.metadata[TASK_ANNOTATIONS_EXTENSION];
  const task = s.tasks.get(wire.id);
  assert.ok(task, 'the task is in the mirror');
  assert.equal(task.contextId, wire.contextId);
  assert.equal(task.state, wire.status.state);
  assert.equal(task.principal, ann.principal);
  assert.deepEqual(task.link, ann.link);
  assert.equal(task.created, Date.parse(ann.created));
  assert.deepEqual(
    task.statusHistory,
    ann.statusHistory.map((h) => ({ state: h.state, ts: Date.parse(h.ts) })),
  );
  assert.ok(
    s.transcript.some((e) => e.kind === 'user' && e.taskId === wire.id && e.text === wire.history[0].parts[0].text),
    "the history's user turn is on the transcript",
  );

  // step: one row for the step, done, with what the event reported.
  const step = frames.get('event-step.json').event.data;
  const rows = s.steps.get(step.run);
  assert.equal(rows?.length, 1);
  assert.equal(rows[0].step, step.step);
  assert.equal(rows[0].phase, 'done');
  assert.equal(rows[0].status, step.status);
  assert.equal(rows[0].tokens, step.tokens);

  // config and auth: notes an operator reads, keyed by the event.
  const note = (seq) => s.transcript.find((e) => e.key.endsWith(`-${seq}`) && e.kind === 'info');
  const config = frames.get('event-config.json').event;
  assert.match(note(config.seq)?.text ?? '', new RegExp(`${config.data.paths[0].replace(/\./g, '\\.')}.*${config.data.source.replace(/\./g, '\\.')}`));
  const auth = frames.get('event-auth.json').event;
  assert.match(note(auth.seq)?.text ?? '', new RegExp(`${auth.data.user_code}.*/approve ${auth.data.user_code}`));
});

test("a command's reply is read from where the extension puts it", () => {
  const annotations = validator(TASK_ANNOTATIONS_EXTENSION);

  // A read answers with a Message: its first DataPart is the answer.
  const read = commandReply({ message: { messageId: 'm1', role: 'ROLE_AGENT', parts: [{ text: 'status' }, { data: { runs: [] } }] } });
  assert.equal(read.kind, 'message');
  assert.deepEqual(read.data, { runs: [] });

  // Work answers with a Task: the result is its `<id>.result` artifact and
  // nothing else, and agentd's facts come from the annotations.
  const [, set] = examplesOf(COMMAND_EXTENSION).find(([f]) => f === 'admin-set.json');
  const result = { path: set[COMMAND_DATA_KEY].path, value: set[COMMAND_DATA_KEY].value };
  check(validator(COMMAND_EXTENSION, `/$defs/ops/${OPS.adminSet}/result`), result, 'the admin.set result');
  for (const [f, ann] of examplesOf(TASK_ANNOTATIONS_EXTENSION)) {
    check(annotations, ann, f);
    const wire = {
      id: 'task-1',
      contextId: 'c1',
      status: { state: 'TASK_STATE_COMPLETED', timestamp: '2026-09-29T10:15:04.870Z' },
      artifacts: [
        { artifactId: 'model-notes', parts: [{ data: { not: 'the answer' } }] },
        { artifactId: 'task-1.result', parts: [{ data: result }] },
      ],
      metadata: { [TASK_ANNOTATIONS_EXTENSION]: ann },
    };
    const r = commandReply({ task: wire });
    assert.equal(r.kind, 'task', f);
    assert.deepEqual(r.data, result, `${f}: the result artifact, not another one`);
    assert.equal(r.task.principal, ann.principal, f);
    assert.deepEqual(r.task.link, ann.link, f);
    assert.equal(r.task.command, ann.command, f);
    assert.deepEqual(r.task.askSchema, ann.askSchema, f);
    // An answer that did not activate the extension: what sits under the
    // key was not written under its contract, and is not read.
    const bare = commandReply({ task: wire }, { annotations: false });
    assert.equal(bare.task.principal, undefined, `${f}: annotations read without the extension`);
  }
  assert.throws(() => commandReply({ nothing: true }), /neither a Task nor a Message/);
});
