// SPDX-License-Identifier: AGPL-3.0-only
/**
 * The web UI, in the format of the TUI: the same the client core Mirror the
 * terminal renders, projected to the DOM. Open it beside the TUI — both stay
 * in sync because both watch the same daemon feed; neither holds truth.
 * The chrome renders the client's own layout (chrome.ts); the composer
 * speaks `/` `@` `#` `$` via the shared composer rules.
 *
 * A browser always signs in: a request that carries an Origin is never the
 * daemon's implicit operator, so this page never connects without a session
 * of its own. It gets one in one of three ways —
 * - the launch code `agentd ui` put in the URL fragment (main.tsx takes it out
 *   of the URL before any request), exchanged once, and only with the
 *   endpoint the server that served this page names in bootstrap.json;
 * - a sign-in the person approves by typing a code at the terminal that runs
 *   `agentd ui` (a second tab, a sandboxed browser, an expired session);
 * - the device grant, when the daemon's card declares it.
 * The session lives in `sessionStorage` (this tab, this visit), bound to the
 * endpoint it was issued for. `localStorage` remembers the endpoint and the
 * layout, and nothing else.
 */
import React, { useCallback, useEffect, useMemo, useRef, useState, useSyncExternalStore } from 'react';
import {
  APPROVAL_NAME,
  AgentdClient,
  CredentialStore,
  DAEMON_KEYS,
  DEFAULT_LAYOUT,
  DISPLAY_ITEMS,
  DeviceDenied,
  DeviceExpired,
  Json,
  LaunchExpired,
  LaunchRefused,
  Mirror,
  NoLauncher,
  Observation,
  RpcError,
  SYSTEM_COMMANDS,
  Suggestion,
  TERMINAL_STATES,
  TaskView,
  TranscriptEntry,
  activityLine,
  applySuggestion,
  askAnswer,
  askForm,
  availableCommands,
  cardUrlOf,
  commandHelp,
  currentGate,
  describe,
  deviceLogin,
  duration,
  endpointKey,
  fetchCard,
  introspectionOn,
  isLoopbackHost,
  itemValue,
  join,
  launchAuthorize,
  launchExchange,
  launchPoll,
  loadEndpoint,
  loginOptions,
  memoryStorage,
  originOf,
  parseAuthCommand,
  parseLayout,
  persistEndpoint,
  prepare,
  revokeToken,
  routeSend,
  runAuthCommand,
  suggest,
  systemClock,
  workflowNames,
} from '../client/index.js';
import type {
  AskForm,
  ChromeInput,
  Credential,
  DeviceCode,
  DeviceFlow,
  Failure,
  KeyValueStorage,
  Layout,
  MirrorState,
  Tone,
} from '../client/index.js';

type Screen = 'chat' | 'tasks' | 'subagents' | 'debug';

/** The client_id this page names itself by at the daemon's OAuth endpoints. */
export const UI_CLIENT_ID = 'agentd-ui';
/** Where the layout is remembered (`localStorage`): `{top, bottom}`. */
export const LAYOUT_KEY = 'agentd.layout';
/** What a refused launch code says. The page then signs in through the terminal. */
export const LAUNCH_LINK_REFUSED = 'this sign-in link was already used or has expired';
/** What an ended session says on the way back to the sign-in screen. */
export const SESSION_ENDED = 'the session ended (it expired or was revoked) — sign in again';

function stateClass(state: string): string {
  switch (state) {
    case 'TASK_STATE_WORKING':
      return 'st-working';
    case 'TASK_STATE_COMPLETED':
      return 'st-done';
    case 'TASK_STATE_FAILED':
      return 'st-failed';
    case 'TASK_STATE_REJECTED':
      return 'st-rejected';
    case 'TASK_STATE_INPUT_REQUIRED':
      return 'st-input';
    default:
      return 'st-queued';
  }
}
const stateShort = (s: string) => s.replace('TASK_STATE_', '').toLowerCase().replace('_', ' ');

/**
 * Each chrome tone as a class of this page's stylesheet. What an item SAYS is
 * chrome.ts's `itemValue`, the one reading both display clients share; the
 * page only decides how a tone looks.
 */
const TONE_CLASS: Readonly<Record<Tone, string>> = Object.freeze({
  identity: 'name',
  muted: 'dim',
  live: 'live',
  caution: 'polling',
  alert: 'drain',
  error: 'error',
  value: 'chip-mem',
});

function obj(v: Json | undefined): { [k: string]: Json } | undefined {
  return v !== null && v !== undefined && typeof v === 'object' && !Array.isArray(v) ? v : undefined;
}

/**
 * What the mirror knows, as chrome.ts reads it. The status document is the
 * bootstrap read overlaid with the live `status` events: the live one is
 * slim, so a fact only the bootstrap carries still shows.
 */
function chromeInput(s: MirrorState, endpoint: string, active: number): ChromeInput {
  const live = obj(s.status);
  const boot = obj(s.bootstrap);
  const out: ChromeInput = {
    conn: s.conn,
    endpoint,
    introspection: introspectionOn(s),
    draining: s.draining,
    paused: s.paused,
    active,
    counts: { runs: s.runs.size, subagents: s.subagents.size, conversations: s.conversations.size },
  };
  const card = s.session?.extended ?? s.session?.card;
  if (card !== undefined) out.card = card;
  if (live !== undefined || boot !== undefined) out.status = { ...boot, ...live };
  if (s.error !== undefined) out.error = s.error;
  return out;
}

/** One display item for the top/bottom edges, or nothing when it has nothing to say. */
function EdgeItem({ name, input }: { name: string; input: ChromeInput }): React.JSX.Element | null {
  const v = itemValue(name, input);
  return v === null ? null : <span className={TONE_CLASS[v.tone]}>{v.text}</span>;
}

/**
 * Who said what is carried by TREATMENT, not by an author label: the user's
 * own lines are inverted (the same idiom as the TUI), everything else takes a
 * gutter mark. `white-space: pre-wrap` in the stylesheet keeps multi-line
 * bodies intact.
 */
function Row({ e }: { e: TranscriptEntry }): React.JSX.Element {
  if (e.kind === 'user')
    return (
      <div className="row user">
        <div className="bubble">{e.text}</div>
        {e.principal && e.principal !== 'operator' ? (
          <div className="principal">{e.principal}</div>
        ) : null}
      </div>
    );
  if (e.kind === 'agent')
    return (
      <div className="row agent">
        <span className="mark">●</span>
        <span className="body">
          {e.text}
          {e.inputRequired ? <span className="gate"> ⏎ reply to continue</span> : null}
          {/* What the live counter settled at. */}
          {e.ms !== undefined && !e.inputRequired ? (
            <span className="took">{duration(e.ms)}</span>
          ) : null}
        </span>
      </div>
    );
  if (e.kind === 'error')
    return (
      <div className="row err">
        <span className="mark">✗</span>
        <span className="body">{e.text}</span>
      </div>
    );
  if (e.kind === 'command')
    return (
      <div className="row cmd">
        <span className="mark">▸</span>
        <span className="body">{e.text}</span>
      </div>
    );
  return (
    <div className="row note">
      <span className="mark">·</span>
      <span className="body">{e.text}</span>
    </div>
  );
}

