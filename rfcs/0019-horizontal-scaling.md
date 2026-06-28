# RFC 0019: Horizontal scaling — work-claim leases, sharding, and autoscaling signals

**Status:** Proposed (agentctl control-plane track)
**Author:** Andrii Tsok
**Date:** 2026-06-27
**Part of:** the agent rewrite — control-plane track (RFC 0014); extends execution modes & reactive routing (RFC 0008)

---

## 1. Problem / Context

RFC 0008 made one agent instance a correct reactive worker: every inbound
`notifications/resources/updated{uri}` matches **exactly one** route, in-order,
no fan-out, at-least-once with convergence on current state. That guarantee is
**intra-instance**. It says nothing about a *second* replica subscribed to the
*same* MCP source.

Horizontal scaling only buys anything if work can be **split across replicas**.
Run two `reactive` Deployments subscribed to `file:///inbox/*.json` and, by
RFC 0008's own rule, *each* instance is an exactly-one-owner — so the same
`updated{uri}` is processed **twice**, once per replica. RFC 0008's
exactly-one-owner is a property *within* a process; replicate the process and
you replicate the owner. KEDA scaling a reactive worker fleet from 1→10 pods
without an inter-instance ownership rule produces 10× duplicate side effects,
not 10× throughput.

This RFC extends the exactly-one-owner guarantee **across instances**. It owns
four mechanisms, in dependency order:

1. **Work-claim / lease** — a convention (not a server we run) by which N
   replicas subscribed to one source cooperatively elect a single processor per
   item, reusing MCP. Claim-before-process, ack/release after; a dead claimer's
   lease expires and another replica retries.
2. **Sharding** (`--shard K/N`) — static partitioning of the URI/key space so a
   fleet need not contend on every item; an instance ignores items outside its
   shard before the claim even happens.
3. **Autoscaling signals** — the metric set agentctl/KEDA scales the fleet on.
   agent *exposes* the signals; the scaler logic is agentctl's.
4. **Warm pool / standby** — an optional mode where an instance holds
   intelligence + MCP connections open, idle, ready to be *assigned* work,
   cutting cold-start.

It also pins **scale-down safety**: a held claim and in-flight work must survive
an HPA scale-down, which means the drain choreography (RFC 0011) must release
claims, and statelessness (RFC 0011 §7) must make any replica fungible.

**This RFC owns** the claim/lease convention agent participates in, the shard
hash + assignment contract, the autoscaling-signal *surface* (names, semantics,
where they live), and the standby mode. It does **not** own: the queue/lease
*server* (that is an external MCP backing service or the source server itself);
the KEDA `ScaledObject` / HPA / `StatefulSet` / operator reconcile (agentctl,
RFC 0014 §6); the metrics *schema freeze* (RFC 0016); the routing rule it
extends (RFC 0008); the exit-code/signal/idempotency contract it leans on
(RFC 0011); the self-MCP surface it profiles (RFC 0005).

**Minimalism moat (non-negotiable, RFC 0014 §3.3).** Every surface here is
feature-gated and **dependency-free**. The default build stays `serde` +
`serde_json` + `libc`; the cloud-native image set stays dep-free. Claim/lease is
**MCP tool-calls over the transport agent already speaks** (RFC 0004) — no
queue client, no Redis, no Raft, no consensus library, no async runtime. Shard
hashing is a hand-rolled FNV-1a, no `hashbrown`/`siphash`/`sha2` crate.
Autoscaling signals are the **already-existing** metric surface (RFC 0010 /
RFC 0016) read by an *external* scraper — agent grows no KEDA client. A feature
that would pull a Kubernetes client, a TLS/gRPC stack, or an async executor is
wrong by construction and belongs in agentctl.

**Primitives, not policy (RFC 0014 §3).** agent exposes: a claim convention it
honours, a shard predicate it applies, signals it emits, an assignment endpoint
it serves. agentctl owns: *which* shard each pod gets, *when* to scale, *how* to
rebalance, *which* warm pod gets the next job. We reuse MCP; we invent no new
protocol.

---

## 2. Decision

1. **Cross-instance ownership is achieved by a work-claim lease, reusing MCP —
   never a bespoke queue agent runs.** Before a reactive worker processes a
   routed item, it **claims** it by calling a `work.claim` tool (or a conventional
   `_meta` lease) on the source/coordination MCP server; it processes only on a
   *granted* claim, then **acks** (`work.ack`) on success or **releases**
   (`work.release`) on a clean wind-down. A claim carries a **lease TTL**; if the
   claimer dies mid-process, the lease expires and the item is redelivered to
   another replica. This makes RFC 0008's exactly-one-owner hold **across**
   instances by turning "owner of the route" into "holder of the lease."

2. **Claiming is opt-in per route via `disposition: claim`** (an extension of the
   RFC 0008 spawn/continue axis), gated by the `cluster` feature. A route with no
   claim convention behaves exactly as RFC 0008 (single-instance ownership). The
   coordination server's claim tools are discovered from its MCP catalogue; agent
   participates, it does not provide them. The dedupe of record lives in the
   backing service (RFC 0011 §6.4), keyed by the **claim key**, which is by
   default the **`AGENT_RUN_ID`** (RFC 0011 §6) the worker would have used anyway.

3. **`--shard K/N` statically partitions the key space.** An instance with shard
   `K` of `N` handles an item only if `fnv1a64(shard_key(item)) % N == K`. The
   shard predicate is applied **at routing intake, before claim and before
   spawn** — out-of-shard items are dropped at near-zero cost (counter
   `agent_shard_skipped_total`). The hash is **stable across versions and
   languages** (FNV-1a/64, hand-rolled). `shard_key` defaults to the resource
   URI; a route may override it. Sharding **composes with claim**: shard narrows
   *which* items a replica considers, claim resolves *contention* among replicas
   that share a shard (N=1, or transient overlap during rebalance).

