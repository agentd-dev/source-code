// SPDX-License-Identifier: AGPL-3.0-only
/**
 * The observation driver: keeps a {@link Mirror} converged with one agent.
 *
 * Discovery first: the card says where JSON-RPC goes and what this client may
 * call, and the {@link AgentdClient} built from it refuses the rest. Then one
 * of two modes, both first-class:
 *
 * - **events** (the card declares events/v1): bootstrap (`status` when the
 *   card offers it, plus paged ListTasks), then hold the feed, resuming from
 *   the goodbye's cursor across the server's stream deadline.
 * - **core** (no feed declared, `--no-extensions`, or the agent does not
 *   serve the one it declared): only A2A 1.0 core methods — paged ListTasks
 *   polled with `statusTimestampAfter`, and SubscribeToTask on the tasks that
 *   are moving. Against any A2A agent, not just agentd.
 *
 * Every failure goes through `classify`. An unauthenticated, forbidden or
 * incompatible answer is terminal: retrying cannot fix it, so every loop and
 * stream stops, `conn` says why, and `onTerminal` fires once. Everything else
 * waits (Retry-After, or a backoff) and re-opens the session.
 */

import { AgentdClient } from './client.js';
import { Mirror } from './mirror.js';
import {
  ClientError,
  FeedEvent,
  RpcError,
  TASK_NOT_FOUND,
  TaskState,
  TaskView,
  TERMINAL_STATES,
  UNSUPPORTED_OPERATION,
} from './types.js';
import { cardCache, cardUrlOf, openSession, Session } from './discovery.js';
import type { Credential } from './auth.js';
import { expiryWatch } from './credstore.js';
import { classify, describe, Failure } from './errors.js';
import { OPS } from './ext.js';
import type { ListQuery } from './a2a.js';

export interface ObserveOptions {
  /** Where the agent is: its base URL, or its card's URL. */
  configured: string;
  /** The person's credential, for the configured endpoint only. */
  credential?: Credential;
  /** Speak core A2A only (`--no-extensions`, `?extensions=off`). */
  noExtensions?: boolean;
  /** Poll cadence in core mode (ms; default 1500). */
  pollMs?: number;
  /** Reconnect backoff cap (ms; default 5000). */
  backoffCapMs?: number;
  /**
   * How many SubscribeToTask streams core mode holds at once (default 3 in a
   * browser, 8 elsewhere). A browser allows six connections per origin, and
   * the unary calls need some of them.
   */
  maxTaskStreams?: number;
  /** Discovery settled: the session, and the client that speaks for it. */
  onSession?: (session: Session, client: AgentdClient) => void;
  /** A terminal failure stopped observation (called once). */
  onTerminal?: (f: Failure) => void;
}

/** The states listed one by one at bootstrap, so a gate past page one is never lost. */
const LIVE_STATES: readonly TaskState[] = [
  'TASK_STATE_SUBMITTED',
  'TASK_STATE_WORKING',
  'TASK_STATE_INPUT_REQUIRED',
  'TASK_STATE_AUTH_REQUIRED',
];
/** History asked for with every listing: enough for a transcript to make sense. */
const LIST_HISTORY = 20;
/** The newest pages listed unfiltered at bootstrap. */
const NEWEST_PAGES = 2;
/** History asked for when a task event arrived without any. */
const LAZY_HISTORY = 10;
/** Consecutive early ends a task stream is resumed after, before polling takes over. */
const STREAM_RESUMES = 3;
const MIN_BACKOFF = 250;

/** A browser allows six connections per origin; stay well under it. */
function defaultMaxStreams(): number {
  const g = globalThis as { window?: unknown; document?: unknown };
  return g.window !== undefined && g.document !== undefined ? 3 : 8;
}

/**
 * A task a SubscribeToTask stream can follow: moving, and not waiting for
 * anyone. The spec's stream closes at an interrupted state, so a stream on a
 * gate would end as soon as it opened; a gate is seen by the poll instead.
 */
