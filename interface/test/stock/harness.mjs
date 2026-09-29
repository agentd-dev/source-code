// SPDX-License-Identifier: AGPL-3.0-only
// What the interop tests share: the official Python SDK's hello-world agent
// as a running peer, and the waits around it.
//
// The Python interpreter is AGENTD_STOCK_PYTHON (default `python3`) and must
// import a2a-sdk 1.1.5 and uvicorn (see requirements.txt beside this file).
// Where it cannot, the tests skip — except under CI, where a skip would turn
// the contract job green without running anything, so they run and fail.
import { spawn, spawnSync } from 'node:child_process';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';
import net from 'node:net';
import readline from 'node:readline';

const here = dirname(fileURLToPath(import.meta.url));

export const PYTHON = process.env.AGENTD_STOCK_PYTHON || 'python3';

/** Why the stock peer cannot run here, or '' when it can. */
export function stockUnavailable() {
  const r = spawnSync(PYTHON, ['-c', 'import a2a, uvicorn, starlette'], { encoding: 'utf8' });
  return r.status === 0 ? '' : `${PYTHON} cannot import a2a-sdk (pip install -r test/stock/requirements.txt): ${r.stderr ?? r.error}`;
}

/** A test's `skip` option: the reason, unless CI must run it anyway. */
export function skipUnless(reason) {
  return reason && !process.env.CI ? reason : false;
}

export function freePort() {
  return new Promise((resolve) => {
    const srv = net.createServer();
    srv.listen(0, '127.0.0.1', () => {
      const { port } = srv.address();
      srv.close(() => resolve(port));
    });
  });
}

export async function until(fn, ms = 10000, what = 'condition') {
  const deadline = Date.now() + ms;
  for (;;) {
    if (await fn()) return;
    if (Date.now() > deadline) throw new Error(`timeout waiting for ${what}`);
    await new Promise((r) => setTimeout(r, 50));
  }
}

/**
 * Start the official hello-world agent (hello_agent.py) on a free port.
 * Resolves once it listens, with its URL and `requests` — every HTTP request
 * it received, parsed from its log as they arrive — and `stop()`.
 */
export async function startHelloAgent() {
  const child = spawn(PYTHON, [join(here, 'hello_agent.py'), '0'], { stdio: ['ignore', 'pipe', 'pipe'] });
  const requests = [];
  let stderr = '';
  child.stderr.on('data', (d) => (stderr += d));
  const exited = new Promise((resolve) => child.once('exit', resolve));
  const url = await new Promise((resolve, reject) => {
    const lines = readline.createInterface({ input: child.stdout });
    lines.on('line', (line) => {
      const m = /^listening (\S+)$/.exec(line);
      if (m) return resolve(m[1]);
      try {
        requests.push(JSON.parse(line));
      } catch {
        // The sample's own prints are not requests.
      }
    });
    exited.then((code) => reject(new Error(`hello_agent.py exited (${code}) before listening:\n${stderr}`)));
    setTimeout(() => reject(new Error(`hello_agent.py never listened:\n${stderr}`)), 30000).unref();
  });
  return {
    url,
    requests,
    /** The JSON-RPC requests, in arrival order. */
    rpc: () => requests.filter((r) => r.method === 'POST' && r.body && typeof r.body === 'object'),
    stderr: () => stderr,
    async stop() {
      child.kill('SIGTERM');
      const t = setTimeout(() => child.kill('SIGKILL'), 3000);
      await exited;
      clearTimeout(t);
    },
  };
}
