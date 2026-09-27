// SPDX-License-Identifier: AGPL-3.0-only
// The client-owned chrome: the item vocabulary, each surface's default
// layout, reading a layout a person wrote, and what an item shows.
import test from 'node:test';
import assert from 'node:assert/strict';
import { DEFAULT_LAYOUT, DISPLAY_ITEMS, itemValue, parseLayout } from '../dist/client/chrome.js';

test('chrome defaults use only items their surface can draw', () => {
  for (const surface of ['tui', 'web']) {
    for (const edge of ['top', 'bottom']) {
      const items = DEFAULT_LAYOUT[surface][edge];
      assert.ok(items.length > 0, `${surface}.${edge} is empty`);
      // Every default parses clean: a default naming an item the vocabulary
      // does not have (or one the surface cannot draw) would silently vanish.
      assert.deepEqual(parseLayout(items, surface), { items: [...items], unknown: [] }, `${surface}.${edge}`);
    }
  }
  // The TUI's screen switcher and key hints mean nothing in a browser.
  assert.ok(DEFAULT_LAYOUT.tui.bottom.includes('keys'));
  assert.ok(!DEFAULT_LAYOUT.web.bottom.includes('keys'));
  assert.deepEqual(DISPLAY_ITEMS.screen.surfaces, ['tui']);
});

test('parseLayout keeps memory items and reports what it does not know', () => {
  assert.deepEqual(parseLayout(' name, memory:x ,bogus,,name', 'tui'), {
    items: ['name', 'memory:x'],
    unknown: ['bogus'],
  });
  // A surface-specific item is unknown elsewhere; a bare prefix is no key.
  assert.deepEqual(parseLayout(['conn', 'keys', 'memory:'], 'web'), { items: ['conn'], unknown: ['keys', 'memory:'] });
  // Object.prototype members are not items.
  assert.deepEqual(parseLayout('toString,constructor', 'tui'), { items: [], unknown: ['toString', 'constructor'] });
});

test('itemValue reads memory values from status.values', () => {
  const base = { conn: 'ready', endpoint: 'http://127.0.0.1:8420' };
  const status = { values: { 'deploy.state': 'green', 'pr': 42, empty: '' } };
  assert.deepEqual(itemValue('memory:deploy.state', { ...base, status }), { text: 'green', tone: 'value' });
  assert.deepEqual(itemValue('memory:pr', { ...base, status }), { text: '42', tone: 'value' });
  // Unset or empty takes no slot at all.
  assert.equal(itemValue('memory:empty', { ...base, status }), null);
  assert.equal(itemValue('memory:missing', { ...base, status }), null);
  assert.equal(itemValue('memory:toString', { ...base, status }), null);
  assert.equal(itemValue('memory:deploy.state', base), null);
});

test('itemValue: chrome text is one line of inert text', () => {
  const base = { conn: 'ready', endpoint: 'http://127.0.0.1:8420' };
  // A model can write memory; a terminal obeys an OSC 52 (set the clipboard),
  // an OSC 8 link or a bare newline. None of it may reach the bar.
  const status = {
    values: {
      osc52: 'ok\u001b]52;c;cm0gLXJmIC8=\u0007done',
      lines: 'a\r\nb',
      c1: 'x\u009by',
      del: 'x\u007fy',
      only: '\u001b\u0007',
    },
  };
  assert.deepEqual(itemValue('memory:osc52', { ...base, status }), { text: 'ok ]52;c;cm0gLXJmIC8= done', tone: 'value' });
  assert.deepEqual(itemValue('memory:lines', { ...base, status }), { text: 'a b', tone: 'value' });
  assert.equal(itemValue('memory:c1', { ...base, status }).text, 'x y');
  assert.equal(itemValue('memory:del', { ...base, status }).text, 'x y');
  // Nothing visible left: no slot, as for an unset key.
  assert.equal(itemValue('memory:only', { ...base, status }), null);
  // The card's name and a connection error come from the other side too.
  assert.equal(itemValue('name', { ...base, card: { name: 'evil\u001b]0;pwned\u0007' } }).text, 'evil ]0;pwned ');
  assert.equal(itemValue('conn', { ...base, conn: 'error', error: 'refused\nby peer' }).text, '✗ refused by peer');
});

test('itemValue: no counters means null, and each item reads its source', () => {
  const base = { conn: 'ready', endpoint: 'http://127.0.0.1:8420' };
  // A non-operator's status carries no counters: the items stay away rather
  // than claiming zero.
  const nonOperator = { status: { instance: 'i1', version: '1.17.0' } };
  for (const n of ['turns', 'tokens', 'tool_calls']) assert.equal(itemValue(n, { ...base, ...nonOperator }), null, n);
  const status = { instance: 'i1', version: '1.17.0', counters: { turns: 3, tokens_in: 10, tokens_out: 4, tool_calls: 2 } };
  assert.deepEqual(itemValue('turns', { ...base, status }), { text: '3 turns', tone: 'muted' });
  assert.deepEqual(itemValue('tokens', { ...base, status }), { text: '10/4 tok', tone: 'muted' });
  assert.deepEqual(itemValue('tool_calls', { ...base, status }), { text: '2 tools', tone: 'muted' });
  assert.deepEqual(itemValue('version', { ...base, status }), { text: '1.17.0', tone: 'muted' });
  assert.deepEqual(itemValue('instance', { ...base, status }), { text: 'i1', tone: 'identity' });
  assert.deepEqual(itemValue('name', { ...base, card: { name: 'beacon' } }), { text: 'beacon', tone: 'identity' });
  // The debug badge follows the card's introspection, nothing else.
  assert.equal(itemValue('debug', base), null);
  assert.deepEqual(itemValue('debug', { ...base, introspection: true }), { text: 'debug', tone: 'caution' });
  // Draining: the live flag wins; the status document is the fallback.
  assert.deepEqual(itemValue('draining', { ...base, status: { draining: true } }), { text: 'DRAINING', tone: 'alert' });
  assert.equal(itemValue('draining', { ...base, draining: false, status: { draining: true } }), null);
  assert.equal(itemValue('active', { ...base, active: 0 }), null);
  assert.equal(itemValue('bogus', base), null);
});

test('itemValue: each stop reason reads as itself', () => {
  const at = (conn, error) => itemValue('conn', { conn, error, endpoint: 'e' });
  assert.deepEqual(at('ready'), { text: '● live', tone: 'live' });
  assert.deepEqual(at('unauthenticated'), { text: '✗ unauthenticated', tone: 'error' });
  assert.deepEqual(at('forbidden'), { text: '✗ forbidden', tone: 'error' });
  assert.deepEqual(at('incompatible'), { text: '✗ incompatible', tone: 'error' });
  assert.deepEqual(at('error', 'ECONNREFUSED'), { text: '✗ ECONNREFUSED', tone: 'error' });
});
