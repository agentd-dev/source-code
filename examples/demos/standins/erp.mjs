// SPDX-License-Identifier: AGPL-3.0-only
// A stand-in for an ERP's general ledger and bank feed: today's bank lines as
// a subscribable MCP resource, the cash book and chart of accounts as tools,
// and journal entries that can be drafted — and, as in any real ERP, posted.
// Every name and number in fixtures/erp.json is fictional.
//
//   node erp.mjs [--port 8702]
//
// MCP (http://127.0.0.1:8702/mcp):
//   resource  erp://bank-feed/today — subscribable; updated when a line lands
//   tools     bank_feed.lines, ledger.cash_entries, accounts.list,
//             journal.draft, journal.list, journal.post
// journal.post exists because a real ERP has it. agentd's configurations never
// grant it (their server `allow` lists leave it out), so no model and no step
// of theirs can post; the rehearsal checks this server never saw the call.
//
// Control (POST http://127.0.0.1:8702/control/<name>, JSON; or control.mjs):
//   bankline  {}                    the next scripted bank line lands
//             {"count": 4}          the next four land, one update
//             {"amount": "-12.00", "description": "…"}  a new one
//   state     {}                    the feed, the cash book and the journals
import { McpServer } from '@modelcontextprotocol/server';
import { z } from 'zod';
import { arg, fixture, logger, refusal, result, serve } from './lib.mjs';

const port = Number(arg('port', process.env.ERP_PORT || 8702));
const log = logger('erp');
const data = fixture('erp.json');
const FEED = 'erp://bank-feed/today';
const lines = [...data.bankLines];
const script = [...data.nextBankLines];
const accounts = new Map(data.accounts.map((a) => [a.code, a]));
const journals = [];

const feed = () => ({ account: data.bankAccount, date: data.feedDate, lines });
const money = (s) => Math.round(Number(s) * 100);

function factory() {
  const mcp = new McpServer(
    { name: 'erp-standin', version: '1.0.0', title: `General ledger — ${data.company}` },
    { capabilities: { tools: {}, resources: { subscribe: true } } },
  );

  mcp.registerResource('bank-feed-today', FEED, {
    title: `Bank feed, today — ${data.bankAccount.name} ${data.bankAccount.number}`,
    mimeType: 'application/json',
  }, async () => ({ contents: [{ uri: FEED, mimeType: 'application/json', text: JSON.stringify(feed(), null, 2) }] }));

  mcp.registerTool('bank_feed.lines', {
    description: "Today's bank statement lines for the operating account (the same as erp://bank-feed/today). Amounts are signed: money in is positive.",
    inputSchema: z.object({}),
    annotations: { readOnlyHint: true },
  }, async () => result(feed()));

  mcp.registerTool('ledger.cash_entries', {
    description: 'Cash-book entries already recorded in the ledger for the operating account, with their references. A bank line with no entry here is unmatched.',
    inputSchema: z.object({ date: z.string().optional() }),
    annotations: { readOnlyHint: true },
  }, async ({ date }) => result({ entries: data.cashBook.filter((e) => !date || e.date === date) }));

  mcp.registerTool('accounts.list', {
    description: 'The chart of accounts: code, name and type.',
    inputSchema: z.object({}),
    annotations: { readOnlyHint: true },
  }, async () => result({ accounts: [...accounts.values()] }));

  mcp.registerTool('journal.draft', {
    description: 'Draft a journal entry for review. A draft changes no balance until a person posts it in the ERP. Lines must balance: total debits equal total credits.',
    inputSchema: z.object({
      date: z.string(),
      memo: z.string(),
      sourceBankLine: z.string().optional(),
      lines: z.array(z.object({ account: z.string(), debit: z.string().optional(), credit: z.string().optional() })).min(2),
    }),
    annotations: { idempotentHint: false },
  }, async ({ date, memo, sourceBankLine, lines: jl }) => {
    const unknown = jl.filter((l) => !accounts.has(l.account)).map((l) => l.account);
    if (unknown.length) return refusal(`unknown account(s): ${unknown.join(', ')} — see accounts.list`);
    const dr = jl.reduce((s, l) => s + money(l.debit ?? 0), 0);
    const cr = jl.reduce((s, l) => s + money(l.credit ?? 0), 0);
    if (dr !== cr || dr === 0) return refusal(`the entry does not balance: debits ${(dr / 100).toFixed(2)}, credits ${(cr / 100).toFixed(2)}`);
    if (sourceBankLine && journals.some((j) => j.sourceBankLine === sourceBankLine)) {
      const j = journals.find((x) => x.sourceBankLine === sourceBankLine);
      return result({ journalId: j.journalId, status: j.status, message: `${sourceBankLine} already has draft ${j.journalId}` });
    }
    const j = { journalId: `JE-DRAFT-${String(journals.length + 1).padStart(3, '0')}`, status: 'draft', date, memo, sourceBankLine, lines: jl, total: (dr / 100).toFixed(2) };
    journals.push(j);
    log('journal.draft', { journalId: j.journalId, sourceBankLine: sourceBankLine ?? '-', total: `GBP ${j.total}`, memo });
    return result({ journalId: j.journalId, status: 'draft', message: `draft ${j.journalId} saved for review; nothing is posted` });
  });

  mcp.registerTool('journal.list', {
    description: 'Journal entries, optionally only drafts or only posted ones.',
    inputSchema: z.object({ status: z.enum(['draft', 'posted']).optional() }),
    annotations: { readOnlyHint: true },
  }, async ({ status }) => result({ journals: journals.filter((j) => !status || j.status === status) }));

  mcp.registerTool('journal.post', {
    description: 'Post a drafted journal entry to the general ledger. Posting changes balances and closes the period entry.',
    inputSchema: z.object({ journalId: z.string() }),
    annotations: { destructiveHint: true },
  }, async ({ journalId }) => {
    log('journal.post', { journalId, warning: 'a client posted a journal entry' });
    const j = journals.find((x) => x.journalId === journalId);
    if (!j) return refusal(`no journal ${journalId}`);
    j.status = 'posted';
    return result({ journalId, status: 'posted' });
  });
  return mcp;
}

let handler;
function land(spec) {
  const added = [];
  if (spec.amount !== undefined) {
    const n = Math.max(...[...lines, ...script].map((l) => Number(l.id.slice(3)))) + 1;
    added.push({ id: `BL-${String(n).padStart(4, '0')}`, date: data.feedDate, amount: Number(spec.amount).toFixed(2), description: spec.description ?? 'UNSPECIFIED' });
  } else {
    const count = Math.max(1, Number(spec.count ?? 1));
    for (let i = 0; i < count && script.length; i++) added.push(script.shift());
    if (!added.length) throw new Error('the scripted bank lines are used up: give an amount and a description');
  }
  lines.push(...added);
  for (const l of added) log('bank_feed.line', { id: l.id, amount: `GBP ${l.amount}`, description: l.description });
  // Tell every open subscription: the client reads the resource again.
  handler.notify.resourceUpdated(FEED);
  log('resources/updated', { uri: FEED, lines: lines.length });
  return { added, nextScripted: script[0]?.id ?? null };
}

({ handler } = await serve({
  name: 'erp',
  port,
  factory,
  control: {
    bankline: async (spec) => { try { return land(spec); } catch (e) { return { error: e.message }; } },
    state: async () => ({ feed: feed(), cashBook: data.cashBook, journals, nextScripted: script.map((l) => l.id) }),
  },
}));
