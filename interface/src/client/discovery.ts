// SPDX-License-Identifier: AGPL-3.0-only
/**
 * Finding the agent: the Agent Card at the well-known URI (A2A 1.0 §8.2), the
 * interface to speak to (§8.3.2), and what the card says the agent can do.
 *
 * The configured value — `--endpoint`, the web UI's Connect field — names
 * where the CARD is, not where JSON-RPC goes. The client reads the card and
 * calls the first JSON-RPC 1.0 interface it lists, with that interface's
 * tenant. There is no fallback to the configured URL: an agent that
 * advertises nothing this client speaks is refused, loudly, rather than
 * probed until something answers.
 *
 * The card is public: it is fetched with no credential and no custom header,
 * and kept in memory for as long as its `Cache-Control: max-age` allows, then
 * revalidated with its ETag.
 *
 * Everything a card says is someone else's text. Whatever of it reaches an
 * error or a warning goes through {@link inert} first, because the TUI
 * prints those to a terminal.
 */

import {
  ClientError,
  Endpoint,
  EXTENDED_CARD_NOT_CONFIGURED,
  inert,
  Json,
  METHOD_NOT_FOUND,
  RpcError,
  UNSUPPORTED_OPERATION,
} from './types.js';
import { A2A_VERSION } from './wire.js';
import { A2aClient } from './a2a.js';
import {
  CLIENT_EXTENSIONS,
  COMMAND_EXTENSION,
  EVENTS_EXTENSION,
  INTROSPECTION_OPS,
  TASK_ANNOTATIONS_EXTENSION,
  UNIX_BINDING,
} from './ext.js';
import { Credential, isLoopbackHost, LoginOption, loginOptions } from './auth.js';
import { parseRetryAfter } from './errors.js';

/** An Agent Card, as ProtoJSON. Only the fields read here are interpreted. */
export type AgentCard = { [k: string]: Json };

/** The well-known path of the card (RFC 8615, A2A 1.0 §8.2). */
export const CARD_PATH = '/.well-known/agent-card.json';

/** A card is a small document; more than this is not one. */
const MAX_CARD = 1 << 20;

type Obj = { [k: string]: Json };

function obj(v: Json | undefined): Obj | undefined {
  return v !== null && v !== undefined && typeof v === 'object' && !Array.isArray(v) ? v : undefined;
}

function str(v: Json | undefined): string | undefined {
  return typeof v === 'string' && v.length > 0 ? v : undefined;
}

// ---- where the card is -----------------------------------------------------

/**
 * The card URL for a configured value: the value itself when it already
 * names the card, else the well-known path at its origin. A base URL with a
 * path still finds the card at the origin root — that is where RFC 8615 puts
 * it, and where agentd serves it.
 */
export function cardUrlOf(configured: string): string {
  let u: URL;
  try {
    u = new URL(configured);
  } catch {
    throw new ClientError('discovery', `${JSON.stringify(configured)} is not a URL`);
  }
  // A credential in the URL would ride every request as userinfo, and fetch
  // refuses such a URL with an error that quotes it — onto the terminal and
  // into any log that keeps errors. Refused here, without echoing it.
  if (u.username !== '' || u.password !== '') {
    throw new ClientError('discovery', `the endpoint URL carries credentials (${u.protocol}//…@${u.host}); use --bearer-file or sign in instead`);
  }
  if (u.pathname.endsWith(CARD_PATH)) {
    u.hash = '';
    return u.href;
  }
  return new URL(CARD_PATH, u).href;
}

// ---- reading it, with its cache --------------------------------------------

/** One cached card. */
interface CardEntry {
  card: AgentCard;
  etag?: string;
  /** Epoch ms until which the card is used without asking again. */
  freshUntil: number;
}

/**
 * The in-memory card cache, keyed by card URL. The clock is injectable so a
 * test can step past `max-age` without waiting for it.
 */
export class CardCache {
  private readonly entries = new Map<string, CardEntry>();
  readonly now: () => number;
  constructor(now: () => number = () => Date.now()) {
    this.now = now;
  }
  get(url: string): CardEntry | undefined {
    return this.entries.get(url);
  }
  set(url: string, e: CardEntry): void {
    this.entries.set(url, e);
  }
  delete(url: string): void {
    this.entries.delete(url);
  }
  clear(): void {
    this.entries.clear();
  }
}

/** The process-wide cache every discovery uses unless it is handed another. */
export const cardCache = new CardCache();