4. **Rebalancing on an `N` change is drain-and-reassign, never live-migrate.**
   agent holds shard identity as immutable config (RFC 0011 §4.1, no live
   reload). agentctl changes the fleet by **rolling the StatefulSet**: each pod's
   `--shard K/N` comes from its stable ordinal identity; an `N` change is a
   rolling restart where every pod drains (releasing held claims, decrementing
   `inflight`) before its replacement comes up with the new `K/N`. No item is
   owned by two live shards at once because the old pod *released* before the new
   pod *claimed*; the lease TTL covers the seam.

5. **agent exposes autoscaling signals; agentctl/KEDA owns the scaler.** The
   signal set — `agent_reactive_backlog`, `agent_active_subagents`,
   `agent_saturation`, `agent_tokens_per_sec`, `agent_intelligence_latency_ms`
   — is part of the **frozen metrics schema (RFC 0016)**, surfaced on the existing
   metrics endpoint (RFC 0010) and mirrored as `agent://metrics` /
   `agent://capacity` resources (RFC 0005). The intended scaler behaviour (scale
   on backlog, respect drain on scale-down, avoid thrash) is **documented here as
   intent** but **implemented in agentctl** — agent ships no HPA logic.

6. **Standby (`--standby`) is a reactive worker that holds connections warm and
   waits to be *assigned*, distinct from reactive-idle.** A reactive-idle worker
   is subscribed and waiting for *its* events; a standby worker is subscribed to
   an **assignment channel** and waiting to be *handed* a unit of work (a directed
   subscribe or a claim grant). Standby trades a small steady-state cost (open
   intelligence + MCP connections, `agent_ready=1`) for near-zero cold-start when
   the scaler or a directed assignment lights it up.

7. **Scale-down must not drop a held claim or in-flight work.** SIGTERM drain
   (RFC 0011 §4) is extended with a **claim-release step**: on `DRAINING`, before
   winding down subagents, the worker **releases every held, un-acked claim** so
   another replica re-claims immediately (rather than waiting out the lease TTL).
   A clean drain still exits **0** (RFC 0011 §5). Because the supervisor is
   stateless (RFC 0011 §7), any replica is fungible and a re-claimed item runs
   identically anywhere.

These decisions are final for the v1 of this contract. Everything is behind the
`cluster` feature; a default build reports `"cluster": false` in its capabilities
manifest (RFC 0014 §5) and behaves exactly as RFC 0008.

---

## 3. Mechanism — work-claim / lease

### 3.1 The problem this convention solves, stated precisely

RFC 0008 §2.2: every `updated{uri}` matches exactly one route. With R replicas,
the source server delivers (via `resources/subscribe`) the *same* notification to
*all R* connections — each replica's router independently routes it to its
own owning route and processes it. There is no inter-process channel in RFC 0008,
so R replicas = R processings. The claim convention inserts a **single
serializing point** — the coordination server — between "I routed this" and "I
process this," so exactly one replica wins.

We do **not** invent a queue. We reuse MCP `tools/call` (RFC 0004 codec) against
whichever server owns the work items (the source server itself, or a thin
coordination MCP server the operator runs). agent is a *participant* in a claim
protocol, identically to how RFC 0011 §6 makes it a participant in an idempotency
protocol — the mechanism is in the backing service; agent supplies the key and
honours the lifecycle.

### 3.2 The claim lifecycle

```
        routed item (RFC 0008 first-match)         shard-passed (§4)
                       │
                       ▼
   ┌──────────┐  work.claim{key,ttl}   ┌─────────────────────────────┐
   │  CLAIMED │◄──────────────────────►│  coordination MCP server     │
   └────┬─────┘   granted=true          │  (source server OR a thin    │
        │                               │   claim/lease server)        │
        │ process (spawn/continue, RFC 0008 §3.3)                      │
        │   ├─ heartbeat: work.renew{key} every ttl/3 if long-running  │
        ▼                               │                              │
   ┌──────────┐  work.ack{key,run_id}   │                              │
   │  DONE     ├──────────────────────► │  side effect deduped on key  │
   └──────────┘                         └─────────────────────────────┘
        ▲  on clean drain / non-terminal wind-down:
        └─ work.release{key}  ──────────►  item immediately re-claimable

   on claimer death (crash / SIGKILL / node loss):
        lease not renewed → TTL elapses on server → item re-offered to fleet
```

Lifecycle states (worker-side, held in the route's in-flight slot — no durable
local state, RFC 0011 §7):

| State | Entered when | Left when |
|---|---|---|
| `OFFERED` | routed + in-shard | `work.claim` returns |
| `CLAIMED` | `work.claim{granted:true}` | ack / release / lease expiry |
| `LOST` | `work.claim{granted:false}` (another replica won) | dropped (counter) |
| `DONE` | `work.ack` accepted | — |
| `RELEASED` | `work.release` (drain or wind-down) | re-claimable by fleet |

### 3.3 The claim tools (discovered, not defined by agent)

agent calls these on the coordination server; their *schemas* are the server's,
discovered via `tools/list` (RFC 0004). The **names and argument convention**
below are what agentctl freezes (RFC 0014 §3.4) so a fleet and its claim server
agree. agent uses them if present; a route declaring `claim` against a server
that does not advertise them fails validation at startup (exit 2, RFC 0011 §3.3).

```jsonc
// work.claim — atomically lease an item. The server's job to make this atomic.
{ "jsonrpc":"2.0","id":7,"method":"tools/call","params":{
    "name":"work.claim",
    "arguments":{ "item":"file:///inbox/42.json", "ttl_ms":30000 },
    "_meta":{
      "agent/claim_key":"01J8Z…",        // = RUN_ID (RFC 0011 §6); the dedupe key
      "agent/instance":"pod-abc",          // identity (RFC 0014 §5 downward API)
      "agent/shard":"3/8",                 // for server-side observability only
      "traceparent":"00-…-01"               // trace-context (RFC 0010)
    }}}
// → result: { "granted": true, "lease_id":"L-991", "expires_in_ms":30000 }
//        or  { "granted": false, "held_by":"pod-xyz" }   // another replica owns it

// work.renew — extend the lease for a long-running process (heartbeat).
{ "name":"work.renew", "arguments":{ "lease_id":"L-991", "ttl_ms":30000 } }

// work.ack — work done; the side effect, keyed on agent/claim_key, is committed.
{ "name":"work.ack", "arguments":{ "lease_id":"L-991" },
  "_meta":{ "agent/claim_key":"01J8Z…" } }

// work.release — relinquish without completing (drain / wind-down); re-claimable now.
{ "name":"work.release", "arguments":{ "lease_id":"L-991", "reason":"draining" } }
```