function followable(t: TaskView): boolean {
  return t.state === 'TASK_STATE_SUBMITTED' || t.state === 'TASK_STATE_WORKING';
}

/** Terminal failures: nothing a retry can fix. */
function isTerminal(f: Failure): boolean {
  return f.kind === 'unauthenticated' || f.kind === 'forbidden' || f.kind === 'incompatible';
}

/** Resolves after `ms`, or at once when `signal` aborts. */
function sleep(ms: number, signal?: AbortSignal): Promise<void> {
  return new Promise((resolve) => {
    if (signal?.aborted) return resolve();
    const t = setTimeout(done, ms);
    function done(): void {
      clearTimeout(t);
      signal?.removeEventListener('abort', done);
      resolve();
    }
    signal?.addEventListener('abort', done, { once: true });
  });
}

export class Observation {
  private mirror: Mirror;
  private opts: ObserveOptions & { pollMs: number; backoffCapMs: number; maxTaskStreams: number };
  private stopped = false;
  private ended = false;
  /** Aborted to end the current session: stop, a terminal failure, or refresh. */
  private session?: AbortController;
  private refreshWanted = false;
  private backoff = MIN_BACKOFF;
  /** SubscribeToTask streams, by task id. */
  private followers = new Map<string, AbortController>();
  /** Tasks this client started: followed first, and polled when the agent cannot stream. */
  private own = new Set<string>();
  /** Tasks already asked for their history once. */
  private historyAsked = new Set<string>();
  /** The newest `status.timestamp` seen, for incremental ListTasks (epoch ms). */
  private newest = 0;
  private notedTruncated = false;
  private lastProtocol?: string;
  private cleanups: (() => void)[] = [];

  constructor(opts: ObserveOptions, mirror: Mirror) {
    this.mirror = mirror;
    this.opts = {
      ...opts,
      pollMs: opts.pollMs ?? 1500,
      backoffCapMs: opts.backoffCapMs ?? 5000,
      maxTaskStreams: opts.maxTaskStreams ?? defaultMaxStreams(),
    };
    this.backoff = Math.min(MIN_BACKOFF, this.opts.backoffCapMs);
  }

  /** Start observing (returns immediately; runs until {@link stop} or a terminal failure). */
  start(): void {
    this.cleanups.push(this.mirror.onConfig(() => this.refresh()));
    const c = this.opts.credential;
    if (c?.expiresAt !== undefined) {
      // There is no refresh token: an expired session is a sign-in, not a retry.
      const w = expiryWatch(c, {
        warn: () => this.mirror.note('this session expires in a minute — sign in again to keep it', 'error'),
        expired: () => this.terminate(classify(new ClientError('session-expired', 'the session expired'))),
      });
      this.cleanups.push(w.stop);
    }
    void this.run();
  }

  stop(): void {
    const wasEnded = this.ended;
    this.halt();
    if (!wasEnded) this.mirror.setConn('closed');
  }

  /**
   * Re-open the session: re-read the card (bypassing its cache — what it
   * offers is exactly what may have changed), re-bootstrap, resume the feed.
   */
  refresh(): void {
    if (this.stopped) return;
    this.refreshWanted = true;
    cardCache.delete(cardUrlOf(this.opts.configured));
    this.session?.abort();
  }

  /**
   * A task this client started (a send, a workflow run). Core mode follows it
   * before others, and polls it with GetTask when the agent does not stream.
   */
  track(taskId: string): void {
    this.own.add(taskId);
  }

  private halt(): void {
    this.stopped = true;
    this.session?.abort();
    for (const ac of this.followers.values()) ac.abort();
    this.followers.clear();
    for (const fn of this.cleanups.splice(0)) fn();
  }

  /** Stop everything for good and say why — once. */
  private terminate(f: Failure): void {
    if (this.ended || this.stopped) return;
    this.ended = true;
    this.halt();
    this.mirror.setConn(f.kind as 'unauthenticated' | 'forbidden' | 'incompatible', describe(f));
    this.opts.onTerminal?.(f);
  }

