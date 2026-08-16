// SPDX-License-Identifier: Apache-2.0
/**
 * The web UI, in the format of the TUI: the same @agentd/client Mirror the
 * terminal renders, projected to the DOM. Open it beside the TUI — both stay
 * in sync because both watch the same daemon feed; neither holds truth.
 * The chrome renders whatever `interface.display` declares; the composer
 * speaks `/` `@` `#` `$` via the shared composer rules; pairing-code login
 * (RFC 0032 §13) is the no-bearer way in.
 */
import React, { useCallback, useEffect, useMemo, useRef, useState, useSyncExternalStore } from 'react';
import {
  AgentdClient,
  Json,
  Mirror,
  Observation,
  Suggestion,
  TERMINAL_STATES,
  TaskView,
  TranscriptEntry,
  applySuggestion,
  prepare,
  suggest,
  workflowNames,
} from '@agentd/client';

type Screen = 'chat' | 'tasks' | 'subagents' | 'debug';

const DEFAULT_TOP = ['name', 'version', 'instance', 'debug'];
const DEFAULT_BOTTOM = ['conn', 'endpoint', 'draining', 'active', 'turns', 'tokens'];

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

function counters(m: Mirror):
  | { turns?: number; tokens_in?: number; tokens_out?: number; tool_calls?: number }
  | undefined {
  const s = m.getState();
  return ((s.status ?? s.bootstrap) as { counters?: { turns?: number; tokens_in?: number; tokens_out?: number; tool_calls?: number } } | undefined)
    ?.counters;
}

/** One display item for the top/bottom edges (RFC 0032 §12). */
function EdgeItem({ name, mirror, endpoint, active }: { name: string; mirror: Mirror; endpoint: string; active: number }): React.JSX.Element | null {
  const s = mirror.getState();
  switch (name) {
    case 'name':
      return <span className="name">{((s.card as { name?: string } | undefined)?.name ?? 'agentd') as string}</span>;
    case 'version':
      return s.info ? <span className="dim">{s.info.version}</span> : null;
    case 'instance':
      return s.info ? <span className="name">{s.info.instance}</span> : null;
    case 'model':
      return s.info?.model ? <span className="dim">{s.info.model}</span> : null;
    case 'endpoint':
      return <span className="dim">{endpoint}</span>;
    case 'conn':
      return (
        <span className={s.conn === 'ready' ? 'live' : s.conn === 'polling' ? 'polling' : s.conn === 'error' ? 'error' : 'dim'}>
          {s.conn === 'ready' ? '● live' : s.conn === 'polling' ? '◐ polling' : s.conn === 'error' ? `✗ ${s.error ?? 'error'}` : '○ connecting'}
        </span>
      );
    case 'debug':
      return s.info?.debug ? <span className="badge">debug</span> : null;
    case 'draining':
      return s.draining ? <span className="drain">DRAINING</span> : null;
    case 'active':
      return active > 0 ? <span className="live">{active} active</span> : null;
    case 'turns': {
      const n = counters(mirror)?.turns;
      return n !== undefined ? <span className="dim">{n} turns</span> : null;
    }
    case 'tokens': {
      const ct = counters(mirror);
      return ct ? <span className="dim">{ct.tokens_in ?? 0}/{ct.tokens_out ?? 0} tok</span> : null;
    }
    case 'tool_calls': {
      const n = counters(mirror)?.tool_calls;
      return n !== undefined ? <span className="dim">{n} tools</span> : null;
    }
    case 'runs':
      return <span className="dim">{s.runs.size} runs</span>;
    case 'subagents':
      return <span className="dim">{s.subagents.size} subagents</span>;
    case 'conversations':
      return <span className="dim">{s.conversations.size} conv</span>;
    case 'clock':
      return <span className="dim">{new Date().toLocaleTimeString()}</span>;
    default:
      return null; // unknown / tui-only items (screen, keys) — skip
  }
}

function Row({ e }: { e: TranscriptEntry }): React.JSX.Element {
  if (e.kind === 'user')
    return (
      <div className="row">
        <span className="who-user">you › </span>
        {e.text}
        {e.principal && e.principal !== 'operator' ? <span className="principal"> ({e.principal})</span> : null}
      </div>
    );
  if (e.kind === 'agent')
    return (
      <div className="row">
        <span className="who-agent">agent › </span>
        {e.text}
        {e.inputRequired ? <span className="gate"> [reply to continue]</span> : null}
      </div>
    );
  if (e.kind === 'error')
    return (
      <div className="row">
        <span className="who-err">agent ✗ </span>
        {e.text}
      </div>
    );
  if (e.kind === 'command') return <div className="row cmd">▸ {e.text}</div>;
  return <div className="row note">· {e.text}</div>;
}

