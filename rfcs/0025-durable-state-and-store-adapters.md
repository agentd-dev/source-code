# RFC 0025: Durable state & store adapters

**Status:** Implemented (agentd 2.0 track, phases P2–P4)
**Author:** Andrii Tsok (drafted with Claude)
**Date:** 2026-08-16
**Part of:** the durable-agent design (`docs/design/01-durable-agent-plan.md` §3.4–3.5, §4.1, §4.4, §5); supersedes the checkpointer profile of RFC 0021 §8 as the store contract (that profile becomes the default `mcp` mapping) and the "lifetime budget" note formerly cited as "RFC 0025" (now RFC 0026 §7 / RFC 0030).

---

## 1. Summary

agentd is a **durable agent**: every unit of progress — an accepted A2A
message, a fired trigger, a workflow step, an intelligence turn, a subagent
result, a memory write, an artifact, a timer, a task transition — is recorded
in a **remote state store** and can be **restored** after the process dies. The
store is reached through a four-operation **adapter contract**
(`put/get/list/delete`) that maps onto **any MCP server's tools** (argument and
result mapping in JSON templates or CEL) or onto **plain HTTP**. agentd links no
database client and defines no schema beyond the envelope in §3.

Guarantees: **exactly-once state transitions** (a `seq`-CAS per key),
**at-least-once effects** (every effect carries an idempotency key), and a
documented **restore protocol** (§6). "Accept means durable": an A2A message or
a trigger firing is written before it is acted on (§5).

## 2. Store contract

```
put(key, seq, envelope)  → Ok | Conflict{latest_seq} | Err(io)   // seq MUST be > the stored seq (CAS); first write: seq 1
get(key[, seq])          → Some(envelope) | None | Err(io)         // latest, or the pinned seq if the store keeps history
list(prefix)             → [{key, seq}] | Unsupported | Err(io)    // OPTIONAL
delete(key)              → Ok | Unsupported | Err(io)              // OPTIONAL
```

- `Conflict` means another writer owns the key (a second instance on the same
  namespace, or a stale replica) — **fatal for the writer** (the run/instance
  stops accepting work and reports `store.conflict`; never retried).
- `Unsupported` for `list`/`delete` is honoured by index records and
  tombstones (§3.3); a store that lacks both still works.
- Every operation is bounded by the management timeout class (RFC 0016 §10)
  with a bounded retry on `Err(io)`; `Conflict` and any 4xx-class response are
  final.

## 3. Keys and envelopes

### 3.1 Keys

`<prefix>/<instance>/<kind>/<id>` — `prefix` from `store.prefix` (default
`agentd`), `instance` from `agent.name` (or the downward-API identity, RFC 0015
§6), `kind` from the table below, `id` per kind. Keys are opaque to the store;
`list(prefix)` is used only for discovery.

### 3.2 Envelope

```json
{ "v": 2, "kind": "run", "id": "01J…", "seq": 17, "ts": 1723800000000,
  "instance": "agentd-0", "hash": "sha256-hex (definition binding, where applicable)",
  "state": { … kind-specific, see RFC 0026/0027/0029 … } }
```

`v` is the envelope major (2); an unknown major is refused at restore. `hash`
binds a `run` to its workflow definition (RFC 0027 §9) and a `context` to its
compaction lineage.

### 3.3 Entity kinds

| kind | id | written when | notes |
|---|---|---|---|
| `manifest` | `agent` | on entity add/remove; budget settle; start-node state change (debounced) | index `{kind,id,seq}` of live entities, generation, start-node state, budget counters, lifecycle |
| `inbox` | ULID | on receipt of an A2A message / trigger firing / signal — **before** the event is acted on or acknowledged | `status: pending → done`; done records are deleted (or tombstoned) |
| `context` | `root` or an A2A `contextId` | after every turn, compaction, plan/skills change | RFC 0026 §5 |
| `run` | run id (ULID) | after every completed step / batch, at suspension, at terminal | RFC 0027 §9 |
| `subagent` | handle | spawn, status change, result | RFC 0026 §6 |
| `task` | A2A task id | every task transition | RFC 0029 §4 |
| `memory` | key | set/delete | value + `{ts, ttl?, by}`; index record `memory/_index` when `list` is unsupported |
| `artifact` | artifact id | create/delete | metadata + inline content or chunk refs (`artifact/<id>/<n>`) |
| `timer` | timer id | arm/disarm | absolute deadline, owner, payload |
| `audit` | ULID | when `observability.audit.sink` includes `store` | append-only |

Tombstones: `delete` unsupported ⇒ `put` an envelope with `state: null` and
`kind` unchanged; readers treat it as absent.

## 4. Adapters

### 4.1 `mcp`

```yaml
store:
  kind: mcp
  prefix: agentd
  mcp:
    server: state                       # a declared MCP server (RFC 0030 §mcp)
    put:    { tool: state.put,    args: '{"key": "{key}", "seq": {seq}, "state": {envelope}}',
              ok: 'result.structuredContent.ok', conflict: 'result.structuredContent.latest' }
    get:    { tool: state.get,    args: '{"key": "{key}"}', value: 'result.structuredContent.state' }
    list:   { tool: state.list,   args: '{"prefix": "{prefix}"}', keys: 'result.structuredContent.keys' }   # optional
    delete: { tool: state.delete, args: '{"key": "{key}"}' }                                              # optional
```

