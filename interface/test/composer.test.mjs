// SPDX-License-Identifier: AGPL-3.0-only
// The composer affordances: `/` `@` `#` `$` suggestions, target routing, and
// live-value interpolation — identical behavior for the TUI and the web UI.
import test from 'node:test';
import assert from 'node:assert/strict';
import {
  COMMAND_EXTENSION,
  Mirror,
  OPS,
  SLASH_OPS,
  SYSTEM_COMMANDS,
  applySuggestion,
  availableCommands,
  capabilitiesOf,
  commandHelp,
  conversationsNote,
  currentGate,
  parseAuthCommand,
  prepare,
  routeSend,
  runAuthCommand,
  suggest,
  triggerToken,
  workflowNames,
} from '../dist/client/index.js';

/** Every built-in op, as the public card's static vocabulary lists them. */
const ALL_OPS = Object.values(OPS);

/** A card declaring command/v2 with `params`. */
function cardWith(params, extra = {}) {
  return { name: 'beacon', version: '1.17.0', capabilities: { extensions: [{ uri: COMMAND_EXTENSION, params }] }, ...extra };
}

/** What discovery settles: a public card, and the extended card when one was read. */
function session(pub, ext = null) {
  return { cardUrl: 'http://127.0.0.1:1/.well-known/agent-card.json', card: pub, extended: ext, ep: { url: 'http://127.0.0.1:1/' }, caps: capabilitiesOf(pub, ext), warnings: [] };
}

/** A daemon-shaped task event. */
function task(id, contextId, state, at) {
  return { task: { id, contextId, status: { state, timestamp: new Date(Date.UTC(2026, 8, 1, 0, 0, at)).toISOString() } } };
}

function seeded() {
  const m = new Mirror();
  m.setSession(session(cardWith({ ops: ALL_OPS, settable: ['agent.approval', 'a2a.introspection.enabled'] })));
  m.bootstrap({
    version: '2.1.0',
    instance: 'box-1',
    model: 'mock-1',
    workflows: [{ name: 'deploy' }, { name: 'triage' }],
    skills: ['release-notes', 'oncall'],
    skill_prefix: '@skill:',
    counters: { turns: 4, tokens_in: 120, tokens_out: 60 },
    conversations: [{ id: 'a2a-7' }],
  });
  m.apply({ seq: 1, ts: 1, kind: 'task', data: { task: { id: 'task-9', contextId: 'c', status: { state: 'TASK_STATE_INPUT_REQUIRED', timestamp: '2026-09-01T00:00:01Z', message: { parts: [{ text: 'which?' }] } } } } });
  return m;
}

test('slash suggests system commands first, then workflows', () => {
  const s = seeded().getState();
  const all = suggest('/', s, 50).map((x) => x.label);
  assert.ok(all.includes('/help') && all.includes('/set'));
  const wf = suggest('/dep', s);
  assert.deepEqual(wf.map((x) => [x.label, x.hint]), [['/deploy', 'workflow']]);
  // Only at line start — a mid-sentence slash is not a command.
  assert.equal(suggest('tell me a/b', s).length, 0);
  // Pairing is gone: the device grant replaced it.
  assert.ok(!all.includes('/pair'));
  assert.ok(!SYSTEM_COMMANDS.some((c) => c.name === 'pair'));
});

test('commands are gated by the card', () => {
  // The card offers this caller two ops: only their commands, and the
  // client's own, are offered.
  const m = new Mirror();
  m.setSession(session(cardWith({ ops: ['status'] }), cardWith({ ops: ['status', 'workflow.run'] })));
  const names = availableCommands(m.getState()).map((c) => c.name);
  for (const local of ['help', 'new', 'layout', 'login', 'logout', 'cancel', 'quit']) assert.ok(names.includes(local), local);
  assert.ok(names.includes('status') && names.includes('workflow'));
  for (const gated of ['set', 'drain', 'pause', 'approve', 'deny', 'devices', 'sessions', 'revoke', 'config']) {
    assert.ok(!names.includes(gated), `/${gated} is offered without its op`);
  }
  const labels = suggest('/', m.getState(), 50).map((x) => x.label);
  assert.ok(labels.includes('/status') && !labels.includes('/set') && !labels.includes('/approve'));
  assert.ok(!commandHelp(m.getState()).includes('/drain'));
  assert.ok(commandHelp(m.getState()).includes('/status'));

  // No command vocabulary at all (core mode, --no-extensions), or no session
  // yet: only the client's own commands, and no workflow shortcuts.
  for (const s of [new Mirror().getState(), (() => { const x = new Mirror(); x.setSession(session({ name: 'plain' })); x.bootstrap({ workflows: [{ name: 'deploy' }] }); return x.getState(); })()]) {
    const got = availableCommands(s);
    assert.ok(got.every((c) => c.needs === undefined), JSON.stringify(got));
    assert.equal(suggest('/dep', s).length, 0);
  }

  // Each op command needs exactly the op its slash word sends, from the one
  // table of them — and every slash op has its command.
  for (const [word, op] of Object.entries(SLASH_OPS)) {
    const c = SYSTEM_COMMANDS.find((x) => x.name === word);
    assert.ok(c, `/${word} has no system command`);
    assert.equal(c.needs, op);
  }
  assert.deepEqual(
    SYSTEM_COMMANDS.filter((c) => c.needs === undefined).map((c) => c.name).sort(),
    ['cancel', 'chat', 'conversations', 'debug', 'help', 'layout', 'login', 'logout', 'new', 'quit', 'subagents', 'tasks'],
  );
});

