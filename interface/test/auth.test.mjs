// SPDX-License-Identifier: AGPL-3.0-only
// Signing in and holding the result: the card's login options, the device
// grant (RFC 8628) on a fake clock, revocation discovery (RFC 8414), the
// launch grant (code exchange and terminal-approved request), and the
// credential store's binding, expiry and storage discipline.
import test from 'node:test';
import assert from 'node:assert/strict';
import http from 'node:http';
import {
  DEVICE_CODE_GRANT,
  DeviceDenied,
  DeviceExpired,
  InsecureEndpoint,
  IssuerMismatch,
  LAUNCH_GRANT_TYPE,
  LaunchExpired,
  LaunchRefused,
  NoLauncher,
  deviceLogin,
  endpointKey,
  join,
  launchAuthorize,
  launchExchange,
  launchPoll,
  loginOptions,
  revocationEndpointOf,
  revokeToken,
} from '../dist/client/auth.js';
import {
  CREDENTIAL_KEY,
  CredentialStore,
  ENDPOINT_KEY,
  expiryWatch,
  loadEndpoint,
  memoryStorage,
  persistEndpoint,
} from '../dist/client/credstore.js';

const ORIGIN = 'http://127.0.0.1:8420';

/** A clock whose sleeps return at once and advance time by what was asked. */
function fakeClock(start = 1_000_000) {
  const c = {
    t: start,
    sleeps: [],
    now: () => c.t,
    sleep: (ms, signal) => {
      if (signal?.aborted) return Promise.reject(signal.reason);
      c.sleeps.push(ms);
      c.t += ms;
      return Promise.resolve();
    },
  };
  return c;
}

/** A fetch that answers from a script, recording every request. */
function scriptedFetch(answers) {
  const calls = [];
  const f = async (url, init = {}) => {
    const headers = Object.fromEntries(Object.entries(init.headers ?? {}).map(([k, v]) => [k.toLowerCase(), v]));
    calls.push({
      url,
      method: init.method ?? 'GET',
      headers,
      form: init.body !== undefined ? Object.fromEntries(new URLSearchParams(init.body)) : undefined,
      redirect: init.redirect,
      credentials: init.credentials,
    });
    const a = answers.shift();
    if (!a) throw new Error(`unscripted request to ${url}`);
    return new Response(a.body === undefined ? null : JSON.stringify(a.body), {
      status: a.status ?? 200,
      headers: { 'content-type': 'application/json', ...(a.headers ?? {}) },
    });
  };
  f.calls = calls;
  return f;
}

const oauthError = (error) => ({ status: 400, body: { error } });

const FLOW = {
  deviceAuthorizationUrl: `${ORIGIN}/oauth2/device_authorization`,
  tokenUrl: `${ORIGIN}/oauth2/token`,
};

const DEVICE_CODE = {
  body: {
    device_code: 'dc1',
    user_code: 'BCDF-GHJK',
    verification_uri: `${ORIGIN}/oauth2/device`,
    expires_in: 600,
    interval: 5,
  },
};

