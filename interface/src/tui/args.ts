// SPDX-License-Identifier: AGPL-3.0-only
/**
 * `agentd-tui`'s command line and environment, and the decisions they settle
 * before anything is drawn: which credential (if any) the TUI holds, how it
 * signs in, and the chrome layout.
 *
 * Kept apart from `cli.tsx` so every rule here is a function a test can call,
 * instead of a side effect of importing the program.
 *
 * Credentials, and where they may come from:
 * - `--bearer-file PATH`, or `AGENTD_BEARER` — read, then deleted from this
 *   process's environment so nothing started later inherits it;
 * - `--login [--scope user|operator]` — the OAuth device grant the card offers;
 * - `--launch-fd N` — the `agentd tui` launcher's single-use launch code on an
 *   inherited fd, exchanged for an operator session held in memory only.
 * A token on the command line (`--bearer`) is not one of them: argv is
 * readable by every local user through `ps` and /proc.
 */

import { closeSync, readFileSync, readSync } from 'node:fs';
import { DEFAULT_LAYOUT, inert, join, originOf, parseLayout } from '../client/index.js';
import type { DeviceCode, Layout, LoginOption } from '../client/index.js';

/** The client_id the TUI names itself by at the daemon's OAuth endpoints. */
export const TUI_CLIENT_ID = 'agentd-tui';

/** A launch code is `agentd_lc_` + 64 hex; anything longer than this is not one. */
export const MAX_LAUNCH_CODE = 256;

/** Flags that no longer exist, refused by name with where to go instead. */
export const REMOVED_FLAGS: ReadonlyArray<readonly [string, string]> = Object.freeze([
  [
    '--bearer',
    'a token on the command line is readable by every local user (ps, /proc): use --bearer-file PATH or AGENTD_BEARER',
  ],
  ['--code', 'pairing was replaced by the OAuth device grant: use --login'],
] as const);

/** A refusal of the command line or environment: printed, then exit 2. */
export class UsageError extends Error {
  constructor(message: string) {
    super(message);
    this.name = 'UsageError';
  }
}

export interface TuiArgs {
  endpoint?: string;
  bearerFile?: string;
  login: boolean;
  scope?: 'user' | 'operator';
  launchFd?: number;
  noExtensions: boolean;
  top?: string;
  bottom?: string;
  debug: boolean;
  inline: boolean;
  insecure: boolean;
  help: boolean;
}

/** `--name value` or `--name=value`; `undefined` when `a` is not `name`. */
function valued(argv: readonly string[], i: number, name: string): { value: string; next: number } | undefined {
  const a = argv[i];
  if (a.startsWith(`${name}=`)) return { value: a.slice(name.length + 1), next: i };
  if (a !== name) return undefined;
  const value = argv[i + 1];
  if (value === undefined) throw new UsageError(`${name} needs a value`);
  return { value, next: i + 1 };
}

/**
 * Read the command line. A removed flag is refused by name — silently
 * ignoring `--bearer TOKEN` would start an unauthenticated client while the
 * token sat in the process list — and so is anything unknown, so a typo is
 * never mistaken for a setting.
 */
