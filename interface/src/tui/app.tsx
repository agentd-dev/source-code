// SPDX-License-Identifier: AGPL-3.0-only
/**
 * The TUI shell: a thin renderer over `the client core`'s {@link Mirror}. All
 * state lives in the daemon; this component holds only view state (which
 * screen, which selection, what's typed), the person's credential, and the
 * layout. Screens: chat · tasks · subagents · debug. The chrome (top/bottom
 * edges) renders the client's own layout (chrome.ts); the composer speaks `/`
 * (the commands the card backs + workflows), `@` (skills), `#`
 * (task/conversation targets) and `$` (live values).
 *
 * A failure retrying cannot fix — the credential refused, the principal not
 * allowed, an agent this client cannot speak to — stops observation, and a
 * banner says what happened and what would fix it, instead of a status dot
 * quietly reading "closed".
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
import { MultilineInput, type EditState } from './parts/input.js';
import {
  AgentdClient,
  DEFAULT_LAYOUT,
  DISPLAY_ITEMS,
  Json,
  Mirror,
  Observation,
  RpcError,
  SYSTEM_COMMANDS,
  Suggestion,
  TERMINAL_STATES,
  activityLine,
  applySuggestion,
  askAnswer,
  askForm,
  availableCommands,
  commandHelp,
  currentGate,
  describe,
  deviceLogin,
  introspectionOn,
  openSession,
  parseAuthCommand,
  parseLayout,
  prepare,
  revocationEndpointOf,
  revokeToken,
  routeSend,
  runAuthCommand,
  suggest,
  workflowNames,
  DAEMON_KEYS,
} from '../client/index.js';
import type { Credential, Failure, Layout, LoginOption, MirrorState, TaskView } from '../client/index.js';
import { GatePrompt, gateOptions, gateRows } from './parts/gate.js';
import { theme } from './theme.js';
import { Transcript } from './parts/transcript.js';
import { TaskList } from './parts/tasks.js';
import { DebugScreen, settable } from './parts/debug.js';
import { Edge } from './parts/chrome.js';
import { StatusBar } from './parts/statusbar.js';
import { SubagentDetail, SubagentList } from './parts/subagents.js';
import { TUI_CLIENT_ID, chooseSignIn, launchTokenUrl } from './args.js';

/** How the credential the TUI holds was obtained; it decides what an ended session says. */
export type SignIn = 'launch' | 'bearer' | 'device';

export interface AppProps {
  /** Where the agent is: its base URL or its card's URL. */
  configured: string;
  /** The person's credential for that endpoint. */
  credential?: Credential;
  /** Where {@link credential} came from. */
  signIn?: SignIn;
  /** The chrome (`--top`/`--bottom`); the TUI's default when absent. */
  layout?: Layout;
  /** Speak core A2A only (`--no-extensions`). */
  noExtensions?: boolean;
  /** Ask for the debug screen up front (still gated by the daemon). */
  debug?: boolean;
  /**
   * Fullscreen (the alternate screen) — the default. The app then owns the
   * scroll, because the alternate screen has no scrollback of its own.
   * `--inline` turns this off and hands history back to the terminal.
   */
  fullscreen?: boolean;
  /** Injection seam for tests (otherwise the client comes from discovery). */
  client?: AgentdClient;
  mirror?: Mirror;
  /** Skip starting the observation loop (tests drive the mirror directly). */
  observe?: boolean;
}

/** What an ended launch session says: it has no expiry, so it ended because someone ended it. */
export const LAUNCH_SESSION_ENDED = 'session ended — restart `agentd tui`, or sign in with --login';

/**
 * The banner for a failure that stopped observation: what happened, then
 * what would fix it. A launch session is the launcher's to give, so when it
 * ends the fix is the launcher (or a sign-in of the person's own), not a
 * retry; a device session can sign in again from here.
 */
