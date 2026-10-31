// SPDX-License-Identifier: AGPL-3.0-only
// What both stand-ins share: serving an MCP handler from the official SDK on
// 127.0.0.1 over Streamable HTTP, a small JSON control surface beside it for
// the live beats, and one log line per thing that happened.
//
// Each server is two paths on one port:
//   /mcp        the MCP endpoint agentd connects to (MCP 2026-07-28, with the
//               SDK's stateless fallback for 2025-era clients)
//   /control/*  POST JSON, for the presenter: inject an invoice, add a bank
//               line. Not MCP; agentd never calls it.
import { createServer } from 'node:http';
import { readFileSync } from 'node:fs';
import {
  createMcpHandler,
  hostHeaderValidationResponse,
  localhostAllowedHostnames,
  ProtocolError,
} from '@modelcontextprotocol/server';
import { z } from 'zod';

/** The value after `--<name>` on the command line, else `fallback`. */
export function arg(name, fallback) {
  const at = process.argv.indexOf(`--${name}`);
  return at > 0 && at + 1 < process.argv.length ? process.argv[at + 1] : fallback;
}

/** One line per event, prefixed with the server's name, on stdout. */
export function logger(name) {
  return (what, detail = {}) => {
    const extra = Object.entries(detail)
      .map(([k, v]) => `${k}=${typeof v === 'string' ? v : JSON.stringify(v)}`)
      .join(' ');
    console.log(`${new Date().toISOString()} [${name}] ${what}${extra ? ' ' + extra : ''}`);
  };
}

/** A fixture file next to this one, parsed. */
export function fixture(file) {
  return JSON.parse(readFileSync(new URL(`./fixtures/${file}`, import.meta.url), 'utf8'));
}

/** A tool result: the JSON as text, and the same value as structured content. */
export function result(value) {
  return { content: [{ type: 'text', text: JSON.stringify(value, null, 2) }], structuredContent: value };
}

/** A tool result the model reads as a refusal. */
export function refusal(text) {
  return { isError: true, content: [{ type: 'text', text }] };
}

/**
 * The server side of MCP Events (SEP-3415, a draft extension), poll delivery,
 * for one event type. The SDK does not implement the extension, so its two
 * methods are custom requests:
 *
 *   events/list  → {events: [descriptor]}
 *   events/poll  {name, arguments, cursor, maxAgeMs?, maxEvents?}
 *                → {events, cursor, truncated, hasMore, nextPollMs}
 *
 * A cursor is "<boot>.<seq>": this process's start, and the last occurrence
 * the client holds. A poll with no cursor starts from now, or `maxAgeMs` back
 * when the client asks to catch up after downtime. The log lives in memory,
 * so a cursor from an earlier start of the stand-in names occurrences that
 * are gone: it is answered from the beginning with `truncated: true`, and the
 * ids carry the boot too, so a re-run invoice is not taken for a duplicate.
 */
export class EventType {
  static EXTENSION = 'io.modelcontextprotocol/events';
  static BOOT = Date.now().toString(36);

  constructor(descriptor, log) {
    this.descriptor = descriptor;
    this.log = log;
    this.occurrences = []; // {seq, eventId, name, timestamp, data}
  }

  get head() {
    return this.occurrences.at(-1)?.seq ?? 0;
  }

  /** Record one occurrence; `id` is made unique to this start of the server. */
  append(id, data) {
    const seq = this.head + 1;
    const eventId = `${id}-${EventType.BOOT}`;
    this.occurrences.push({ seq, eventId, name: this.descriptor.name, timestamp: new Date().toISOString(), data });
    return seq;
  }

  /** The capability to merge into the server's `capabilities.extensions`. */
  static capability() {
    return { [EventType.EXTENSION]: { listChanged: false } };
  }

  poll(params) {
    if (params.name !== this.descriptor.name) throw new ProtocolError(-32023, 'Not found', { kind: 'event' });
    const allowed = Object.keys(this.descriptor.inputSchema?.properties ?? {});
    if (Object.keys(params.arguments ?? {}).some((k) => !allowed.includes(k))) {
      throw new ProtocolError(-32602, 'arguments do not match inputSchema');
    }
    const max = Math.max(1, Math.min(Number(params.maxEvents ?? 50), 100));
    let from;
    let truncated = false;
    if (params.cursor == null) {
      const since = params.maxAgeMs ? Date.now() - Number(params.maxAgeMs) : Infinity;
      const first = this.occurrences.find((o) => Date.parse(o.timestamp) >= since);
      from = first ? first.seq - 1 : this.head;
    } else {
      const [boot, seq] = String(params.cursor).split('.');
      if (boot === EventType.BOOT) {
        from = Number(seq) || 0;
      } else {
        from = 0;
        truncated = true;
      }
    }
    const after = this.occurrences.filter((o) => o.seq > from);
    const page = after.slice(0, max);
    if (page.length) this.log('events/poll', { type: this.descriptor.name, delivered: page.map((o) => o.eventId) });
    return {
      events: page.map(({ seq, ...o }) => o),
      cursor: `${EventType.BOOT}.${page.at(-1)?.seq ?? from}`,
      truncated,
      hasMore: after.length > page.length,
      nextPollMs: 500,
    };
  }