  private resetBackoff(): void {
    this.backoff = Math.min(MIN_BACKOFF, this.opts.backoffCapMs);
  }

  /**
   * How long to wait after a failure, or `undefined` when it was terminal.
   * A protocol error is shown once and retried at the cap: one side is wrong,
   * and hammering will not make it right.
   */
  private failed(e: unknown): number | undefined {
    const f = classify(e);
    if (isTerminal(f)) {
      this.terminate(f);
      return undefined;
    }
    this.mirror.setConn('error', describe(f));
    switch (f.kind) {
      case 'rate-limited':
        return f.retryAfterMs ?? this.backoff;
      case 'protocol':
        if (f.message !== this.lastProtocol) {
          this.lastProtocol = f.message;
          this.mirror.note(describe(f), 'error');
        }
        return this.opts.backoffCapMs;
      default: {
        const wait = this.backoff;
        this.backoff = Math.min(this.backoff * 2, this.opts.backoffCapMs);
        return wait;
      }
    }
  }

  private async run(): Promise<void> {
    while (!this.stopped) {
      const session = new AbortController();
      this.session = session;
      this.refreshWanted = false;
      try {
        this.mirror.setConn('connecting');
        const s = await openSession(this.opts.configured, {
          credential: this.opts.credential,
          noExtensions: this.opts.noExtensions,
          signal: session.signal,
        });
        if (session.signal.aborted) continue;
        const client = new AgentdClient(s.ep, s.caps);
        this.mirror.setSession(s);
        for (const w of s.warnings) this.mirror.note(w);
        this.opts.onSession?.(s, client);
        const core = s.caps.events === null || (await this.eventsLoop(client, session.signal)) === 'core';
        if (core && !session.signal.aborted) await this.coreLoop(client, session.signal);
      } catch (e) {
        // A refresh or a stop aborted what was in flight: not a failure.
        if (session.signal.aborted) continue;
        const wait = this.failed(e);
        if (wait === undefined) return;
        await sleep(wait, session.signal);
      } finally {
        session.abort();
        for (const ac of this.followers.values()) ac.abort();
        this.followers.clear();
      }
    }
  }

  // ---- reads shared by both modes ----------------------------------------

  /**
   * The bootstrap: the `status` document when the card offers the op (a
   * command/v2 read — never sent to an agent that did not declare it), one
   * listing per live state so no gate is lost past page one, and the newest
   * pages with enough history for the transcript.
   */
  private async bootstrap(client: AgentdClient): Promise<void> {
    const q = { historyLength: LIST_HISTORY };
    const [status, ...lists] = await Promise.all([
      client.offers(OPS.status) ? client.status() : Promise.resolve(null),
      ...LIVE_STATES.map((state) => client.listTasks({ ...q, status: state })),
      client.listTasks(q, NEWEST_PAGES),
    ]);
    if (status !== null) this.mirror.bootstrap(status);
    // The unfiltered listing stops at its page cap by design; only a state
    // listing that ran out of pages is news.
    this.adopt(lists.flatMap((l) => l.tasks), lists.slice(0, LIVE_STATES.length).some((l) => l.truncated));
  }

  /** Adopt listed tasks that changed, and remember the newest timestamp. */
  private adopt(tasks: TaskView[], truncated = false): void {
    const s = this.mirror.getState();
    const changed = tasks.filter((t) => {
      const have = s.tasks.get(t.id);
      if (!have) return true;
      // A listing can be older than what a task stream already said; it
      // never moves a task backwards.
      if (t.updated < have.updated) return false;
      return have.updated !== t.updated || have.state !== t.state || have.history.length < t.history.length;
    });
    for (const t of tasks) if (t.updated > this.newest) this.newest = t.updated;
    if (changed.length > 0) this.mirror.adoptTasks(changed);
    if (truncated && !this.notedTruncated) {
      this.notedTruncated = true;
      this.mirror.note('the agent holds more live tasks than this client lists; the oldest are not shown');
    }
  }

