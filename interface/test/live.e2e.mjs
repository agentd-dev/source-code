// SPDX-License-Identifier: AGPL-3.0-only
// LIVE end-to-end: drive a real agentd daemon (the compiled Rust binary) with
// this TS client over the actual wire. Four passes, one daemon each:
//
// - a launched terminal client signs in with the code `agentd tui` hands it,
//   and two clients converge on one daemon — every feed frame and every read
//   reply held to the published extension schemas, not to agentd's internals;
// - device sign-in: the operator approves a NAME, and a second session
//   approved under that name is the same principal, with the same tasks;
// - `agentd ui` with the real agentd-ui: the code in the printed launch URL
//   signs in once, from the launched origin, as the operator — and a browser
//   request without that session is never the implicit operator;
// - a client told to use no extension converses with core A2A alone.
//
// Gated: set AGENTD_E2E_BIN to an agentd built with `--features
// a2a,internal-mocks` (the mock LLM), else the tests skip — except under CI,
// where a skip would pass the contract job without running it.
//   AGENTD_E2E_BIN=../target/debug/agentd node --test test/live.e2e.mjs
import test from 'node:test';
import assert from 'node:assert/strict';
import { spawn } from 'node:child_process';
import { chmodSync, existsSync, mkdtempSync, openSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import http from 'node:http';
import { tmpdir } from 'node:os';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';
import net from 'node:net';
import {
  A2A_VERSION,
  A2aClient,
  AgentdClient,
  CARD_PATH,
  COMMAND_EXTENSION,
  ClientError,
  DEFAULT_LAYOUT,
  EVENTS_EXTENSION,
  LAUNCH_GRANT_TYPE,
  LaunchRefused,
  Mirror,
  OPS,
  Observation,
  RpcError,
  TASK_ANNOTATIONS_EXTENSION,
  TASK_NOT_FOUND,
  VERSION_NOT_SUPPORTED,
  commandMessage,
  deviceLogin,
  itemValue,
  launchExchange,
  loginOptions,
  openSession,
  parseLayout,
  userMessage,
} from '../dist/client/index.js';
import { check, validator } from './schemas.mjs';

const BIN = process.env.AGENTD_E2E_BIN;
const skip = !BIN && !process.env.CI ? 'set AGENTD_E2E_BIN to an agentd built with a2a,internal-mocks' : false;
const INTERFACE = join(dirname(fileURLToPath(import.meta.url)), '..');

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
    if (await fn()) return;
    if (Date.now() > deadline) throw new Error(`timeout waiting for ${typeof what === 'function' ? what() : what}`);
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

const connects = (port) =>
  new Promise((resolve) => {
    const s = net.connect(port, '127.0.0.1');
    s.once('connect', () => (s.destroy(), resolve(true)));
    s.once('error', () => resolve(false));
  });

/** A config for a daemon on `port` with the mock LLM playing `playbook`, `a2a` lines under `a2a:`. */
function config(name, port, playbook, a2a = [], rest = []) {
  return [
    'agent:',
    `  name: ${name}`,
    '  instruction: You are a test agent.',
    '  preflight: never',
    'intelligence:',
    `  endpoints: "mock:file:${playbook}"`,
    '  model: mock',
    'store:',
    '  kind: memory',
    'a2a:',
    `  listen: http://127.0.0.1:${port}`,
    ...a2a.map((l) => `  ${l}`),
    'lifecycle:',
    '  run_until: drained',
    '  drain_timeout: 2s',
    ...rest,
    '',
  ].join('\n');
}

/**
 * One request over node:http, which — unlike `fetch` — sends exactly the
 * headers it is given: a browser's `Origin` included.
 */
function raw(url, { method = 'POST', headers = {}, body } = {}) {
  return new Promise((resolve, reject) => {
    const req = http.request(url, { method, headers }, (res) => {
      let text = '';
      res.setEncoding('utf8');
      res.on('data', (d) => (text += d));
      res.on('end', () => {
        let json;
        try {
          json = JSON.parse(text);
        } catch {
          json = undefined;
        }
        resolve({ status: res.statusCode, headers: res.headers, text, json });
      });
    });
    req.on('error', reject);
    if (body !== undefined) req.write(body);
    req.end();
  });
}

/** A JSON-RPC SendMessage carrying one command op, as a raw request body. */
function commandBody(op, args = {}) {
  return JSON.stringify({ jsonrpc: '2.0', id: 1, method: 'SendMessage', params: { message: commandMessage(op, args), configuration: { returnImmediately: false } } });
}

/** A command reply's document: a read's first DataPart. */
const dataOf = (result) => result?.message?.parts?.find((p) => p.data !== undefined)?.data;

/** Validate `value` as the published result of `op`. */
function checkResult(op, value) {
  check(validator(COMMAND_EXTENSION, `/$defs/ops/${op}/result`), value, `the ${op} reply`);
}

test('live: a launched client signs in, and two TS clients converge on one real daemon', { skip }, async () => {
  const dir = mkdtempSync(join(tmpdir(), 'agentd-live-'));
  const playbook = join(dir, 'playbook.json');
  writeFileSync(playbook, JSON.stringify({ turns: [{ content: 'Hello from the live daemon.' }, { content: 'Second reply.' }, { content: 'Third reply.' }] }));
  const port = await freePort();
  const cfg = join(dir, 'agentd.yaml');
  writeFileSync(
    cfg,
    config(
      'live-e2e',
      port,
      playbook,
      ['events:', '  enabled: true', 'introspection:', '  enabled: true'],
      ['workflows:', '  - name: greet', '    steps:', '      s: {kind: manual}', '      f: {kind: finish, depends_on: [s], output: "done"}'],
    ),
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
  const feed = new AbortController();
  // Observers poll and stream until stopped; one left running when an
  // assertion fails would keep this process alive instead of reporting it.
  const observers = [];
  try {
    await until(() => existsSync(codeFile), 20000, () => `the launch code (launcher: ${launcherOut()})`);
    assert.deepEqual(JSON.parse(readFileSync(argvFile, 'utf8')), ['--endpoint', url, '--launch-fd', '3']);
    const code = readFileSync(codeFile, 'utf8');
    assert.match(code, /^\S+\n$/, 'one launch code and a newline, read to end-of-file');

    // Sign in the way agentd-tui does: one exchange at the endpoint's origin,
    // no Origin header, an operator session with no expiry.
    const credential = await launchExchange(`${url}/oauth2/token`, code.trim(), 'agentd-tui');
    assert.equal(credential.scope, 'operator');
    assert.equal(credential.expiresAt, undefined, "a terminal client's session has no expiry of its own");
    await assert.rejects(launchExchange(`${url}/oauth2/token`, code.trim(), 'agentd-tui'), LaunchRefused, 'the code signs in once');

    // Discovery starts from the public card, which anyone may read.
    const card = await (await fetch(`${url}${CARD_PATH}`)).json();
    assert.equal(card.name, 'live-e2e');

    // Client A: a full observing client (mirror + feed), signed in.
    const mirrorA = new Mirror();
    let a = null;
    const obsA = new Observation({ configured: url, credential, onSession: (_s, client) => (a = client) }, mirrorA);
    observers.push(obsA);
    obsA.start();
    await until(() => mirrorA.getState().conn === 'ready', 8000, 'client A feed');
    assert.ok(a instanceof AgentdClient);
    const declared = (c) => (c?.capabilities?.extensions ?? []).map((e) => e.uri);
    const session = mirrorA.getState().session;
    const uris = declared(session.extended ?? session.card);
    for (const uri of [COMMAND_EXTENSION, EVENTS_EXTENSION, TASK_ANNOTATIONS_EXTENSION]) {
      assert.ok(uris.includes(uri), `the card declares ${uri}: ${uris}`);
    }
    assert.equal(session.caps.annotations, true);
    assert.ok(session.caps.events !== null && session.caps.command !== null);

    // Every frame the feed sends from here on, as it arrives, for the
    // published schema to judge at the end.
    const frames = [];
    const feedDone = a
      .subscribeEvents(0, (hello) => frames.push({ hello }), (event) => frames.push({ event }), feed.signal)
      .catch((e) => (feed.signal.aborted ? undefined : Promise.reject(e)));

    // The launch session is the operator's, and the daemon lists it as a launch.
    const sessions = await a.authSessions();
    checkResult(OPS.authSessions, sessions);
    assert.ok(
      sessions.sessions.some((s) => s.kind === 'launch' && s.role === 'operator' && s.client_id === 'agentd-tui'),
      JSON.stringify(sessions),
    );

    // Client B: a second, independent client — no browser and no credential,
    // so the implicit operator of a loopback daemon with no principals.
    const sb = await openSession(url);
    const b = new AgentdClient(sb.ep, sb.caps);
    const sent = await b.send('Say something nice');
    // A task id is the server's to choose: any non-empty string, which reads
    // the same task back.
    assert.equal(typeof sent.task?.id, 'string', JSON.stringify(sent));
    assert.ok(sent.task.id.length > 0);
    assert.equal((await b.getTask(sent.task.id))?.id, sent.task.id);

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

    // Reads over the live wire, each held to its published result schema.
    const ctx = mirrorA.getState().transcript.find((e) => e.kind === 'user').ctx;
    const conv = await a.conversationGet(ctx);
    checkResult(OPS.conversationGet, conv);
    assert.ok(conv.conversation.messages.length >= 2, 'transcript bodies over conversation.get');
    const ring = await a.debugEvents(0, 50);
    checkResult(OPS.debugEvents, ring);
    assert.ok(ring.events.length > 0, 'log ring tail');
    checkResult(OPS.status, await a.status());

    // A workflow runs and its run lands in A's mirror via the feed.
    await b.workflowRun('greet');
    await until(() => [...mirrorA.getState().runs.values()].some((r) => r.workflow === 'greet'), 10000, 'run in mirror');

    // A request that states no protocol version is refused as the spec says:
    // the version is not guessed.
    const unversioned = await raw(url, {
      headers: { 'content-type': 'application/json' },
      body: JSON.stringify({ jsonrpc: '2.0', id: 7, method: 'SendMessage', params: { message: userMessage('no version') } }),
    });
    assert.equal(unversioned.json?.error?.code, VERSION_NOT_SUPPORTED, unversioned.text);
    assert.equal(unversioned.json?.id, 7, unversioned.text);

    // --no-extensions: a client told to use none sees none, activates none,
    // and still converses, observes and reads its task back.
    const sc = await openSession(url, { noExtensions: true });
    assert.equal(sc.caps.command, null);
    assert.equal(sc.caps.events, null);
    assert.equal(sc.caps.annotations, false);
    const core = new AgentdClient(sc.ep, sc.caps);
    await assert.rejects(core.status(), (e) => e instanceof ClientError && e.kind === 'extension-not-declared');
    const plain = await new A2aClient(sc.ep).sendMessage(userMessage('core only, please'), { returnImmediately: true });
    assert.ok(plain.echo === null || plain.echo.length === 0, `nothing was activated: ${plain.echo}`);
    assert.ok(plain.task?.id, JSON.stringify(plain));
    assert.equal(plain.task.metadata?.[TASK_ANNOTATIONS_EXTENSION], undefined, 'no annotations without the extension');
    const mirrorC = new Mirror();
    const obsC = new Observation({ configured: url, noExtensions: true, pollMs: 100 }, mirrorC);
    observers.push(obsC);
    obsC.start();
    await until(() => mirrorC.getState().conn === 'polling', 8000, 'the core-only observer');
    await until(() => mirrorC.getState().tasks.has(plain.task.id), 10000, 'the task in the core-only mirror');
    obsC.stop();

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
    feed.abort();
    await feedDone;

    // Every frame the daemon sent validates against the published bundle,
    // and every task event's annotations against theirs.
    const frame = validator(EVENTS_EXTENSION);
    const annotations = validator(TASK_ANNOTATIONS_EXTENSION);
    assert.ok(frames[0]?.hello, 'the feed opens with hello');
    const kinds = new Set();
    for (const f of frames) {
      check(frame, f, `the feed frame ${JSON.stringify(f).slice(0, 80)}`);
      if (f.event) kinds.add(f.event.kind);
      const ann = f.event?.kind === 'task' ? f.event.data.task.metadata?.[TASK_ANNOTATIONS_EXTENSION] : undefined;
      if (ann !== undefined) check(annotations, ann, `task ${f.event.data.task.id}'s annotations`);
    }
    for (const kind of ['task', 'run', 'step']) assert.ok(kinds.has(kind), `the feed carried a ${kind} event: ${[...kinds]}`);

    // The stand-in exiting drains the daemon, and the launcher with it.
    writeFileSync(release, '');
    const { code: exit } = await within(exited, 20000, () => {
      throw new Error(`the launcher never exited: ${launcherOut()}`);
    });
    assert.equal(exit, 0, launcherOut());
    assert.ok(!readOr(join(dir, 'daemon.log')).includes(code.trim()), 'the code never reached the daemon log');
  } finally {
    for (const o of observers) o.stop();
    feed.abort();
    writeFileSync(release, '');
    daemon.kill('SIGTERM');
    await within(exited, 5000, () => null);
    daemon.kill('SIGKILL');
    rmSync(dir, { recursive: true, force: true });
  }
});

test('live: device sign-in approves a name, and every session under it is one principal', { skip }, async (t) => {
  const dir = mkdtempSync(join(tmpdir(), 'agentd-live-device-'));
  t.after(() => rmSync(dir, { recursive: true, force: true }));
  const playbook = join(dir, 'playbook.json');
  writeFileSync(playbook, JSON.stringify({ turns: [{ content: 'ok' }] }));
  const port = await freePort();
  const cfg = join(dir, 'agentd.yaml');
  const OPS_TOKEN = `live-operator-${process.pid}-${Date.now()}`;
  writeFileSync(
    cfg,
    config('live-device', port, playbook, [
      'bearer: "{{secret:AGENTD_LIVE_OPS}}"',
      'device_grant:',
      '  enabled: true',
      '  scopes: [user, operator]',
      'events:',
      '  enabled: true',
    ]),
  );
  const log = join(dir, 'daemon.log');
  const daemon = spawn(BIN, ['--config', cfg], {
    stdio: ['ignore', 'ignore', openSync(log, 'w')],
    env: { ...process.env, AGENTD_LIVE_OPS: OPS_TOKEN },
  });
  const exited = new Promise((resolve) => daemon.once('exit', resolve));
  t.after(async () => {
    daemon.kill('SIGTERM');
    await within(exited, 5000, () => daemon.kill('SIGKILL'));
  });
  await until(() => connects(port), 20000, () => `the listener (${readOr(log)})`);
  const url = `http://127.0.0.1:${port}`;

  // The operator, holding the configured bearer.
  const os = await openSession(url, { credential: { token: OPS_TOKEN } });
  const operator = new AgentdClient(os.ep, os.caps);

  // The card offers the device grant; this client signs in through it.
  const option = loginOptions(os.card).find((o) => o.method === 'device');
  assert.ok(option, `the card offers the device grant: ${JSON.stringify(os.card.securitySchemes)}`);
  // The grant's interval is seconds; a person takes longer than that, this
  // test does not, so its waits are shortened.
  const clock = { now: () => Date.now(), sleep: (ms) => new Promise((r) => setTimeout(r, Math.min(ms, 200))) };
  const approvals = [];
  const signIn = async (name) => {
    let approved;
    const credential = await deviceLogin({
      flow: option.flow,
      clientId: 'live-e2e',
      clock,
      onCode: (c) => {
        approved = operator.authDeviceApprove(c.userCode, name);
      },
    });
    approvals.push(await approved);
    const s = await openSession(url, { credential });
    return new AgentdClient(s.ep, s.caps);
  };

  const first = await signIn('live-user');
  const task = (await first.send('from the first session')).task;
  assert.ok(task?.id);
  assert.equal(task.principal, 'user:live-user', 'the task is the name’s, not the session’s');

  // A second sign-in approved under the same name is the same principal.
  const second = await signIn('live-user');
  assert.equal((await second.getTask(task.id))?.id, task.id, 'the second session reads the first one’s task');
  assert.ok((await second.listTasks()).tasks.some((x) => x.id === task.id), 'and lists it');

  // A different name is a different principal, which does not.
  const other = await signIn('someone-else');
  await assert.rejects(other.getTask(task.id), (e) => e instanceof RpcError && e.code === TASK_NOT_FOUND);

  for (const a of approvals) checkResult(OPS.authDeviceApprove, a);
  assert.deepEqual(
    approvals.map((a) => a.approved.principal),
    ['user:live-user', 'user:live-user', 'user:someone-else'],
  );
});

test('live: agentd ui signs its tab in once, from the launched origin, as the operator', { skip }, async (t) => {
  const dir = mkdtempSync(join(tmpdir(), 'agentd-live-ui-'));
  t.after(() => rmSync(dir, { recursive: true, force: true }));
  const playbook = join(dir, 'playbook.json');
  writeFileSync(playbook, JSON.stringify({ turns: [{ content: 'ok' }] }));
  const port = await freePort();
  const cfg = join(dir, 'agentd.yaml');
  writeFileSync(cfg, config('live-ui', port, playbook, ['events:', '  enabled: true']));
  const terminal = join(dir, 'terminal');
  // The real agentd-ui: the launcher hands it the socket it bound, and it
  // serves the page and the daemon's endpoint from there.
  const launcher = spawn(BIN, ['ui', '--port', '0', '--no-open', '--daemon-log', join(dir, 'daemon.log'), '--config', cfg], {
    stdio: ['ignore', 'ignore', openSync(terminal, 'w')],
    env: { ...process.env, AGENTD_UI_BIN: join(INTERFACE, 'bin', 'serve.mjs') },
  });
  const exited = new Promise((resolve) => launcher.once('exit', (code, signal) => resolve({ code, signal })));
  t.after(async () => {
    launcher.kill('SIGTERM');
    await within(exited, 10000, () => launcher.kill('SIGKILL'));
  });
  const said = () => readOr(terminal);
  let m;
  await until(() => (m = /(http:\/\/127\.0\.0\.1:(\d+))\/#launch=(\S+)/.exec(said())), 20000, () => `the launch URL (${said()})`);
  const [, origin, , code] = m;
  const endpoint = `http://127.0.0.1:${port}`;

  // The page the URL opens is agentd-ui's, and it names the daemon.
  const boot = await raw(`${origin}/bootstrap.json`, { method: 'GET' });
  assert.equal(boot.status, 200, boot.text);
  assert.deepEqual(boot.json, { endpoint });

  // The exchange a browser tab makes: from the launched origin. (auth.ts
  // launchExchange sets no Origin, as a terminal client must not; a browser
  // adds its own, so this request carries it by hand.)
  const exchange = () =>
    raw(`${endpoint}/oauth2/token`, {
      headers: { origin, 'content-type': 'application/x-www-form-urlencoded' },
      body: new URLSearchParams({ grant_type: LAUNCH_GRANT_TYPE, code, client_id: 'agentd-ui' }).toString(),
    });
  const signed = await exchange();
  assert.equal(signed.status, 200, signed.text);
  assert.equal(signed.json.scope, 'operator');
  const token = signed.json.access_token;
  assert.ok(typeof token === 'string' && token.length > 0);

  // The session reads status as the operator, from that origin.
  const status = (bearer) =>
    raw(endpoint, {
      headers: {
        origin,
        'content-type': 'application/json',
        'a2a-version': A2A_VERSION,
        'a2a-extensions': COMMAND_EXTENSION,
        ...(bearer ? { authorization: `Bearer ${bearer}` } : {}),
      },
      body: commandBody(OPS.status),
    });
  const read = await status(token);
  assert.equal(read.status, 200, read.text);
  checkResult(OPS.status, dataOf(read.json.result));
  const listed = await raw(endpoint, {
    headers: { origin, 'content-type': 'application/json', 'a2a-version': A2A_VERSION, 'a2a-extensions': COMMAND_EXTENSION, authorization: `Bearer ${token}` },
    body: commandBody(OPS.authSessions),
  });
  const sessions = dataOf(listed.json?.result);
  assert.ok(
    sessions?.sessions?.some((s) => s.kind === 'launch' && s.role === 'operator' && s.client_id === 'agentd-ui'),
    listed.text,
  );

  // The code signs in once.
  const again = await exchange();
  assert.equal(again.status, 400, again.text);
  assert.equal(again.json?.error, 'invalid_grant', again.text);

  // A request from a browser is never the implicit operator: without the
  // session, the same read is a 401 — on a loopback daemon with no principals.
  const anonymous = await status(undefined);
  assert.equal(anonymous.status, 401, anonymous.text);

  launcher.kill('SIGTERM');
  const { code: exit, signal } = await within(exited, 20000, () => {
    throw new Error(`the launcher never exited: ${said()}`);
  });
  assert.ok(exit === 0 || signal === 'SIGTERM', `${exit} ${signal}: ${said()}`);
  assert.ok(!readOr(join(dir, 'daemon.log')).includes(code), 'the code never reached the daemon log');
});
