// SPDX-License-Identifier: AGPL-3.0-only
/**
 * The observation driver: keeps a {@link Mirror} converged with one agentd.
 *
 * Feed-first: bootstrap (`status` + `ListTasks`), then hold a
 * `SubscribeToEvents` stream, resuming with `fromSeq` across the server's
 * stream deadline and transport drops (`hello.resync` triggers a
 * re-bootstrap). When the daemon serves no feed — `interface.enabled: false`
 * or an older agentd — it degrades to POLLING the same reads on an interval,
 * so the renderer code never knows the difference (only `conn` shows
 * 'polling' instead of 'ready').
 */

import { AgentdClient } from './client.js';
import { Mirror } from './mirror.js';
import { RpcError, UNSUPPORTED_OPERATION } from './types.js';

export interface ObserveOptions {
  /** Poll cadence in fallback mode (ms; default 1500). */
  pollMs?: number;
  /** Reconnect backoff cap (ms; default 5000). */
  backoffCapMs?: number;
}

export class Observation {
  private client: AgentdClient;
  private mirror: Mirror;
  private opts: Required<ObserveOptions>;
  private stopped = false;
  private abort?: AbortController;

  constructor(client: AgentdClient, mirror: Mirror, opts: ObserveOptions = {}) {
    this.client = client;
    this.mirror = mirror;
    this.opts = { pollMs: opts.pollMs ?? 1500, backoffCapMs: opts.backoffCapMs ?? 5000 };
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

  private async bootstrap(): Promise<void> {
    const [status, tasks] = await Promise.all([this.client.status(), this.client.listTasks()]);
    this.mirror.bootstrap(status);
    this.mirror.adoptTasks(tasks);
    void this.backfill();
  }

  /**
   * One-shot transcript hydration at attach (debug daemons only): read the
   * most recently updated conversation's stored history so the operator
   * doesn't start from a blank screen. Best-effort — a non-debug daemon
   * simply refuses the read.
   */
  private backfilled = false;
  private async backfill(): Promise<void> {
    if (this.backfilled) return;
    this.backfilled = true;
    const s = this.mirror.getState();
    if (!s.info?.debug || s.transcript.length > 0 || s.conversations.size === 0) return;
    const newest = [...s.conversations.values()]
      .map((c) => c as { id?: string; updated?: number; kind?: string })
      .filter((c) => typeof c.id === 'string' && c.kind !== 'root')
      .sort((a, b) => (b.updated ?? 0) - (a.updated ?? 0))[0];
    if (!newest?.id) return;
    try {
      const conv = (await this.client.conversationGet(newest.id, 100)) as {
        messages?: unknown[];
      } | null;
      if (Array.isArray(conv?.messages)) {
        this.mirror.backfillTranscript(newest.id, conv.messages as never[]);
      }
    } catch {
      /* debug off / not owner — start blank */
    }
  }

  private async run(): Promise<void> {
    // Discovery (best-effort; the card is public, info needs the surface on).
    try {
      this.mirror.setCard(await this.client.agentCard());
    } catch {
      /* offline — the connect loop below reports it */
    }
    try {
      this.mirror.setInfo(await this.client.interfaceInfo());
    } catch (e) {
      if (e instanceof RpcError && e.code === UNSUPPORTED_OPERATION) {
        // Interface off: fall straight back to polling the core surface.
        await this.pollLoop();
        return;
      }
      // Transport trouble — the feed loop below retries/reports.
    }

    let backoff = 250;
    let bootstrapped = false;
    while (!this.stopped) {
      try {
        if (!bootstrapped) {
          this.mirror.setConn('connecting');
          await this.bootstrap();
          bootstrapped = true;
        }
        this.abort = new AbortController();
        const from = this.mirror.getState().lastSeq;
        let resync = false;
        this.mirror.setConn('ready');
        await this.client.subscribeEvents(
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
        if (e instanceof RpcError && (e.code === UNSUPPORTED_OPERATION || e.code === -32601)) {
          // No feed on this daemon: degrade to polling permanently.
          await this.pollLoop();
          return;
        }
        this.mirror.setConn('error', e instanceof Error ? e.message : String(e));
        bootstrapped = false; // re-bootstrap after an outage — state may have moved
        await sleep(backoff);
        backoff = Math.min(backoff * 2, this.opts.backoffCapMs);
      }
    }
  }

  /** The fallback: converge by re-reading `status` + `ListTasks`. */
  private async pollLoop(): Promise<void> {
    let backoff = 250;
    while (!this.stopped) {
      try {
        await this.bootstrap();
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
