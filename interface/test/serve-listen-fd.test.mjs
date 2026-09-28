// SPDX-License-Identifier: AGPL-3.0-only
// `agentd-ui --listen-fd N`: serve on a listening socket inherited from the
// launcher, and never bind a port of its own. `agentd ui` binds
// 127.0.0.1:<port> itself and passes it as fd 3; a server that ignored the
// flag would bind 4173 on its own (and die with EADDRINUSE, or serve a port
// someone else could have taken first).
//
// serve.mjs runs from a temporary copy beside a stub dist/web, so the test
// needs no web build and never touches the real dist/.
import test from 'node:test';
import assert from 'node:assert/strict';
import net from 'node:net';
import http from 'node:http';
import { spawn } from 'node:child_process';
import { copyFileSync, existsSync, mkdirSync, mkdtempSync, readdirSync, readFileSync, readlinkSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

const here = dirname(fileURLToPath(import.meta.url));

/** GET / with an explicit Host header; resolves {status, body}. */
function get(port, host) {
  return new Promise((resolve, reject) => {
    const req = http.request({ host: '127.0.0.1', port, path: '/', headers: { host } }, (res) => {
      let body = '';
      res.on('data', (c) => (body += c));
      res.on('end', () => resolve({ status: res.statusCode, body }));
    });
    req.on('error', reject);
    req.end();
  });
}

/** Ports a process holds in LISTEN state, from /proc (Linux only). */
function listeningPorts(pid) {
  const inodes = new Set();
  for (const fd of readdirSync(`/proc/${pid}/fd`)) {
    try {
      const m = /^socket:\[(\d+)\]$/.exec(readlinkSync(`/proc/${pid}/fd/${fd}`));
      if (m) inodes.add(m[1]);
    } catch {
      /* the fd closed while we looked */
    }
  }
  const ports = [];
  for (const table of ['/proc/net/tcp', '/proc/net/tcp6']) {
    if (!existsSync(table)) continue;
    for (const line of readFileSync(table, 'utf8').split('\n').slice(1)) {
      const f = line.trim().split(/\s+/);
      // local_address is HEX_IP:HEX_PORT; st 0A is LISTEN; f[9] is the inode.
      if (f.length > 9 && f[3] === '0A' && inodes.has(f[9])) ports.push(parseInt(f[1].split(':')[1], 16));
    }
  }
  return ports;
}

/**
 * A temporary copy of serve.mjs beside a stub dist/web. Every case runs from
 * one: serve.mjs also exits 2 when dist/ is not built, which is the state CI
 * tests in, so a refusal asserted against the real bin/ could pass for that
 * reason alone.
 */
function stubRoot(t) {
  const root = mkdtempSync(join(tmpdir(), 'agentd-ui-fd-'));
  t.after(() => rmSync(root, { recursive: true, force: true }));
  mkdirSync(join(root, 'bin'));
  mkdirSync(join(root, 'dist', 'web'), { recursive: true });
  copyFileSync(join(here, '..', 'bin', 'serve.mjs'), join(root, 'bin', 'serve.mjs'));
  writeFileSync(join(root, 'dist', 'web', 'index.html'), '<!doctype html><title>stub</title>');
  return root;
}

/**
 * Run the stub copy to exit; resolves {code, err}. `fd3` is inherited as fd 3.
 * A refusal is immediate, so a server still running after a few seconds is
 * one that served what it should have refused: killed, and reported as such.
 */
function runToExit(root, args, fd3) {
  return new Promise((resolve) => {
    const stdio = ['ignore', 'pipe', 'pipe'];
    if (fd3 !== undefined) stdio.push(fd3);
    const c = spawn(process.execPath, [join(root, 'bin', 'serve.mjs'), ...args], { stdio });
    let err = '';
    c.stderr.on('data', (d) => (err += d));
    const timer = setTimeout(() => {
      err += '\n(still serving after 5s; killed)';
      c.kill();
    }, 5000);
    c.on('exit', (code) => {
      clearTimeout(timer);
      resolve({ code, err });
    });
  });
}

test('--listen-fd serves on the inherited socket', async (t) => {
  const root = stubRoot(t);

  // The launcher's half: bind 127.0.0.1:0 and hand the socket over as fd 3.
  const listener = net.createServer();
  await new Promise((resolve) => listener.listen(0, '127.0.0.1', resolve));
  const port = listener.address().port;
  const child = spawn(process.execPath, [join(root, 'bin', 'serve.mjs'), '--endpoint', 'http://127.0.0.1:8420', '--listen-fd', '3'], {
    stdio: ['ignore', 'pipe', 'pipe', listener._handle.fd],
  });
  // The parent stops accepting; the child's copy keeps the socket listening.
  listener.close();
  t.after(() => child.kill());

  let out = '';
  let err = '';
  child.stderr.on('data', (c) => (err += c));
  await new Promise((resolve, reject) => {
    child.stdout.on('data', (c) => {
      out += c;
      if (out.includes('serving on')) resolve();
    });
    child.on('exit', (code) => reject(new Error(`serve.mjs exited ${code}: ${err}`)));
  });
  assert.match(out, new RegExp(`serving on http://127\\.0\\.0\\.1:${port}/`));

  // Answered on the inherited port, under either loopback name for it…
  const ok = await get(port, `127.0.0.1:${port}`);
  assert.equal(ok.status, 200);
  assert.match(ok.body, /stub/);
  assert.equal((await get(port, `localhost:${port}`)).status, 200);
  // …and refused under the default port it was never given, or a foreign name.
  assert.equal((await get(port, '127.0.0.1:4173')).status, 403);
  assert.equal((await get(port, 'evil.example')).status, 403);

  // It holds exactly one listening socket: the inherited one.
  if (process.platform === 'linux') assert.deepEqual(listeningPorts(child.pid), [port]);
});

test('--listen-fd refuses what it cannot serve on', async (t) => {
  const root = stubRoot(t);
  const run = (args, fd3) => runToExit(root, args, fd3);
  // A bare flag or a non-number never falls back to binding a port.
  let r = await run(['--listen-fd']);
  assert.equal(r.code, 2);
  assert.match(r.err, /--listen-fd takes a file descriptor number/);
  r = await run(['--listen-fd', 'three']);
  assert.equal(r.code, 2);
  assert.match(r.err, /--listen-fd takes a file descriptor number, got "three"/);
  r = await run(['--listen-fd', '3', '--port', '4173']);
  assert.equal(r.code, 2);
  assert.match(r.err, /exclusive/);

  // A socket bound to every address is not served: the Host check is no
  // access control, and the page and config.js would be on the network.
  const wide = net.createServer();
  await new Promise((resolve) => wide.listen(0, '0.0.0.0', resolve));
  t.after(() => wide.close());
  r = await run(['--endpoint', 'http://127.0.0.1:8420', '--listen-fd', '3'], wide._handle.fd);
  assert.equal(r.code, 2);
  assert.match(r.err, /bound to 0\.0\.0\.0, not loopback/);
});