/**
 * How long a response may be used, in seconds, from its `Cache-Control`:
 * `undefined` for `no-store` (never keep it), else 0 for `no-cache`, else
 * `max-age`, else 0 — a card without a lifetime is still kept for its ETag,
 * but revalidated every time.
 *
 * Every directive is read before deciding: `no-cache, no-store` is a common
 * spelling, and stopping at `no-cache` would keep what `no-store` forbids.
 */
function maxAgeOf(h: string | null): number | undefined {
  let age = 0;
  let noCache = false;
  let noStore = false;
  for (const raw of (h ?? '').split(',')) {
    const d = raw.trim().toLowerCase();
    if (d === 'no-store') noStore = true;
    else if (d === 'no-cache') noCache = true;
    const m = /^max-age\s*=\s*"?(\d+)"?$/.exec(d);
    if (m) age = Number(m[1]);
  }
  if (noStore) return undefined;
  return noCache ? 0 : age;
}

/** Read at most {@link MAX_CARD} bytes; `undefined` when the body is larger. */
async function readCard(res: Response): Promise<string | undefined> {
  if (!res.body) return '';
  const reader = res.body.getReader();
  const decoder = new TextDecoder();
  let text = '';
  let size = 0;
  for (;;) {
    const { done, value } = await reader.read();
    if (done) break;
    size += value.byteLength;
    if (size > MAX_CARD) {
      await reader.cancel().catch(() => {});
      return undefined;
    }
    text += decoder.decode(value, { stream: true });
  }
  return text + decoder.decode();
}

export interface FetchCardOptions {
  signal?: AbortSignal;
  /** Defaults to {@link cardCache}. */
  cache?: CardCache;
}

/**
 * The card at `cardUrl`: from the cache while it is fresh, else a GET that
 * sends `If-None-Match` when the cached copy has an ETag, where a 304 keeps
 * the cached card for another `max-age`.
 *
 * The GET carries `accept` and nothing else — no credential, no A2A header.
 * That keeps it a CORS simple request (no preflight) and keeps a credential
 * from going anywhere before an interface has been chosen. A browser reading
 * a cross-origin card sees no ETag (not a CORS-safelisted response header),
 * so it never sends the non-safelisted `If-None-Match` either; it just asks
 * again once `max-age` runs out.
 *
 * A redirect is refused, not followed. Everything after this — which
 * interface is "same-origin" and so gets the person's credential, the tenant,
 * the login options — is judged against the URL the person named; a card
 * fetched from wherever that URL redirects would be judged as that origin's
 * own.
 */
export async function fetchCard(cardUrl: string, o: FetchCardOptions = {}): Promise<AgentCard> {
  const cache = o.cache ?? cardCache;
  const cached = cache.get(cardUrl);
  if (cached && cache.now() < cached.freshUntil) return cached.card;

  const headers: Record<string, string> = { accept: 'application/json' };
  if (cached?.etag) headers['if-none-match'] = cached.etag;
  const res = await fetch(cardUrl, {
    method: 'GET',
    headers,
    signal: o.signal,
    credentials: 'omit',
    // This cache decides; a browser's HTTP cache answering underneath it would
    // make the revalidation above unobservable.
    cache: 'no-store',
    redirect: 'manual',
  });
  // A browser reports a redirect it did not follow as an opaque response
  // (status 0, no Location); Node reports the 3xx itself.
  if (res.type === 'opaqueredirect' || (res.status >= 300 && res.status < 400 && res.status !== 304)) {
    await res.body?.cancel().catch(() => {});
    const to = res.headers.get('location');
    throw new ClientError(
      'discovery',
      `${cardUrl} redirects${to ? ` to ${inert(to)}` : ''}; point the client at the agent's own URL`,
    );
  }
  const age = maxAgeOf(res.headers.get('cache-control'));
  const etag = res.headers.get('etag') ?? undefined;

  if (res.status === 304) {
    await res.body?.cancel().catch(() => {});
    if (!cached) throw new ClientError('discovery', `${cardUrl} answered 304 to a request that was not conditional`);
    if (age === undefined) cache.delete(cardUrl);
    else cache.set(cardUrl, { card: cached.card, etag: etag ?? cached.etag, freshUntil: cache.now() + age * 1000 });
    return cached.card;
  }
  if (res.status === 429 || res.status >= 500) {
    // The agent is there and says "not now" — a card endpoint answers 503
    // while it cannot build the card. That is the server's passing state,
    // not a missing agent, so it carries its status (and Retry-After) and
    // `classify` sorts it as a wait instead of a reason to give up.
    await res.body?.cancel().catch(() => {});
    const extra: { status: number; retryAfterMs?: number } = { status: res.status };
    const wait = parseRetryAfter(res.headers.get('retry-after'));
    if (wait !== undefined) extra.retryAfterMs = wait;
    throw new RpcError(-res.status, `the agent card at ${cardUrl} is unavailable (HTTP ${res.status})`, extra);
  }
  if (res.status !== 200) {
    await res.body?.cancel().catch(() => {});
    throw new ClientError('discovery', `no agent card at ${cardUrl} (HTTP ${res.status})`);
  }
  const text = await readCard(res);
  if (text === undefined) throw new ClientError('discovery', `the agent card at ${cardUrl} exceeds ${MAX_CARD >> 20} MiB`);
  let card: AgentCard | undefined;
  try {
    card = obj(JSON.parse(text) as Json);
  } catch {
    /* reported below */
  }
  if (!card) throw new ClientError('discovery', `${cardUrl} is not an agent card (not a JSON object)`);
  if (age === undefined) cache.delete(cardUrl);
  else {
    const e: CardEntry = { card, freshUntil: cache.now() + age * 1000 };
    if (etag !== undefined) e.etag = etag;
    cache.set(cardUrl, e);
  }
  return card;
}

