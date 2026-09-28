// SPDX-License-Identifier: AGPL-3.0-only
/**
 * The page's entry, and the order it runs in is the point: the launch code
 * `agentd ui` put in the URL fragment is taken out of the URL FIRST — before
 * ./bootstrap.json or any other request — and kept in memory only, so nothing
 * the page does next (a history entry, an error that quotes the location, a
 * request) can carry it. Only then is the bootstrap read and the app drawn.
 */
import React from 'react';
import { createRoot } from 'react-dom/client';
import { App, readBootstrap, takeLaunchCode } from './app.js';

const launchCode = takeLaunchCode(location, history);
void readBootstrap(location.href).then((bootstrap) => {
  createRoot(document.getElementById('root') as HTMLElement).render(<App bootstrap={bootstrap} launchCode={launchCode} />);
});