- **Canonical inputs** available to templates: `key`, `seq`, `prefix`,
  `instance`, `envelope` (the JSON envelope), `kind`, `id`.
- **Templates**: `{name}` interpolation (a JSON literal is substituted where the
  template is a bare `{envelope}`/`{seq}`; a string context substitutes the
  string form); or `CEL: <expr>` (feature `cel`) evaluating to the argument
  object.
- **Result extraction**: `value`/`ok`/`conflict`/`keys` are JSON pointers or
  `CEL:` expressions over `result` (the `CallToolResult`, `structuredContent`
  preferred, text-JSON fallback). `ok` truthy ⇒ success; `conflict` present
  and truthy ⇒ `Conflict{latest_seq}`; `isError: true` ⇒ `Err`.
- **Default mapping** = the RFC 0021 §8.3 checkpointer profile
  (`state.put {key, seq, state}` returning `{ok, latest?}`, `state.get {key,
  seq?}` returning `{state}`, `state.list {key|prefix}` returning `{seqs|keys}`)
  — a server that advertises those tools needs no mapping.
- Idempotency: every `put` carries `_meta["agent/idempotency_key"] =
  "<key>#<seq>"` and `_meta["agent/instance"]`.

### 4.2 `http`

```yaml
store:
  kind: http
  http:
    base_url: https://state.internal/v1
    headers: { authorization: "Bearer {{secret:STATE_TOKEN}}" }
    get:    { method: GET,    url: "{base_url}/kv/{key}",              value: 'body' }
    put:    { method: PUT,    url: "{base_url}/kv/{key}?seq={seq}",    body: '{envelope}', conflict_status: 409 }
    list:   { method: GET,    url: "{base_url}/kv?prefix={prefix}",    keys: 'body.keys' }
    delete: { method: DELETE, url: "{base_url}/kv/{key}" }
```

- HTTPS required (loopback `http://` for dev), the RFC 0012 egress classifier
  applies; headers may carry secret refs; the idempotency key is sent as
  `Idempotency-Key`; `conflict_status` maps to `Conflict`; `404` on `get` maps
  to `None`; other 4xx/5xx to `Err`.

### 4.3 `memory` (tests/dev) and `none`

`memory` is the in-process store used by unit tests and `--validate-config`
dry runs; `none` refuses to start when any durable feature is configured
(runs, A2A intake) — dev-only.

## 5. Write-ahead of events

An inbound event (`A2aMessage`, `TriggerFired`, `Signal`) is `put` to `inbox`
**before** the loop acts on it; an A2A `SendMessage` is acknowledged only after
that write (`store.durability.a2a: strict`, the default). Processing marks the
record `done`. On restore, `pending` inbox records are re-delivered in `ts`
order with their original ids.

`store.durability.steps: eventual { debounce_ms }` lets high-frequency step
progress inside a batch coalesce (the batch's own records still bound the
replay). `store.on_error: halt` (default) stops intake and drains when the
store fails persistently; `degrade` keeps working with `durability: degraded`
surfaced in status/metrics.

## 6. Restore protocol

1. `get manifest` — none ⇒ fresh instance (write generation 1).
2. For every indexed entity: `get` latest; verify `v` and `hash`; an entity
   newer than the manifest is authoritative (entity-first write order); a
   listed-but-missing entity is `lost` (logged, audited, counted).
3. Rebuild the run registry, contexts, subagent registry, tasks; re-arm timers
   from absolute deadlines (fire immediately if past); re-arm MCP
   subscriptions for pending waits/`subscribe` start nodes; re-open `working`
   tasks; re-spawn subagents whose parent step is pending (`attempt+1`);
   re-deliver `pending` inbox events.
4. Emit `restore.done {entities, runs_resumed, events_replayed, lost}`; bump
   the generation; write the manifest.

## 7. Effects and idempotency

A **step** = `put(running, attempt n)` → perform → `put(done|failed)`. A crash
between the two writes replays the effect (at-least-once). Every effect carries
an idempotency key `(instance, run, step, attempt)` (or `(ctx, turn, call)`)
surfaced to MCP servers as `_meta["agent/idempotency_key"]` and to HTTP as
`Idempotency-Key`. Per step: `on_replay: retry | skip | fail` (RFC 0027 §7).

## 8. Observability

Metrics `agentd_store_ops_total{op,result}`, `agentd_store_latency_seconds{op}`,
`agentd_inbox_pending`, `agentd_restore_total{result}`; events `store.put.fail`,
`store.conflict`, `store.degraded`, `restore.begin/done`; `agent://store`
(kind, prefix, durability, last error, generation).

## 9. Security

Keys are instance-namespaced; the store credential is a secret ref; envelopes
never contain resolved secrets (payloads are the same secret-free shapes RFC
0009 already enforces); artifacts may be marked `sensitive` and are then
excluded from `agent://` reads.

## 10. Test plan

Unit: adapters against the `memory` store and mock MCP/HTTP servers (mapping,
conflict, unsupported ops, tombstones); envelope round-trips. E2E: SIGKILL
between `put(running)` and `put(done)` → restart → replay with the same
idempotency key; inbox re-delivery; restore with a lost entity; conflict
injection.
