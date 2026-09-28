// SPDX-License-Identifier: AGPL-3.0-only
// An in-process A2A 1.0 agent over real HTTP, for the client tests.
//
// It answers the way a stock A2A server does, not the way agentd happens to:
// the card at the well-known path with an ETag, `Cache-Control` and 304s;
// JSON-RPC on POST at the interface path; SSE framed with CRLF, as the
// official Python SDK (sse-starlette) frames it; and stubs of the OAuth
// device grant and of agentd's launch grant. Every request is recorded, headers included, so a test can
// assert what the client sent — and what it did not.
//
//   const fake = await startFakeA2a();
//   … fake.url, fake.card, fake.fail('GetTask', {code: -32001}), fake.requests …
//   await fake.close();
//
// A handler may answer `{stream: [frames], hold}`: the frames are written at
// once and the stream stays open until the `hold` promise settles (or the
// client goes away), so a test can see how many streams a client holds.
import http from 'node:http';
import { createHash } from 'node:crypto';

const CARD_PATH = '/.well-known/agent-card.json';
/** The daemon's `surface::launch::LAUNCH_GRANT_TYPE`. */
const LAUNCH_GRANT_TYPE = 'https://agentd.dev/oauth/grant-type/launch/v1';

/** A public card for an agent at `origin`, JSON-RPC 1.0 at `origin/`. */
export function defaultCard(origin) {
  return {
    name: 'fake',
    description: 'an in-process A2A 1.0 agent',
    version: '0.0.0',
    supportedInterfaces: [{ url: `${origin}/`, protocolBinding: 'JSONRPC', protocolVersion: '1.0' }],
    capabilities: { streaming: true, pushNotifications: false, extendedAgentCard: false, extensions: [] },
    defaultInputModes: ['text/plain'],
    defaultOutputModes: ['text/plain'],
    skills: [{ id: 'conversation', name: 'Conversation', description: 'talk', tags: ['conversation'] }],
  };
}

function readBody(req) {
  return new Promise((resolve, reject) => {
    const chunks = [];
    req.on('data', (c) => chunks.push(c));
    req.on('end', () => resolve(Buffer.concat(chunks).toString('utf8')));
    req.on('error', reject);
  });
}

/** An RPC error a handler throws to answer with a JSON-RPC error object. */
export class RpcFailure extends Error {
  constructor(code, message, { status = 200, data, headers = {} } = {}) {
    super(message);
    this.code = code;
    this.status = status;
    this.data = data;
    this.headers = headers;
  }
}

/**
 * Start the fake. Options:
 * - `card(origin)`: the public card (default {@link defaultCard});
 * - `cacheControl`: the card's `Cache-Control` (default `public, max-age=60`);
 * - `bearer`: when set, every JSON-RPC call must carry it or gets 401/-31401.
 */
