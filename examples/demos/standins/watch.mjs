// SPDX-License-Identifier: AGPL-3.0-only
// One line per agentd event worth seeing on camera, from one or more agentd
// logs, as they are written: a pane for the recording beside the TUIs.
//
//   node watch.mjs chat=/tmp/agentd-demos/support-chat.log voice=… email=…
//   node watch.mjs --events instruction.applied,turn.reply chat=…
//
// Each argument is <label>=<agentd log file> (agentd's JSON lines on stderr,
// as `2> >(tee …)` writes them). The file may not exist yet; it is followed
// from its end, and from its start if it appears later. Default events:
// instruction.applied, instruction.unavailable, start.fired, human.ask,
// human.answered, exec.run.
import { closeSync, existsSync, openSync, readSync, statSync, watchFile } from 'node:fs';

const args = process.argv.slice(2);
const at = args.indexOf('--events');
const events = new Set((at >= 0 ? args.splice(at, 2)[1] : 'instruction.applied,instruction.unavailable,start.fired,human.ask,human.answered,exec.run').split(','));
const sources = args.map((a) => a.split('=')).filter(([l, f]) => l && f);
if (!sources.length) {
  console.error('usage: node watch.mjs [--events a,b] <label>=<agentd log> …');
  process.exit(2);
}
const short = (v) => (typeof v === 'string' ? v.replace(/^ver_local_/, 'v').replace(/^(ver_[0-9A-Z]{6})[0-9A-Z]+$/, '$1…') : v);
const width = Math.max(...sources.map(([l]) => l.length));

function line(label, raw) {
  let e;
  try { e = JSON.parse(raw); } catch { return; }
  if (!events.has(e.event)) return;
  const ts = (e.ts ?? '').slice(11, 23);
  let what = e.event;
  if (e.event === 'instruction.applied') what += `  ${short(e.old_version_id) ?? '(start)'} → ${short(e.new_version_id)}`;
  else if (e.event === 'start.fired') what += `  ${e.workflow ?? ''} (${e.kind ?? ''})`;
  else if (e.event === 'human.ask') what += `  ${String(e.question ?? '').split('\n')[0].slice(0, 70)}`;
  else if (e.event === 'human.answered') what += `  by ${e.by ?? '?'} ${JSON.stringify(e.labels ?? {})}`;
  else if (e.event === 'exec.run') what += `  ${e.cmd} (${e.argc} args)`;
  else if (e.event === 'instruction.unavailable') what += `  ${e.policy ?? ''}`;
  console.log(`${ts}  ${label.padEnd(width)}  ${what}`);
}

for (const [label, file] of sources) {
  let pos = existsSync(file) ? statSync(file).size : 0;
  let rest = '';
  const read = () => {
    if (!existsSync(file)) return;
    const size = statSync(file).size;
    if (size < pos) pos = 0; // truncated or replaced: start again
    if (size === pos) return;
    const fd = openSync(file, 'r');
    const buf = Buffer.alloc(size - pos);
    readSync(fd, buf, 0, buf.length, pos);
    closeSync(fd);
    pos = size;
    const text = rest + buf.toString('utf8');
    const lines = text.split('\n');
    rest = lines.pop();
    for (const l of lines) line(label, l);
  };
  watchFile(file, { interval: 100 }, read);
}
console.log(`watching ${sources.map(([l]) => l).join(', ')} for ${[...events].join(', ')}`);