export function parseArgs(argv: readonly string[]): TuiArgs {
  const out: TuiArgs = { login: false, noExtensions: false, debug: false, inline: false, insecure: false, help: false };
  for (let i = 0; i < argv.length; i++) {
    const a = argv[i];
    const removed = REMOVED_FLAGS.find(([flag]) => a === flag || a.startsWith(`${flag}=`));
    if (removed) throw new UsageError(`${removed[0]} was removed in agentd 1.17.0: ${removed[1]}`);
    let v: { value: string; next: number } | undefined;
    if (a === '-e') v = valued(argv, i, '-e');
    else v = valued(argv, i, '--endpoint');
    if (v) {
      out.endpoint = v.value;
      i = v.next;
      continue;
    }
    if ((v = valued(argv, i, '--bearer-file'))) {
      out.bearerFile = v.value;
      i = v.next;
    } else if ((v = valued(argv, i, '--scope'))) {
      if (v.value !== 'user' && v.value !== 'operator') throw new UsageError(`--scope is user or operator, not ${JSON.stringify(v.value)}`);
      out.scope = v.value;
      i = v.next;
    } else if ((v = valued(argv, i, '--launch-fd'))) {
      // 0–2 are the terminal: reading a "code" from stdin would swallow what
      // the person types, so only an extra inherited fd qualifies.
      if (!/^\d+$/.test(v.value) || Number(v.value) < 3) throw new UsageError(`--launch-fd names an inherited file descriptor (3 or more), not ${JSON.stringify(v.value)}`);
      out.launchFd = Number(v.value);
      i = v.next;
    } else if ((v = valued(argv, i, '--top'))) {
      out.top = v.value;
      i = v.next;
    } else if ((v = valued(argv, i, '--bottom'))) {
      out.bottom = v.value;
      i = v.next;
    } else if (a === '--login') out.login = true;
    else if (a === '--no-extensions') out.noExtensions = true;
    else if (a === '--debug') out.debug = true;
    else if (a === '--inline') out.inline = true;
    else if (a === '--insecure') out.insecure = true;
    else if (a === '-h' || a === '--help') out.help = true;
    else throw new UsageError(`unknown option ${JSON.stringify(a)} — see --help`);
  }
  if (out.scope !== undefined && !out.login) throw new UsageError('--scope chooses what --login asks for; add --login');
  return out;
}

/**
 * Take the credential out of the environment: `AGENTD_BEARER` is returned
 * and deleted, so no code that runs later — and no process this one starts —
 * finds it there. Called before anything else reads the environment.
 */
export function scrubEnv(env: NodeJS.ProcessEnv): { bearer?: string } {
  const bearer = env.AGENTD_BEARER;
  delete env.AGENTD_BEARER;
  return bearer !== undefined && bearer.trim() !== '' ? { bearer: bearer.trim() } : {};
}

/** The line printed when TLS verification is off, or `undefined` when it is on. */
export function insecureWarning(args: Pick<TuiArgs, 'insecure'>, env: NodeJS.ProcessEnv): string | undefined {
  if (!args.insecure && env.AGENTD_INSECURE !== '1') return undefined;
  return (
    `agentd-tui: TLS certificate verification is OFF (${args.insecure ? '--insecure' : 'AGENTD_INSECURE=1'}) — ` +
    'anyone on the path can read and change this session; use it only against a self-signed development daemon\n'
  );
}

/** How the TUI signs in, as the command line and environment say. */
export type CredentialSource =
  | { kind: 'none' }
  | { kind: 'bearer'; token: string }
  | { kind: 'login'; scope?: 'user' | 'operator' }
  | { kind: 'launch'; fd: number };

/**
 * Settle the ONE credential source. Two at once are refused rather than
 * ranked: which one wins is a guess, and a guess about a credential is how
 * someone ends up acting as a principal they did not mean. The launch fd is
 * the launcher's own sign-in and is never combined with anything.
 */
export function resolveCredential(
  args: Pick<TuiArgs, 'bearerFile' | 'login' | 'scope' | 'launchFd'>,
  envBearer: string | undefined,
  readFile: (path: string) => string = (p) => readFileSync(p, 'utf8'),
): CredentialSource {
  const named = [
    args.bearerFile !== undefined ? '--bearer-file' : undefined,
    args.login ? '--login' : undefined,
    envBearer !== undefined ? 'AGENTD_BEARER' : undefined,
  ].filter((x): x is string => x !== undefined);
  if (args.launchFd !== undefined) {
    if (named.length > 0) {
      throw new UsageError(
        `the launch fd is the launcher's sign-in; it cannot be combined with another credential (${named.join(', ')})`,
      );
    }
    return { kind: 'launch', fd: args.launchFd };
  }
  if (named.length > 1) throw new UsageError(`one credential at a time: ${named.join(' and ')} were both given`);
  if (args.bearerFile !== undefined) {
    let token: string;
    try {
      token = readFile(args.bearerFile).trim();
    } catch (e) {
      throw new UsageError(`--bearer-file ${args.bearerFile}: ${(e as { code?: string }).code ?? String(e)}`);
    }
    if (token === '') throw new UsageError(`--bearer-file ${args.bearerFile} is empty`);
    return { kind: 'bearer', token };
  }
  if (envBearer !== undefined) return { kind: 'bearer', token: envBearer };
  if (args.login) return args.scope !== undefined ? { kind: 'login', scope: args.scope } : { kind: 'login' };
  return { kind: 'none' };
}

