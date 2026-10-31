// SPDX-License-Identifier: AGPL-3.0-only
// The presenter's buttons, from a terminal. Talks to the stand-ins' /control
// endpoints on 127.0.0.1; agentd is not involved.
//
//   node control.mjs invoice                    the next scripted invoice arrives
//   node control.mjs invoice INV-2044           that scripted invoice arrives
//   node control.mjs invoice --vendor V-1002 --amount 18500 [--memo "…"]
//   node control.mjs bankline                   the next scripted bank line lands
//   node control.mjs bankline --count 4         the next four land, one update
//   node control.mjs bankline --amount -12.00 --description "CARD FEE"
//   node control.mjs state payables|erp         what the stand-in holds now
const [what, ...rest] = process.argv.slice(2);
const flag = (name) => {
  const at = rest.indexOf(`--${name}`);
  return at >= 0 ? rest[at + 1] : undefined;
};
const PAYABLES = process.env.PAYABLES_CONTROL ?? 'http://127.0.0.1:8701/control';
const ERP = process.env.ERP_CONTROL ?? 'http://127.0.0.1:8702/control';

async function call(base, name, body = {}) {
  const r = await fetch(`${base}/${name}`, { method: 'POST', headers: { 'content-type': 'application/json' }, body: JSON.stringify(body) });
  const out = await r.json();
  console.log(JSON.stringify(out, null, 2));
  if (out.error) process.exit(1);
}

const drop = (o) => Object.fromEntries(Object.entries(o).filter(([, v]) => v !== undefined));
switch (what) {
  case 'invoice':
    await call(PAYABLES, 'invoice', drop({
      invoiceId: rest[0] && !rest[0].startsWith('--') ? rest[0] : undefined,
      vendorId: flag('vendor'), amount: flag('amount'), memo: flag('memo'),
    }));
    break;
  case 'bankline':
    await call(ERP, 'bankline', drop({ count: flag('count'), amount: flag('amount'), description: flag('description') }));
    break;
  case 'state':
    await call(rest[0] === 'erp' ? ERP : PAYABLES, 'state');
    break;
  default:
    console.error('usage: node control.mjs invoice [INV-…|--vendor V-… --amount N] | bankline [--count N|--amount N --description T] | state payables|erp');
    process.exit(2);
}
