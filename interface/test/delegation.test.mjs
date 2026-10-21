// SPDX-License-Identifier: AGPL-3.0-only
// Many people, each signed in through their own identity provider (RFC 0044
// P2): the sign-in at the IdP the daemon federates (`a2a.login`) on a fake
// clock, the consent gate and enrolment link a task carries, the person's
// own `/enrol` and `/consent`, and the TUI's `--login idp`.
import test from 'node:test';
import assert from 'node:assert/strict';
import {
  IdpLoginExpired,
  IdpLoginRefused,
  IssuerMismatch,
  LOGIN_GRANT_TYPE,
  idpLogin,
  idpLoginOf,
} from '../dist/client/auth.js';
import { normalizeTask } from '../dist/client/client.js';
import {
  consentLine,
  consentWait,
  currentGate,
  parseDelegationCommand,
  runDelegationCommand,
} from '../dist/client/composer.js';
import { TASK_ANNOTATIONS_EXTENSION } from '../dist/client/ext.js';
import { chooseSignIn, parseArgs, resolveCredential } from '../dist/tui/args.js';

const ORIGIN = 'http://127.0.0.1:8420';

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

function scriptedFetch(answers) {
  const calls = [];
  const f = async (url, init = {}) => {
    calls.push({
      url,
      method: init.method ?? 'GET',
      form: init.body !== undefined ? Object.fromEntries(new URLSearchParams(init.body)) : undefined,
      redirect: init.redirect,
      credentials: init.credentials,
    });
    const a = answers.shift();
    if (!a) throw new Error(`unscripted request to ${url}`);
    return new Response(a.body === undefined ? null : JSON.stringify(a.body), {
      status: a.status ?? 200,
      headers: { 'content-type': 'application/json' },
    });
  };
  f.calls = calls;
  return f;
}

const META = {
  body: {
    issuer: ORIGIN,
    token_endpoint: `${ORIGIN}/oauth2/token`,
    revocation_endpoint: `${ORIGIN}/oauth2/revoke`,
    login_authorization_endpoint: `${ORIGIN}/oauth2/login_authorization`,
  },
};
const FLOW = { loginAuthorizationUrl: `${ORIGIN}/oauth2/login_authorization`, tokenUrl: `${ORIGIN}/oauth2/token` };
const ASKED = {
  body: {
    request_code: 'agentd_lq_1',
    start_uri: `${ORIGIN}/oauth2/start/abc`,
    start_expires_in: 60,
    expires_in: 600,
    interval: 2,
  },
};

test('the IdP sign-in the metadata names', async (t) => {
  await t.test('found on the issuer origin', async () => {
    const f = scriptedFetch([META]);
    assert.deepEqual(await idpLoginOf(ORIGIN, { fetch: f }), FLOW);
    assert.equal(f.calls[0].url, `${ORIGIN}/.well-known/oauth-authorization-server`);
  });
  await t.test('absent: no metadata, or none names it', async () => {
    assert.equal(await idpLoginOf(ORIGIN, { fetch: scriptedFetch([{ status: 404 }]) }), undefined);
    const plain = { body: { ...META.body, login_authorization_endpoint: undefined } };
    assert.equal(await idpLoginOf(ORIGIN, { fetch: scriptedFetch([plain]) }), undefined);
  });
  await t.test('metadata of another issuer, or endpoints elsewhere, are refused', async () => {
    const other = { body: { ...META.body, issuer: 'http://127.0.0.1:9999' } };
    await assert.rejects(idpLoginOf(ORIGIN, { fetch: scriptedFetch([other]) }), IssuerMismatch);
    const away = { body: { ...META.body, login_authorization_endpoint: 'http://127.0.0.1:9999/oauth2/login_authorization' } };
    await assert.rejects(idpLoginOf(ORIGIN, { fetch: scriptedFetch([away]) }), IssuerMismatch);
  });
});

