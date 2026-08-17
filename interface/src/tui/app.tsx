// SPDX-License-Identifier: AGPL-3.0-only
/**
 * The TUI shell: a thin renderer over `the client core`'s {@link Mirror}. All
 * state lives in the daemon; this component holds only view state (which
 * screen, which selection, what's typed). Screens: chat · tasks · subagents ·
 * debug. The chrome (top/bottom edges) renders whatever `interface.display`
 * declares; the composer speaks `/` (commands + workflows), `@` (skills),
 * `#` (task/conversation targets) and `$` (live values).
 */
import React, {
  useCallback,
  useEffect,
  useMemo,
  useRef,
  useState,
  useSyncExternalStore,
} from 'react';
import { Box, Text, useApp, useInput, useStdin, useWindowSize } from 'ink';
import TextInput from 'ink-text-input';
import {
  AgentdClient,
  Json,
  Mirror,
  Observation,
  RpcError,
  Suggestion,
  TERMINAL_STATES,
  activityLine,
  applySuggestion,
  prepare,
  suggest,
  workflowNames,
} from '../client/index.js';
import { theme } from './theme.js';
import { Transcript } from './parts/transcript.js';
import { TaskList } from './parts/tasks.js';
import { DebugScreen } from './parts/debug.js';
import { DEFAULT_BOTTOM, DEFAULT_TOP, Edge } from './parts/chrome.js';
import { SubagentDetail, SubagentList } from './parts/subagents.js';

export interface AppProps {
  endpoint: string;
  bearer?: string;
  /** Ask for the debug screen up front (still gated by the daemon). */
  debug?: boolean;
  /**
   * Fullscreen (the alternate screen) — the default. The app then owns the
   * scroll, because the alternate screen has no scrollback of its own.
   * `--inline` turns this off and hands history back to the terminal.
   */
  fullscreen?: boolean;
  /** Injection seam for tests. */
  client?: AgentdClient;
  mirror?: Mirror;
  /** Skip starting the observation loop (tests drive the mirror directly). */
  observe?: boolean;
}

type Screen = 'chat' | 'tasks' | 'subagents' | 'debug';