// ---- which interface --------------------------------------------------------

/** The interface every JSON-RPC call goes to. */
export interface SelectedInterface {
  url: string;
  tenant?: string;
  protocolVersion: string;
  /** The interface is on another origin than the card the person named. */
  crossOrigin: boolean;
}

/** Major.Minor of a protocol version, a missing minor read as 0. */
function majorMinor(v: string): string {
  const [major, minor = '0'] = v.split('.');
  return `${major}.${minor}`;
}

/** A host no client can dial: a wildcard bind leaked onto a card, or nothing. */
function isWildcardHost(host: string): boolean {
  const h = host.replace(/^\[/, '').replace(/\]$/, '');
  return h === '' || h === '0.0.0.0' || h === '::';
}

/**
 * How close to the person a host is, as far as its spelling says: 0 public,
 * 1 their network (RFC 1918, link-local, IPv6 unique-local and link-local),
 * 2 their machine (loopback, `localhost`, `*.localhost`).
 */
type Reach = 0 | 1 | 2;

/**
 * The {@link Reach} of `host`. An IPv4-mapped IPv6 address (which the URL
 * parser has already turned to hex) counts as this machine: it can map
 * loopback, and nothing a card needs is spelled that way. A NAME that
 * resolves to a private address is not caught — only a browser's own
 * private-network rules could see that — but a card that wants to aim a
 * client at the person's own machine has to spell it, and these are the
 * spellings.
 */
function reachOf(host: string): Reach {
  const h = host.replace(/^\[/, '').replace(/\]$/, '').toLowerCase();
  if (isLoopbackHost(h) || h.endsWith('.localhost') || h.startsWith('::ffff:')) return 2;
  const v4 = /^(\d{1,3})\.(\d{1,3})\.\d{1,3}\.\d{1,3}$/.exec(h);
  if (v4) {
    const a = Number(v4[1]);
    const b = Number(v4[2]);
    const net = a === 10 || (a === 172 && b >= 16 && b <= 31) || (a === 192 && b === 168) || (a === 169 && b === 254);
    return net ? 1 : 0;
  }
  return /^f[cd][0-9a-f]{0,2}:/.test(h) || /^fe[89ab][0-9a-f]?:/.test(h) ? 1 : 0;
}

/**
 * Pick the interface (A2A 1.0 §8.3.2): the first `supportedInterfaces` entry
 * whose binding is JSON-RPC and whose protocol version is this client's
 * Major.Minor, and whose URL is one `fetch` can reach. A unix-socket binding,
 * a `unix:` URL, a wildcard host and a URL carrying credentials are passed
 * over by name; when nothing is left the agent is refused with everything it
 * did offer.
 *
 * An interface on another origin than the card is followed only when
 *  - no credential of the person's is in play — a card is data from whoever
 *    answered the configured URL, and a token must never follow it somewhere
 *    the person did not name;
 *  - the hop cannot be read on the wire (https, or http on loopback);
 *  - and it does not lead closer in than the card came from: from a public
 *    card into the person's network or machine, or from their network onto
 *    their machine. Loopback is a privilege boundary as well as a private
 *    wire — a daemon there may admit any local process as its operator — so
 *    a remote card naming `http://127.0.0.1:…` would have this client drive
 *    the person's own agent while they believe they are talking to someone
 *    else's.
 * A cross-origin entry refused for one of these is passed over like any
 * other unusable entry; the refusal is thrown only when no later entry works.
 */
