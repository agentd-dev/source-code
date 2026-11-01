// SPDX-License-Identifier: AGPL-3.0-only
// `npm start`: a demo's stand-ins in one terminal, their lines interleaved.
//
//   npm start                                  demos (a) and (b): payables on :8701, ERP on :8702
//   npm start -- --policy /path/to/policy.md   and the registry stand-in on :8703
//                                              (rehearsing demo (a) without the mirror)
//
//   npm start -- --demo close   [--policy F] [--mock]     demo (b): the ERP on :8702
//   npm start -- --demo support [--policy F] [--mock]     demo (c)
//   npm start -- --demo coder   [--policy F]              demo (d)
//   npm start -- --demo home    [--policy F] [--mock]     demo (e): thermostat :8741,
//                                                         aircon :8742, home agent :8744
//
// For demos (b)–(e), --policy F serves F as the demo's instruction from a
// registry stand-in under the instruction's own address (close :8713,
// support :8723, coder :8733, home :8743), and --mock starts the scripted
// model with the demo's playbook (close :8719, support :8729, home :8749). Leave both out when recording
// on the mirror and a real model. The home agent asks for the bearer in
// $AGENTD_DEMO_HOME/tokens/home-peer.token (env.sh makes it) when it exists.
import { spawn } from 'node:child_process';
import { existsSync } from 'node:fs';

const flag = (name) => {
  const at = process.argv.indexOf(`--${name}`);
  return at > 0 ? process.argv[at + 1] : undefined;
};
const has = (name) => process.argv.includes(`--${name}`);
const demo = flag('demo');
const policy = flag('policy');
const mock = (playbook, port) => ['model.mjs', '--port', String(port), '--playbook', new URL(`../mock/${playbook}`, import.meta.url).pathname];
const registry = (port, uri, title) => ['registry.mjs', '--file', policy, '--port', String(port), '--uri', uri, '--title', title];

const servers = [];
switch (demo) {
  case undefined:
    servers.push(['payables.mjs'], ['erp.mjs']);
    if (policy) servers.push(['registry.mjs', '--file', policy]);
    break;
  case 'close':
    servers.push(['erp.mjs']);
    if (policy) servers.push(registry(8713, 'instruction://lumen-foods/month-end-close@latest', 'Month-end close'));
    if (has('mock')) servers.push(mock('close.model.json', 8719));
    break;
  case 'support':
    if (policy) servers.push(registry(8723, 'instruction://ins_01M69N5T2ATZNEJVW397VRQTXW@latest', 'Card disputes'));
    if (has('mock')) servers.push(mock('support.model.json', 8729));
    break;
  case 'coder':
    if (policy) servers.push(registry(8733, 'instruction://ins_01M6A4D6VM7DM190DD1D7V14G0@latest', 'Payments service — how we work'));
    break;
  case 'home': {
    const token = `${process.env.AGENTD_DEMO_HOME ?? '/tmp/agentd-demos'}/tokens/home-peer.token`;
    servers.push(['thermostat.mjs'], ['aircon.mjs'], ['home-agent.mjs', ...(existsSync(token) ? ['--token-file', token] : [])]);
    if (policy) servers.push(registry(8743, 'instruction://ins_01M69H2WEK2P005WHYHJJMYM4A@latest', 'Home climate'));
    if (has('mock')) servers.push(mock('home.model.json', 8749));
    break;
  }
  default:
    console.error(`start.mjs: no demo "${demo}" (close, support, coder or home)`);
    process.exit(2);
}
if (!servers.length) {
  console.error('start.mjs: nothing to start: give --policy and/or --mock');
  process.exit(2);
}

const children = servers.map((args) =>
  spawn(process.execPath, args, { cwd: new URL('.', import.meta.url), stdio: 'inherit' }));
const stop = () => children.forEach((c) => c.kill('SIGTERM'));
process.on('SIGINT', stop);
process.on('SIGTERM', stop);
for (const c of children) c.on('exit', (code) => { if (code) { stop(); process.exitCode = code; } });