/**
 * The launch code on inherited fd `fd`: read to EOF (the launcher writes the
 * code and closes its end), at most {@link MAX_LAUNCH_CODE} bytes, and the fd
 * is closed either way — so the code is in this process's memory only, and no
 * later reader of /proc/<pid>/fd finds a pipe to drain.
 */
export function readLaunchCode(fd: number): string {
  const buf = Buffer.alloc(MAX_LAUNCH_CODE + 1);
  let len = 0;
  try {
    for (;;) {
      let n: number;
      try {
        n = readSync(fd, buf, len, buf.length - len, null);
      } catch (e) {
        const code = (e as { code?: string }).code;
        // A descriptor someone set O_NONBLOCK on: wait for the writer.
        if (code === 'EAGAIN') {
          Atomics.wait(new Int32Array(new SharedArrayBuffer(4)), 0, 0, 10);
          continue;
        }
        throw new UsageError(`--launch-fd ${fd}: no launch code to read (${code ?? String(e)})`);
      }
      if (n === 0) break;
      len += n;
      if (len > MAX_LAUNCH_CODE) throw new UsageError(`--launch-fd ${fd}: more than ${MAX_LAUNCH_CODE} bytes, which is not a launch code`);
    }
  } finally {
    try {
      closeSync(fd);
    } catch {
      /* already gone: nothing left to close */
    }
  }
  const code = buf.subarray(0, len).toString('utf8').trim();
  buf.fill(0);
  if (code === '') throw new UsageError(`--launch-fd ${fd}: the launcher wrote no code`);
  return code;
}

/**
 * Where a launch code is redeemed: the token endpoint on the endpoint's
 * ORIGIN. The launcher's daemon serves it there whether or not the card
 * declares any OAuth scheme, so it is not read from the card.
 */
export function launchTokenUrl(endpoint: string): string {
  return join(originOf(endpoint), '/oauth2/token');
}

/**
 * A device code as the terminal shows it: the link to open and the code to
 * enter. Both are the authorization server's words, and that server is
 * whatever the card names — so they are inert, and a control sequence in
 * either is shown rather than run by the terminal.
 */
export function shownCode(c: DeviceCode): { uri: string; code: string } {
  return { uri: inert(c.verificationUriComplete ?? c.verificationUri), code: inert(c.userCode) };
}

/**
 * What `/logout` says when the daemon offers nowhere to revoke the session.
 * A device session expires; the launcher's has no expiry, so it stays valid
 * for as long as the daemon runs — and saying otherwise would be a promise
 * that never comes true.
 */
export function unrevokedNote(signIn: 'device' | 'launch'): string {
  return signIn === 'launch'
    ? 'signed out here; the daemon lists no revocation endpoint, so the launch session stays valid until the daemon exits'
    : 'signed out here; the daemon lists no revocation endpoint, so the session ends when it expires';
}

/** What a refused launch exchange tells the person (exit 2). */
export const LAUNCH_REFUSED = 'the launch code was refused (already used or expired): restart `agentd tui`';

/**
 * How to sign in, given the card's ways in and whether `--login` was asked:
 * - `anonymous` — the card offers an anonymous way in (or declares nothing),
 *   and no sign-in was asked for;
 * - `device` — sign in with the device grant;
 * - `refuse` — nothing this client can present, or `--login` with no device
 *   grant; the reason is the whole message.
 */
