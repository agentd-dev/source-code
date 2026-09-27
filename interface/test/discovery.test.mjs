// SPDX-License-Identifier: AGPL-3.0-only
// Discovery (discovery.ts) and the core A2A client (a2a.ts), against the
// in-process fake agent over real HTTP: the well-known card and its cache,
// interface selection and every refusal it makes, capabilities read from the
// public and the extended card, and the messages and paging every call rides.
import test from 'node:test';
import assert from 'node:assert/strict';
import {
  CardCache,
  capabilitiesOf,
  cardUrlOf,
  fetchCard,
  openSession,
  requiredUnsupported,
  selectInterface,
} from '../dist/client/discovery.js';
import { A2aClient, CORE_METHODS, commandMessage, userMessage } from '../dist/client/a2a.js';
import {
  CLIENT_EXTENSIONS,
  COMMAND_DATA_KEY,
  COMMAND_EXTENSION,
  EVENTS_EXTENSION,
  INTROSPECTION_OPS,
  SLASH_OPS,
  TASK_ANNOTATIONS_EXTENSION,
  UNIX_BINDING,
} from '../dist/client/ext.js';
import { ClientError, RpcError } from '../dist/client/types.js';
import { classify } from '../dist/client/errors.js';
import { startFakeA2a } from './fake-a2a.mjs';

// The command/v2 op vocabulary a public card publishes: every op an agentd
// can serve, the same for every caller (the daemon's `static_vocabulary()`).
const STATIC_OPS = [
  { op: 'status', reply: 'message' },
  { op: 'config', reply: 'message' },
  { op: 'workflow.status', reply: 'message' },
  { op: 'workflow.run', reply: 'task' },
  { op: 'admin.drain', reply: 'task' },
  { op: 'admin.set', reply: 'task' },
  { op: 'conversation.get', reply: 'message' },
  { op: 'debug.events', reply: 'message' },
  { op: 'auth.device.approve', reply: 'task' },
];

async function fake(t, opts) {
  const f = await startFakeA2a(opts);
  t.after(() => f.close());
  return f;
}

async function rejects(p, check) {
  let err;
  try {
    await p;
  } catch (e) {
    err = e;
  }
  assert.ok(err, 'expected a rejection');
  check(err);
  return err;
}

function throwsKind(fn, kind, pattern) {
  let err;
  try {
    fn();
  } catch (e) {
    err = e;
  }
  assert.ok(err instanceof ClientError, `expected ClientError(${kind}), got ${err}`);
  assert.equal(err.kind, kind);
  if (pattern) assert.match(err.message, pattern);
  return err;
}

const card = (interfaces, extra = {}) => ({ name: 'agent', supportedInterfaces: interfaces, ...extra });
const CARD = 'https://agent.example/.well-known/agent-card.json';

// ---- where the card is, and which interface --------------------------------

