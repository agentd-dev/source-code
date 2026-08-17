// SPDX-License-Identifier: Apache-2.0
import React from 'react';
import { createRoot } from 'react-dom/client';
import { App, Defaults } from './app.js';

declare global {
  interface Window {
    AGENTD_DEFAULTS?: Defaults;
  }
}

const defaults: Defaults = window.AGENTD_DEFAULTS ?? {};
createRoot(document.getElementById('root') as HTMLElement).render(<App defaults={defaults} />);
