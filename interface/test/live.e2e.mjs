// SPDX-License-Identifier: Apache-2.0
// LIVE end-to-end: drive a real agentd daemon (the compiled Rust binary) with
// this TS client over the actual wire — bootstrap, feed convergence across two
// clients, debug reads, cancel. Gated: set AGENTD_E2E_BIN to the agentd binary
// (and it uses the built-in mock LLM), else the test is skipped.
//   AGENTD_E2E_BIN=../../target/debug/agentd node --test test/live.e2e.mjs
import test from 'node:test';
import assert from 'node:assert/strict';
import { spawn } from 'node:child_process';
import { mkdtempSync, writeFileSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import net from 'node:net';
import { AgentdClient, Mirror, Observation } from '../dist/client/index.js';

const BIN = process.env.AGENTD_E2E_BIN;

function freePort() {
  return new Promise((resolve) => {
    const srv = net.createServer();
    srv.listen(0, '127.0.0.1', () => {
      const { port } = srv.address();
      srv.close(() => resolve(port));
    });
  });
}

function waitConnectable(port, ms = 8000) {
  const deadline = Date.now() + ms;
  return new Promise((resolve, reject) => {
    const tryOnce = () => {
      const s = net.connect(port, '127.0.0.1');
      s.once('connect', () => {
        s.destroy();
        resolve();
      });
      s.once('error', () => {
        s.destroy();
        if (Date.now() > deadline) reject(new Error('listener never came up'));
        else setTimeout(tryOnce, 60);
      });
    };
    tryOnce();
  });
}

async function until(fn, ms = 10000, what = 'condition') {
  const deadline = Date.now() + ms;
  for (;;) {
    if (fn()) return;
    if (Date.now() > deadline) throw new Error(`timeout waiting for ${what}`);
    await new Promise((r) => setTimeout(r, 50));
  }
}

test('live: two TS clients converge on one real daemon', { skip: !BIN }, async () => {
  const dir = mkdtempSync(join(tmpdir(), 'agentd-live-'));
  const playbook = join(dir, 'playbook.json');
  writeFileSync(playbook, JSON.stringify({ turns: [{ content: 'Hello from the live daemon.' }, { content: 'Second reply.' }] }));
  const addrFile = join(dir, 'llm.addr');
  const llm = spawn(BIN, ['--internal-mock-llm', addrFile, `file:${playbook}`], { stdio: 'ignore' });
  const { readFileSync } = await import('node:fs');
  let llmAddr = '';
  await until(() => {
    try {
      llmAddr = readFileSync(addrFile, 'utf8').trim();
      return llmAddr.length > 0;
    } catch {
      return false;
    }
  }, 5000, 'mock llm addr');

  const port = await freePort();
  const cfg = join(dir, 'agentd.yaml');
  writeFileSync(
    cfg,
    [
      'config_version: "2"',
      'agent:',
      '  name: live-e2e',
      '  instruction: You are a test agent.',
      '  preflight: never',
      'intelligence:',
      `  endpoints: http://${llmAddr}`,
      '  model: mock',
      'store:',
      '  kind: memory',
      'a2a:',
      `  listen: http://127.0.0.1:${port}`,
      'interface:',
      '  enabled: true',
      '  debug: true',
      '  pairing:',
      '    enabled: true',
      'lifecycle:',
      '  run_until: drained',
      'workflows:',
      '  - name: greet',
      '    steps:',
      '      s: {kind: manual}',
      '      f: {kind: finish, depends_on: [s], output: "done"}',
      '',
    ].join('\n'),
  );
  const daemon = spawn(BIN, ['--config', cfg], { stdio: 'ignore' });
  try {
    await waitConnectable(port);
    const url = `http://127.0.0.1:${port}`;

    // Client A: a full observing client (mirror + feed).
    const a = new AgentdClient({ url });
    const mirrorA = new Mirror();
    const obsA = new Observation(a, mirrorA);
    obsA.start();
    await until(() => mirrorA.getState().conn === 'ready', 8000, 'client A feed');
    assert.equal(mirrorA.getState().info.debug, true);

    // Client B: a second, independent client sends the prompt.
    const b = new AgentdClient({ url });
    const sent = await b.send('Say something nice');
    assert.ok(sent.task.id.startsWith('task-'));

    // A (which sent NOTHING) sees B's prompt AND the reply via the feed.
    await until(
      () => mirrorA.getState().transcript.some((e) => e.kind === 'user' && e.text.includes('Say something nice')),
      10000,
      "B's prompt on A's transcript",
    );
    await until(
      () => mirrorA.getState().transcript.some((e) => e.kind === 'agent' && e.text.includes('Hello from the live daemon')),
      15000,
      "the reply on A's transcript",
    );

    // Debug reads over the live wire.
    const ctx = mirrorA.getState().transcript.find((e) => e.kind === 'user').ctx;
    const conv = await a.conversationGet(ctx);
    assert.ok(conv.messages.length >= 2, 'transcript bodies over conversation.get');
    const ring = await a.debugEvents(0, 50);
    assert.ok(ring.events.length > 0, 'log ring tail');

    // A workflow runs and its run lands in A's mirror via the feed.
    await b.workflowRun('greet');
    await until(() => [...mirrorA.getState().runs.values()].some((r) => r.workflow === 'greet'), 10000, 'run in mirror');

    // Pairing (RFC 0032 §13): read the code as operator, exchange it (as a
    // credential-less client would), and use the session token.
    const pc = await a.pairingCode();
    assert.equal(pc.code.length, 6);
    const session = await new AgentdClient({ url }).pair(pc.code);
    assert.ok(session.token.startsWith('pat-'));
    const paired = new AgentdClient({ url, bearer: session.token });
    const pairedInfo = await paired.interfaceInfo();
    assert.equal(pairedInfo.enabled, true, 'the session token works as a bearer');

    // Runtime config.set: reshape the display; A sees it via the config event.
    await b.configSet('interface.display.bottom', ['conn', 'model']);
    await until(
      () => JSON.stringify(mirrorA.getState().info?.display?.bottom) === '["conn","model"]',
      8000,
      'display reshaped through the feed',
    );

    obsA.stop();
  } finally {
    daemon.kill('SIGTERM');
    llm.kill('SIGKILL');
    await new Promise((r) => setTimeout(r, 300));
    daemon.kill('SIGKILL');
    rmSync(dir, { recursive: true, force: true });
  }
});