test('device login', async (t) => {
  await t.test('pending, then slow_down, then the token', async () => {
    const clock = fakeClock();
    const f = scriptedFetch([
      DEVICE_CODE,
      oauthError('authorization_pending'),
      oauthError('slow_down'),
      { body: { access_token: 'agentd_at_1', token_type: 'Bearer', expires_in: 3600, scope: 'user' } },
    ]);
    const shown = [];
    const cred = await deviceLogin({ flow: FLOW, clientId: 'agentd-tui', fetch: f, clock, onCode: (c) => shown.push(c) });
    assert.deepEqual(shown, [{ userCode: 'BCDF-GHJK', verificationUri: `${ORIGIN}/oauth2/device`, expiresIn: 600 }]);
    // Wait the interval before each poll; slow_down adds 5 s to every later
    // wait (RFC 8628 §3.5).
    assert.deepEqual(clock.sleeps, [5000, 5000, 10000]);
    assert.ok(clock.sleeps[2] >= 5000 + 5000, 'slow_down must lengthen the wait by 5 s');
    assert.deepEqual(cred, { token: 'agentd_at_1', scope: 'user', expiresAt: clock.t + 3600 * 1000 });
    assert.deepEqual(f.calls[0].form, { client_id: 'agentd-tui' });
    for (const c of f.calls.slice(1)) {
      assert.equal(c.url, FLOW.tokenUrl);
      assert.deepEqual(c.form, { grant_type: DEVICE_CODE_GRANT, device_code: 'dc1', client_id: 'agentd-tui' });
      assert.equal(c.headers['content-type'], 'application/x-www-form-urlencoded');
      assert.equal(c.redirect, 'error');
      assert.equal(c.credentials, 'omit');
    }
  });

  await t.test('access_denied and expired_token are told apart', async () => {
    const denied = deviceLogin({
      flow: FLOW,
      clientId: 'c',
      clock: fakeClock(),
      onCode: () => {},
      fetch: scriptedFetch([DEVICE_CODE, oauthError('access_denied')]),
    });
    await assert.rejects(denied, (e) => e instanceof DeviceDenied && !(e instanceof DeviceExpired));
    const expired = deviceLogin({
      flow: FLOW,
      clientId: 'c',
      clock: fakeClock(),
      onCode: () => {},
      fetch: scriptedFetch([DEVICE_CODE, oauthError('expired_token')]),
    });
    await assert.rejects(expired, (e) => e instanceof DeviceExpired && !(e instanceof DeviceDenied));
    // The code's own lifetime ends the polling locally, without another request.
    const f = scriptedFetch([{ body: { ...DEVICE_CODE.body, expires_in: 12 } }, oauthError('authorization_pending'), oauthError('authorization_pending')]);
    await assert.rejects(deviceLogin({ flow: FLOW, clientId: 'c', clock: fakeClock(), onCode: () => {}, fetch: f }), DeviceExpired);
    assert.equal(f.calls.length, 3);
  });

  await t.test('a 429 on the token endpoint waits out its Retry-After', async () => {
    const clock = fakeClock();
    const f = scriptedFetch([
      DEVICE_CODE,
      { status: 429, body: { error: 'slow_down' }, headers: { 'retry-after': '30' } },
      { body: { access_token: 'agentd_at_1', token_type: 'Bearer' } },
    ]);
    await deviceLogin({ flow: FLOW, clientId: 'c', fetch: f, clock, onCode: () => {} });
    // The interval before the first poll, then the server's 30 s — not the
    // 5 s interval, and not the slow_down step either.
    assert.deepEqual(clock.sleeps, [5000, 30000]);
  });

  await t.test('a token response that is not a Bearer is refused', async () => {
    const f = scriptedFetch([DEVICE_CODE, { body: { access_token: 'agentd_at_1', token_type: 'mac' } }]);
    await assert.rejects(
      deviceLogin({ flow: FLOW, clientId: 'c', fetch: f, clock: fakeClock(), onCode: () => {} }),
      (e) => e.code === 'invalid-response',
    );
  });

  await t.test('a verification URI a person could not safely follow is refused', async () => {
    for (const uri of ['javascript:alert(document.domain)', 'http://phish.example/device', 'not a url']) {
      const shown = [];
      const f = scriptedFetch([{ body: { ...DEVICE_CODE.body, verification_uri: uri } }]);
      await assert.rejects(
        deviceLogin({ flow: FLOW, clientId: 'c', fetch: f, clock: fakeClock(), onCode: (c) => shown.push(c) }),
        (e) => e.code === 'invalid-response',
        uri,
      );
      assert.deepEqual(shown, [], `onCode must never see ${uri}`);
      assert.equal(f.calls.length, 1, 'no poll follows');
    }
    // A bad complete URI is dropped; the plain one still works.
    const shown = [];
    const f = scriptedFetch([
      { body: { ...DEVICE_CODE.body, verification_uri_complete: 'javascript:alert(1)' } },
      { body: { access_token: 'a', token_type: 'Bearer' } },
    ]);
    await deviceLogin({ flow: FLOW, clientId: 'c', fetch: f, clock: fakeClock(), onCode: (c) => shown.push(c) });
    assert.equal(shown.length, 1);
    assert.equal(shown[0].verificationUri, `${ORIGIN}/oauth2/device`);
    assert.ok(!('verificationUriComplete' in shown[0]), JSON.stringify(shown[0]));
    // A safe complete URI is passed through.
    const ok = [];
    await deviceLogin({
      flow: FLOW,
      clientId: 'c',
      clock: fakeClock(),
      onCode: (c) => ok.push(c),
      fetch: scriptedFetch([
        { body: { ...DEVICE_CODE.body, verification_uri_complete: `${ORIGIN}/oauth2/device?user_code=BCDF-GHJK` } },
        { body: { access_token: 'a', token_type: 'Bearer' } },
      ]),
    });
    assert.equal(ok[0].verificationUriComplete, `${ORIGIN}/oauth2/device?user_code=BCDF-GHJK`);
  });

  await t.test('the token endpoint must share the device endpoint origin', async () => {
    const f = scriptedFetch([DEVICE_CODE]);
    const flow = { deviceAuthorizationUrl: 'https://a.example/oauth2/device_authorization', tokenUrl: 'https://evil.example/oauth2/token' };
    await assert.rejects(deviceLogin({ flow, clientId: 'c', clock: fakeClock(), onCode: () => {}, fetch: f }), IssuerMismatch);
    assert.equal(f.calls.length, 0, 'nothing is sent to either');
  });

  await t.test('plain http off loopback is refused before any request', async () => {
    for (const flow of [
      { ...FLOW, tokenUrl: 'http://agent.example/oauth2/token' },
      { ...FLOW, deviceAuthorizationUrl: 'http://10.0.0.5:8420/oauth2/device_authorization' },
    ]) {
      const f = scriptedFetch([DEVICE_CODE]);
      await assert.rejects(deviceLogin({ flow, clientId: 'c', clock: fakeClock(), onCode: () => {}, fetch: f }), InsecureEndpoint);
      assert.equal(f.calls.length, 0);
    }
    // https anywhere, and http on every loopback spelling, pass the guard.
    for (const origin of ['https://agent.example', 'http://localhost:1', 'http://127.9.9.9:1', 'http://[::1]:1']) {
      const f = scriptedFetch([DEVICE_CODE, { body: { access_token: 'a', token_type: 'bearer' } }]);
      const flow = { deviceAuthorizationUrl: join(origin, '/oauth2/device_authorization'), tokenUrl: join(origin, '/oauth2/token') };
      await deviceLogin({ flow, clientId: 'c', clock: fakeClock(), onCode: () => {}, fetch: f });
      assert.equal(f.calls.length, 2, origin);
    }
  });
});