If the source server itself models items as **resources with a lease field**
(rather than offering claim *tools*), agent supports the **resource-lease
variant**: `resource.read` the item, observe a `lease` field, attempt a
compare-and-set write (`work.claim` degenerates to a conditional `tools/call`
the server exposes). The lifecycle is identical; only the wire shape differs.
Which variant a server uses is declared per route (`claim.style=tool|resource`).

### 3.4 Where the claim sits in one reactive wake (extends RFC 0008 §3.7)

```
reactor.recv_timeout(…)                                   // RFC 0008 §3.7
 └─ event: McpNotification(server, Updated{uri})
     ├─ route = first_match(routes, uri)                  // RFC 0008 §2.2 exactly-one-owner (intra-instance)
     │    └─ None: unrouted++; drop.
     ├─ SHARD GATE (§4): if !in_shard(route, uri) { shard_skipped++; drop; return }   ◄── NEW
     ├─ route.queue.push(coalesce); arm debounce_timer    // RFC 0008 §3.4
     └─ debounce expiry → deliver(route):
          CLAIM GATE (§3.2): work.claim{key=run_id, ttl}  ◄── NEW (disposition: claim)
            ├─ granted=false → claim_lost++; drop (another replica owns it). DONE.
            └─ granted=true  → proceed:
                 Spawn:    spawn_root_from_event(item)     // RFC 0008 §3.3
                 Continue: re_enter_session(item)
 └─ inner loop runs (RFC 0007); on terminal:
        completed → work.ack{lease}                        ◄── NEW
        non-terminal wind-down / drain → work.release{lease}
        (long-running → work.renew every ttl/3 from the supervisor timer)
```

The shard gate (cheap, pure-CPU) precedes the claim gate (a network round-trip)
deliberately: most replicas reject most items locally and never pay for a claim.

### 3.5 At-least-once + idempotency interaction (the load-bearing seam)

The claim convention is **at-least-once, not exactly-once** — by construction,
and consistent with RFC 0008 §2.6. The lease TTL means a claimer that dies after
the side effect but before `work.ack` will have its item **redelivered**, and a
second replica will re-process it. Correctness therefore rests on the **same
idempotency mechanism RFC 0011 §6 already mandates**:

- The **claim key is the `RUN_ID`** (RFC 0011 §6.1). agent injects
  `agent/run_id` (== `agent/claim_key`) into the `_meta` of *every* downstream
  `tools/call` the worker makes while processing the item (RFC 0011 §6.2). So the
  durable side effect is deduped on the **same key** whether the work is done by
  the first claimer or a post-expiry second claimer.
