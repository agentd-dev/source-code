// SPDX-License-Identifier: AGPL-3.0-only
/**
 * Signing in: what the card offers (`securitySchemes`), the OAuth 2.0 device
 * grant (RFC 8628) against the daemon's own authorization server, revocation
 * (RFC 7009, found through RFC 8414 metadata), and the launch grant the
 * `agentd tui` / `agentd ui` launcher hands its client.
 *
 * Every token comes from the daemon's /oauth2 endpoints on the listener
 * origin and is presented as a Bearer. No long-lived credential is ever
 * minted for, or stored by, a client: a device session expires, a launch
 * session ends with the launcher, and neither has a refresh token.
 *
 * Every request here goes out with `redirect: 'error'` and no ambient
 * credentials, and only to an https URL or plain http on a loopback host —
 * the endpoints receive a device code, a launch code or a token, and a
 * redirect or a cleartext hop would hand it to someone the operator never
 * named.
 */

import { parseRetryAfter } from './errors.js';
import type { Json } from './types.js';

/** The RFC 8628 grant type. */
export const DEVICE_CODE_GRANT = 'urn:ietf:params:oauth:grant-type:device_code';

/**
 * The launch grant (an RFC 6749 §4.5 extension grant): a single-use code the
 * launcher mints in the daemon's process, or a request the person at the
 * launcher's terminal approves. Must equal the daemon's
 * `surface::launch::LAUNCH_GRANT_TYPE`; `interface_client_guard` checks it.
 */
export const LAUNCH_GRANT_TYPE = 'https://agentd.dev/oauth/grant-type/launch';

/** The RFC 8414 metadata path, relative to the issuer (the listener origin). */
export const AUTHORIZATION_SERVER_METADATA = '/.well-known/oauth-authorization-server';

/** RFC 8628 §3.2: the polling interval when the server names none, in seconds. */
const DEFAULT_INTERVAL_S = 5;
/** RFC 8628 §3.5: `slow_down` adds this much to every later poll. */
const DEVICE_SLOW_DOWN_MS = 5_000;
/** The launch grant's `slow_down` step; its interval is 2 s, so +5 s would triple it. */
const LAUNCH_SLOW_DOWN_MS = 2_000;
/** An OAuth answer is a small JSON object; more than this is not one. */
const MAX_BODY = 64 << 10;

/** A credential a client holds for one endpoint. */
export interface Credential {
  /** The Bearer token. */
  token: string;
  /** The granted scope (`operator`, `user`, …). */
  scope?: string;
  /** Epoch ms after which the token is refused. Absent: it ends only when revoked. */
  expiresAt?: number;
}

/** Time, injectable so a test runs a 10-minute device flow in no time. */
export interface Clock {
  now(): number;
  /** Resolves after `ms`; rejects with the signal's reason when it aborts. */
  sleep(ms: number, signal?: AbortSignal): Promise<void>;
}

export const systemClock: Clock = {
  now: () => Date.now(),
  sleep: (ms, signal) =>
    new Promise<void>((resolve, reject) => {
      if (signal?.aborted) return reject(signal.reason);
      const t = setTimeout(() => {
        signal?.removeEventListener('abort', onAbort);
        resolve();
      }, ms);
      const onAbort = (): void => {
        clearTimeout(t);
        reject(signal?.reason);
      };
      signal?.addEventListener('abort', onAbort, { once: true });
    }),
};

/** Per-call transport options. */
export interface AuthTransport {
  /** Defaults to the global `fetch`. */
  fetch?: typeof fetch;
  signal?: AbortSignal;
}

// ---- errors ---------------------------------------------------------------

/**
 * A sign-in failure. `code` is the OAuth `error` when the server sent one,
 * else a local reason (`insecure-endpoint`, `invalid-response`, …).
 */
export class AuthError extends Error {
  code: string;
  status?: number;
  retryAfterMs?: number;
  constructor(code: string, message: string, extra: { status?: number; retryAfterMs?: number } = {}) {
    super(message);
    this.name = 'AuthError';
    this.code = code;
    if (extra.status !== undefined) this.status = extra.status;
    if (extra.retryAfterMs !== undefined) this.retryAfterMs = extra.retryAfterMs;
  }
}