export function selectInterface(card: AgentCard, cardUrl: string, userCredential: boolean): SelectedInterface {
  const configured = new URL(cardUrl);
  const configuredReach = reachOf(configured.hostname);
  const name = inert(str(card.name) ?? cardUrl);
  const found: string[] = [];
  let crossOriginRefusal: string | undefined;
  const list = Array.isArray(card.supportedInterfaces) ? card.supportedInterfaces : [];
  for (const raw of list) {
    const i = obj(raw);
    if (!i) continue;
    const binding = str(i.protocolBinding) ?? '?';
    const version = str(i.protocolVersion) ?? '?';
    const url = str(i.url) ?? '';
    const label = inert(`${binding}@${version}`);
    if (binding === UNIX_BINDING) {
      found.push(`${label} (a unix socket, not reachable from this client)`);
      continue;
    }
    if (binding !== 'JSONRPC' || majorMinor(version) !== A2A_VERSION) {
      found.push(label);
      continue;
    }
    let u: URL;
    try {
      u = new URL(url);
    } catch {
      found.push(`${label} at ${inert(JSON.stringify(url))} (not an absolute URL)`);
      continue;
    }
    if (u.protocol !== 'http:' && u.protocol !== 'https:') {
      found.push(`${label} at ${inert(u.href)} (${inert(u.protocol)}// is not reachable from this client)`);
      continue;
    }
    if (u.username !== '' || u.password !== '') {
      found.push(`${label} at ${u.protocol}//…@${u.host} (carries credentials in the URL)`);
      continue;
    }
    if (isWildcardHost(u.hostname)) {
      found.push(`${label} at ${inert(u.href)} (a wildcard address, not a host)`);
      continue;
    }
    const crossOrigin = u.origin !== configured.origin;
    if (crossOrigin) {
      const at = `the card at ${cardUrl} points JSON-RPC at ${inert(u.href)}`;
      let refusal: string | undefined;
      if (userCredential) {
        refusal = `${at} (another origin); connect to ${inert(u.origin)} directly to trust it with your credential`;
      } else if (u.protocol !== 'https:' && !isLoopbackHost(u.hostname)) {
        refusal = `${at}: another origin, over plain http`;
      } else if (reachOf(u.hostname) > configuredReach) {
        refusal = `${at}: closer to this machine than the card itself is; connect to ${inert(u.origin)} directly if that is the agent you mean`;
      }
      if (refusal !== undefined) {
        crossOriginRefusal ??= refusal;
        found.push(`${label} at ${inert(u.href)} (refused: another origin)`);
        continue;
      }
    }
    const out: SelectedInterface = { url: u.href, protocolVersion: version, crossOrigin };
    const tenant = str(i.tenant);
    if (tenant !== undefined) out.tenant = tenant;
    return out;
  }
  if (crossOriginRefusal !== undefined) throw new ClientError('cross-origin', crossOriginRefusal);
  throw new ClientError(
    'no-interface',
    `${name} advertises no JSON-RPC A2A ${A2A_VERSION} interface (found: ${found.length > 0 ? found.join(', ') : 'none'})`,
  );
}

// ---- what the agent can do --------------------------------------------------

/** One `AgentExtension` declaration. */
interface Declared {
  uri: string;
  required: boolean;
  params: Obj;
}

function declarations(card: AgentCard | null): Declared[] {
  const list = obj(card?.capabilities)?.extensions;
  if (!Array.isArray(list)) return [];
  const out: Declared[] = [];
  for (const raw of list) {
    const d = obj(raw);
    const uri = str(d?.uri);
    if (!d || uri === undefined) continue;
    out.push({ uri, required: d.required === true, params: obj(d.params) ?? {} });
  }
  return out;
}

/**
 * The extensions the card marks `required` that this client will not speak:
 * those outside {@link CLIENT_EXTENSIONS}, or every required one when the
 * person turned extensions off. Non-empty means the client must stop.
 */
export function requiredUnsupported(card: AgentCard, noExtensions = false): string[] {
  return declarations(card)
    .filter((d) => d.required && (noExtensions || !CLIENT_EXTENSIONS.has(d.uri)))
    .map((d) => d.uri);
}