- For redelivery to map to the *same* key, the claim key MUST be **derived from
  the item, not minted per-process** — otherwise two claimers of the redelivered
  item use two keys and the side effect duplicates. Therefore for `claim` routes
  the RUN_ID default changes from "per-process random ULID" (RFC 0011 §6.1) to
  **`run_id = derive(item_key)`** — a stable function of the claimed item
  (default: a ULID seeded from a FNV-1a of the item URI + the route id). The
  operator may override with an explicit stable key (e.g. the item's own id).
  **This is the one place this RFC narrows RFC 0011's default**, and only for
  `claim` routes; everywhere else RUN_ID semantics are unchanged.
- `work.ack` carrying `agent/claim_key` lets the server **collapse the
  ack** too: a redelivered-but-already-acked item is a server-side no-op, so the
  "already done" path (RFC 0011 §6.3) is cheap — the second claimer's
  `resource.read` finds the work done and the loop exits `completed`→`0` without
  burning an LLM turn.

> The honest scope statement (mirroring RFC 0011 §6.4): agent guarantees (1) a
> single serializing claim before processing, (2) a stable, item-derived key
> propagated into every side effect, (3) lease release on clean wind-down and
> lease expiry on death, and (4) no local non-idempotent side effect. End-to-end
> exactly-once is the **backing service's** dedupe on the key — agent supplies
> the key and the lifecycle, never the storage.

### 3.6 Config surface (claim slice)

Precedence built-in < file < env < flag (RFC 0011 §3.1); validated at startup
before any side effect (RFC 0011 §3.3).

| Knob | Flag / env | Default | Notes |
|---|---|---|---|
| Enable claim on a route | `--route …=>claim:server` | off | route disposition extends RFC 0008 §3.2 |
| Claim style | `claim.style=tool\|resource` | `tool` | §3.3 |
| Lease TTL | `--claim-ttl` / `AGENT_CLAIM_TTL` | `30s` | server is the authority; this is the *requested* TTL |
| Renew fraction | `AGENT_CLAIM_RENEW_FRACTION` | `0.33` | heartbeat at `ttl * fraction` |
| Claim key source | `claim.key=run_id\|item\|<expr>` | `run_id`(=item-derived, §3.5) | stable per item |
| Coordination server | `--route …=>claim:SERVER` | — | a `--mcp` server name; MUST advertise `work.*` |

Validation (exit 2, RFC 0011 §3.3) rejects: a `claim` route whose server is not a
declared `--mcp` server; a server missing `work.claim`/`work.ack` in its
`tools/list` (checked post-handshake, before the first wake — a `connect`-time
not a `load`-time check, so it maps to **exit 6 EXIT_MCP** if the server is
simply down, exit 2 if the server is up but lacks the tools); a `claim` route
combined with `overflow=block` (claiming and source-backpressure are mutually
exclusive ownership models).

---

## 4. Mechanism — sharding (`--shard K/N`)

### 4.1 The predicate

```rust
// cluster/shard.rs   [feature = "cluster"]
pub struct Shard { pub k: u32, pub n: u32 }   // --shard K/N, 0 <= K < N, N >= 1

/// Stable across versions, languages, and architectures. Hand-rolled FNV-1a/64.
/// No hashbrown/siphash/sha2 crate — the default hasher is randomized and the
/// crates are deps; a shard hash MUST be deterministic fleet-wide.
fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in bytes { h ^= b as u64; h = h.wrapping_mul(0x0000_0100_0000_01B3); }
    h
}

impl Shard {
    /// Applied at routing intake, before claim, before spawn (RFC 0008 §3.7).
    pub fn owns(&self, shard_key: &str) -> bool {
        self.n == 1 || (fnv1a64(shard_key.as_bytes()) % self.n as u64) == self.k as u64
    }
}
```

- **`shard_key` default = the resource URI** (the same string RFC 0008 routes
  on). A route may override: `--route …,shard_key=<expr>` where `<expr>` extracts
  a stable item key (e.g. a path component) so that *related* items co-locate on
  one shard (useful for `continue` routes whose session must see all updates for
  one entity). `continue` routes that override `shard_key` to the entity id keep
  a warm session on exactly one shard — a precondition for stateful sharding.
- **Timer events** (RFC 0008 §3.6) carry no URI; a sharded `schedule`/`loop`
  fleet shards the *timer route* on a configured `shard_key` (e.g. the tick's
  target id) or, with none, only **shard 0** fires (a single fleet-wide ticker)
  — the default, to avoid N replicas all firing the same cron tick.
- **`--shard` is absent ⇒ `N=1, K=0`**: a single logical shard, `owns()` always
  true. A non-sharded fleet relies purely on claim (§3) for cross-instance
  ownership. Sharding is the *cheap pre-filter*; claim is the *correctness
  backstop*. A fleet may use either or both:

| Shard | Claim | Cross-instance ownership | Cost |
|---|---|---|---|
| none (`N=1`) | yes | claim serializes all replicas on every item | 1 claim RT / item |
| `K/N` | no | each item deterministically owned by one shard; **no claim** | 0 RT, but **no redelivery on pod death until rebalance** |
| `K/N` | yes | shard pre-filters; claim resolves intra-shard + rebalance overlap | 1 claim RT / in-shard item |

The middle row (shard-only) is the cheapest and is correct **as long as a dead
shard's items wait for the pod to come back** (StatefulSet reschedules the same
ordinal) — acceptable for level-triggered, current-state work (RFC 0008 §3.5)
where a delayed reprocess is harmless. The bottom row adds redelivery-on-death
at one round-trip per item and is the recommended default for work that must make
progress despite a node loss.

### 4.2 Assignment: agentctl owns K, the StatefulSet owns identity

agent does **not** discover its own shard. agentctl assigns it, leveraging
**StatefulSet stable ordinal identity** (`pod-0 … pod-(N-1)`): the pod's ordinal
*is* `K`, the replica count *is* `N`, both injected as config (downward API or
env) the same way `terminationGracePeriodSeconds` is (RFC 0011 §3.3).

```jsonc
// what agentctl injects (env, resolved by RFC 0011 §3.1 precedence); agent only reads it
{ "AGENT_SHARD": "3/8" }   // from: K = ordinal(pod-3), N = .spec.replicas
```

agent's role is to **read `K/N`, validate `0 <= K < N`, apply `owns()`**, and
report `"shard":"3/8"` in its capabilities manifest (RFC 0014 §5) and on every
claim `_meta` (§3.3) for server-side observability. The mapping ordinal→K and the
StatefulSet are entirely agentctl's (RFC 0014 §6 non-goals).

### 4.3 Rebalancing on N change — drain + reassign (Decision 4)

A shard count change (`N: 8 → 12`) re-partitions the key space: items that hashed
to shard 3-of-8 may now belong to shard 5-of-12. agent holds shard identity as
**immutable config** (RFC 0011 §4.1 — no live reload, restart-to-reconfigure), so
a rebalance is a **rolling restart driven by agentctl**, not an in-process
migration:

```
agentctl: kubectl scale / patch .spec.replicas 8 → 12   (the StatefulSet)
  for each pod, rolling:
    SIGTERM pod-k  ──► drain (RFC 0011 §4 + §6 below):
                        1. DRAINING: stop routing new items (RFC 0008 §3.4 disarm)
                        2. work.release every held claim (§6)        ◄── frees in-flight items
                        3. wind down subagents at turn boundaries
                        4. exit 0
    start pod-k with AGENT_SHARD="k/12"  ──► new owns() predicate
```

The seam is covered three ways: (1) the draining pod **releases** its claims
before exit, so they are immediately re-claimable by whichever shard now owns
them; (2) the lease TTL covers any claim not cleanly released (e.g. a forced
kill); (3) **no item is owned by two *live* shards** because the old pod's
`owns()` stops routing (step 1) before the new pod with the new `N` starts. A
brief window where an item is owned by *neither* live shard (old drained, new not
yet ready) is harmless: it is level-triggered, and reconnect reconcile (RFC 0008
§3.5, read-after-subscribe) re-synthesizes a `Synthetic("possibly changed")`
event so the now-owning shard picks it up. Rolling one-pod-at-a-time keeps the
neither-owner window to one pod's restart, not the whole fleet's.

**Sharding composes with claim during rebalance.** With claim enabled (§4.1
bottom row), even a transient two-owner overlap during a fast rebalance is
resolved by the claim: only one of the overlapping replicas wins `work.claim`.
Claim is what makes rebalancing safe under imperfect rollout timing.

### 4.4 Config surface (shard slice)

