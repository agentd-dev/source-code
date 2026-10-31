// SPDX-License-Identifier: AGPL-3.0-only
// A rehearsal stand-in for the instruction.md registry: serves ONE local
// instruction file as an MCP resource, the way the mirror serves a published
// instruction, so ap-agent.yaml follows it by the same path it follows the
// real registry — the resource's subscription and an MCP Events type. The
// recorded demo uses the mirror; this exists so the beat can be rehearsed
// with no registry, no account and no network.
//
//   node registry.mjs --file /path/to/ap-policy.md [--port 8703]
//                     [--uri instruction://northwind/ap-approval-policy@latest]
//
// "Releasing a version" is replacing the file's content (cp ap-policy-v2.md
// over it). The stand-in sees the change, gives it the next version id, and
// tells subscribers: `notifications/resources/updated` on the subscription,
// and an `md.instruction.activity` occurrence.
import { readFileSync, watchFile } from 'node:fs';
import { createHash } from 'node:crypto';
import { McpServer } from '@modelcontextprotocol/server';
import { arg, EventType, logger, serve } from './lib.mjs';

const port = Number(arg('port', process.env.REGISTRY_PORT || 8703));
const file = arg('file', process.env.AP_INSTRUCTION_FILE);
const uri = arg('uri', 'instruction://northwind/ap-approval-policy@latest');
if (!file) {
  console.error('registry.mjs: --file <instruction.md> is required');
  process.exit(2);
}
const log = logger('registry');

let current = { text: '', digest: '', number: 0 };
function load() {
  const text = readFileSync(file, 'utf8');
  const digest = createHash('sha256').update(text).digest('hex');
  if (digest === current.digest) return false;
  current = { text, digest, number: current.number + 1 };
  return true;
}
load();
const versionId = () => `ver_local_${current.number}_${current.digest.slice(0, 8)}`;

const activity = new EventType({
  name: 'md.instruction.activity',
  description: 'Something happened to an instruction this caller can read: a release moved a ref.',
  delivery: ['poll'],
  inputSchema: { type: 'object', properties: {} },
  payloadSchema: { type: 'object', properties: { uri: { type: 'string' }, versionId: { type: 'string' }, number: { type: 'integer' } } },
}, log);

function factory() {
  const mcp = new McpServer(
    { name: 'instruction-registry-standin', version: '1.0.0' },
    { capabilities: { resources: { subscribe: true }, extensions: EventType.capability() } },
  );
  mcp.registerResource('ap-approval-policy', uri, { title: 'Accounts payable approval policy', mimeType: 'text/markdown' },
    async () => {
      log('resources/read', { uri, versionId: versionId() });
      return {
        contents: [{
          uri, mimeType: 'text/markdown', text: current.text,
          // What the registry says about the version it served; agentd logs
          // the move from one versionId to the next as `instruction.applied`.
          _meta: { 'md.instruction/versionId': versionId(), 'md.instruction/revision': current.number },
        }],
      };
    });
  activity.attach(mcp);
  return mcp;
}

const { handler } = await serve({
  name: 'registry',
  port,
  factory,
  control: { state: async () => ({ uri, versionId: versionId(), bytes: current.text.length }) },
});
log('serving', { uri, file, versionId: versionId() });

watchFile(file, { interval: 250 }, () => {
  try {
    if (!load()) return;
  } catch {
    return; // mid-copy: the next tick reads it whole
  }
  log('released', { uri, versionId: versionId(), number: current.number });
  activity.append(`act-${current.number}-${current.digest.slice(0, 8)}`, { uri, versionId: versionId(), number: current.number });
  handler.notify.resourceUpdated(uri);
});
