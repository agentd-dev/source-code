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

// The command op vocabulary a public card publishes: every op an agentd
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
  // A URL carrying credentials is refused, without repeating them: fetch
  // would otherwise throw an error that quotes the whole URL.
  const userinfo = throwsKind(() => cardUrlOf('https://op:sekrit@agent.example/'), 'discovery', /carries credentials/);
  assert.doesNotMatch(userinfo.message, /sekrit|op:/);

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
  // An interface URL with credentials in it is passed over, and not echoed.
  const withCreds = throwsKind(
    () => selectInterface(card([{ url: 'https://op:sekrit@agent.example/rpc', protocolBinding: 'JSONRPC', protocolVersion: '1.0' }]), CARD, false),
    'no-interface',
    /carries credentials/,
  );
  assert.doesNotMatch(withCreds.message, /sekrit/);

  // Card text is someone else's: control characters in anything that reaches
  // an error are spelled out, never sent to the terminal.
  const hostile = throwsKind(
    () => selectInterface({ name: '\u001b]0;pwned\u0007', supportedInterfaces: [{ protocolBinding: '\u001b[31mX', protocolVersion: '1\u009b' }] }, CARD, false),
    'no-interface',
  );
  assert.doesNotMatch(hostile.message, /[\u0000-\u001f\u007f-\u009f]/);
  assert.match(hostile.message, /\\u001b\]0;pwned\\u0007 advertises/);
  assert.match(hostile.message, /\\u001b\[31mX@1\\u009b/);

  // Another origin: never with the person's credential…
  const elsewhere = card([{ url: 'https://rpc.example/a2a', protocolBinding: 'JSONRPC', protocolVersion: '1.0' }]);
  throwsKind(() => selectInterface(elsewhere, CARD, true), 'cross-origin', /connect to https:\/\/rpc\.example directly/);
  // …followed without one over https, or loopback http from a loopback card…
  assert.equal(selectInterface(elsewhere, CARD, false).crossOrigin, true);
  const LOCAL_CARD = 'http://127.0.0.1:8420/.well-known/agent-card.json';
  const loop = card([{ url: 'http://127.0.0.1:9000/', protocolBinding: 'JSONRPC', protocolVersion: '1.0' }]);
  assert.equal(selectInterface(loop, LOCAL_CARD, false).crossOrigin, true);
  // …never over plain http to another host…
  const plain = card([{ url: 'http://rpc.example/', protocolBinding: 'JSONRPC', protocolVersion: '1.0' }]);
  throwsKind(() => selectInterface(plain, CARD, false), 'cross-origin', /plain http/);
  // …and never closer in than the card came from: a public card cannot aim
  // this client at the person's own machine (where a daemon may take any
  // local process for its operator) or their network, and a card on their
  // network cannot aim it at their machine.
  const inward = (url, from) =>
    throwsKind(
      () => selectInterface(card([{ url, protocolBinding: 'JSONRPC', protocolVersion: '1.0' }]), from, false),
      'cross-origin',
      /closer to this machine than the card itself is/,
    );
  for (const url of ['http://127.0.0.1:8420/', 'http://localhost:8420/', 'https://[::1]:8420/', 'https://[::ffff:127.0.0.1]:8420/', 'https://agentd.localhost/', 'https://192.168.1.5/', 'https://10.0.0.2/', 'https://169.254.169.254/', 'https://[fd12:3456::1]/']) {
    inward(url, CARD);
  }
  const LAN_CARD = 'https://192.168.1.5/.well-known/agent-card.json';
  inward('http://127.0.0.1:8420/', LAN_CARD);
  assert.equal(selectInterface(card([{ url: 'https://10.0.0.2/', protocolBinding: 'JSONRPC', protocolVersion: '1.0' }]), LAN_CARD, false).crossOrigin, true);
  // A cross-origin entry this client may not use is passed over like any
  // other: a later entry on the card's own origin still wins.
  const thenHome = card([
    { url: 'https://rpc.example/a2a', protocolBinding: 'JSONRPC', protocolVersion: '1.0' },
    { url: 'http://127.0.0.1:9/', protocolBinding: 'JSONRPC', protocolVersion: '1.0' },
    { url: 'https://agent.example/rpc', protocolBinding: 'JSONRPC', protocolVersion: '1.0' },
  ]);
  assert.deepEqual(selectInterface(thenHome, CARD, true), { url: 'https://agent.example/rpc', protocolVersion: '1.0', crossOrigin: false });

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

  // A card URL that redirects is refused, not followed: the card would be
  // judged as the configured origin's own, and its "same-origin" interface
  // handed the person's credential.
  const h = await fake(t);
  h.card.supportedInterfaces = [{ url: f.url, protocolBinding: 'JSONRPC', protocolVersion: '1.0', tenant: 'attacker' }];
  f.cardRedirect = `${h.origin}/.well-known/agent-card.json`;
  await rejects(openSession(f.url, { credential: { token: 'tok' }, cache: new CardCache() }), (e) => {
    assert.ok(e instanceof ClientError, String(e));
    assert.equal(e.kind, 'discovery');
    assert.match(e.message, /redirects to http:\/\/127\.0\.0\.1:\d+\/\.well-known\/agent-card\.json/);
  });
  assert.equal(h.cardGets().length, 0, 'the redirect target was never asked');
  f.cardRedirect = undefined;

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
  // A 304 to a request that asked nothing conditional has no card to keep.
  f.cardBody = undefined;
  f.cardStatus = 304;
  await rejects(fetchCard(`${f.origin}/.well-known/agent-card.json`, { cache: new CardCache() }), (e) => {
    assert.equal(e.kind, 'discovery');
    assert.match(e.message, /not conditional/);
  });
  f.cardStatus = undefined;
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

  // …also when `no-cache` comes first, the common spelling of "never keep".
  f.cacheControl = 'no-cache, no-store, must-revalidate';
  await fetchCard(url, { cache });
  assert.equal(cache.get(url), undefined, 'no-store after no-cache must not be kept');
  await fetchCard(url, { cache });
  assert.equal(f.cardGets()[6].headers['if-none-match'], undefined);

  // No Cache-Control, or `no-cache` beside a max-age: kept for the ETag, and
  // revalidated at once rather than trusted for the max-age.
  for (const cc of ['', 'no-cache, max-age=60']) {
    f.cacheControl = cc;
    const fresh = new CardCache(() => now);
    const before = f.cardGets().length;
    await fetchCard(url, { cache: fresh });
    await fetchCard(url, { cache: fresh });
    const gets = f.cardGets().slice(before);
    assert.equal(gets.length, 2, `${JSON.stringify(cc)}: revalidated at once`);
    assert.match(gets[1].headers['if-none-match'] ?? '', /^"[0-9a-f]{32}"$/, JSON.stringify(cc));
    assert.equal(gets[1].status, 304, JSON.stringify(cc));
  }
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
  assert.equal(anon.caps.introspection, null, 'the static list cannot say whether introspection is on — not "off"');
  assert.equal(anon.caps.authRequired, true);
  assert.deepEqual(anon.caps.workflows, []);
  // An op is an `{op, reply}` entry; one without that shape names no op.
  const shapes = capabilitiesOf(
    { capabilities: { extensions: [{ uri: COMMAND_EXTENSION, params: { ops: ['status', { op: 'config', reply: 'message' }] } }] } },
    null,
  );
  assert.deepEqual([...shapes.command.ops], ['config']);

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
  assert.equal(plain.caps.introspection, null);

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
  // The extended card says introspection is off for this caller: false.
  const narrowed = f.extendedCard;
  f.extendedCard = {
    ...narrowed,
    capabilities: { ...narrowed.capabilities, extensions: [{ uri: COMMAND_EXTENSION, params: { ops: [{ op: 'status', reply: 'message' }] } }] },
  };
  const off = await openSession(f.url, { credential: { token: 'tok' }, cache: new CardCache() });
  assert.equal(off.caps.introspection, false);

  // Declared but not served: a warning, and the public card. A2A 1.0 names
  // that case -32007 (a stock server's answer, and the fake's when no
  // extended card is configured); agentd answers -32004.
  f.extendedCard = null;
  const unconfigured = await openSession(f.url, { credential: { token: 'tok' }, cache: new CardCache() });
  assert.equal(unconfigured.caps.extendedCard, false);
  assert.match(unconfigured.warnings.join('\n'), /does not serve it \(the extended agent card is not configured\)/);
  f.extendedCard = narrowed;
  f.fail('GetExtendedAgentCard', { code: -32004, message: 'not offered', times: 1 });
  const unserved = await openSession(f.url, { credential: { token: 'tok' }, cache: new CardCache() });
  assert.equal(unserved.caps.extendedCard, false);
  assert.match(unserved.warnings.join('\n'), /does not serve it/);
  // Anything else is not papered over.
  f.fail('GetExtendedAgentCard', { code: -32603, message: 'boom', times: 1 });
  await rejects(openSession(f.url, { credential: { token: 'tok' }, cache: new CardCache() }), (e) => {
    assert.ok(e instanceof RpcError);
    assert.equal(e.code, -32603);
  });
  // An extended card that is not an object: a warning, and the public card.
  f.extendedCard = ['not', 'a', 'card'];
  const notObj = await openSession(f.url, { credential: { token: 'tok' }, cache: new CardCache() });
  assert.equal(notObj.caps.extendedCard, false);
  assert.match(notObj.warnings.join('\n'), /not an object/);
  // An extension the EXTENDED card requires binds like one the public card
  // requires: the client stops rather than carry on without it.
  f.extendedCard = {
    ...narrowed,
    capabilities: { ...narrowed.capabilities, extensions: [...narrowed.capabilities.extensions, { uri: 'https://other.example/ext/z/v1', required: true }] },
  };
  await rejects(openSession(f.url, { credential: { token: 'tok' }, cache: new CardCache() }), (e) => {
    assert.equal(e.kind, 'required-extension');
    assert.match(e.message, /other\.example\/ext\/z\/v1/);
  });
  f.extendedCard = narrowed;

  // A URI that is not exactly one this client speaks is someone else's
  // extension, however close its spelling — even agentd's own with a version
  // appended: not used, and nothing to say about it.
  const near = `${COMMAND_EXTENSION}/v2`;
  const caps = capabilitiesOf(
    { capabilities: { extensions: [{ uri: near }, { uri: 'https://other.example/ext/x/v1' }] } },
    null,
  );
  assert.equal(caps.command, null);
  f.card.capabilities = { extensions: [{ uri: near }, { uri: 'https://other.example/ext/x/v1' }] };
  const other = await openSession(f.url, { cache: new CardCache() });
  assert.equal(other.caps.command, null);
  assert.deepEqual(other.warnings, []);
});