| Knob | Flag / env | Default | Notes |
|---|---|---|---|
| Shard identity | `--shard K/N` / `AGENT_SHARD` | `0/1` (single shard) | `0 <= K < N`; validated exit 2 |
| Shard key (per route) | `--route …,shard_key=<expr>` | resource URI | stable item key; co-locates related items |
| Timer shard behaviour | `AGENT_SHARD_TIMER` | `shard0` | `shard0` (one ticker) \| `keyed` (shard on target) |

Validation (exit 2): `K >= N`; `N == 0`; a `continue` route sharded on the URI
rather than a stable entity key when the session must aggregate multiple URIs
(warned, not fatal — it is a footgun, not always wrong).

---

## 5. Mechanism — autoscaling signals

### 5.1 The signal set (names frozen in RFC 0016; surfaced here)

agent **exposes** these; the scaler **consumes** them. They are part of the
**frozen metrics schema (RFC 0016)** — this RFC names them and pins their
*semantics for scaling*, but RFC 0016 owns the schema, `# HELP`/`# TYPE`, and the
`metrics_schema` version in the capabilities manifest (RFC 0014 §5). They are
served on the existing `/metrics` surface (RFC 0010 §3.8 — gated `metrics`
feature, hand-written Prometheus text, no SDK) and mirrored as the
`agent://metrics` and `agent://capacity` resources (RFC 0005 §3.3) for
vsock-only fleets with no cluster network (RFC 0014 §2).

| Signal | Type | Meaning | Scaler intent |
|---|---|---|---|
| `agent_reactive_backlog` | gauge | distinct queued+unclaimed items this replica sees (sum of route `queue` depths + offered-not-yet-claimed) | **primary scale-up trigger**: backlog > 0 sustained ⇒ add replicas |
| `agent_active_subagents` | gauge | live subagents (= RFC 0010 `agent_active_subagents`) | concurrency headroom; near `max` ⇒ saturated |
| `agent_saturation` | gauge `[0.0,1.0]` | `in_flight / capacity` where capacity = `min(max_inflight·routes, max_total_subagents)` | scale up as it approaches 1.0; the HPA "utilization" target |
| `agent_tokens_per_sec` | gauge | rolling tokens/s across the tree (derived from RFC 0010 `agent_tokens_total`) | intelligence-bound load; informs *model-aware* placement & cost ceilings |
| `agent_intelligence_latency_ms` | gauge | p50/p95 intel call latency (from RFC 0010 `gen_ai.client.operation.duration`) | rising latency ⇒ upstream saturation; **scale the model service, not necessarily agent** |
| `agent_claims_lost_total` | counter | claims lost to another replica (contention) | high & rising under low backlog ⇒ **over-provisioned**, scale *down* |
| `agent_shard_skipped_total` | counter | items dropped as out-of-shard | sanity: confirms shard partitioning is live |

`agent_reactive_backlog` is new-to-this-RFC in intent but mechanically a sum
over the existing per-route bounded queues (RFC 0008 §3.4) plus the count of
`OFFERED`-not-`CLAIMED` items — it costs nothing extra to expose. Cardinality
discipline (RFC 0010 §3.8) holds: **no `run_id`/URI/shard-id in metric labels** —
shard identity rides `_meta` and the manifest, never a metric label, so a 1000-pod
fleet does not explode label cardinality.

### 5.2 Intended scaler behaviour (intent — implemented in agentctl)

Documented so agentctl/KEDA authors target the right thing; **none of this is
agent code**.

- **Scale up on backlog, not on CPU.** A reactive worker is intelligence-bound
  and idles at near-zero CPU between events (RFC 0008 §3.1.3) — CPU-based HPA is
  blind to a deep backlog of pending LLM work. The KEDA trigger is
  `agent_reactive_backlog` (a `prometheus` scaler) or `agent_saturation` as an
  HPA `External`/`Object` metric with a target of ~0.7.
- **Respect drain on scale-down (Decision 7).** KEDA/HPA scale-down deletes a
  pod via SIGTERM → the pod drains, **releasing claims** (§6) so no item is lost.
  The HPA's `terminationGracePeriodSeconds` MUST exceed `AGENT_DRAIN_TIMEOUT`
  (the RFC 0011 §3.3 invariant, re-asserted: a scale-down that SIGKILLs before
  drain completes leaks a held claim until its TTL expires — correct but slow).
- **Avoid thrash.** Backlog is bursty (RFC 0008 debounce/coalesce). agentctl
  configures KEDA `cooldownPeriod` / HPA `stabilizationWindowSeconds` so a
  coalesced burst does not scale 1→20→1. agent's debounce already smooths the
  *signal*; the hysteresis lives in the scaler.
- **Scale-down preference: lame-duck the least-loaded.** agentctl picks the
  victim (lowest `agent_active_subagents`, drained via the `lame-duck` /
  `drain` management tools, RFC 0014 §4 / RFC 0015). agent exposes per-pod load;
  the *choice* is agentctl's. `agent_claims_lost_total` rising under low backlog
  is the over-provisioning signal that should trigger scale-*down*.

agent's whole contribution to autoscaling is **emitting honest signals and
draining cleanly**. The control loop is agentctl's (RFC 0014 §6 non-goals: "HPA/KEDA
scalers" are explicitly agentctl's).

---

## 6. Mechanism — scale-down safety (drain extension)

This extends RFC 0011 §4.2's drain state machine with **one inserted step** — it
does not redefine the choreography, which RFC 0011 owns.

On `DRAINING` (RFC 0011 §4.2), the reactor runs, **after step 1 (disarm triggers
/ flip not-ready) and before step 2 (wind down subagents)**:

> **Step 1.5 — release held claims.** For every route in-flight slot in state
> `CLAIMED` whose work has **not** reached a terminal status, call
> `work.release{lease_id, reason:"draining"}` on its coordination server. This
> hands the item back to the fleet **immediately**, so a surviving replica
> re-claims it without waiting out the lease TTL. Claims whose work *has* reached
> a terminal status proceed to `work.ack` in the normal wind-down (step 2).
> Releases are best-effort with a hard sub-budget (`min(2s, drain_timeout/4)`):
> if the coordination server is unreachable, we **do not block drain** — the lease
> TTL is the backstop (the item redelivers when the lease expires). A failed
> release is logged (`drain.claim_release_failed`) and counted; it is never fatal.