test('@ completion uses the published prefix', () => {
  const m = seeded();
  // The daemon's prefix, whatever it is configured to.
  m.apply({ seq: 2, ts: 2, kind: 'status', data: { skill_prefix: '#sk:' } });
  const s = m.getState();
  assert.deepEqual(suggest('use @rel', s).map((x) => [x.label, x.insert]), [['#sk:release-notes', '#sk:release-notes ']]);
  assert.equal(applySuggestion('please use @onc', suggest('please use @onc', s)[0]), 'please use #sk:oncall ');
  // No published prefix: no guess.
  const bare = new Mirror();
  bare.bootstrap({ skills: ['oncall'] });
  assert.deepEqual(suggest('use @onc', bare.getState()), []);
});

test('/set completions come from settable', () => {
  const s = seeded().getState();
  assert.deepEqual(suggest('/set ', s).map((x) => x.label), ['agent.approval', 'a2a.introspection.enabled']);
  assert.deepEqual(suggest('/set a2a', s).map((x) => x.label), ['a2a.introspection.enabled']);
  assert.equal(applySuggestion('/set a2a.in', suggest('/set a2a.in', s)[0]), '/set a2a.introspection.enabled ');
  // Past the path, nothing more is completed.
  assert.deepEqual(suggest('/set agent.approval ', s), []);
  // A caller the card lists nothing settable for gets nothing.
  const m = new Mirror();
  m.setSession(session(cardWith({ ops: ['admin.set'] })));
  assert.deepEqual(suggest('/set ', m.getState()), []);
  // Nor does one not offered admin.set, whatever else the card says.
  const n = new Mirror();
  n.setSession(session(cardWith({ ops: ['status'], settable: ['agent.approval'] })));
  assert.deepEqual(suggest('/set ', n.getState()), []);
});

test('workflow names strip the workflow: skill-id prefix', () => {
  const pub = cardWith({ ops: ALL_OPS }, { capabilities: { extendedAgentCard: true, extensions: [{ uri: COMMAND_EXTENSION, params: { ops: ALL_OPS } }] } });
  const ext = cardWith(
    { ops: ALL_OPS },
    {
      skills: [
        { id: 'workflow:deploy', name: 'Deploy the service', tags: ['workflow'] },
        { id: 'summarize', name: 'summarize', tags: ['chat'] },
      ],
    },
  );
  const m = new Mirror();
  m.setSession(session(pub, ext));
  // The extended card lists this caller's workflows; status names others.
  m.bootstrap({ workflows: [{ name: 'triage' }] });
  const s = m.getState();
  assert.deepEqual(workflowNames(s), ['deploy']);
  assert.deepEqual(m.workflows(), ['deploy']);
  assert.deepEqual(suggest('/dep', s).map((x) => x.label), ['/deploy']);
  // Without an extended card, status.workflows is the list.
  const plain = new Mirror();
  plain.setSession(session(cardWith({ ops: ALL_OPS })));
  plain.bootstrap({ workflows: [{ name: 'triage' }] });
  assert.deepEqual(workflowNames(plain.getState()), ['triage']);
});

