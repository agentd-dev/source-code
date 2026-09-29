// SPDX-License-Identifier: AGPL-3.0-only
// INTEROP: agentd as a CLIENT of the official Python server.
//
// agentd's outbound peer client is tested against agentd and against a
// fixture written from agentd's reading of the spec; here the peer is the
// official hello-world sample on a2a-sdk 1.1.5, which answers the way the
// SDK's authors read it. A workflow — started by the TypeScript client as a
// command, activating the command extension — runs both outbound nodes
// against it: `a2a.send` (a message, handed back at once) and
// `a2a.delegate` (a task, waited for until its artifact arrives).
//
// The sample logs every request before its SDK parses it, so what agentd put
// on the wire is asserted from the receiving side: A2A-Version 1.0 on every
// call, ROLE_USER on every message, returnImmediately on the send.
//
//   AGENTD_E2E_BIN=../target/debug/agentd AGENTD_STOCK_PYTHON=… node --test test/stock/peer.e2e.mjs
import test from 'node:test';
import assert from 'node:assert/strict';
import { spawn } from 'node:child_process';
import { mkdtempSync, openSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import net from 'node:net';
import { AgentdClient, COMMAND_EXTENSION, TERMINAL_STATES, openSession } from '../../dist/client/index.js';
import { freePort, skipUnless, startHelloAgent, stockUnavailable, until } from './harness.mjs';

const BIN = process.env.AGENTD_E2E_BIN;
const skip = skipUnless(BIN ? stockUnavailable() : 'set AGENTD_E2E_BIN to an agentd built with a2a,internal-mocks');

const connects = (port) =>
  new Promise((resolve) => {
    const s = net.connect(port, '127.0.0.1');
    s.once('connect', () => (s.destroy(), resolve(true)));
    s.once('error', () => resolve(false));
  });

test('peer: a2a.send and a2a.delegate complete against the official Python server', { skip }, async (t) => {
  const hello = await startHelloAgent();
  t.after(() => hello.stop());

  const dir = mkdtempSync(join(tmpdir(), 'agentd-peer-'));
  t.after(() => rmSync(dir, { recursive: true, force: true }));
  const playbook = join(dir, 'playbook.json');
  writeFileSync(playbook, JSON.stringify({ turns: [{ content: 'ok' }] }));
  const port = await freePort();
  const cfg = join(dir, 'agentd.yaml');
  writeFileSync(
    cfg,
    [
      'agent:',
      '  name: peer-e2e',
      '  instruction: You are a test agent.',
      '  preflight: never',
      'intelligence:',
      `  endpoints: "mock:file:${playbook}"`,
      '  model: mock',
      'store:',
      '  kind: memory',
      'a2a:',
      `  listen: http://127.0.0.1:${port}`,
      '  peers:',
      '    - name: hello',
      `      endpoint: ${hello.url}`,
      'lifecycle:',
      '  run_until: drained',
      '  drain_timeout: 2s',
      'observability:',
      '  log_level: info',
      'workflows:',
      '  - name: relay',
      '    steps:',
      '      s: {kind: manual}',
      '      tell: {kind: a2a.send, depends_on: [s], to: hello, parts: "hello from agentd"}',
      '      ask: {kind: a2a.delegate, depends_on: [tell], peer: hello, objective: "greet the peer", timeout: 30s}',
      '      f: {kind: finish, depends_on: [ask], status: completed, output: "peer said {{steps.ask.output}}"}',
      '',
    ].join('\n'),
  );
  const log = join(dir, 'daemon.log');
  const daemon = spawn(BIN, ['--config', cfg], { stdio: ['ignore', 'ignore', openSync(log, 'w')] });
  const exited = new Promise((resolve) => daemon.once('exit', resolve));
  t.after(async () => {
    daemon.kill('SIGTERM');
    const k = setTimeout(() => daemon.kill('SIGKILL'), 5000);
    await exited;
    clearTimeout(k);
  });
  const daemonLog = () => readFileSync(log, 'utf8');
  await until(() => connects(port), 20000, `the listener (${daemonLog()})`);

  // The TypeScript client starts the run as a command: the command
  // extension activated, the op one DataPart.
  const url = `http://127.0.0.1:${port}`;
  const session = await openSession(url);
  assert.notEqual(session.caps.command, null, `the card declares ${COMMAND_EXTENSION}`);
  const client = new AgentdClient(session.ep, session.caps);
  const { task } = await client.workflowRun('relay');
  assert.ok(task, 'workflow.run answers with the task the run is under');
  let done = task;
  await until(
    async () => TERMINAL_STATES.has((done = (await client.getTask(task.id)) ?? done).state),
    30000,
    `the run to finish (${daemonLog()}\n--- the peer ---\n${hello.stderr()})`,
  );
  assert.equal(done.state, 'TASK_STATE_COMPLETED', `${JSON.stringify(done)}\n${daemonLog()}`);
  const output = JSON.stringify([done.artifacts, done.artifactData]);
  assert.match(output, /peer said Hello, World! I have received your request \(.*greet the peer/, output);

  // Both steps finished, each one in the daemon's own words.
  const steps = daemonLog()
    .split('\n')
    .filter((l) => l.startsWith('{'))
    .map((l) => JSON.parse(l))
    .filter((e) => e.event === 'step.done');
  for (const step of ['tell', 'ask']) {
    const e = steps.find((x) => x.step === step);
    assert.equal(e?.status, 'done', `${step}: ${JSON.stringify(e)}\n${daemonLog()}`);
  }

  // What the Python server received from agentd.
  const calls = hello.rpc();
  const sends = calls.filter((r) => r.body.method === 'SendMessage' || r.body.method === 'SendStreamingMessage');
  assert.ok(sends.length >= 2, `a message for the send and one for the delegation: ${JSON.stringify(calls)}`);
  for (const r of calls) {
    assert.equal(r['a2a-version'], '1.0', `${r.body.method} without A2A-Version 1.0`);
  }
  for (const r of sends) {
    assert.equal(r.body.params.message.role, 'ROLE_USER', JSON.stringify(r.body));
  }
  const tell = sends.find((r) => r.body.params.message.parts.some((p) => p.text === 'hello from agentd'));
  assert.ok(tell, `the send's text reached the peer: ${JSON.stringify(sends)}`);
  assert.equal(tell.body.params.configuration?.returnImmediately, true, JSON.stringify(tell.body));
});
