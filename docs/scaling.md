# Horizontal scaling

A single `agentd` is one process running one agent. To handle more load you run
**more replicas of the same binary** — a fleet. But a reactive fleet has a
correctness problem the moment it has more than one member: every replica is
subscribed to the same MCP resources, so a single `file:///inbox/42.json`
`updated` notification fans out to **all** of them and each one spawns a
reaction. That is duplicate processing — N replicas doing the same work N times.

agentd solves this with two composable mechanisms, both behind the `cluster`
build feature:

1. **Sharding** (`--shard K/N`) — a cheap, deterministic pre-filter: each
   replica owns a disjoint slice of the URI space and drops everything else.
2. **Work-claim leases** (`--claim`) — a correctness backstop: before processing
   an item a replica claims it against a coordination server and proceeds only on
   a granted lease.

agentd itself does **not** scale the fleet — it only partitions work and emits
the signals a control plane (agentctl / KEDA / an HPA) scales on. Everything
below is verified against the shipped binary; features that are forward-compat
stubs are called out explicitly.

> **Build status.** Sharding, work-claim leases (tool-style), the autoscaling
> signal set, and the `agentd://capacity` surface all ship behind
> `--features cluster`. Standby mode is wired as an assignment-channel claim-pull
> (no warm-child pool yet — see §6). Resource-style claims are a documented stub
> (§3.4). All flags below are in the binary's `--help`; build without `cluster`
> and a `--shard N>1` / `--claim` / `--standby` directive is rejected at startup
> with exit `2` (never silently ignored).

---

## 1. The problem: duplicate processing

Two `reactive` replicas, each `--subscribe file:///inbox/`. A new file lands. The
MCP server notifies both. Both read it, both spawn an agent, both write the
result. Without coordination, work is duplicated and side effects double up.

The fix is to make exactly one replica own each item. agentd gives you two layers
that compose; you can run either alone or both together (§5).

```
        item:  file:///inbox/42.json   updated → fans to every replica
                       │
   ┌───────────────────┼───────────────────┐
   │  shard gate       │  shard gate        │  shard gate
   │  (cheap, local)   │                    │
   ▼                   ▼                    ▼
 replica 0 drops    replica 1 OWNS       replica 2 drops
                       │
                       ▼  claim gate (optional, authoritative)
                    work.claim → granted → spawn → work.ack
```

---

## 2. Sharding — the cheap pre-filter (`--shard K/N`)

A shard identity is `K/N`: this replica is shard `K` of `N` total. An item with
URI `uri` is owned by this replica iff

```
fnv1a64(uri) % N == K
```

The hash is a **hand-rolled FNV-1a/64** (offset basis `0xcbf29ce484222325`,
prime `0x00000100000001B3`) — deterministic fleet-wide, stable across versions,
languages, and architectures. The default hasher is randomized and the obvious
crates are dependencies, so agentd rolls its own; there is exactly one FNV in the
tree and the work-claim key derivation reuses it.

The gate runs at **reactive routing intake, before any debounce or spawn**, so
out-of-shard items are dropped at near-zero cost. The partition is **total and
disjoint**: every URI is owned by exactly one of the `N` shards — no duplicate,
no gap.

```bash
# replica 2 of a 4-shard fleet
agentd --instruction-file /etc/agentd/task.md \
       --intelligence unix:/run/intel.sock \
       --mode reactive \
       --subscribe 'file:///inbox/' \
       --shard 2/4
```

`--shard K/N` (or `AGENTD_SHARD=K/N`) is validated at startup: `N == 0`,
`K >= N`, and any malformed form exit `2`. The default is `0/1` — a single
logical shard that owns everything, byte-for-byte the unsharded behaviour. `N`
is immutable for the process's life: **restart to re-shard** (a hot reload
rejects a shard change — re-sharding mid-flight would move ownership of in-flight
items).

### 2.1 Who assigns K/N

agentd does **not** discover its own shard. The standard pattern is a Kubernetes
**StatefulSet**: each replica gets a stable ordinal (`agentd-0`, `agentd-1`, …),
and agentctl injects `AGENTD_SHARD=<ordinal>/<replicas>` from it. The binary only
reads, validates, and applies the value. See §7 for the sketch.

### 2.2 Timer routes in a sharded fleet (`AGENTD_SHARD_TIMER`)

Timer events (`--mode schedule` / `--mode loop`) carry no URI, so there is no key
to hash. `AGENTD_SHARD_TIMER` picks which replicas fire a tick:

