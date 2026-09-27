// SPDX-License-Identifier: AGPL-3.0-only
// MONOREPO-ONLY: this test reads the daemon's published config schema from
// the repository (web/public/schema/config-1.json), which the npm package
// does not ship. It runs wherever `npm test` runs — inside the agentd tree.
//
// Every daemon key the display clients name in help text (daemon-keys.ts)
// must be a real path in the daemon's config schema. A key the daemon
// renames or removes then fails here, instead of shipping a UI that sends
// people to edit a setting that no longer exists.
import test from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { DAEMON_KEYS } from '../dist/client/daemon-keys.js';

const here = dirname(fileURLToPath(import.meta.url));
const schema = JSON.parse(readFileSync(join(here, '..', '..', 'web', 'public', 'schema', 'config-1.json'), 'utf8'));

/** Follow a local `$ref` (`#/$defs/…`) to the schema it names. */
function deref(node) {
  let n = node;
  for (let hops = 0; n && typeof n.$ref === 'string' && hops < 16; hops++) {
    assert.ok(n.$ref.startsWith('#/'), `only local refs are followed, got ${n.$ref}`);
    n = n.$ref
      .slice(2)
      .split('/')
      .reduce((o, k) => (o === undefined ? undefined : o[k.replace(/~1/g, '/').replace(/~0/g, '~')]), schema);
  }
  return n;
}

/** The schema for `key` under `node`, looking through anyOf/oneOf/allOf. */
function child(node, key) {
  const n = deref(node);
  if (!n || typeof n !== 'object') return undefined;
  if (n.properties && Object.hasOwn(n.properties, key)) return n.properties[key];
  for (const k of ['anyOf', 'oneOf', 'allOf']) {
    for (const alt of n[k] ?? []) {
      const found = child(alt, key);
      if (found !== undefined) return found;
    }
  }
  return undefined;
}

function resolves(path) {
  let node = schema;
  for (const seg of path.split('.')) {
    node = child(node, seg);
    if (node === undefined) return false;
  }
  return true;
}

test('every daemon key the clients name is in the config schema', () => {
  const keys = Object.values(DAEMON_KEYS);
  assert.ok(keys.length > 0);
  for (const key of keys) {
    assert.ok(resolves(key), `${key} is not a path in web/public/schema/config-1.json`);
  }
  // The resolver itself refuses what is not there.
  assert.equal(resolves('a2a.no_such_key'), false);
  assert.equal(resolves('no_such_section.enabled'), false);
});

test("no daemon key lives under the removed 'interface' section", () => {
  for (const key of Object.values(DAEMON_KEYS)) {
    assert.ok(!key.startsWith('interface.'), `${key}: the interface section was removed in agentd 1.17.0`);
  }
});
