// SPDX-License-Identifier: AGPL-3.0-only
/**
 * The A2A wire: JSON-RPC 2.0 over HTTP POST, and SSE over the POST response
 * for the streaming methods. fetch-based on purpose — `EventSource` cannot
 * POST or send `Authorization`, so both node (>=20) and browsers ride the
 * same code path here.
 *
 * This layer knows the transport and nothing else: the protocol version, the
 * tenant, the headers, bounded reads, the error envelope and the extension
 * echo. What a method means is the caller's business.
 */

import { ClientError, Endpoint, Json, RpcError } from './types.js';
import { parseChallenge, parseRetryAfter } from './errors.js';

/**
 * The A2A protocol version this client speaks. It goes on every JSON-RPC
 * request as `A2A-Version`: a missing header means 0.3 to a 1.0 server, which
 * refuses it with -32009. Interface selection compares against the same value.
 */
export const A2A_VERSION = '1.0';

let nextId = 1;

/**
 * The A2A extensions agentd declares on its card. A client names the ones it
 * intends to use in `A2A-Extensions`; the response echoes what was actually
 * activated. Neither is `required`, so sending the header is an announcement,
 * not a precondition — but announcing lets the peer confirm the vocabulary
 * instead of inferring it from the payload.
 */
export const COMMAND_EXTENSION = 'https://agentd.dev/a2a/ext/command/v1';
export const INTERFACE_EXTENSION = 'https://agentd.dev/a2a/ext/interface/v1';

/** A unary success body is capped here; a larger one is not a JSON-RPC reply we can use. */
const MAX_BODY = 16 << 20;
/** An error body is only read for its envelope or a line of text, so far less is kept. */
const MAX_ERROR_BODY = 64 << 10;

/** Per-call transport options. */
export interface CallOptions {
  /** Extension URIs to activate (`A2A-Extensions`). */
  exts?: readonly string[];
  /**
   * The subset of `exts` the result depends on. When the server echoes
   * `A2A-Extensions` and a required URI is missing from it, the result is
   * refused with `ClientError('extension-not-activated')`.
   */
  require?: readonly string[];
  signal?: AbortSignal;
  /** Resume a stream after this SSE id (`Last-Event-ID`). */
  lastEventId?: string;
}

function headers(ep: Endpoint, accept: string, o: CallOptions): Record<string, string> {
  const h: Record<string, string> = { 'content-type': 'application/json', accept };
  h['a2a-version'] = A2A_VERSION;
  if (ep.bearer) h.authorization = `Bearer ${ep.bearer}`;
  if (o.exts && o.exts.length > 0) h['a2a-extensions'] = o.exts.join(', ');
  if (o.lastEventId !== undefined) h['last-event-id'] = o.lastEventId;
  return h;
}

/**
 * Write the interface's `tenant` into a request's params. An interface that
 * declares a tenant must be addressed with exactly that value, so it is done
 * once here rather than at every call site, where one could forget it.
 */
export function withTenant(params: Json, tenant?: string): Json {
  if (tenant === undefined) return params;
  if (params === null || typeof params !== 'object' || Array.isArray(params)) return params;
  return { ...params, tenant };
}

/**
 * Parse an `A2A-Extensions` header into its URIs: comma-split, trimmed,
 * empties dropped, duplicates removed. `null` when the header is absent —
 * which is different from an empty echo: absent means the server said
 * nothing (the echo is only a SHOULD), empty means it activated nothing.
 */
export function parseExtensionHeader(v: string | null): string[] | null {
  if (v === null) return null;
  const out: string[] = [];
  for (const raw of v.split(',')) {
    const t = raw.trim();
    if (t.length > 0 && !out.includes(t)) out.push(t);
  }
  return out;
}

/**
 * Refuse a result the server did not produce under an extension the call
 * depends on. A present echo that lacks, say, the command extension means the
 * peer did not run our payload as a command — an LLM agent might have read it
 * as prose — so its answer must not be trusted as a command result.
 */
function verifyEcho(echo: string[] | null, o: CallOptions): void {
  if (echo === null) return; // advisory: the card governs
  for (const uri of o.require ?? []) {
    if (!echo.includes(uri)) {
      throw new ClientError('extension-not-activated', `the agent did not activate ${uri} for this call`);
    }
  }
}