function Chat({ mirror, gate, onSend }: { mirror: Mirror; gate: TaskView | undefined; onSend: (text: string) => void }): React.JSX.Element {
  const s = mirror.getState();
  const [input, setInput] = useState('');
  const [sugIndex, setSugIndex] = useState(0);
  // Local clock for the working row's elapsed (never streamed).
  const [tick, setTick] = useState(() => Date.now());
  const working = mirror.activeTasks().length > 0;
  useEffect(() => {
    if (!working) return;
    const t = setInterval(() => setTick(Date.now()), 1000);
    return () => clearInterval(t);
  }, [working]);
  const scrollRef = useRef<HTMLDivElement>(null);
  useEffect(() => {
    scrollRef.current?.scrollTo({ top: scrollRef.current.scrollHeight });
  });
  useEffect(() => setSugIndex(0), [input]);
  const active = mirror.activeTasks();
  // The open gate — this conversation's, the one a plain reply answers — and
  // the form its schema describes. A gate that declared no schema falls
  // through to the composer, which is what always happened.
  const gateForm = useMemo(() => askForm(gate?.askSchema), [gate?.id, gate?.askSchema]);
  const suggestions: Suggestion[] = suggest(input, s);
  const accept = (i: number) => setInput(applySuggestion(input, suggestions[i] ?? suggestions[0]));
  return (
    <div className="pane">
      <div className="scroll" ref={scrollRef}>
        {s.transcript.map((e) => (
          <Row key={e.key} e={e} />
        ))}
        {gate && gateForm.kind !== 'text' ? (
          <GateForm
            task={gate}
            form={gateForm}
            onAnswer={(value) => {
              // The same path a typed reply takes. The parent owns the client
              // and the conversation id, and it already routes a plain message
              // to the open gate — so an answer picked from the form should not
              // need its own way in.
              onSend(typeof value === 'string' ? value : JSON.stringify(value));
            }}
          />
        ) : null}
        {active.length > 0 ? (
          <div className="working">
            ⠿{' '}
            {active[0].state === 'TASK_STATE_INPUT_REQUIRED'
              ? 'waiting for your answer'
              : activityLine(mirror.activityFor(active[0].id), tick)}
            {active.length > 1 ? ` · ${active.length} tasks` : ''}
            <span className="cursor">▌</span>
          </div>
        ) : null}
      </div>
      <div className="composer-wrap">
        {suggestions.length > 0 ? (
          <div className="suggestions">
            {suggestions.map((sug, i) => (
              <button key={sug.label} className={i === sugIndex ? 'on' : ''} onMouseDown={(ev) => { ev.preventDefault(); accept(i); }}>
                {sug.label} <span className="hint">{sug.hint}</span>
              </button>
            ))}
          </div>
        ) : null}
        <form
          className="composer"
          onSubmit={(ev) => {
            ev.preventDefault();
            if (input.trim()) onSend(input);
            setInput('');
          }}
        >
          <span className="prompt">›</span>
          <textarea
            autoFocus
            rows={Math.min(12, input.split('\n').length)}
            value={input}
            onChange={(ev) => setInput(ev.target.value)}
            onKeyDown={(ev) => {
              // Enter sends; Shift/Ctrl/Alt+Enter is a newline (a browser CAN
              // tell them apart, unlike a terminal).
              if (ev.key === 'Enter' && !ev.shiftKey && !ev.ctrlKey && !ev.altKey && !ev.metaKey) {
                ev.preventDefault();
                if (input.trim()) onSend(input);
                setInput('');
                return;
              }
              if (suggestions.length === 0) return;
              if (ev.key === 'Tab') {
                ev.preventDefault();
                accept(sugIndex);
              } else if (ev.key === 'ArrowUp') {
                ev.preventDefault();
                setSugIndex((i) => (i + suggestions.length - 1) % suggestions.length);
              } else if (ev.key === 'ArrowDown') {
                ev.preventDefault();
                setSugIndex((i) => (i + 1) % suggestions.length);
              }
            }}
            placeholder="message agentd — / commands · @skill · #target · $value    (⇧⏎ newline)"
          />
        </form>
      </div>
    </div>
  );
}

