// SPDX-License-Identifier: Apache-2.0
// The composer affordances: `/` `@` `#` `$` suggestions, target routing, and
// live-value interpolation — identical behavior for the TUI and the web UI.
import test from 'node:test';
import assert from 'node:assert/strict';
import { Mirror, suggest, applySuggestion, prepare, triggerToken } from '../dist/index.js';

function seeded() {
  const m = new Mirror();
  m.setInfo({ enabled: true, debug: false, version: '2.1.0', instance: 'box-1', model: 'mock-1', protocol: 1, feed: { ring: 1024, method: 'SubscribeToEvents' }, ops: [] });
  m.bootstrap({
    workflows: [{ name: 'deploy' }, { name: 'triage' }],
    skills: ['release-notes', 'oncall'],
    counters: { turns: 4, tokens_in: 120, tokens_out: 60 },
    conversations: [{ id: 'a2a-7' }],
  });
  m.apply({ seq: 1, ts: 1, kind: 'task', data: { task: { id: 'task-9', contextId: 'c', updated: 1, status: { state: 'TASK_STATE_INPUT_REQUIRED', timestamp: 1, message: { parts: [{ text: 'which?' }] } } } } });
  return m;
}

test('slash suggests system commands first, then workflows', () => {
  const s = seeded().getState();
  const all = suggest('/', s, 50).map((x) => x.label);
  assert.ok(all.includes('/help') && all.includes('/pair') && all.includes('/set'));
  const wf = suggest('/dep', s);
  assert.deepEqual(wf.map((x) => [x.label, x.hint]), [['/deploy', 'workflow']]);
  // Only at line start — a mid-sentence slash is not a command.
  assert.equal(suggest('tell me a/b', s).length, 0);
});

test('@ suggests skills, # suggests tasks/conversations, $ suggests values', () => {
  const s = seeded().getState();
  assert.deepEqual(suggest('use @rel', s).map((x) => x.label), ['@release-notes']);
  const hash = suggest('#', s, 10);
  assert.ok(hash.some((x) => x.label === '#task-9' && x.hint === 'answer this task'), `${JSON.stringify(hash)}`);
  assert.ok(hash.some((x) => x.label === '#a2a-7' && x.hint === 'conversation'));
  const dollar = suggest('model is $mo', s);
  assert.deepEqual(dollar.map((x) => [x.label, x.hint]), [['$model', 'mock-1']]);
});

test('applySuggestion replaces the trigger token', () => {
  const s = seeded().getState();
  const sug = suggest('please use @onc', s)[0];
  assert.equal(applySuggestion('please use @onc', sug), 'please use @oncall ');
  assert.equal(triggerToken('nothing here'), null);
});

test('prepare routes leading # targets and interpolates $ values', () => {
  const s = seeded().getState();
  // A task target (answers the input-required gate).
  const p1 = prepare('#task-9 use the blue one', s);
  assert.deepEqual(p1, { text: 'use the blue one', taskId: 'task-9' });
  // A conversation target.
  const p2 = prepare('#a2a-7 hello again', s);
  assert.equal(p2.contextId, 'a2a-7');
  assert.equal(p2.text, 'hello again');
  // $ interpolation: known names only, $$ escapes, inline # untouched.
  const p3 = prepare('running $model on $instance costs $$5 for issue #42', s);
  assert.equal(p3.text, 'running mock-1 on box-1 costs $5 for issue #42');
  assert.equal(p3.taskId, undefined);
  // Unknown $word left alone.
  assert.equal(prepare('price is $unknownvar', s).text, 'price is $unknownvar');
});

test('a config feed event updates the live info every surface renders from', () => {
  const m = seeded();
  m.apply({ seq: 5, ts: 5, kind: 'config', data: { path: 'interface.debug', value: true } });
  assert.equal(m.getState().info.debug, true);
  m.apply({ seq: 6, ts: 6, kind: 'config', data: { path: 'interface.display.bottom', value: ['conn', 'model'] } });
  assert.deepEqual(m.getState().info.display.bottom, ['conn', 'model']);
  assert.ok(m.getState().transcript.some((e) => e.kind === 'info' && e.text.includes('interface.debug')));
});
