// SPDX-License-Identifier: AGPL-3.0-only
// `agentd-ui` holds no credential of any kind. The v1.16 server read the
// operator's bearer from AGENTD_BEARER and served it as /config.js — a script
// any page could include — so the page could connect without asking. Now the
// page signs in on its own, and this server hands it only the endpoint, as a
// JSON document only the page's own origin may read.
//
// serve.mjs runs from a temporary copy beside a stub dist/web, so the test
// needs no web build and never touches the real dist/.
import test from 'node:test';
import assert from 'node:assert/strict';
import http from 'node:http';
import { spawn } from 'node:child_process';
import { copyFileSync, mkdirSync, mkdtempSync, readdirSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { dirname, join, relative } from 'node:path';
import { fileURLToPath } from 'node:url';

const here = dirname(fileURLToPath(import.meta.url));
const ENDPOINT = 'http://127.0.0.1:8420/';

/** A temporary copy of serve.mjs beside a stub dist/web; `cwd` is an empty directory to run it in. */
function stubRoot(t) {
  const root = mkdtempSync(join(tmpdir(), 'agentd-ui-serve-'));
  t.after(() => rmSync(root, { recursive: true, force: true }));
  mkdirSync(join(root, 'bin'));
  mkdirSync(join(root, 'cwd'));
  mkdirSync(join(root, 'dist', 'web'), { recursive: true });
  copyFileSync(join(here, '..', 'bin', 'serve.mjs'), join(root, 'bin', 'serve.mjs'));
  writeFileSync(join(root, 'dist', 'web', 'index.html'), '<!doctype html><title>stub</title>');
  return root;
}

/** Every file under `dir`, relative to it. */
function tree(dir) {
  const out = [];
  const walk = (d) => {
    for (const e of readdirSync(d, { withFileTypes: true })) {
      const p = join(d, e.name);
      if (e.isDirectory()) walk(p);
      else out.push(relative(dir, p));
    }
  };
  walk(dir);
  return out.sort();
}

/** The environment the server runs with: this one, minus anything that names a credential. */
function envWith(extra) {
  const env = { ...process.env, ...extra };
  if (!('AGENTD_BEARER' in extra)) delete env.AGENTD_BEARER;
  return env;
}

/**
 * Start the stub copy; resolves {child, port, out} once it serves, or
 * {code, err, out} when it exits first. A refusal is immediate, so a server
 * still starting after 5 s is reported as a failure to refuse.
 */
function start(t, root, args, env) {
  return new Promise((resolve) => {
    const child = spawn(process.execPath, [join(root, 'bin', 'serve.mjs'), ...args], {
      cwd: join(root, 'cwd'),
      env,
      stdio: ['ignore', 'pipe', 'pipe'],
    });
    // Whatever the test concludes, a server it started does not outlive it.
    t.after(() => child.kill());
    let out = '';
    let err = '';
    const timer = setTimeout(() => {
      child.kill();
      resolve({ code: 'timeout', err, out });
    }, 5000);
    child.stderr.on('data', (d) => (err += d));
    child.stdout.on('data', (d) => {
      out += d;
      const m = /serving on http:\/\/127\.0\.0\.1:(\d+)\//.exec(out);
      if (m) {
        clearTimeout(timer);
        resolve({ child, port: Number(m[1]), out });
      }
    });
    child.on('exit', (code) => {
      clearTimeout(timer);
      resolve({ code, err, out });
    });
  });
}

/** One GET with the given headers (Host defaults to the served name); resolves {status, headers, body}. */
function get(port, path, headers = {}) {
  return new Promise((resolve, reject) => {
    const req = http.request(
      { host: '127.0.0.1', port, path, headers: { host: `127.0.0.1:${port}`, ...headers } },
      (res) => {
        let body = '';
        res.on('data', (c) => (body += c));
        res.on('end', () => resolve({ status: res.statusCode, headers: res.headers, body }));
      },
    );
    req.on('error', reject);
    req.end();
  });
}

/** The security headers every response carries, refusals included. */
function assertHardened(r, what) {
  assert.equal(r.headers['cross-origin-resource-policy'], 'same-origin', `${what}: CORP`);
  assert.equal(r.headers['x-content-type-options'], 'nosniff', `${what}: nosniff`);
  assert.equal(r.headers['referrer-policy'], 'no-referrer', `${what}: Referrer-Policy`);
  const csp = r.headers['content-security-policy'] ?? '';
  const directives = new Map(
    csp
      .split(';')
      .map((d) => d.trim().split(/\s+/))
      .filter((d) => d[0])
      .map(([k, ...v]) => [k, v]),
  );
  // The page may talk to itself and to the daemon's origin — nothing else.
  assert.deepEqual(directives.get('connect-src'), ["'self'", 'http://127.0.0.1:8420'], `${what}: connect-src`);
  assert.deepEqual(directives.get('frame-ancestors'), ["'none'"], `${what}: frame-ancestors`);
  assert.deepEqual(directives.get('default-src'), ["'self'"], `${what}: default-src`);
  assert.deepEqual(directives.get('base-uri'), ["'none'"], `${what}: base-uri`);
  assert.deepEqual(directives.get('form-action'), ["'none'"], `${what}: form-action`);
}

test('agentd-ui never holds a credential', async (t) => {
  const root = stubRoot(t);
  const before = tree(root);

  // AGENTD_BEARER is refused by name, before anything is served: the operator
  // who still sets it learns where sign-in went instead of believing the tab
  // is signed in with it.
  const refused = await start(t, root, ['--endpoint', ENDPOINT, '--port', '0'], envWith({ AGENTD_BEARER: 'sekrit' }));
  assert.equal(refused.code, 2, `exits 2 (got ${refused.code}: ${refused.err})`);
  assert.match(refused.err, /agentd-ui no longer reads AGENTD_BEARER: the page signs in with the device grant, or run `agentd ui` for a signed-in tab/);
  assert.doesNotMatch(refused.out, /serving on/, 'nothing was served');
  assert.doesNotMatch(refused.err + refused.out, /sekrit/, 'and the value is not echoed');

  const s = await start(t, root, ['--endpoint', ENDPOINT, '--port', '0'], envWith({}));
  assert.ok(s.child, `serves without it (${s.code}: ${s.err})`);
  const { port } = s;
  const self = `http://127.0.0.1:${port}`;

  // The page, hardened.
  const page = await get(port, '/');
  assert.equal(page.status, 200);
  assertHardened(page, '/');

  // The removed script route is gone: nothing answers it but a 404.
  const cfg = await get(port, '/config.js');
  assert.equal(cfg.status, 404);
  assert.doesNotMatch(cfg.body, /AGENTD_DEFAULTS|bearer/i);
  assertHardened(cfg, '/config.js');

  // The bootstrap document is the endpoint and nothing else, never cached.
  for (const headers of [{}, { 'sec-fetch-site': 'same-origin' }, { 'sec-fetch-site': 'none' }, { 'sec-fetch-site': 'same-origin', origin: self }]) {
    const b = await get(port, '/bootstrap.json', headers);
    assert.equal(b.status, 200, JSON.stringify(headers));
    assert.deepEqual(JSON.parse(b.body), { endpoint: ENDPOINT });
    assert.match(b.headers['content-type'], /^application\/json/);
    assert.equal(b.headers['cache-control'], 'no-store');
    assertHardened(b, '/bootstrap.json');
  }
  // Another name for this server (DNS rebinding) is refused everywhere…
  for (const path of ['/', '/bootstrap.json']) {
    const r = await get(port, path, { host: 'evil.example' });
    assert.equal(r.status, 403, `Host evil.example on ${path}`);
    assertHardened(r, `403 ${path}`);
  }
  // …and the bootstrap is not another site's to read.
  for (const headers of [{ 'sec-fetch-site': 'cross-site' }, { 'sec-fetch-site': 'same-site' }, { origin: 'http://evil.example' }, { origin: 'null' }]) {
    const r = await get(port, '/bootstrap.json', headers);
    assert.equal(r.status, 403, JSON.stringify(headers));
    assert.doesNotMatch(r.body, /8420/);
    assertHardened(r, `403 bootstrap ${JSON.stringify(headers)}`);
  }

  // Nothing was written: no launch file, no config, nothing in its cwd.
  s.child.kill();
  await new Promise((r) => s.child.once('exit', r));
  assert.deepEqual(tree(root), before);
});

test('agentd-ui refuses an endpoint it could not point the page at', async (t) => {
  const root = stubRoot(t);
  for (const bad of ['not a url', 'ftp://127.0.0.1:8420/']) {
    const r = await start(t, root, ['--endpoint', bad, '--port', '0'], envWith({}));
    assert.equal(r.code, 2, `${bad}: ${r.err}`);
    assert.match(r.err, /is not an http:\/\/ or https:\/\/ URL/);
  }
});

test('without an endpoint the bootstrap is empty and the page may reach only what carries a credential safely', async (t) => {
  const root = stubRoot(t);
  const s = await start(t, root, ['--port', '0'], envWith({ AGENTD_ENDPOINT: '' }));
  assert.ok(s.child, `${s.code}: ${s.err}`);
  const b = await get(s.port, '/bootstrap.json');
  assert.deepEqual(JSON.parse(b.body), {});
  assert.match(b.headers['content-security-policy'], /connect-src 'self' https: http:\/\/127\.0\.0\.1:\* http:\/\/localhost:\* http:\/\/\[::1\]:\*;/);
});