/** Read at most `cap` bytes of a body as text; `over` reports whether more was there. */
async function readCapped(res: Response, cap: number): Promise<{ text: string; over: boolean }> {
  if (!res.body) return { text: '', over: false };
  const reader = res.body.getReader();
  const decoder = new TextDecoder();
  let text = '';
  let size = 0;
  for (;;) {
    const { done, value } = await reader.read();
    if (done) break;
    size += value.byteLength;
    if (size > cap) {
      // Stop reading: a server that streams forever must not grow our memory.
      await reader.cancel().catch(() => {});
      text += decoder.decode(value.subarray(0, value.byteLength - (size - cap)), { stream: true });
      return { text: text + decoder.decode(), over: true };
    }
    text += decoder.decode(value, { stream: true });
  }
  return { text: text + decoder.decode(), over: false };
}

/** What a JSON-RPC error object looks like on the wire. */
interface ErrorObject {
  code: number;
  message: string;
  data?: Json;
}

function isErrorObject(v: unknown): v is ErrorObject {
  if (v === null || typeof v !== 'object') return false;
  const o = v as { [k: string]: unknown };
  return typeof o.code === 'number' && typeof o.message === 'string';
}

/**
 * The error a response carries, with everything the caller might act on: the
 * JSON-RPC data (the `@type`d google.rpc details), the HTTP status, the parsed
 * `WWW-Authenticate` challenge and `Retry-After`. Dropping any of these was
 * how a revoked session and a rate limit both used to read as "HTTP 401".
 */
function rpcErrorOf(err: ErrorObject, res: Response): RpcError {
  return new RpcError(err.code, err.message, {
    data: err.data,
    status: res.status,
    challenge: parseChallenge(res.headers.get('www-authenticate')),
    retryAfterMs: parseRetryAfter(res.headers.get('retry-after')),
  });
}

/**
 * A non-2xx answer. A JSON-RPC envelope in the body wins (the listener sends
 * 401/403/429 as JSON-RPC errors); anything else keeps `code = -status` and
 * the first line of whatever text came with it.
 */
async function httpError(ep: Endpoint, res: Response): Promise<RpcError> {
  const { text } = await readCapped(res, MAX_ERROR_BODY);
  try {
    const v = JSON.parse(text) as { error?: unknown };
    if (v && typeof v === 'object' && isErrorObject(v.error)) return rpcErrorOf(v.error, res);
  } catch {
    /* not JSON: fall through to the text */
  }
  const snippet = text.trim().slice(0, 200);
  return new RpcError(-res.status, `HTTP ${res.status} from ${ep.url}${snippet ? `: ${snippet}` : ''}`, {
    status: res.status,
    challenge: parseChallenge(res.headers.get('www-authenticate')),
    retryAfterMs: parseRetryAfter(res.headers.get('retry-after')),
  });
}

async function post(ep: Endpoint, method: string, params: Json, accept: string, o: CallOptions) {
  const id = nextId++;
  const body = JSON.stringify({ jsonrpc: '2.0', id, method, params: withTenant(params, ep.tenant) });
  const res = await fetch(ep.url, {
    method: 'POST',
    headers: headers(ep, accept, o),
    body,
    signal: o.signal,
    // A JSON-RPC endpoint never redirects; following one would carry the
    // credential somewhere the operator never named. No ambient cookies, and
    // nothing cached: every answer here is live state.
    redirect: 'error',
    credentials: 'omit',
    cache: 'no-store',
  });
  return { id, res };
}

/**
 * Parse a unary JSON-RPC reply. An error envelope becomes {@link RpcError};
 * anything that is not a JSON-RPC 2.0 reply to THIS request becomes
 * `ClientError('invalid-response')`.
 */
function envelopeOf(text: string, id: number, res: Response): { result: Json } {
  let v: unknown;
  try {
    v = JSON.parse(text);
  } catch {
    throw new ClientError('invalid-response', `the agent's reply is not JSON (HTTP ${res.status})`);
  }
  if (v === null || typeof v !== 'object' || Array.isArray(v)) {
    throw new ClientError('invalid-response', 'the agent replied with something other than a JSON-RPC object');
  }
  const o = v as { [k: string]: unknown };
  if (o.jsonrpc !== '2.0') throw new ClientError('invalid-response', 'the agent replied without jsonrpc "2.0"');
  if (o.error !== undefined) {
    // An error may carry `id: null` (a parse or envelope refusal cannot know
    // the id), so only a DIFFERENT id marks it as someone else's reply.
    if (o.id !== id && o.id !== null) {
      throw new ClientError('invalid-response', `the agent answered request ${String(o.id)}, not ${id}`);
    }
    if (!isErrorObject(o.error)) throw new ClientError('invalid-response', 'the agent sent a malformed error object');
    throw rpcErrorOf(o.error, res);
  }
  if (o.id !== id) throw new ClientError('invalid-response', `the agent answered request ${String(o.id)}, not ${id}`);
  if (!('result' in o)) throw new ClientError('invalid-response', 'the agent replied with neither result nor error');
  return { result: (o.result ?? null) as Json };
}

