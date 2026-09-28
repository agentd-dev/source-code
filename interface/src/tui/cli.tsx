// SPDX-License-Identifier: AGPL-3.0-only
/**
 * `agentd-tui` — the terminal display client for a running agentd.
 *
 *   agentd-tui --endpoint http://127.0.0.1:8420 [--bearer …] [--debug]
 *
 * Renders FULLSCREEN (the alternate screen) by default — PgUp/PgDn scroll the
 * transcript, and your terminal is restored on exit. `--inline` renders into
 * the normal buffer instead, leaving the conversation in your scrollback.
 *
 * Environment: AGENTD_ENDPOINT, AGENTD_BEARER, AGENTD_INSECURE=1 (skip TLS
 * verification — self-signed dev daemons only). `agentd tui -c cfg.yaml` runs
 * the daemon and this client together (the passthrough spawns us).
 */
import React from 'react';
import { render } from 'ink';
import { App } from './app.js';

interface Args {
  endpoint?: string;
  bearer?: string;
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
    else if (a === '--code' || a.startsWith('--code=')) {
      // Pairing is gone, not deprecated: the daemon signs clients in with the
      // OAuth device grant now, and a code no daemon mints must not be
      // silently ignored.
      process.stderr.write('agentd-tui: --code is gone: pairing was replaced by the OAuth device grant\n');
      process.exit(2);
    }
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
      '  agentd-tui --endpoint http://127.0.0.1:8420 [--bearer TOKEN] [--debug] [--inline] [--insecure]',
      '',
      '  --inline   render inline instead of fullscreen — the conversation stays in',
      '             your terminal scrollback after you quit (fullscreen is the default;',
      '             PgUp/PgDn scroll there)',
      '  env: AGENTD_ENDPOINT, AGENTD_BEARER, AGENTD_TUI_INLINE=1, AGENTD_INSECURE=1',
      '  --endpoint names the agent (or its card URL); the card says where to talk to it.',
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

const bearer = args.bearer ?? process.env.AGENTD_BEARER;

// Fullscreen (the alternate screen) is the default; `--inline` keeps the
// conversation in the terminal's own scrollback instead. Ink itself guards the
// alternate screen behind an interactive TTY, so a pipe/CI run degrades to
// inline automatically.
const inline = args.inline || process.env.AGENTD_TUI_INLINE === '1';
render(
  <App
    endpoint={endpoint}
    credential={bearer ? { token: bearer } : undefined}
    debug={args.debug}
    fullscreen={!inline}
  />,
  { alternateScreen: !inline },
);
