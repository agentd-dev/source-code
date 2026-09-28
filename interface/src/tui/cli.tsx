// SPDX-License-Identifier: AGPL-3.0-only
/**
 * `agentd-tui` — the terminal display client for a running agentd.
 *
 *   agentd-tui --endpoint http://127.0.0.1:8420 [--bearer-file PATH | --login] [--debug]
 *
 * Renders FULLSCREEN (the alternate screen) by default — PgUp/PgDn scroll the
 * transcript, and your terminal is restored on exit. `--inline` renders into
 * the normal buffer instead, leaving the conversation in your scrollback.
 *
 * `agentd tui -c cfg.yaml` runs the daemon and this client together: the
 * launcher starts it with `--endpoint <url> --launch-fd 3` and a single-use
 * launch code on fd 3, which is exchanged here for an operator session that
 * lives only in this process's memory. That is the one way the two run
 * together; this program never starts a daemon.
 *
 * No request this client makes carries an Origin header: the daemon binds a
 * launch code to the ABSENCE of one, and treats a request with one as a
 * browser's, which is never the implicit operator.
 */
import React from 'react';
import { render } from 'ink';
import { App } from './app.js';
import type { SignIn } from './app.js';
import {
  HELP,
  LAUNCH_REFUSED,
  TUI_CLIENT_ID,
  UsageError,
  chooseSignIn,
  insecureWarning,
  launchTokenUrl,
  layoutOf,
  parseArgs,
  readLaunchCode,
  resolveCredential,
  scrubEnv,
} from './args.js';
import { LaunchRefused, classify, describe, deviceLogin, launchExchange, openSession } from '../client/index.js';
import type { Credential } from '../client/index.js';

function fail(message: string): never {
  process.stderr.write(`agentd-tui: ${message}\n`);
  process.exit(2);
}

// The credential leaves the environment before anything else can read it.
const { bearer: envBearer } = scrubEnv(process.env);

async function main(): Promise<void> {
  let args;
  try {
    args = parseArgs(process.argv.slice(2));
  } catch (e) {
    fail((e as Error).message);
  }
  if (args.help) {
    process.stdout.write(HELP);
    process.exit(0);
  }
  const warning = insecureWarning(args, process.env);
  if (warning !== undefined) {
    process.env.NODE_TLS_REJECT_UNAUTHORIZED = '0';
    process.stderr.write(warning);
  }
  const endpoint = args.endpoint ?? process.env.AGENTD_ENDPOINT;
  if (!endpoint) fail('no endpoint — pass --endpoint http://127.0.0.1:8420 or set AGENTD_ENDPOINT');

  let layout;
  let source;
  try {
    layout = layoutOf(args, process.env);
    source = resolveCredential(args, envBearer);
  } catch (e) {
    if (e instanceof UsageError) fail(e.message);
    throw e;
  }

  let credential: Credential | undefined;
  let signIn: SignIn | undefined;
  switch (source.kind) {
    case 'launch': {
      let code: string;
      try {
        code = readLaunchCode(source.fd);
      } catch (e) {
        fail((e as Error).message);
      }
      // Exactly once: any presentation consumes the code, so there is no
      // retry to make — a refusal means restart the launcher.
      try {
        credential = await launchExchange(launchTokenUrl(endpoint), code, TUI_CLIENT_ID);
      } catch (e) {
        if (e instanceof LaunchRefused) fail(LAUNCH_REFUSED);
        fail(`the launch sign-in failed: ${describe(classify(e))}`);
      }
      signIn = 'launch';
      break;
    }
    case 'bearer':
      credential = { token: source.token };
      signIn = 'bearer';
      break;
    case 'login':
    case 'none': {
      // Read the card first: it says whether this agent lets anyone in
      // anonymously, and how to sign in when it does not.
      let session;
      try {
        session = await openSession(endpoint, { noExtensions: args.noExtensions });
      } catch (e) {
        const f = classify(e);
        // Not up yet, or busy: without --login the app waits for it like any
        // reconnect. A card this client can never use is final either way.
        if (source.kind === 'login' || f.kind === 'incompatible') fail(describe(f));
        break;
      }
      const choice = chooseSignIn(session.caps.login, source.kind === 'login');
      if (choice.kind === 'refuse') fail(choice.reason);
      if (choice.kind === 'anonymous') break;
      if (!process.stdin.isTTY || !process.stderr.isTTY) {
        fail('this agent requires sign-in, and the device sign-in needs an interactive terminal (or pass --bearer-file)');
      }
      try {
        credential = await deviceLogin({
          flow: choice.flow,
          clientId: TUI_CLIENT_ID,
          scope: source.kind === 'login' ? source.scope : undefined,
          onCode: (c) =>
            process.stderr.write(
              `To sign in, open ${c.verificationUriComplete ?? c.verificationUri} and enter ${c.userCode},\n` +
                `or ask an operator to run: /approve ${c.userCode} <name>\nwaiting for approval…\n`,
            ),
        });
      } catch (e) {
        fail(describe(classify(e)));
      }
      signIn = 'device';
      break;
    }
  }

  // Fullscreen (the alternate screen) is the default; `--inline` keeps the
  // conversation in the terminal's own scrollback instead. Ink itself guards
  // the alternate screen behind an interactive TTY, so a pipe/CI run degrades
  // to inline automatically.
  const inline = args.inline || process.env.AGENTD_TUI_INLINE === '1';
  render(
    <App
      configured={endpoint}
      credential={credential}
      signIn={signIn}
      layout={layout}
      noExtensions={args.noExtensions}
      debug={args.debug}
      fullscreen={!inline}
    />,
    { alternateScreen: !inline },
  );
}

void main();