test('# suggests tasks/conversations, $ suggests values', () => {
  const s = seeded().getState();
  const hash = suggest('#', s, 10);
  assert.ok(hash.some((x) => x.label === '#task-9' && x.hint === 'answer this task'), `${JSON.stringify(hash)}`);
  assert.ok(hash.some((x) => x.label === '#a2a-7' && x.hint === 'conversation'));
  const dollar = suggest('model is $mo', s);
  assert.deepEqual(dollar.map((x) => [x.label, x.hint]), [['$model', 'mock-1']]);
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
  // A task the mirror has not seen: the server mints task ids as UUIDv4, so
  // one in that shape continues the task rather than starting a conversation
  // under its name.
  const older = '0b7f7c4e-2d1a-4c3b-9e8f-5a6b7c8d9e0f';
  assert.deepEqual(prepare(`#${older} and then?`, s), { text: 'and then?', taskId: older });
  // A conversation the mirror holds is one, whatever its shape.
  const ctxUuid = '6f1e2d3c-4b5a-4987-a6b5-c4d3e2f1a0b9';
  const m2 = seeded();
  m2.bootstrap({ conversations: [{ id: ctxUuid }] });
  assert.deepEqual(prepare(`#${ctxUuid} hi`, m2.getState()), { text: 'hi', contextId: ctxUuid });
  // A conversation is addressed by the name its owner sent, not by the
  // runtime's key: `#` offers the name, and the name routes to it even when
  // it is server-minted in a task id's shape.
  const m3 = seeded();
  m3.bootstrap({
    conversations: [
      { id: 'ctx-k', contextId: 'mine', messages: 2, turns: 1 },
      { id: 'ctx-u', contextId: ctxUuid, messages: 1, turns: 0 },
    ],
  });
  const s3 = m3.getState();
  const offered = suggest('#', s3, 10).filter((x) => x.hint === 'conversation').map((x) => x.label);
  assert.deepEqual(offered.sort(), [`#${ctxUuid}`, '#mine'].sort());
  assert.deepEqual(prepare('#mine hi', s3), { text: 'hi', contextId: 'mine' });
  assert.deepEqual(prepare(`#${ctxUuid} hi`, s3), { text: 'hi', contextId: ctxUuid });
  const note = conversationsNote(s3);
  assert.ok(note.includes('#mine  2 msgs · 1 turns · key ctx-k'), note);
  assert.ok(!note.includes('#ctx-k'), note);
  // Any other unknown name is a conversation the message may start.
  assert.deepEqual(prepare('#fresh-chat hi', s), { text: 'hi', contextId: 'fresh-chat' });
  // $ interpolation: known names only, $$ escapes, inline # untouched.
  const p3 = prepare('running $model on $instance costs $$5 for issue #42', s);
  assert.equal(p3.text, 'running mock-1 on box-1 costs $5 for issue #42');
  assert.equal(p3.taskId, undefined);
  // Unknown $word left alone.
  assert.equal(prepare('price is $unknownvar', s).text, 'price is $unknownvar');
});

test('$ values read the status document, then the card, and never make one up', () => {
  const m = new Mirror();
  m.setSession(session(cardWith({ ops: ['status'] })));
  // A non-operator's status: no counters. The card names the agent and its version.
  m.bootstrap({ instance: 'box-2' });
  let s = m.getState();
  assert.equal(prepare('$name $version on $instance', s).text, 'beacon 1.17.0 on box-2');
  // Unpublished: left as typed, not `0` or an empty string.
  assert.equal(prepare('$turns and $tokens and $model', s).text, '$turns and $tokens and $model');
  assert.deepEqual(suggest('$tu', s).map((x) => x.hint), ['value']);
  // The status document's version wins over the card's.
  m.bootstrap({ instance: 'box-2', version: '1.17.1', counters: { turns: 3, tokens_in: 5, tokens_out: 7 } });
  s = m.getState();
  assert.equal(prepare('$version $turns $tokens', s).text, '1.17.1 3 5/7');
});

test('a config feed event is noted, and $ values read the status document', () => {
  const m = seeded();
  m.apply({ seq: 5, ts: 5, kind: 'config', data: { paths: ['agent.approval'], source: 'admin.set' } });
  assert.ok(m.getState().transcript.some((e) => e.kind === 'info' && e.text.includes('agent.approval')));
  // The live status wins over the bootstrap once the feed publishes one.
  m.apply({ seq: 6, ts: 6, kind: 'status', data: { model: 'mock-2' } });
  assert.equal(prepare('on $model', m.getState()).text, 'on mock-2');
});

