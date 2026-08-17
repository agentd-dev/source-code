// SPDX-License-Identifier: AGPL-3.0-only
/**
 * The A2A wire: JSON-RPC 2.0 over HTTP POST, and SSE over the POST response
 * for the streaming methods. fetch-based on purpose — `EventSource` cannot
 * POST or send `Authorization`, so both node (>=20) and browsers ride the
 * same code path here.
 */

import { Endpoint, Json, RpcError } from './types.js';

let nextId = 1;

function headers(ep: Endpoint): Record<string, string> {
  const h: Record<string, string> = { 'content-type': 'application/json' };
  if (ep.bearer) h.authorization = `Bearer ${ep.bearer}`;
  return h;
}

/** One unary JSON-RPC call; returns `result` or throws {@link RpcError}. */
export async function rpc(ep: Endpoint, method: string, params: Json): Promise<Json> {
  const body = JSON.stringify({ jsonrpc: '2.0', id: nextId++, method, params });
  const res = await fetch(ep.url, { method: 'POST', headers: headers(ep), body });
  if (!res.ok) throw new RpcError(-res.status, `HTTP ${res.status} from ${ep.url}`);
  const v = (await res.json()) as { result?: Json; error?: { code: number; message: string } };
  if (v.error) throw new RpcError(v.error.code, v.error.message);
  return v.result ?? null;
}

/**
 * Parse an SSE byte stream into `data:` payloads. Handles chunk boundaries,
 * multi-line `data:` fields, and comment keep-alives (`: keep-alive`).
 * Exported for tests.
 */
export function sseParser(onData: (payload: string) => void): (chunk: string) => void {
  let buf = '';
  return (chunk: string) => {
    buf += chunk;
    // Events are separated by a blank line.
    for (;;) {
      const cut = buf.indexOf('\n\n');
      if (cut < 0) break;
      const raw = buf.slice(0, cut);
      buf = buf.slice(cut + 2);
      const data = raw
        .split('\n')
        .filter((l) => l.startsWith('data:'))
        .map((l) => l.slice(5).trimStart())
        .join('\n');
      if (data.length > 0) onData(data);
    }
  };
}

/** What a streaming call yields: each frame's JSON-RPC `result` or `error`. */
export interface StreamFrame {
  result?: Json;
  error?: { code: number; message: string };
}

/**
 * A streaming JSON-RPC call (`SendStreamingMessage` / `SubscribeToTask` /
 * `SubscribeToEvents`): POST, then consume the `text/event-stream` response.
 * `onFrame` fires per frame; the promise resolves when the server closes the
 * stream (the last frame is the terminal one) and rejects on transport errors.
 * Abort via the signal.
 */
export async function rpcStream(
  ep: Endpoint,
  method: string,
  params: Json,
  onFrame: (frame: StreamFrame) => void,
  signal?: AbortSignal,
): Promise<void> {
  const body = JSON.stringify({ jsonrpc: '2.0', id: nextId++, method, params });
  const res = await fetch(ep.url, { method: 'POST', headers: headers(ep), body, signal });
  if (!res.ok) throw new RpcError(-res.status, `HTTP ${res.status} from ${ep.url}`);
  const ctype = res.headers.get('content-type') ?? '';
  if (!ctype.includes('text/event-stream')) {
    // The server answered unary (e.g. an error before the upgrade).
    const v = (await res.json()) as StreamFrame;
    onFrame(v);
    return;
  }
  if (!res.body) throw new RpcError(-1, 'no response body');
  const reader = res.body.getReader();
  const decoder = new TextDecoder();
  const feed = sseParser((payload) => {
    try {
      onFrame(JSON.parse(payload) as StreamFrame);
    } catch {
      // A malformed frame is dropped; the stream carries on.
    }
  });
  for (;;) {
    const { done, value } = await reader.read();
    if (done) break;
    feed(decoder.decode(value, { stream: true }));
  }
}
