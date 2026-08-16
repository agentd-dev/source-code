// SPDX-License-Identifier: Apache-2.0
/**
 * `agentd-tui` — the terminal display client for a running agentd.
 *
 *   agentd-tui --endpoint http://127.0.0.1:8420 [--bearer …] [--code 123456] [--debug]
 *
 * Renders FULLSCREEN (the alternate screen) by default — PgUp/PgDn scroll the
 * transcript, and your terminal is restored on exit. `--inline` renders into
 * the normal buffer instead, leaving the conversation in your scrollback.
 *
 * `--code` exchanges a pairing code (shown by the daemon's operator — `/pair`)
 * for a session token, so no bearer ever needs copying (RFC 0032 §13).
 * Environment: AGENTD_ENDPOINT, AGENTD_BEARER, AGENTD_INSECURE=1 (skip TLS
 * verification — self-signed dev daemons only). `agentd tui -c cfg.yaml` runs
 * the daemon and this client together (the passthrough spawns us).
 */
import React from 'react';
import { render } from 'ink';
import { AgentdClient } from '@agentd/client';
import { App } from './app.js';

interface Args {
  endpoint?: string;
  bearer?: string;
  code?: string;
  debug: boolean;
  inline: boolean;
  help: boolean;
}

function parseArgs(argv: string[]): Args {
  const out: Args = { debug: false, inline: false, help: false };
  for (let i = 0; i < argv.length; i++) {
    const a = argv[i];
    if (a === '--endpoint' || a === '-e') out.endpoint = argv[++i];
    else if (a.startsWith('--endpoint=')) out.endpoint = a.slice('--endpoint='.length);
    else if (a === '--bearer') out.bearer = argv[++i];
    else if (a.startsWith('--bearer=')) out.bearer = a.slice('--bearer='.length);
    else if (a === '--code') out.code = argv[++i];
    else if (a.startsWith('--code=')) out.code = a.slice('--code='.length);
    else if (a === '--debug') out.debug = true;
    else if (a === '--inline') out.inline = true;
    else if (a === '--insecure') process.env.NODE_TLS_REJECT_UNAUTHORIZED = '0';
    else if (a === '-h' || a === '--help') out.help = true;
  }
  return out;
}

const args = parseArgs(process.argv.slice(2));
if (args.help) {
  process.stdout.write(
    [
      'agentd-tui — terminal display client for agentd (thin: the daemon hosts all state)',
      '',
      '  agentd-tui --endpoint http://127.0.0.1:8420 [--bearer TOKEN] [--code 123456] [--debug] [--inline] [--insecure]',
      '',
      '  --code     pair with the rotating code the operator shows (/pair) instead of a bearer',
      '  --inline   render inline instead of fullscreen — the conversation stays in',
      '             your terminal scrollback after you quit (fullscreen is the default;',
      '             PgUp/PgDn scroll there)',
      '  env: AGENTD_ENDPOINT, AGENTD_BEARER, AGENTD_TUI_INLINE=1, AGENTD_INSECURE=1',
      '  The daemon must have `interface.enabled: true` (or be started as `agentd tui -c …`).',
      '',
    ].join('\n'),
  );
  process.exit(0);
}
if (process.env.AGENTD_INSECURE === '1') process.env.NODE_TLS_REJECT_UNAUTHORIZED = '0';

const endpoint = args.endpoint ?? process.env.AGENTD_ENDPOINT;
if (!endpoint) {
  process.stderr.write(
    'agentd-tui: no endpoint — pass --endpoint http://127.0.0.1:8420 or set AGENTD_ENDPOINT\n',
  );
  process.exit(2);
}

let bearer = args.bearer ?? process.env.AGENTD_BEARER;
if (args.code) {
  // Pairing login: exchange the code for a session token first (RFC 0032 §13).
  try {
    const session = await new AgentdClient({ url: endpoint }).pair(args.code);
    bearer = session.token;
    process.stderr.write(`agentd-tui: paired as ${session.role} with ${session.agent.instance}\n`);
  } catch (e) {
    process.stderr.write(
      `agentd-tui: pairing failed: ${e instanceof Error ? e.message : String(e)}\n`,
    );
    process.exit(2);
  }
}

// Fullscreen (the alternate screen) is the default; `--inline` keeps the
// conversation in the terminal's own scrollback instead. Ink itself guards the
// alternate screen behind an interactive TTY, so a pipe/CI run degrades to
// inline automatically.
const inline = args.inline || process.env.AGENTD_TUI_INLINE === '1';
render(
  <App endpoint={endpoint} bearer={bearer} debug={args.debug} fullscreen={!inline} />,
  { alternateScreen: !inline },
);
