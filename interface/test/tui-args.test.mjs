// SPDX-License-Identifier: AGPL-3.0-only
// agentd-tui's command line: what it refuses, where its credential may come
// from, and the launcher's sign-in over an inherited fd. The rules are tested
// as functions AND through the real program, because a rule the function
// keeps but the program skips protects nothing.
import test from 'node:test';
import assert from 'node:assert/strict';
import { spawn } from 'node:child_process';
import { writeFileSync, mkdtempSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { fileURLToPath } from 'node:url';
import {
  LAUNCH_REFUSED,
  MAX_LAUNCH_CODE,
  UsageError,
  chooseSignIn,
  layoutOf,
  parseArgs,
  resolveCredential,
  scrubEnv,
} from '../dist/tui/args.js';
import { DEFAULT_LAYOUT } from '../dist/client/index.js';
import { startFakeA2a } from './fake-a2a.mjs';

const root = fileURLToPath(new URL('..', import.meta.url));
const CLI = join(root, 'dist', 'tui', 'cli.js');
const ARGS = join(root, 'dist', 'tui', 'args.js');

/**
 * Run `node <script> args…` with a socket on fd 3 (what `--launch-fd 3`
 * reads) and `write(fd3)` driving the other end. Resolves with the exit code
 * and output; the child is killed after `ms`.
 */
function run(argv, { env = {}, write, ms = 10_000 } = {}) {
  return new Promise((resolve) => {
    const child = spawn(process.execPath, argv, {
      env: { PATH: process.env.PATH, ...env },
      stdio: ['ignore', 'pipe', 'pipe', 'pipe'],
    });
    let stdout = '';
    let stderr = '';
    child.stdout.on('data', (d) => (stdout += d));
    child.stderr.on('data', (d) => (stderr += d));
    // The child may exit before reading fd 3 (a refusal): that is not an error here.
    child.stdio[3].on('error', () => {});
    const timer = setTimeout(() => child.kill('SIGKILL'), ms);
    child.on('close', (code, signal) => {
      clearTimeout(timer);
      resolve({ code, signal, stdout, stderr });
    });
    if (write) write(child.stdio[3]);
    else child.stdio[3].end();
  });
}

test('removed flags are refused by name and the env token is scrubbed', async () => {
  for (const argv of [['--bearer', 'tok'], ['--bearer=tok'], ['--code', '1234'], ['--code=1234']]) {
    const flag = argv[0].split('=')[0];
    assert.throws(
      () => parseArgs(['--endpoint', 'http://127.0.0.1:1', ...argv]),
      (e) => e instanceof UsageError && e.message.startsWith(`${flag} was removed in agentd 1.17.0: `),
      argv.join(' '),
    );
  }
  // The token's value is never echoed back in the refusal.
  assert.throws(() => parseArgs(['--bearer=s3cret']), (e) => !e.message.includes('s3cret'));
  // The program itself refuses, with exit 2, before any request.
  const r = await run([CLI, '--endpoint', 'http://127.0.0.1:1', '--bearer', 's3cret']);
  assert.equal(r.code, 2);
  assert.match(r.stderr, /--bearer was removed in agentd 1\.17\.0: .*--bearer-file/);
  assert.doesNotMatch(r.stderr, /s3cret/);
  // An unknown flag is refused too, rather than mistaken for a setting.
  assert.throws(() => parseArgs(['--spawn']), /unknown option "--spawn"/);

  // AGENTD_BEARER is taken out of the environment as it is read.
  const env = { AGENTD_BEARER: ' tok-1 \n', OTHER: 'x' };
  assert.deepEqual(scrubEnv(env), { bearer: 'tok-1' });
  assert.equal('AGENTD_BEARER' in env, false, 'deleted from the environment');
  assert.equal(env.OTHER, 'x', 'nothing else is touched');
  assert.deepEqual(scrubEnv({}), {});

  // One credential source at a time.
  assert.deepEqual(resolveCredential({ login: false }, 'tok-1'), { kind: 'bearer', token: 'tok-1' });
  assert.deepEqual(resolveCredential({ login: true, scope: 'operator' }, undefined), { kind: 'login', scope: 'operator' });
  assert.deepEqual(resolveCredential({ login: false, bearerFile: 'f' }, undefined, () => 'from-file\n'), { kind: 'bearer', token: 'from-file' });
  assert.throws(() => resolveCredential({ login: false, bearerFile: 'f' }, 'tok-1', () => 'x'), /one credential at a time/);
  assert.throws(() => resolveCredential({ login: false, bearerFile: 'f' }, undefined, () => ' \n'), /is empty/);
  assert.throws(() => parseArgs(['--scope', 'operator']), /add --login/);
});

test('--launch-fd reads the code from the inherited fd until EOF, closes it, and cannot be combined with another credential', async () => {
  // A probe process that reads fd 3 the way the TUI does and reports what it
  // read and whether the fd is still open.
  const probe = [
    '--input-type=module',
    '-e',
    `import { readLaunchCode } from ${JSON.stringify(ARGS)};
     import { fstatSync } from 'node:fs';
     const closed = () => { try { fstatSync(3); return false; } catch (e) { return e.code === 'EBADF'; } };
     let out;
     try { out = { code: readLaunchCode(3), closed: closed() }; }
     catch (e) { out = { error: e.message, closed: closed() }; }
     process.stdout.write(JSON.stringify(out));`,
  ];
  // The code arrives in two writes with a pause between them: a reader that
  // stops at its first read would return half a code.
  let r = await run(probe, {
    write: (s) => {
      s.write('agentd_lc_ab');
      setTimeout(() => s.end('cd\n'), 200);
    },
  });
  assert.deepEqual(JSON.parse(r.stdout), { code: 'agentd_lc_abcd', closed: true }, r.stderr);

  // More than MAX_LAUNCH_CODE bytes is not a launch code — refused, and the
  // fd is closed all the same.
  r = await run(probe, { write: (s) => s.end('x'.repeat(MAX_LAUNCH_CODE + 1)) });
  const over = JSON.parse(r.stdout);
  assert.match(over.error, /more than 256 bytes/);
  assert.equal(over.closed, true);
  // Exactly the limit is still a code.
  r = await run(probe, { write: (s) => s.end('y'.repeat(MAX_LAUNCH_CODE)) });
  assert.equal(JSON.parse(r.stdout).code, 'y'.repeat(MAX_LAUNCH_CODE));
  // Nothing written: no code.
  r = await run(probe, { write: (s) => s.end() });
  assert.match(JSON.parse(r.stdout).error, /wrote no code/);

  // The launch fd is the launcher's sign-in and never combined.
  const dir = mkdtempSync(join(tmpdir(), 'tui-args-'));
  const file = join(dir, 'bearer');
  writeFileSync(file, 'tok\n');
  const combine = /the launch fd is the launcher's sign-in; it cannot be combined with another credential/;
  assert.throws(() => resolveCredential({ login: false, launchFd: 3, bearerFile: file }, undefined), combine);
  assert.throws(() => resolveCredential({ login: true, launchFd: 3 }, undefined), combine);
  assert.throws(() => resolveCredential({ login: false, launchFd: 3 }, 'tok'), combine);
  assert.deepEqual(resolveCredential({ login: false, launchFd: 3 }, undefined), { kind: 'launch', fd: 3 });
  for (const [extra, env] of [
    [['--bearer-file', file], {}],
    [['--login'], {}],
    [[], { AGENTD_BEARER: 'tok' }],
  ]) {
    const res = await run([CLI, '--endpoint', 'http://127.0.0.1:1', '--launch-fd', '3', ...extra], { env });
    assert.equal(res.code, 2, `${extra.join(' ')} ${JSON.stringify(env)}: ${res.stderr}`);
    assert.match(res.stderr, combine);
  }
  // The terminal is not a launch fd.
  assert.throws(() => parseArgs(['--launch-fd', '0']), /3 or more/);

  // Through the real program: an over-long code is refused before any
  // request, and a code the daemon refuses ends the TUI with the one line
  // that says what to do.
  const fake = await startFakeA2a();
  try {
    const long = await run([CLI, '--endpoint', fake.url, '--launch-fd', '3'], { write: (s) => s.end('z'.repeat(300)) });
    assert.equal(long.code, 2);
    assert.match(long.stderr, /more than 256 bytes/);
    assert.equal(fake.requests.length, 0, 'nothing was sent');

    fake.launch.code = 'agentd_lc_good';
    const refused = await run([CLI, '--endpoint', fake.url, '--launch-fd', '3'], { write: (s) => s.end('agentd_lc_bad\n') });
    assert.equal(refused.code, 2, refused.stderr);
    assert.equal(refused.stderr.trim(), `agentd-tui: ${LAUNCH_REFUSED}`);
    const exchange = fake.requests.filter((q) => q.path === '/oauth2/token');
    assert.equal(exchange.length, 1, 'exchanged exactly once, never retried');
    const form = new URLSearchParams(exchange[0].body);
    assert.equal(form.get('grant_type'), 'https://agentd.dev/oauth/grant-type/launch/v1');
    assert.equal(form.get('client_id'), 'agentd-tui');
    assert.equal(form.get('code'), 'agentd_lc_bad');
  } finally {
    await fake.close();
  }
});

test('sign-in follows what the card offers, and the layout refuses unknown items', () => {
  const device = { method: 'device', flow: { scheme: 'device_code', deviceAuthorizationUrl: 'x', tokenUrl: 'y', scopes: [] } };
  assert.deepEqual(chooseSignIn([{ method: 'none' }, device], false), { kind: 'anonymous' });
  assert.equal(chooseSignIn([{ method: 'none' }, device], true).kind, 'device');
  assert.equal(chooseSignIn([{ method: 'bearer', scheme: 'bearer' }, device], false).kind, 'device');
  assert.match(chooseSignIn([{ method: 'bearer', scheme: 'bearer' }], false).reason, /--bearer-file/);
  const only = chooseSignIn([{ method: 'unsupported', schemes: ['mtls'], reason: 'a client certificate (mutual TLS)' }], false);
  assert.equal(only.kind, 'refuse');
  assert.match(only.reason, /only sign-in methods this client cannot use: mtls \(a client certificate/);

  assert.deepEqual(layoutOf({}, {}), { top: [...DEFAULT_LAYOUT.tui.top], bottom: [...DEFAULT_LAYOUT.tui.bottom] });
  assert.deepEqual(layoutOf({ top: 'name,memory:pr' }, { AGENTD_TUI_BOTTOM: 'conn' }), { top: ['name', 'memory:pr'], bottom: ['conn'] });
  assert.throws(() => layoutOf({ bottom: 'conn,nope' }, {}), /--bottom: unknown chrome item\(s\) "nope"/);
  assert.throws(() => layoutOf({}, { AGENTD_TUI_TOP: 'bogus' }), /AGENTD_TUI_TOP: unknown/);
});