test('revocation endpoint', async () => {
  const flow = { tokenUrl: `${ORIGIN}/oauth2/token` };
  const meta = (issuer, revocation_endpoint = `${ORIGIN}/oauth2/revoke`) => ({
    body: { issuer, token_endpoint: `${ORIGIN}/oauth2/token`, revocation_endpoint },
  });
  // Loopback http: the card declares no metadata URL, so the well-known path
  // on the token endpoint's origin is read.
  let f = scriptedFetch([meta('http://evil.example')]);
  await assert.rejects(revocationEndpointOf(flow, { fetch: f }), IssuerMismatch);
  assert.equal(f.calls[0].url, `${ORIGIN}/.well-known/oauth-authorization-server`);
  f = scriptedFetch([meta(ORIGIN)]);
  assert.equal(await revocationEndpointOf(flow, { fetch: f }), `${ORIGIN}/oauth2/revoke`);
  // The token goes to the revocation endpoint, so it must be on the issuer.
  f = scriptedFetch([meta(ORIGIN, 'https://elsewhere.example/revoke')]);
  await assert.rejects(revocationEndpointOf(flow, { fetch: f }), IssuerMismatch);
  // A declared metadata URL is used as given; its issuer must still be the origin.
  const https = { tokenUrl: 'https://agent.example/oauth2/token', oauth2MetadataUrl: 'https://agent.example/.well-known/oauth-authorization-server' };
  f = scriptedFetch([{ body: { issuer: 'https://agent.example', revocation_endpoint: 'https://agent.example/oauth2/revoke' } }]);
  assert.equal(await revocationEndpointOf(https, { fetch: f }), 'https://agent.example/oauth2/revoke');
  assert.equal(f.calls[0].url, https.oauth2MetadataUrl);
  f = scriptedFetch([{ body: { issuer: 'https://agent.example/', revocation_endpoint: 'https://agent.example/oauth2/revoke' } }]);
  await assert.rejects(revocationEndpointOf(https, { fetch: f }), IssuerMismatch);
  // https without a declared metadata URL: nothing to ask.
  f = scriptedFetch([]);
  assert.equal(await revocationEndpointOf({ tokenUrl: 'https://agent.example/oauth2/token' }, { fetch: f }), undefined);
  assert.equal(f.calls.length, 0);

  f = scriptedFetch([{ status: 200 }]);
  await revokeToken(`${ORIGIN}/oauth2/revoke`, 'agentd_at_1', { fetch: f, clientId: 'agentd-ui' });
  assert.deepEqual(f.calls[0].form, { token: 'agentd_at_1', token_type_hint: 'access_token', client_id: 'agentd-ui' });
  f = scriptedFetch([]);
  await assert.rejects(revokeToken('http://agent.example/oauth2/revoke', 't', { fetch: f }), InsecureEndpoint);
  assert.equal(f.calls.length, 0);
});

