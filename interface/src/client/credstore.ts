// SPDX-License-Identifier: AGPL-3.0-only
/**
 * Where a client keeps what it signed in with — and, as importantly, where it
 * does not.
 *
 * A credential lives in a {@link CredentialStore} over a storage the CALLER
 * hands in: the browser passes `sessionStorage` (the tab's lifetime, never
 * shared with another tab or a later visit), the TUI a {@link memoryStorage}
 * (the process's lifetime). There is deliberately no default and no
 * `localStorage` path, so no code here can put a token where it outlives the
 * tab, syncs, or is read by the next person at the browser.
 *
 * Each credential is bound to the {@link endpointKey} it was issued for and
 * is handed back only for that key and only until it expires: a page opened
 * with a different `?endpoint=` finds nothing to send.
 *
 * The one thing a browser does persist is the endpoint itself
 * ({@link persistEndpoint}), and that writer keeps nothing else.
 */

import { systemClock } from './auth.js';
import type { Clock, Credential } from './auth.js';

/** The part of the Web Storage API used here; `sessionStorage` is one. */
export interface KeyValueStorage {
  getItem(key: string): string | null;
  setItem(key: string, value: string): void;
  removeItem(key: string): void;
}

/** The storage key of the credential map. */
export const CREDENTIAL_KEY = 'agentd-ui.cred';
/** The storage key of the remembered endpoint. */
export const ENDPOINT_KEY = 'agentd-ui';
/** How long before expiry {@link expiryWatch} warns. */
export const EXPIRY_WARNING_MS = 60_000;

/** A storage that lives as long as the process: the TUI's. */
export function memoryStorage(): KeyValueStorage {
  const m = new Map<string, string>();
  return {
    getItem: (k) => m.get(k) ?? null,
    setItem: (k, v) => void m.set(k, String(v)),
    removeItem: (k) => void m.delete(k),
  };
}

function credentialOf(v: unknown): Credential | undefined {
  if (v === null || typeof v !== 'object' || Array.isArray(v)) return undefined;
  const o = v as { [k: string]: unknown };
  if (typeof o.token !== 'string' || o.token.length === 0) return undefined;
  const c: Credential = { token: o.token };
  if (typeof o.scope === 'string') c.scope = o.scope;
  if (typeof o.expiresAt === 'number') c.expiresAt = o.expiresAt;
  return c;
}

/** Credentials by endpoint key, in the storage the caller chose. */
export class CredentialStore {
  private readonly storage: KeyValueStorage;
  private readonly clock: Pick<Clock, 'now'>;
  private readonly key: string;

  constructor(storage: KeyValueStorage, clock: Pick<Clock, 'now'> = systemClock, key: string = CREDENTIAL_KEY) {
    this.storage = storage;
    this.clock = clock;
    this.key = key;
  }

  private read(): Map<string, Credential> {
    const out = new Map<string, Credential>();
    const raw = this.storage.getItem(this.key);
    if (raw === null) return out;
    let v: unknown;
    try {
      v = JSON.parse(raw);
    } catch {
      return out; // unreadable: as good as empty, and overwritten on the next write
    }
    if (v === null || typeof v !== 'object' || Array.isArray(v)) return out;
    for (const [k, c] of Object.entries(v as { [k: string]: unknown })) {
      const cred = credentialOf(c);
      if (cred) out.set(k, cred);
    }
    return out;
  }

  private write(m: Map<string, Credential>): void {
    if (m.size === 0) this.storage.removeItem(this.key);
    else this.storage.setItem(this.key, JSON.stringify(Object.fromEntries(m)));
  }

  /**
   * The credential for exactly this endpoint key, unless it has expired. An
   * expired one is deleted as it is found: the daemon would refuse it, and
   * keeping it would only make the next start look signed in.
   */
  get(endpointKey: string): Credential | undefined {
    const m = this.read();
    const c = m.get(endpointKey);
    if (!c) return undefined;
    if (c.expiresAt !== undefined && c.expiresAt <= this.clock.now()) {
      m.delete(endpointKey);
      this.write(m);
      return undefined;
    }
    return c;
  }