/** The approver said no (`access_denied`). Asking again will not change that. */
export class DeviceDenied extends AuthError {
  constructor(message = 'the sign-in was denied') {
    super('access_denied', message);
    this.name = 'DeviceDenied';
  }
}

/** Nobody approved the device code in time (`expired_token`); start over for a new code. */
export class DeviceExpired extends AuthError {
  constructor(message = 'the sign-in code expired before it was approved') {
    super('expired_token', message);
    this.name = 'DeviceExpired';
  }
}

/** The launch code or request was refused: already used, expired, or not ours. */
export class LaunchRefused extends AuthError {
  constructor(message = 'the launch code was refused (already used or expired)') {
    super('invalid_grant', message);
    this.name = 'LaunchRefused';
  }
}

/** The terminal-approved launch request expired or was evicted; request a new one. */
export class LaunchExpired extends AuthError {
  constructor(message = 'the sign-in request expired before the terminal approved it') {
    super('expired_token', message);
    this.name = 'LaunchExpired';
  }
}

/** No launcher is running this daemon for this page: there is nobody to approve a request. */
export class NoLauncher extends AuthError {
  constructor(message = 'this daemon was not started by `agentd ui`') {
    super('no-launcher', message, { status: 404 });
    this.name = 'NoLauncher';
  }
}

/** A URL that would carry a code or token in the clear, refused before any request. */
export class InsecureEndpoint extends AuthError {
  constructor(message: string) {
    super('insecure-endpoint', message);
    this.name = 'InsecureEndpoint';
  }
}

/** RFC 8414 metadata that does not describe the server it was fetched for. */
export class IssuerMismatch extends AuthError {
  constructor(message: string) {
    super('issuer-mismatch', message);
    this.name = 'IssuerMismatch';
  }
}

// ---- URLs -----------------------------------------------------------------

/**
 * `localhost` or a loopback IP, the same rule as the daemon's
 * `is_loopback_host`: 127.0.0.0/8 and ::1 (an IPv4-mapped address is not
 * loopback there, so not here either).
 */
export function isLoopbackHost(host: string): boolean {
  const h = host.replace(/^\[/, '').replace(/\]$/, '').toLowerCase();
  if (h === 'localhost' || h === '::1') return true;
  const m = /^(\d{1,3})\.(\d{1,3})\.(\d{1,3})\.(\d{1,3})$/.exec(h);
  return m !== null && Number(m[1]) === 127 && m.slice(2).every((o) => Number(o) <= 255);
}

/**
 * Parse `url` and refuse it unless it is https, or http on a loopback host.
 * Called on every URL before the first request to it, so a card that names a
 * plaintext token endpoint on a public host costs nothing but this error.
 */
export function requireSecureUrl(url: string, what = 'endpoint'): URL {
  let u: URL;
  try {
    u = new URL(url);
  } catch {
    throw new InsecureEndpoint(`${what} ${JSON.stringify(url)} is not a URL`);
  }
  if (u.protocol === 'https:') return u;
  if (u.protocol === 'http:' && isLoopbackHost(u.hostname)) return u;
  throw new InsecureEndpoint(
    `${what} ${u.href} would carry a credential in the clear: use https://, or http:// on a loopback host`,
  );
}

/**
 * {@link requireSecureUrl}, and a loopback host besides. A launch code or
 * request is redeemable only from a loopback peer (an SSH `-L` forward is
 * loopback too), so sending one anywhere else could only burn it — or hand a
 * single-use operator credential to whoever answers.
 */
function requireLoopbackUrl(url: string, what: string): URL {
  const u = requireSecureUrl(url, what);
  if (!isLoopbackHost(u.hostname)) {
    throw new InsecureEndpoint(`${what} ${u.href} is not on this machine: a launch sign-in is redeemable only over loopback`);
  }
  return u;
}