export function terminalBanner(f: Failure, signIn: SignIn | undefined, login: readonly LoginOption[]): string[] {
  const device = login.some((o) => o.method === 'device');
  switch (f.kind) {
    case 'unauthenticated':
      if (signIn === 'launch') return [LAUNCH_SESSION_ENDED];
      if (signIn === 'device') return [describe(f), device ? '/login to sign in again' : 'restart agentd-tui with --login'];
      if (signIn === 'bearer') {
        return [describe(f), `the bearer token was refused — check --bearer-file / AGENTD_BEARER${device ? ', or /login' : ''}`];
      }
      return [describe(f), device ? '/login to sign in' : 'restart agentd-tui with --bearer-file PATH'];
    case 'forbidden':
      return [describe(f), 'this principal is not allowed to observe this agent — an operator can grant it'];
    default:
      return [describe(f), 'this client cannot work with this agent — check --endpoint'];
  }
}

/** A failed command, as one line: the agent's words and code, or the error's message. */
function failureText(e: unknown): string {
  if (e instanceof RpcError) return `${e.message} (${e.code})`;
  return e instanceof Error ? e.message : String(e);
}

/** The task a plain reply in conversation `ctx` cannot answer: the newest AUTH_REQUIRED one there. */
function authWait(s: MirrorState, ctx: string | undefined): TaskView | undefined {
  if (ctx === undefined) return undefined;
  let found: TaskView | undefined;
  for (const t of s.tasks.values()) {
    if (t.state !== 'TASK_STATE_AUTH_REQUIRED' || t.contextId !== ctx) continue;
    if (found === undefined || t.updated > found.updated) found = t;
  }
  return found;
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
  // The client exists once discovery settled where JSON-RPC goes and what
  // the card lets this client call; until then a command has nowhere to go.
  const [discovered, setDiscovered] = useState<AgentdClient | null>(null);
  const client = props.client ?? discovered;
  const need = useCallback((): AgentdClient => {
    if (!client) throw new Error('not connected yet');
    return client;
  }, [client]);
  const mirror = useMemo(() => props.mirror ?? new Mirror(), [props.mirror]);
  useSyncExternalStore(mirror.subscribe, mirror.getVersion);
  const s = mirror.getState();

  // The credential is the person's, and /login and /logout change it; the
  // observation below restarts with whatever it now is.
  const [credential, setCredential] = useState<Credential | undefined>(props.credential);
  const [signIn, setSignIn] = useState<SignIn | undefined>(props.signIn);
  const signInRef = useRef(signIn);
  signInRef.current = signIn;
  const loginAbort = useRef<AbortController | null>(null);
  useEffect(() => () => loginAbort.current?.abort(), []);
  const [layout, setLayout] = useState<Layout>(
    () => props.layout ?? { top: [...DEFAULT_LAYOUT.tui.top], bottom: [...DEFAULT_LAYOUT.tui.bottom] },
  );
  /** Why observation stopped for good, and what would fix it. */
  const [banner, setBanner] = useState<string[] | null>(null);
  /** A yes/no question waiting for one keystroke (an approval). */
  const [ask, setAsk] = useState<{ question: string; resolve: (yes: boolean) => void } | null>(null);
  const obsRef = useRef<Observation | null>(null);

  const fullscreen = props.fullscreen !== false && isRawModeSupported;
  const { rows, columns } = useWindowSize();
  const [screen, setScreen] = useState<Screen>(props.debug ? 'debug' : 'chat');
  /** Entries hidden below the viewport (0 = following the live end). */
  const [scroll, setScroll] = useState(0);
  const [input, setInput] = useState('');
  // The composer is a multiline editor, so the cursor is app state too.
  const [cursor, setCursor] = useState(0);
  const [selected, setSelected] = useState(0);
  const [sugIndex, setSugIndex] = useState(0);
  const [subDetail, setSubDetail] = useState<{ handle: string; detail: Json | null } | null>(null);
  // Stopping a subagent is not undoable, so it asks first — one keystroke to
  // confirm, any other key to back out.
  const [killAsk, setKillAsk] = useState<string | null>(null);
  const [spin, setSpin] = useState(0);
  const [logLines, setLogLines] = useState<Json[]>([]);
  const logCursor = useRef(0);
  /** The conversation this person is in; a fresh TUI is in none yet. */
  const [ctx, setCtx] = useState<string | undefined>(undefined);

  // The observation loop (discovery, then the feed or core polling).
  useEffect(() => {
    if (props.observe === false) return;
    setBanner(null);
    const obs = new Observation(
      {
        configured: props.configured,
        credential,
        noExtensions: props.noExtensions,
        onSession: (_, c) => setDiscovered(c),
        onTerminal: (f) => setBanner(terminalBanner(f, signInRef.current, mirror.getState().session?.caps.login ?? [])),
      },
      mirror,
    );
    obsRef.current = obs;
    obs.start();
    return () => {
      obsRef.current = null;
      obs.stop();
    };
  }, [props.configured, credential, props.noExtensions, mirror, props.observe]);
  const debugOn = introspectionOn(s);

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
    if (screen !== 'debug' || !debugOn || !client) return;
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
  }, [screen, debugOn, client]);

  // The gate a plain reply answers: this conversation's newest INPUT_REQUIRED
  // task, never another conversation's (routeSend reads the same rule). A
  // task waiting on AUTH_REQUIRED is shown, but nothing typed answers it.
  const gate = currentGate(s, ctx);
  const waiting = gate ?? authWait(s, ctx);
  // The form the gate's schema describes. A gate that declared no schema keeps
  // the old behaviour exactly: type an answer into the composer.
  const gateForm = useMemo(() => askForm(gate?.askSchema), [gate?.id, gate?.askSchema]);
  const [gatePick, setGatePick] = useState<string[]>([]);
  // A new gate must not inherit the previous one's selection.
  useEffect(() => {
    setGatePick([]);
  }, [gate?.id]);

  // Rows the body may use: the terminal minus the chrome (top edge, composer,
  // suggestions, bottom edge — which wraps on narrow terminals). The
  // fullscreen composer is boxed, so it costs its two border rows too; get
  // this wrong and the viewport overflows and clips its own newest message.
  const composerRows = 1 + (props.fullscreen !== false && isRawModeSupported ? 2 : 0);
  // The gate's options sit between the transcript and the composer, so they
  // cost rows the transcript may not also use — one per option, one for the
  // hint, and one more for the free-text line when `other…` is selected.
  const promptRows = screen === 'chat' && waiting ? gateRows(waiting.state, gateForm, gatePick) : 0;
  // The banner is boxed: its lines plus the two border rows.
  const bannerRows = banner ? banner.length + 2 : 0;
  const bodyRows = Math.max(
    3,
    rows -
      (2 +
        composerRows +
        promptRows +
        bannerRows +
        (ask ? 1 : 0) +
        (suggestions.length > 0 ? 1 : 0) +
        // The bottom edge wraps to a second line only on a narrow terminal.
        (columns < 100 && layout.bottom.length > 6 ? 1 : 0)),
  );

  const submit = useCallback(
    async (raw: string) => {
      const trimmed = raw.trim();
      setInput('');
      setCursor(0);
      if (trimmed.length === 0) return;
      if (trimmed.startsWith('/')) {
        await runSlash(trimmed);
        return;
      }
      try {
        // `#target` routing + `$value` interpolation, then where it goes: a
        // named task, else this conversation — and its open gate, if any.
        const p = prepare(trimmed, s);
        const route = routeSend(p, s, ctx);
        const sent = await need().send(p.text, route);
        landed(sent, p.text, route.taskId);
      } catch (e) {
        mirror.note(failureText(e), 'error');
      }
    },
    [need, mirror, s, ctx],
  );

  /**
   * A send the agent accepted: echo it, then adopt what came back — the echo
   * first, so the task's history claims it instead of doubling it. An agent
   * may answer with a Message instead of a task; that is its reply, and the
   * conversation it names is where the person now is.
   */
  const landed = useCallback(
    (sent: { task: TaskView | null; reply?: Json; messageId: string }, text: string, taskId?: string) => {
      const replyCtx = (sent.reply as { contextId?: Json } | undefined)?.contextId;
      const next = sent.task?.contextId || (typeof replyCtx === 'string' && replyCtx !== '' ? replyCtx : undefined) || ctx;
      mirror.localEcho(sent.messageId, next, text, sent.task?.id ?? taskId);
      if (sent.task) {
        mirror.adoptTasks([sent.task]);
        // Core mode follows the tasks this client started first.
        obsRef.current?.track(sent.task.id);
      }
      if (sent.reply !== undefined) mirror.applyStream({ message: sent.reply });
      if (next !== ctx) setCtx(next);
    },
    [mirror, ctx],
  );

  /** Ask a yes/no question on the next keystroke; anything but `y` is no. */
  const confirm = useCallback(
    (question: string) => new Promise<boolean>((resolve) => setAsk({ question, resolve })),
    [],
  );

  /** Sign in with the device grant the card offers (`/login [user|operator]`). */
  const login = useCallback(
    async (scope: string | undefined) => {
      if (loginAbort.current) {
        mirror.note('a sign-in is already waiting for approval', 'error');
        return;
      }
      if (scope !== undefined && scope !== 'user' && scope !== 'operator') {
        mirror.note('usage: /login [user|operator]', 'error');
        return;
      }
      // The card this session came from — or, when observation stopped
      // before discovery settled, a fresh read of it: sign-in is how a
      // refused session recovers, so it cannot depend on that session.
      const caps = s.session?.caps ?? (await openSession(props.configured, { noExtensions: props.noExtensions })).caps;
      const choice = chooseSignIn(caps.login, true);
      if (choice.kind !== 'device') {
        mirror.note(choice.kind === 'refuse' ? choice.reason : 'nothing to sign in to', 'error');
        return;
      }
      const ac = new AbortController();
      loginAbort.current = ac;
      try {
        const c = await deviceLogin({
          flow: choice.flow,
          clientId: TUI_CLIENT_ID,
          scope,
          signal: ac.signal,
          onCode: (code) =>
            mirror.note(
              `to sign in, open ${code.verificationUriComplete ?? code.verificationUri} and enter ${code.userCode} — ` +
                `or an operator runs /approve ${code.userCode} <name>`,
            ),
        });
        setCredential(c);
        setSignIn('device');
        mirror.note(`signed in${c.scope ? ` (${c.scope})` : ''}`);
      } finally {
        loginAbort.current = null;
      }
    },
    [mirror, s, props.configured, props.noExtensions],
  );

  /**
   * Sign out: forget the credential and, for a session the daemon issued,
   * revoke it there (RFC 7009) so it is not left working in anyone's hands.
   */
  const logout = useCallback(async () => {
    if (!credential) {
      mirror.note('not signed in');
      return;
    }
    const token = credential.token;
    const issued = signIn === 'device' || signIn === 'launch';
    setCredential(undefined);
    setSignIn(undefined);
    if (!issued) {
      mirror.note('signed out (a bearer token is the daemon config\'s to revoke)');
      return;
    }
    const device = (s.session?.caps.login ?? []).find((o) => o.method === 'device');
    const flow =
      device?.method === 'device'
        ? { tokenUrl: device.flow.tokenUrl, oauth2MetadataUrl: device.flow.oauth2MetadataUrl }
        : { tokenUrl: launchTokenUrl(props.configured) };
    try {
      const url = await revocationEndpointOf(flow);
      if (url === undefined) {
        mirror.note('signed out here; the daemon lists no revocation endpoint, so the session ends when it expires', 'error');
        return;
      }
      await revokeToken(url, token, { clientId: TUI_CLIENT_ID });
      mirror.note('signed out; the session is revoked');
    } catch (e) {
      mirror.note(`signed out here, but revoking the session failed: ${failureText(e)}`, 'error');
    }
  }, [credential, signIn, mirror, s, props.configured]);

  const runSlash = useCallback(
    async (line: string) => {
      const [cmd, ...rest] = line.slice(1).split(/\s+/);
      const arg = rest.join(' ');
      // A command whose op the card does not list for this caller is not
      // sent: the agent would refuse it, and the refusal would only teach
      // the person to try things.
      const sys = SYSTEM_COMMANDS.find((c) => c.name === cmd);
      if (sys?.needs !== undefined && !availableCommands(s).includes(sys)) {
        mirror.note(`/${cmd} is not offered by this agent to you (its card does not list ${sys.needs})`, 'error');
        return;
      }
      // The sign-in administration commands parse and run in the composer
      // module, so both display clients answer them alike.
      if (parseAuthCommand(cmd, rest) !== null) {
        try {
          const out = await runAuthCommand(cmd, rest, need(), confirm);
          if (out !== null) mirror.note(out.text, out.error ? 'error' : 'info');
        } catch (e) {
          mirror.note(failureText(e), 'error');
        }
        return;
      }
      try {
        switch (cmd) {
          case 'help':
            mirror.note(commandHelp(s));
            break;
          case 'new':
            setCtx(undefined);
            mirror.note('new conversation');
            break;
          case 'layout': {
            const [edge, items] = rest;
            if (edge === undefined) {
              const known = Object.keys(DISPLAY_ITEMS).filter((k) => DISPLAY_ITEMS[k].surfaces.includes('tui'));
              mirror.note(
                `top: ${layout.top.join(',')}\nbottom: ${layout.bottom.join(',')}\n` +
                  `items: ${known.join(', ')}, memory:<key>\n/layout top|bottom <items,…> · /layout reset`,
              );
              break;
            }
            if (edge === 'reset' && items === undefined) {
              setLayout({ top: [...DEFAULT_LAYOUT.tui.top], bottom: [...DEFAULT_LAYOUT.tui.bottom] });
              mirror.note('layout reset');
              break;
            }
            if ((edge !== 'top' && edge !== 'bottom') || items === undefined || rest.length > 2) {
              mirror.note('usage: /layout [top|bottom <items,…> | reset]', 'error');
              break;
            }
            const parsed = parseLayout(items, 'tui');
            if (parsed.unknown.length > 0) {
              mirror.note(`unknown item(s): ${parsed.unknown.join(', ')} — /layout lists them`, 'error');
              break;
            }
            setLayout((cur) => ({ ...cur, [edge]: parsed.items }));
            mirror.note(`${edge}: ${parsed.items.join(',')}`);
            break;
          }
          case 'login':
            await login(rest[0]);
            break;
          case 'logout':
            await logout();
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
            const st = (await need().status()) as { [k: string]: Json };
            mirror.bootstrap(st);
            mirror.note(
              `runs ${Array.isArray(st.runs) ? st.runs.length : 0} · conversations ${Array.isArray(st.conversations) ? st.conversations.length : 0} · subagents ${Array.isArray(st.subagents) ? st.subagents.length : 0} · draining ${st.draining}`,
            );
            break;
          }
          case 'config': {
            const cfg = await need().config();
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
              mirror.note('usage: /set <path> <value> — one of the paths the agent lists as settable', 'error');
              break;
            }
            const rawVal = valueParts.join(' ');
            let value: Json;
            try {
              value = JSON.parse(rawVal) as Json;
            } catch {
              value = rawVal;
            }
            const r = (await need().adminSet(path, value)) as { [k: string]: Json } | null;
            mirror.note(`set ${path} = ${JSON.stringify(r?.value ?? value)}`);
            // What the card offers may have moved with it (introspection on,
            // a new settable path): re-read it.
            obsRef.current?.refresh();
            break;
          }
          case 'signal': {
            const [name, run] = rest;
            if (!name) {
              mirror.note('usage: /signal <name> [run]', 'error');
              break;
            }
            const r = (await need().signal(name, undefined, run)) as { [k: string]: Json };
            mirror.note(`signal ${name} → delivered ${(r as { delivered?: number } | null)?.delivered ?? '?'}`);
            break;
          }
          case 'send': {
            const [handle, ...msg] = rest;
            if (!handle || msg.length === 0) {
              mirror.note('usage: /send <handle> <message>', 'error');
              break;
            }
            await need().subagentSend(handle, msg.join(' '));
            mirror.note(`sent to ${handle}`);
            break;
          }
          case 'pause': {
            await need().pause(arg || undefined);
            mirror.note(arg ? `paused ${arg}` : 'instance paused — /resume to release');
            break;
          }
          case 'resume': {
            await need().resume(arg || undefined);
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
            const p = (await need().planGet(arg || undefined)) as { [k: string]: Json } | null;
            mirror.note(`plan: ${JSON.stringify(p?.plan ?? null).slice(0, 800)}`);
            break;
          }
          case 'workflow': {
            if (!arg) {
              mirror.note('usage: /workflow <name>', 'error');
              break;
            }
            const r = await need().workflowRun(arg);
            mirror.note(`workflow ${arg} → ${r.task?.id ?? '?'}`);
            break;
          }
          case 'cancel': {
            const id = arg || active[0]?.id;
            if (!id) {
              mirror.note('nothing to cancel');
              break;
            }
            const t = await need().cancelTask(id);
            if (t) mirror.adoptTasks([t]);
            mirror.note(`cancelled ${id}`);
            break;
          }
          case 'drain':
            await need().drain();
            mirror.note('draining requested');
            break;
          case 'quit':
          case 'exit':
            exit();
            break;
          default: {
            // Not a system command: a workflow shortcut (`/deploy` ⇒ run it).
            if (workflowNames(s).includes(cmd)) {
              const r = await need().workflowRun(cmd);
              mirror.note(`workflow ${cmd} → ${r.task?.id ?? '?'}`);
            } else {
              mirror.note(`unknown command /${cmd} — /help`, 'error');
            }
          }
        }
      } catch (e) {
        mirror.note(failureText(e), 'error');
      }
    },
    [need, mirror, exit, active, s, confirm, layout, login, logout],
  );

  const openSubagent = useCallback(
    (handle: string) => {
      setSubDetail({ handle, detail: null });
      if (!client) return;
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
      // A pending yes/no takes the next keystroke, whatever it is: `y` is
      // yes, anything else is no — an approval is never given by accident.
      if (ask) {
        setAsk(null);
        ask.resolve(ch === 'y' || ch === 'Y');
        return;
      }
      // A form-shaped gate takes the number keys, so picking an option is one
      // keystroke instead of typing its wording. Only when the composer is
      // EMPTY: someone mid-sentence typing "1" means the character, and
      // stealing it would be maddening.
      if (screen === 'chat' && gate && gateForm.kind !== 'text' && input.length === 0) {
        const rows = gateOptions(gateForm);
        const n = Number(ch);
        if (Number.isInteger(n) && n >= 1 && n <= rows.length) {
          const v = rows[n - 1];
          setGatePick((cur) =>
            gateForm.kind === 'many'
              ? cur.includes(v)
                ? cur.filter((x) => x !== v)
                : [...cur, v]
              : [v],
          );
          return;
        }
        if (key.return && gatePick.length > 0 && !gatePick.includes('__other__')) {
          const answer = askAnswer(gateForm, gatePick, '');
          const text = typeof answer === 'string' ? answer : JSON.stringify(answer);
          setGatePick([]);
          if (!client) return;
          void client
            .send(text, { taskId: gate.id })
            .then((sent) => landed(sent, text, gate.id))
            .catch((e: unknown) => mirror.note(failureText(e), 'error'));
          return;
        }
      }
      // Suggestions capture Tab/↑/↓ while visible (chat only).
      if (screen === 'chat' && suggestions.length > 0) {
        if (key.tab) {
          {
            const next = applySuggestion(
              input,
              suggestions[Math.min(sugIndex, suggestions.length - 1)],
            );
            setInput(next);
            setCursor(next.length);
          }
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
          const order: Screen[] = debugOn
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
        if (client && newest && !TERMINAL_STATES.has(newest.state)) {
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
          if (t && client) void client.cancelTask(t.id).catch(() => {});
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
      } else if (screen === 'subagents' && subDetail) {
        // Control lives on the thing being controlled. `m` opens the composer
        // pre-addressed to this subagent so a message is one keystroke away
        // rather than a `/send <handle> …` you have to retype the handle into;
        // `k` stops it — a subagent is a process the supervisor owns, so stop
        // means stop rather than a request the child can decline.
        if (key.backspace || key.delete || key.escape) setSubDetail(null);
        else if (ch === 'm') {
          const pre = `/send ${subDetail.handle} `;
          setScreen('chat');
          setInput(pre);
          setCursor(pre.length);
          setSubDetail(null);
        } else if (ch === 'k') setKillAsk(subDetail.handle);
        else if (killAsk && (ch === 'y' || ch === 'Y') && client) {
          const h = killAsk;
          setKillAsk(null);
          void client
            .subagentKill(h)
            .then(() => mirror.note(`stopped ${h}`))
            .catch((e: unknown) => mirror.note(`stop failed: ${String(e)}`));
        } else if (killAsk) setKillAsk(null);
      }
    },
    { isActive: isRawModeSupported },
  );

  // ---- render ------------------------------------------------------------

  const { top, bottom } = layout;
  const chrome = { s, endpoint: props.configured, screen, active: active.length };
  // The live working line (RFC 0032 §17): what the daemon is doing, ticking
  // its own clock off the activity record's `started_ms` (the spinner interval
  // already re-renders us, so elapsed advances for free).
  const workingRow =
    active.length > 0
      ? {
          text:
            active[0].state === 'TASK_STATE_INPUT_REQUIRED'
              ? // A form-shaped gate lists its options below, so the working
                // line does not repeat the wait.
                gateForm.kind === 'text'
                ? 'waiting for your answer'
                : 'waiting for your choice'
              : active[0].state === 'TASK_STATE_AUTH_REQUIRED'
                ? 'waiting for authorization'
                : activityLine(mirror.activityFor(active[0].id)) +
                  (active.length > 1 ? ` · ${active.length} tasks` : ''),
          frame: spin,
        }
      : null;

  return (
    <Box flexDirection="column" height={fullscreen ? rows : undefined}>
      <Edge items={top} ctx={chrome} />
      {banner ? (
        <Box flexDirection="column" borderStyle="round" borderColor={theme.error} paddingX={1}>
          {banner.map((line, i) => (
            <Text key={i} color={i === 0 ? theme.error : undefined} bold={i === 0}>
              {line}
            </Text>
          ))}
        </Box>
      ) : null}
      {screen === 'chat' ? (
        <Transcript
          entries={s.transcript}
          working={workingRow}
          viewport={fullscreen ? { rows: bodyRows, columns, offset: scroll } : undefined}
          gate={gate?.id}
        />
      ) : screen === 'tasks' ? (
        <TaskList tasks={mirror.allTasks()} selected={selected} />
      ) : screen === 'subagents' ? (
        subDetail ? (
          <SubagentDetail
            handle={subDetail.handle}
            killAsk={killAsk === subDetail.handle}
            summary={s.subagents.get(subDetail.handle) as { [k: string]: Json } | undefined}
            detail={subDetail.detail as { [k: string]: Json } | null}
            debug={debugOn}
            canSet={settable(s, DAEMON_KEYS.introspection)}
          />
        ) : (
          <SubagentList s={s} selected={selected} />
        )
      ) : (
        <DebugScreen s={s} logLines={logLines} />
      )}
      {screen === 'chat' && waiting ? (
        <GatePrompt state={waiting.state} form={gateForm} picked={gatePick} other={input} />
      ) : null}
      {ask ? (
        <Text color={theme.warn} bold>
          {`? ${ask.question} (y/N)`}
        </Text>
      ) : null}
      {screen === 'chat' ? (
        isRawModeSupported ? (
          <Box flexDirection="column">
            <Box
              flexDirection="row"
              borderStyle={fullscreen ? 'round' : undefined}
              borderColor={theme.border}
              paddingX={fullscreen ? 1 : 0}
            >
              <Text color={theme.accent} bold>
                {'› '}
              </Text>
              <Box flexGrow={1}>
                <MultilineInput
                  value={input}
                  cursor={cursor}
                  onChange={(s: EditState) => {
                    setInput(s.value);
                    setCursor(s.cursor);
                  }}
                  onSubmit={(v: string) => void submit(v)}
                  ignoreVertical={suggestions.length > 0}
                  isActive={ask === null}
                />
              </Box>
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
      <StatusBar items={bottom} ctx={chrome} />
    </Box>
  );
}
