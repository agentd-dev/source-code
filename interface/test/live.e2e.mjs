// SPDX-License-Identifier: AGPL-3.0-only
// LIVE end-to-end: drive a real agentd daemon (the compiled Rust binary) with
// this TS client over the actual wire — signed in the way a launched client
// is, discovery from the public card, feed convergence across two clients,
// debug reads, a workflow run, and the client's own layout drawn from what
// the daemon reported. Gated: set AGENTD_E2E_BIN to the agentd binary (it
// uses the built-in mock LLM), else the test is skipped.
//   AGENTD_E2E_BIN=../../target/debug/agentd node --test test/live.e2e.mjs
//
// The daemon runs under `agentd tui`, the thin launcher, with a stand-in for
// the terminal UI: the launcher hands it a single-use launch code on fd 3 and
// nothing else, and the stand-in passes the code to this test, which redeems
// it exactly as agentd-tui does.
import test from 'node:test';
import assert from 'node:assert/strict';
import { spawn } from 'node:child_process';
import { chmodSync, existsSync, mkdtempSync, openSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import net from 'node:net';
import {
  AgentdClient,
  CARD_PATH,
  COMMAND_EXTENSION,
  DEFAULT_LAYOUT,
  EVENTS_EXTENSION,
  LaunchRefused,
  Mirror,
  Observation,
  TASK_ANNOTATIONS_EXTENSION,
  itemValue,
  launchExchange,
  openSession,
  parseLayout,
} from '../dist/client/index.js';

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

async function until(fn, ms = 10000, what = 'condition') {
  const deadline = Date.now() + ms;
  for (;;) {
    if (fn()) return;
    if (Date.now() > deadline) throw new Error(`timeout waiting for ${what}`);
    await new Promise((r) => setTimeout(r, 50));
  }
}

/** `promise`, or `onTimeout()` after `ms` — with the timer cleared either way. */
function within(promise, ms, onTimeout) {
  let t;
  const timeout = new Promise((resolve, reject) => {
    t = setTimeout(() => {
      try {
        resolve(onTimeout());
      } catch (e) {
        reject(e);
      }
    }, ms);
  });
  return Promise.race([promise, timeout]).finally(() => clearTimeout(t));
}

function readOr(path, fallback = '') {
  try {
    return readFileSync(path, 'utf8');
  } catch {
    return fallback;
  }
}

test('live: a launched client signs in, and two TS clients converge on one real daemon', { skip: !BIN }, async () => {
  const dir = mkdtempSync(join(tmpdir(), 'agentd-live-'));
  const playbook = join(dir, 'playbook.json');
  writeFileSync(playbook, JSON.stringify({ turns: [{ content: 'Hello from the live daemon.' }, { content: 'Second reply.' }] }));
  const addrFile = join(dir, 'llm.addr');
  const llm = spawn(BIN, ['--internal-mock-llm', addrFile, `file:${playbook}`], { stdio: 'ignore' });
  let llmAddr = '';
  await until(() => (llmAddr = readOr(addrFile).trim()).length > 0, 5000, 'mock llm addr');

  const port = await freePort();
  const cfg = join(dir, 'agentd.yaml');
  writeFileSync(
    cfg,
    [
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
      '  events:',
      '    enabled: true',
      '  introspection:',
      '    enabled: true',
      'lifecycle:',
      '  run_until: drained',
      '  drain_timeout: 2s',
      'workflows:',
      '  - name: greet',
      '    steps:',
      '      s: {kind: manual}',
      '      f: {kind: finish, depends_on: [s], output: "done"}',
      '',
    ].join('\n'),
  );

  // The stand-in terminal UI: it records its argv, hands the code it read
  // from fd 3 to this test, and stays up until released — its exit drains
  // the daemon.
  const stub = join(dir, 'client.mjs');
  const codeFile = join(dir, 'code');
  const argvFile = join(dir, 'argv.json');
  const release = join(dir, 'release');
  writeFileSync(
    stub,
    [
      `#!${process.execPath}`,
      "import { existsSync, readFileSync, renameSync, writeFileSync } from 'node:fs';",
      `writeFileSync(${JSON.stringify(argvFile)}, JSON.stringify(process.argv.slice(2)));`,
      "const code = readFileSync(3, 'utf8');",
      `writeFileSync(${JSON.stringify(codeFile + '.part')}, code);`,
      `renameSync(${JSON.stringify(codeFile + '.part')}, ${JSON.stringify(codeFile)});`,
      // Released, or the test's directory is gone: either way, done.
      `setInterval(() => { if (existsSync(${JSON.stringify(release)}) || !existsSync(${JSON.stringify(codeFile)})) process.exit(0); }, 50);`,
      '',
    ].join('\n'),
  );
  chmodSync(stub, 0o755);
  // The launcher's terminal is a file: the stand-in inherits it, and a pipe
  // it held open would keep this process waiting after the test.
  const terminal = join(dir, 'terminal');
  const daemon = spawn(BIN, ['tui', '--daemon-log', join(dir, 'daemon.log'), '--config', cfg], {
    stdio: ['ignore', 'ignore', openSync(terminal, 'w')],
    env: { ...process.env, AGENTD_TUI_BIN: stub },
  });
  const launcherOut = () => readOr(terminal);
  const exited = new Promise((resolve) => daemon.once('exit', (code, signal) => resolve({ code, signal })));
  const url = `http://127.0.0.1:${port}`;
  try {
    await until(() => existsSync(codeFile), 20000, `the launch code (launcher: ${launcherOut()})`);
    assert.deepEqual(JSON.parse(readFileSync(argvFile, 'utf8')), ['--endpoint', url, '--launch-fd', '3']);
    const raw = readFileSync(codeFile, 'utf8');
    assert.match(raw, /^agentd_lc_[0-9a-f]{64}\n$/, 'one launch code and a newline, read to end-of-file');

    // Sign in the way agentd-tui does: one exchange at the endpoint's origin,
    // no Origin header, an operator session with no expiry.
    const credential = await launchExchange(`${url}/oauth2/token`, raw.trim(), 'agentd-tui');
    assert.equal(credential.scope, 'operator');
    assert.equal(credential.expiresAt, undefined, "a terminal client's session has no expiry of its own");
    await assert.rejects(launchExchange(`${url}/oauth2/token`, raw.trim(), 'agentd-tui'), LaunchRefused, 'the code signs in once');

    // Discovery starts from the public card, which anyone may read.
    const card = await (await fetch(`${url}${CARD_PATH}`)).json();
    assert.equal(card.name, 'live-e2e');

    // Client A: a full observing client (mirror + feed), signed in.
    const mirrorA = new Mirror();
    let a = null;
    const obsA = new Observation(
      { configured: url, credential, onSession: (_s, client) => (a = client) },
      mirrorA,
    );
    obsA.start();
    await until(() => mirrorA.getState().conn === 'ready', 8000, 'client A feed');
    assert.ok(a instanceof AgentdClient);
    const declared = (c) => (c?.capabilities?.extensions ?? []).map((e) => e.uri);
    const session = mirrorA.getState().session;
    const uris = declared(session.extended ?? session.card);
    for (const ext of [COMMAND_EXTENSION, EVENTS_EXTENSION, TASK_ANNOTATIONS_EXTENSION]) {
      assert.ok(uris.includes(ext), `the card declares ${ext}: ${uris}`);
    }
    assert.equal(session.caps.annotations, true);
    assert.ok(session.caps.events !== null && session.caps.command !== null);

    // The launch session is the operator's, and the daemon lists it as a launch.
    const sessions = await a.authSessions();
    assert.ok(
      sessions.sessions.some((s) => s.kind === 'launch' && s.role === 'operator' && s.client_id === 'agentd-tui'),
      JSON.stringify(sessions),
    );

    // Client B: a second, independent client — no browser and no credential,
    // so the implicit operator of a loopback daemon with no principals.
    const sb = await openSession(url);
    const b = new AgentdClient(sb.ep, sb.caps);
    const sent = await b.send('Say something nice');
    assert.ok(sent.task?.id, JSON.stringify(sent));

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
    assert.ok(conv.conversation.messages.length >= 2, 'transcript bodies over conversation.get');
    const ring = await a.debugEvents(0, 50);
    assert.ok(ring.events.length > 0, 'log ring tail');

    // A workflow runs and its run lands in A's mirror via the feed.
    await b.workflowRun('greet');
    await until(() => [...mirrorA.getState().runs.values()].some((r) => r.workflow === 'greet'), 10000, 'run in mirror');

    // The layout is the client's own: parsed from what a person wrote and
    // drawn from what the daemon reported.
    const layout = parseLayout('name,conn,nonsense', 'tui');
    assert.deepEqual(layout, { items: ['name', 'conn'], unknown: ['nonsense'] });
    assert.ok(DEFAULT_LAYOUT.tui.top.includes('name'));
    const st = mirrorA.getState();
    const input = { card: st.session.card, status: st.bootstrap, conn: st.conn, endpoint: url };
    assert.equal(itemValue('name', input)?.text, 'live-e2e');
    assert.equal(itemValue('conn', input)?.tone, 'live');

    obsA.stop();

    // The stand-in exiting drains the daemon, and the launcher with it.
    writeFileSync(release, '');
    const { code } = await within(exited, 20000, () => {
      throw new Error(`the launcher never exited: ${launcherOut()}`);
    });
    assert.equal(code, 0, launcherOut());
    assert.ok(!readOr(join(dir, 'daemon.log')).includes('agentd_lc_'), 'the code never reached the daemon log');
  } finally {
    writeFileSync(release, '');
    daemon.kill('SIGTERM');
    llm.kill('SIGKILL');
    await within(exited, 5000, () => null);
    daemon.kill('SIGKILL');
    rmSync(dir, { recursive: true, force: true });
  }
});