/** The origin of a URL (`scheme://host[:port]`, no trailing slash). */
export function originOf(url: string): string {
  return new URL(url).origin;
}

/**
 * `base` + `path` with exactly one slash between them — the one way URLs are
 * joined here, so an endpoint configured with a trailing slash never becomes
 * `//oauth2/token`.
 */
export function join(base: string, path: string): string {
  return `${base.replace(/\/+$/, '')}/${path.replace(/^\/+/, '')}`;
}

/**
 * What a credential is bound to: origin plus path. A credential is sent only
 * to the endpoint it was issued for — never to whatever a query string or a
 * typed endpoint names later.
 */
export function endpointKey(url: string): string {
  const u = new URL(url);
  return `${u.origin}${u.pathname}`;
}

// ---- the wire -------------------------------------------------------------

type Obj = { [k: string]: Json };

function obj(v: Json | undefined): Obj | undefined {
  return v !== null && v !== undefined && typeof v === 'object' && !Array.isArray(v) ? v : undefined;
}

async function readJson(res: Response): Promise<Obj | undefined> {
  if (!res.body) return undefined;
  const reader = res.body.getReader();
  const decoder = new TextDecoder();
  let text = '';
  let size = 0;
  for (;;) {
    const { done, value } = await reader.read();
    if (done) break;
    size += value.byteLength;
    if (size > MAX_BODY) {
      await reader.cancel().catch(() => {});
      return undefined;
    }
    text += decoder.decode(value, { stream: true });
  }
  text += decoder.decode();
  try {
    return obj(JSON.parse(text) as Json);
  } catch {
    return undefined;
  }
}

interface Answer {
  status: number;
  body: Obj | undefined;
  retryAfterMs?: number;
}

/**
 * One request. No Origin header is set here: a browser adds its own, and the
 * TUI must send none (the daemon binds a launch code to the presence or
 * absence of Origin).
 */
async function send(url: URL, init: { method: 'GET' } | { method: 'POST'; form: Record<string, string> }, o: AuthTransport): Promise<Answer> {
  const f = o.fetch ?? fetch;
  const headers: Record<string, string> = { accept: 'application/json' };
  let body: string | undefined;
  if (init.method === 'POST') {
    headers['content-type'] = 'application/x-www-form-urlencoded';
    body = new URLSearchParams(init.form).toString();
  }
  const res = await f(url.href, {
    method: init.method,
    headers,
    body,
    signal: o.signal,
    redirect: 'error',
    credentials: 'omit',
    cache: 'no-store',
  });
  const out: Answer = { status: res.status, body: await readJson(res) };
  const ra = parseRetryAfter(res.headers.get('retry-after'));
  if (ra !== undefined) out.retryAfterMs = ra;
  return out;
}

/** The OAuth `error` of a refusal, when it carries one. */
function errorCode(a: Answer): string | undefined {
  const e = a.body?.error;
  return typeof e === 'string' ? e : undefined;
}

function failure(a: Answer, what: string): AuthError {
  const code = errorCode(a) ?? `http-${a.status}`;
  const d = a.body?.error_description;
  const extra: { status: number; retryAfterMs?: number } = { status: a.status };
  if (a.retryAfterMs !== undefined) extra.retryAfterMs = a.retryAfterMs;
  return new AuthError(code, `${what}: ${code}${typeof d === 'string' ? ` (${d})` : ''}`, extra);
}

/** RFC 6749 §5.1: a successful token response, as a {@link Credential}. */
function credentialOf(a: Answer, now: number): Credential {
  const b = a.body;
  const token = b?.access_token;
  const type = b?.token_type;
  if (typeof token !== 'string' || token.length === 0 || typeof type !== 'string' || type.toLowerCase() !== 'bearer') {
    throw new AuthError('invalid-response', 'the token endpoint answered without a Bearer access_token', { status: a.status });
  }
  const c: Credential = { token };
  if (typeof b?.scope === 'string') c.scope = b.scope;
  if (typeof b?.expires_in === 'number' && b.expires_in > 0) c.expiresAt = now + b.expires_in * 1000;
  return c;
}

