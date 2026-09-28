#!/usr/bin/env node
// SPDX-License-Identifier: AGPL-3.0-only
// `agentd-ui` — serve the built web UI locally and (optionally) open the
// browser. Zero dependencies: node:http + the static dist/. The page learns
// the daemon's endpoint from ./bootstrap.json, and nothing else: this server
// holds no credential of any kind, so there is none here for another page to
// read. A browser tab signs in on its own — the launch code `agentd ui` puts
// in the URL fragment, a sign-in approved at the launcher's terminal, or the
// device grant the daemon's card offers.
//
//   agentd-ui --endpoint http://127.0.0.1:8420 [--port 4173 | --listen-fd N] [--open]
//
// `--listen-fd N` serves on a listening socket inherited as fd N: `agentd ui`
// binds 127.0.0.1:<port> itself and hands it over, so no other local process
// can hold the port the browser is sent to.
//
// The daemon admits this page's origin only when it is listed in
// `a2a.cors.origins` — except the one origin `agentd ui` launched, which that
// launcher's daemon admits for as long as it runs.
import http from 'node:http';
import { readFile } from 'node:fs/promises';
import { existsSync } from 'node:fs';
import { spawn } from 'node:child_process';
import { dirname, join, normalize } from 'node:path';
import { fileURLToPath } from 'node:url';

const args = process.argv.slice(2);
const opt = (name, dflt) => {
  const i = args.indexOf(`--${name}`);
  if (i >= 0 && args[i + 1] && !args[i + 1].startsWith('--')) return args[i + 1];
  const eq = args.find((a) => a.startsWith(`--${name}=`));
  return eq ? eq.slice(name.length + 3) : dflt;
};
const refuse = (msg) => {
  process.stderr.write(`agentd-ui: ${msg}\n`);
  process.exit(2);
};
if (args.includes('-h') || args.includes('--help')) {
  process.stdout.write(
    'agentd-ui — serve the agentd web UI\n\n  agentd-ui --endpoint http://127.0.0.1:8420 [--port 4173 | --listen-fd N] [--open]\n  env: AGENTD_ENDPOINT\n',
  );
  process.exit(0);
}
// The v1.16 server read the operator's bearer from here and served it to any
// page that asked. Ignoring the variable would leave the operator believing
// the tab is signed in with it; refusing says where sign-in went instead.
if (process.env.AGENTD_BEARER !== undefined) {
  refuse('agentd-ui no longer reads AGENTD_BEARER: the page signs in with the device grant, or run `agentd ui` for a signed-in tab');
}
const endpoint = opt('endpoint', process.env.AGENTD_ENDPOINT ?? '');
const port = Number(opt('port', '4173'));
const listenFd = opt('listen-fd', args.includes('--listen-fd') ? '' : undefined);
const open = args.includes('--open');
if (listenFd !== undefined) {
  // A bare or malformed --listen-fd must not quietly fall back to binding a
  // port of our own: the launcher's socket would then sit unserved.
  if (!/^\d+$/.test(listenFd)) refuse(`--listen-fd takes a file descriptor number, got ${JSON.stringify(listenFd)}`);
  if (opt('port', undefined) !== undefined) refuse('--port and --listen-fd are exclusive: the inherited socket already has its port');
}

// The endpoint's origin is the one place besides this server the page may
// connect to (the CSP below), so it must be one: a value that is not an
// http(s) URL would otherwise surface as a page that cannot reach anything.
let endpointOrigin;
if (endpoint !== '') {
  try {
    const u = new URL(endpoint);
    if (u.protocol !== 'http:' && u.protocol !== 'https:') throw new Error('scheme');
    endpointOrigin = u.origin;
  } catch {
    refuse(`--endpoint ${JSON.stringify(endpoint)} is not an http:// or https:// URL`);
  }
}

const dist = join(dirname(fileURLToPath(import.meta.url)), '..', 'dist', 'web');
if (!existsSync(join(dist, 'index.html'))) refuse('dist/ is not built — run `npm run build`');

const types = {
  '.html': 'text/html; charset=utf-8',
  '.js': 'text/javascript; charset=utf-8',
  '.css': 'text/css; charset=utf-8',
  '.svg': 'image/svg+xml',
};

// Where the page may send requests: this server, and the daemon's origin —
// the launch exchange, the card and every JSON-RPC call go there. Without a
// configured endpoint the person types one, so the rule the client itself
// holds for anything carrying a credential applies instead: https anywhere,
// plain http on loopback only.
const connectSrc = endpointOrigin
  ? `'self' ${endpointOrigin}`
  : "'self' https: http://127.0.0.1:* http://localhost:* http://[::1]:*";