  // ---- events mode -------------------------------------------------------

  /**
   * Hold the events/v1 feed. Returns 'core' when the agent does not serve the
   * feed it declared, 'done' when the session was aborted; throws anything
   * else to `run`.
   */
  private async eventsLoop(client: AgentdClient, signal: AbortSignal): Promise<'core' | 'done'> {
    let bootstrapped = false;
    // Where to resume: the last goodbye's cursor. Without one (a first
    // connect, a drop) the highest seq applied — the goodbye is the better
    // answer, because a caller who may see only some events has a cursor
    // well past the last one it saw.
    let cursor: number | undefined;
    while (!signal.aborted) {
      if (!bootstrapped) {
        this.mirror.setConn('connecting');
        await this.bootstrap(client);
        bootstrapped = true;
      }
      let bye;
      try {
        bye = await client.subscribeEvents(
          cursor ?? this.mirror.getState().lastSeq,
          (hello) => {
            this.mirror.onHello(hello);
            this.mirror.setConn('ready');
            this.resetBackoff();
            if (hello.resync) {
              // The mirror reset its cursor already; the sections and tasks
              // need the same, now rather than at the next reconnect.
              this.bootstrap(client).catch(() => {
                bootstrapped = false;
              });
            }
          },
          (ev) => this.onEvent(client, ev, signal),
          signal,
        );
      } catch (e) {
        if (signal.aborted) return 'done';
        if (classify(e, { extensionCall: true }).kind === 'unavailable') {
          this.mirror.note('the agent does not serve its event feed; following tasks with core A2A instead');
          return 'core';
        }
        throw e;
      }
      if (bye?.reason === 'revoked') throw new ClientError('session-revoked', 'the agent revoked this session');
      cursor = bye?.seq ?? this.mirror.getState().lastSeq;
      // A goodbye is the stream's planned end: resume at once. A stream that
      // just stopped is a drop, and is retried after a backoff.
      if (bye === undefined) {
        await sleep(this.backoff, signal);
        this.backoff = Math.min(this.backoff * 2, this.opts.backoffCapMs);
      }
    }
    return 'done';
  }

  /**
   * One feed event. A `task` event carries at most a few messages of
   * history; a turn that arrives with none, and whose prompt this client has
   * not seen, is read once with GetTask so its conversation shows.
   */
  private onEvent(client: AgentdClient, ev: FeedEvent, signal: AbortSignal): void {
    this.mirror.apply(ev);
    if (ev.kind !== 'task') return;
    const id = (ev.data as { task?: { id?: unknown } } | null)?.task?.id;
    if (typeof id !== 'string' || this.historyAsked.has(id)) return;
    const s = this.mirror.getState();
    const t = s.tasks.get(id);
    if (!t || t.history.length > 0 || !(t.link?.kind === 'turn' || t.contextId.length > 0)) return;
    if (s.transcript.some((e) => e.taskId === id && (e.kind === 'user' || e.kind === 'command'))) return;
    this.historyAsked.add(id);
    client.getTask(id, LAZY_HISTORY).then(
      (full) => {
        if (full && !signal.aborted) this.mirror.adoptTasks([full]);
      },
      () => {
        /* the task went away, or this caller may not read it: no history to show */
      },
    );
  }

  // ---- core mode ---------------------------------------------------------

  /**
   * Converge with core A2A only: poll ListTasks for what changed since the
   * newest timestamp seen, re-read `status` only when the card offers it,
   * and follow moving tasks with SubscribeToTask when the agent streams.
   */
  private async coreLoop(client: AgentdClient, signal: AbortSignal): Promise<void> {
    this.mirror.setConn('connecting');
    await this.bootstrap(client);
    this.mirror.setConn('polling');
    this.resetBackoff();
    while (!signal.aborted) {
      this.follow(client, signal);
      await sleep(this.opts.pollMs, signal);
      if (signal.aborted) return;
      await this.poll(client);
      this.mirror.setConn('polling');
    }
  }

