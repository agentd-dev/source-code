// SPDX-License-Identifier: AGPL-3.0-only
// A stand-in for an ERP's accounts-payable module ("payables"):
// invoices and vendors as MCP tools, and new invoices as an MCP Events stream.
// Every name and number in fixtures/payables.json is fictional.
//
//   node payables.mjs [--port 8701]
//
// MCP (http://127.0.0.1:8701/mcp):
//   tools   invoices.list, invoices.get, vendors.get, invoices.approve, invoices.hold
//   events  invoice.created — MCP Events (SEP-3415, a draft), poll delivery
// Control (POST http://127.0.0.1:8701/control/<name>, JSON; or control.mjs):
//   invoice  {}                        the next scripted invoice arrives
//            {"invoiceId": "INV-2043"}  that scripted invoice arrives
//            {"vendorId": "V-1002", "amount": "18500.00", "memo": "…"}  a new one
//   state    {}                        every invoice and its status
import { McpServer } from '@modelcontextprotocol/server';
import { z } from 'zod';
import { arg, EventType, fixture, logger, refusal, result, serve } from './lib.mjs';

const port = Number(arg('port', process.env.PAYABLES_PORT || 8701));
const log = logger('payables');
const data = fixture('payables.json');
const invoices = new Map(data.invoices.map((i) => [i.invoiceId, i]));
const vendors = new Map(data.vendors.map((v) => [v.vendorId, v]));
const script = [...data.next];

const created = new EventType({
  name: 'invoice.created',
  description: 'A vendor invoice was entered in accounts payable and awaits a decision.',
  delivery: ['poll'],
  inputSchema: {
    type: 'object',
    additionalProperties: false,
    properties: { subsidiary: { type: 'string', description: 'Only invoices for this subsidiary' } },
  },
  payloadSchema: {
    type: 'object',
    required: ['invoiceId', 'vendorId', 'amount', 'currency'],
    properties: {
      invoiceId: { type: 'string' }, vendorId: { type: 'string' }, vendorName: { type: 'string' },
      amount: { type: 'string', description: 'decimal, two places' }, currency: { type: 'string' },
      dueOn: { type: 'string' }, poNumber: { type: 'string' }, memo: { type: 'string' },
    },
  },
}, log);

// The SDK builds one server instance per HTTP request; the state is this module's.
function factory() {
  const mcp = new McpServer(
    { name: 'payables-standin', version: '1.0.0', title: `Accounts payable — ${data.company.name}` },
    { capabilities: { tools: {}, extensions: EventType.capability() } },
  );
  const view = (i) => ({ ...i, vendorName: vendors.get(i.vendorId)?.name });

  mcp.registerTool('invoices.list', {
    description: 'List vendor invoices, optionally only those in one status (pending_approval, approved, on_hold).',
    inputSchema: z.object({ status: z.enum(['pending_approval', 'approved', 'on_hold']).optional() }),
    annotations: { readOnlyHint: true },
  }, async ({ status }) => result({ invoices: [...invoices.values()].filter((i) => !status || i.status === status).map(view) }));

  mcp.registerTool('invoices.get', {
    description: 'Read one vendor invoice: amount, vendor, PO number, whether the goods receipt matched, and its status.',
    inputSchema: z.object({ invoiceId: z.string() }),
    annotations: { readOnlyHint: true },
  }, async ({ invoiceId }) => {
    const i = invoices.get(invoiceId);
    return i ? result(view(i)) : refusal(`no invoice ${invoiceId}`);
  });

  mcp.registerTool('vendors.get', {
    description: 'Read one vendor: status, payment terms, and the date its bank details last changed.',
    inputSchema: z.object({ vendorId: z.string() }),
    annotations: { readOnlyHint: true },
  }, async ({ vendorId }) => {
    const v = vendors.get(vendorId);
    return v ? result(v) : refusal(`no vendor ${vendorId}`);
  });

  mcp.registerTool('invoices.approve', {
    description: 'Approve a pending invoice for payment in the next payment run.',
    inputSchema: z.object({ invoiceId: z.string(), note: z.string().optional() }),
    annotations: { idempotentHint: true },
  }, async ({ invoiceId, note }) => {
    const i = invoices.get(invoiceId);
    if (!i) return refusal(`no invoice ${invoiceId}`);
    if (i.status === 'approved') return result({ invoiceId, status: 'approved', message: `${invoiceId} was already approved` });
    Object.assign(i, { status: 'approved', note: note ?? '' });
    log('invoices.approve', { invoiceId, amount: `${i.currency} ${i.amount}`, note: note ?? '' });
    return result({ invoiceId, status: 'approved', message: `decision recorded: approved ${invoiceId}, in the next payment run` });
  });

  mcp.registerTool('invoices.hold', {
    description: 'Put an invoice on hold so it is not paid, with the reason a person will read.',
    inputSchema: z.object({ invoiceId: z.string(), reason: z.string() }),
    annotations: { idempotentHint: true },
  }, async ({ invoiceId, reason }) => {
    const i = invoices.get(invoiceId);
    if (!i) return refusal(`no invoice ${invoiceId}`);
    Object.assign(i, { status: 'on_hold', holdReason: reason });
    log('invoices.hold', { invoiceId, amount: `${i.currency} ${i.amount}`, reason });
    return result({ invoiceId, status: 'on_hold', message: `decision recorded: on hold ${invoiceId} (${reason})` });
  });

  created.attach(mcp);
  return mcp;
}