```
RUNNING ──SIGTERM──► DRAINING:
   1.   disarm triggers; stop routing new items; not-ready (RFC 0008 §3.4; RFC 0010)
   1.5. work.release every CLAIMED-but-not-terminal item   ◄── NEW (this RFC)
   2.   wind down in-flight subagents at turn boundaries (RFC 0011 §4.2)
        └─ each that reaches `completed` → work.ack; else → work.release
   3.   ladder stragglers (RFC 0003)
   4.   flush logs; exit 0 (clean drain, RFC 0011 §5)        ◄── 0, not 143
```

**Statelessness makes any replica fungible (RFC 0011 §7).** A released claim
carries no agent-local state — the item lives on the coordination server, the
side-effect dedupe key is item-derived (§3.5), and the supervisor holds nothing
durable a SIGKILL could corrupt (RFC 0011 §4.4). So a re-claimed item runs
*identically* on any replica; there is no "sticky" worker. This is what lets the
HPA treat the fleet as interchangeable cattle.

The **second SIGTERM → force** path (RFC 0011 §4.3) skips step 1.5 (it is the
operator's "stop now" escape hatch); held leases then expire by TTL. Force-drain
trades immediacy for a one-TTL redelivery delay — acceptable, because force-drain
is already the ungraceful path (exit 143).

---

## 7. Mechanism — warm pool / standby

### 7.1 Standby vs reactive-idle (Decision 6)

| | **reactive-idle** (RFC 0008 §3.1.3) | **standby** (`--standby`, this RFC) |
|---|---|---|
| Subscriptions | its declared `--subscribe` URIs | an **assignment channel** + (lazily) the work it is assigned |
| Intelligence conn | established on first event (cold-ish) | **held open** from start (warm) |
| MCP conns | declared servers, connected | declared servers, connected |
| Readiness | `agent_ready=1` (RFC 0010) | `agent_ready=1`, **and** `agent://capacity` advertises free slots |
| Wakes on | `updated{uri}` for *its* routes | an **assignment** (directed subscribe or a claim grant) |
| Cost when idle | near-zero CPU, open conns | near-zero CPU, open conns + held intel session |
| Cold-start on work | one intel handshake | **none** — already warm |

A standby worker is a reactive worker whose event source is an **assignment
channel** rather than (or in addition to) a content subscription. It exists to
absorb a scale-*up* with no cold-start: the pool is pre-warmed, and the scaler
(or a directed assignment) lights up a member by *handing it work*.

### 7.2 How work is assigned to a standby member (reuse MCP — no new protocol)

Two mechanisms, both reusing the existing surface; an operator picks one:

1. **Claim-pull (preferred, symmetric with §3).** Standby members subscribe to a
   single shared **assignment resource** on the coordination server (e.g.
   `work://pending`). On its `updated`, every standby member races `work.claim`
   (§3.2); exactly one wins and processes; the rest go back to standby. This is
   the claim convention with the pool as the contender set — **no new code**, just
   a route whose disposition is `claim` and whose source is the assignment
   channel. The lease still covers a winner that dies.

2. **Directed-assign (push, via the management surface).** agentctl, having
   chosen a specific warm member, calls the **`subagent.spawn`** self-tool
   (RFC 0005 §3.2) or a `assign` management tool (RFC 0015) over vsock/unix to
   hand that member a specific unit of work directly. This is a *directed
   subscribe*: agentctl issues, on the member, the equivalent of RFC 0008's
   self-subscribe (§3.6) bound to the assigned item, so the member re-enters a
   warm session on it. The choice of *which* member is **agentctl's policy** (RFC
   0014 §3); agent only exposes the spawn/assign primitive and `agent://capacity`
   so agentctl can see who is free.

```jsonc
// agent://capacity  (RFC 0005 resource) — what agentctl reads to place work
{ "instance":"pod-abc", "shard":"3/8", "standby":true,
  "free_slots":4, "active_subagents":0, "intelligence":{"warm":true,"healthy":true},
  "max_total_subagents":64, "saturation":0.0 }
```

### 7.3 Config surface (standby slice)

| Knob | Flag / env | Default | Notes |
|---|---|---|---|
| Standby mode | `--standby` / `AGENT_STANDBY` | off | reactive worker, warm, assignment-driven |
| Assignment channel | `--assign-from server:uri` | — | the shared pending resource (claim-pull) |
| Warm intelligence | `AGENT_WARM_INTEL` | `on` (when `--standby`) | keep the intel session open while idle |

Standby is **mode-orthogonal**: it is `--mode reactive` with `--standby` and an
`--assign-from`. It reports `"standby": true` in its capabilities manifest
(RFC 0014 §5). It is **not** session checkpointing (resuming a *prior* run after
reschedule) — that remains the deferred RFC 0013 line; standby holds *no
prior work*, only *open connections*, so a standby pod is as stateless and
fungible as any other (§6).

---

## 8. Edge cases & failure semantics