test('reads the well-known card and picks JSONRPC 1.0', async (t) => {
  // The card URL: the well-known path at the configured origin, or the value
  // itself when it already names the card.
  assert.equal(cardUrlOf('https://agent.example'), CARD);
  assert.equal(cardUrlOf('https://agent.example/some/base/'), CARD);
  assert.equal(cardUrlOf('http://127.0.0.1:8420/'), 'http://127.0.0.1:8420/.well-known/agent-card.json');
  assert.equal(cardUrlOf('https://agent.example/.well-known/agent-card.json'), CARD);
  throwsKind(() => cardUrlOf('not a url'), 'discovery');

  // The first JSONRPC entry at Major.Minor 1.0 wins; other bindings and
  // versions are passed over, and a patch version is still 1.0.
  const picked = selectInterface(
    card([
      { url: 'https://agent.example/grpc', protocolBinding: 'GRPC', protocolVersion: '1.0' },
      { url: 'https://agent.example/old', protocolBinding: 'JSONRPC', protocolVersion: '0.3' },
      { url: 'https://agent.example/rpc', protocolBinding: 'JSONRPC', protocolVersion: '1.0.1', tenant: 't1' },
      { url: 'https://agent.example/later', protocolBinding: 'JSONRPC', protocolVersion: '1.0' },
    ]),
    CARD,
    true,
  );
  assert.deepEqual(picked, { url: 'https://agent.example/rpc', protocolVersion: '1.0.1', crossOrigin: false, tenant: 't1' });

  // A unix-socket card is refused by name: no-interface, never a fallback.
  throwsKind(
    () => selectInterface(card([{ url: 'unix:///run/agentd.sock', protocolBinding: UNIX_BINDING, protocolVersion: '1.0' }]), CARD, false),
    'no-interface',
    /advertises no JSON-RPC A2A 1\.0 interface.*unix socket/,
  );
  // …as is a JSONRPC entry at a URL fetch cannot dial, a wildcard host, or
  // no absolute URL.
  for (const url of ['unix:///run/agentd.sock', 'unix://agent.example/sock', 'ws://agent.example/', 'http://0.0.0.0:8420/', 'http://[::]:8420/', '/relative']) {
    throwsKind(
      () => selectInterface(card([{ url, protocolBinding: 'JSONRPC', protocolVersion: '1.0' }]), CARD, false),
      'no-interface',
    );
  }
  // Only 0.3 on offer: refused, listing what was found.
  throwsKind(
    () => selectInterface(card([{ url: 'https://agent.example/', protocolBinding: 'JSONRPC', protocolVersion: '0.3' }]), CARD, false),
    'no-interface',
    /found: JSONRPC@0\.3/,
  );
  throwsKind(() => selectInterface(card(undefined), CARD, false), 'no-interface', /found: none/);

  // Another origin: never with the person's credential…
  const elsewhere = card([{ url: 'https://rpc.example/a2a', protocolBinding: 'JSONRPC', protocolVersion: '1.0' }]);
  throwsKind(() => selectInterface(elsewhere, CARD, true), 'cross-origin', /re-run with --endpoint https:\/\/rpc\.example/);
  // …followed without one over https or loopback http…
  assert.equal(selectInterface(elsewhere, CARD, false).crossOrigin, true);
  const loop = card([{ url: 'http://127.0.0.1:9000/', protocolBinding: 'JSONRPC', protocolVersion: '1.0' }]);
  assert.equal(selectInterface(loop, CARD, false).crossOrigin, true);
  // …and never over plain http to another host.
  const plain = card([{ url: 'http://rpc.example/', protocolBinding: 'JSONRPC', protocolVersion: '1.0' }]);
  throwsKind(() => selectInterface(plain, CARD, false), 'cross-origin', /plain http/);

  // A required extension this client does not speak stops it; a required one
  // it speaks does not — unless extensions are off.
  const reqCard = {
    capabilities: {
      extensions: [
        { uri: 'https://other.example/ext/x/v1', required: true },
        { uri: COMMAND_EXTENSION, required: true },
        { uri: 'https://other.example/ext/y/v1', required: false },
      ],
    },
  };
  assert.deepEqual(requiredUnsupported(reqCard), ['https://other.example/ext/x/v1']);
  assert.deepEqual(requiredUnsupported(reqCard, true), ['https://other.example/ext/x/v1', COMMAND_EXTENSION]);

  // End to end against a real server: the card GET carries no credential and
  // no A2A header; the session targets the card's interface with its tenant,
  // and the tenant rides every call.
  const f = await fake(t);
  f.card.supportedInterfaces = [{ url: `${f.origin}/`, protocolBinding: 'JSONRPC', protocolVersion: '1.0', tenant: 'acme' }];
  const s = await openSession(`${f.origin}/anything`, { credential: { token: 'tok' }, cache: new CardCache() });
  assert.equal(s.cardUrl, `${f.origin}/.well-known/agent-card.json`);
  assert.deepEqual(s.ep, { url: `${f.origin}/`, tenant: 'acme', bearer: 'tok' });
  const get = f.cardGets()[0];
  assert.equal(get.headers.authorization, undefined);
  assert.equal(get.headers['a2a-version'], undefined);
  assert.equal(get.headers['a2a-extensions'], undefined);
  await new A2aClient(s.ep).listTasks();
  assert.equal(f.rpcCalls('ListTasks')[0].params.tenant, 'acme');

  // The same fake, now naming only a unix socket: refused before any RPC.
  f.card.supportedInterfaces = [{ url: 'unix:///run/agentd.sock', protocolBinding: UNIX_BINDING, protocolVersion: '1.0' }];
  await rejects(openSession(f.url, { cache: new CardCache() }), (e) => {
    assert.ok(e instanceof ClientError);
    assert.equal(e.kind, 'no-interface');
    assert.equal(classify(e).kind, 'incompatible');
  });

  // A cross-origin interface with no credential: followed, with a warning,
  // and no bearer on the endpoint.
  const g = await fake(t);
  f.card.supportedInterfaces = [{ url: g.url, protocolBinding: 'JSONRPC', protocolVersion: '1.0' }];
  const x = await openSession(f.url, { cache: new CardCache() });
  assert.equal(x.ep.url, g.url);
  assert.equal(x.ep.bearer, undefined);
  assert.match(x.warnings.join('\n'), /another origin/);
  // With one, refused.
  await rejects(openSession(f.url, { credential: { token: 'tok' }, cache: new CardCache() }), (e) => {
    assert.equal(e.kind, 'cross-origin');
  });

  // A required foreign extension, end to end.
  f.card = { ...f.card, supportedInterfaces: [{ url: f.url, protocolBinding: 'JSONRPC', protocolVersion: '1.0' }] };
  f.card.capabilities = { ...f.card.capabilities, extensions: [{ uri: 'https://other.example/ext/x/v1', required: true }] };
  await rejects(openSession(f.url, { cache: new CardCache() }), (e) => {
    assert.equal(e.kind, 'required-extension');
    assert.match(e.message, /other\.example\/ext\/x\/v1/);
  });

  // No card, a card that is not JSON, and one too big to be a card.
  f.cardStatus = 404;
  await rejects(fetchCard(`${f.origin}/.well-known/agent-card.json`, { cache: new CardCache() }), (e) => {
    assert.equal(e.kind, 'discovery');
    assert.match(e.message, /HTTP 404/);
  });
  f.cardStatus = undefined;
  f.cardBody = '<html>';
  await rejects(fetchCard(`${f.origin}/.well-known/agent-card.json`, { cache: new CardCache() }), (e) => {
    assert.equal(e.kind, 'discovery');
  });
  f.cardBody = JSON.stringify({ name: 'x'.repeat((1 << 20) + 1) });
  await rejects(fetchCard(`${f.origin}/.well-known/agent-card.json`, { cache: new CardCache() }), (e) => {
    assert.equal(e.kind, 'discovery');
    assert.match(e.message, /exceeds 1 MiB/);
  });
});

