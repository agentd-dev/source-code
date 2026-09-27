// SPDX-License-Identifier: AGPL-3.0-only
/**
 * Reading failures: the `WWW-Authenticate` challenge, `Retry-After`, the
 * `@type`d google.rpc details in `error.data`, and the one `classify()` that
 * every loop and UI uses to decide what a failure means — stop, wait, or
 * retry. One table here, so the TUI and the web UI can never disagree about
 * whether a revoked session is worth retrying (it is not).
 */

import {
  BearerChallenge,
  ClientError,
  CONTENT_TYPE_NOT_SUPPORTED,
  EXTENSION_SUPPORT_REQUIRED,
  INTERNAL_ERROR,
  INVALID_PARAMS,
  INVALID_REQUEST,
  Json,
  METHOD_NOT_FOUND,
  PARSE_ERROR,
  PERMISSION_DENIED,
  RpcError,
  UNAUTHENTICATED,
  UNSUPPORTED_OPERATION,
  VERSION_NOT_SUPPORTED,
} from './types.js';
import { A2A_VERSION } from './wire.js';

const TCHAR = /[!#$%&'*+\-.^_`|~0-9A-Za-z]/;

/** Every challenge in a `WWW-Authenticate` value, params keyed lower-case. */
function parseChallenges(h: string): { scheme: string; params: { [k: string]: string } }[] {
  const out: { scheme: string; params: { [k: string]: string } }[] = [];
  const n = h.length;
  let i = 0;
  const ows = (): void => {
    while (i < n && (h[i] === ' ' || h[i] === '\t')) i++;
  };
  const token = (): string => {
    const s = i;
    while (i < n && TCHAR.test(h[i])) i++;
    return h.slice(s, i);
  };
  // quoted-string with quoted-pair: `\x` stands for `x`.
  const quoted = (): string => {
    let v = '';
    i++; // the opening quote
    while (i < n && h[i] !== '"') {
      if (h[i] === '\\' && i + 1 < n) i++;
      v += h[i++];
    }
    i++; // the closing quote
    return v;
  };
  // Does an auth-param (`token BWS "=" BWS value`) start here? After a comma
  // this is the only way to tell "another param" from "the next challenge";
  // `token==` is a token68, not a param.
  const paramAhead = (): boolean => {
    const save = i;
    ows();
    const t = token();
    ows();
    let ok = false;
    if (t.length > 0 && h[i] === '=') {
      i++;
      ows();
      ok = i < n && h[i] !== ',' && h[i] !== '=';
    }
    i = save;
    return ok;
  };
  const skipSeparators = (): void => {
    while (i < n && (h[i] === ',' || h[i] === ' ' || h[i] === '\t')) i++;
  };

  while (i < n) {
    skipSeparators();
    if (i >= n) break;
    const scheme = token();
    if (!scheme) {
      i++; // garbage: step over it
      continue;
    }
    const c = { scheme, params: {} as { [k: string]: string } };
    out.push(c);
    ows();
    if (!paramAhead()) {
      while (i < n && h[i] !== ',') i++; // a token68, or nothing
      continue;
    }
    for (;;) {
      ows();
      const name = token().toLowerCase(); // auth-param names are case-insensitive
      ows();
      i++; // '='
      ows();
      const value = h[i] === '"' ? quoted() : token();
      if (!(name in c.params)) c.params[name] = value;
      ows();
      while (i < n && h[i] !== ',') i++; // anything malformed up to the next element
      if (i >= n) break;
      skipSeparators();
      if (!paramAhead()) break;
    }
  }
  return out;
}

/**
 * Parse a `WWW-Authenticate` value (RFC 7235 auth-params, quoted-string and
 * quoted-pair). The Bearer challenge is returned when there is one (the
 * scheme compares case-insensitively), else the first.
 */
export function parseChallenge(h: string | null | undefined): BearerChallenge | undefined {
  if (!h) return undefined;
  const all = parseChallenges(h);
  const c = all.find((x) => x.scheme.toLowerCase() === 'bearer') ?? all[0];
  if (!c) return undefined;
  const out: BearerChallenge = { scheme: c.scheme };
  if (c.params.realm !== undefined) out.realm = c.params.realm;
  if (c.params.error !== undefined) out.error = c.params.error;
  if (c.params.error_description !== undefined) out.errorDescription = c.params.error_description;
  if (c.params.resource_metadata !== undefined) out.resourceMetadata = c.params.resource_metadata;
  return out;
}

/** `Retry-After` (delta-seconds or an HTTP-date) in milliseconds from `now`. */
export function parseRetryAfter(h: string | null | undefined, now: number = Date.now()): number | undefined {
  if (!h) return undefined;
  const v = h.trim();
  if (/^\d+$/.test(v)) return Number(v) * 1000;
  const at = Date.parse(v);
  if (Number.isNaN(at)) return undefined;
  return Math.max(0, at - now);
}

/** The `@type`d detail objects of an error's `data`. */
function details(e: unknown): { [k: string]: Json }[] {
  if (!(e instanceof RpcError)) return [];
  const d = e.data;
  const list = Array.isArray(d) ? d : d !== undefined && d !== null ? [d] : [];
  return list.filter((x): x is { [k: string]: Json } => x !== null && typeof x === 'object' && !Array.isArray(x));
}

function detailOf(e: unknown, type: string): { [k: string]: Json } | undefined {
  const want = `google.rpc.${type}`;
  return details(e).find((x) => typeof x['@type'] === 'string' && x['@type'].endsWith(want));
}

/** The `google.rpc.ErrorInfo` an error carries: the stable, machine-readable reason. */
export function errorInfo(
  e: unknown,
): { reason: string; domain?: string; metadata?: { [k: string]: string } } | undefined {
  const d = detailOf(e, 'ErrorInfo');
  if (!d || typeof d.reason !== 'string') return undefined;
  const out: { reason: string; domain?: string; metadata?: { [k: string]: string } } = { reason: d.reason };
  if (typeof d.domain === 'string') out.domain = d.domain;
  if (d.metadata && typeof d.metadata === 'object' && !Array.isArray(d.metadata)) {
    out.metadata = Object.fromEntries(
      Object.entries(d.metadata).filter((kv): kv is [string, string] => typeof kv[1] === 'string'),
    );
  }
  return out;
}

/** The `google.rpc.BadRequest` field violations an error carries, if any. */
export function badRequest(e: unknown): { field: string; description: string }[] | undefined {
  const d = detailOf(e, 'BadRequest');
  if (!d || !Array.isArray(d.fieldViolations)) return undefined;
  return d.fieldViolations
    .filter((v): v is { [k: string]: Json } => v !== null && typeof v === 'object' && !Array.isArray(v))
    .map((v) => ({ field: String(v.field ?? ''), description: String(v.description ?? '') }));
}

/**
 * What a failure means for the loop that hit it.
 * - Terminal: `unauthenticated`, `forbidden`, `incompatible` — retrying
 *   cannot help, so every loop stops and the UI asks for a person.
 * - `rate-limited`: wait `retryAfterMs`, then retry.
 * - `protocol`: one side is wrong; show it, retry only at the cap.
 * - `unavailable`: an extension call the agent does not serve; degrade.
 * - `transient`: network, 5xx, internal errors (draining included); back off.
 */
export type FailureKind =
  | 'unauthenticated'
  | 'forbidden'
  | 'incompatible'
  | 'rate-limited'
  | 'protocol'
  | 'unavailable'
  | 'transient';

export interface Failure {
  kind: FailureKind;
  message: string;
  retryAfterMs?: number;
}

/**
 * The daemon key a browser's origin must be listed under. Named here because
 * the hint below is the one place it is shown.
 */
const CORS_ORIGINS_KEY = 'a2a.cors.origins';

/** The page's origin when running in a browser; `undefined` in Node. */
function pageOrigin(): string | undefined {
  const g = globalThis as { window?: unknown; location?: { origin?: string } };
  return g.window !== undefined && typeof g.location?.origin === 'string' ? g.location.origin : undefined;
}

function networkMessage(e: TypeError): string {
  const origin = pageOrigin();
  if (origin !== undefined) {
    // A browser reports a CORS refusal as the same opaque TypeError as a dead
    // network, so name the likelier cause a person can actually fix.
    return (
      `network error or CORS refusal — is ${origin} listed in the agent's allowed origins ` +
      `(agentd: ${CORS_ORIGINS_KEY})?`
    );
  }
  const cause = (e as { cause?: { code?: string; message?: string } }).cause;
  const why = cause?.code ?? cause?.message;
  return `network error: ${e.message}${why ? ` (${why})` : ''}`;
}

/** Sort any failure into a {@link Failure}. `ctx.extensionCall` marks a call to an extension method. */
export function classify(e: unknown, ctx: { extensionCall?: boolean } = {}): Failure {
  if (e instanceof ClientError) {
    switch (e.kind) {
      case 'session-revoked':
      case 'session-expired':
        return { kind: 'unauthenticated', message: 'session expired or revoked — sign in again' };
      case 'discovery':
      case 'no-interface':
      case 'cross-origin':
      case 'required-extension':
      case 'unsupported-scheme':
        return { kind: 'incompatible', message: e.message };
      case 'extension-not-activated':
      case 'extension-not-declared':
        return { kind: ctx.extensionCall ? 'unavailable' : 'protocol', message: e.message };
      default:
        return { kind: 'protocol', message: e.message };
    }
  }
  if (e instanceof RpcError) {
    const info = errorInfo(e);
    const status = e.status ?? 0;
    if (status === 401 || e.code === UNAUTHENTICATED) {
      return {
        kind: 'unauthenticated',
        message:
          e.challenge?.error === 'invalid_token'
            ? 'session expired or revoked — sign in again'
            : 'this agent requires sign-in',
      };
    }
    if (status === 403 || e.code === PERMISSION_DENIED) {
      return { kind: 'forbidden', message: `not permitted: ${e.message}` };
    }
    if (e.code === VERSION_NOT_SUPPORTED) {
      const supports = info?.metadata?.supportedVersions ?? 'unknown versions';
      return { kind: 'incompatible', message: `agent does not support A2A ${A2A_VERSION} (supports ${supports})` };
    }
    if (e.code === EXTENSION_SUPPORT_REQUIRED) {
      const which = info?.metadata?.extensions ?? e.message;
      return { kind: 'incompatible', message: `agent requires extensions this client does not speak: ${which}` };
    }
    if (status === 429 || info?.reason === 'RATE_LIMITED') {
      const secs = Number(info?.metadata?.retryAfterSeconds);
      const retryAfterMs = e.retryAfterMs ?? (Number.isFinite(secs) ? secs * 1000 : undefined);
      const f: Failure = { kind: 'rate-limited', message: e.message };
      if (retryAfterMs !== undefined) f.retryAfterMs = retryAfterMs;
      return f;
    }
    if (
      e.code === PARSE_ERROR ||
      e.code === INVALID_REQUEST ||
      e.code === INVALID_PARAMS ||
      e.code === CONTENT_TYPE_NOT_SUPPORTED ||
      status === 415
    ) {
      return { kind: 'protocol', message: e.message };
    }
    if ((e.code === METHOD_NOT_FOUND || e.code === UNSUPPORTED_OPERATION) && ctx.extensionCall) {
      return { kind: 'unavailable', message: e.message };
    }
    if (status >= 500 || e.code === INTERNAL_ERROR) {
      return { kind: 'transient', message: e.message };
    }
    return { kind: 'protocol', message: e.message };
  }
  if (e instanceof TypeError) return { kind: 'transient', message: networkMessage(e) };
  return { kind: 'protocol', message: e instanceof Error ? e.message : String(e) };
}

/** The one line a UI prints for a failure. */
export function describe(f: Failure): string {
  const line = (s: string): string => s.replace(/\s*[\r\n]+\s*/g, ' ');
  switch (f.kind) {
    case 'unauthenticated':
    case 'forbidden':
      return line(f.message);
    case 'incompatible':
      return line(`incompatible agent: ${f.message}`);
    case 'rate-limited': {
      const wait = f.retryAfterMs !== undefined ? ` — retrying in ${Math.ceil(f.retryAfterMs / 1000)}s` : '';
      return line(`rate limited: ${f.message}${wait}`);
    }
    case 'protocol':
      return line(`protocol error: ${f.message}`);
    case 'unavailable':
      return line(`not offered by this agent: ${f.message}`);
    case 'transient':
      return line(`${f.message} — retrying`);
  }
}