test('routeSend never answers a gate from another conversation', () => {
  const m = new Mirror();
  m.apply({ seq: 1, ts: 1, kind: 'task', data: task('task-a', 'ctx-a', 'TASK_STATE_INPUT_REQUIRED', 1) });
  m.apply({ seq: 2, ts: 2, kind: 'task', data: task('task-b-old', 'ctx-b', 'TASK_STATE_INPUT_REQUIRED', 2) });
  m.apply({ seq: 3, ts: 3, kind: 'task', data: task('task-b', 'ctx-b', 'TASK_STATE_INPUT_REQUIRED', 3) });
  m.apply({ seq: 4, ts: 4, kind: 'task', data: task('task-auth', 'ctx-c', 'TASK_STATE_AUTH_REQUIRED', 4) });
  // The NEWEST gate in the whole mirror belongs to ctx-b.
  const s = m.getState();
  const route = (input, current) => routeSend(prepare(input, s), s, current);

  // In ctx-a, a plain reply answers ctx-a's gate — not the newer one in ctx-b.
  assert.deepEqual(route('blue', 'ctx-a'), { taskId: 'task-a' });
  // In ctx-b, the newest of its own gates.
  assert.deepEqual(route('blue', 'ctx-b'), { taskId: 'task-b' });
  assert.equal(currentGate(s, 'ctx-b').id, 'task-b');
  // A conversation with no gate: just the conversation.
  assert.deepEqual(route('hello', 'ctx-d'), { contextId: 'ctx-d' });
  // A fresh conversation has no gate to answer, however many are open elsewhere.
  assert.deepEqual(route('hello', undefined), {});
  // `#ctx` addresses THAT conversation: its gate, never the current one's.
  assert.deepEqual(route('#ctx-d hello', 'ctx-a'), { contextId: 'ctx-d' });
  assert.deepEqual(route('#ctx-b blue', 'ctx-a'), { taskId: 'task-b' });
  // A named task is the person's choice, sent without a context to disagree with.
  assert.deepEqual(route('#task-a blue', 'ctx-b'), { taskId: 'task-a' });
  // AUTH_REQUIRED is not answered by a message.
  assert.equal(currentGate(s, 'ctx-c'), undefined);
  assert.deepEqual(route('hi', 'ctx-c'), { contextId: 'ctx-c' });
  // Never both ids.
  for (const [input, cur] of [['x', 'ctx-a'], ['#ctx-b x', 'ctx-a'], ['#task-a x', 'ctx-b'], ['x', 'ctx-d']]) {
    const r = route(input, cur);
    assert.ok(!(r.taskId !== undefined && r.contextId !== undefined), JSON.stringify(r));
  }
});

/** A client that records the sign-in calls it gets. */
function authSpy() {
  const calls = [];
  const rec = (name, reply) => async (...args) => {
    calls.push([name, ...args]);
    return reply;
  };
  return {
    calls,
    authDeviceApprove: rec('authDeviceApprove', { approved: { user_code: 'ABCD-EFGH', client_id: 'agentd-ui', scope: 'user', principal: 'user:alice', existing: false } }),
    authDeviceDeny: rec('authDeviceDeny', { denied: 1 }),
    authDevicePending: rec('authDevicePending', { pending: [{ user_code: 'ABCD-EFGH', client_id: 'evil\u001b[2J', scope: 'user', peer: '10.0.0.2' }] }),
    authSessions: rec('authSessions', { sessions: [] }),
    authSessionsRevoke: rec('authSessionsRevoke', { revoked: 2 }),
  };
}

test('/approve needs a name', async () => {
  const c = authSpy();
  // No name: the usage line, and nothing sent.
  const refused = await runAuthCommand('approve', ['ABCD-EFGH'], c);
  assert.equal(refused.error, true);
  assert.match(refused.text, /^usage: \/approve <code> <name>/);
  assert.deepEqual(c.calls, []);
  // A name outside the approval-name shape is refused the same way.
  for (const bad of [['ABCD-EFGH', 'Alice'], ['ABCD-EFGH', '-x'], ['ABCD-EFGH', 'a:b'], ['ABCD-EFGH', 'alice', 'agent'], ['ABCD-EFGH', 'alice', 'operator', 'x']]) {
    assert.ok('usage' in parseAuthCommand('approve', bad), bad.join(' '));
  }
  assert.deepEqual(c.calls, []);

  const ok = await runAuthCommand('approve', ['ABCD-EFGH', 'alice'], c);
  assert.deepEqual(c.calls, [['authDeviceApprove', 'ABCD-EFGH', 'alice', undefined]]);
  assert.equal(ok.text, 'approved ABCD-EFGH as user:alice (user)');
  // `operator` raises the scope; a declined confirmation sends nothing.
  await runAuthCommand('approve', ['WXYZ-1234', 'bob', 'operator'], c);
  assert.deepEqual(c.calls.at(-1), ['authDeviceApprove', 'WXYZ-1234', 'bob', 'operator']);
  const before = c.calls.length;
  const no = await runAuthCommand('approve', ['WXYZ-1234', 'bob'], c, async () => false);
  assert.equal(c.calls.length, before);
  assert.match(no.text, /^not approved/);
});