| Value | Behaviour |
|---|---|
| `shard0` *(default)* | Only shard 0 fires — a single fleet-wide ticker, so `N` replicas don't all fire the same cron tick. |
| `keyed` | Every replica fires. The per-tick key gate (sharding on the tick's target) is a forward-compat knob — **not yet a live behaviour difference**, so today `keyed` means "every replica fires". |

A non-sharded instance (`N == 1`) always fires regardless of the mode.

Each dropped out-of-shard item increments the counter
**`agentd_shard_skipped_total`** (§4).

---

## 3. Work-claim leases — the correctness backstop (`--claim`)

Sharding alone is enough when the partition is clean and stable. But if you want
**cross-instance ownership** that survives a replica dying mid-item — at-least-once
delivery with redelivery — you add a work-claim lease.

agentd does **not** run a queue. It is the *participant* half of a coordination
convention: before processing an item, it calls `work.claim` on a **coordination
MCP server** (a declared `--mcp` server that advertises the `work.*` tools) and
proceeds only on a granted lease.

```bash
agentd --instruction-file /etc/agentd/task.md \
       --intelligence unix:/run/intel.sock \
       --mode reactive \
       --mcp coord='mcp-server-workqueue --addr /run/coord.sock' \
       --claim 'file:///inbox/'=coord
```

`--claim <uri>=<server>[:tool|resource]` is repeatable. The route's `uri` is
automatically added to the subscribe set (subscribed and routed as a spawn). The
`<server>` must be a declared `--mcp` server, validated at startup (exit `2`
otherwise). agentd *calls* the `work.*` tools — it never serves them.

### 3.1 The `work.*` convention

The four tool names are a **frozen contract**; the tools' *schemas* are the
coordination server's own (discovered via `tools/list`):

| Tool | When agentd calls it |
|---|---|
| `work.claim` | Before processing a routed item. Args `{item, ttl_ms}`. Returns `{granted:true, lease_id, expires_in_ms}` or `{granted:false, held_by}`. |
| `work.ack` | On a terminal `completed` run — the durable side effect is committed. |
| `work.release` | On a non-terminal wind-down or drain — the item becomes immediately re-claimable. |
| `work.renew` | Extend a held lease (used by continue-claims, §3.3). |

A coordination server is valid only if its `tools/list` advertises **both**
`work.claim` and `work.ack`. A server that is *up but missing* them is a wiring
mistake → exit `2`; a server that is *down* fails the MCP connect → exit `6`.

**No secret or URL ever rides in `_meta`.** The only `_meta` keys agentd emits on
a claim are `agentd/claim_key`, `agentd/instance`, `agentd/shard` (omitted when
unsharded), and `traceparent` (when present). The item URI is a `work.claim`
*argument*, never a `_meta` value.

### 3.2 The lease lifecycle (spawn-claim)

For a normal (spawn) claim route, each delivery is claimed and settled within one
iteration:

```
work.claim(item, ttl_ms)
  ├─ granted → spawn the reaction with the item-derived RUN_ID
  │             ├─ run completes  → work.ack(lease_id)
  │             └─ run non-terminal → work.release(lease_id, "wind-down")
  ├─ lost    → drop the delivery, increment agentd_claims_lost_total
  └─ error   → skip the delivery, keep serving (never crash the daemon)
```

On **drain** (`SIGTERM`), any still-held lease is `work.release`d so another
replica re-claims it promptly rather than waiting out the TTL.

### 3.3 Lease TTL and renewal

| Flag | Env | Default | Meaning |
|---|---|---|---|
| `--claim-ttl <dur>` | `AGENTD_CLAIM_TTL` | `30s` | Requested lease TTL. The **server is the authority** — this is the requested value; it returns the effective `expires_in_ms`. |
| `--claim-renew-fraction <F>` | `AGENTD_CLAIM_RENEW_FRACTION` | `0.33` | A long-held lease renews at `ttl × F`. Must be in `(0, 1)`. |

If a claimer dies, its lease expires server-side after the TTL and another
replica re-claims the item — this is what makes delivery **at-least-once** with
redelivery.

### 3.4 Spawn-claim vs continue-claim

A claim route's URI that is *also* a `--continue` URI becomes a **continue-claim**:
the lease is held across the warm session's whole life — claimed on the session's
first delivery, renewed by the heartbeat every `ttl × fraction` while the session
is live, and acked/released when the session ends or drains — instead of
claimed-then-settled per event. No new flag: it is the existing idiom of "a claim
route whose URI is also a `--continue` URI".

```bash
# continue-claim: one warm session per claimed channel, lease held for its life
agentd … --mode reactive \
         --mcp coord='mcp-server-workqueue --addr /run/coord.sock' \
         --continue 'file:///stream/in.json' \
         --claim 'file:///stream/in.json'=coord
```

### 3.5 At-least-once + idempotency (NOT exactly-once)

The claim convention is **at-least-once**, not exactly-once. A claimer can die
after committing a side effect but before `work.ack`; the lease expires and the
item is redelivered to another replica. agentd makes this safe with a
**deterministic, item-derived claim key**:

- The key is `derive_claim_key(item_uri, route_id)` — two FNV passes over
  `(item, route)`, a stable 32-hex string. The same `(item, route)` always maps
  to the same key, so the first claimer and a post-expiry second claimer write
  under the **same key**.
- The spawned reaction's **RUN_ID is set to this claim key**, so every downstream
  side-effect `tools/call` carries it in `_meta.agentd/run_id` — the dedupe key a
  backing service uses to collapse a retry (see [Configuration §8](configuration.md)).

So redelivery is correct *if your backing tools dedupe on the run-id key*. agentd
guarantees the stable key; the durable store must honour it.

### 3.6 `claim.style=resource` is a stub

`--claim <uri>=<server>:resource` (CAS / resource-lease style) is **not
implemented**. RFC 0015 froze the *direction* of a resource-style claim but not
the compare-and-set tool's name or argument shape, and a half-built CAS could
double-grant — the one thing a claim must never do. So a `resource`-style claim
returns a loud error (the delivery is skipped, the daemon keeps serving), and it
also fails startup validation because a pure resource-lease server need not
advertise `work.claim`/`work.ack`. **Use `:tool` (the default).** Resource-style
slots in unchanged behind the same lease lifecycle once the CAS contract is
frozen.

---

## 4. How shard + claim compose

Use the cheapest layer that meets your correctness need:

| Configuration | Ownership guarantee | Cost | Use when |
|---|---|---|---|
| **shard only** (`--shard K/N`) | Each item owned by exactly one shard, *as long as the partition holds*. No cross-instance recovery — a dead shard's items are not picked up until it restarts. | One FNV hash per item, fully local — no network round-trip. | A clean, stable partition is enough and you tolerate a brief gap while a replica restarts. |
| **claim only** (`--claim`) | At-least-once with redelivery: a dead claimer's items are re-claimed by any other replica after the TTL. | One `work.claim` round-trip per item to the coordination server. | You need recovery on replica death and don't have (or want) a stable shard partition. |
| **shard + claim** (both) | The shard pre-filter cuts each replica's claim traffic to its slice; the claim then provides recovery within that slice (and a clean handoff if a shard is reassigned). | FNV pre-filter **then** a claim round-trip only for in-shard items. | A large fleet that wants both cheap partitioning **and** death recovery — the recommended production shape. |

Composition is intake-ordered: the **shard gate runs first** (drop out-of-shard
items for free), then the **claim gate** runs for the items that survive (the
network round-trip only happens for items this replica might own).

---

## 5. Autoscaling signals

agentd emits the signals; a control plane scales on them. With `--features
metrics` these are Prometheus gauges/counters on `/metrics` (served on
`--metrics-addr`); without it they are derivable from the JSON-lines event stream
(see [Observability](observability.md)). The names are part of the frozen metrics
schema:

| Metric | Type | Meaning |
|---|---|---|
| `agentd_saturation` | gauge `[0,1]` | `in_flight / capacity` — the HPA "utilization" target. |
| `agentd_pending_events` | gauge | Reactive events received but not yet routed (backlog). |
| `agentd_inflight_reactions` | gauge | Reactions currently executing. |
| `agentd_reaction_lag_ms` | gauge | Age of the oldest un-routed pending event (ms). |
| `agentd_subscriptions_active` | gauge | Reconciled declared subscriptions. |
| `agentd_active_subagents` | gauge | Subagents currently alive in the tree. |
| `agentd_shard_skipped_total` | counter | Items dropped as out-of-shard — high on an over-sharded fleet. |
| `agentd_claims_lost_total` | counter | Claims lost to another replica. **High and rising under low backlog ⇒ over-provisioned ⇒ scale down.** |
| `agentd_claims_granted_total` | counter | Claims this replica won. |
| `agentd_claims_released_total` | counter | Held claims handed back (wind-down / drain). |

A typical scaler scales **out** on rising backlog (`agentd_pending_events` /
`agentd_reaction_lag_ms`) or high `agentd_saturation`, and scales **in** when
`agentd_claims_lost_total` rises under low backlog (replicas fighting over too
little work). agentd never changes its own replica count — scaling is the control
plane's job.

### 5.1 `agentd://capacity` — the placement view

When serving its self-MCP (`--serve-mcp`) in a `cluster` build, agentd exposes
**`agentd://capacity`** — a management-only read surface agentctl uses to place
work onto the right replica:

```jsonc
{
  "instance": "agentd-2",          // downward-API instance identity
  "shard": "2/4",                  // the K/N identity, or null when unsharded
  "standby": false,                // reflects --standby (§6)
  "free_slots": 14,                // max_total_subagents − active_subagents
  "active_subagents": 2,           // in-flight served-run spawns
  "intelligence": { "warm": true, "healthy": true },
  "max_total_subagents": 16,       // the subagent tree cap (RFC 0009)
  "saturation": 0.125              // active / max_total, in [0,1]
}
```

`saturation` here is `active_subagents / max_total_subagents` (the tree cap);
`intelligence.warm`/`healthy` derive from whether the configured endpoint list is
all-down (see [Intelligence](intelligence.md)). No secret, no URL is ever in this
body.

---

## 6. Standby workers (`--standby`) — read this honestly

`--standby` (env `AGENTD_STANDBY`) plus `--assign-from <server>:<uri>` makes a
reactive worker that is driven by a shared **assignment channel** rather than its
own content subscriptions. On the shared "pending work" resource's `updated`,
every standby member races `work.claim` on it (claim-pull) and processes only
what it wins. Under the hood `--assign-from` is just **desugared into a claim
route** on `(uri, server)` whose URI is folded into the subscribe set — it reuses
the existing claim machinery with no new code path.

```bash
agentd --instruction-file /etc/agentd/task.md \
       --intelligence unix:/run/intel.sock \
       --mode reactive --standby \
       --mcp coord='mcp-server-workqueue --addr /run/coord.sock' \
       --assign-from coord:'agentd://assignments'
```

`--standby` / `--assign-from` are only valid with `--mode reactive` and need the
`cluster` feature (both validated, exit `2`). A standby instance reports
`standby:true` on `agentd://capacity` so agentctl can direct an assignment only to
warm members.

> **What standby is NOT (yet).** There is **no warm-child pool**. agentd's
> supervisor runs no LLM loop — every reaction re-execs and connects its own
> intelligence — so today "standby" means *a reactive worker that claim-pulls an
> assignment channel and reports `standby:true`*. It does **not** eliminate
> cold-start. The `AGENTD_WARM_INTEL` flag (default `true` when `--standby`) is
> **forward-compat only**: it is accepted, stored, and reported, but pre-warms
> nothing in v1. It exists so a future warm-child-pool build honours the
> operator's intent without a config break. Do not deploy standby expecting
> cold-start elimination.

---

## 7. Deploy a sharded fleet (sketch)

A `cluster`-build image, run as a StatefulSet so each replica gets a stable
ordinal, with agentctl (or an init step) deriving `AGENTD_SHARD` from it:

```yaml
apiVersion: apps/v1
kind: StatefulSet
metadata:
  name: agentd
spec:
  serviceName: agentd
  replicas: 4
  template:
    spec:
      containers:
        - name: agentd
          image: registry.example/agentd:cluster
          env:
            # The ordinal (agentd-0 → "0") becomes K; replicas becomes N.
            # An init/entrypoint sets AGENTD_SHARD="${ORDINAL}/4".
            - name: AGENTD_SHARD
              value: "0/4"            # rewritten per-pod from the ordinal
            - name: AGENTD_INTELLIGENCE
              value: "unix:/run/intel.sock"
            - name: AGENTD_MODE
              value: "reactive"
          args:
            - --instruction-file=/etc/agentd/task.md
            - --subscribe=file:///inbox/
            # Optional claim backstop for death recovery within the shard:
            - --mcp=coord=mcp-server-workqueue --addr /run/coord.sock
            - --claim=file:///inbox/=coord
            - --metrics-addr=:9090
          livenessProbe:
            exec: { command: ["sh","-c","test -f /run/agentd/health"] }
```

Scaling `replicas` requires re-deriving `N` for every pod and a **rolling
restart** (the shard count is restart-only). An external HPA/KEDA scaler watches
`agentd_saturation` / `agentd_pending_events` (scale out) and
`agentd_claims_lost_total` (scale in), and rewrites `replicas` — agentd only
emits the signals.

---

## See also

- [Deploying agentd](deployment.md) — pod recipes, StatefulSets, drain timing,
  `terminationGracePeriodSeconds`.
- [Observability](observability.md) — the full metrics schema, the JSON-lines
  event stream, and deriving metrics from logs.
- [Intelligence](intelligence.md) — endpoint health, the circuit breaker, and the
  `agentd://intelligence` resource behind `intelligence.warm`/`healthy`.
- [Modes & triggers](modes-and-triggers.md) — reactive routing, `--subscribe` vs
  `--continue`, the spawn-vs-continue disposition.
- [Configuration reference](configuration.md) — every flag/env, including the
  run-id idempotency key the claim convention rides.
- [Operations](operations.md) — the management surface and the `drain`/`lame-duck`
  operator tools agentctl uses to scale a fleet down safely (drain releases held
  claims, §3), plus hot reload.
