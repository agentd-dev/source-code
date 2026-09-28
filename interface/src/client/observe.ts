// SPDX-License-Identifier: AGPL-3.0-only
/**
 * The observation driver: keeps a {@link Mirror} converged with one agent.
 *
 * Discovery first: the card says where JSON-RPC goes and what this client may
 * call, and the {@link AgentdClient} built from it refuses the rest. Then
 * feed-first: bootstrap (`status`, when the card offers it, + `ListTasks`),
 * then hold the events/v1 stream, resuming with `fromSeq` across the server's
 * stream deadline and transport drops (`hello.resync` triggers a
 * re-bootstrap). When the card declares no feed, or the agent refuses it, it
 * degrades to POLLING the same reads on an interval, so the renderer code
 * never knows the difference (only `conn` shows 'polling' instead of 'ready').
 */

import { AgentdClient } from './client.js';
import { introspectionOn, Mirror } from './mirror.js';
import { METHOD_NOT_FOUND, RpcError, UNSUPPORTED_OPERATION } from './types.js';
import { openSession, Session } from './discovery.js';
import type { Credential } from './auth.js';
import { OPS } from './ext.js';

export interface ObserveOptions {
  /** Where the agent is: its base URL, or its card's URL. */
  configured: string;
  /** The person's credential, for the configured endpoint only. */
  credential?: Credential;
  /** Speak core A2A only (`--no-extensions`, `?extensions=off`). */
  noExtensions?: boolean;
  /** Poll cadence in fallback mode (ms; default 1500). */
  pollMs?: number;
  /** Reconnect backoff cap (ms; default 5000). */
  backoffCapMs?: number;
  /** Discovery settled: the session, and the client that speaks for it. */
  onSession?: (session: Session, client: AgentdClient) => void;
}

export class Observation {
  private mirror: Mirror;
  private opts: ObserveOptions & { pollMs: number; backoffCapMs: number };
  private stopped = false;
  private abort?: AbortController;

  constructor(opts: ObserveOptions, mirror: Mirror) {
    this.mirror = mirror;
    this.opts = { ...opts, pollMs: opts.pollMs ?? 1500, backoffCapMs: opts.backoffCapMs ?? 5000 };
  }

  /** Start observing (returns immediately; runs until {@link stop}). */
  start(): void {
    void this.run();
  }

  stop(): void {
    this.stopped = true;
    this.abort?.abort();
    this.mirror.setConn('closed');
  }

  private async bootstrap(client: AgentdClient): Promise<void> {
    // `status` is a command/v2 read: asked only when the card offers it, so a
    // stock A2A agent is never sent a DataPart it would read as a message.
    const [status, listed] = await Promise.all([
      client.offers(OPS.status) ? client.status() : Promise.resolve(null),
      client.listTasks(),
    ]);
    if (status !== null) this.mirror.bootstrap(status);
    this.mirror.adoptTasks(listed.tasks);
    void this.backfill(client);
  }

  /**
   * One-shot transcript hydration at attach (debug daemons only): read the
   * most recently updated conversation's stored history so the operator
   * doesn't start from a blank screen. Best-effort — a non-debug daemon
   * simply refuses the read.
   */
  private backfilled = false;
  private async backfill(client: AgentdClient): Promise<void> {
    if (this.backfilled) return;
    this.backfilled = true;
    const s = this.mirror.getState();
    if (!introspectionOn(s) || s.transcript.length > 0 || s.conversations.size === 0) return;
    const newest = [...s.conversations.values()]
      .map((c) => c as { id?: string; updated?: number; kind?: string })
      .filter((c) => typeof c.id === 'string' && c.kind !== 'root')
      .sort((a, b) => (b.updated ?? 0) - (a.updated ?? 0))[0];
    if (!newest?.id) return;
    try {
      const conv = (await client.conversationGet(newest.id, 100)) as {
        messages?: unknown[];
      } | null;
      if (Array.isArray(conv?.messages)) {
        this.mirror.backfillTranscript(newest.id, conv.messages as never[]);
      }
    } catch {
      /* debug off / not owner — start blank */
    }
  }

  /** Discover the agent until it answers; `undefined` once stopped. */
  private async discover(): Promise<AgentdClient | undefined> {
    let backoff = 250;
    while (!this.stopped) {
      try {
        const session = await openSession(this.opts.configured, {
          credential: this.opts.credential,
          noExtensions: this.opts.noExtensions,
        });
        if (this.stopped) return undefined;
        const client = new AgentdClient(session.ep, session.caps);
        this.mirror.setSession(session);
        for (const w of session.warnings) this.mirror.note(w);
        this.opts.onSession?.(session, client);
        return client;
      } catch (e) {
        if (this.stopped) return undefined;
        this.mirror.setConn('error', e instanceof Error ? e.message : String(e));
        await sleep(backoff);
        backoff = Math.min(backoff * 2, this.opts.backoffCapMs);
      }
    }
    return undefined;
  }

  private async run(): Promise<void> {
    const client = await this.discover();
    if (!client) return;
    if (client.caps.events === null) {
      // No feed declared: converge by polling instead.
      await this.pollLoop(client);
      return;
    }

    let backoff = 250;
    let bootstrapped = false;
    while (!this.stopped) {
      try {
        if (!bootstrapped) {
          this.mirror.setConn('connecting');
          await this.bootstrap(client);
          bootstrapped = true;
        }
        this.abort = new AbortController();
        const from = this.mirror.getState().lastSeq;
        let resync = false;
        this.mirror.setConn('ready');
        await client.subscribeEvents(
          from,
          (hello) => {
            this.mirror.onHello(hello);
            if (hello.resync) resync = true;
          },
          (ev) => this.mirror.apply(ev),
          this.abort.signal,
        );
        // Clean end (stream deadline): reconnect from the cursor at once.
        backoff = 250;
        if (resync) bootstrapped = false;
      } catch (e) {
        if (this.stopped) return;
        if (e instanceof RpcError && (e.code === UNSUPPORTED_OPERATION || e.code === METHOD_NOT_FOUND)) {
          // The card declared a feed the agent does not serve: poll instead.
          await this.pollLoop(client);
          return;
        }
        this.mirror.setConn('error', e instanceof Error ? e.message : String(e));
        bootstrapped = false; // re-bootstrap after an outage — state may have moved
        await sleep(backoff);
        backoff = Math.min(backoff * 2, this.opts.backoffCapMs);
      }
    }
  }

  /** The fallback: converge by re-reading `status` (when offered) + `ListTasks`. */
  private async pollLoop(client: AgentdClient): Promise<void> {
    let backoff = 250;
    while (!this.stopped) {
      try {
        await this.bootstrap(client);
        this.mirror.setConn('polling');
        backoff = 250;
      } catch (e) {
        this.mirror.setConn('error', e instanceof Error ? e.message : String(e));
        backoff = Math.min(backoff * 2, this.opts.backoffCapMs);
      }
      await sleep(Math.max(this.opts.pollMs, backoff));
    }
  }
}

function sleep(ms: number): Promise<void> {
  return new Promise((r) => setTimeout(r, ms));
}