test('signing in at the IdP', async (t) => {
  await t.test('the start link, then pending, then the session', async () => {
    const clock = fakeClock();
    const f = scriptedFetch([
      ASKED,
      { status: 400, body: { error: 'authorization_pending' } },
      { body: { access_token: 'agentd_at_9', token_type: 'Bearer', expires_in: 28800, scope: 'user' } },
    ]);
    const links = [];
    const cred = await idpLogin({ flow: FLOW, clientId: 'agentd-ui', fetch: f, clock, onLink: (l) => links.push(l) });
    assert.deepEqual(links, [{ uri: `${ORIGIN}/oauth2/start/abc`, expiresIn: 60 }]);
    assert.deepEqual(f.calls[0].form, { client_id: 'agentd-ui' });
    for (const c of f.calls.slice(1)) {
      assert.deepEqual(c.form, { grant_type: LOGIN_GRANT_TYPE, request_code: 'agentd_lq_1', client_id: 'agentd-ui' });
      assert.equal(c.redirect, 'error');
      assert.equal(c.credentials, 'omit');
    }
    assert.deepEqual(clock.sleeps, [2000, 2000]);
    assert.equal(cred.token, 'agentd_at_9');
  });
  await t.test('a refusal says why; an expiry is its own error', async () => {
    const refused = scriptedFetch([ASKED, { status: 400, body: { error: 'access_denied', error_description: 'the sign-in came back to a browser that did not start it' } }]);
    await assert.rejects(
      idpLogin({ flow: FLOW, clientId: 'c', fetch: refused, clock: fakeClock(), onLink: () => {} }),
      (e) => e instanceof IdpLoginRefused && /did not start it/.test(e.message),
    );
    const expired = scriptedFetch([ASKED, { status: 400, body: { error: 'expired_token' } }]);
    await assert.rejects(idpLogin({ flow: FLOW, clientId: 'c', fetch: expired, clock: fakeClock(), onLink: () => {} }), IdpLoginExpired);
  });
  await t.test('a start link off the listener origin is never handed to the browser', async () => {
    const away = { body: { ...ASKED.body, start_uri: 'https://idp.example/authorize?x=1' } };
    const links = [];
    await assert.rejects(
      idpLogin({ flow: FLOW, clientId: 'c', fetch: scriptedFetch([away]), clock: fakeClock(), onLink: (l) => links.push(l) }),
      IssuerMismatch,
    );
    const script = { body: { ...ASKED.body, start_uri: 'javascript:alert(1)' } };
    await assert.rejects(idpLogin({ flow: FLOW, clientId: 'c', fetch: scriptedFetch([script]), clock: fakeClock(), onLink: (l) => links.push(l) }));
    assert.deepEqual(links, []);
  });
});

/** A task waiting on `annotations`, in conversation `ctx`. */
function task(id, ctx, annotations, state = 'TASK_STATE_INPUT_REQUIRED') {
  return normalizeTask({
    id,
    contextId: ctx,
    status: { state, timestamp: '2026-10-21T00:00:00Z' },
    metadata: { [TASK_ANNOTATIONS_EXTENSION]: annotations },
  });
}

test('a consent gate and an enrolment link, as a task carries them', () => {
  const action = task('t1', 'c1', { securityGate: true, consentGate: { gate: 'g1', kind: 'action' } });
  assert.deepEqual(action.consentGate, { gate: 'g1', kind: 'action' });
  const step = task('t2', 'c1', { consentGate: { gate: 'g2', kind: 'authorize', server: 'linear', start_uri: `${ORIGIN}/oauth2/start/s`, expires_at: 5 } });
  assert.deepEqual(step.consentGate, { gate: 'g2', kind: 'authorize', server: 'linear', startUri: `${ORIGIN}/oauth2/start/s`, expiresAt: 5 });
  // A link this page would run, or send in the clear, is dropped.
  const bad = task('t3', 'c1', { consentGate: { gate: 'g3', kind: 'authorize', start_uri: 'javascript:alert(1)' } });
  assert.equal(bad.consentGate.startUri, undefined);
  const enrol = task('t4', 'c1', { signIn: { idp: 'okta', start_uri: `${ORIGIN}/oauth2/start/e`, expires_at: 9 } }, 'TASK_STATE_WORKING');
  assert.deepEqual(enrol.signIn, { idp: 'okta', startUri: `${ORIGIN}/oauth2/start/e`, expiresAt: 9 });
  assert.equal(task('t5', 'c1', { signIn: { start_uri: 'http://idp.example/x' } }).signIn, undefined);
  assert.equal(task('t6', 'c1', { consentGate: { gate: 'g6', kind: 'other' } }).consentGate, undefined);
});

test('a message never answers a consent gate', () => {
  const s = { tasks: new Map() };
  const ask = task('ask', 'c1', {});
  const gate = task('gate', 'c1', { consentGate: { gate: 'g1', kind: 'action' } });
  gate.updated = ask.updated + 1;
  s.tasks.set(ask.id, ask);
  s.tasks.set(gate.id, gate);
  assert.equal(currentGate(s, 'c1')?.id, 'ask', 'the newer consent gate is not what a reply answers');
  assert.equal(consentWait(s, 'c1')?.id, 'gate');
  assert.equal(consentWait(s, 'c2'), undefined, 'never another conversation\'s');
  assert.match(consentLine(gate.consentGate), /\/consent approve g1 · \/consent deny g1/);
  assert.match(consentLine({ gate: 'g2', kind: 'device', verificationUri: 'https://idp.example/activate', userCode: 'ABCD' }), /enter ABCD/);
});