  private async poll(client: AgentdClient): Promise<void> {
    const q: ListQuery = { historyLength: LIST_HISTORY };
    // `statusTimestampAfter` is inclusive, so the newest task comes back every
    // time; `adopt` drops what did not change.
    if (this.newest > 0) q.statusTimestampAfter = new Date(this.newest).toISOString();
    const [listed, status] = await Promise.all([
      client.listTasks(q),
      client.offers(OPS.status) ? client.status() : Promise.resolve(null),
    ]);
    if (status !== null) this.mirror.bootstrap(status);
    this.adopt(listed.tasks);
    if (client.caps.streaming) return;
    // No streams: this client's own unfinished tasks are read one by one, so
    // a reply shows even on an agent whose listing lags.
    const s = this.mirror.getState();
    const mine = [...this.own].filter((id) => {
      const t = s.tasks.get(id);
      return t === undefined || !TERMINAL_STATES.has(t.state);
    });
    const got = await Promise.all(mine.map((id) => client.getTask(id, LIST_HISTORY).catch(() => null)));
    this.adopt(got.filter((t): t is TaskView => t !== null));
    for (const id of this.own) {
      const t = s.tasks.get(id);
      if (t && TERMINAL_STATES.has(t.state)) this.own.delete(id);
    }
  }

  /** Start SubscribeToTask streams on moving tasks, this client's own first, up to the cap. */
  private follow(client: AgentdClient, signal: AbortSignal): void {
    if (!client.caps.streaming) return;
    const live = [...this.mirror.getState().tasks.values()]
      .filter((t) => followable(t) && !this.followers.has(t.id))
      .sort((a, b) => Number(this.own.has(b.id)) - Number(this.own.has(a.id)) || b.updated - a.updated);
    for (const t of live) {
      if (this.followers.size >= this.opts.maxTaskStreams) return;
      this.startFollower(client, t.id, signal);
    }
  }

  /**
   * Follow one task. The stream closes when the task ends or stops at a gate;
   * one that ends while the task is still moving is resumed after its last
   * SSE id, a few times, before the poll takes over again.
   */
  private startFollower(client: AgentdClient, id: string, parent: AbortSignal): void {
    const ac = new AbortController();
    const onAbort = (): void => ac.abort();
    parent.addEventListener('abort', onAbort, { once: true });
    this.followers.set(id, ac);
    void (async () => {
      let last: string | undefined;
      let ends = 0;
      try {
        while (!ac.signal.aborted) {
          try {
            await client.subscribeTask(
              id,
              (frame, sseId) => {
                if (sseId !== undefined) last = sseId;
                this.mirror.applyStream(frame);
              },
              ac.signal,
              last,
            );
          } catch (e) {
            if (ac.signal.aborted) return;
            if (e instanceof RpcError && e.code === UNSUPPORTED_OPERATION) {
              // Terminal already (or not streamable after all): read it once.
              const t = await client.getTask(id, LIST_HISTORY).catch(() => null);
              if (t && !ac.signal.aborted) this.mirror.adoptTasks([t]);
              return;
            }
            if (e instanceof RpcError && e.code === TASK_NOT_FOUND) {
              this.mirror.removeTask(id);
              return;
            }
            const f = classify(e);
            if (isTerminal(f)) this.terminate(f);
            if (f.kind !== 'transient') return;
          }
          const t = this.mirror.getState().tasks.get(id);
          if (!t || !followable(t) || ++ends > STREAM_RESUMES) return;
          await sleep(MIN_BACKOFF * ends, ac.signal);
        }
      } finally {
        parent.removeEventListener('abort', onAbort);
        if (this.followers.get(id) === ac) this.followers.delete(id);
      }
    })();
  }
}