  /** Register events/list and events/poll on one McpServer instance. */
  attach(mcp) {
    mcp.server.setRequestHandler('events/list', { params: z.object({ cursor: z.string().nullish() }).passthrough() },
      async () => ({ events: [this.descriptor] }));
    mcp.server.setRequestHandler('events/poll', {
      params: z.object({
        name: z.string(), arguments: z.record(z.string(), z.unknown()).optional(),
        cursor: z.string().nullish(), maxAgeMs: z.number().optional(), maxEvents: z.number().optional(),
      }).passthrough(),
    }, async (params) => this.poll(params));
  }
}

async function body(req) {
  const chunks = [];
  for await (const c of req) chunks.push(c);
  return Buffer.concat(chunks);
}

/**
 * Serve `factory` (the SDK's per-request server factory) at
 * http://127.0.0.1:<port>/mcp and `control` (path → async (json) => json)
 * at /control/<path>. Resolves with `{handler, close}` once listening.
 */
export async function serve({ name, port, factory, control }) {
  const log = logger(name);
  const handler = createMcpHandler(factory, {
    onerror: (e) => log('mcp.error', { message: e.message }),
  });
  const http = createServer(async (req, res) => {
    try {
      const url = new URL(req.url ?? '/', 'http://127.0.0.1');
      // DNS-rebinding guard: a loopback server answers loopback Host names only.
      const host = (req.headers.host ?? '').replace(/:\d+$/, '');
      if (!localhostAllowedHostnames().includes(host)) return void res.writeHead(403).end();
      if (url.pathname.startsWith('/control/')) {
        if (req.method !== 'POST') return void res.writeHead(405).end();
        const fn = control[url.pathname.slice('/control/'.length)];
        if (!fn) return void res.writeHead(404, { 'content-type': 'application/json' }).end('{"error":"no such control"}');
        const raw = (await body(req)).toString('utf8');
        const out = await fn(raw ? JSON.parse(raw) : {});
        res.writeHead(200, { 'content-type': 'application/json' }).end(JSON.stringify(out, null, 2) + '\n');
        return;
      }
      if (url.pathname !== '/mcp') return void res.writeHead(404).end();
      // Node's request, as the web-standard Request the SDK's handler takes.
      const ac = new AbortController();
      res.on('close', () => ac.abort());
      const hasBody = req.method !== 'GET' && req.method !== 'HEAD';
      const request = new Request(new URL(url.pathname + url.search, `http://${req.headers.host}`), {
        method: req.method,
        headers: Object.entries(req.headers).flatMap(([k, v]) =>
          v === undefined ? [] : Array.isArray(v) ? v.map((x) => [k, x]) : [[k, v]]),
        body: hasBody ? await body(req) : undefined,
        signal: ac.signal,
      });
      // The SDK's own check as well, on the request it serves.
      const rejected = hostHeaderValidationResponse(request, localhostAllowedHostnames());
      const response = rejected ?? (await handler.fetch(request));
      res.writeHead(response.status, Object.fromEntries(response.headers));
      if (!response.body) return void res.end();
      const reader = response.body.getReader();
      for (;;) {
        const { done, value } = await reader.read();
        if (done) break;
        res.write(value);
      }
      res.end();
    } catch (e) {
      if (e?.name === 'AbortError') return;
      log('http.error', { message: String(e?.message ?? e) });
      if (!res.headersSent) res.writeHead(500);
      res.end();
    }
  });
  await new Promise((resolve, reject) => {
    http.once('error', reject);
    // Loopback only: these are demo servers with no authentication.
    http.listen(port, '127.0.0.1', resolve);
  });
  log('listening', { mcp: `http://127.0.0.1:${port}/mcp`, control: `http://127.0.0.1:${port}/control/` });
  const close = async () => {
    await handler.close();
    http.closeAllConnections?.();
    await new Promise((r) => http.close(r));
  };
  for (const sig of ['SIGINT', 'SIGTERM']) process.once(sig, () => close().finally(() => process.exit(0)));
  return { handler, close, log };
}