// ---- the cache ---------------------------------------------------------------

test('honours Cache-Control and ETag', async (t) => {
  const f = await fake(t, { cacheControl: 'public, max-age=60' });
  let now = 1_000_000;
  const cache = new CardCache(() => now);
  const url = `${f.origin}/.well-known/agent-card.json`;

  const first = await fetchCard(url, { cache });
  assert.equal(f.cardGets().length, 1);
  assert.equal(f.cardGets()[0].headers['if-none-match'], undefined);

  // Inside max-age: the cache answers, no request.
  now += 59_000;
  assert.equal(await fetchCard(url, { cache }), first);
  assert.equal(f.cardGets().length, 1);

  // Past it: one conditional GET with the ETag; the 304 keeps the cached card.
  now += 2_000;
  const again = await fetchCard(url, { cache });
  assert.equal(f.cardGets().length, 2);
  const cond = f.cardGets()[1];
  assert.match(cond.headers['if-none-match'] ?? '', /^"[0-9a-f]{32}"$/);
  assert.equal(cond.status, 304);
  assert.equal(again, first, 'a 304 must keep the cached card');

  // The 304 renewed the lifetime.
  now += 30_000;
  await fetchCard(url, { cache });
  assert.equal(f.cardGets().length, 2);

  // A changed card is fetched in full after expiry.
  f.card = { ...f.card, description: 'changed' };
  now += 31_000;
  const changed = await fetchCard(url, { cache });
  assert.equal(f.cardGets()[2].status, 200);
  assert.equal(changed.description, 'changed');

  // no-store: never kept, so every discovery asks.
  f.cacheControl = 'no-store';
  now += 61_000;
  await fetchCard(url, { cache });
  await fetchCard(url, { cache });
  assert.equal(f.cardGets().length, 5);
  assert.equal(f.cardGets()[4].headers['if-none-match'], undefined);
});