// ---- what the card offers -------------------------------------------------

/** An RFC 8628 flow as the card declares it. */
export interface DeviceFlow {
  /** The card's name for the scheme. */
  scheme: string;
  deviceAuthorizationUrl: string;
  tokenUrl: string;
  /** RFC 8414 metadata, declared only for an https origin. */
  oauth2MetadataUrl?: string;
  /** The scopes an approver may grant, in card order. */
  scopes: string[];
}

/**
 * One way in, per alternative of the card's `securityRequirements`:
 * - `none` — the anonymous `{}` alternative, or a card that declares nothing;
 * - `bearer` — a Bearer token the person already holds (`--bearer-file`);
 * - `device` — sign in with the device grant (its token is a Bearer too);
 * - `unsupported` — something this client cannot present (a client
 *   certificate, an API key, OpenID Connect, a scheme the card never declared).
 */
export type LoginOption =
  | { method: 'none' }
  | { method: 'bearer'; scheme: string }
  | { method: 'device'; flow: DeviceFlow }
  | { method: 'unsupported'; schemes: string[]; reason: string };

function schemeOption(name: string, s: Obj | undefined): LoginOption {
  if (!s) return { method: 'unsupported', schemes: [name], reason: `the card does not declare scheme ${name}` };
  const http = obj(s.httpAuthSecurityScheme);
  if (http) {
    if (typeof http.scheme === 'string' && http.scheme.toLowerCase() === 'bearer') return { method: 'bearer', scheme: name };
    return { method: 'unsupported', schemes: [name], reason: `HTTP ${String(http.scheme)} authentication` };
  }
  const oauth = obj(s.oauth2SecurityScheme);
  if (oauth) {
    const dc = obj(obj(oauth.flows)?.deviceCode);
    if (dc && typeof dc.deviceAuthorizationUrl === 'string' && typeof dc.tokenUrl === 'string') {
      const flow: DeviceFlow = {
        scheme: name,
        deviceAuthorizationUrl: dc.deviceAuthorizationUrl,
        tokenUrl: dc.tokenUrl,
        scopes: Object.keys(obj(dc.scopes) ?? {}),
      };
      if (typeof oauth.oauth2MetadataUrl === 'string' && oauth.oauth2MetadataUrl.length > 0) {
        flow.oauth2MetadataUrl = oauth.oauth2MetadataUrl;
      }
      return { method: 'device', flow };
    }
    return { method: 'unsupported', schemes: [name], reason: 'an OAuth 2.0 flow other than the device grant' };
  }
  if (obj(s.mtlsSecurityScheme)) return { method: 'unsupported', schemes: [name], reason: 'a client certificate (mutual TLS)' };
  if (obj(s.apiKeySecurityScheme)) return { method: 'unsupported', schemes: [name], reason: 'an API key' };
  if (obj(s.openIdConnectSecurityScheme)) return { method: 'unsupported', schemes: [name], reason: 'OpenID Connect' };
  return { method: 'unsupported', schemes: [name], reason: 'an unknown security scheme' };
}

/**
 * The ways into this agent, read from its card (A2A 1.0 `securitySchemes` and
 * `securityRequirements`, ProtoJSON). Each requirement alternative becomes
 * one option; an alternative that needs several schemes at once is
 * `unsupported`, because this client presents one credential. A card that
 * declares schemes but no requirements offers each scheme on its own.
 */
export function loginOptions(card: Json | undefined): LoginOption[] {
  const c = obj(card);
  const schemes = obj(c?.securitySchemes) ?? {};
  const reqs = Array.isArray(c?.securityRequirements) ? c.securityRequirements : undefined;
  const alternatives: string[][] =
    reqs !== undefined
      ? reqs.map((r) => Object.keys(obj(obj(r)?.schemes) ?? {}))
      : Object.keys(schemes).map((name) => [name]);
  if (alternatives.length === 0) return [{ method: 'none' }];
  return alternatives.map((names): LoginOption => {
    if (names.length === 0) return { method: 'none' };
    if (names.length === 1) return schemeOption(names[0], obj(schemes[names[0]]));
    return { method: 'unsupported', schemes: names, reason: `${names.join(' and ')} together` };
  });
}