/**
 * One unary JSON-RPC call. Returns the result with the extension echo and the
 * HTTP status; throws {@link RpcError} for an error reply and
 * {@link ClientError} for a reply that cannot be trusted.
 */
export async function call(
  ep: Endpoint,
  method: string,
  params: Json,
  o: CallOptions = {},
): Promise<{ result: Json; echo: string[] | null; status: number }> {
  const { id, res } = await post(ep, method, params, 'application/json', o);
  if (!res.ok) throw await httpError(ep, res);
  const { text, over } = await readCapped(res, MAX_BODY);
  if (over) throw new ClientError('invalid-response', `the agent's reply exceeds ${MAX_BODY >> 20} MiB`);
  const { result } = envelopeOf(text, id, res);
  const echo = parseExtensionHeader(res.headers.get('a2a-extensions'));
  verifyEcho(echo, o);
  return { result, echo, status: res.status };
}

/** One unary JSON-RPC call; returns `result` or throws (see {@link call}). */
export async function rpc(ep: Endpoint, method: string, params: Json, o: CallOptions = {}): Promise<Json> {
  return (await call(ep, method, params, o)).result;
}

/** One dispatched SSE event. */
export interface SseEvent {
  data: string;
  /** The last event ID in effect (it persists across events, per WHATWG). */
  id?: string;
  /** The `event:` type, when the server named one. */
  event?: string;
}

/**
 * Parse an SSE character stream into events, following the WHATWG "parsing
 * an event stream" rules. Exported for tests.
 *
 * - A line ends at CRLF, LF or a lone CR. A CR is acted on at once — so a
 *   CR-only terminator dispatches without waiting for more input — and a flag
 *   drops the LF that may open the next chunk, so a CRLF split across chunks
 *   counts once. The official Python SDK (sse-starlette) frames with CRLF.
 * - A leading U+FEFF is dropped. Lines starting with ':' are comments.
 * - `field:value` loses EXACTLY one leading space; a line with no colon is a
 *   field with an empty value. `data` lines join with '\n'; `id` persists
 *   unless it contains U+0000; `event` is recorded; `retry` is ignored.
 * - An event whose data is empty is not dispatched. (WHATWG would fire an
 *   empty MessageEvent; no JSON-RPC frame can be empty, so it carries nothing.)
 * - One event is capped at `maxEvent` characters, so a server that never
 *   sends a blank line cannot grow our memory without bound.
 * - An unterminated tail is never dispatched: the caller simply stops feeding.
 */
export function sseParser(onEvent: (ev: SseEvent) => void, maxEvent = 8 << 20): (chunk: string) => void {
  let line = ''; // the current, unterminated line
  let data: string[] = [];
  let size = 0; // characters held for the event being built
  let event = '';
  let lastId = '';
  let skipLF = false; // the previous chunk ended in '\r': a leading '\n' completes that CRLF
  let first = true;

  const field = (l: string): void => {
    if (l === '') {
      const payload = data.join('\n');
      const type = event;
      data = [];
      event = '';
      size = 0;
      if (payload.length > 0) {
        onEvent({ data: payload, id: lastId === '' ? undefined : lastId, event: type === '' ? undefined : type });
      }
      return;
    }
    if (l.startsWith(':')) return; // a comment (keep-alive)
    const colon = l.indexOf(':');
    const name = colon < 0 ? l : l.slice(0, colon);
    let value = colon < 0 ? '' : l.slice(colon + 1);
    if (value.startsWith(' ')) value = value.slice(1); // exactly one space
    if (name === 'data') {
      data.push(value);
      size += value.length + 1;
    } else if (name === 'event') event = value;
    else if (name === 'id') {
      if (!value.includes('\0')) lastId = value;
    }
    // `retry` and unknown fields are ignored.
  };

  return (chunk: string) => {
    if (first && chunk.length > 0) {
      first = false;
      if (chunk.charCodeAt(0) === 0xfeff) chunk = chunk.slice(1);
    }
    let i = 0;
    if (skipLF && chunk.startsWith('\n')) i = 1;
    if (chunk.length > 0) skipLF = false;
    const eol = /[\r\n]/g;
    for (;;) {
      eol.lastIndex = i;
      const m = eol.exec(chunk);
      if (!m) break;
      const j = m.index;
      const l = line + chunk.slice(i, j);
      line = '';
      if (size + l.length > maxEvent) throw new RpcError(-1, 'SSE event exceeds cap');
      field(l);
      if (chunk[j] === '\r') {
        if (j + 1 === chunk.length) {
          skipLF = true;
          i = j + 1;
        } else i = chunk[j + 1] === '\n' ? j + 2 : j + 1;
      } else i = j + 1;
    }
    line += chunk.slice(i);
    if (size + line.length > maxEvent) throw new RpcError(-1, 'SSE event exceeds cap');
  };
}