/** What the client may do with this agent, read once from its card(s). */
export interface Capabilities {
  /** The agent streams (SendStreamingMessage, SubscribeToTask). */
  streaming: boolean;
  /** These capabilities come from GetExtendedAgentCard, not the public card. */
  extendedCard: boolean;
  /** events/v1, when declared. */
  events: { ring?: number; kinds: string[] } | null;
  /**
   * command/v2, when declared. Read from the extended card, `ops` is what
   * THIS caller may run; from the public card it is the daemon's static
   * vocabulary — every op an agentd can serve, the same for every caller.
   */
  command: {
    ops: ReadonlySet<string>;
    /** Commands a workflow declares, with the workflow that answers each. */
    commands: { op: string; workflow: string; schema?: Json }[];
    /** The paths `admin.set` accepts from this caller. */
    settable: string[];
  } | null;
  /** task-annotations/v1 is declared. */
  annotations: boolean;
  /**
   * The introspection reads are offered to this caller: `true` or `false` when
   * the extended card said so, or when no command vocabulary is declared at
   * all; `null` when the cards cannot tell. The public vocabulary names the
   * reads whether or not this agent has introspection on, and an agent with
   * no listener auth serves no extended card — so `null` is the common case
   * on a loopback daemon, and the feed's `hello.introspection` is where the
   * answer is then read. `null` is not "off".
   */
  introspection: boolean | null;
  /** Workflows this caller may run (skills tagged `workflow` on the extended card). */
  workflows: string[];
  /** The ways in, from the public card's security declarations. */
  login: LoginOption[];
  /** No anonymous way in is declared. */
  authRequired: boolean;
  /** Declared extension URIs this client does not implement (not required ones). */
  ignored: string[];
}

function strings(v: Json | undefined): string[] {
  return Array.isArray(v) ? v.filter((x): x is string => typeof x === 'string') : [];
}

/**
 * Derive {@link Capabilities} from the public card and, when it was read, the
 * extended card — which describes the same agent for this caller, so its
 * declarations win. `noExtensions` (`--no-extensions`, `?extensions=off`)
 * leaves only core A2A.
 */
export function capabilitiesOf(pub: AgentCard, ext: AgentCard | null, o: { noExtensions?: boolean } = {}): Capabilities {
  const card = ext ?? pub;
  const decls = declarations(card);
  const find = (uri: string): Declared | undefined =>
    o.noExtensions ? undefined : decls.find((d) => d.uri === uri);

  let events: Capabilities['events'] = null;
  const ev = find(EVENTS_EXTENSION);
  if (ev) {
    events = { kinds: strings(ev.params.kinds) };
    if (typeof ev.params.ring === 'number') events.ring = ev.params.ring;
  }

  let command: Capabilities['command'] = null;
  const cmd = find(COMMAND_EXTENSION);
  if (cmd) {
    const ops = new Set<string>();
    for (const raw of Array.isArray(cmd.params.ops) ? cmd.params.ops : []) {
      // `[{op, reply}]`; a bare name is read the same way.
      const op = typeof raw === 'string' ? raw : str(obj(raw)?.op);
      if (op !== undefined) ops.add(op);
    }
    const commands: { op: string; workflow: string; schema?: Json }[] = [];
    for (const raw of Array.isArray(cmd.params.commands) ? cmd.params.commands : []) {
      const c = obj(raw);
      const op = str(c?.op);
      const workflow = str(c?.workflow);
      if (!c || op === undefined || workflow === undefined) continue;
      commands.push(c.schema !== undefined ? { op, workflow, schema: c.schema } : { op, workflow });
    }
    command = { ops, commands, settable: strings(cmd.params.settable) };
  }

  const workflows: string[] = [];
  if (ext) {
    for (const raw of Array.isArray(ext.skills) ? ext.skills : []) {
      const s = obj(raw);
      if (!s || !strings(s.tags).includes('workflow')) continue;
      const id = str(s.id);
      const name = id?.startsWith('workflow:') ? id.slice('workflow:'.length) : str(s.name);
      if (name !== undefined) workflows.push(name);
    }
  }

  const login = loginOptions(pub);
  return {
    streaming: obj(pub.capabilities)?.streaming === true,
    extendedCard: ext !== null,
    events,
    command,
    annotations: find(TASK_ANNOTATIONS_EXTENSION) !== undefined,
    introspection: command === null ? false : ext === null ? null : INTROSPECTION_OPS.some((op) => command.ops.has(op)),
    workflows,
    login,
    authRequired: !login.some((l) => l.method === 'none'),
    ignored: decls.filter((d) => !CLIENT_EXTENSIONS.has(d.uri)).map((d) => d.uri),
  };
}