  set(endpointKey: string, c: Credential): void {
    const m = this.read();
    // Copy the known fields only, so nothing else a caller's object carries
    // rides along into storage.
    const keep: Credential = { token: c.token };
    if (c.scope !== undefined) keep.scope = c.scope;
    if (c.expiresAt !== undefined) keep.expiresAt = c.expiresAt;
    m.set(endpointKey, keep);
    this.write(m);
  }

  delete(endpointKey: string): void {
    const m = this.read();
    if (m.delete(endpointKey)) this.write(m);
  }

  /** Forget every credential (sign-out everywhere this storage reaches). */
  clear(): void {
    this.storage.removeItem(this.key);
  }
}

/**
 * Remember the endpoint for the next visit, and ONLY the endpoint: whatever
 * else the object carries — a bearer, a token — is dropped here, so the
 * persistent storage this is given never holds a credential.
 */
export function persistEndpoint(storage: KeyValueStorage, v: { endpoint: string }): void {
  storage.setItem(ENDPOINT_KEY, JSON.stringify({ endpoint: v.endpoint }));
}

/**
 * The remembered endpoint, if any. Nothing else in that entry is read — and
 * nothing else is left in it: the v1.16 web UI stored `{endpoint, bearer}`
 * under this same key, often with the operator's bearer, so an entry carrying
 * anything besides the endpoint is rewritten to the endpoint alone (or
 * removed, when it has no endpoint to keep). Without that, an upgrade would
 * leave the old credential in persistent, cross-tab storage indefinitely.
 */
export function loadEndpoint(storage: KeyValueStorage): string | undefined {
  const raw = storage.getItem(ENDPOINT_KEY);
  if (raw === null) return undefined;
  let v: unknown;
  try {
    v = JSON.parse(raw);
  } catch {
    v = undefined; // unreadable: nothing remembered, and nothing worth keeping
  }
  const endpoint =
    v !== null && typeof v === 'object' && !Array.isArray(v) && typeof (v as { endpoint?: unknown }).endpoint === 'string'
      ? (v as { endpoint: string }).endpoint
      : undefined;
  if (endpoint === undefined) {
    storage.removeItem(ENDPOINT_KEY);
    return undefined;
  }
  if (Object.keys(v as object).some((k) => k !== 'endpoint')) persistEndpoint(storage, { endpoint });
  return endpoint;
}

/**
 * Watch a credential's expiry: `warn` {@link EXPIRY_WARNING_MS} before it (at
 * once, if that moment has passed), then `expired` at it. There is no
 * refresh token, so the warning is the person's chance to sign in again
 * before the connection drops. A credential with no expiry is not watched.
 * `stop` cancels both; `done` settles when the watch ends either way.
 */
export function expiryWatch(
  c: Credential,
  on: { warn?: () => void; expired: () => void },
  clock: Clock = systemClock,
): { stop: () => void; done: Promise<void> } {
  const ac = new AbortController();
  const stop = (): void => ac.abort();
  const at = c.expiresAt;
  if (at === undefined) return { stop, done: Promise.resolve() };
  // A stop rejects the pending sleep; that ends the watch quietly. An error
  // thrown by a callback is not swallowed: it rejects `done`.
  const wait = (ms: number): Promise<boolean> =>
    ms > 0 ? clock.sleep(ms, ac.signal).then(() => !ac.signal.aborted, () => false) : Promise.resolve(!ac.signal.aborted);
  const done = (async () => {
    if (!(await wait(at - EXPIRY_WARNING_MS - clock.now()))) return;
    on.warn?.();
    if (!(await wait(at - clock.now()))) return;
    on.expired();
  })();
  return { stop, done };
}
