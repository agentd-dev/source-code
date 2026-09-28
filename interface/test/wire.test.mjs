// SPDX-License-Identifier: AGPL-3.0-only
// Unit tests for the transport (wire.ts) and failure reading (errors.ts):
// the WHATWG SSE parser, the headers and tenant every request carries, the
// error envelope with everything it holds, the extension echo, stream errors,
// and the one classify() table the loops and UIs act on. A stub fetch stands
// in for the network, so each test sees exactly the bytes a server would send.
import test from 'node:test';
import assert from 'node:assert/strict';
import {
  A2A_VERSION,
  call,
  parseExtensionHeader,
  rpc,
  rpcStream,
  sseParser,
  withTenant,
} from '../dist/client/wire.js';
import {
  badRequest,
  classify,
  describe,
  errorInfo,
  parseChallenge,
  parseRetryAfter,
} from '../dist/client/errors.js';
import { ClientError, RpcError } from '../dist/client/types.js';
import {
  AuthError,
  DeviceDenied,
  DeviceExpired,
  InsecureEndpoint,
  IssuerMismatch,
  LaunchExpired,
  LaunchRefused,
  NoLauncher,
} from '../dist/client/auth.js';
import { AgentdClient } from '../dist/client/index.js';
import { capabilitiesOf } from '../dist/client/discovery.js';
import { COMMAND_EXTENSION, EVENTS_EXTENSION, EVENTS_METHOD } from '../dist/client/ext.js';

const EP = { url: 'http://agent.test/' };
// What a card declaring the feed, streaming and the `status` op grants: the
// AgentdClient refuses anything its card does not back, before the wire.
const CAPS = capabilitiesOf(
  {
    capabilities: {
      streaming: true,
      extensions: [
        { uri: COMMAND_EXTENSION, params: { ops: [{ op: 'status', reply: 'message' }] } },
        { uri: EVENTS_EXTENSION },
      ],
    },
  },
  null,
);
const X = 'https://example.test/ext/x/v1';
const Y = 'https://example.test/ext/y/v1';

// ---- helpers ---------------------------------------------------------------

/** A body that yields `chunks` one read at a time and records a cancel. */
function streamOf(chunks, state = {}) {
  const enc = new TextEncoder();
  let i = 0;
  return new ReadableStream({
    pull(c) {
      if (i < chunks.length) c.enqueue(typeof chunks[i] === 'string' ? enc.encode(chunks[i++]) : chunks[i++]);
      else c.close();
    },
    cancel() {
      state.cancelled = true;
    },
  });
}

/**
 * Replace fetch for one test. `reply(req)` gets {url, init, body, headers}
 * and returns a Response; every request is recorded.
 */
function stubFetch(t, reply) {
  const calls = [];
  const orig = globalThis.fetch;
  globalThis.fetch = async (url, init) => {
    const req = { url: String(url), init, body: JSON.parse(init.body), headers: init.headers };
    calls.push(req);
    return reply(req);
  };
  t.after(() => {
    globalThis.fetch = orig;
  });
  return calls;
}

function json(v, status = 200, headers = {}) {
  return new Response(typeof v === 'string' ? v : JSON.stringify(v), {
    status,
    headers: { 'content-type': 'application/json', ...headers },
  });
}

function ok(req, result, headers) {
  return json({ jsonrpc: '2.0', id: req.body.id, result }, 200, headers);
}

function sse(chunks, headers = {}, state) {
  return new Response(streamOf(chunks, state), {
    status: 200,
    headers: { 'content-type': 'text/event-stream', ...headers },
  });
}

