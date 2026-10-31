// SPDX-License-Identifier: AGPL-3.0-only
// A stand-in for the home agent (demo (e)): the A2A agent the climate
// assistant asks to move the curtains. Built on the official A2A JavaScript
// SDK (@a2a-js/sdk, A2A 1.0): its Agent Card at /.well-known/agent-card.json,
// and JSON-RPC (SendMessage, GetTask, …) at /, served by the SDK's own
// request handler. It understands one kind of request — open or close a
// room's curtains — answers in a sentence, and logs what it was asked.
// Fictional, like the flat.
//
//   node home-agent.mjs [--port 8744] [--token-file <file>]
//
// With --token-file (or HOME_PEER_TOKEN), a request must carry that bearer:
// the document's rule "NEVER: take a request from an agent you don't know".
//   POST /control/state   what it was asked, and the curtains now
import { createServer } from 'node:http';
import { readFileSync } from 'node:fs';
import { randomUUID } from 'node:crypto';
import { AgentCard, A2A_VERSION_HEADER, Extensions, HTTP_EXTENSION_HEADER, Role } from '@a2a-js/sdk';
import {
  AgentEvent,
  DefaultRequestHandler,
  InMemoryTaskStore,
  JsonRpcTransportHandler,
  UnauthenticatedUser,
  defaultServerCallContextBuilder,
  validateVersion,
} from '@a2a-js/sdk/server';
import { arg, logger } from './lib.mjs';

const port = Number(arg('port', process.env.HOME_AGENT_PORT || 8744));
const tokenFile = arg('token-file');
const token = tokenFile ? readFileSync(tokenFile, 'utf8').trim() : process.env.HOME_PEER_TOKEN || '';
const base = `http://127.0.0.1:${port}`;
const log = logger('home-agent');
const curtains = { living: 'open', bedroom: 'open' };
const asked = [];

const card = {
  name: 'Home agent',
  description: 'Moves the curtains and the blinds and keeps the lights in the flat (a stand-in; fictional).',
  supportedInterfaces: [{ url: base, protocolBinding: 'JSONRPC', protocolVersion: '1.0', tenant: '' }],
  provider: undefined,
  version: '1.0.0',
  capabilities: { streaming: false, pushNotifications: false, extensions: [] },
  securitySchemes: {},
  securityRequirements: [],
  defaultInputModes: ['text/plain'],
  defaultOutputModes: ['text/plain'],
  skills: [{
    id: 'curtains', name: 'Curtains and blinds',
    description: 'Open or close the living room or bedroom curtains.',
    tags: ['home', 'curtains'], examples: ['Close the living room curtains.'],
    inputModes: [], outputModes: [], securityRequirements: [],
  }],
  signatures: [],
};

/** What to do with one request: the room and the move, read from its text. */
function act(text) {
  const room = /bed/i.test(text) ? 'bedroom' : 'living';
  const move = /open/i.test(text) ? 'open' : /close|shut|draw/i.test(text) ? 'closed' : null;
  if (!move) return `I can open or close the curtains; I did not understand "${text}".`;
  const before = curtains[room];
  curtains[room] = move;
  log('curtains', { room, from: before, to: move });
  const name = room === 'living' ? 'living room' : 'bedroom';
  return before === move ? `The ${name} curtains were already ${move}.` : `Done: the ${name} curtains are ${move}.`;
}

const executor = {
  async execute(ctx, bus) {
    const text = (ctx.userMessage.parts ?? []).map((p) => (p.content?.$case === 'text' ? p.content.value : '')).join(' ').trim();
    asked.push({ at: new Date().toISOString(), text });
    log('asked', { text, contextId: ctx.contextId });
    const reply = act(text);
    bus.publish(AgentEvent.message({
      messageId: randomUUID(), contextId: ctx.contextId, taskId: '', role: Role.ROLE_AGENT,
      parts: [{ content: { $case: 'text', value: reply }, metadata: undefined, filename: '', mediaType: 'text/plain' }],
      metadata: undefined, extensions: [], referenceTaskIds: [],
    }));
    bus.finished();
  },
  async cancelTask() {},
};

const handler = new DefaultRequestHandler(card, new InMemoryTaskStore(), executor);
const jsonrpc = new JsonRpcTransportHandler(handler);

async function body(req) {
  const chunks = [];
  for await (const c of req) chunks.push(c);
  return Buffer.concat(chunks).toString('utf8');
}

const http = createServer(async (req, res) => {
  try {
    const host = (req.headers.host ?? '').replace(/:\d+$/, '');
    if (!['127.0.0.1', 'localhost', '[::1]'].includes(host)) return void res.writeHead(403).end();
    const url = new URL(req.url ?? '/', base);
    if (req.method === 'GET' && url.pathname === '/.well-known/agent-card.json') {
      res.writeHead(200, { 'content-type': 'application/json' }).end(JSON.stringify(AgentCard.toJSON(await handler.getAgentCard())));
      return;
    }
    if (req.method === 'POST' && url.pathname === '/control/state') {
      res.writeHead(200, { 'content-type': 'application/json' }).end(JSON.stringify({ curtains, asked }, null, 2) + '\n');
      return;
    }
    if (req.method !== 'POST' || url.pathname !== '/') return void res.writeHead(404).end();
    if (token && req.headers.authorization !== `Bearer ${token}`) {
      log('refused', { reason: 'an agent this home agent does not know' });
      return void res.writeHead(401, { 'www-authenticate': 'Bearer' }).end();
    }
    const raw = await body(req);
    const context = defaultServerCallContextBuilder({
      extensions: Extensions.parseServiceParameter(req.headers[HTTP_EXTENSION_HEADER.toLowerCase()]),
      user: new UnauthenticatedUser(),
      headers: req.headers,
      requestedVersion: req.headers[A2A_VERSION_HEADER.toLowerCase()] || undefined,
    });
    let out;
    try {
      validateVersion(context.requestedVersion, card, 'JSONRPC');
      out = await jsonrpc.handle(raw, context);
    } catch (e) {
      let id = null;
      try { id = JSON.parse(raw).id ?? null; } catch { /* not JSON */ }
      out = { jsonrpc: '2.0', id, error: JsonRpcTransportHandler.mapToJSONRPCError(e) };
    }
    if (typeof out?.[Symbol.asyncIterator] === 'function') {
      res.writeHead(200, { 'content-type': 'text/event-stream', 'cache-control': 'no-cache' });
      for await (const ev of out) res.write(`data: ${JSON.stringify(ev)}\n\n`);
      return void res.end();
    }
    res.writeHead(200, { 'content-type': 'application/json' }).end(JSON.stringify(out));
  } catch (e) {
    log('http.error', { message: String(e?.message ?? e) });
    if (!res.headersSent) res.writeHead(500);
    res.end();
  }
});
http.listen(port, '127.0.0.1', () => log('listening', { card: `${base}/.well-known/agent-card.json`, jsonrpc: base, auth: token ? 'bearer' : 'none' }));
for (const sig of ['SIGINT', 'SIGTERM']) process.once(sig, () => { http.closeAllConnections?.(); http.close(() => process.exit(0)); });