export async function startFakeA2a(opts = {}) {
  const requests = [];
  const handlers = new Map();
  const failures = new Map();
  const tasks = new Map();
  let seq = 0;

  const fake = {
    /** Every request: {method, path, headers, body, at (epoch ms), rpc?, params?}. */
    requests,
    /** The public card; replace or edit it freely. */
    card: null,
    /**
     * The extended card GetExtendedAgentCard returns. When the public card
     * does not declare one the call answers -32004 (agentd's answer when no
     * listener auth is configured); when it is declared but this is null,
     * -32007 ExtendedAgentCardNotConfigured, as A2A 1.0 names that case.
     */
    extendedCard: null,
    cacheControl: opts.cacheControl ?? 'public, max-age=60',
    /** When set, the card route answers this status and no card. */
    cardStatus: undefined,
    /** When set, the card route serves these bytes instead of `card`. */
    cardBody: undefined,
    /** When set, the card route answers 302 to this Location. */
    cardRedirect: undefined,
    bearer: opts.bearer,
    /**
     * Echo `A2A-Extensions` the way the spec says a server SHOULD: the
     * requested URIs the card declares. Off by default (the echo is optional).
     */
    echo: false,
    /** Streams open right now, and the most ever open at once, by method. */
    open: {},
    maxOpen: {},
    tasks,
    /** The device grant: approved once `approve()` ran. */
    device: { approved: false, token: 'fake-device-token', polls: 0 },
    /**
     * The launch grant, as a launcher's slot answers it: `code` is redeemable
     * once, and only without an Origin header (a NoOrigin code); anything else
     * is `invalid_grant`. Unset `code` means no code was minted.
     */
    launch: { code: undefined, token: `agentd_at_${'f'.repeat(64)}`, used: false },
    url: '',
    origin: '',
    /** JSON-RPC calls, optionally of one method. */
    rpcCalls(method) {
      return requests.filter((r) => r.rpc !== undefined && (method === undefined || r.rpc === method));
    },
    /** Card GETs, each with the `status` it was answered. */
    cardGets() {
      return requests.filter((r) => r.method === 'GET' && r.path === CARD_PATH);
    },
    /** Answer `method` with `fn(params, req)`; a thrown RpcFailure becomes the error. */
    handle(method, fn) {
      handlers.set(method, fn);
    },
    /**
     * Make the next `times` calls of `method` fail with a JSON-RPC error
     * `{code, message, data}` under HTTP `status` (default 200) and `headers`.
     * The method `'*'` fails every call that has no failure of its own.
     */
    fail(method, { code, message = 'injected', data, status = 200, headers = {}, times = Infinity }) {
      failures.set(method, { code, message, data, status, headers, times });
    },
    /** Put a task in the store (a full A2A Task object). */
    putTask(t) {
      tasks.set(t.id, t);
      return t;
    },
    close() {
      return new Promise((resolve) => {
        server.closeAllConnections?.();
        server.close(() => resolve());
      });
    },
  };

  const newTask = (message, state = 'TASK_STATE_SUBMITTED') => {
    const id = `task-${++seq}`;
    return fake.putTask({
      id,
      contextId: message?.contextId ?? `ctx-${seq}`,
      status: { state, timestamp: new Date(Date.UTC(2026, 0, 1, 0, 0, seq)).toISOString() },
      history: message ? [message] : [],
      artifacts: [],
    });
  };

  // The default handlers: a minimal, spec-shaped agent.
  const defaults = {
    SendMessage: ({ message }) => ({ task: newTask(message) }),
    GetTask: ({ id }) => {
      const t = tasks.get(id);
      if (!t) throw new RpcFailure(-32001, 'task not found');
      return t;
    },
    CancelTask: ({ id }) => {
      const t = tasks.get(id);
      if (!t) throw new RpcFailure(-32001, 'task not found');
      t.status = { ...t.status, state: 'TASK_STATE_CANCELED' };
      return t;
    },
    // The filters and projections of A2A 1.0 ListTasks: `status`,
    // `statusTimestampAfter` (inclusive), `includeArtifacts` (default false)
    // and `historyLength` (unset: all).
    ListTasks: ({ pageSize = 50, pageToken = '', status, statusTimestampAfter, includeArtifacts = false, historyLength }) => {
      const after = statusTimestampAfter === undefined ? undefined : Date.parse(statusTimestampAfter);
      const all = [...tasks.values()].filter(
        (t) =>
          (status === undefined || t.status?.state === status) &&
          (after === undefined || Date.parse(t.status?.timestamp ?? '') >= after),
      );
      const from = pageToken === '' ? 0 : Number(pageToken);
      const page = all.slice(from, from + pageSize).map((t) => {
        const out = { ...t };
        if (!includeArtifacts) delete out.artifacts;
        if (historyLength !== undefined) out.history = historyLength === 0 ? undefined : (t.history ?? []).slice(-historyLength);
        return out;
      });
      const next = from + pageSize < all.length ? String(from + pageSize) : '';
      return { tasks: page, nextPageToken: next, pageSize, totalSize: all.length };
    },
    GetExtendedAgentCard: () => {
      if (!fake.card.capabilities?.extendedAgentCard) {
        throw new RpcFailure(-32004, 'the extended agent card is not offered');
      }
      if (!fake.extendedCard) throw new RpcFailure(-32007, 'the extended agent card is not configured');
      return fake.extendedCard;
    },
    // Streams answer with an array of frames.
    SendStreamingMessage: ({ message }) => {
      const t = newTask(message, 'TASK_STATE_WORKING');
      return { stream: [{ task: t }] };
    },
    SubscribeToTask: ({ id }) => {
      const t = tasks.get(id);
      if (!t) throw new RpcFailure(-32001, 'task not found');
      return { stream: [{ task: t }] };
    },
  };

  const send = (res, status, headers, body) => {
    res.writeHead(status, headers);
    res.end(body);
  };
  const rpcError = (res, id, f) =>
    send(
      res,
      f.status,
      { 'content-type': 'application/json', ...f.headers },
      JSON.stringify({ jsonrpc: '2.0', id, error: { code: f.code, message: f.message, ...(f.data !== undefined ? { data: f.data } : {}) } }),
    );

  // The card route records the status it answered, so a test can tell a 304
  // from a 200 without trusting the client's account of it.
  const cardRoute = (req, res, rec) => {
    if (fake.cardRedirect !== undefined) {
      rec.status = 302;
      return send(res, 302, { location: fake.cardRedirect, 'content-type': 'text/plain' }, 'moved');
    }
    if (fake.cardStatus !== undefined) {
      rec.status = fake.cardStatus;
      return send(res, fake.cardStatus, { 'content-type': 'text/plain' }, 'no card');
    }
    const body = fake.cardBody ?? JSON.stringify(fake.card);
    const etag = `"${createHash('sha256').update(body).digest('hex').slice(0, 32)}"`;
    const headers = {
      etag,
      'cache-control': fake.cacheControl,
      'access-control-allow-origin': '*',
    };
    const inm = req.headers['if-none-match'];
    if (inm !== undefined && inm.split(',').some((t) => t.trim().replace(/^W\//, '') === etag || t.trim() === '*')) {
      rec.status = 304;
      return send(res, 304, headers, '');
    }
    rec.status = 200;
    return send(res, 200, { ...headers, 'content-type': 'application/json' }, body);
  };

  const oauthRoute = (req, res, path, form) => {
    const json = (status, v) => send(res, status, { 'content-type': 'application/json', 'cache-control': 'no-store' }, JSON.stringify(v));
    if (path === '/oauth2/device_authorization') {
      return json(200, {
        device_code: 'fake-device-code',
        user_code: 'ABCD-EFGH',
        verification_uri: `${fake.origin}/device`,
        expires_in: 600,
        interval: 1,
      });
    }
    if (path === '/oauth2/token' && form.get('grant_type') === LAUNCH_GRANT_TYPE) {
      const l = fake.launch;
      const ok = l.code !== undefined && !l.used && form.get('code') === l.code && req.headers.origin === undefined;
      // Any presentation of a live code consumes it, as the daemon's slot does.
      if (form.get('code') === l.code) l.used = true;
      if (!ok || !form.get('client_id')) return json(400, { error: 'invalid_grant' });
      return json(200, { access_token: l.token, token_type: 'Bearer', scope: 'operator' });
    }
    if (path === '/oauth2/token') {
      if (form.get('grant_type') !== 'urn:ietf:params:oauth:grant-type:device_code') {
        return json(400, { error: 'unsupported_grant_type' });
      }
      fake.device.polls++;
      if (!fake.device.approved) return json(400, { error: 'authorization_pending' });
      return json(200, { access_token: fake.device.token, token_type: 'Bearer', expires_in: 3600, scope: 'user' });
    }
    return json(404, { error: 'not_found' });
  };

  const server = http.createServer(async (req, res) => {
    const path = new URL(req.url ?? '/', 'http://x').pathname;
    const raw = await readBody(req);
    const rec = { method: req.method, path, headers: { ...req.headers }, body: raw, at: Date.now() };
    requests.push(rec);

    if (path === CARD_PATH && (req.method === 'GET' || req.method === 'HEAD')) return cardRoute(req, res, rec);
    if (path.startsWith('/oauth2/') && req.method === 'POST') return oauthRoute(req, res, path, new URLSearchParams(raw));
    if (req.method !== 'POST' || path !== '/') return send(res, 404, { 'content-type': 'text/plain' }, 'not found');

    let env;
    try {
      env = JSON.parse(raw);
    } catch {
      return rpcError(res, null, { code: -32700, message: 'parse error', status: 200, headers: {} });
    }
    rec.rpc = env.method;
    rec.params = env.params;
    rec.body = env;

    if (fake.bearer !== undefined && req.headers.authorization !== `Bearer ${fake.bearer}`) {
      return rpcError(res, env.id, {
        code: -31401,
        message: 'unauthenticated',
        status: 401,
        headers: { 'www-authenticate': 'Bearer realm="fake"' },
      });
    }
    if (req.headers['a2a-version'] !== '1.0') {
      return rpcError(res, env.id, { code: -32009, message: 'A2A-Version 1.0 required', status: 200, headers: {} });
    }
    const own = failures.get(env.method);
    const f = own && own.times > 0 ? own : failures.get('*');
    if (f && f.times > 0) {
      f.times--;
      return rpcError(res, env.id, f);
    }
    const handler = handlers.get(env.method) ?? defaults[env.method];
    if (!handler) return rpcError(res, env.id, { code: -32601, message: `method not found: ${env.method}`, status: 200, headers: {} });
    let result;
    try {
      result = await handler(env.params ?? {}, rec);
    } catch (e) {
      if (e instanceof RpcFailure) return rpcError(res, env.id, e);
      return rpcError(res, env.id, { code: -32603, message: String(e?.message ?? e), status: 500, headers: {} });
    }
    const echoed = {};
    if (fake.echo) {
      const declared = new Set((fake.card.capabilities?.extensions ?? []).map((e) => e.uri));
      const asked = (req.headers['a2a-extensions'] ?? '').split(',').map((u) => u.trim()).filter((u) => declared.has(u));
      echoed['a2a-extensions'] = asked.join(', ');
    }
    if (result && Array.isArray(result.stream)) {
      // CRLF framing, one `data:` line per frame, as sse-starlette sends it.
      res.writeHead(200, { 'content-type': 'text/event-stream', 'cache-control': 'no-store', ...echoed });
      const m = env.method;
      fake.open[m] = (fake.open[m] ?? 0) + 1;
      fake.maxOpen[m] = Math.max(fake.maxOpen[m] ?? 0, fake.open[m]);
      result.stream.forEach((frame, i) => {
        res.write(`id: ${i + 1}\r\ndata: ${JSON.stringify({ jsonrpc: '2.0', id: env.id, result: frame })}\r\n\r\n`);
      });
      if (result.hold) {
        await Promise.race([result.hold.catch(() => {}), new Promise((r) => res.on('close', r))]);
      }
      fake.open[m]--;
      return res.end();
    }
    return send(res, 200, { 'content-type': 'application/json', ...echoed }, JSON.stringify({ jsonrpc: '2.0', id: env.id, result }));
  });

  await new Promise((resolve) => server.listen(0, '127.0.0.1', resolve));
  fake.origin = `http://127.0.0.1:${server.address().port}`;
  fake.url = `${fake.origin}/`;
  fake.card = (opts.card ?? defaultCard)(fake.origin);
  return fake;
}