test('a step-up sign-in is a consent gate on an AUTH_REQUIRED task', () => {
  const s = { tasks: new Map() };
  const step = task('step', 'c1', { consentGate: { gate: 'g3', kind: 'authorize', start_uri: `${ORIGIN}/oauth2/start/s` } }, 'TASK_STATE_AUTH_REQUIRED');
  s.tasks.set(step.id, step);
  assert.equal(consentWait(s, 'c1')?.id, 'step', 'its sign-in link and its deny are shown');
  assert.equal(currentGate(s, 'c1'), undefined, 'a reply answers no sign-in');
  // A task waiting on a sign-in with no consent gate is no consent gate.
  const s2 = { tasks: new Map([['admit', task('admit', 'c1', { signIn: { idp: 'okta', start_uri: `${ORIGIN}/oauth2/start/a` } }, 'TASK_STATE_AUTH_REQUIRED')]]) };
  assert.equal(consentWait(s2, 'c1'), undefined);
});

test('/enrol and /consent', async (t) => {
  await t.test('parsed, or their usage', () => {
    assert.deepEqual(parseDelegationCommand('enrol', ['okta']), { cmd: 'enrol', idp: 'okta' });
    assert.ok('usage' in parseDelegationCommand('enrol', []));
    assert.deepEqual(parseDelegationCommand('consent', []), { cmd: 'consent' });
    assert.deepEqual(parseDelegationCommand('consent', ['deny', 'g1']), { cmd: 'consent', answer: 'deny', gate: 'g1' });
    assert.ok('usage' in parseDelegationCommand('consent', ['maybe', 'g1']));
    assert.equal(parseDelegationCommand('approve', ['X']), null, 'a device approval is not this');
  });
  const listed = {
    grants: [],
    gates: [
      { gate: 'g1', tool: 'linear.create_issue', server: 'linear', nonce: 'n1', args_hash: 'h1' },
      { gate: 'g9', tool: 'linear.create_issue', server: 'linear' },
    ],
  };
  const done = (text) => ({ kind: 'task', task: { id: 'x', state: 'TASK_STATE_COMPLETED', message: text }, data: undefined });
  await t.test('an approval is bound to the nonce and hash the daemon listed', async () => {
    const sent = [];
    const client = {
      delegationList: async () => listed,
      delegationApprove: async (...a) => (sent.push(['approve', ...a]), done('approved')),
      delegationDeny: async (...a) => (sent.push(['deny', ...a]), done('denied')),
    };
    assert.equal((await runDelegationCommand('consent', ['approve', 'g1'], client)).text, 'approved');
    assert.equal((await runDelegationCommand('consent', ['deny', 'g1'], client)).text, 'denied');
    assert.deepEqual(sent, [['approve', 'g1', 'n1', 'h1'], ['deny', 'g1', 'n1']]);
    // A gate listed without its binding is someone else's: nothing is sent.
    const other = await runDelegationCommand('consent', ['approve', 'g9'], client);
    assert.equal(other.error, true);
    assert.equal(sent.length, 2);
    assert.match((await runDelegationCommand('consent', [], client)).text, /g1 {2}linear.create_issue · linear/);
  });
  await t.test('/enrol says where to sign in, once', async () => {
    const client = {
      delegationEnrol: async () => ({
        kind: 'task',
        task: { id: 'e', state: 'TASK_STATE_WORKING', signIn: { idp: 'okta', startUri: `${ORIGIN}/oauth2/start/e` } },
        data: undefined,
      }),
    };
    const out = await runDelegationCommand('enrol', ['okta'], client);
    assert.match(out.text, /open http:\/\/127\.0\.0\.1:8420\/oauth2\/start\/e in your browser to sign in at okta/);
    const refused = {
      delegationEnrol: async () => ({ kind: 'task', task: { id: 'e', state: 'TASK_STATE_FAILED', message: 'delegation refused (principal_not_delegable): …' }, data: undefined }),
    };
    assert.equal((await runDelegationCommand('enrol', ['okta'], refused)).error, true);
  });
});

test('the TUI signs in at the IdP with --login idp', () => {
  const a = parseArgs(['-e', ORIGIN, '--login', 'idp']);
  assert.equal(a.loginIdp, true);
  assert.deepEqual(resolveCredential(a, undefined), { kind: 'login', idp: true });
  assert.equal(parseArgs(['--login=idp']).loginIdp, true);
  assert.throws(() => parseArgs(['--login', 'idp', '--scope', 'operator']), /identity provider is a user session/);
  const bearerOnly = [{ method: 'bearer', scheme: 'bearer' }];
  assert.deepEqual(chooseSignIn(bearerOnly, false, FLOW), { kind: 'idp', flow: FLOW }, 'the one way in');
  assert.deepEqual(chooseSignIn(bearerOnly, false, FLOW, true), { kind: 'idp', flow: FLOW });
  assert.equal(chooseSignIn(bearerOnly, false, undefined, true).kind, 'refuse');
  // Asked for the device grant, a daemon without one is refused, not switched.
  assert.equal(chooseSignIn(bearerOnly, true, FLOW).kind, 'refuse');
});