// ---- polling a token endpoint ----------------------------------------------

interface Poll {
  tokenUrl: URL;
  form: Record<string, string>;
  /** Seconds between polls. */
  interval: number;
  /** What `slow_down` adds to every later poll. */
  slowDownMs: number;
  /** Epoch ms after which polling stops locally. */
  deadline?: number;
  /** The error for `expired_token` (and a passed deadline). */
  expired: () => AuthError;
  /** The error for any other refusal code this grant gives a meaning to. */
  refused: (code: string) => AuthError | undefined;
  clock: Clock;
  what: string;
}

/**
 * Poll a token endpoint until it issues a token: wait `interval`, ask, and on
 * `authorization_pending` wait again; `slow_down` lengthens every later wait
 * (RFC 8628 §3.5 — a client that keeps its pace is being rate-limited for a
 * reason). A 429 waits out its Retry-After.
 */
async function poll(p: Poll, o: AuthTransport): Promise<Credential> {
  let waitMs = p.interval * 1000;
  let next = waitMs;
  for (;;) {
    if (p.deadline !== undefined && p.clock.now() + next > p.deadline) throw p.expired();
    await p.clock.sleep(next, o.signal);
    const a = await send(p.tokenUrl, { method: 'POST', form: p.form }, o);
    if (a.status === 200) return credentialOf(a, p.clock.now());
    const code = errorCode(a);
    next = waitMs;
    if (a.status === 429) {
      next = Math.max(waitMs, a.retryAfterMs ?? 0);
      continue;
    }
    switch (code) {
      case 'authorization_pending':
        continue;
      case 'slow_down':
        waitMs += p.slowDownMs;
        next = waitMs;
        continue;
      case 'expired_token':
        throw p.expired();
      default:
        throw (code !== undefined ? p.refused(code) : undefined) ?? failure(a, p.what);
    }
  }
}

// ---- the device grant (RFC 8628) ------------------------------------------

/** What the person must do to approve a device sign-in. */
export interface DeviceCode {
  userCode: string;
  verificationUri: string;
  verificationUriComplete?: string;
  /** Seconds the code stays valid. */
  expiresIn: number;
}

export interface DeviceLoginOptions extends AuthTransport {
  flow: Pick<DeviceFlow, 'deviceAuthorizationUrl' | 'tokenUrl'>;
  clientId: string;
  /** One of the flow's scopes; the server's default (`user`) when absent. */
  scope?: string;
  /** Show the code to the person; called once, before the first poll. */
  onCode: (c: DeviceCode) => void;
  clock?: Clock;
}

/**
 * Sign in with the device grant: ask for a code, show it, and poll until an
 * approver acts on it. Resolves with the session credential; rejects with
 * {@link DeviceDenied} or {@link DeviceExpired}, which a UI tells apart
 * (asking again helps only after an expiry).
 */