// ---- the vocabulary and the core client ----------------------------------------

test('a2a.ts messages', async (t) => {
  // The vocabulary: three URIs under one module, and no agentd-owned name
  // carries a version.
  assert.deepEqual([...CLIENT_EXTENSIONS], [COMMAND_EXTENSION, EVENTS_EXTENSION, TASK_ANNOTATIONS_EXTENSION]);
  for (const uri of [...CLIENT_EXTENSIONS, UNIX_BINDING]) assert.doesNotMatch(uri, /\/v\d+$/);
  assert.equal(SLASH_OPS.set, 'admin.set');
  assert.ok(INTROSPECTION_OPS.every((op) => !Object.values(SLASH_OPS).includes(op)));

  // A2A 1.0 has exactly eleven methods.
  assert.equal(CORE_METHODS.length, 11);

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
  // An argument named `op` never replaces the op the caller named.
  const shadow = commandMessage('ship', { op: 'admin.drain', x: 1 }, { messageId: 'm4' });
  assert.deepEqual(shadow.parts[0].data[COMMAND_DATA_KEY], { op: 'ship', x: 1 });

  const f = await fake(t);
  const c = new A2aClient({ url: f.url });

  // SendMessage: returnImmediately is always explicit.
  const sent = await c.sendMessage(plain, { returnImmediately: true });
  assert.ok(sent.task.id);
  const body = f.rpcCalls('SendMessage')[0].params;
  assert.deepEqual(body.configuration, { returnImmediately: true });
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