// ---- capabilities ------------------------------------------------------------

test('capabilities come from the card', async (t) => {
  const f = await fake(t, { bearer: 'tok' });
  f.card.capabilities = {
    streaming: true,
    extendedAgentCard: true,
    extensions: [
      { uri: COMMAND_EXTENSION, params: { dataPartKey: COMMAND_DATA_KEY, ops: STATIC_OPS } },
      { uri: EVENTS_EXTENSION, params: { method: 'agentd.events/SubscribeToEvents' } },
      { uri: TASK_ANNOTATIONS_EXTENSION, params: {} },
    ],
  };
  f.card.securitySchemes = { bearer: { httpAuthSecurityScheme: { scheme: 'Bearer' } } };
  f.card.securityRequirements = [{ schemes: { bearer: { list: [] } } }];
  f.extendedCard = {
    ...f.card,
    skills: [
      { id: 'conversation', name: 'Conversation', tags: ['conversation'] },
      { id: 'workflow:deploy', name: 'deploy', tags: ['workflow'] },
    ],
    capabilities: {
      ...f.card.capabilities,
      extensions: [
        {
          uri: COMMAND_EXTENSION,
          params: {
            ops: [{ op: 'status', reply: 'message' }, { op: 'debug.events', reply: 'message' }],
            commands: [{ op: 'ship', workflow: 'deploy' }],
            settable: ['agent.approval'],
          },
        },
        { uri: EVENTS_EXTENSION, params: { ring: 4096, kinds: ['task', 'run'] } },
      ],
    },
  };
  const statics = new Set(STATIC_OPS.map((o) => o.op));

  // No credential: the extended card is never asked for (it could only 401),
  // and the ops are the public static vocabulary.
  const anon = await openSession(f.url, { cache: new CardCache() });
  assert.equal(f.rpcCalls('GetExtendedAgentCard').length, 0);
  assert.equal(anon.caps.extendedCard, false);
  assert.deepEqual(anon.caps.command.ops, statics);
  assert.equal(anon.caps.introspection, false, 'the static list cannot say introspection is on');
  assert.equal(anon.caps.authRequired, true);
  assert.deepEqual(anon.caps.workflows, []);

  // A credential, and a card that offers the extended card: read, with the
  // bearer and A2A-Version, and it narrows what this caller may do.
  const signed = await openSession(f.url, { credential: { token: 'tok' }, cache: new CardCache() });
  const ext = f.rpcCalls('GetExtendedAgentCard');
  assert.equal(ext.length, 1);
  assert.equal(ext[0].headers.authorization, 'Bearer tok');
  assert.equal(ext[0].headers['a2a-version'], '1.0');
  assert.equal(signed.caps.extendedCard, true);
  assert.deepEqual(signed.caps.command.ops, new Set(['status', 'debug.events']));
  assert.deepEqual(signed.caps.command.commands, [{ op: 'ship', workflow: 'deploy' }]);
  assert.deepEqual(signed.caps.command.settable, ['agent.approval']);
  assert.deepEqual(signed.caps.events, { ring: 4096, kinds: ['task', 'run'] });
  assert.equal(signed.caps.annotations, false, 'the extended card governs: it does not declare annotations');
  assert.equal(signed.caps.introspection, true);
  assert.deepEqual(signed.caps.workflows, ['deploy']);

  // extendedAgentCard false: a credential changes nothing — the ops stay the
  // public static list and no GetExtendedAgentCard is sent.
  f.card.capabilities = { ...f.card.capabilities, extendedAgentCard: false };
  const plain = await openSession(f.url, { credential: { token: 'tok' }, cache: new CardCache() });
  assert.equal(f.rpcCalls('GetExtendedAgentCard').length, 1);
  assert.equal(plain.caps.extendedCard, false);
  assert.deepEqual(plain.caps.command.ops, statics);
  assert.equal(plain.caps.annotations, true);
  assert.deepEqual(plain.caps.events, { kinds: [] });
  assert.equal(plain.caps.streaming, true);

  // --no-extensions: core A2A only.
  const core = await openSession(f.url, { credential: { token: 'tok' }, noExtensions: true, cache: new CardCache() });
  assert.equal(core.caps.command, null);
  assert.equal(core.caps.events, null);
  assert.equal(core.caps.annotations, false);
  assert.equal(core.caps.introspection, false);

  // The extended card refused with 401: the credential is wrong — thrown, and
  // classified as a sign-in problem, not papered over with the public card.
  f.card.capabilities = { ...f.card.capabilities, extendedAgentCard: true };
  await rejects(openSession(f.url, { credential: { token: 'wrong' }, cache: new CardCache() }), (e) => {
    assert.ok(e instanceof RpcError);
    assert.equal(classify(e).kind, 'unauthenticated');
  });
  // Declared but not served (-32004): a warning, and the public card.
  f.fail('GetExtendedAgentCard', { code: -32004, message: 'not offered', times: 1 });
  const unserved = await openSession(f.url, { credential: { token: 'tok' }, cache: new CardCache() });
  assert.equal(unserved.caps.extendedCard, false);
  assert.match(unserved.warnings.join('\n'), /does not serve it/);

  // An agentd extension at a version this client does not speak is ignored,
  // with a note naming both; a foreign one is ignored silently.
  const v1 = COMMAND_EXTENSION.replace(/v2$/, 'v1');
  const caps = capabilitiesOf(
    { capabilities: { extensions: [{ uri: v1 }, { uri: 'https://other.example/ext/x/v1' }] } },
    null,
  );
  assert.equal(caps.command, null);
  assert.deepEqual(caps.ignored, [v1, 'https://other.example/ext/x/v1']);
  f.card.capabilities = { extensions: [{ uri: v1 }, { uri: 'https://other.example/ext/x/v1' }] };
  const old = await openSession(f.url, { cache: new CardCache() });
  assert.deepEqual(old.warnings, [`agent offers ${v1}; this client speaks ${COMMAND_EXTENSION}`]);
});