test('login options', () => {
  // The card an agentd with a2a.bearer and the device grant publishes.
  const card = {
    securitySchemes: {
      bearer: { httpAuthSecurityScheme: { scheme: 'Bearer', description: 'b' } },
      device_code: {
        oauth2SecurityScheme: {
          flows: {
            deviceCode: {
              deviceAuthorizationUrl: `${ORIGIN}/oauth2/device_authorization`,
              tokenUrl: `${ORIGIN}/oauth2/token`,
              scopes: { user: 'u', operator: 'o' },
            },
          },
        },
      },
    },
    securityRequirements: [{ schemes: { bearer: {} } }, { schemes: { device_code: {} } }, {}],
  };
  assert.deepEqual(loginOptions(card), [
    { method: 'bearer', scheme: 'bearer' },
    {
      method: 'device',
      flow: {
        scheme: 'device_code',
        deviceAuthorizationUrl: `${ORIGIN}/oauth2/device_authorization`,
        tokenUrl: `${ORIGIN}/oauth2/token`,
        scopes: ['user', 'operator'],
      },
    },
    { method: 'none' },
  ]);
  // No security declared: the anonymous way in only.
  assert.deepEqual(loginOptions({ name: 'a' }), [{ method: 'none' }]);
  // mTLS alone, mTLS with a bearer, an API key, an undeclared name: nothing this client can present.
  const mtls = {
    securitySchemes: { mtls: { mtlsSecurityScheme: {} }, bearer: card.securitySchemes.bearer },
    securityRequirements: [{ schemes: { mtls: {} } }, { schemes: { mtls: {}, bearer: {} } }, { schemes: { ghost: {} } }],
  };
  const got = loginOptions(mtls);
  assert.deepEqual(got.map((o) => o.method), ['unsupported', 'unsupported', 'unsupported']);
  assert.deepEqual(got[1].schemes, ['mtls', 'bearer']);
  // Schemes without requirements: each on its own; the metadata URL travels.
  const https = {
    securitySchemes: {
      key: { apiKeySecurityScheme: { location: 'header', name: 'x' } },
      dc: {
        oauth2SecurityScheme: {
          flows: { deviceCode: { deviceAuthorizationUrl: 'https://a/d', tokenUrl: 'https://a/t', scopes: {} } },
          oauth2MetadataUrl: 'https://a/.well-known/oauth-authorization-server',
        },
      },
    },
  };
  const [key, dc] = loginOptions(https);
  assert.equal(key.method, 'unsupported');
  assert.equal(dc.flow.oauth2MetadataUrl, 'https://a/.well-known/oauth-authorization-server');
});

