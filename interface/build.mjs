// SPDX-License-Identifier: AGPL-3.0-only
// Build the static web UI into dist/web/: one JS bundle + the shell + the styles.
// The result is deployable to any static host; `bin/serve.mjs` serves it
// locally with an injected default endpoint.
import { build } from 'esbuild';
import { cpSync, mkdirSync } from 'node:fs';

mkdirSync('dist/web', { recursive: true });
await build({
  entryPoints: ['src/ui/main.tsx'],
  bundle: true,
  format: 'iife',
  outfile: 'dist/web/app.js',
  minify: true,
  define: { 'process.env.NODE_ENV': '"production"' },
  logLevel: 'info',
});
cpSync('public/index.html', 'dist/web/index.html');
cpSync('public/style.css', 'dist/web/style.css');