// ---- the vocabulary and the core client ----------------------------------------

test('a2a.ts messages', async (t) => {
  // The vocabulary: three URIs, all versioned, all under one module.
  assert.deepEqual([...CLIENT_EXTENSIONS], [COMMAND_EXTENSION, EVENTS_EXTENSION, TASK_ANNOTATIONS_EXTENSION]);
  for (const uri of CLIENT_EXTENSIONS) assert.match(uri, /\/v\d+$/);
  assert.equal(SLASH_OPS.set, 'admin.set');
  assert.ok(INTROSPECTION_OPS.every((op) => !Object.values(SLASH_OPS).includes(op)));

  // A2A 1.0 has exactly eleven methods.
  assert.equal(CORE_METHODS.length, 11);
  assert.ok(!CORE_METHODS.includes('GetAgentCard'));

  // A natural-language message: ROLE_USER, and a taskId is never paired with
  // a contextId.
  const plain = userMessage('hi', { messageId: 'm1', contextId: 'c1' });
  assert.deepEqual(plain, { role: 'ROLE_USER', messageId: 'm1', parts: [{ text: 'hi' }], contextId: 'c1' });
  const answer = userMessage('yes', { messageId: 'm2', contextId: 'c1', taskId: 't1' });
  assert.deepEqual(answer, { role: 'ROLE_USER', messageId: 'm2', parts: [{ text: 'yes' }], taskId: 't1' });
  assert.match(userMessage('x').messageId, /^ui-[0-9a-z]+-\d+$/);

  // A command: one application/json DataPart under the envelope key, marked
  // with the command extension, and no taskId.
  const cmd = commandMessage('workflow.run', { workflow: 'deploy' }, { messageId: 'm3', contextId: 'c1' });
  assert.deepEqual(cmd, {
    role: 'ROLE_USER',
    messageId: 'm3',
    parts: [{ data: { [COMMAND_DATA_KEY]: { op: 'workflow.run', workflow: 'deploy' } }, mediaType: 'application/json' }],
    extensions: [COMMAND_EXTENSION],
    contextId: 'c1',
  });
  assert.equal('taskId' in cmd, false);

  const f = await fake(t);
  const c = new A2aClient({ url: f.url });

  // SendMessage: returnImmediately is always explicit; `blocking` is gone.
  const sent = await c.sendMessage(plain, { returnImmediately: true });
  assert.ok(sent.task.id);
  const body = f.rpcCalls('SendMessage')[0].params;
  assert.deepEqual(body.configuration, { returnImmediately: true });
  assert.equal(JSON.stringify(body).includes('blocking'), false);
  // The core client activates no extension on its own.
  assert.equal(f.rpcCalls('SendMessage')[0].headers['a2a-extensions'], undefined);

  // A reply that is neither a task nor a message is not trusted.
  f.handle('SendMessage', () => ({ something: 'else' }));
  await rejects(c.sendMessage(plain, { returnImmediately: false }), (e) => assert.equal(e.kind, 'invalid-response'));

  // Streaming over CRLF-framed SSE, with the SSE id for a resume.
  const frames = [];
  await c.sendStreamingMessage(plain, { returnImmediately: true }, (fr, id) => frames.push({ fr, id }));
  assert.equal(frames.length, 1);
  assert.ok(frames[0].fr.task);
  assert.equal(frames[0].id, '1');

  // ListTasks: pages of 100 with artifacts, following nextPageToken…
  f.tasks.clear();
  for (let i = 0; i < 250; i++) f.putTask({ id: `t${i}`, contextId: 'c', status: { state: 'TASK_STATE_WORKING' } });
  const before = f.rpcCalls('ListTasks').length;
  const all = await c.listTasks({ contextId: 'c' });
  const calls = f.rpcCalls('ListTasks').slice(before);
  assert.equal(all.tasks.length, 250);
  assert.equal(all.truncated, false);
  assert.equal(calls.length, 3);
  for (const call of calls) {
    assert.equal(call.params.pageSize, 100);
    assert.equal(call.params.includeArtifacts, true);
    assert.equal(call.params.contextId, 'c');
  }
  assert.equal(calls[0].params.pageToken, undefined);
  assert.equal(calls[1].params.pageToken, '100');
  // …and stopping at maxPages, saying so.
  const capped = await c.listTasks({}, undefined, 2);
  assert.equal(capped.tasks.length, 200);
  assert.equal(capped.truncated, true);

  // Every method this client sent is an A2A 1.0 method.
  for (const r of f.rpcCalls()) assert.ok(CORE_METHODS.includes(r.rpc), `${r.rpc} is not a core method`);
});