/** An invoice arrives: it is entered as pending, and `invoice.created` fires. */
function arrive(spec) {
  let inv;
  if (spec.vendorId) {
    if (!vendors.has(spec.vendorId)) throw new Error(`no vendor ${spec.vendorId}`);
    const n = Math.max(...[...invoices.keys(), ...script.map((i) => i.invoiceId)].map((id) => Number(id.slice(4)))) + 1;
    inv = {
      invoiceId: spec.invoiceId ?? `INV-${n}`, vendorId: spec.vendorId,
      vendorInvoiceNumber: spec.vendorInvoiceNumber ?? `EXT-${10000 + n}`,
      amount: Number(spec.amount ?? 1000).toFixed(2), currency: spec.currency ?? 'GBP',
      issuedOn: new Date().toISOString().slice(0, 10),
      dueOn: spec.dueOn ?? new Date(Date.now() + 30 * 864e5).toISOString().slice(0, 10),
      poNumber: spec.poNumber ?? `PO-${77600 + (n % 100)}`, receiptMatched: spec.receiptMatched ?? true,
      memo: spec.memo ?? 'Services',
    };
  } else if (spec.invoiceId) {
    const at = script.findIndex((i) => i.invoiceId === spec.invoiceId);
    if (at < 0) throw new Error(`no scripted invoice ${spec.invoiceId}`);
    inv = script.splice(at, 1)[0];
  } else {
    inv = script.shift();
    if (!inv) throw new Error('the scripted invoices are used up: name a vendorId and an amount');
  }
  if (invoices.has(inv.invoiceId)) throw new Error(`${inv.invoiceId} already arrived`);
  inv.status = 'pending_approval';
  invoices.set(inv.invoiceId, inv);
  const payload = {
    invoiceId: inv.invoiceId, vendorId: inv.vendorId, vendorName: vendors.get(inv.vendorId).name,
    amount: inv.amount, currency: inv.currency, dueOn: inv.dueOn, poNumber: inv.poNumber, memo: inv.memo,
  };
  const seq = created.append(`evt-${inv.invoiceId}`, payload);
  log('invoice.created', { invoiceId: inv.invoiceId, vendor: payload.vendorName, amount: `${inv.currency} ${inv.amount}`, seq });
  return { arrived: payload, nextScripted: script[0]?.invoiceId ?? null };
}

await serve({
  name: 'payables',
  port,
  factory,
  control: {
    invoice: async (spec) => { try { return arrive(spec); } catch (e) { return { error: e.message }; } },
    state: async () => ({ invoices: [...invoices.values()], nextScripted: script.map((i) => i.invoiceId) }),
  },
});