// ---- a session --------------------------------------------------------------

/** Everything discovery settled, for the client that talks to the agent. */
export interface Session {
  cardUrl: string;
  card: AgentCard;
  /** The extended card, when it was read. */
  extended: AgentCard | null;
  /** The selected interface, its tenant, and the credential when it may go there. */
  ep: Endpoint;
  caps: Capabilities;
  /** Things the person should see, none of them fatal. */
  warnings: string[];
}

export interface OpenOptions {
  /** The person's credential for the configured endpoint. */
  credential?: Credential;
  noExtensions?: boolean;
  signal?: AbortSignal;
  cache?: CardCache;
}

/** The answers to GetExtendedAgentCard that mean "declared, but not served". */
const EXTENDED_CARD_UNSERVED: ReadonlySet<number> = new Set([
  EXTENDED_CARD_NOT_CONFIGURED,
  UNSUPPORTED_OPERATION,
  METHOD_NOT_FOUND,
]);

/** A URI without its trailing `/vN`: two versions of one extension share it. */
function unversioned(uri: string): string {
  return uri.replace(/\/v\d+$/, '');
}

/**
 * Discover the agent behind `configured` and settle how to talk to it: read
 * the card, refuse what cannot work (a required extension, no usable
 * interface, a credential that would cross origins), and read the extended
 * card when the agent offers one and a credential can ask for it.
 *
 * The extended card needs a credential the agent authenticates by a declared
 * scheme; without one it would only answer 401, so it is not asked. When it
 * is asked and answers 401 or 403, that is thrown — the credential is wrong,
 * and no amount of public card makes it right.
 */
export async function openSession(configured: string, o: OpenOptions = {}): Promise<Session> {
  const cardUrl = cardUrlOf(configured);
  const card = await fetchCard(cardUrl, { signal: o.signal, cache: o.cache });
  const name = inert(str(card.name) ?? cardUrl);
  const refuseRequired = (c: AgentCard): void => {
    const missing = requiredUnsupported(c, o.noExtensions);
    if (missing.length > 0) {
      throw new ClientError(
        'required-extension',
        `${name} requires extensions this client ${o.noExtensions ? 'was told not to use' : 'does not speak'}: ${missing.map(inert).join(', ')}`,
      );
    }
  };
  refuseRequired(card);
  const iface = selectInterface(card, cardUrl, o.credential !== undefined);
  const warnings: string[] = [];
  if (iface.crossOrigin) {
    warnings.push(`the card at ${cardUrl} points JSON-RPC at ${inert(iface.url)} (another origin); following it without a credential`);
  }
  const ep: Endpoint = { url: iface.url };
  if (iface.tenant !== undefined) ep.tenant = iface.tenant;
  // selectInterface refused a credential on another origin, so this one goes
  // only where the person pointed it.
  if (o.credential !== undefined) ep.bearer = o.credential.token;

  let extended: AgentCard | null = null;
  if (obj(card.capabilities)?.extendedAgentCard === true && ep.bearer !== undefined) {
    try {
      extended = obj(await new A2aClient(ep).getExtendedAgentCard({ signal: o.signal })) ?? null;
      if (extended === null) warnings.push('the extended agent card is not an object; using the public card');
    } catch (e) {
      // An agent that declares the extended card but does not serve it is
      // wrong, not unusable: the public card still describes it. A2A 1.0
      // names that case -32007; agentd answers -32004 when nothing on its
      // listener authenticates a caller, and an older server -32601.
      if (!(e instanceof RpcError) || !EXTENDED_CARD_UNSERVED.has(e.code)) throw e;
      warnings.push(`the agent declares an extended card but does not serve it (${inert(e.message)}); using the public card`);
    }
  }
  // The extended card describes the same agent for this caller and its
  // declarations win, so an extension it REQUIRES binds as much as one the
  // public card requires.
  if (extended !== null) refuseRequired(extended);

  const caps = capabilitiesOf(card, extended, { noExtensions: o.noExtensions });
  for (const uri of caps.ignored) {
    const ours = [...CLIENT_EXTENSIONS].find((c) => unversioned(c) === unversioned(uri));
    if (ours !== undefined) warnings.push(`agent offers ${inert(uri)}; this client speaks ${ours}`);
  }
  return { cardUrl, card, extended, ep, caps, warnings };
}