| # | Situation | Behaviour |
|---|---|---|
| 1 | **Two replicas claim the same item simultaneously** | The coordination server's `work.claim` is the single serializing point; it grants to exactly one (`granted:true`), the other gets `granted:false` → `claims_lost_total++`, drops. agent assumes the server makes claim atomic; if it cannot, two-owner is possible and only idempotency (§3.5) saves correctness. |
| 2 | **Claimer dies after side effect, before `work.ack`** | Lease not renewed → TTL expires → item redelivered → second claimer re-processes → side effect deduped on the **item-derived key** (§3.5) → "already done" → `completed`→exit 0 cheaply. At-least-once + idempotent. |
| 3 | **Claimer dies after `work.claim`, before any side effect** | Lease expires → redelivered → clean reprocess. No duplication. |
| 4 | **Coordination server is down at startup** | `tools/list` handshake fails → exit **6 EXIT_MCP** (RFC 0011 §5, retriable — sidecar may be racing up). A `claim` route requires its server like any required MCP server. |
| 5 | **Coordination server is up but lacks `work.*` tools** | Startup validation (post-handshake) fails → exit **2 EXIT_USAGE** (operator wired a non-claim server to a claim route). Non-retriable. |
| 6 | **Coordination server dies mid-run** | In-flight claims continue locally; renew/ack/release calls fail → logged, counted (`agent_mcp_*`); the daemon keeps serving other routes (RFC 0008: a failed reaction never kills the daemon). If the server is a *required* server and stays down, the reactive exit class **6** fires (RFC 0008 §3.1.3). Held leases expire server-side on its recovery. |
| 7 | **Lease expires while work is still in-flight** (slow LLM, missed renew) | The item is redelivered to the fleet; **two replicas may now process concurrently** (the original + the re-claimer). Both write under the **same item-derived key** → the backing service dedupes → one effect. The straggler's eventual `work.ack` is a no-op (already-acked). Set `--claim-ttl` > realistic processing time; the renew heartbeat (`ttl/3`) is the primary guard. |
| 8 | **`N` change mid-burst (rebalance)** | Rolling restart (§4.3): each pod drains+releases before its replacement starts. Brief neither-owner window per pod is recovered by read-after-subscribe (RFC 0008 §3.5). Claim (if enabled) resolves any transient two-owner overlap. |
| 9 | **Item hashes to a shard whose pod is down (shard-only, no claim)** | The item waits until the StatefulSet reschedules that ordinal (same `K`). On restart, read-after-subscribe re-synthesizes the event (RFC 0008 §3.5). Acceptable for level-triggered work; for progress-under-node-loss, enable claim (§4.1 bottom row). |
| 10 | **Scale-down SIGKILLs before drain (grace < drain_timeout)** | Held leases are **not** released → they expire by TTL → items redeliver after one TTL. Correct but delayed. The RFC 0011 §3.3 grace > drain_timeout invariant prevents this; agent warns at startup if it can prove the coupling is wrong. |
| 11 | **Standby member assigned work, then dies before claiming** | Claim-pull: nothing was claimed → no loss, another member wins. Directed-assign: the directed subscribe produced no claim → the assigner observes no `agent://capacity` change / no ack and re-assigns (agentctl policy). |
| 12 | **All replicas in a shard are over budget (tree token ceiling)** | Each stops spawning and drains warm sessions (RFC 0008 §3.1, the ultimate backpressure), releasing claims (§6). Backlog rises → `agent_reactive_backlog` climbs → the scaler adds replicas (which have fresh tree budgets). Budget exhaustion becomes a *scale signal*, not a meltdown. |
| 13 | **`work.release` itself fails during drain** | Best-effort, sub-budgeted (§6); logged `drain.claim_release_failed`; never blocks drain; lease TTL is the backstop. Drain still exits 0. |
| 14 | **Two replicas with the same `--shard K/N` (mis-assignment)** | Both `owns()` the same items → duplicate processing unless claim is enabled. This is an agentctl mis-assignment (two pods, same ordinal — should be impossible with a StatefulSet); claim makes it merely wasteful, not incorrect. agent surfaces `shard` in `_meta`/manifest so agentctl can detect the collision. |

**Invariant preserved across all rows:** correctness never depends on
exactly-once delivery. It depends on (a) a single serializing claim *or* a single
owning shard, **and** (b) an item-derived idempotency key on every side effect.
When (a) momentarily fails (TTL expiry, rebalance seam, mis-assignment), (b)
holds the line. This is RFC 0008 §2.6's "convergence on current state, not
exactly-once" extended across instances.

---

## 9. Capabilities manifest additions (RFC 0014 §5)

A `cluster`-feature build advertises, in the manifest agentctl reads first:

```jsonc
{
  "build_features": ["metrics","serve-mcp","cluster","vsock"],
  "surfaces": {
    "cluster": true,                       // this RFC's surface is present
    "claim": { "styles": ["tool","resource"] },
    "shard": "3/8",                        // current shard identity (null if N=1)
    "standby": false,
    "metrics_schema": "1.0"                // RFC 0016 — carries the autoscaling-signal set
  }
}
```

agentctl uses this to: place a `claim` route only on instances reporting
`claim`; assemble a `StatefulSet` whose ordinals feed `AGENT_SHARD`; target the
right autoscaling metrics by `metrics_schema` version; and route directed-assign
only to instances reporting `standby:true`. An instance with `"cluster":false`
degrades to RFC 0008 single-instance behaviour and agentctl runs it as a singleton
(no scale-out) — graceful degradation per RFC 0014 §7.

---

## 10. Non-goals / Deferred

- **No queue, no broker, no consensus in agent.** Claim/lease is a *convention*
  over MCP `tools/call`; the atomic claim and the lease store live in an external
  coordination MCP server (the source server or a thin one the operator runs).
  agent ships no Redis/Raft/etcd client and no leader election (those are
  agentctl/operator concerns, RFC 0014 §6).
- **No exactly-once.** At-least-once + item-derived idempotency only (§3.5,
  RFC 0008 §2.6). A use case needing true exactly-once needs a transactional
  backing service; agent supplies the key, not the transaction.
- **No live shard migration / consistent-hashing ring.** Rebalance is a rolling
  restart driven by agentctl (§4.3); agent's shard identity is immutable config
  (RFC 0011 §4.1). A minimal-disruption consistent-hash assignment is an
  *agentctl* placement choice, not an agent mechanism.
- **No in-process autoscaler.** agent emits signals; HPA/KEDA logic, scale
  decisions, cooldowns, and victim selection are agentctl's (RFC 0014 §6, §5.2).