test('credentials stay bound', async () => {
  const clock = fakeClock();
  const storage = memoryStorage();
  const store = new CredentialStore(storage, clock);
  const a = endpointKey('http://127.0.0.1:8420');
  assert.equal(a, 'http://127.0.0.1:8420/');
  assert.equal(endpointKey('http://127.0.0.1:8420/?endpoint=x#frag'), a);
  store.set(a, { token: 'agentd_at_a', scope: 'operator', expiresAt: clock.t + 120_000 });
  assert.equal(store.get(a).token, 'agentd_at_a');
  // Another port, another path, another host: not the same endpoint.
  for (const other of ['http://127.0.0.1:8421', 'http://127.0.0.1:8420/x', 'http://localhost:8420']) {
    assert.equal(store.get(endpointKey(other)), undefined, other);
  }
  // Expired: dropped, and gone from storage too.
  clock.t += 120_000;
  assert.equal(store.get(a), undefined);
  assert.equal(storage.getItem(CREDENTIAL_KEY), null);
  // No expiry: kept until revoked.
  store.set(a, { token: 't' });
  clock.t += 30 * 86_400_000;
  assert.equal(store.get(a).token, 't');
  store.delete(a);
  assert.equal(store.get(a), undefined);

  const local = memoryStorage();
  persistEndpoint(local, { endpoint: 'http://127.0.0.1:8420' });
  assert.deepEqual(JSON.parse(local.getItem(ENDPOINT_KEY)), { endpoint: 'http://127.0.0.1:8420' });
  assert.equal(loadEndpoint(local), 'http://127.0.0.1:8420');

  // Warn a minute before expiry, then expire at it.
  const wclock = fakeClock(0);
  const fired = [];
  const w = expiryWatch(
    { token: 't', expiresAt: 300_000 },
    { warn: () => fired.push(['warn', wclock.now()]), expired: () => fired.push(['expired', wclock.now()]) },
    wclock,
  );
  await w.done;
  assert.deepEqual(fired, [
    ['warn', 300_000 - 60_000],
    ['expired', 300_000],
  ]);
  // Stopped before the warning: neither fires.
  const s = fakeClock(0);
  const quiet = [];
  const w2 = expiryWatch({ token: 't', expiresAt: 300_000 }, { warn: () => quiet.push('w'), expired: () => quiet.push('e') }, {
    now: s.now,
    sleep: (ms, signal) => new Promise((_, reject) => signal.addEventListener('abort', () => reject(signal.reason))),
  });
  w2.stop();
  await w2.done;
  assert.deepEqual(quiet, []);
});

/** A local HTTP server recording each request; `answer` decides the reply. */
async function recordingServer(answer) {
  const seen = [];
  const server = http.createServer((req, res) => {
    let body = '';
    req.on('data', (c) => (body += c));
    req.on('end', () => {
      seen.push({ method: req.method, url: req.url, headers: req.headers, body });
      const { status, json } = answer(seen.length);
      res.writeHead(status, { 'content-type': 'application/json', 'cache-control': 'no-store' });
      res.end(JSON.stringify(json));
    });
  });
  await new Promise((r) => server.listen(0, '127.0.0.1', r));
  const { port } = server.address();
  return { seen, origin: `http://127.0.0.1:${port}`, close: () => new Promise((r) => server.close(r)) };
}