export async function deviceLogin(o: DeviceLoginOptions): Promise<Credential> {
  const clock = o.clock ?? systemClock;
  // Both URLs are checked before the first request: the device code and the
  // token travel to both.
  const authUrl = requireSecureUrl(o.flow.deviceAuthorizationUrl, 'the device authorization endpoint');
  const tokenUrl = requireSecureUrl(o.flow.tokenUrl, 'the token endpoint');
  // One authorization server, on the listener origin: a card (or a caller)
  // that names a token endpoint anywhere else would have us post the device
  // code to a server that did not issue it.
  if (tokenUrl.origin !== authUrl.origin) {
    throw new IssuerMismatch(
      `the token endpoint ${tokenUrl.origin} is not on the device authorization endpoint's origin ${authUrl.origin}`,
    );
  }
  const form: Record<string, string> = { client_id: o.clientId };
  if (o.scope !== undefined) form.scope = o.scope;
  const started = clock.now();
  const a = await send(authUrl, { method: 'POST', form }, o);
  if (a.status !== 200) throw failure(a, 'device authorization');
  const b = a.body;
  if (
    typeof b?.device_code !== 'string' ||
    typeof b.user_code !== 'string' ||
    typeof b.verification_uri !== 'string' ||
    typeof b.expires_in !== 'number'
  ) {
    throw new AuthError('invalid-response', 'the device authorization answer is incomplete', { status: a.status });
  }
  // The verification URI is the link a person is told to follow, and a UI
  // renders it as one: a `javascript:` URI would run in the UI's origin, where
  // the credentials of every other endpoint live, and a plaintext one is a
  // phishing page. The daemon's own config holds the same line (https, or
  // http on loopback); the client does not take the server's word for it.
  const safe = (u: string): boolean => {
    try {
      requireSecureUrl(u);
      return true;
    } catch {
      return false;
    }
  };
  if (!safe(b.verification_uri)) {
    throw new AuthError(
      'invalid-response',
      `the verification URI ${JSON.stringify(b.verification_uri)} is not an https:// URL (or http:// on a loopback host)`,
      { status: a.status },
    );
  }
  const code: DeviceCode = { userCode: b.user_code, verificationUri: b.verification_uri, expiresIn: b.expires_in };
  // The complete URI is a convenience; one that fails the same test is
  // dropped, and the person types the code at the plain URI instead.
  if (typeof b.verification_uri_complete === 'string' && safe(b.verification_uri_complete)) {
    code.verificationUriComplete = b.verification_uri_complete;
  }
  o.onCode(code);
  return poll(
    {
      tokenUrl,
      form: { grant_type: DEVICE_CODE_GRANT, device_code: b.device_code, client_id: o.clientId },
      interval: typeof b.interval === 'number' && b.interval > 0 ? b.interval : DEFAULT_INTERVAL_S,
      slowDownMs: DEVICE_SLOW_DOWN_MS,
      deadline: started + b.expires_in * 1000,
      expired: () => new DeviceExpired(),
      refused: (c) => (c === 'access_denied' ? new DeviceDenied() : undefined),
      clock,
      what: 'device sign-in',
    },
    o,
  );
}

// ---- revocation (RFC 7009 via RFC 8414) ------------------------------------

/**
 * Where to revoke a token issued by `tokenUrl`, from the server's RFC 8414
 * metadata: `oauth2MetadataUrl` when the card declares it, else — for plain
 * http on loopback, where the card declares none — the well-known path on the
 * token endpoint's origin. `undefined` when there is nothing to ask or the
 * server lists no revocation endpoint.
 *
 * The metadata must name the token endpoint's origin as its `issuer`, and the
 * revocation endpoint must be on that origin: the token is sent there, so
 * metadata that points anywhere else is refused rather than followed.
 */
export async function revocationEndpointOf(
  flow: { tokenUrl: string; oauth2MetadataUrl?: string },
  o: AuthTransport = {},
): Promise<string | undefined> {
  const token = requireSecureUrl(flow.tokenUrl, 'the token endpoint');
  let meta = flow.oauth2MetadataUrl;
  if (meta === undefined) {
    if (token.protocol !== 'http:') return undefined;
    meta = join(token.origin, AUTHORIZATION_SERVER_METADATA);
  }
  const a = await send(requireSecureUrl(meta, 'the authorization server metadata'), { method: 'GET' }, o);
  if (a.status !== 200 || !a.body) throw failure(a, 'authorization server metadata');
  if (a.body.issuer !== token.origin) {
    throw new IssuerMismatch(
      `the authorization server metadata names issuer ${JSON.stringify(a.body.issuer)}, not ${token.origin}`,
    );
  }
  const rev = a.body.revocation_endpoint;
  if (typeof rev !== 'string') return undefined;
  const revUrl = requireSecureUrl(rev, 'the revocation endpoint');
  if (revUrl.origin !== token.origin) {
    throw new IssuerMismatch(`the revocation endpoint ${revUrl.href} is not on the issuer ${token.origin}`);
  }
  return revUrl.href;
}