export function App(props: AppProps): React.JSX.Element {
  const { exit } = useApp();
  // Without an interactive terminal (piped/CI) the TUI degrades to a live
  // read-only view — the daemon doesn't care; input just has nowhere to come
  // from. NB: ink reports `stdin.isTTY`, which is UNDEFINED (not false) for a
  // pipe, and `useInput` skips only on a strict `false` — coerce.
  const { isRawModeSupported: rawMode } = useStdin();
  const isRawModeSupported = rawMode === true;
  const client = useMemo(
    () => props.client ?? new AgentdClient({ url: props.endpoint, bearer: props.bearer }),
    [props.client, props.endpoint, props.bearer],
  );
  const mirror = useMemo(() => props.mirror ?? new Mirror(), [props.mirror]);
  useSyncExternalStore(mirror.subscribe, mirror.getVersion);
  const s = mirror.getState();

  const fullscreen = props.fullscreen !== false && isRawModeSupported;
  const { rows, columns } = useWindowSize();
  const [screen, setScreen] = useState<Screen>(props.debug ? 'debug' : 'chat');
  /** Entries hidden below the viewport (0 = following the live end). */
  const [scroll, setScroll] = useState(0);
  const [input, setInput] = useState('');
  const [selected, setSelected] = useState(0);
  const [sugIndex, setSugIndex] = useState(0);
  const [subDetail, setSubDetail] = useState<{ handle: string; detail: Json | null } | null>(null);
  const [spin, setSpin] = useState(0);
  const [logLines, setLogLines] = useState<Json[]>([]);
  const logCursor = useRef(0);
  const ctxRef = useRef<string | undefined>(undefined);
  const inputTaskRef = useRef<string | undefined>(undefined);

  // The observation loop (feed-first, poll fallback).
  useEffect(() => {
    if (props.observe === false) return;
    const obs = new Observation(client, mirror);
    obs.start();
    return () => obs.stop();
  }, [client, mirror, props.observe]);

  const active = mirror.activeTasks();
  const suggestions: Suggestion[] = screen === 'chat' ? suggest(input, s) : [];

  // Spinner ticks only while something is actually working — it also drives
  // the working row's elapsed clock (nothing is streamed for that).
  useEffect(() => {
    if (active.length === 0) return;
    const t = setInterval(() => setSpin((n) => n + 1), 90);
    return () => clearInterval(t);
  }, [active.length > 0]);

  useEffect(() => setSugIndex(0), [input]);

  // Scrolled up? New entries must not yank the view — hold position by
  // counting them into the offset. At the bottom (offset 0) we follow.
  const lastLen = useRef(s.transcript.length);
  useEffect(() => {
    const grew = s.transcript.length - lastLen.current;
    lastLen.current = s.transcript.length;
    if (grew > 0 && scroll > 0) setScroll((o) => o + grew);
  }, [s.transcript.length, scroll]);

  // The debug log tail: poll the ring (cursored) while the pane is visible.
  useEffect(() => {
    if (screen !== 'debug' || !s.info?.debug) return;
    let alive = true;
    const tick = async () => {
      try {
        const r = (await client.debugEvents(logCursor.current, 100)) as { [k: string]: Json };
        if (!alive) return;
        const events = Array.isArray(r.events) ? r.events : [];
        if (events.length > 0) {
          logCursor.current = (r.newest_seq as number) ?? logCursor.current;
          setLogLines((prev) => [...prev, ...events].slice(-200));
        }
      } catch {
        /* the pane just stops filling */
      }
    };
    void tick();
    const t = setInterval(tick, 1000);
    return () => {
      alive = false;
      clearInterval(t);
    };
  }, [screen, s.info?.debug, client]);

  // Track the newest input-required gate so a plain reply answers it.
  useEffect(() => {
    const gate = active.find((t) => t.state === 'TASK_STATE_INPUT_REQUIRED');
    inputTaskRef.current = gate?.id;
  });

  // Rows the body may use: the terminal minus the chrome (top edge, composer,
  // suggestions, bottom edge — which wraps on narrow terminals).
  const bodyRows = Math.max(
    3,
    rows -
      (3 +
        (suggestions.length > 0 ? 1 : 0) +
        // The bottom edge wraps to a second line only on a narrow terminal.
        (columns < 100 && (s.info?.display?.bottom?.length ?? 8) > 6 ? 1 : 0)),
  );

  const submit = useCallback(
    async (raw: string) => {
      const trimmed = raw.trim();
      setInput('');
      if (trimmed.length === 0) return;
      if (trimmed.startsWith('/')) {
        await runSlash(trimmed);
        return;
      }
      try {
        // `#target` routing + `$value` interpolation (shared composer rules).
        const p = prepare(trimmed, s);
        const gate = p.taskId ?? inputTaskRef.current;
        const sent = await client.send(p.text, {
          contextId: p.contextId ?? ctxRef.current,
          taskId: gate,
        });
        inputTaskRef.current = undefined;
        if (sent.task) {
          ctxRef.current = sent.task.contextId || ctxRef.current;
          mirror.adoptTasks([sent.task]);
        }
        mirror.localEcho(sent.messageId, sent.task?.contextId ?? ctxRef.current, p.text, sent.task?.id);
      } catch (e) {
        mirror.note(e instanceof Error ? e.message : String(e), 'error');
      }
    },
    [client, mirror, s],
  );

  const runSlash = useCallback(
    async (line: string) => {
      const [cmd, ...rest] = line.slice(1).split(/\s+/);
      const arg = rest.join(' ');
      try {
        switch (cmd) {
          case 'help':
            mirror.note(
              '/new · /tasks · /subagents · /debug · /status · /config [path] · /set · /workflow <name> · /signal <name> · /send <handle> <msg> · /pause [run] · /resume [run] · /plan · /cancel [task] · /pair · /drain · /quit — plus @skill, #target, $value in messages',
            );
            break;
          case 'new':
            ctxRef.current = undefined;
            mirror.note('new conversation');
            break;
          case 'tasks':
            setScreen('tasks');
            break;
          case 'subagents':
            setScreen('subagents');
            break;
          case 'debug':
            setScreen('debug');
            break;
          case 'chat':
            setScreen('chat');
            break;
          case 'status': {
            const st = (await client.status()) as { [k: string]: Json };
            mirror.bootstrap(st);
            mirror.note(
              `runs ${Array.isArray(st.runs) ? st.runs.length : 0} · conversations ${Array.isArray(st.conversations) ? st.conversations.length : 0} · subagents ${Array.isArray(st.subagents) ? st.subagents.length : 0} · draining ${st.draining}`,
            );
            break;
          }
          case 'config': {
            const cfg = await client.config();
            if (arg) {
              // One path: walk the effective document.
              let v: Json = (cfg as { config?: Json }).config ?? cfg;
              for (const part of arg.split('.')) {
                v = (v as { [k: string]: Json } | null)?.[part] ?? null;
              }
              mirror.note(`${arg} = ${JSON.stringify(v)}`);
            } else {
              mirror.note(
                `${JSON.stringify((cfg as { config?: Json }).config ?? cfg, null, 1).slice(0, 2000)}\nrun /config <path> for one value · /set for the runtime-settable knobs`,
              );
            }
            break;
          }
          case 'set': {
            const [path, ...valueParts] = rest;
            if (!path || valueParts.length === 0) {
              mirror.note('usage: /set <path> <value> — e.g. /set interface.debug true', 'error');
              break;
            }
            const rawVal = valueParts.join(' ');
            let value: Json;
            try {
              value = JSON.parse(rawVal) as Json;
            } catch {
              value = rawVal;
            }
            const r = (await client.configSet(path, value)) as { [k: string]: Json };
            mirror.note(`set ${path} = ${JSON.stringify((r.set as { [k: string]: Json })?.value ?? value)}`);
            break;
          }
          case 'signal': {
            const [name, run] = rest;
            if (!name) {
              mirror.note('usage: /signal <name> [run]', 'error');
              break;
            }
            const r = (await client.signal(name, undefined, run)) as { [k: string]: Json };
            mirror.note(`signal ${name} → delivered ${(r as { delivered?: number }).delivered ?? '?'}`);
            break;
          }
          case 'send': {
            const [handle, ...msg] = rest;
            if (!handle || msg.length === 0) {
              mirror.note('usage: /send <handle> <message>', 'error');
              break;
            }
            await client.subagentSend(handle, msg.join(' '));
            mirror.note(`sent to ${handle}`);
            break;
          }
          case 'pause': {
            const r = (await client.pause(arg || undefined)) as { [k: string]: Json };
            mirror.note(arg ? `paused ${arg}` : 'instance paused — /resume to release');
            void r;
            break;
          }
          case 'resume': {
            await client.resume(arg || undefined);
            mirror.note(arg ? `resumed ${arg}` : 'instance resumed');
            break;
          }
          case 'conversations': {
            const convs = [...s.conversations.values()] as { [k: string]: Json }[];
            if (convs.length === 0) {
              mirror.note('no conversations yet');
              break;
            }
            const lines = convs
              .map((c) => `#${c.id}  ${c.messages ?? 0} msgs · ${c.turns ?? 0} turns${c.principal ? ` · ${c.principal}` : ''}`)
              .join('\n');
            mirror.note(`conversations:\n${lines}\nstart a message with #<id> to address one`);
            break;
          }
          case 'plan': {
            const p = (await client.planGet(arg || undefined)) as { [k: string]: Json };
            mirror.note(`plan: ${JSON.stringify(p.plan ?? null).slice(0, 800)}`);
            break;
          }
          case 'pair': {
            const p = await client.pairingCode();
            mirror.note(
              `pairing code: ${p.code}  (valid ${Math.ceil(p.expires_in_ms / 1000)}s, role ${p.role}, ${p.sessions} live sessions)\nconnect with: agentd-tui --endpoint ${props.endpoint} --code ${p.code}  ·  or enter it in the web UI`,
            );
            break;
          }
          case 'workflow': {
            if (!arg) {
              mirror.note('usage: /workflow <name>', 'error');
              break;
            }
            const r = await client.workflowRun(arg);
            mirror.note(`workflow ${arg} → ${r.task?.id ?? '?'}`);
            break;
          }
          case 'cancel': {
            const id = arg || active[0]?.id;
            if (!id) {
              mirror.note('nothing to cancel');
              break;
            }
            const t = await client.cancelTask(id);
            if (t) mirror.adoptTasks([t]);
            mirror.note(`cancelled ${id}`);
            break;
          }
          case 'drain':
            await client.drain();
            mirror.note('draining requested');
            break;
          case 'quit':
          case 'exit':
            exit();
            break;
          default: {
            // Not a system command: a workflow shortcut (`/deploy` ⇒ run it).
            if (workflowNames(s).includes(cmd)) {
              const r = await client.workflowRun(cmd);
              mirror.note(`workflow ${cmd} → ${r.task?.id ?? '?'}`);
            } else {
              mirror.note(`unknown command /${cmd} — /help`, 'error');
            }
          }
        }
      } catch (e) {
        const msg = e instanceof RpcError ? `${e.message} (${e.code})` : String(e);
        mirror.note(msg, 'error');
      }
    },
    [client, mirror, exit, active, s, props.endpoint],
  );

  const openSubagent = useCallback(
    (handle: string) => {
      setSubDetail({ handle, detail: null });
      void client
        .subagentGet(handle)
        .then((d) => setSubDetail((cur) => (cur?.handle === handle ? { handle, detail: d } : cur)))
        .catch(() => {
          /* summary-only (debug off) — the view says so */
        });
    },
    [client],
  );

  useInput(
    (ch, key) => {
      // Suggestions capture Tab/↑/↓ while visible (chat only).
      if (screen === 'chat' && suggestions.length > 0) {
        if (key.tab || key.rightArrow) {
          setInput(applySuggestion(input, suggestions[Math.min(sugIndex, suggestions.length - 1)]));
          return;
        }
        if (key.upArrow) {
          setSugIndex((i) => (i + suggestions.length - 1) % suggestions.length);
          return;
        }
        if (key.downArrow) {
          setSugIndex((i) => (i + 1) % suggestions.length);
          return;
        }
      }
      // Scrolling the transcript (fullscreen owns its own scrollback).
      if (screen === 'chat' && fullscreen) {
        const page = Math.max(1, Math.floor(bodyRows / 2));
        if (key.pageUp) {
          setScroll((o) => Math.min(Math.max(0, s.transcript.length - 1), o + page));
          return;
        }
        if (key.pageDown) {
          setScroll((o) => Math.max(0, o - page));
          return;
        }
      }
      if (key.tab) {
        setScreen((cur) => {
          const order: Screen[] = s.info?.debug
            ? ['chat', 'tasks', 'subagents', 'debug']
            : ['chat', 'tasks', 'subagents'];
          setSubDetail(null);
          setSelected(0);
          return order[(order.indexOf(cur) + 1) % order.length];
        });
        return;
      }
      if (key.escape) {
        if (subDetail) {
          setSubDetail(null);
          return;
        }
        const newest = active[0];
        if (newest && !TERMINAL_STATES.has(newest.state)) {
          void client.cancelTask(newest.id).then(
            (t) => {
              if (t) mirror.adoptTasks([t]);
              mirror.note(`cancelled ${newest.id}`);
            },
            () => mirror.note(`cancel ${newest.id} failed`, 'error'),
          );
        }
        return;
      }
      if (screen === 'tasks') {
        const all = mirror.allTasks();
        if (key.upArrow) setSelected((i) => Math.max(0, i - 1));
        else if (key.downArrow) setSelected((i) => Math.min(all.length - 1, i + 1));
        else if (ch === 'c') {
          const t = all[selected];
          if (t) void client.cancelTask(t.id).catch(() => {});
        } else if (key.return) {
          const t = all[selected];
          if (t) {
            mirror.note(
              `task ${t.id}: ${t.state}${t.artifacts[0] ? ` — ${t.artifacts[0].slice(0, 400)}` : ''}`,
            );
            setScreen('chat');
          }
        }
      }
      if (screen === 'subagents' && !subDetail) {
        const subs = [...s.subagents.keys()];
        if (key.upArrow) setSelected((i) => Math.max(0, i - 1));
        else if (key.downArrow) setSelected((i) => Math.min(subs.length - 1, i + 1));
        else if (key.return) {
          const handle = subs[selected];
          if (handle) openSubagent(handle);
        }
      } else if (screen === 'subagents' && subDetail && (key.backspace || key.delete)) {
        setSubDetail(null);
      }
    },
    { isActive: isRawModeSupported },
  );

  // ---- render ------------------------------------------------------------

  const info = s.info;
  const top = info?.display?.top ?? DEFAULT_TOP;
  const bottom = info?.display?.bottom ?? DEFAULT_BOTTOM;
  const chrome = { s, endpoint: props.endpoint, screen, active: active.length };
  // The live working line (RFC 0032 §17): what the daemon is doing, ticking
  // its own clock off the activity record's `started_ms` (the spinner interval
  // already re-renders us, so elapsed advances for free).
  const workingRow =
    active.length > 0
      ? {
          text:
            active[0].state === 'TASK_STATE_INPUT_REQUIRED'
              ? 'waiting for your answer'
              : activityLine(mirror.activityFor(active[0].id)) +
                (active.length > 1 ? ` · ${active.length} tasks` : ''),
          frame: spin,
        }
      : null;

  return (
    <Box flexDirection="column" height={fullscreen ? rows : undefined}>
      <Edge items={top} ctx={chrome} />
      {screen === 'chat' ? (
        <Transcript
          entries={s.transcript}
          working={workingRow}
          viewport={fullscreen ? { rows: bodyRows, columns, offset: scroll } : undefined}
        />
      ) : screen === 'tasks' ? (
        <TaskList tasks={mirror.allTasks()} selected={selected} />
      ) : screen === 'subagents' ? (
        subDetail ? (
          <SubagentDetail
            handle={subDetail.handle}
            summary={s.subagents.get(subDetail.handle) as { [k: string]: Json } | undefined}
            detail={subDetail.detail as { [k: string]: Json } | null}
            debug={info?.debug === true}
          />
        ) : (
          <SubagentList s={s} selected={selected} />
        )
      ) : (
        <DebugScreen s={s} logLines={logLines} />
      )}
      {screen === 'chat' ? (
        isRawModeSupported ? (
          <Box flexDirection="column">
            <Box flexDirection="row">
              <Text color={theme.accent} bold>
                {'› '}
              </Text>
              <TextInput value={input} onChange={setInput} onSubmit={(v: string) => void submit(v)} />
            </Box>
            {suggestions.length > 0 ? (
              <Box flexDirection="row" gap={2} marginLeft={2}>
                {suggestions.map((sug, i) => (
                  <Text
                    key={sug.label}
                    color={i === sugIndex ? theme.accent : theme.dim}
                    bold={i === sugIndex}
                  >
                    {sug.label}
                    <Text color={theme.dim}> {sug.hint}</Text>
                  </Text>
                ))}
              </Box>
            ) : null}
          </Box>
        ) : (
          <Text color={theme.dim}>read-only (no interactive terminal)</Text>
        )
      ) : null}
      <Edge items={bottom} ctx={chrome} />
    </Box>
  );
}