test('launch exchange', async () => {
  const ok = await recordingServer(() => ({
    status: 200,
    json: { access_token: 'agentd_at_l', token_type: 'Bearer', scope: 'operator' },
  }));
  try {
    const cred = await launchExchange(join(ok.origin, '/oauth2/token'), 'agentd_lc_abc', 'agentd-tui');
    // A tui launch session has no expiry.
    assert.deepEqual(cred, { token: 'agentd_at_l', scope: 'operator' });
    assert.equal(ok.seen.length, 1);
    const [r] = ok.seen;
    assert.equal(r.method, 'POST');
    assert.equal(r.url, '/oauth2/token');
    assert.equal(r.headers['content-type'], 'application/x-www-form-urlencoded');
    assert.deepEqual(Object.fromEntries(new URLSearchParams(r.body)), {
      grant_type: LAUNCH_GRANT_TYPE,
      code: 'agentd_lc_abc',
      client_id: 'agentd-tui',
    });
    // The daemon binds a tui code to the ABSENCE of Origin.
    assert.equal(r.headers.origin, undefined);
  } finally {
    await ok.close();
  }
  const refused = await recordingServer(() => ({ status: 400, json: { error: 'invalid_grant' } }));
  try {
    await assert.rejects(launchExchange(join(refused.origin, '/oauth2/token'), 'agentd_lc_x', 'agentd-ui'), LaunchRefused);
    // A launch code is single-use: a retry could only be refused.
    assert.equal(refused.seen.length, 1);
  } finally {
    await refused.close();
  }
  assert.equal(LAUNCH_GRANT_TYPE, 'https://agentd.dev/oauth/grant-type/launch/v1');
  await assert.rejects(launchExchange('http://agent.example/oauth2/token', 'c', 'agentd-ui'), InsecureEndpoint);
  // A launch code is redeemable only over loopback, so even https elsewhere
  // would only burn it — or hand it to whoever answers. Nothing is sent.
  const f = scriptedFetch([]);
  await assert.rejects(launchExchange('https://agent.example/oauth2/token', 'c', 'agentd-ui', { fetch: f }), InsecureEndpoint);
  await assert.rejects(launchAuthorize('https://agent.example/oauth2/launch_authorization', 'agentd-ui', { fetch: f }), InsecureEndpoint);
  await assert.rejects(
    launchPoll('https://agent.example/oauth2/token', 'agentd_lr_1', 'agentd-ui', fakeClock(), { fetch: f }),
    InsecureEndpoint,
  );
  assert.equal(f.calls.length, 0);
});

test('terminal launch request', async () => {
  const authUrl = `${ORIGIN}/oauth2/launch_authorization`;
  const tokenUrl = `${ORIGIN}/oauth2/token`;
  let f = scriptedFetch([{ body: { request_code: 'agentd_lr_1', user_code: 'BCDF-GHJK', expires_in: 120, interval: 2 } }]);
  const req = await launchAuthorize(authUrl, 'agentd-ui', { fetch: f });
  assert.deepEqual(req, { requestCode: 'agentd_lr_1', userCode: 'BCDF-GHJK', expiresIn: 120, interval: 2 });
  assert.equal(f.calls[0].method, 'POST');
  assert.equal(f.calls[0].headers['content-type'], 'application/x-www-form-urlencoded');
  assert.deepEqual(f.calls[0].form, { client_id: 'agentd-ui' });

  f = scriptedFetch([{ status: 404 }]);
  await assert.rejects(launchAuthorize(authUrl, 'agentd-ui', { fetch: f }), NoLauncher);

  const clock = fakeClock();
  f = scriptedFetch([
    oauthError('authorization_pending'),
    oauthError('slow_down'),
    { body: { access_token: 'agentd_at_t', token_type: 'Bearer', scope: 'operator', expires_in: 28800 } },
  ]);
  const cred = await launchPoll(tokenUrl, 'agentd_lr_1', 'agentd-ui', clock, { fetch: f, interval: 2 });
  // `interval` between polls; slow_down adds 2 s to every later one.
  assert.deepEqual(clock.sleeps, [2000, 2000, 4000]);
  assert.deepEqual(cred, { token: 'agentd_at_t', scope: 'operator', expiresAt: clock.t + 28800 * 1000 });
  for (const c of f.calls) {
    assert.deepEqual(c.form, { grant_type: LAUNCH_GRANT_TYPE, request_code: 'agentd_lr_1', client_id: 'agentd-ui' });
  }

  f = scriptedFetch([oauthError('authorization_pending'), oauthError('expired_token')]);
  await assert.rejects(launchPoll(tokenUrl, 'agentd_lr_1', 'agentd-ui', fakeClock(), { fetch: f, interval: 2 }), LaunchExpired);
  f = scriptedFetch([oauthError('invalid_grant')]);
  await assert.rejects(launchPoll(tokenUrl, 'agentd_lr_1', 'agentd-ui', fakeClock(), { fetch: f, interval: 2 }), LaunchRefused);
});

