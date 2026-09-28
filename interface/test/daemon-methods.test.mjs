// SPDX-License-Identifier: AGPL-3.0-only
// MONOREPO-ONLY: this test reads the daemon's Rust sources from the
// repository (crates/agentd/src/runtime/surface/), which the npm package
// does not ship. It runs wherever `npm test` runs — inside the agentd tree.
//
// CORE_METHODS (a2a.ts) is the client's list of the A2A 1.0 methods. The
// daemon's is `SpecMethod::ALL`, spelled by the one exhaustive match in
// `SpecMethod::name()`; its extension methods (EXTENSION_METHODS) are routed
// beside that table, never in it. Checking the client's list only against
// itself proves nothing, so it is checked against the daemon's here: a method
// either side adds, drops or misspells fails.
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

/**
 * The daemon's core methods: `SpecMethod::ALL`, each variant mapped through
 * the arms of `SpecMethod::name()`. Reading both — not just the match — means
 * a variant spelled but left out of ALL (and so never routed) fails here too.
 */
function specMethods(src) {
  const all = [...constBody(src, 'ALL').matchAll(/SpecMethod::(\w+)/g)].map((m) => m[1]);
  const fn = /pub fn name\(self\) -> &'static str \{([\s\S]*?)\n    \}/.exec(src);
  assert.ok(fn, 'SpecMethod::name() not found');
  const spelled = new Map(
    [...fn[1].matchAll(/SpecMethod::(\w+)\s*=>\s*"([^"\\]*)"/g)].map((m) => [m[1], m[2]]),
  );
  return all.map((variant) => {
    assert.ok(spelled.has(variant), `SpecMethod::${variant} has no wire name`);
    return spelled.get(variant);
  });
}

test('CORE_METHODS is what the daemon routes as core, and no extension method', () => {
  const core = specMethods(readFileSync(join(surface, 'methods.rs'), 'utf8'));
  // `(method, extension URI)` pairs: the method is each tuple's first literal.
  const extBody = constBody(readFileSync(join(surface, 'ext.rs'), 'utf8'), 'EXTENSION_METHODS');
  const extension = [...extBody.matchAll(/\(\s*"([^"\\]*)"/g)].map((m) => m[1]);
  assert.ok(core.length > 0 && extension.length > 0, 'the daemon lists were read');
  for (const m of extension) {
    assert.ok(!core.includes(m), `extension method ${m} is never a core one`);
    assert.ok(!CORE_METHODS.includes(m), `CORE_METHODS does not list extension method ${m}`);
  }
  assert.deepEqual([...CORE_METHODS].sort(), [...core].sort());
});