function Tasks({ mirror, client }: { mirror: Mirror; client: AgentdClient }): React.JSX.Element {
  const tasks = mirror.allTasks();
  return (
    <div className="pane">
      <div className="scroll">
        {tasks.length === 0 ? (
          <div className="row note">no tasks yet</div>
        ) : (
          <table className="grid">
            <thead>
              <tr>
                <th>id</th>
                <th>state</th>
                <th>link</th>
                <th>principal</th>
                <th>result</th>
                <th />
              </tr>
            </thead>
            <tbody>
              {tasks.map((t: TaskView) => (
                <tr key={t.id}>
                  <td>{t.id}</td>
                  <td className={stateClass(t.state)}>{stateShort(t.state)}</td>
                  <td>{t.link?.kind ?? ''}</td>
                  <td>{t.principal ?? ''}</td>
                  <td>{(t.artifacts[0] ?? t.message ?? '').slice(0, 80)}</td>
                  <td>
                    {!TERMINAL_STATES.has(t.state) ? (
                      <button className="mini" onClick={() => void client.cancelTask(t.id)}>
                        cancel
                      </button>
                    ) : null}
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        )}
      </div>
    </div>
  );
}

function Subagents({ mirror, client }: { mirror: Mirror; client: AgentdClient }): React.JSX.Element {
  const s = mirror.getState();
  const subs = [...s.subagents.values()] as { [k: string]: Json }[];
  // Master–detail, not a drill-down. A subagent tree is something you watch
  // while it moves: pushing the list off-screen to read one child means losing
  // sight of the others exactly when a second one starts misbehaving.
  const [sel, setSel] = useState<string | null>(null);
  const [detail, setDetail] = useState<Json | null>(null);
  const [msg, setMsg] = useState('');
  const [busy, setBusy] = useState(false);
  const [note, setNote] = useState<string | null>(null);

  const handle = sel ?? (subs[0] ? String(subs[0].handle ?? '') : null);
  useEffect(() => {
    if (!handle) return;
    setDetail(null);
    let live = true;
    void client
      .subagentGet(handle)
      .then((d) => live && setDetail(d))
      .catch(() => {
        /* summary-only (debug off) — the view says so */
      });
    return () => {
      live = false;
    };
  }, [handle, client]);

  if (subs.length === 0) {
    return (
      <div className="pane">
        <div className="empty">
          <h2>No subagents yet</h2>
          <p>
            The agent spawns a subagent when it delegates — each one is a real child
            process the supervisor can stop. They will appear here as they start,
            and you can message or stop one from this screen.
          </p>
        </div>
      </div>
    );
  }

  const summary = (handle ? (s.subagents.get(handle) ?? {}) : {}) as { [k: string]: Json };
  const d = (detail ?? summary) as { [k: string]: Json };
  const status = String(d.status ?? '');
  const warm = status === 'running';

  const act = async (fn: () => Promise<unknown>, ok: string) => {
    setBusy(true);
    setNote(null);
    try {
      await fn();
      setNote(ok);
    } catch (e) {
      setNote(e instanceof Error ? e.message : String(e));
    } finally {
      setBusy(false);
    }
  };

  const field = (label: string, v: Json | undefined, cls = '') =>
    v === undefined || v === null ? null : (
      <div className="field" key={label}>
        <span className="k">{label}</span>
        <span className={cls}>{typeof v === 'string' ? v : JSON.stringify(v, null, 1)}</span>
      </div>
    );

  return (
    <div className="pane split">
      <aside className="masterlist">
        {subs.map((x, i) => {
          const h = String(x.handle ?? i);
          const st = String(x.status ?? '');
          return (
            <button
              key={h}
              className={`subrow ${h === handle ? 'is-sel' : ''} ${subagentClass(st)}`}
              onClick={() => setSel(h)}
            >
              <span className="subrow-h">{h}</span>
              <span className="subrow-meta">
                {String(x.mode ?? '')} · {String(x.tokens ?? 0)} tok
              </span>
              <span className="subrow-st">{st}</span>
            </button>
          );
        })}
      </aside>

      <div className="scroll detail">
        <header className="detail-head">
          <h2>{handle}</h2>
          <span className={`pill ${subagentClass(status)}`}>{status || 'unknown'}</span>
        </header>

        {field('mode', d.mode)}
        {field('attempt', d.attempt)}
        {field('tokens', d.tokens)}
        {field('instruction', d.instruction)}
        {field('result', d.result)}
        {field('error', d.error, 'err')}
        {field('requested_by', d.requested_by)}
        {!introspectionOn(s) ? (
          <div className="row note">
            summary only — enable {DAEMON_KEYS.introspection} for instruction and result
          </div>
        ) : null}

        {/* Control. A subagent is a process, so "stop" means stop. */}
        <div className="controls">
          <form
            className="chat"
            onSubmit={(e) => {
              e.preventDefault();
              const text = msg.trim();
              if (!text || !handle) return;
              setMsg('');
              void act(() => client.subagentSend(handle, text), 'message delivered');
            }}
          >
            <input
              value={msg}
              onChange={(e) => setMsg(e.target.value)}
              placeholder={warm ? `message ${handle}…` : 'only a running subagent can be messaged'}
              disabled={!warm || busy}
              aria-label="message this subagent"
            />
            <button className="mini" type="submit" disabled={!warm || busy || !msg.trim()}>
              send
            </button>
          </form>
          <button
            className="mini danger"
            disabled={!warm || busy || !handle}
            onClick={() => handle && void act(() => client.subagentKill(handle), 'stopped')}
          >
            stop
          </button>
        </div>
        {note ? <div className="row note">{note}</div> : null}
      </div>
    </div>
  );
}

/** A step's state as a class, so severity reads as form and not only as a word. */
function stepClass(st: { phase: string; status?: string }): string {
  if (st.phase === 'start') return 'is-running';
  switch (st.status) {
    case 'done':
      return 'is-done';
    case 'pruned':
    case 'skipped':
      return 'is-skipped';
    case 'suspended':
      return 'is-waiting';
    default:
      return 'is-failed';
  }
}

/** A SUBAGENT status as a class, so state reads as colour and not only as text.
 *  (Distinct from `stateClass` above, which maps A2A task states.) */
function subagentClass(status: string): string {
  switch (status) {
    case 'running':
      return 'is-running';
    case 'completed':
      return 'is-done';
    case 'failed':
    case 'crashed':
    case 'killed':
      return 'is-failed';
    default:
      return 'is-idle';
  }
}

function Debug({ mirror, client }: { mirror: Mirror; client: AgentdClient }): React.JSX.Element {
  const s = mirror.getState();
  const [log, setLog] = useState<{ [k: string]: Json }[]>([]);
  const cursor = useRef(0);
  const on = introspectionOn(s);
  useEffect(() => {
    if (!on) return;
    let alive = true;
    const tick = async () => {
      try {
        const r = (await client.debugEvents(cursor.current, 100)) as { [k: string]: Json };
        if (!alive) return;
        const events = (Array.isArray(r.events) ? r.events : []) as { [k: string]: Json }[];
        if (events.length > 0) {
          cursor.current = (r.newest_seq as number) ?? cursor.current;
          setLog((prev) => [...prev, ...events].slice(-300));
        }
      } catch {
        /* pane stops filling */
      }
    };
    void tick();
    const t = setInterval(tick, 1000);
    return () => {
      alive = false;
      clearInterval(t);
    };
  }, [on, client]);
  if (!on)
    return (
      <div className="pane">
        <div className="scroll">
          <div className="row note">debug is off on this daemon — set {DAEMON_KEYS.introspection} in its config</div>
        </div>
      </div>
    );
  const line = (v: Json) => {
    const str = JSON.stringify(v) ?? '';
    return str.length > 160 ? `${str.slice(0, 160)}…` : str;
  };
  return (
    <div className="pane">
      <div className="debug">
        <section className="wide">
          <h3 data-count={String(s.feedLog.length)}>feed</h3>
          {s.feedLog.slice(-14).map((e) => (
            <div key={e.seq} className="line">
              {e.seq} <span className="k">{e.kind}</span> {line(e.data)}
            </div>
          ))}
        </section>
        <section>
          <h3 data-count={String(s.runs.size)}>runs</h3>
          {[...s.runs.values()].slice(-10).map((r, i) => {
            const o = r as { [k: string]: Json };
            const id = (o.id as string) ?? String(i);
            // What the run is DOING, not how many steps it has. Newest first, so
            // the step that matters — the running one, or the one that failed —
            // is at the top where the eye lands.
            const steps = (s.steps.get(id) ?? []).slice(-6).reverse();
            return (
              <div key={id} className="run">
                <div className="line">
                  <span className="k">{id}</span> {o.status as string} {line(o.steps ?? null)}
                </div>
                {steps.length > 0 ? (
                  <ol className="steps">
                    {steps.map((st, j) => (
                      <li key={`${st.step}${j}`} className={`step ${stepClass(st)}`}>
                        <span className="step-name">{st.step}</span>
                        <span className="step-kind">{st.kind ?? ''}</span>
                        <span className="step-state">
                          {st.phase === 'start' ? 'running' : (st.status ?? '')}
                          {/* The slow step is usually what you came to find. */}
                          {st.ms !== undefined ? (
                            <span className="step-ms">{duration(st.ms)}</span>
                          ) : null}
                        </span>
                        {st.attempt && st.attempt > 1 ? (
                          <span className="step-attempt">attempt {st.attempt}</span>
                        ) : null}
                        {st.err ? <span className="step-err">{st.err}</span> : null}
                      </li>
                    ))}
                  </ol>
                ) : null}
              </div>
            );
          })}
        </section>
        <section>
          <h3 data-count={String(s.subagents.size + s.children.size)}>subagents / children</h3>
          {[...s.subagents.values()].slice(-6).map((x, i) => {
            const o = x as { [k: string]: Json };
            return (
              <div key={`s${i}`} className="line">
                sub <span className="k">{o.handle as string}</span> {o.status as string} · {String(o.tokens ?? 0)} tok
              </div>
            );
          })}
          {[...s.children.values()].slice(-6).map((x, i) => {
            const o = x as { [k: string]: Json };
            return (
              <div key={`c${i}`} className="line">
                pid <span className="k">{String(o.pid ?? '')}</span> {String(o.kind ?? '')}
              </div>
            );
          })}
        </section>
        <section className="wide">
          <h3 data-count={String(log.length)}>log</h3>
          {log.slice(-14).map((l, i) => (
            <div key={(l.seq as number) ?? i} className="line">
              {String(l.level ?? '')} <span className="k">{String(l.event ?? '')}</span> {line(l)}
            </div>
          ))}
        </section>
      </div>
    </div>
  );
}

// ---- getting a session --------------------------------------------------------

/** What the server that served this page says about where the daemon is. */
export interface Bootstrap {
  endpoint?: string;
}

/**
 * Take the launch code out of the URL — the one thing main.tsx does before
 * any request. The code (`#launch=<code>`) is returned for the page to hold in
 * memory, and the fragment is replaced away, so no later request, history
 * entry or error that quotes the location carries it. `?bearer=` (a v1.16
 * link) is stripped in the same step and never read: a credential in a query
 * string reaches server logs and history, so this page takes none from one.
 */
export function takeLaunchCode(
  loc: Pick<Location, 'hash' | 'search' | 'pathname'>,
  hist: Pick<History, 'replaceState'>,
): string | undefined {
  const code = new URLSearchParams(loc.hash.replace(/^#/, '')).get('launch') ?? '';
  const query = new URLSearchParams(loc.search);
  const bearer = query.has('bearer');
  query.delete('bearer');
  if (loc.hash !== '' || bearer) {
    const rest = query.toString();
    hist.replaceState(null, '', `${loc.pathname}${rest ? `?${rest}` : ''}`);
  }
  return code !== '' ? code : undefined;
}

/**
 * Read ./bootstrap.json: the endpoint `agentd-ui` was started with. A hosted
 * copy of the page has no such document, and then there is simply no default.
 */
export async function readBootstrap(href: string): Promise<Bootstrap> {
  try {
    const res = await fetch(new URL('bootstrap.json', href).href, { cache: 'no-store', redirect: 'error' });
    if (!res.ok) return {};
    const v = (await res.json()) as { endpoint?: unknown } | null;
    return typeof v?.endpoint === 'string' && v.endpoint !== '' ? { endpoint: v.endpoint } : {};
  } catch {
    return {};
  }
}

/** Do two endpoint URLs name the same endpoint (the key a credential is bound to)? */
function sameEndpoint(a: string | undefined, b: string | undefined): boolean {
  if (a === undefined || b === undefined) return false;
  try {
    return endpointKey(a) === endpointKey(b);
  } catch {
    return false;
  }
}

/**
 * Where the page starts, and whether it may start on its own:
 * - `?endpoint=` pre-fills the form and waits for the person to connect —
 *   a link names the endpoint, not the person's decision to trust it —
 *   unless it names the bootstrap endpoint itself;
 * - else the bootstrap endpoint, the only one the launch code is for;
 * - else the endpoint remembered from the last visit.
 * `launch` says whether the launch code may be spent here: only on the
 * bootstrap endpoint. Anywhere else it is dropped unsent.
 */
export function startPlan(
  bootstrap: Bootstrap,
  search: string,
  remembered: string | undefined,
): { endpoint?: string; auto: boolean; launch: boolean } {
  const q = new URLSearchParams(search).get('endpoint') ?? '';
  if (q !== '') {
    const boot = sameEndpoint(q, bootstrap.endpoint);
    return { endpoint: q, auto: boot, launch: boot };
  }
  if (bootstrap.endpoint !== undefined) return { endpoint: bootstrap.endpoint, auto: true, launch: true };
  if (remembered !== undefined) return { endpoint: remembered, auto: true, launch: false };
  return { auto: false, launch: false };
}

/**
 * Web Storage, or memory when the browser refuses this origin site data (the
 * accessor itself throws then). A credential in memory lasts until the tab
 * reloads, which is still somewhere it is allowed to be.
 */
function storageOf(get: () => KeyValueStorage): KeyValueStorage {
  try {
    const s = get();
    s.getItem(LAYOUT_KEY);
    return s;
  } catch {
    return memoryStorage();
  }
}

/** The remembered layout, or the web default for an edge that is missing or unreadable. */
export function loadLayout(storage: KeyValueStorage): Layout {
  const out: Layout = { top: [...DEFAULT_LAYOUT.web.top], bottom: [...DEFAULT_LAYOUT.web.bottom] };
  let v: unknown;
  try {
    const raw = storage.getItem(LAYOUT_KEY);
    if (raw === null) return out;
    v = JSON.parse(raw);
  } catch {
    return out;
  }
  const o = v !== null && typeof v === 'object' && !Array.isArray(v) ? (v as { top?: unknown; bottom?: unknown }) : {};
  // Items an older or newer client knew and this one does not are left out,
  // not refused: the rest of the edge still draws.
  if (Array.isArray(o.top)) out.top = parseLayout(o.top.filter((x): x is string => typeof x === 'string'), 'web').items;
  if (Array.isArray(o.bottom)) out.bottom = parseLayout(o.bottom.filter((x): x is string => typeof x === 'string'), 'web').items;
  return out;
}

/** A failed call, as one line: the agent's words and code, or the error's message. */
function failureText(e: unknown): string {
  if (e instanceof RpcError) return `${e.message} (${e.code})`;
  return e instanceof Error ? e.message : String(e);
}

/** Where the Connect screen is in signing in. */
type SignIn =
  | { kind: 'idle' }
  | { kind: 'busy'; what: string }
  /** A terminal-approved request is waiting for the person at `agentd ui`'s terminal. */
  | { kind: 'terminal'; userCode: string }
  /** A device code is waiting for an approver. */
  | { kind: 'device'; code: DeviceCode }
  /** The card offers the device grant; the person starts it. */
  | { kind: 'offer'; flow: DeviceFlow }
  /** Nothing this page can sign in with. */
  | { kind: 'none' };

export interface AppProps {
  /** What ./bootstrap.json said (main.tsx read it). */
  bootstrap: Bootstrap;
  /** The launch code main.tsx took out of the fragment, held in memory only. */
  launchCode?: string;
}

export function App({ bootstrap, launchCode }: AppProps): React.JSX.Element {
  // The session store is this tab's; the persistent store gets the endpoint
  // and the layout only (credstore.ts has no path that writes a credential
  // there, and loadEndpoint rewrites a v1.16 `{endpoint, bearer}` entry).
  const tab = useMemo(() => storageOf(() => sessionStorage), []);
  const local = useMemo(() => storageOf(() => localStorage), []);
  const creds = useMemo(() => new CredentialStore(tab), [tab]);
  const remembered = useMemo(() => loadEndpoint(local), [local]);
  const plan = useMemo(() => startPlan(bootstrap, location.search, remembered), [bootstrap, remembered]);
  const noExtensions = useMemo(() => new URLSearchParams(location.search).get('extensions') === 'off', []);

  const [form, setForm] = useState(plan.endpoint ?? 'http://127.0.0.1:8420');
  const [signIn, setSignIn] = useState<SignIn>({ kind: 'idle' });
  const [notice, setNotice] = useState<string | undefined>(undefined);
  const [error, setError] = useState<string | undefined>(undefined);
  const [conn, setConn] = useState<{ endpoint: string; credential: Credential } | null>(null);
  // One sign-in at a time: starting another (a new endpoint, a device
  // sign-in) abandons the one in flight, and so does leaving the page.
  const flight = useRef<AbortController | null>(null);
  // The launch code is spent at most once, whatever happens next.
  const launch = useRef<string | undefined>(launchCode);

  const connect = useCallback(
    (endpoint: string, credential: Credential) => {
      persistEndpoint(local, { endpoint });
      setForm(endpoint);
      setSignIn({ kind: 'idle' });
      setError(undefined);
      setConn({ endpoint, credential });
    },
    [local],
  );

  /**
   * Get a session for `endpoint` and connect with it — or stop on the
   * Connect screen saying what would get one. The order is the contract:
   * the launch code (bootstrap endpoint only), a session this tab already
   * holds for exactly this endpoint, the launcher's terminal (bootstrap
   * endpoint on loopback), then whatever the card offers. Nothing here ever
   * connects without a credential.
   */
  const begin = useCallback(
    (endpoint: string, opts: { code?: string; notice?: string } = {}) => {
      flight.current?.abort();
      const ac = new AbortController();
      flight.current = ac;
      const { signal } = ac;
      setConn(null);
      setNotice(opts.notice);
      setError(undefined);
      const run = async (): Promise<void> => {
        let key: string;
        try {
          key = endpointKey(endpoint);
        } catch {
          setSignIn({ kind: 'idle' });
          setError(`${JSON.stringify(endpoint)} is not a URL`);
          return;
        }
        const boot = sameEndpoint(endpoint, bootstrap.endpoint);
        const origin = originOf(endpoint);
        // Only the start plan hands a code in, and only for the bootstrap
        // endpoint (startPlan): the one place that decision is made.
        if (opts.code !== undefined) {
          setSignIn({ kind: 'busy', what: 'signing in' });
          try {
            const c = await launchExchange(join(origin, '/oauth2/token'), opts.code, UI_CLIENT_ID, { signal });
            creds.set(key, c);
            return connect(endpoint, c);
          } catch (e) {
            if (signal.aborted) return;
            setNotice(e instanceof LaunchRefused ? LAUNCH_LINK_REFUSED : `the sign-in link did not work: ${failureText(e)}`);
          }
        }
        const held = creds.get(key);
        if (held) return connect(endpoint, held);
        // The terminal-approved request: only the launcher's own daemon can
        // approve it, and it is redeemable over loopback only.
        if (boot && isLoopbackHost(new URL(endpoint).hostname)) {
          const tokenUrl = join(origin, '/oauth2/token');
          for (;;) {
            let req;
            try {
              setSignIn({ kind: 'busy', what: 'asking the launcher' });
              req = await launchAuthorize(join(origin, '/oauth2/launch_authorization'), UI_CLIENT_ID, { signal });
            } catch (e) {
              if (signal.aborted) return;
              // No launcher slot: this daemon was not started by `agentd ui`.
              if (!(e instanceof NoLauncher)) setError(failureText(e));
              break;
            }
            setSignIn({ kind: 'terminal', userCode: req.userCode });
            try {
              const c = await launchPoll(tokenUrl, req.requestCode, UI_CLIENT_ID, systemClock, { interval: req.interval, signal });
              creds.set(key, c);
              return connect(endpoint, c);
            } catch (e) {
              if (signal.aborted) return;
              // Nobody typed the code in time, or a newer tab's request
              // pushed this one out: ask for a fresh code and keep waiting.
              if (e instanceof LaunchExpired) continue;
              setError(failureText(e));
              break;
            }
          }
        }
        setSignIn({ kind: 'busy', what: 'reading the agent card' });
        let card;
        try {
          card = await fetchCard(cardUrlOf(endpoint), { signal });
        } catch (e) {
          if (signal.aborted) return;
          setSignIn({ kind: 'idle' });
          setError(failureText(e));
          return;
        }
        if (signal.aborted) return;
        const device = loginOptions(card).find((o) => o.method === 'device');
        setSignIn(device?.method === 'device' ? { kind: 'offer', flow: device.flow } : { kind: 'none' });
      };
      void run().catch((e: unknown) => {
        if (signal.aborted) return;
        setSignIn({ kind: 'idle' });
        setError(failureText(e));
      });
    },
    [bootstrap.endpoint, creds, connect],
  );

  /** The device grant, started by the person from the Connect screen. */
  const deviceSignIn = useCallback(
    (endpoint: string, flow: DeviceFlow, scope: string | undefined) => {
      flight.current?.abort();
      const ac = new AbortController();
      flight.current = ac;
      setNotice(undefined);
      setError(undefined);
      setSignIn({ kind: 'busy', what: 'asking for a sign-in code' });
      void deviceLogin({ flow, clientId: UI_CLIENT_ID, scope, signal: ac.signal, onCode: (code) => setSignIn({ kind: 'device', code }) })
        .then((c) => {
          creds.set(endpointKey(endpoint), c);
          connect(endpoint, c);
        })
        .catch((e: unknown) => {
          if (ac.signal.aborted) return;
          setSignIn({ kind: 'offer', flow });
          setError(e instanceof DeviceDenied || e instanceof DeviceExpired ? e.message : failureText(e));
        });
    },
    [creds, connect],
  );

  /**
   * Sign out: revoke the session at the daemon (RFC 7009), then forget it.
   * The authorization server is on the listener origin, the same origin the
   * session was issued on, so that is where the revocation goes.
   */
  const signOut = useCallback(
    async (endpoint: string, credential: Credential) => {
      flight.current?.abort();
      setConn(null);
      setSignIn({ kind: 'busy', what: 'signing out' });
      let said: string;
      try {
        await revokeToken(join(originOf(endpoint), '/oauth2/revoke'), credential.token, {
          clientId: UI_CLIENT_ID,
          signal: AbortSignal.timeout(10_000),
        });
        said = 'signed out; the session is revoked';
      } catch (e) {
        said = `signed out here, but revoking the session failed: ${failureText(e)}`;
      }
      creds.delete(endpointKey(endpoint));
      setSignIn({ kind: 'idle' });
      setNotice(said);
    },
    [creds],
  );

  /** The daemon refused the session (expired, revoked): drop it and sign in again. */
  const ended = useCallback(
    (endpoint: string, why: string) => {
      creds.delete(endpointKey(endpoint));
      begin(endpoint, { notice: why });
    },
    [creds, begin],
  );

  useEffect(() => {
    // Spent or dropped now, whichever way the plan goes: a code that did not
    // go to the bootstrap endpoint at startup goes nowhere, ever.
    const code = launch.current;
    launch.current = undefined;
    if (plan.auto && plan.endpoint !== undefined) begin(plan.endpoint, plan.launch && code !== undefined ? { code } : {});
    return () => flight.current?.abort();
  }, []);

  if (conn) {
    return (
      <Connected
        endpoint={conn.endpoint}
        credential={conn.credential}
        noExtensions={noExtensions}
        layoutStore={local}
        onEnded={(why) => ended(conn.endpoint, why)}
        onSignOut={() => void signOut(conn.endpoint, conn.credential)}
      />
    );
  }
  return (
    <Connect
      endpoint={form}
      setEndpoint={setForm}
      signIn={signIn}
      notice={notice}
      error={error}
      onConnect={() => begin(form.trim())}
      onDevice={(flow, scope) => deviceSignIn(form.trim(), flow, scope)}
    />
  );
}

function Connect({
  endpoint,
  setEndpoint,
  signIn,
  notice,
  error,
  onConnect,
  onDevice,
}: {
  endpoint: string;
  setEndpoint: (v: string) => void;
  signIn: SignIn;
  notice?: string;
  error?: string;
  onConnect: () => void;
  onDevice: (flow: DeviceFlow, scope: string | undefined) => void;
}): React.JSX.Element {
  // The origin the daemon must admit, as this page really has it — the
  // instructions carry it rather than a placeholder to adapt.
  const origin = typeof location !== 'undefined' ? location.origin : '';
  const hosted =
    typeof location !== 'undefined' && location.protocol === 'https:' && !isLoopbackHost(location.hostname);
  const [scope, setScope] = useState<string | undefined>(undefined);
  return (
    <div className="app">
      <div className="connect">
        <h1>agentd</h1>
        <p>
          Connect to your own running agentd. This page holds nothing: the endpoint stays in your
          browser, a session lives in this tab only, and every request goes straight from here to
          your daemon.
        </p>
        {notice ? <p className="notice">{notice}</p> : null}
        {signIn.kind === 'busy' ? <p>{signIn.what}…</p> : null}
        {signIn.kind === 'terminal' ? (
          <p className="signin">
            Type this code in the terminal that runs <code>agentd ui</code>: <strong>{signIn.userCode}</strong>
          </p>
        ) : null}
        {signIn.kind === 'device' ? (
          <p className="signin">
            To sign in, open{' '}
            <a href={signIn.code.verificationUriComplete ?? signIn.code.verificationUri} target="_blank" rel="noreferrer noopener">
              {signIn.code.verificationUri}
            </a>{' '}
            and enter <strong>{signIn.code.userCode}</strong> — or an operator runs{' '}
            <code>/approve {signIn.code.userCode} &lt;name&gt;</code>
          </p>
        ) : null}
        {signIn.kind === 'offer' ? (
          <div className="signin">
            {signIn.flow.scopes.length > 1 ? (
              <>
                <label>ask for</label>
                <select value={scope ?? ''} onChange={(e) => setScope(e.target.value || undefined)} aria-label="scope">
                  <option value="">the default scope</option>
                  {signIn.flow.scopes.map((sc) => (
                    <option key={sc} value={sc}>
                      {sc}
                    </option>
                  ))}
                </select>
              </>
            ) : null}
            <button type="button" onClick={() => onDevice(signIn.flow, scope)}>
              sign in
            </button>
          </div>
        ) : null}
        {signIn.kind === 'none' ? (
          <p className="notice">
            This daemon issues browser sessions only through <code>a2a.device_grant</code> or the{' '}
            <code>agentd ui</code> launcher: turn the device grant on (an operator then approves the
            code this page shows), or run <code>agentd ui</code> for a tab that is already signed in.
          </p>
        ) : null}
        {error ? <div className="err">{error}</div> : null}
        <details className="setup" open={hosted || signIn.kind === 'none'}>
          <summary>What your daemon needs</summary>
          <p>
            The daemon answers a web page only from an origin it lists — this page's included, even
            on loopback. The one exception is the tab <code>agentd ui</code> opens. Add this origin
            to its config (it takes effect on reload, no restart):
          </p>
          <pre>{`a2a:
  cors:
    origins: ["${origin}"]`}</pre>
          {hosted ? (
            <p className="warn-note">
              <strong>Safari cannot do this.</strong> It is the one browser that blocks an HTTPS
              page from reaching <code>http://localhost</code>, and no setting on this page can
              change that. Use Chrome, Edge or Firefox — or run <code>agentd ui</code> locally,
              which serves the same interface next to the daemon.
            </p>
          ) : null}
        </details>
        <form
          onSubmit={(e) => {
            e.preventDefault();
            onConnect();
          }}
        >
          <label>endpoint (a2a.listen)</label>
          <input value={endpoint} onChange={(e) => setEndpoint(e.target.value)} placeholder="http://127.0.0.1:8420" />
          <button type="submit">connect</button>
        </form>
      </div>
    </div>
  );
}

/**
 * The approval dialog: approving a device sign-in hands it a principal, and
 * every session approved under one name IS that principal, so the name is
 * asked for — never defaulted — and the grant is confirmed here.
 */
function ApproveDialog({
  userCode,
  name: initialName,
  operator: initialOperator,
  onApprove,
  onCancel,
}: {
  userCode: string;
  name: string;
  operator: boolean;
  onApprove: (name: string, operator: boolean) => void;
  onCancel: () => void;
}): React.JSX.Element {
  const [name, setName] = useState(initialName);
  const [operator, setOperator] = useState(initialOperator);
  const valid = APPROVAL_NAME.test(name);
  return (
    <form
      className="gate approve"
      onSubmit={(e) => {
        e.preventDefault();
        if (valid) onApprove(name, operator);
      }}
    >
      <div className="gate-q">
        Approve device sign-in <strong>{userCode}</strong> as:
      </div>
      <input
        className="gate-other"
        value={name}
        onChange={(e) => setName(e.target.value)}
        placeholder="the name it signs in as (a-z, 0-9, . _ -)"
        aria-label="the name the device signs in as"
        autoFocus
      />
      <label className="gate-hint">
        <input type="checkbox" checked={operator} onChange={(e) => setOperator(e.target.checked)} /> with OPERATOR scope
      </label>
      <div className="gate-actions">
        <button className="mini" type="submit" disabled={!valid}>
          approve
        </button>
        <button className="mini" type="button" onClick={onCancel}>
          cancel
        </button>
        <span className="gate-hint">every device approved under one name shares its tasks and conversations</span>
      </div>
    </form>
  );
}

function Connected({
  endpoint,
  credential,
  noExtensions,
  layoutStore,
  onEnded,
  onSignOut,
}: {
  endpoint: string;
  credential: Credential;
  noExtensions: boolean;
  layoutStore: KeyValueStorage;
  /** The daemon no longer takes the session. */
  onEnded: (why: string) => void;
  /** The person signed out (`/disconnect`, `/logout`). */
  onSignOut: () => void;
}): React.JSX.Element {
  // The client exists once discovery settled where JSON-RPC goes and what
  // the card lets this client call; until then a command has nowhere to go.
  const [client, setClient] = useState<AgentdClient | null>(null);
  const mirror = useMemo(() => new Mirror(), []);
  useSyncExternalStore(mirror.subscribe, mirror.getVersion);
  /** Why observation stopped for good, and what would fix it. */
  const [banner, setBanner] = useState<string[] | null>(null);
  const obsRef = useRef<Observation | null>(null);
  const endedRef = useRef(onEnded);
  endedRef.current = onEnded;
  useEffect(() => {
    setClient(null);
    const obs = new Observation(
      {
        configured: endpoint,
        credential,
        noExtensions,
        onSession: (_, c) => setClient(c),
        onTerminal: (f: Failure) => {
          // A refused session is a sign-in, not a retry: back to the Connect
          // screen, which starts the way in again. Anything else stays on
          // screen, saying what would fix it.
          if (f.kind === 'unauthenticated') endedRef.current(`${describe(f)} — ${SESSION_ENDED}`);
          else if (f.kind === 'forbidden') setBanner([describe(f), 'this principal is not allowed to observe this agent — an operator can grant it']);
          else setBanner([describe(f), 'this page cannot work with this agent — /disconnect and check the endpoint']);
        },
      },
      mirror,
    );
    obsRef.current = obs;
    obs.start();
    return () => {
      obsRef.current = null;
      obs.stop();
    };
  }, [endpoint, credential, noExtensions, mirror]);
  const need = useCallback((): AgentdClient => {
    if (!client) throw new Error('not connected yet');
    return client;
  }, [client]);
  const [screen, setScreen] = useState<Screen>('chat');
  const [layout, setLayout] = useState<Layout>(() => loadLayout(layoutStore));
  const [approving, setApproving] = useState<{ userCode: string; name: string; operator: boolean } | null>(null);
  const s = mirror.getState();
  /** The conversation this person is in; a fresh page is in none yet. */
  const [ctx, setCtx] = useState<string | undefined>(undefined);
  const debugOn = introspectionOn(s);

  const keepLayout = useCallback(
    (next: Layout | null) => {
      if (next === null) {
        layoutStore.removeItem(LAYOUT_KEY);
        setLayout({ top: [...DEFAULT_LAYOUT.web.top], bottom: [...DEFAULT_LAYOUT.web.bottom] });
        return;
      }
      layoutStore.setItem(LAYOUT_KEY, JSON.stringify({ top: next.top, bottom: next.bottom }));
      setLayout(next);
    },
    [layoutStore],
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
      if (cmd === 'approve') {
        // The dialog asks for the name when it was not typed, and is the
        // confirmation either way.
        const [userCode, name, scope, ...extra] = rest;
        if (!userCode || extra.length > 0 || (scope !== undefined && scope !== 'operator')) {
          mirror.note('usage: /approve <code> [name] [operator]', 'error');
          return;
        }
        setApproving({ userCode, name: name ?? '', operator: scope === 'operator' });
        return;
      }
      // The other sign-in administration commands parse and run in the
      // composer module, so both display clients answer them alike.
      if (parseAuthCommand(cmd, rest) !== null) {
        const out = await runAuthCommand(cmd, rest, need());
        if (out !== null) mirror.note(out.text, out.error ? 'error' : 'info');
        return;
      }
      switch (cmd) {
        case 'help':
          mirror.note(`${commandHelp(s)} · /disconnect`);
          return;
        case 'new':
          setCtx(undefined);
          mirror.note('new conversation');
          return;
        case 'chat':
        case 'tasks':
        case 'subagents':
          setScreen(cmd);
          return;
        case 'debug':
          if (!debugOn) mirror.note(`debug is off on this daemon — set ${DAEMON_KEYS.introspection} in its config`, 'error');
          else setScreen('debug');
          return;
        case 'layout': {
          const [edge, items] = rest;
          if (edge === undefined) {
            const known = Object.keys(DISPLAY_ITEMS).filter((k) => DISPLAY_ITEMS[k].surfaces.includes('web'));
            mirror.note(
              `top: ${layout.top.join(',')}\nbottom: ${layout.bottom.join(',')}\n` +
                `items: ${known.join(', ')}, memory:<key>\n/layout top|bottom <items,…> · /layout reset`,
            );
            return;
          }
          if (edge === 'reset' && items === undefined) {
            keepLayout(null);
            mirror.note('layout reset');
            return;
          }
          if ((edge !== 'top' && edge !== 'bottom') || items === undefined || rest.length > 2) {
            mirror.note('usage: /layout [top|bottom <items,…> | reset]', 'error');
            return;
          }
          const parsed = parseLayout(items, 'web');
          if (parsed.unknown.length > 0) {
            mirror.note(`unknown item(s): ${parsed.unknown.join(', ')} — /layout lists them`, 'error');
            return;
          }
          keepLayout({ ...layout, [edge]: parsed.items });
          mirror.note(`${edge}: ${parsed.items.join(',')}`);
          return;
        }
        case 'login':
          mirror.note('signed in already — /disconnect signs out, and the connect screen signs in again');
          return;
        case 'logout':
        case 'disconnect':
          onSignOut();
          return;
        case 'quit':
        case 'exit':
          mirror.note('close the tab to leave — /disconnect also signs this session out');
          return;
        case 'status': {
          const st = (await need().status()) as { [k: string]: Json };
          mirror.bootstrap(st);
          mirror.note(
            `runs ${Array.isArray(st.runs) ? st.runs.length : 0} · conversations ${Array.isArray(st.conversations) ? st.conversations.length : 0} · subagents ${Array.isArray(st.subagents) ? st.subagents.length : 0} · draining ${st.draining}`,
          );
          return;
        }
        case 'config': {
          const cfg = await need().config();
          if (arg) {
            let v: Json = (cfg as { config?: Json }).config ?? cfg;
            for (const part of arg.split('.')) v = (v as { [k: string]: Json } | null)?.[part] ?? null;
            mirror.note(`${arg} = ${JSON.stringify(v)}`);
          } else {
            mirror.note(`${JSON.stringify((cfg as { config?: Json }).config ?? cfg, null, 1).slice(0, 2000)}\n/config <path> for one value · /set for runtime knobs`);
          }
          return;
        }
        case 'set': {
          const [path, ...valueParts] = rest;
          if (!path || valueParts.length === 0) {
            mirror.note('usage: /set <path> <value> — one of the paths the agent lists as settable', 'error');
            return;
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
          // What the card offers may have moved with it (introspection on, a
          // new settable path): re-read it.
          obsRef.current?.refresh();
          return;
        }
        case 'signal': {
          const [name, run] = rest;
          if (!name) {
            mirror.note('usage: /signal <name> [run]', 'error');
            return;
          }
          const r = (await need().signal(name, undefined, run)) as { delivered?: number } | null;
          mirror.note(`signal ${name} → delivered ${r?.delivered ?? '?'}`);
          return;
        }
        case 'send': {
          const [handle, ...msg] = rest;
          if (!handle || msg.length === 0) {
            mirror.note('usage: /send <handle> <message>', 'error');
            return;
          }
          await need().subagentSend(handle, msg.join(' '));
          mirror.note(`sent to ${handle}`);
          return;
        }
        case 'pause':
          await need().pause(arg || undefined);
          mirror.note(arg ? `paused ${arg}` : 'instance paused — /resume to release');
          return;
        case 'resume':
          await need().resume(arg || undefined);
          mirror.note(arg ? `resumed ${arg}` : 'instance resumed');
          return;
        case 'conversations': {
          const convs = [...s.conversations.values()] as { [k: string]: Json }[];
          mirror.note(
            convs.length === 0
              ? 'no conversations yet'
              : `conversations:\n${convs.map((c) => `#${c.id}  ${c.messages ?? 0} msgs · ${c.turns ?? 0} turns`).join('\n')}\nstart a message with #<id> to address one`,
          );
          return;
        }
        case 'plan': {
          const p = (await need().planGet(arg || undefined)) as { plan?: Json } | null;
          mirror.note(`plan: ${JSON.stringify(p?.plan ?? null).slice(0, 800)}`);
          return;
        }
        case 'workflow': {
          if (!arg) {
            mirror.note('usage: /workflow <name>', 'error');
            return;
          }
          const r = await need().workflowRun(arg);
          mirror.note(`workflow ${arg} → ${r.task?.id ?? '?'}`);
          return;
        }
        case 'cancel': {
          const id = arg || mirror.activeTasks()[0]?.id;
          if (!id) {
            mirror.note('nothing to cancel');
            return;
          }
          const t = await need().cancelTask(id);
          if (t) mirror.adoptTasks([t]);
          mirror.note(`cancelled ${id}`);
          return;
        }
        case 'drain':
          await need().drain();
          mirror.note('draining requested');
          return;
        default:
          // Not a system command: a workflow shortcut (`/deploy` ⇒ run it),
          // named by the extended card or, without one, by `status`.
          if (workflowNames(s).includes(cmd)) {
            const r = await need().workflowRun(cmd);
            mirror.note(`workflow ${cmd} → ${r.task?.id ?? '?'}`);
          } else {
            mirror.note(`unknown command /${cmd} — /help`, 'error');
          }
      }
    },
    [need, mirror, s, debugOn, layout, keepLayout, onSignOut],
  );

  const onSend = useCallback(
    (raw: string) => {
      const text = raw.trim();
      if (text.length === 0) return;
      void (async () => {
        try {
          if (text.startsWith('/')) {
            await runSlash(text);
            return;
          }
          // `#target` routing + `$value` interpolation, then where it goes: a
          // named task, else this conversation — and its open gate, if any.
          const p = prepare(text, s);
          const route = routeSend(p, s, ctx);
          const sent = await need().send(p.text, route);
          landed(sent, p.text, route.taskId);
        } catch (e) {
          mirror.note(failureText(e), 'error');
        }
      })();
    },
    [runSlash, need, mirror, s, ctx, landed],
  );

  const approve = useCallback(
    (userCode: string, name: string, operator: boolean) => {
      setApproving(null);
      void runAuthCommand('approve', [userCode, name, ...(operator ? ['operator'] : [])], need())
        .then((out) => out !== null && mirror.note(out.text, out.error ? 'error' : 'info'))
        .catch((e: unknown) => mirror.note(failureText(e), 'error'));
    },
    [need, mirror],
  );

  const active = mirror.activeTasks().length;
  const input = chromeInput(s, endpoint, active);
  const edge = (items: readonly string[]) =>
    items.map((n, i) => (
      <React.Fragment key={`${n}${i}`}>
        <EdgeItem name={n} input={input} />
      </React.Fragment>
    ));
  // The debug screen exists only while the daemon offers introspection; a
  // switch turned off under it falls back to the conversation.
  const shown: Screen = screen === 'debug' && !debugOn ? 'chat' : screen;
  return (
    <div className="app">
      <div className="hdr">
        {edge(layout.top)}
        <span className="tabs">
          {(['chat', 'tasks', 'subagents', 'debug'] as Screen[])
            .filter((t) => t !== 'debug' || debugOn)
            .map((t) => (
              <button key={t} className={shown === t ? 'on' : ''} onClick={() => setScreen(t)}>
                {t}
              </button>
            ))}
        </span>
      </div>
      {banner ? (
        <div className="row err">
          <span className="mark">✗</span>
          <span className="body">{banner.join(' — ')}</span>
        </div>
      ) : null}
      {approving ? (
        <ApproveDialog
          userCode={approving.userCode}
          name={approving.name}
          operator={approving.operator}
          onApprove={(name, operator) => approve(approving.userCode, name, operator)}
          onCancel={() => {
            setApproving(null);
            mirror.note(`not approved: ${approving.userCode}`);
          }}
        />
      ) : null}
      <div className="main">
        {shown === 'chat' ? (
          <Chat mirror={mirror} gate={currentGate(s, ctx)} onSend={onSend} />
        ) : !client ? (
          <div className="pane">
            <div className="scroll">
              <div className="row note">connecting…</div>
            </div>
          </div>
        ) : shown === 'tasks' ? (
          <Tasks mirror={mirror} client={client} />
        ) : shown === 'subagents' ? (
          <Subagents mirror={mirror} client={client} />
        ) : (
          <Debug mirror={mirror} client={client} />
        )}
      </div>
      <div className="statusbar">{edge(layout.bottom)}</div>
    </div>
  );
}

/**
 * The form a gate's schema describes.
 *
 * A gate that says "one of these three" should offer three options, not a text
 * box the person has to guess the wording for — and then guess again when the
 * answer is rejected. The schema already knows the acceptable answers, so the
 * only job here is to show them.
 */
function GateForm({
  task,
  form,
  onAnswer,
}: {
  task: TaskView;
  form: AskForm;
  onAnswer: (value: Json) => void;
}): React.JSX.Element {
  const [picked, setPicked] = useState<string[]>(() =>
    form.kind === 'many' ? (form.def ?? []) : form.kind === 'one' && form.def ? [form.def] : [],
  );
  const [other, setOther] = useState('');
  const multi = form.kind === 'many';
  const options =
    form.kind === 'bool'
      ? ['yes', 'no']
      : form.kind === 'one' || form.kind === 'many'
        ? form.options
        : [];
  const allowOther = (form.kind === 'one' || form.kind === 'many') && form.other;

  const toggle = (v: string) =>
    setPicked((cur) =>
      multi ? (cur.includes(v) ? cur.filter((x) => x !== v) : [...cur, v]) : [v],
    );
  const ready =
    picked.length > 0 && (!picked.includes('__other__') || other.trim().length > 0);

  return (
    <form
      className="gate"
      onSubmit={(e) => {
        e.preventDefault();
        if (ready) onAnswer(askAnswer(form, picked, other));
      }}
    >
      <div className="gate-q">{task.message ?? 'The agent needs your input.'}</div>
      <div className="gate-opts">
        {options.map((o) => (
          <button
            type="button"
            key={o}
            className={`gate-opt ${picked.includes(o) ? 'on' : ''}`}
            aria-pressed={picked.includes(o)}
            onClick={() => toggle(o)}
          >
            <span className={multi ? 'box' : 'dot'} aria-hidden="true" />
            {o}
          </button>
        ))}
        {allowOther ? (
          <button
            type="button"
            className={`gate-opt ${picked.includes('__other__') ? 'on' : ''}`}
            aria-pressed={picked.includes('__other__')}
            onClick={() => toggle('__other__')}
          >
            <span className={multi ? 'box' : 'dot'} aria-hidden="true" />
            other…
          </button>
        ) : null}
      </div>
      {picked.includes('__other__') ? (
        <input
          className="gate-other"
          value={other}
          onChange={(e) => setOther(e.target.value)}
          placeholder="your answer"
          aria-label="your answer"
          autoFocus
        />
      ) : null}
      <div className="gate-actions">
        <button className="mini" type="submit" disabled={!ready}>
          {multi ? `answer (${picked.filter((p) => p !== '__other__').length + (other.trim() ? 1 : 0)})` : 'answer'}
        </button>
        <span className="gate-hint">
          {multi ? 'pick any number' : 'pick one'} · or type a reply below
        </span>
      </div>
    </form>
  );
}