export function chooseSignIn(
  options: readonly LoginOption[],
  wantLogin: boolean,
): { kind: 'anonymous' } | { kind: 'device'; flow: Extract<LoginOption, { method: 'device' }>['flow'] } | { kind: 'refuse'; reason: string } {
  const device = options.find((o): o is Extract<LoginOption, { method: 'device' }> => o.method === 'device');
  if (!wantLogin && options.some((o) => o.method === 'none')) return { kind: 'anonymous' };
  if (device) return { kind: 'device', flow: device.flow };
  if (wantLogin) return { kind: 'refuse', reason: 'this agent offers no device sign-in (its card declares no device_code flow): use --bearer-file' };
  if (options.some((o) => o.method === 'bearer')) {
    return { kind: 'refuse', reason: 'this agent requires a bearer token: pass --bearer-file PATH or set AGENTD_BEARER' };
  }
  const why = options
    .filter((o): o is Extract<LoginOption, { method: 'unsupported' }> => o.method === 'unsupported')
    .map((o) => `${o.schemes.join('+')} (${o.reason})`);
  return { kind: 'refuse', reason: `this agent offers only sign-in methods this client cannot use: ${why.join('; ')}` };
}

/**
 * The chrome layout: `--top`/`--bottom`, else `AGENTD_TUI_TOP`/`_BOTTOM`,
 * else the TUI's default. An unknown item is refused with the list of known
 * ones, instead of quietly not showing.
 */
export function layoutOf(args: Pick<TuiArgs, 'top' | 'bottom'>, env: NodeJS.ProcessEnv): Layout {
  const edge = (flag: string, raw: string | undefined, fallback: readonly string[]): string[] => {
    if (raw === undefined) return [...fallback];
    const { items, unknown } = parseLayout(raw, 'tui');
    if (unknown.length > 0) {
      throw new UsageError(`${flag}: unknown chrome item(s) ${unknown.map((u) => JSON.stringify(u)).join(', ')} — /layout lists the known ones`);
    }
    return items;
  };
  return {
    top: edge(args.top !== undefined ? '--top' : 'AGENTD_TUI_TOP', args.top ?? env.AGENTD_TUI_TOP, DEFAULT_LAYOUT.tui.top),
    bottom: edge(args.bottom !== undefined ? '--bottom' : 'AGENTD_TUI_BOTTOM', args.bottom ?? env.AGENTD_TUI_BOTTOM, DEFAULT_LAYOUT.tui.bottom),
  };
}

/** `--help`. */
export const HELP = [
  'agentd-tui — terminal display client for agentd (thin: the daemon hosts all state)',
  '',
  '  agentd-tui --endpoint URL [--bearer-file PATH | --login [--scope user|operator]]',
  '             [--no-extensions] [--top items] [--bottom items] [--debug] [--inline] [--insecure]',
  '',
  '  --endpoint, -e    the agent (its base URL or its card URL); the card says where to talk to it',
  '  --bearer-file     read a bearer token from PATH',
  '  --login           sign in with the device grant the card offers (--scope asks for one)',
  '  --launch-fd N     the `agentd tui` launcher hands its sign-in over this fd; not for hand use',
  '  --no-extensions   speak core A2A only',
  '  --top, --bottom   the chrome: comma-separated items (/layout lists them)',
  '  --debug           open on the debug screen (shown only while the daemon offers introspection)',
  '  --inline          render into the scrollback instead of fullscreen',
  '  --insecure        skip TLS verification (self-signed development daemons only)',
  '',
  '  env: AGENTD_ENDPOINT, AGENTD_BEARER (read, then removed from the environment),',
  '       AGENTD_TUI_TOP, AGENTD_TUI_BOTTOM, AGENTD_TUI_INLINE=1, AGENTD_INSECURE=1',
  '',
  `  removed: ${REMOVED_FLAGS.map(([f]) => f).join(', ')}`,
  '',
].join('\n');