function frame(result, id = 1) {
  return `data: ${JSON.stringify({ jsonrpc: '2.0', id, result })}\n\n`;
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

function run(chunks) {
  const got = [];
  const feed = sseParser((ev) => got.push(ev));
  for (const c of chunks) feed(c);
  return got;
}
const datas = (chunks) => run(chunks).map((e) => e.data);

// ---- the SSE parser --------------------------------------------------------

test('sse parser follows the WHATWG rules', () => {
  // CRLF framing, as sse-starlette (the official Python SDK) sends it.
  assert.deepEqual(datas(['data: x\r\n\r\n']), ['x']);
  // CR-only, fed as the FINAL chunk: dispatched without waiting for more input.
  assert.deepEqual(datas(['data: x\r\r']), ['x']);
  // A CRLF split across chunks counts once, at a line end and at the blank line.
  assert.deepEqual(datas(['data: a\r', '\n\r', '\n']), ['a']);
  // … and mid-event: the '\n' opening the second chunk completes the CRLF
  // rather than ending the event early.
  assert.deepEqual(datas(['data: a\r', '\ndata: b\r\n\r\n']), ['a\nb']);
  // Comments are keep-alives.
  assert.deepEqual(datas([': ping\r\n\r\n']), []);
  // `event:` is recorded — the a2a-python error frame shape.
  const ev = run(['event: error\r\ndata: {"e":1}\r\n\r\n']);
  assert.equal(ev.length, 1);
  assert.equal(ev[0].event, 'error');
  assert.equal(ev[0].data, '{"e":1}');
  // Exactly one leading space is removed.
  assert.deepEqual(datas(['data:  two\n\n']), [' two']);
  assert.deepEqual(datas(['data:tight\n\n']), ['tight']);
  // A bare `data` is a field with an empty value: alone it is not dispatched…
  assert.deepEqual(datas(['data\n\n']), []);
  // …but it still contributes its line.
  assert.deepEqual(datas(['data\ndata: y\n\n']), ['\ny']);
  // A leading BOM is ignored.
  assert.deepEqual(datas(['﻿data: b\n\n']), ['b']);
  // `id:` is read, and persists as the last event ID.
  const ids = run(['id: 7\ndata: z\n\n', 'data: w\n\n']);
  assert.deepEqual(ids[0], { data: 'z', id: '7', event: undefined });
  assert.equal(ids[1].id, '7');
  // An id containing NUL is ignored.
  assert.equal(run(['id: a\0b\ndata: q\n\n'])[0].id, undefined);
  // LF framing with chunk boundaries anywhere.
  assert.deepEqual(datas(['data: {"a"', ':1}\n', '\n', ': keep-alive\n\n', 'data: one\n\ndata: two\n\n']), [
    '{"a":1}',
    'one',
    'two',
  ]);
  // An unterminated tail is never emitted.
  assert.deepEqual(datas(['data: done\n\n', 'data: tail']), ['done']);
  assert.deepEqual(datas(['data: tail\n']), []);
});

test('sse parser caps one event at 8 MiB', () => {
  const big = 'x'.repeat(9 << 20);
  // No line end at all.
  assert.throws(() => run([`data: ${big}`]), (e) => e instanceof RpcError && /exceeds cap/.test(e.message));
  // Many lines, no blank line.
  const line = `data: ${'y'.repeat(1 << 20)}\n`;
  assert.throws(() => run(Array(9).fill(line)), (e) => e instanceof RpcError);
  // Just under the cap is fine.
  assert.equal(datas([`data: ${'z'.repeat((8 << 20) - 64)}\n\n`])[0].length, (8 << 20) - 64);
});

// ---- headers, tenant, content-type ------------------------------------------

test('every request carries a2a-version, tenant and content-type', async (t) => {
  const calls = stubFetch(t, (req) => {
    if (req.body.method === EVENTS_METHOD) {
      return sse([frame({ hello: { seq: 0 } }), frame({ goodbye: { seq: 3 } })]);
    }
    if (req.body.method === 'SubscribeToTask') return sse([frame({ task: { id: 't' } })]);
    if (req.body.method === 'ListTasks') return ok(req, { tasks: [] });
    return ok(req, { task: { id: 't', status: { state: 'TASK_STATE_WORKING' } } });
  });
  assert.equal(A2A_VERSION, '1.0');

  for (const ep of [{ ...EP, tenant: 't1' }, EP]) {
    calls.length = 0;
    const c = new AgentdClient(ep, CAPS);
    await c.send('hi');
    await c.command('status', {});
    await c.getTask('t');
    await c.listTasks();
    await c.cancelTask('t');
    await c.subscribeTask('t', () => {});
    await c.subscribeEvents(0, () => {}, () => {});
    await rpc(ep, 'GetExtendedAgentCard', {});
    assert.equal(calls.length, 8);
    for (const r of calls) {
      assert.equal(r.headers['a2a-version'], A2A_VERSION, r.body.method);
      assert.equal(r.headers['content-type'], 'application/json', r.body.method);
      assert.equal(r.init.method, 'POST');
      assert.equal(r.init.redirect, 'error');
      assert.equal(r.init.credentials, 'omit');
      assert.equal(r.init.cache, 'no-store');
      if (ep.tenant) assert.equal(r.body.params.tenant, 't1', r.body.method);
      else assert.ok(!('tenant' in r.body.params), r.body.method);
      const streaming = r.body.method === 'SubscribeToTask' || r.body.method === EVENTS_METHOD;
      assert.equal(r.headers.accept, streaming ? 'text/event-stream' : 'application/json', r.body.method);
    }
  }

  // Extensions and a resume id travel as headers only when asked for.
  calls.length = 0;
  await rpcStream(EP, 'SubscribeToTask', { id: 't' }, () => {}, { exts: [X, Y], lastEventId: '42' });
  await rpc(EP, 'GetTask', { id: 't' });
  assert.equal(calls[0].headers['a2a-extensions'], `${X}, ${Y}`);
  assert.equal(calls[0].headers['last-event-id'], '42');
  assert.ok(!('a2a-extensions' in calls[1].headers));
  assert.ok(!('last-event-id' in calls[1].headers));

  // withTenant overwrites a stray tenant with the declared one and leaves
  // everything else alone.
  assert.deepEqual(withTenant({ id: 'a', tenant: 'x' }, 't1'), { id: 'a', tenant: 't1' });
  assert.deepEqual(withTenant({ id: 'a' }), { id: 'a' });
});

test('a credential never travels in the clear', async (t) => {
  const calls = stubFetch(t, (req) => ok(req, { tasks: [] }));
  // One missing `s` must not hand the token to the path: refused before fetch.
  for (const url of ['http://agent.example.com/', 'http://10.0.0.5:8420/', 'http://[2001:db8::1]:8420/']) {
    for (const call of [
      () => rpc({ url, bearer: 'agentd_at_secret' }, 'ListTasks', {}),
      () => rpcStream({ url, bearer: 'agentd_at_secret' }, 'SubscribeToTask', { id: 't' }, () => {}),
    ]) {
      await rejects(call(), (e) => {
        assert.ok(e instanceof ClientError && e.kind === 'insecure-endpoint', String(e));
        assert.equal(classify(e).kind, 'incompatible');
      });
    }
  }
  assert.equal(calls.length, 0, 'nothing may be sent');
  // https anywhere, http on loopback, and http with no credential all go out.
  for (const ep of [
    { url: 'https://agent.example.com/', bearer: 'b' },
    { url: 'http://127.0.0.1:8420/', bearer: 'b' },
    { url: 'http://localhost:8420/', bearer: 'b' },
    { url: 'http://[::1]:8420/', bearer: 'b' },
    { url: 'http://agent.example.com/' },
  ]) {
    await rpc(ep, 'ListTasks', {});
  }
  assert.equal(calls.length, 5);
  assert.equal(calls[1].headers.authorization, 'Bearer b');
});

// ---- errors ------------------------------------------------------------------

test('errors keep status, data, challenge and Retry-After', async (t) => {
  const info = { '@type': 'type.googleapis.com/google.rpc.ErrorInfo', reason: 'UNAUTHENTICATED', domain: 'agentd.dev' };
  let next;
  stubFetch(t, (req) => next(req));

  // 401 with a JSON-RPC body and a challenge.
  next = () =>
    json({ jsonrpc: '2.0', id: null, error: { code: -31401, message: 'm', data: [info] } }, 401, {
      'www-authenticate': 'Bearer realm="agentd", error="invalid_token"',
    });
  await rejects(rpc(EP, 'GetTask', { id: 't' }), (e) => {
    assert.ok(e instanceof RpcError);
    assert.equal(e.code, -31401);
    assert.equal(e.status, 401);
    assert.equal(e.challenge.error, 'invalid_token');
    assert.equal(e.challenge.realm, 'agentd');
    assert.deepEqual(e.data, [info]);
    assert.equal(errorInfo(e).reason, 'UNAUTHENTICATED');
    assert.equal(errorInfo(e).domain, 'agentd.dev');
  });

  // A non-JSON-RPC body keeps -status and the text.
  next = () => new Response('origin not allowed', { status: 403 });
  await rejects(rpc(EP, 'GetTask', { id: 't' }), (e) => {
    assert.equal(e.code, -403);
    assert.equal(e.status, 403);
    assert.match(e.message, /origin not allowed/);
  });

  // 429 with Retry-After.
  next = () => new Response('', { status: 429, headers: { 'retry-after': '7' } });
  await rejects(rpc(EP, 'GetTask', { id: 't' }), (e) => {
    assert.equal(e.retryAfterMs, 7000);
    assert.equal(e.message, 'HTTP 429 from http://agent.test/');
  });

  // A 2xx that is not JSON, not JSON-RPC 2.0, or answers another id.
  for (const bad of [
    () => json('<html>'),
    (req) => json({ id: req.body.id, result: {} }),
    (req) => json({ jsonrpc: '2.0', id: req.body.id + 1000, result: {} }),
    (req) => json({ jsonrpc: '2.0', id: req.body.id }),
  ]) {
    next = bad;
    await rejects(rpc(EP, 'GetTask', { id: 't' }), (e) => {
      assert.ok(e instanceof ClientError, String(e));
      assert.equal(e.kind, 'invalid-response');
    });
  }

  // A JSON-RPC error at HTTP 200 keeps its data verbatim, and an id of null
  // is accepted (a refusal before the id was read cannot echo it).
  const data = [
    { '@type': 'type.googleapis.com/google.rpc.BadRequest', fieldViolations: [{ field: 'id', description: 'required' }] },
  ];
  next = () => json({ jsonrpc: '2.0', id: null, error: { code: -32602, message: 'bad', data } });
  await rejects(rpc(EP, 'GetTask', {}), (e) => {
    assert.equal(e.code, -32602);
    assert.equal(e.status, 200);
    assert.deepEqual(e.data, data);
    assert.deepEqual(badRequest(e), [{ field: 'id', description: 'required' }]);
  });

  // A success body over 16 MiB is refused rather than buffered, even when it
  // is a well-formed reply.
  next = (req) => {
    const head = `{"jsonrpc":"2.0","id":${req.body.id},"result":"`;
    return new Response(streamOf([head, ...Array(17).fill('x'.repeat(1 << 20)), '"}']), { status: 200 });
  };
  await rejects(rpc(EP, 'GetTask', { id: 't' }), (e) => {
    assert.equal(e.kind, 'invalid-response');
    assert.match(e.message, /exceeds 16 MiB/);
  });
});

test('challenge and Retry-After parsing', () => {
  assert.deepEqual(
    parseChallenge(
      'Bearer realm="agentd", error="invalid_token", error_description="the session \\"x\\" is gone", resource_metadata="https://a/.well-known/oauth-protected-resource"',
    ),
    {
      scheme: 'Bearer',
      realm: 'agentd',
      error: 'invalid_token',
      errorDescription: 'the session "x" is gone',
      resourceMetadata: 'https://a/.well-known/oauth-protected-resource',
    },
  );
  // The Bearer challenge is picked out of several, case-insensitively, and a
  // token68 challenge before it does not confuse the parse.
  assert.deepEqual(parseChallenge('Negotiate abc==, Basic realm="b", bearer ERROR=insufficient_scope'), {
    scheme: 'bearer',
    error: 'insufficient_scope',
  });
  assert.equal(parseChallenge(null), undefined);
  assert.equal(parseRetryAfter('7'), 7000);
  assert.equal(parseRetryAfter('Wed, 21 Oct 2015 07:28:10 GMT', Date.parse('Wed, 21 Oct 2015 07:28:00 GMT')), 10000);
  assert.equal(parseRetryAfter('Wed, 21 Oct 2015 07:28:00 GMT', Date.parse('Wed, 21 Oct 2015 07:29:00 GMT')), 0);
  assert.equal(parseRetryAfter('soon'), undefined);
  assert.equal(parseRetryAfter(null), undefined);
});

// ---- streams -----------------------------------------------------------------

test('stream errors throw', async (t) => {
  let next;
  stubFetch(t, (req) => next(req));
  const frames = [];
  const onFrame = (r, id) => frames.push({ r, id });

  // An error answered as plain JSON before the stream opened.
  next = (req) => json({ jsonrpc: '2.0', id: req.body.id, error: { code: -32004, message: 'terminal task' } });
  await rejects(rpcStream(EP, 'SubscribeToTask', { id: 't' }, onFrame), (e) => {
    assert.ok(e instanceof RpcError);
    assert.equal(e.code, -32004);
  });

  // A refusal with an HTTP status goes through the same error path.
  next = () =>
    json({ jsonrpc: '2.0', id: null, error: { code: -31403, message: 'no' } }, 403, {
      'www-authenticate': 'Bearer realm="agentd", error="insufficient_scope"',
    });
  await rejects(rpcStream(EP, 'SubscribeToTask', { id: 't' }, onFrame), (e) => {
    assert.equal(e.code, -31403);
    assert.equal(e.status, 403);
    assert.equal(e.challenge.error, 'insufficient_scope');
  });

  // An in-stream error frame after a good one: the good one is delivered,
  // then the call rejects and the body is cancelled.
  frames.length = 0;
  const state = {};
  next = () =>
    sse(
      [
        'id: 5\r\n' + frame({ statusUpdate: { taskId: 't' } }).replace(/\n/g, '\r\n'),
        `data: ${JSON.stringify({ jsonrpc: '2.0', id: 1, error: { code: -32001, message: 'gone', data: [] } })}\r\n\r\n`,
        frame({ never: true }),
      ],
      {},
      state,
    );
  await rejects(rpcStream(EP, 'SubscribeToTask', { id: 't' }, onFrame), (e) => {
    assert.ok(e instanceof RpcError);
    assert.equal(e.code, -32001);
    assert.deepEqual(e.data, []);
  });
  assert.deepEqual(frames, [{ r: { statusUpdate: { taskId: 't' } }, id: '5' }]);
  assert.equal(state.cancelled, true);

  // A frame that is not JSON is an untrustworthy reply, not a silent drop.
  next = () => sse(['data: {not json\n\n']);
  await rejects(rpcStream(EP, 'SubscribeToTask', { id: 't' }, onFrame), (e) => {
    assert.ok(e instanceof ClientError);
    assert.equal(e.kind, 'invalid-response');
  });

  // The client-level wrappers reject too, rather than resolving as if the
  // stream had simply ended.
  next = (req) => json({ jsonrpc: '2.0', id: req.body.id, error: { code: -32004, message: 'terminal' } });
  await rejects(new AgentdClient(EP, CAPS).subscribeTask('t', () => {}), (e) => assert.equal(e.code, -32004));
  next = () => sse([`data: ${JSON.stringify({ jsonrpc: '2.0', id: 1, error: { code: -32603, message: 'x' } })}\n\n`]);
  await rejects(
    new AgentdClient(EP, CAPS).subscribeEvents(0, () => {}, () => {}),
    (e) => assert.equal(e.code, -32603),
  );

  // A clean stream resolves; a unary result is delivered as one frame.
  frames.length = 0;
  next = () => sse([frame({ a: 1 }), frame({ b: 2 })]);
  await rpcStream(EP, 'SubscribeToTask', { id: 't' }, onFrame);
  next = (req) => ok(req, { task: { id: 't' } });
  await rpcStream(EP, 'SendStreamingMessage', {}, onFrame);
  assert.deepEqual(
    frames.map((f) => f.r),
    [{ a: 1 }, { b: 2 }, { task: { id: 't' } }],
  );
});

// ---- the extension echo --------------------------------------------------------

test('echo verification', async (t) => {
  assert.equal(parseExtensionHeader(null), null);
  assert.deepEqual(parseExtensionHeader(''), []);
  assert.deepEqual(parseExtensionHeader(` ${X} ,, ${Y}, ${X} `), [X, Y]);

  let echo;
  const state = {};
  stubFetch(t, (req) => {
    const h = echo === undefined ? {} : { 'a2a-extensions': echo };
    return req.body.method === 'SubscribeToTask'
      ? sse([frame({ statusUpdate: {} })], h, state)
      : ok(req, { fine: true }, h);
  });
  const o = { exts: [X, Y], require: [X] };

  // Absent: advisory — the card governs.
  echo = undefined;
  assert.deepEqual(await call(EP, 'SendMessage', {}, o), { result: { fine: true }, echo: null, status: 200 });

  // Present and complete (an optional URI may be missing).
  echo = X;
  assert.deepEqual((await call(EP, 'SendMessage', {}, o)).echo, [X]);

  // Present but missing a required URI: the result is refused.
  echo = Y;
  await rejects(call(EP, 'SendMessage', {}, o), (e) => {
    assert.ok(e instanceof ClientError);
    assert.equal(e.kind, 'extension-not-activated');
    assert.match(e.message, new RegExp(X.replace(/[./]/g, '\\$&')));
  });
  // An empty echo is an answer too: nothing was activated.
  echo = '';
  await rejects(call(EP, 'SendMessage', {}, o), (e) => assert.equal(e.kind, 'extension-not-activated'));
  // No requirement, no refusal.
  echo = Y;
  assert.deepEqual((await call(EP, 'SendMessage', {}, { exts: [X, Y] })).echo, [Y]);

  // On a stream the echo is checked before a single frame is delivered, and
  // the body is cancelled.
  const frames = [];
  await rejects(
    rpcStream(EP, 'SubscribeToTask', {}, (r) => frames.push(r), o),
    (e) => assert.equal(e.kind, 'extension-not-activated'),
  );
  assert.deepEqual(frames, []);
  assert.equal(state.cancelled, true);
  echo = `${Y}, ${X}`;
  assert.deepEqual((await rpcStream(EP, 'SubscribeToTask', {}, (r) => frames.push(r), o)).echo, [Y, X]);
  assert.equal(frames.length, 1);
});

// ---- classify ------------------------------------------------------------------

const info = (reason, metadata) => ({
  '@type': 'type.googleapis.com/google.rpc.ErrorInfo',
  reason,
  domain: 'agentd.dev',
  ...(metadata ? { metadata } : {}),
});

test('classify() sorts failures', () => {
  const cases = [
    // [name, error, ctx, kind, message pattern, retryAfterMs]
    ['http 401', new RpcError(-401, 'HTTP 401', { status: 401 }), {}, 'unauthenticated', /requires sign-in/],
    [
      '-31401 invalid_token',
      new RpcError(-31401, 'm', { status: 401, challenge: { scheme: 'Bearer', error: 'invalid_token' } }),
      {},
      'unauthenticated',
      /session expired or revoked/,
    ],
    ['-31401 without status', new RpcError(-31401, 'm'), {}, 'unauthenticated', /requires sign-in/],
    ['goodbye revoked', new ClientError('session-revoked', 'revoked'), {}, 'unauthenticated', /session expired or revoked/],
    ['token expiry', new ClientError('session-expired', 'expired'), {}, 'unauthenticated', /session expired or revoked/],
    ['http 403', new RpcError(-403, 'HTTP 403', { status: 403 }), {}, 'forbidden', /not permitted/],
    ['-31403', new RpcError(-31403, 'admin.set is not permitted for user:a'), {}, 'forbidden', /not permitted: admin\.set/],
    [
      '-32009',
      new RpcError(-32009, 'v', { data: [info('VERSION_NOT_SUPPORTED', { supportedVersions: '1.0' })] }),
      {},
      'incompatible',
      /supports 1\.0/,
    ],
    ['-32008', new RpcError(-32008, 'needs x'), {}, 'incompatible', /needs x/],
    ['cross-origin', new ClientError('cross-origin', 'elsewhere'), {}, 'incompatible', /elsewhere/],
    ['discovery', new ClientError('discovery', 'no agent card'), {}, 'incompatible', /no agent card/],
    ['no-interface', new ClientError('no-interface', 'none'), {}, 'incompatible', /none/],
    [
      '429',
      new RpcError(-32603, 'slow down', { status: 429, retryAfterMs: 3000, data: [info('RATE_LIMITED')] }),
      {},
      'rate-limited',
      /slow down/,
      3000,
    ],
    [
      'RATE_LIMITED at 200 with metadata',
      new RpcError(-32603, 'slow', { status: 200, data: [info('RATE_LIMITED', { retryAfterSeconds: '4' })] }),
      {},
      'rate-limited',
      /slow/,
      4000,
    ],
    ['-32603 DRAINING', new RpcError(-32603, 'draining', { data: [info('DRAINING')] }), {}, 'transient', /draining/],
    ['TypeError', new TypeError('fetch failed'), {}, 'transient', /network error/],
    ['502', new RpcError(-502, 'HTTP 502', { status: 502 }), {}, 'transient', /502/],
    ['-32700', new RpcError(-32700, 'parse'), {}, 'protocol', /parse/],
    ['-32600', new RpcError(-32600, 'envelope'), {}, 'protocol', /envelope/],
    ['-32602', new RpcError(-32602, 'params'), {}, 'protocol', /params/],
    ['-32005', new RpcError(-32005, 'parts'), {}, 'protocol', /parts/],
    ['415', new RpcError(-415, 'HTTP 415', { status: 415 }), {}, 'protocol', /415/],
    ['invalid-response', new ClientError('invalid-response', 'junk'), {}, 'protocol', /junk/],
    ['-32601 extension call', new RpcError(-32601, 'nope'), { extensionCall: true }, 'unavailable', /nope/],
    ['-32601 core call', new RpcError(-32601, 'nope'), {}, 'protocol', /nope/],
    ['-32004 extension call', new RpcError(-32004, 'off'), { extensionCall: true }, 'unavailable', /off/],
    [
      'extension-not-activated, extension call',
      new ClientError('extension-not-activated', 'x'),
      { extensionCall: true },
      'unavailable',
      /x/,
    ],
    ['-32001', new RpcError(-32001, 'not found'), {}, 'protocol', /not found/],
    // Sign-in failures land in the same kinds as the calls they gate.
    [
      'device authorization 429',
      new AuthError('temporarily_unavailable', 'device authorization: temporarily_unavailable', {
        status: 429,
        retryAfterMs: 12000,
      }),
      {},
      'rate-limited',
      /temporarily_unavailable/,
      12000,
    ],
    ['temporarily_unavailable without 429', new AuthError('temporarily_unavailable', 'busy'), {}, 'rate-limited', /busy/],
    ['insecure sign-in endpoint', new InsecureEndpoint('in the clear'), {}, 'incompatible', /in the clear/],
    ['insecure call endpoint', new ClientError('insecure-endpoint', 'in the clear'), {}, 'incompatible', /in the clear/],
    ['issuer mismatch', new IssuerMismatch('another issuer'), {}, 'incompatible', /another issuer/],
    ['device denied', new DeviceDenied(), {}, 'unauthenticated', /denied — sign in again/],
    ['device expired', new DeviceExpired(), {}, 'unauthenticated', /expired .* — sign in again/],
    ['launch refused', new LaunchRefused(), {}, 'unauthenticated', /sign in again/],
    ['launch expired', new LaunchExpired(), {}, 'unauthenticated', /sign in again/],
    ['no launcher', new NoLauncher(), {}, 'unavailable', /agentd ui/],
    ['token endpoint 503', new AuthError('http-503', 'sign-in: http-503', { status: 503 }), {}, 'transient', /503/],
    ['invalid-response', new AuthError('invalid-response', 'junk answer'), {}, 'protocol', /junk answer/],
  ];
  for (const [name, e, ctx, kind, msg, retry] of cases) {
    const f = classify(e, ctx);
    assert.equal(f.kind, kind, name);
    assert.match(f.message, msg, name);
    assert.equal(f.retryAfterMs, retry, name);
  }
  // In a browser a network TypeError is as likely a CORS refusal, so the
  // message names the page origin and the daemon key that admits it.
  const g = globalThis;
  g.window = {};
  g.location = { origin: 'http://ui.test:8080' };
  try {
    const f = classify(new TypeError('Failed to fetch'));
    assert.equal(f.kind, 'transient');
    assert.match(f.message, /CORS refusal — is http:\/\/ui\.test:8080 listed .*a2a\.cors\.origins/);
  } finally {
    delete g.window;
    delete g.location;
  }
  assert.doesNotMatch(classify(new TypeError('fetch failed')).message, /CORS/);

  // describe() is one line and says what the UI will do.
  assert.equal(describe({ kind: 'rate-limited', message: 'slow', retryAfterMs: 2500 }), 'rate limited: slow — retrying in 3s');
  assert.ok(!describe({ kind: 'protocol', message: 'a\nb' }).includes('\n'));
});