function Chat({ mirror, onSend }: { mirror: Mirror; onSend: (text: string) => void }): React.JSX.Element {
  const s = mirror.getState();
  const [input, setInput] = useState('');
  const [sugIndex, setSugIndex] = useState(0);
  const scrollRef = useRef<HTMLDivElement>(null);
  useEffect(() => {
    scrollRef.current?.scrollTo({ top: scrollRef.current.scrollHeight });
  });
  useEffect(() => setSugIndex(0), [input]);
  const active = mirror.activeTasks();
  const suggestions: Suggestion[] = suggest(input, s);
  const accept = (i: number) => setInput(applySuggestion(input, suggestions[i] ?? suggestions[0]));
  return (
    <div className="pane">
      <div className="scroll" ref={scrollRef}>
        {s.transcript.map((e) => (
          <Row key={e.key} e={e} />
        ))}
        {active.length > 0 ? (
          <div className="working">
            ⠿ working — {active.length} task{active.length > 1 ? 's' : ''} live<span className="cursor">▌</span>
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
          <input
            autoFocus
            value={input}
            onChange={(ev) => setInput(ev.target.value)}
            onKeyDown={(ev) => {
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
            placeholder="message agentd — / commands · @skill · #target · $value"
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
                  <td>{t.link ? Object.keys(t.link)[0] : ''}</td>
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
  const [open, setOpen] = useState<{ handle: string; detail: Json | null } | null>(null);
  const subs = [...s.subagents.values()] as { [k: string]: Json }[];
  const openOne = (handle: string) => {
    setOpen({ handle, detail: null });
    void client
      .subagentGet(handle)
      .then((d) => setOpen((cur) => (cur?.handle === handle ? { handle, detail: d } : cur)))
      .catch(() => {
        /* summary-only (debug off) — the view says so */
      });
  };
  if (open) {
    const summary = (s.subagents.get(open.handle) ?? {}) as { [k: string]: Json };
    const d = (open.detail ?? summary) as { [k: string]: Json };
    const field = (label: string, v: Json | undefined, cls = '') =>
      v === undefined || v === null ? null : (
        <div className="field" key={label}>
          <span className="k">{label}</span>
          <span className={cls}>{typeof v === 'string' ? v : JSON.stringify(v, null, 1)}</span>
        </div>
      );
    return (
      <div className="pane">
        <div className="scroll detail">
          <button className="mini" onClick={() => setOpen(null)}>
            ← back to subagents
          </button>
          <h2>subagent {open.handle}</h2>
          {field('status', d.status)}
          {field('mode', d.mode)}
          {field('attempt', d.attempt)}
          {field('tokens', d.tokens)}
          {field('instruction', d.instruction)}
          {field('result', d.result)}
          {field('error', d.error, 'err')}
          {field('requested_by', d.requested_by)}
          {!s.info?.debug ? (
            <div className="row note">summary only — enable interface.debug (or /set interface.debug true) for instruction/result</div>
          ) : null}
        </div>
      </div>
    );
  }
  return (
    <div className="pane">
      <div className="scroll">
        {subs.length === 0 ? (
          <div className="row note">no subagents yet — the agent spawns them as it delegates</div>
        ) : (
          <table className="grid clickable">
            <thead>
              <tr>
                <th>handle</th>
                <th>mode</th>
                <th>status</th>
                <th>tokens</th>
                <th />
              </tr>
            </thead>
            <tbody>
              {subs.map((x, i) => (
                <tr key={String(x.handle ?? i)} onClick={() => openOne(String(x.handle ?? ''))}>
                  <td>{String(x.handle ?? '')}</td>
                  <td>{String(x.mode ?? '')}</td>
                  <td className={String(x.status) === 'completed' ? 'st-done' : String(x.status) === 'running' ? 'st-working' : String(x.status) === 'failed' ? 'st-failed' : 'st-queued'}>
                    {String(x.status ?? '')}
                  </td>
                  <td>{String(x.tokens ?? 0)}</td>
                  <td className="dim">open ›</td>
                </tr>
              ))}
            </tbody>
          </table>
        )}
      </div>
    </div>
  );
}

function Debug({ mirror, client }: { mirror: Mirror; client: AgentdClient }): React.JSX.Element {
  const s = mirror.getState();
  const [log, setLog] = useState<{ [k: string]: Json }[]>([]);
  const cursor = useRef(0);
  useEffect(() => {
    if (!s.info?.debug) return;
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
  }, [s.info?.debug, client]);
  if (!s.info?.debug)
    return (
      <div className="pane">
        <div className="scroll">
          <div className="row note">debug is off on this daemon — /set interface.debug true (operator) or set it in the config</div>
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
        <section>
          <h3>feed</h3>
          {s.feedLog.slice(-14).map((e) => (
            <div key={e.seq} className="line">
              {e.seq} <span className="k">{e.kind}</span> {line(e.data)}
            </div>
          ))}
        </section>
        <section>
          <h3>runs</h3>
          {[...s.runs.values()].slice(-10).map((r, i) => {
            const o = r as { [k: string]: Json };
            return (
              <div key={(o.id as string) ?? i} className="line">
                <span className="k">{o.id as string}</span> {o.status as string} {line(o.steps ?? null)}
              </div>
            );
          })}
        </section>
        <section>
          <h3>subagents / children</h3>
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
        <section>
          <h3>log</h3>
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

export interface Defaults {
  endpoint?: string;
  bearer?: string;
}

export function App({ defaults }: { defaults: Defaults }): React.JSX.Element {
  const stored = useMemo(() => {
    try {
      return JSON.parse(localStorage.getItem('agentd-ui') ?? '{}') as Defaults;
    } catch {
      return {};
    }
  }, []);
  const fromQuery = useMemo(() => {
    const q = new URLSearchParams(location.search);
    return { endpoint: q.get('endpoint') ?? undefined, bearer: q.get('bearer') ?? undefined };
  }, []);
  const [conn, setConn] = useState<Defaults | null>(() => {
    const endpoint = fromQuery.endpoint ?? defaults.endpoint ?? stored.endpoint;
    return endpoint ? { endpoint, bearer: fromQuery.bearer ?? defaults.bearer ?? stored.bearer } : null;
  });
  if (!conn) return <Connect onConnect={setConn} stored={stored} />;
  return <Connected conn={conn} onDisconnect={() => setConn(null)} />;
}

function Connect({ onConnect, stored }: { onConnect: (d: Defaults) => void; stored: Defaults }): React.JSX.Element {
  const [endpoint, setEndpoint] = useState(stored.endpoint ?? 'http://127.0.0.1:8420');
  const [bearer, setBearer] = useState('');
  const [code, setCode] = useState('');
  const [err, setErr] = useState('');
  const go = async () => {
    setErr('');
    const ep = endpoint.trim();
    let credential = bearer.trim() || undefined;
    // A pairing code (RFC 0032 §13) exchanges for a session token — no bearer
    // to copy: read the 6 digits off the operator's screen (/pair).
    if (!credential && code.trim()) {
      try {
        const session = await new AgentdClient({ url: ep }).pair(code.trim());
        credential = session.token;
      } catch (e) {
        setErr(e instanceof Error ? e.message : String(e));
        return;
      }
    }
    const d = { endpoint: ep, bearer: credential };
    localStorage.setItem('agentd-ui', JSON.stringify(d));
    onConnect(d);
  };
  return (
    <div className="app">
      <div className="connect">
        <h1>agentd</h1>
        <p>
          Connect to a running agentd. The daemon needs <code>interface.enabled: true</code> (and your
          origin in <code>interface.origins</code> when this page isn't served from localhost).
        </p>
        <form
          onSubmit={(e) => {
            e.preventDefault();
            void go();
          }}
        >
          <label>endpoint (a2a.listen)</label>
          <input value={endpoint} onChange={(e) => setEndpoint(e.target.value)} placeholder="http://127.0.0.1:8420" />
          <label>pairing code — ask the operator for /pair (rotates every minute)</label>
          <input value={code} onChange={(e) => setCode(e.target.value)} placeholder="123456" inputMode="numeric" autoComplete="one-time-code" />
          <label>or a bearer (only if a2a.bearer is configured)</label>
          <input value={bearer} onChange={(e) => setBearer(e.target.value)} type="password" />
          <button type="submit">connect</button>
          {err ? <div className="err">{err}</div> : null}
        </form>
      </div>
    </div>
  );
}

function Connected({ conn, onDisconnect }: { conn: Defaults; onDisconnect: () => void }): React.JSX.Element {
  const client = useMemo(
    () => new AgentdClient({ url: conn.endpoint as string, bearer: conn.bearer }),
    [conn.endpoint, conn.bearer],
  );
  const mirror = useMemo(() => new Mirror(), [client]);
  useSyncExternalStore(mirror.subscribe, mirror.getVersion);
  useEffect(() => {
    const obs = new Observation(client, mirror);
    obs.start();
    return () => obs.stop();
  }, [client, mirror]);
  const [screen, setScreen] = useState<Screen>('chat');
  const s = mirror.getState();
  const ctxRef = useRef<string | undefined>(undefined);

  const onSend = useCallback(
    (raw: string) => {
      const text = raw.trim();
      void (async () => {
        try {
          if (text.startsWith('/')) {
            const [cmd, ...rest] = text.slice(1).split(/\s+/);
            const arg = rest.join(' ');
            if (cmd === 'help')
              mirror.note('/new · /tasks · /subagents · /debug · /status · /config [path] · /set <path> <value> · /workflow <name> · /cancel [task] · /pair · /drain · /disconnect — plus @skill, #target, $value');
            else if (cmd === 'new') {
              ctxRef.current = undefined;
              mirror.note('new conversation');
            } else if (cmd === 'tasks' || cmd === 'subagents' || cmd === 'debug' || cmd === 'chat') setScreen(cmd as Screen);
            else if (cmd === 'status') {
              const st = (await client.status()) as { [k: string]: Json };
              mirror.bootstrap(st);
              mirror.note(`runs ${Array.isArray(st.runs) ? st.runs.length : 0} · subagents ${Array.isArray(st.subagents) ? st.subagents.length : 0} · draining ${st.draining}`);
            } else if (cmd === 'config') {
              const cfg = await client.config();
              if (arg) {
                let v: Json = (cfg as { config?: Json }).config ?? cfg;
                for (const part of arg.split('.')) v = (v as { [k: string]: Json } | null)?.[part] ?? null;
                mirror.note(`${arg} = ${JSON.stringify(v)}`);
              } else {
                mirror.note(`${JSON.stringify((cfg as { config?: Json }).config ?? cfg, null, 1).slice(0, 2000)}\n/config <path> for one value · /set for runtime knobs`);
              }
            } else if (cmd === 'set') {
              const [path, ...valueParts] = rest;
              if (!path || valueParts.length === 0) {
                mirror.note('usage: /set <path> <value>', 'error');
                return;
              }
              let value: Json;
              try {
                value = JSON.parse(valueParts.join(' ')) as Json;
              } catch {
                value = valueParts.join(' ');
              }
              await client.configSet(path, value);
            } else if (cmd === 'pair') {
              const p = await client.pairingCode();
              mirror.note(`pairing code: ${p.code} (valid ${Math.ceil(p.expires_in_ms / 1000)}s, role ${p.role}, ${p.sessions} sessions)`);
            } else if (cmd === 'workflow') {
              const r = await client.workflowRun(arg);
              mirror.note(`workflow → ${r.task?.id ?? '?'}`);
            } else if (cmd === 'cancel') {
              const id = rest[0] ?? mirror.activeTasks()[0]?.id;
              if (id) await client.cancelTask(id);
            } else if (cmd === 'drain') {
              await client.drain();
              mirror.note('draining requested');
            } else if (cmd === 'disconnect') onDisconnect();
            else if (workflowNames(s).includes(cmd)) {
              const r = await client.workflowRun(cmd);
              mirror.note(`workflow ${cmd} → ${r.task?.id ?? '?'}`);
            } else mirror.note(`unknown command /${cmd} — /help`, 'error');
            return;
          }
          const p = prepare(text, s);
          const gate = p.taskId ?? mirror.activeTasks().find((t) => t.state === 'TASK_STATE_INPUT_REQUIRED')?.id;
          const sent = await client.send(p.text, { contextId: p.contextId ?? ctxRef.current, taskId: gate });
          if (sent.task) {
            ctxRef.current = sent.task.contextId || ctxRef.current;
            mirror.adoptTasks([sent.task]);
          }
          mirror.localEcho(sent.messageId, sent.task?.contextId ?? ctxRef.current, p.text, sent.task?.id);
        } catch (e) {
          mirror.note(e instanceof Error ? e.message : String(e), 'error');
        }
      })();
    },
    [client, mirror, onDisconnect, s],
  );

  const info = s.info;
  const active = mirror.activeTasks().length;
  const top = info?.display?.top ?? DEFAULT_TOP;
  const bottom = info?.display?.bottom ?? DEFAULT_BOTTOM;
  const edge = (items: string[]) =>
    items.map((n, i) => (
      <React.Fragment key={`${n}${i}`}>
        <EdgeItem name={n} mirror={mirror} endpoint={conn.endpoint as string} active={active} />
      </React.Fragment>
    ));
  return (
    <div className="app">
      <div className="hdr">
        {edge(top)}
        <span className="tabs">
          {(['chat', 'tasks', 'subagents', 'debug'] as Screen[])
            .filter((t) => t !== 'debug' || info?.debug)
            .map((t) => (
              <button key={t} className={screen === t ? 'on' : ''} onClick={() => setScreen(t)}>
                {t}
              </button>
            ))}
        </span>
      </div>
      <div className="main">
        {screen === 'chat' ? (
          <Chat mirror={mirror} onSend={onSend} />
        ) : screen === 'tasks' ? (
          <Tasks mirror={mirror} client={client} />
        ) : screen === 'subagents' ? (
          <Subagents mirror={mirror} client={client} />
        ) : (
          <Debug mirror={mirror} client={client} />
        )}
      </div>
      <div className="statusbar">{edge(bottom)}</div>
    </div>
  );
}