/**
 * Sent with every response, refusals included. CORP keeps another origin
 * from embedding what this server returns (`<script src>` reads a response
 * cross-origin without CORS); nosniff stops a response being run as a type it
 * is not; no-referrer keeps this page's URL out of every request it makes;
 * the CSP pins where the page loads code from and talks to, and that nothing
 * frames it.
 */
const SECURITY_HEADERS = Object.freeze({
  'cross-origin-resource-policy': 'same-origin',
  'x-content-type-options': 'nosniff',
  'referrer-policy': 'no-referrer',
  'content-security-policy': `default-src 'self'; connect-src ${connectSrc}; frame-ancestors 'none'; base-uri 'none'; form-action 'none'`,
});

// The port actually served, read from the socket once it listens: with
// --listen-fd nobody told this process the number, and it must not guess.
const servedPort = () => server.address()?.port;

// A request must name this server as it was reached — 127.0.0.1 or localhost
// on the served port. Anything else is a page that resolved its own name to
// 127.0.0.1 (DNS rebinding) and is reading this origin as if it were its own.
function hostAllowed(host) {
  const p = servedPort();
  return host === `127.0.0.1:${p}` || host === `localhost:${p}`;
}

/**
 * The bootstrap document is read by this page's own `fetch`, and by nothing
 * else: `Sec-Fetch-Site` (sent by every current browser) must say the request
 * came from this origin or from no page at all, and an `Origin`, when there is
 * one, must be this server's. It names only the endpoint, but which daemon a
 * person runs, on which port, is still not another site's business.
 */
function bootstrapAllowed(req) {
  const site = req.headers['sec-fetch-site'];
  if (site !== undefined && site !== 'same-origin' && site !== 'none') return false;
  const origin = req.headers.origin;
  return origin === undefined || origin === `http://${req.headers.host}`;
}

const reply = (res, status, headers, body) => {
  res.writeHead(status, { ...SECURITY_HEADERS, ...headers });
  res.end(body);
};

const server = http.createServer(async (req, res) => {
  if (!hostAllowed(req.headers.host)) return reply(res, 403, { 'content-type': 'text/plain' }, 'forbidden host');
  const path = (req.url ?? '/').split('?')[0];
  if (path === '/bootstrap.json') {
    if (!bootstrapAllowed(req)) return reply(res, 403, { 'content-type': 'text/plain' }, 'forbidden');
    const doc = endpoint ? { endpoint } : {};
    return reply(res, 200, { 'content-type': 'application/json', 'cache-control': 'no-store' }, JSON.stringify(doc));
  }
  const file = normalize(join(dist, path === '/' ? 'index.html' : path));
  if (!file.startsWith(dist)) return reply(res, 403, {}, '');
  try {
    const body = await readFile(file);
    const ext = file.slice(file.lastIndexOf('.'));
    reply(res, 200, { 'content-type': types[ext] ?? 'application/octet-stream' }, body);
  } catch {
    reply(res, 404, { 'content-type': 'text/plain' }, 'not found');
  }
});

const listening = () => {
  // An inherited socket is whatever the parent bound. The Host check above is
  // no access control — any peer can send that Host — so a socket on a
  // wildcard or public address would serve this page to the network. Only
  // 127.0.0.1 is served: it is the address printed below and the one the Host
  // check names.
  const bound = server.address()?.address;
  if (listenFd !== undefined && bound !== '127.0.0.1') {
    refuse(`the socket on fd ${listenFd} is bound to ${bound}, not loopback; refusing to serve it`);
  }
  const url = `http://127.0.0.1:${servedPort()}/`;
  process.stdout.write(`agentd-ui: serving on ${url}${endpoint ? ` → ${endpoint}` : ''}\n`);
  if (open) {
    // The plain URL: a signed-in tab is `agentd ui`'s to open, with a launch
    // code in the fragment; this server has no credential to put in one.
    const opener =
      process.platform === 'darwin' ? 'open' : process.platform === 'win32' ? 'start' : 'xdg-open';
    spawn(opener, [url], { stdio: 'ignore', detached: true, shell: process.platform === 'win32' }).unref();
  }
};
if (listenFd !== undefined) server.listen({ fd: Number(listenFd) }, listening);
else server.listen(port, '127.0.0.1', listening);