/** Deliver a streamed JSON-RPC frame, or throw what it carries. */
function frameOf(ev: SseEvent, res: Response): Json {
  let v: unknown;
  try {
    v = JSON.parse(ev.data);
  } catch {
    throw new ClientError('invalid-response', 'the agent streamed a frame that is not JSON');
  }
  if (v === null || typeof v !== 'object' || Array.isArray(v)) {
    throw new ClientError('invalid-response', 'the agent streamed a frame that is not a JSON-RPC object');
  }
  // A frame's `id` is not checked: the stream itself is the answer to our
  // request, and the frame adds nothing a mismatch could protect.
  const o = v as { [k: string]: unknown };
  if (o.error !== undefined) {
    if (!isErrorObject(o.error)) {
      throw new ClientError('invalid-response', 'the agent streamed a malformed error object');
    }
    throw rpcErrorOf(o.error, res);
  }
  if (!('result' in o)) {
    throw new ClientError('invalid-response', 'the agent streamed a frame with neither result nor error');
  }
  return (o.result ?? null) as Json;
}

/**
 * A streaming JSON-RPC call (`SendStreamingMessage` / `SubscribeToTask` /
 * `SubscribeToEvents`): POST, then consume the `text/event-stream` response.
 * `onFrame` fires per frame's `result` with the SSE id in effect (for a
 * `Last-Event-ID` resume); the promise resolves with the echo when the server
 * closes the stream.
 *
 * Errors are never swallowed: an error answered as plain JSON before the
 * stream, or an error frame inside it, rejects the promise (and cancels the
 * body). A caller that sees the promise resolve saw the stream end cleanly.
 */
export async function rpcStream(
  ep: Endpoint,
  method: string,
  params: Json,
  onFrame: (result: Json, sseId?: string) => void,
  o: CallOptions = {},
): Promise<{ echo: string[] | null }> {
  const { id, res } = await post(ep, method, params, 'text/event-stream', o);
  if (!res.ok) throw await httpError(ep, res);
  const echo = parseExtensionHeader(res.headers.get('a2a-extensions'));
  const ctype = res.headers.get('content-type') ?? '';
  if (!ctype.includes('text/event-stream')) {
    // The server answered unary: an error before the stream opened, or a
    // single result.
    const { text, over } = await readCapped(res, MAX_BODY);
    if (over) throw new ClientError('invalid-response', `the agent's reply exceeds ${MAX_BODY >> 20} MiB`);
    const { result } = envelopeOf(text, id, res);
    verifyEcho(echo, o);
    onFrame(result);
    return { echo };
  }
  if (!res.body) throw new ClientError('invalid-response', 'the agent opened a stream with no body');
  const reader = res.body.getReader();
  try {
    // Checked before a single frame is read: nothing from an unactivated
    // extension reaches the caller.
    verifyEcho(echo, o);
    const decoder = new TextDecoder();
    const feed = sseParser((ev) => onFrame(frameOf(ev, res), ev.id));
    for (;;) {
      const { done, value } = await reader.read();
      if (done) break;
      feed(decoder.decode(value, { stream: true }));
    }
    // Flush a multi-byte character split across the last chunk boundary.
    feed(decoder.decode());
  } catch (e) {
    await reader.cancel().catch(() => {});
    throw e;
  }
  return { echo };
}