- **No KEDA/Kubernetes client, no gRPC/TLS stack, no async runtime.** All four
  mechanisms are `cluster`-feature-gated and built from the existing dep-free
  surface (MCP client RFC 0004, metrics RFC 0010, self-MCP RFC 0005, FNV-1a
  hand-rolled). Anything heavier belongs in agentctl (RFC 0014 §3.3).
- **No warm-session checkpoint/restore.** Standby holds *open connections*, never
  *prior work*; resuming a warm session/run across reschedule remains the deferred
  "MCP-backed session checkpointing" line (RFC 0013 / RFC 0014 §4). Standby pods
  stay stateless and fungible (§6).
- **No reactivity over HTTP in v1.** Claim/lease and standby ride the same
  stdio/unix/vsock transports as RFC 0008's stdio-only reactivity; Streamable-HTTP
  serving is deferred (RFC 0013).

---

## 11. Interactions with other RFCs

- **RFC 0005 (self-MCP & control protocol):** standby directed-assign reuses
  `subagent.spawn`/`subagent.send` and the `agent://capacity`/`agent://metrics`
  resources; the claim convention is just `tools/call` against a peer server over
  the same transports.
- **RFC 0008 (modes & reactive routing):** this RFC extends exactly-one-owner
  from intra- to inter-instance. `disposition: claim` extends the
  spawn/continue axis (§3.2); the shard gate inserts into the §3.7 wake; the
  read-after-subscribe reconcile (§3.5) covers rebalance seams.
- **RFC 0010 (observability/health):** the autoscaling signals are this surface;
  cardinality discipline (no URI/shard-id labels) is inherited, not re-stated.
- **RFC 0011 (cloud-native contract):** drain (§4.2) gains step 1.5 (claim
  release); RUN_ID (§6) becomes the item-derived claim key for `claim` routes;
  exit codes (2/6) classify claim-server failures; statelessness (§7) is what
  makes a re-claimed item fungible. This RFC extends, never restates, that
  contract.
- **RFC 0014 (control-plane umbrella):** this RFC is sub-RFC 0019; its surfaces
  are reported in the §5 manifest and consumed by agentctl's StatefulSet
  assignment, KEDA scalers, and directed placement.
- **RFC 0015 (management surface):** `drain`/`lame-duck` are how agentctl picks
  and drains a scale-down victim; an `assign` management tool (if added there) is
  the directed-assign push path (§7.2).
- **RFC 0016 (telemetry & lifecycle contract):** owns the *frozen schema* for the
  autoscaling-signal metric set this RFC names (§5); the `metrics_schema` version
  agentctl negotiates on lives there.

---

## 12. Open items (for the umbrella author to reconcile)

- **`work.*` tool names are a frozen contract — confirm ownership.** ✅ **Resolved:
  frozen in RFC 0015 §5.6.** §3.3 defines `work.claim`/`work.renew`/`work.ack`/
  `work.release` and the `agent/claim_key` `_meta` convention; the umbrella ratified
  RFC 0015 §5.6 as the single authority for the names + `_meta` keys + style variants
  (RFC 0015 serves the management surface agentctl negotiates on, even though agent
  *calls* `work.*` rather than serving them). The schemas remain the server's,
  discovered via `tools/list`.
- **RUN_ID default narrowing for `claim` routes.** ✅ **Resolved: recorded in
  RFC 0015 §5.6** as an additive, route-scoped extension to RFC 0011 §6.1 (not a
  conflict) — §3.5's item-derived key applies to `claim` routes only; non-claim
  RUN_ID semantics are unchanged.
- **Backlog-signal definition belongs to RFC 0016.** §5.1 defines
  `agent_reactive_backlog` operationally (queued + offered-not-claimed). RFC 0016
  owns the frozen schema; the exact derivation and `# HELP` text should land
  there, with this RFC as the consumer/justification. Confirm RFC 0016 will carry it.
- **Shard injection key name.** §4.2 assumes agentctl injects `AGENT_SHARD="K/N"`
  from the StatefulSet ordinal, paralleling RFC 0011 §3.3's
  `AGENT_POD_GRACE_SECONDS` downward-API convention. The exact env name is a
  documentation convention to settle with RFC 0015/agentctl; not a design gap.
- **Directed-assign tool surface.** §7.2 mechanism 2 references an `assign`
  management tool that may or may not exist in RFC 0015. If RFC 0015 does not add
  it, directed-assign falls back to `subagent.spawn` over the management transport;
  confirm which is canonical.

---

## 13. References

- **RFC 0004** — MCP client subset & wire codec: the `tools/call` codec the
  claim convention rides; no bespoke queue protocol.
- **RFC 0005** — self-MCP server & control protocol: `subagent.spawn`/`send`, the
  `agent://capacity`/`agent://metrics` resources used for placement and signals.
- **RFC 0007** — agentic loop & terminal-status: §3.4 terminal-status vocabulary
  (`completed` → `work.ack`/exit 0; non-terminal → `work.release`).
- **RFC 0008** — execution modes & reactive routing: the exactly-one-owner rule
  (§2.2), spawn/continue axis, debounce/coalesce, read-after-subscribe reconcile —
  this RFC extends all of it across instances.
- **RFC 0010** — observability, health & telemetry: the metric surface and
  cardinality discipline the autoscaling signals live in.
- **RFC 0011** — cloud-native contract: config precedence, drain choreography
  (extended in §6), the exit-code table (§5), RUN_ID idempotency (the claim key),
  statelessness (fungible replicas).
- **RFC 0013** — deferred v2 surface: MCP-backed session checkpointing (what
  standby is *not*); HTTP serving.
- **RFC 0014** — control-plane (agentctl) contract: the umbrella; primitives-not-
  policy, the capabilities manifest (§5), the data-plane/control-plane split.
- **RFC 0015** — management & control surface: `drain`/`lame-duck` for scale-down
  victim selection; the candidate home for the frozen `work.*` tool schema.
- **RFC 0016** — telemetry & lifecycle contract: the **frozen** metrics schema
  that owns the autoscaling-signal set this RFC names.
