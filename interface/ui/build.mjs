// SPDX-License-Identifier: AGPL-3.0-only
// Build the static web UI into dist/: one JS bundle + the shell + the styles.
// The result is deployable to any static host; `bin/serve.mjs` serves it
// locally with an injected default endpoint.
import { build } from 'esbuild';
import { cpSync, mkdirSync } from 'node:fs';

mkdirSync('dist', { recursive: true });
await build({
  entryPoints: ['src/main.tsx'],
  bundle: true,
  format: 'iife',
  outfile: 'dist/app.js',
  minify: true,
  define: { 'process.env.NODE_ENV': '"production"' },
  logLevel: 'info',
});
cpSync('public/index.html', 'dist/index.html');
cpSync('public/style.css', 'dist/style.css');
