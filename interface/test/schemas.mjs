// SPDX-License-Identifier: AGPL-3.0-only
// MONOREPO-ONLY: the published extension contract, as the tests read it —
// the registry agentd.dev serves (web/lib/extensions.json) and the schema
// bundle each extension publishes at `<uri>/schema.json`, compiled with ajv:
// a validator that is not agentd's own, so the daemon and the client cannot
// pass by sharing one misreading of the files.
import assert from 'node:assert/strict';
import { readdirSync, readFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';
import Ajv2020 from 'ajv/dist/2020.js';

const repo = join(dirname(fileURLToPath(import.meta.url)), '..', '..');

export const readRepoJson = (...p) => JSON.parse(readFileSync(join(repo, ...p), 'utf8'));
export const readRepoText = (...p) => readFileSync(join(repo, ...p), 'utf8');

/** Every extension and binding URI agentd publishes, and where its files are. */
export const REGISTRY = readRepoJson('web', 'lib', 'extensions.json');

/** One ajv for every bundle: each carries its own `$id`, so none can shadow another. */
export const ajv = new Ajv2020({ allErrors: true, strict: false });
// RFC 3339 date-time, which the bundles use for task timestamps. ajv knows
// only the formats it is given, and an unknown one is an error, not a pass.
ajv.addFormat('date-time', /^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}(\.\d+)?(Z|[+-]\d{2}:\d{2})$/i);

function entryOf(uri) {
  const entry = REGISTRY.find((e) => e.uri === uri);
  assert.ok(entry?.schema, `the registry publishes a schema for ${uri}`);
  return entry;
}

/** The bundle of the extension `uri`, as agentd.dev publishes it. */
export function bundleOf(uri) {
  return readRepoJson('web', 'public', entryOf(uri).path, 'schema.json');
}

/** A compiled validator for the bundle of `uri`, or for the sub-schema at JSON pointer `at` inside it. */
export function validator(uri, at) {
  const bundle = bundleOf(uri);
  if (ajv.getSchema(bundle.$id) === undefined) ajv.addSchema(bundle);
  const v = ajv.getSchema(at === undefined ? bundle.$id : `${bundle.$id}#${at}`);
  assert.ok(v, `${uri}: nothing at ${at}`);
  return v;
}

/** Assert `value` is what the validator accepts, naming `what` and ajv's reasons when not. */
export function check(validate, value, what) {
  assert.ok(validate(value), `${what} misses the published schema: ${ajv.errorsText(validate.errors)}\n${JSON.stringify(value)}`);
}

/** Every golden example beside the bundle of `uri`: [file name, document]. */
export function examplesOf(uri) {
  const dir = join(repo, 'web', 'public', entryOf(uri).path, 'examples');
  return readdirSync(dir)
    .filter((f) => f.endsWith('.json'))
    .sort()
    .map((f) => [f, JSON.parse(readFileSync(join(dir, f), 'utf8'))]);
}
