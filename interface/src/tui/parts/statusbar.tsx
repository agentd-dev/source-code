// SPDX-License-Identifier: AGPL-3.0-only
/**
 * The status bar: the bottom edge of the chrome, in the client's layout.
 * Pure projection — every item reads through chrome.ts `itemValue`, like the
 * top edge and the web UI's bar, so the three cannot disagree about what a
 * state is called.
 *
 * A connection that stopped for good — `unauthenticated`, `forbidden`,
 * `incompatible` — is named as itself and drawn as an inverse block, never
 * folded into "closed": "closed" sent people looking for a network fault when
 * the fix was to sign in again or to point the client at another agent.
 */
import React from 'react';
import type { ConnState } from '../../client/index.js';
import { ItemRow, chromeInput } from './chrome.js';
import type { ChromeCtx } from './chrome.js';

/** The connection states retrying cannot fix. */
export const TERMINAL_CONN: ReadonlySet<ConnState> = new Set(['unauthenticated', 'forbidden', 'incompatible']);

const CONN_ONLY: ReadonlySet<string> = new Set(['conn']);

export function StatusBar({ items, ctx }: { items: string[]; ctx: ChromeCtx }): React.JSX.Element {
  return (
    <ItemRow items={items} input={chromeInput(ctx)} inverse={TERMINAL_CONN.has(ctx.s.conn) ? CONN_ONLY : undefined} />
  );
}