test('the other sign-in commands route to their client calls', async () => {
  const c = authSpy();
  await runAuthCommand('deny', ['all'], c);
  await runAuthCommand('deny', ['ABCD-EFGH'], c);
  await runAuthCommand('revoke', ['ds_0123456789abcdef'], c);
  await runAuthCommand('revoke', ['name', 'alice'], c);
  await runAuthCommand('revoke', ['all'], c);
  assert.deepEqual(c.calls, [
    ['authDeviceDeny', { all: true }],
    ['authDeviceDeny', { userCode: 'ABCD-EFGH' }],
    ['authSessionsRevoke', { sid: 'ds_0123456789abcdef' }],
    ['authSessionsRevoke', { name: 'alice' }],
    ['authSessionsRevoke', { all: true }],
  ]);
  // A requester-chosen client_id is shown inert, never as terminal input.
  const devices = await runAuthCommand('devices', [], c);
  assert.ok(!devices.text.includes('\u001b') && devices.text.includes('evil\\u001b[2J'), devices.text);
  // Wrong shapes answer locally; a non-auth word is not theirs.
  const before = c.calls.length;
  for (const [cmd, args] of [['revoke', []], ['revoke', ['name']], ['deny', []], ['sessions', ['x']]]) {
    assert.equal((await runAuthCommand(cmd, args, c)).error, true, `${cmd} ${args.join(' ')}`);
  }
  assert.equal(c.calls.length, before);
  assert.equal(await runAuthCommand('status', [], c), null);
});

test('the activity line says what the agent is doing, for how long, at what cost', async () => {
  const { activityLine, elapsed, tokens } = await import('../dist/client/index.js');
  const now = 1_000_000;
  const base = { id: '7', round: 1, tokens_in: 0, tokens_out: 0, started_ms: now - 12_000, updated_ms: now };
  // Thinking, with elapsed only (no tokens spent yet).
  assert.equal(activityLine({ ...base, phase: 'thinking' }, now), 'thinking · 12s');
  // A tool names itself and the spend shows.
  assert.equal(
    activityLine({ ...base, phase: 'tool', tool: 'read_file', tokens_in: 900, tokens_out: 300 }, now),
    'read_file · 12s · 1.2k tok',
  );
  // A later round is worth showing; a parked unit says what it waits on.
  assert.match(activityLine({ ...base, phase: 'thinking', round: 3 }, now), /round 3$/);
  assert.match(activityLine({ ...base, phase: 'waiting', tool: 'subagent' }, now), /^waiting · subagent/);
  // No record yet ⇒ a bare label, never a crash.
  assert.equal(activityLine(undefined, now), 'working');
  // Formatting.
  assert.equal(elapsed(now - 8_000, now), '8s');
  assert.equal(elapsed(now - 74_000, now), '1m14s');
  assert.equal(elapsed(now - 3_723_000, now), '1h02m');
  assert.deepEqual([tokens(940), tokens(1200), tokens(18_000), tokens(250_000)], ['940', '1.2k', '18k', '250k']);
});

test('the mirror folds activity events and finds the record for a task', async () => {
  const { Mirror } = await import('../dist/client/index.js');
  const m = new Mirror();
  m.apply({ seq: 1, ts: 1, kind: 'activity', data: { id: '7', task: 't1', phase: 'thinking', round: 1, tokens_in: 10, tokens_out: 5, started_ms: 1, updated_ms: 1 } });
  assert.equal(m.activityFor('t1').phase, 'thinking');
  // A later frame replaces it in place (one record per unit).
  m.apply({ seq: 2, ts: 2, kind: 'activity', data: { id: '7', task: 't1', phase: 'tool', tool: 'grep', round: 1, tokens_in: 10, tokens_out: 5, started_ms: 1, updated_ms: 2 } });
  assert.equal(m.getState().activity.size, 1);
  assert.equal(m.activityFor('t1').tool, 'grep');
  // Unknown task falls back to the newest record; removal clears it.
  assert.equal(m.activityFor('nope').id, '7');
  m.apply({ seq: 3, ts: 3, kind: 'activity.removed', data: { id: '7' } });
  assert.equal(m.getState().activity.size, 0);
  assert.equal(m.activityFor('t1'), undefined);
});