/**
 * Revoke a token (RFC 7009). The server answers 200 for a token it does not
 * know too, so success means "this token no longer works", not "it existed".
 */
export async function revokeToken(
  revocationUrl: string,
  token: string,
  o: AuthTransport & { clientId?: string } = {},
): Promise<void> {
  const url = requireSecureUrl(revocationUrl, 'the revocation endpoint');
  const form: Record<string, string> = { token, token_type_hint: 'access_token' };
  if (o.clientId !== undefined) form.client_id = o.clientId;
  const a = await send(url, { method: 'POST', form }, o);
  if (a.status !== 200) throw failure(a, 'revocation');
}

// ---- the launch grant -------------------------------------------------------

/**
 * Exchange the launcher's single-use code for an operator session. Exactly
 * one request, never retried: any presentation consumes the code, so a retry
 * could only be refused — and a refusal is final ({@link LaunchRefused}).
 */
export async function launchExchange(tokenUrl: string, code: string, clientId: string, o: AuthTransport = {}): Promise<Credential> {
  const url = requireLoopbackUrl(tokenUrl, 'the token endpoint');
  const a = await send(url, { method: 'POST', form: { grant_type: LAUNCH_GRANT_TYPE, code, client_id: clientId } }, o);
  if (a.status === 200) return credentialOf(a, Date.now());
  if (errorCode(a) === 'invalid_grant') throw new LaunchRefused();
  throw failure(a, 'launch sign-in');
}

/** A terminal-approved launch request: the page shows `userCode`, the person types it at the launcher. */
export interface LaunchRequest {
  requestCode: string;
  userCode: string;
  /** Seconds the request stays pending. */
  expiresIn: number;
  /** Seconds between polls. */
  interval: number;
}

/**
 * Ask the launcher for a sign-in the person approves at its terminal
 * (POST /oauth2/launch_authorization). {@link NoLauncher} when the daemon
 * has no launcher slot for a page — the page then offers the device grant, if
 * the card declares it.
 */
export async function launchAuthorize(authorizationUrl: string, clientId: string, o: AuthTransport = {}): Promise<LaunchRequest> {
  const url = requireLoopbackUrl(authorizationUrl, 'the launch authorization endpoint');
  const a = await send(url, { method: 'POST', form: { client_id: clientId } }, o);
  if (a.status === 404) throw new NoLauncher();
  if (a.status !== 200) throw failure(a, 'launch authorization');
  const b = a.body;
  if (typeof b?.request_code !== 'string' || typeof b.user_code !== 'string' || typeof b.expires_in !== 'number') {
    throw new AuthError('invalid-response', 'the launch authorization answer is incomplete', { status: a.status });
  }
  return {
    requestCode: b.request_code,
    userCode: b.user_code,
    expiresIn: b.expires_in,
    interval: typeof b.interval === 'number' && b.interval > 0 ? b.interval : DEFAULT_INTERVAL_S,
  };
}

/**
 * Poll for the operator session a terminal-approved request yields. Pass the
 * request's `interval`. {@link LaunchExpired} means ask for a new request;
 * {@link LaunchRefused} means this one is not ours to redeem.
 */
export async function launchPoll(
  tokenUrl: string,
  requestCode: string,
  clientId: string,
  clock: Clock = systemClock,
  o: AuthTransport & { interval?: number } = {},
): Promise<Credential> {
  const url = requireLoopbackUrl(tokenUrl, 'the token endpoint');
  return poll(
    {
      tokenUrl: url,
      form: { grant_type: LAUNCH_GRANT_TYPE, request_code: requestCode, client_id: clientId },
      interval: o.interval !== undefined && o.interval > 0 ? o.interval : DEFAULT_INTERVAL_S,
      slowDownMs: LAUNCH_SLOW_DOWN_MS,
      expired: () => new LaunchExpired(),
      refused: (c) => (c === 'invalid_grant' ? new LaunchRefused('the sign-in request was refused') : undefined),
      clock,
      what: 'launch sign-in',
    },
    o,
  );
}
