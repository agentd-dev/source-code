#!/usr/bin/env node
// SPDX-License-Identifier: AGPL-3.0-only
// `agentd-ui` — serve the built web UI locally and (optionally) open the
// browser. Zero dependencies: node:http + the static dist/. The daemon's
// endpoint is injected as ./config.js so the page connects without asking.
//
//   agentd-ui --endpoint http://127.0.0.1:8420 [--port 4173 | --listen-fd N] [--open]
//
// `--listen-fd N` serves on a listening socket inherited as fd N: `agentd ui`
// binds 127.0.0.1:<port> itself and hands it over, so no other local process
// can hold the port the browser is sent to.
//
// Served from loopback, the page's Origin is loopback — allowed by agentd's
// rebind guard with CORS out of the box. A HOSTED copy of dist/ instead needs
// its origin listed in the daemon's `interface.origins`.
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
if (args.includes('-h') || args.includes('--help')) {
  process.stdout.write(
    'agentd-ui — serve the agentd web UI\n\n  agentd-ui --endpoint http://127.0.0.1:8420 [--port 4173 | --listen-fd N] [--open]\n  env: AGENTD_ENDPOINT, AGENTD_BEARER\n',
  );
  process.exit(0);
}
const endpoint = opt('endpoint', process.env.AGENTD_ENDPOINT ?? '');
const bearer = process.env.AGENTD_BEARER ?? '';
const port = Number(opt('port', '4173'));
const listenFd = opt('listen-fd', args.includes('--listen-fd') ? '' : undefined);
const open = args.includes('--open');
if (listenFd !== undefined) {
  // A bare or malformed --listen-fd must not quietly fall back to binding a
  // port of our own: the launcher's socket would then sit unserved.
  if (!/^\d+$/.test(listenFd)) {
    process.stderr.write(`agentd-ui: --listen-fd takes a file descriptor number, got ${JSON.stringify(listenFd)}\n`);
    process.exit(2);
  }
  if (opt('port', undefined) !== undefined) {
    process.stderr.write('agentd-ui: --port and --listen-fd are exclusive: the inherited socket already has its port\n');
    process.exit(2);
  }
}

const dist = join(dirname(fileURLToPath(import.meta.url)), '..', 'dist', 'web');
if (!existsSync(join(dist, 'index.html'))) {
  process.stderr.write('agentd-ui: dist/ is not built — run `npm run build`\n');
  process.exit(2);
}

const types = {
  '.html': 'text/html; charset=utf-8',
  '.js': 'text/javascript; charset=utf-8',
  '.css': 'text/css; charset=utf-8',
  '.svg': 'image/svg+xml',
};

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

const server = http.createServer(async (req, res) => {
  if (!hostAllowed(req.headers.host)) {
    res.writeHead(403, { 'content-type': 'text/plain' }).end('forbidden host');
    return;
  }
  const path = (req.url ?? '/').split('?')[0];
  if (path === '/config.js') {
    const cfg = { endpoint: endpoint || undefined, bearer: bearer || undefined };
    res.writeHead(200, { 'content-type': types['.js'], 'cache-control': 'no-store' });
    res.end(`window.AGENTD_DEFAULTS=${JSON.stringify(cfg)};`);
    return;
  }
  const file = normalize(join(dist, path === '/' ? 'index.html' : path));
  if (!file.startsWith(dist)) {
    res.writeHead(403).end();
    return;
  }
  try {
    const body = await readFile(file);
    const ext = file.slice(file.lastIndexOf('.'));
    res.writeHead(200, { 'content-type': types[ext] ?? 'application/octet-stream' });
    res.end(body);
  } catch {
    res.writeHead(404, { 'content-type': 'text/plain' }).end('not found');
  }
});

const listening = () => {
  const url = `http://127.0.0.1:${servedPort()}/`;
  process.stdout.write(`agentd-ui: serving on ${url}${endpoint ? ` → ${endpoint}` : ''}\n`);
  if (open) {
    const opener =
      process.platform === 'darwin' ? 'open' : process.platform === 'win32' ? 'start' : 'xdg-open';
    spawn(opener, [url], { stdio: 'ignore', detached: true, shell: process.platform === 'win32' }).unref();
  }
};
if (listenFd !== undefined) server.listen({ fd: Number(listenFd) }, listening);
else server.listen(port, '127.0.0.1', listening);
