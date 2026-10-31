// SPDX-License-Identifier: AGPL-3.0-only
// `npm start`: both stand-ins in one terminal, their lines interleaved.
//
//   npm start                                  payables on :8701, ERP on :8702
//   npm start -- --policy /path/to/policy.md   and the registry stand-in on :8703
//                                              (rehearsing demo (a) without the mirror)
import { spawn } from 'node:child_process';

const at = process.argv.indexOf('--policy');
const servers = [['payables.mjs'], ['erp.mjs']];
if (at > 0) servers.push(['registry.mjs', '--file', process.argv[at + 1]]);

const children = servers.map((args) =>
  spawn(process.execPath, args, { cwd: new URL('.', import.meta.url), stdio: 'inherit' }));
const stop = () => children.forEach((c) => c.kill('SIGTERM'));
process.on('SIGINT', stop);
process.on('SIGTERM', stop);
for (const c of children) c.on('exit', (code) => { if (code) { stop(); process.exitCode = code; } });
