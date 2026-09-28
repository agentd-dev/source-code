// SPDX-License-Identifier: AGPL-3.0-only
// MONOREPO-ONLY: this test reads the daemon's Rust sources from the
// repository (crates/agentd/src/runtime/surface/), which the npm package
// does not ship. It runs wherever `npm test` runs — inside the agentd tree.
//
// CORE_METHODS (a2a.ts) is the client's list of the A2A 1.0 methods. The
// daemon keeps its own: every method its listener dispatches (METHODS), of
// which the extension methods (EXTENSION_METHODS) are not core. Checking the
// client's list only against itself proves nothing, so it is checked against
// the daemon's here: a method either side adds, drops or misspells fails.
import test from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { CORE_METHODS } from '../dist/client/a2a.js';

const here = dirname(fileURLToPath(import.meta.url));
const surface = join(here, '..', '..', 'crates', 'agentd', 'src', 'runtime', 'surface');

/** The body of `pub const <name>: <type> = &[ … ];` in a Rust source. */
function constBody(src, name) {
  const m = new RegExp(`pub const ${name}: [^=]+= &\\[([\\s\\S]*?)\\];`).exec(src);
  assert.ok(m, `pub const ${name} not found`);
  return m[1];
}

/** Every string literal in `body`, in order. */
function literals(body) {
  return [...body.matchAll(/"([^"\\]*)"/g)].map((m) => m[1]);
}

test('CORE_METHODS is what the daemon dispatches, minus its extension methods', () => {
  const methods = literals(constBody(readFileSync(join(surface, 'methods.rs'), 'utf8'), 'METHODS'));
  // `(method, extension URI)` pairs: the method is each tuple's first literal.
  const extBody = constBody(readFileSync(join(surface, 'ext.rs'), 'utf8'), 'EXTENSION_METHODS');
  const extension = [...extBody.matchAll(/\(\s*"([^"\\]*)"/g)].map((m) => m[1]);
  assert.ok(methods.length > 0 && extension.length > 0, 'the daemon lists were read');
  for (const m of extension) assert.ok(methods.includes(m), `extension method ${m} is dispatched`);

  const daemonCore = methods.filter((m) => !extension.includes(m)).sort();
  assert.deepEqual([...CORE_METHODS].sort(), daemonCore);
});
