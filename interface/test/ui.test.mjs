// SPDX-License-Identifier: AGPL-3.0-only
// The web UI's sign-in and credential rules, run for real: the page (and
// main.tsx, its entry) is bundled with esbuild, rendered by a minimal React
// renderer into plain objects, and pointed at an in-process A2A agent. The
// browser around it is a set of globals this file controls — location,
// history, sessionStorage, localStorage and fetch — so every request the page
// makes, and everything it stores, is on the record.
//
// What is held to account:
// - a credential is sent only to the endpoint it was issued for;
// - a browser never connects without a session of its own;
// - the launch code leaves the URL before any request and is spent once,
//   only with the bootstrap endpoint;
// - tokens live in sessionStorage and nowhere else — never localStorage,
//   never a URL.
import test from 'node:test';
import assert from 'node:assert/strict';
import React from 'react';
import createReconciler from 'react-reconciler';
import { DefaultEventPriority, LegacyRoot, NoEventPriority } from 'react-reconciler/constants.js';
import { build } from 'esbuild';
import { mkdirSync, mkdtempSync, rmSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';
import { CREDENTIAL_KEY, ENDPOINT_KEY, LAUNCH_GRANT_TYPE, endpointKey, join as joinUrl, originOf } from '../dist/client/index.js';
import { startFakeA2a } from './fake-a2a.mjs';

const root = join(dirname(fileURLToPath(import.meta.url)), '..');
const UI = 'http://127.0.0.1:4173/';
const LAUNCH_CODE = `agentd_lc_${'c'.repeat(64)}`;

// ---- the page, bundled ------------------------------------------------------

// Inside dist/ (not tmpdir) so the bundles' bare `react` imports resolve to
// the same React this renderer uses. react-dom/client is replaced for main.tsx:
// its createRoot hands the element it was asked to render to the test.
const out = (() => {
  mkdirSync(join(root, 'dist'), { recursive: true });
  return mkdtempSync(join(root, 'dist', 'ui-test-'));
})();
test.after(() => rmSync(out, { recursive: true, force: true }));
await build({
  absWorkingDir: root,
  entryPoints: { app: 'src/ui/app.tsx', main: 'src/ui/main.tsx' },
  outdir: out,
  outExtension: { '.js': '.mjs' },
  bundle: true,
  format: 'esm',
  platform: 'browser',
  jsx: 'automatic',
  external: ['react', 'react/*'],
  logLevel: 'silent',
  plugins: [
    {
      name: 'capture-root',
      setup(b) {
        b.onResolve({ filter: /^react-dom\/client$/ }, () => ({ path: 'react-dom/client', namespace: 'capture' }));
        b.onLoad({ filter: /.*/, namespace: 'capture' }, () => ({
          contents: 'export function createRoot() { return { render(el) { globalThis.__uiRendered = el; } }; }',
          loader: 'js',
        }));
      },
    },
  ],
});
const ui = await import(pathToFileURL(join(out, 'app.mjs')).href);
let mainRuns = 0;
/** Run main.tsx once, afresh (a new module URL re-evaluates it); resolves the element it rendered. */
async function runMain() {
  globalThis.__uiRendered = undefined;
  await import(`${pathToFileURL(join(out, 'main.mjs')).href}?run=${++mainRuns}`);
  await until(() => globalThis.__uiRendered !== undefined, 'main.tsx rendered the app');
  return globalThis.__uiRendered;
}

// ---- a renderer into plain objects --------------------------------------------

let priority = NoEventPriority;
const detach = (parent, child) => {
  const i = parent.children.indexOf(child);
  if (i >= 0) parent.children.splice(i, 1);
  child.parent = null;
};
const append = (parent, child) => {
  if (child.parent) detach(child.parent, child);
  child.parent = parent;
  parent.children.push(child);
};
const insertBefore = (parent, child, before) => {
  if (child.parent) detach(child.parent, child);
  child.parent = parent;
  const i = parent.children.indexOf(before);
  parent.children.splice(i < 0 ? parent.children.length : i, 0, child);
};
const reconciler = createReconciler({
  getRootHostContext: () => ({}),
  getChildHostContext: (c) => c,
  prepareForCommit: () => null,
  resetAfterCommit() {},
  preparePortalMount: () => null,
  clearContainer(c) {
    c.children = [];
  },
  shouldSetTextContent: () => false,
  // What a ref sees: the node, with the one element method the page calls.
  createInstance: (type, props) => ({ type, props, children: [], parent: null, scrollTo() {} }),
  createTextInstance: (text) => ({ text, parent: null }),
  appendInitialChild: append,
  appendChild: append,
  appendChildToContainer: append,
  insertBefore,
  insertInContainerBefore: insertBefore,
  removeChild: detach,
  removeChildFromContainer: detach,
  finalizeInitialChildren: () => false,
  commitUpdate(node, _type, _old, next) {
    node.props = next;
  },
  commitTextUpdate(node, _old, text) {
    node.text = text;
  },
  resetTextContent() {},
  hideInstance() {},
  unhideInstance() {},
  hideTextInstance() {},
  unhideTextInstance() {},
  getPublicInstance: (i) => i,
  isPrimaryRenderer: true,
  supportsMutation: true,
  supportsPersistence: false,
  supportsHydration: false,
  supportsMicrotasks: true,
  scheduleMicrotask: queueMicrotask,
  scheduleTimeout: setTimeout,
  cancelTimeout: clearTimeout,
  noTimeout: -1,
  beforeActiveInstanceBlur() {},
  afterActiveInstanceBlur() {},
  detachDeletedInstance() {},
  getInstanceFromNode: () => null,
  prepareScopeUpdate() {},
  getInstanceFromScope: () => null,
  setCurrentUpdatePriority(p) {
    priority = p;
  },
  getCurrentUpdatePriority: () => priority,
  resolveUpdatePriority: () => (priority !== NoEventPriority ? priority : DefaultEventPriority),
  maySuspendCommit: () => false,
  NotPendingTransition: undefined,
  HostTransitionContext: React.createContext(null),
  resetFormInstance() {},
  requestPostPaintCallback() {},
  shouldAttemptEagerTransition: () => false,
  trackSchedulerEvent() {},
  resolveEventType: () => null,
  resolveEventTimeStamp: () => -1.1,
  preloadInstance: () => true,
  startSuspendingCommit() {},
  suspendInstance() {},
  waitForCommitToBeReady: () => null,
  rendererPackageName: 'agentd-ui-test',
  rendererVersion: React.version,
});

function textOf(n) {
  return n.text !== undefined ? n.text : n.children.map(textOf).join('');
}

function findAll(n, pred, acc = []) {
  if (n.type !== undefined && pred(n)) acc.push(n);
  for (const c of n.children ?? []) findAll(c, pred, acc);
  return acc;
}

/** Render `element`; the handle reads the tree and drives it like a person would. */
function render(element, t) {
  const container = { children: [] };
  const errors = [];
  const fiberRoot = reconciler.createContainer(container, LegacyRoot, null, false, null, 'ui', (e) => errors.push(e), (e) => errors.push(e), () => {}, () => {}, null);
  reconciler.updateContainerSync(element, fiberRoot, null, () => {});
  reconciler.flushSyncWork();
  const h = {
    errors,
    text: () => textOf(container),
    find: (pred) => findAll(container, pred)[0],
    all: (pred) => findAll(container, pred),
    connected: () => findAll(container, (n) => n.props.className === 'statusbar').length > 0,
    onConnect: () => findAll(container, (n) => n.props.className === 'connect').length > 0,
    click(label) {
      const b = h.find((n) => n.type === 'button' && textOf(n).trim() === label);
      assert.ok(b, `a "${label}" button (screen: ${h.text().slice(0, 300)})`);
      b.props.onClick({ preventDefault() {} });
    },
    /** Type into the Connect form's endpoint and submit it. */
    async connectTo(endpoint) {
      const form = () => h.find((n) => n.type === 'form' && findAll(n, (c) => c.type === 'input').length > 0);
      findAll(form(), (c) => c.type === 'input')[0].props.onChange({ target: { value: endpoint } });
      // The submit handler closes over the typed value: let that render land first.
      await until(() => findAll(form(), (c) => c.type === 'input')[0].props.value === endpoint, 'the typed endpoint');
      form().props.onSubmit({ preventDefault() {} });
    },
    /** Type a line into the composer and send it. */
    async say(line) {
      const area = () => h.find((n) => n.type === 'textarea');
      assert.ok(area(), 'the composer');
      area().props.onChange({ target: { value: line } });
      await until(() => area().props.value === line, 'the typed line');
      h.find((n) => n.type === 'form' && n.props.className === 'composer').props.onSubmit({ preventDefault() {} });
    },
    unmount() {
      reconciler.updateContainerSync(null, fiberRoot, null, () => {});
      reconciler.flushSyncWork();
    },
  };
  t?.after(() => h.unmount());
  return h;
}

async function until(pred, what, ms = 4000) {
  const end = Date.now() + ms;
  while (!pred()) {
    if (Date.now() > end) throw new Error(`timed out waiting for: ${what}`);
    await new Promise((r) => setTimeout(r, 10));
  }
}
const settle = (ms = 150) => new Promise((r) => setTimeout(r, ms));

// ---- the browser around it ----------------------------------------------------

function webStorage(seed = {}) {
  const m = new Map(Object.entries(seed));
  return {
    getItem: (k) => (m.has(k) ? m.get(k) : null),
    setItem: (k, v) => void m.set(k, String(v)),
    removeItem: (k) => void m.delete(k),
    clear: () => m.clear(),
    key: (i) => [...m.keys()][i] ?? null,
    get length() {
      return m.size;
    },
    entries: () => [...m.entries()],
  };
}

const realFetch = globalThis.fetch;

/**
 * A browser tab at `href`. `routes(call)` answers a request with `[status,
 * json]` (or a promise of it), or `undefined` to let it through to the real
 * network — the fake agents. Every request is recorded in `calls`, and
 * `events` orders history rewrites and requests on one line.
 */
function browser(href, { session = {}, local = {}, routes = () => undefined } = {}) {
  let url = new URL(href);
  const events = [];
  const calls = [];
  globalThis.location = {
    get href() {
      return url.href;
    },
    get origin() {
      return url.origin;
    },
    get protocol() {
      return url.protocol;
    },
    get hostname() {
      return url.hostname;
    },
    get pathname() {
      return url.pathname;
    },
    get search() {
      return url.search;
    },
    get hash() {
      return url.hash;
    },
  };
  globalThis.history = {
    replaceState(_state, _title, next) {
      events.push('replaceState');
      url = new URL(next, url);
    },
  };
  globalThis.document = { getElementById: () => ({}) };
  const tab = {
    session: webStorage(session),
    local: webStorage(local),
    events,
    calls,
    /** The requests to `path` (on any origin). */
    to: (path, method) => calls.filter((c) => c.path === path && (method === undefined || c.method === method)),
  };
  globalThis.sessionStorage = tab.session;
  globalThis.localStorage = tab.local;
  globalThis.fetch = async (input, init = {}) => {
    const u = new URL(typeof input === 'string' ? input : input.url);
    const call = {
      url: u.href,
      origin: u.origin,
      path: u.pathname,
      method: (init.method ?? 'GET').toUpperCase(),
      headers: Object.fromEntries(new Headers(init.headers ?? {}).entries()),
      body: typeof init.body === 'string' ? init.body : undefined,
    };
    call.form = call.body !== undefined && /x-www-form-urlencoded/.test(call.headers['content-type'] ?? '') ? new URLSearchParams(call.body) : undefined;
    calls.push(call);
    events.push(`fetch ${call.method} ${call.path}`);
    const answer = await routes(call);
    if (answer === undefined) return realFetch(input, init);
    const [status, json] = answer;
    return new Response(json === undefined ? null : JSON.stringify(json), {
      status,
      headers: { 'content-type': 'application/json', 'cache-control': 'no-store' },
    });
  };
  return tab;
}
test.after(() => {
  globalThis.fetch = realFetch;
});

/** A fake agent whose card declares the device grant on its own origin. */
async function deviceAgent(t) {
  const fake = await startFakeA2a();
  t.after(() => fake.close());
  fake.card.securitySchemes = {
    device_code: {
      oauth2SecurityScheme: {
        flows: {
          deviceCode: {
            deviceAuthorizationUrl: `${fake.origin}/oauth2/device_authorization`,
            tokenUrl: `${fake.origin}/oauth2/token`,
            scopes: { user: 'act as yourself' },
          },
        },
      },
    },
  };
  fake.card.securityRequirements = [{ schemes: { device_code: { list: [] } } }];
  return fake;
}

async function plainAgent(t) {
  const fake = await startFakeA2a();
  t.after(() => fake.close());
  return fake;
}

/** Is `call` sending anything that looks like a credential of ours? */
const carries = (call, secret) => call.url.includes(secret) || (call.body ?? '').includes(secret) || JSON.stringify(call.headers).includes(secret);

// ---- the tests ------------------------------------------------------------------

test('credentials bind to their endpoint', async (t) => {
  const a = await plainAgent(t);
  const b = await plainAgent(t);
  const tokenA = `agentd_at_${'a'.repeat(64)}`;
  a.bearer = tokenA;
  const seeded = { [CREDENTIAL_KEY]: JSON.stringify({ [endpointKey(a.url)]: { token: tokenA, scope: 'operator' } }) };

  // A link naming another endpoint pre-fills the form and sends nothing: not
  // the session this tab holds for the bootstrap endpoint, not even a card GET.
  let tab = browser(`${UI}?endpoint=${encodeURIComponent(b.url)}`, { session: seeded });
  let page = render(React.createElement(ui.App, { bootstrap: { endpoint: a.url } }), t);
  await settle();
  assert.ok(page.onConnect(), 'the Connect screen');
  assert.equal(page.find((n) => n.type === 'input').props.value, b.url, 'pre-filled with the linked endpoint');
  assert.equal(tab.calls.length, 0, `no request before the person connects: ${JSON.stringify(tab.calls.map((c) => c.url))}`);
  // Connecting to it signs in there; A's session goes nowhere near it.
  await page.connectTo(b.url);
  await until(() => /a2a\.device_grant/.test(page.text()), 'the no-sign-in notice for B');
  assert.ok(tab.calls.length > 0);
  for (const c of tab.calls) {
    assert.equal(c.headers.authorization, undefined, `no Authorization on ${c.method} ${c.url}`);
    assert.ok(!carries(c, tokenA), `A's token never travels: ${c.url}`);
  }
  assert.equal(b.rpcCalls().length, 0, 'and B is never called without a session of its own');
  page.unmount();

  // The bootstrap endpoint itself: the held session is exactly its key's,
  // and every JSON-RPC call to A carries it.
  tab = browser(UI, { session: seeded });
  page = render(React.createElement(ui.App, { bootstrap: { endpoint: a.url } }), t);
  await until(() => page.connected() && a.rpcCalls().length > 0, 'connected to A');
  for (const c of a.rpcCalls()) assert.equal(c.headers.authorization, `Bearer ${tokenA}`);
  assert.equal(tab.to('/oauth2/launch_authorization').length, 0, 'a held session needs no new sign-in');
  assert.deepEqual(Object.keys(JSON.parse(tab.session.getItem(CREDENTIAL_KEY))), [endpointKey(a.url)]);
  assert.ok(tab.calls.every((c) => c.origin === a.origin), 'and nothing goes to B');
});

test('device sign-in renders the user code and polls', async (t) => {
  const fake = await deviceAgent(t);
  const token = `agentd_at_${'d'.repeat(64)}`;
  fake.bearer = token;
  let approved = false;
  let polls = 0;
  const tab = browser(UI, {
    local: { [ENDPOINT_KEY]: JSON.stringify({ endpoint: fake.url }) },
    routes: (c) => {
      if (c.path === '/oauth2/device_authorization') {
        return [200, { device_code: 'dc-1', user_code: 'BCDF-GHJK', verification_uri: `${fake.origin}/device`, expires_in: 600, interval: 0.02 }];
      }
      if (c.path === '/oauth2/token') {
        polls++;
        return approved ? [200, { access_token: token, token_type: 'Bearer', expires_in: 3600, scope: 'user' }] : [400, { error: 'authorization_pending' }];
      }
      return undefined;
    },
  });
  // A hosted page (no bootstrap): the remembered endpoint's card offers the device grant.
  const page = render(React.createElement(ui.App, { bootstrap: {} }), t);
  await until(() => page.find((n) => n.type === 'button' && textOf(n) === 'sign in'), 'the sign-in offer');
  assert.equal(tab.to('/oauth2/launch_authorization').length, 0, 'no launcher is asked for a page it did not serve');
  page.click('sign in');
  await until(() => page.text().includes('BCDF-GHJK'), 'the user code');
  const link = page.find((n) => n.type === 'a');
  assert.equal(link.props.href, `${fake.origin}/device`);
  assert.match(page.text(), /\/approve BCDF-GHJK <name>/, 'and how an operator approves it');
  const [asked] = tab.to('/oauth2/device_authorization', 'POST');
  assert.equal(asked.form.get('client_id'), 'agentd-ui');
  await until(() => polls >= 2, 'polling while nobody approved');
  assert.ok(!page.connected(), 'not connected while pending');
  approved = true;
  await until(() => page.connected() && fake.rpcCalls().length > 0, 'connected once approved');
  for (const c of fake.rpcCalls()) assert.equal(c.headers.authorization, `Bearer ${token}`);
  const held = JSON.parse(tab.session.getItem(CREDENTIAL_KEY));
  assert.deepEqual(Object.keys(held), [endpointKey(fake.url)]);
  assert.equal(held[endpointKey(fake.url)].token, token);
});

test('a launch code is exchanged once with the bootstrap endpoint', async (t) => {
  const fake = await plainAgent(t);
  const session = `agentd_at_${'1'.repeat(64)}`;
  fake.bearer = session;
  const launchRoutes = (answer) => (c) => {
    if (c.path === '/bootstrap.json') return [200, { endpoint: fake.url }];
    if (c.path === '/oauth2/token' && c.form?.get('code') !== undefined && c.form?.get('code') !== null) return answer;
    if (c.path.startsWith('/oauth2/')) return [404, { error: 'not_found' }];
    return undefined;
  };

  // The code leaves the URL before the first request, and is spent exactly
  // once, at the bootstrap endpoint's origin.
  let tab = browser(`${UI}#launch=${LAUNCH_CODE}`, {
    routes: launchRoutes([200, { access_token: session, token_type: 'Bearer', scope: 'operator', expires_in: 28800 }]),
  });
  let el = await runMain();
  assert.equal(tab.events[0], 'replaceState', `the fragment is stripped first: ${tab.events.join(', ')}`);
  assert.equal(tab.events[1], 'fetch GET /bootstrap.json', 'and only then is the bootstrap read');
  assert.equal(location.hash, '');
  assert.ok(!location.href.includes(LAUNCH_CODE));
  let page = render(el, t);
  await until(() => page.connected() && fake.rpcCalls().length > 0, 'connected with the launch session');
  const posts = tab.calls.filter((c) => carries(c, LAUNCH_CODE));
  assert.equal(posts.length, 1, 'the code is presented once');
  assert.equal(posts[0].url, joinUrl(originOf(fake.url), '/oauth2/token'));
  assert.equal(posts[0].method, 'POST');
  assert.equal(posts[0].form.get('grant_type'), LAUNCH_GRANT_TYPE);
  assert.equal(posts[0].form.get('code'), LAUNCH_CODE);
  assert.equal(posts[0].form.get('client_id'), 'agentd-ui');
  for (const c of fake.rpcCalls()) assert.equal(c.headers.authorization, `Bearer ${session}`);
  assert.equal(JSON.parse(tab.session.getItem(CREDENTIAL_KEY))[endpointKey(fake.url)].token, session);
  page.unmount();

  // A link naming another endpoint: the code is sent nowhere — not at
  // startup, and not when the person connects there.
  const other = await plainAgent(t);
  tab = browser(`${UI}?endpoint=${encodeURIComponent(other.url)}#launch=${LAUNCH_CODE}`, {
    routes: launchRoutes([200, { access_token: session, token_type: 'Bearer', scope: 'operator' }]),
  });
  el = await runMain();
  page = render(el, t);
  await settle();
  await page.connectTo(other.url);
  await until(() => /a2a\.device_grant/.test(page.text()), 'the notice for the other endpoint');
  await page.connectTo(fake.url);
  await until(() => /a2a\.device_grant/.test(page.text()) && tab.to('/.well-known/agent-card.json').length >= 2, 'the bootstrap endpoint, without the code');
  assert.equal(tab.calls.filter((c) => carries(c, LAUNCH_CODE)).length, 0, `the code went nowhere: ${tab.calls.map((c) => c.url).join(' ')}`);
  assert.equal(location.hash, '');
  page.unmount();

  // A refused code says so, and the page goes on to the terminal sign-in.
  tab = browser(`${UI}#launch=${LAUNCH_CODE}`, { routes: launchRoutes([400, { error: 'invalid_grant' }]) });
  el = await runMain();
  page = render(el, t);
  await until(() => page.text().includes(ui.LAUNCH_LINK_REFUSED), 'the refused-link notice');
  assert.equal(ui.LAUNCH_LINK_REFUSED, 'this sign-in link was already used or has expired');
  await until(() => tab.to('/oauth2/launch_authorization').length === 1, 'the terminal sign-in is tried next');
  assert.equal(tab.calls.filter((c) => carries(c, LAUNCH_CODE)).length, 1, 'still presented only once');
});

test('tokens never touch localStorage or a URL', async (t) => {
  const assertClean = (tab) => {
    for (const [k, v] of tab.local.entries()) {
      assert.ok(['agentd-ui', 'agentd.layout'].includes(k), `localStorage key ${k}`);
      assert.ok(!v.includes('agentd_at_') && !v.includes('agentd_lc_'), `no credential in localStorage[${k}]: ${v}`);
    }
    for (const part of [location.search, location.hash]) assert.ok(!/agentd_(at|lc)_|bearer/.test(part), `no credential in ${part}`);
  };

  // A device sign-in, then a layout change.
  const dev = await deviceAgent(t);
  const devToken = `agentd_at_${'e'.repeat(64)}`;
  let tab = browser(UI, {
    local: { [ENDPOINT_KEY]: JSON.stringify({ endpoint: dev.url }) },
    routes: (c) => {
      if (c.path === '/oauth2/device_authorization') {
        return [200, { device_code: 'dc', user_code: 'LMNP-QRST', verification_uri: `${dev.origin}/device`, expires_in: 600, interval: 0.02 }];
      }
      if (c.path === '/oauth2/token') return [200, { access_token: devToken, token_type: 'Bearer', expires_in: 3600, scope: 'user' }];
      if (c.path === '/bootstrap.json') return [404, undefined];
      return undefined;
    },
  });
  let page = render(await runMain(), t);
  await until(() => page.find((n) => n.type === 'button' && textOf(n) === 'sign in'), 'the sign-in offer');
  page.click('sign in');
  await until(() => page.connected(), 'signed in');
  await page.say('/layout top name,version');
  await until(() => tab.local.getItem('agentd.layout') !== null, 'the layout is remembered');
  assert.deepEqual(JSON.parse(tab.local.getItem('agentd.layout')).top, ['name', 'version']);
  assert.ok(tab.session.getItem(CREDENTIAL_KEY).includes(devToken), 'the session is in sessionStorage');
  assertClean(tab);
  page.unmount();

  // A launch exchange.
  const fake = await plainAgent(t);
  const token = `agentd_at_${'2'.repeat(64)}`;
  tab = browser(`${UI}#launch=${LAUNCH_CODE}`, {
    routes: (c) => {
      if (c.path === '/bootstrap.json') return [200, { endpoint: fake.url }];
      if (c.path === '/oauth2/token') return [200, { access_token: token, token_type: 'Bearer', scope: 'operator', expires_in: 28800 }];
      return undefined;
    },
  });
  page = render(await runMain(), t);
  await until(() => page.connected(), 'signed in by the launch code');
  assert.deepEqual(JSON.parse(tab.local.getItem(ENDPOINT_KEY)), { endpoint: fake.url }, 'the endpoint alone is remembered');
  assert.ok(tab.session.getItem(CREDENTIAL_KEY).includes(token));
  assertClean(tab);
  // No request URL carries a credential either.
  for (const c of tab.calls) assert.ok(!/agentd_(at|lc)_/.test(c.url), `no credential in ${c.url}`);
});

test('no credential means sign-in, never an implicit connection', async (t) => {
  // Served by agentd-ui (a bootstrap endpoint on loopback), no launcher
  // running it, a card with no device grant: the page says how to get a
  // session, and calls nothing.
  for (const bootstrap of [true, false]) {
    const fake = await plainAgent(t);
    const tab = browser(UI, {
      local: bootstrap ? {} : { [ENDPOINT_KEY]: JSON.stringify({ endpoint: fake.url }) },
      routes: (c) => (c.path.startsWith('/oauth2/') ? [404, { error: 'not_found' }] : undefined),
    });
    const page = render(React.createElement(ui.App, { bootstrap: bootstrap ? { endpoint: fake.url } : {} }), t);
    await until(() => /a2a\.device_grant/.test(page.text()), 'the notice');
    assert.match(page.text(), /browser sessions only through a2a\.device_grant or the agentd ui launcher/);
    assert.ok(page.onConnect() && !page.connected());
    await settle();
    assert.equal(fake.rpcCalls().length, 0, 'no JSON-RPC request');
    assert.equal(tab.calls.filter((c) => c.method === 'POST' && c.path === '/').length, 0);
    assert.equal(tab.to('/oauth2/launch_authorization').length, bootstrap ? 1 : 0, 'the launcher is asked only for its own page');
    page.unmount();
  }
});

test('an expired session returns to Connect', async (t) => {
  const fake = await plainAgent(t);
  const token = `agentd_at_${'3'.repeat(64)}`;
  // Expired before the page loaded: dropped as it is found, never sent.
  let tab = browser(UI, {
    session: { [CREDENTIAL_KEY]: JSON.stringify({ [endpointKey(fake.url)]: { token, expiresAt: Date.now() - 1000 } }) },
    local: { [ENDPOINT_KEY]: JSON.stringify({ endpoint: fake.url }) },
  });
  let page = render(React.createElement(ui.App, { bootstrap: {} }), t);
  await until(() => page.onConnect() && /a2a\.device_grant/.test(page.text()), 'the Connect screen');
  assert.equal(tab.session.getItem(CREDENTIAL_KEY), null, 'the expired session is gone');
  assert.equal(fake.rpcCalls().length, 0);
  assert.ok(!tab.calls.some((c) => carries(c, token)));
  page.unmount();

  // Refused while connected (revoked, or the daemon restarted): the same.
  fake.bearer = 'something-else';
  tab = browser(UI, {
    session: { [CREDENTIAL_KEY]: JSON.stringify({ [endpointKey(fake.url)]: { token, expiresAt: Date.now() + 3_600_000 } }) },
    local: { [ENDPOINT_KEY]: JSON.stringify({ endpoint: fake.url }) },
  });
  page = render(React.createElement(ui.App, { bootstrap: {} }), t);
  await until(() => page.onConnect() && page.text().includes(ui.SESSION_ENDED), 'back on Connect, saying why');
  assert.equal(tab.session.getItem(CREDENTIAL_KEY), null, 'the refused session is dropped');
});

test('terminal sign-in when there is no code', async (t) => {
  const fake = await plainAgent(t);
  const token = `agentd_at_${'4'.repeat(64)}`;
  fake.bearer = token;
  let seenFirst;
  const firstShown = new Promise((r) => (seenFirst = r));
  const requests = [
    { request_code: 'agentd_lr_one', user_code: 'WXYZ-BCDF', expires_in: 120, interval: 0.02 },
    { request_code: 'agentd_lr_two', user_code: 'QRST-VWXZ', expires_in: 120, interval: 0.02 },
  ];
  let secondPolls = 0;
  const tab = browser(UI, {
    routes: async (c) => {
      if (c.path === '/oauth2/launch_authorization') {
        const next = requests.shift();
        return next ? [200, next] : [500, { error: 'server_error' }];
      }
      if (c.path === '/oauth2/token') {
        assert.equal(c.form.get('grant_type'), LAUNCH_GRANT_TYPE);
        assert.equal(c.form.get('client_id'), 'agentd-ui');
        if (c.form.get('request_code') === 'agentd_lr_one') {
          // Nobody typed the first code: it expires once the page has shown it.
          await firstShown;
          return [400, { error: 'expired_token' }];
        }
        secondPolls++;
        return secondPolls < 2 ? [400, { error: 'authorization_pending' }] : [200, { access_token: token, token_type: 'Bearer', scope: 'operator', expires_in: 28800 }];
      }
      return undefined;
    },
  });
  const page = render(React.createElement(ui.App, { bootstrap: { endpoint: fake.url } }), t);
  await until(() => /Type this code in the terminal that runs agentd ui: WXYZ-BCDF/.test(page.text()), 'the first code');
  seenFirst();
  await until(() => /Type this code in the terminal that runs agentd ui: QRST-VWXZ/.test(page.text()), 'a fresh code after expiry');
  await until(() => page.connected() && fake.rpcCalls().length > 0, 'the operator session');
  assert.equal(tab.to('/oauth2/launch_authorization', 'POST').length, 2);
  assert.equal(tab.to('/oauth2/launch_authorization')[0].url, joinUrl(originOf(fake.url), '/oauth2/launch_authorization'));
  for (const c of fake.rpcCalls()) assert.equal(c.headers.authorization, `Bearer ${token}`);
  assert.equal(JSON.parse(tab.session.getItem(CREDENTIAL_KEY))[endpointKey(fake.url)].token, token);
  for (const [, v] of tab.local.entries()) assert.ok(!v.includes(token));
  page.unmount();

  // No launcher slot (404): device sign-in when the card offers it, and no poll.
  const dev = await deviceAgent(t);
  const tab2 = browser(UI, { routes: (c) => (c.path === '/oauth2/launch_authorization' ? [404, { error: 'not_found' }] : undefined) });
  const page2 = render(React.createElement(ui.App, { bootstrap: { endpoint: dev.url } }), t);
  await until(() => page2.find((n) => n.type === 'button' && textOf(n) === 'sign in'), 'the device sign-in offer');
  await settle();
  assert.equal(tab2.to('/oauth2/token').length, 0, 'nothing is polled');
  assert.doesNotMatch(page2.text(), /Type this code/);
});

test('/disconnect revokes the session, then forgets it', async (t) => {
  const fake = await plainAgent(t);
  const token = `agentd_at_${'5'.repeat(64)}`;
  fake.bearer = token;
  let heldAtRevoke;
  const tab = browser(UI, {
    session: { [CREDENTIAL_KEY]: JSON.stringify({ [endpointKey(fake.url)]: { token, scope: 'operator' } }) },
    local: { [ENDPOINT_KEY]: JSON.stringify({ endpoint: fake.url }) },
    routes: (c) => {
      if (c.path === '/oauth2/revoke') {
        heldAtRevoke = tab.session.getItem(CREDENTIAL_KEY);
        return [200, undefined];
      }
      return undefined;
    },
  });
  const page = render(React.createElement(ui.App, { bootstrap: {} }), t);
  await until(() => page.connected(), 'connected');
  await page.say('/disconnect');
  await until(() => page.text().includes('signed out; the session is revoked'), 'signed out');
  const [rev] = tab.to('/oauth2/revoke', 'POST');
  assert.equal(rev.url, joinUrl(originOf(fake.url), '/oauth2/revoke'));
  assert.equal(rev.form.get('token'), token);
  assert.ok(heldAtRevoke?.includes(token), 'revoked first…');
  assert.equal(tab.session.getItem(CREDENTIAL_KEY), null, '…then cleared');
  assert.ok(page.onConnect());
});

test('slash commands the card does not back are not sent, and the debug tab needs introspection', async (t) => {
  const fake = await plainAgent(t);
  const token = `agentd_at_${'6'.repeat(64)}`;
  fake.bearer = token;
  browser(UI, {
    session: { [CREDENTIAL_KEY]: JSON.stringify({ [endpointKey(fake.url)]: { token } }) },
    local: { [ENDPOINT_KEY]: JSON.stringify({ endpoint: fake.url }) },
  });
  const page = render(React.createElement(ui.App, { bootstrap: {} }), t);
  await until(() => page.connected() && fake.rpcCalls().length > 0, 'connected');
  const before = fake.rpcCalls().length;
  await page.say('/drain');
  await until(() => /\/drain is not offered by this agent to you/.test(page.text()), 'the refusal');
  await page.say('/debug');
  await until(() => /debug is off on this daemon/.test(page.text()), 'debug is off');
  assert.equal(page.all((n) => n.type === 'button' && textOf(n) === 'debug').length, 0, 'no debug tab');
  // Polling may have moved on, but nothing but core reads went out.
  assert.ok(fake.rpcCalls().slice(before).every((c) => ['ListTasks', 'GetTask', 'SubscribeToTask'].includes(c.rpc)));
});