/** A Storage that records every write. */
function recordingStorage() {
  const s = memoryStorage();
  const writes = [];
  return {
    writes,
    getItem: (k) => s.getItem(k),
    setItem: (k, v) => {
      writes.push([k, v]);
      s.setItem(k, v);
    },
    removeItem: (k) => {
      writes.push([k, null]);
      s.removeItem(k);
    },
  };
}

test('no credential reaches persistent storage', () => {
  const local = recordingStorage();
  persistEndpoint(local, { endpoint: 'http://127.0.0.1:8420', bearer: 'sekrit', token: 'agentd_at_x' });
  assert.deepEqual(local.writes, [[ENDPOINT_KEY, JSON.stringify({ endpoint: 'http://127.0.0.1:8420' })]]);

  // A v1.16 web UI left `{endpoint, bearer}` under the same key. Reading the
  // endpoint scrubs the bearer out of persistent storage.
  const legacy = recordingStorage();
  legacy.setItem(ENDPOINT_KEY, JSON.stringify({ endpoint: 'http://127.0.0.1:8420', bearer: 'operator-secret' }));
  assert.equal(loadEndpoint(legacy), 'http://127.0.0.1:8420');
  assert.deepEqual(JSON.parse(legacy.getItem(ENDPOINT_KEY)), { endpoint: 'http://127.0.0.1:8420' });
  // …and an entry with a credential but no endpoint to keep is removed.
  for (const junk of [JSON.stringify({ bearer: 'operator-secret' }), '{not json', JSON.stringify(['x'])]) {
    const s = recordingStorage();
    s.setItem(ENDPOINT_KEY, junk);
    assert.equal(loadEndpoint(s), undefined, junk);
    assert.equal(s.getItem(ENDPOINT_KEY), null, junk);
  }
  // A clean entry is read without a write.
  const clean = recordingStorage();
  persistEndpoint(clean, { endpoint: 'http://127.0.0.1:8420' });
  clean.writes.length = 0;
  assert.equal(loadEndpoint(clean), 'http://127.0.0.1:8420');
  assert.deepEqual(clean.writes, []);

  // Whatever the global storages are, a store writes only to the one it was given.
  const had = { localStorage: globalThis.localStorage, sessionStorage: globalThis.sessionStorage };
  const globalLocal = recordingStorage();
  const globalSession = recordingStorage();
  Object.defineProperty(globalThis, 'localStorage', { value: globalLocal, configurable: true, writable: true });
  Object.defineProperty(globalThis, 'sessionStorage', { value: globalSession, configurable: true, writable: true });
  try {
    const mine = recordingStorage();
    const store = new CredentialStore(mine);
    const key = endpointKey('http://127.0.0.1:8420');
    store.set(key, { token: 'agentd_at_x', scope: 'operator', expiresAt: Date.now() + 60_000, extra: 'dropped' });
    store.get(key);
    store.delete(key);
    store.clear();
    assert.deepEqual(globalLocal.writes, []);
    assert.deepEqual(globalSession.writes, []);
    assert.ok(mine.writes.length > 0);
    assert.ok(mine.writes.every(([k]) => k === CREDENTIAL_KEY));
    // Only the credential's own fields are stored.
    assert.deepEqual(Object.keys(JSON.parse(mine.writes[0][1])[key]).sort(), ['expiresAt', 'scope', 'token']);
  } finally {
    for (const [k, v] of Object.entries(had)) {
      Object.defineProperty(globalThis, k, { value: v, configurable: true, writable: true });
    }
  }
});
