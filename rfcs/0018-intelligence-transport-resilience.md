# RFC 0018: Intelligence transport resilience — multi-endpoint failover, health, and runtime hot-swap

**Status:** Proposed (agentctl control-plane track)
**Author:** Andrii Tsok
**Date:** 2026-06-27
**Part of:** the agentd rewrite — control-plane track (RFC 0014); extends the intelligence transport & wire (RFC 0006)

---

## 1. Problem / Context

RFC 0006 nails the intelligence wire to **exactly one endpoint** named by a
single URI in `AGENTD_INTELLIGENCE`, dialed fresh per call with `Connection:
close`. That is the right primitive for the data plane, but it is brittle under
the deployment shape RFC 0014 introduces: a fleet of agentd pods whose
intelligence is supplied by a **host-side model service over vsock** (the
node-agent's job, RFC 0014 §2). In that world the single endpoint is a moving,
fallible thing:

- The host model service is **rolled / upgraded** under the running fleet — for
  a few seconds its CID:port refuses connections, or returns 503.
- A model service **moves CID** (host reschedule, device re-provision) and every
  pod's pinned `vsock:<cid>:<port>` is now wrong, with no restart in sight.
- A gateway sidecar **flaps** (OOM-restart loop), so a naive per-call dial wedges
  every subagent turn on connect timeouts and the whole tree stalls.
- An operator wants to **swap the model** under a long-lived reactive daemon —
  small↔large, or to a cheaper tier — without dropping in-flight work.

RFC 0006 today turns each of these into an `EXIT_INTELLIGENCE` (4) and a pod
restart. For a `once`-mode `Job` that is acceptable (the scheduler retries). For
a long-lived **reactive `Deployment`** it is not: a host-side model roll should
not look like a fleet of crashing pods, and a CID move should not require a
rolling restart of every agent.

This RFC makes the intelligence channel **resilient** without breaking RFC 0006's
moat: it stays the same hand-rolled HTTP/1.1 client over the same `Transport:
Read + Write` trait, over the same three transports (unix / https-tls / vsock).
It adds **zero dependencies** — failover, circuit-breaking, health, and hot-swap
are pure state machines over the existing dial path, feature-gated behind a new
`intel-resilience` gate that the cloud-native image set turns on but the default
serde+serde_json+libc build leaves off. **No async runtime, no connection pool,
no service-discovery client, no TLS/gRPC stack** is admitted; anything that would
pull one belongs in agentctl, which owns *which* endpoints exist and *where* they
move (RFC 0014 §6 — provisioning the vsock device and host model service is the
node-agent's job).

This RFC slots under RFC 0014 as sub-RFC **0018** and couples to four contracts it
does **not** redefine: the metrics schema and the `agentd://` resource surface
that agentctl scrapes (RFC 0016 / RFC 0015), the hot-reload mechanism (RFC 0017),
and the capabilities manifest (RFC 0015). It owns one thing: **how agentd survives
its model endpoint moving, flapping, or being swapped.**

This RFC owns: the `--intelligence` endpoint *list* and its failover policy; the
per-endpoint health/circuit-breaker state machine; the `agentd://intelligence`
resource and the endpoint health metrics' *semantics* (the metric *schema* is
frozen by RFC 0016); the quiesce-switch-resume hot-swap choreography on top of RFC
0017; and the optional model-discovery handshake surfaced into the RFC 0015
manifest. It does **not** own: the wire types or adapters (RFC 0006), the
terminal-status vocabulary (RFC 0007 §3.4), the exit-code table (RFC 0011 §5), the
secrets front door (RFC 0006 §6 / RFC 0012), the reload trigger (RFC 0017), or the
metric exposition format (RFC 0010 §3.8 / RFC 0016).

---

## 2. Decision

1. **`--intelligence` accepts an ORDERED LIST of endpoints**, comma-separated:
   `--intelligence vsock:3:8080,vsock:3:8081,unix:/run/intel.sock`. The list is
   **primary + ordered fallbacks**, may mix transports (RFC 0006's unix / https /
   vsock set), and parses each element with RFC 0006's `parse_intelligence_uri`
   unchanged. A single-element list is exactly RFC 0006 behaviour — the resilience
   machinery is inert with one endpoint. Per-endpoint credentials stay
   env/flag/file (§7); **the list never carries a secret**.

2. **Failover is health-aware, sticky-primary, with a documented policy.** A call
   tries the **active** endpoint first; on a *failover-class* failure (connect
   refused/reset, timeout, HTTP 5xx, or a circuit-open endpoint) it advances to the
   next *available* endpoint in list order, within one bounded retry budget. A
   *non*-failover failure (HTTP 401/403 auth, 4xx request error, malformed body) is
   **not** failed over — it is the same fatal/observation class RFC 0006/0007
   already define, identical on every endpoint. After recovery the client returns
   to the **lowest-index healthy endpoint** (sticky-primary), so a transient
   fallback does not become permanent.

3. **Each endpoint carries a health record and a circuit breaker.** Per endpoint
   we track reachability, EWMA latency, and a rolling error rate, and run a
   three-state breaker (`closed → open → half-open`): N consecutive failover-class
   failures **open** the breaker (skip the endpoint for a cooldown), one probe in
   **half-open** decides re-close or re-open. A flapping host model service is thus
   *removed from rotation*, not retried into the ground — the loop never wedges on
   it. Health is exposed as **metrics (schema frozen in RFC 0016)** and as the
   **`agentd://intelligence`** resource (RFC 0015 surface), so agentctl sees which
   endpoint is active and why.

4. **The endpoint list and the model are hot-swappable at runtime via RFC 0017**,
   without restart and without losing in-flight work. A reload that changes
   `intelligence` or `model` triggers a **quiesce → switch → resume** at a model-call
   boundary: in-flight subagents **finish their current turn on the old config**,
   the supervisor flips the shared endpoint/model snapshot, and the **next** turn
   uses the new one. **What is atomic:** the swap of the supervisor-held config
   snapshot (one pointer). **What a model swap means for an in-flight run:**
   **finish-on-old by default** (the turn boundary is the seam), **restart-turn-on-new
   is opt-in** via `--model-swap=restart-turn`. Endpoint repoint (same model, new
   CID/host) is always finish-on-old: it is invisible to the run.

5. **Model discovery is optional and capability-negotiated.** If an endpoint
   advertises it, agentd performs a tiny handshake (`GET /v1/models`, the
   OpenAI-compatible shape RFC 0006 already speaks) to learn which models it
   serves, and surfaces the result into the RFC 0015 capabilities manifest
   (`intelligence.models`) so agentctl can do **model-aware placement**. An endpoint
   that does not support it (404 / connection-style failure on the probe) **degrades
   silently** — discovery is never required, never blocks startup, never a fatal.

6. **All-endpoints-down has documented terminal semantics.** When *every* endpoint
   is unavailable after the bounded budget: **`once` exits `4` (`EXIT_INTELLIGENCE`,
   RFC 0011 §5)** exactly as today — the scheduler retries. **`loop`/`reactive`
   daemons do not exit**; they enter a **bounded, jittered all-down backoff**,
   surfaced as the `intel.all_endpoints_down` event + an `agentd_intel_all_down`
   gauge, and resume the instant any endpoint half-opens healthy. Liveness is **not**
   failed (RFC 0010 §3.7 — a stuck *upstream* is not a stuck *supervisor*);
   readiness flips **not-ready** while all-down so the orchestrator stops routing
   work to the pod.

These decisions are additive and feature-gated. A default build, or a build with
`intel-resilience` off, is byte-for-byte RFC 0006: one endpoint, one dial, exit 4
on failure.

---

## 3. Mechanisms — the endpoint list & failover policy

### 3.1 Parsing the list (`intel/endpoints.rs`)

`AGENTD_INTELLIGENCE` / `--intelligence` is split on `,` into an ordered
`Vec<IntelEndpoint>`, each element parsed by RFC 0006's `parse_intelligence_uri`
verbatim. Config precedence is unchanged (RFC 0011 §3.1 — flag > env > file >
default); the list is a single scalar, so it lives in env/flags, **not** the
config file, like every per-environment value (RFC 0011 §3.2).

```rust
// intel/endpoints.rs  [feature = "intel-resilience"]
pub struct EndpointList {
    eps: Vec<Endpoint>,        // list order == failover priority; eps[0] is primary
    active: AtomicUsize,       // index currently preferred (sticky-primary, §3.3)
}

pub struct Endpoint {
    uri:     IntelEndpoint,    // RFC 0006 parsed URI (unix | https | vsock)
    dialect: Dialect,          // RFC 0006 §4 (openai | anthropic); per-endpoint override allowed
    wire:    Wire,             // RFC 0006 §3 (http | framed)
    key_name: Option<String>,  // secret NAME to resolve() per request (RFC 0006 §6); never the value
    health:  HealthRecord,     // §4.1
    breaker: Breaker,          // §4.2
}
```

Validation at startup (RFC 0011 §3.3, exit `2` before any side effect):

- empty list → `ConfigError::missing("AGENTD_INTELLIGENCE")`;
- any element fails `parse_intelligence_uri` → exit `2` with the bad element;
- any `https:` element without the `tls` feature, or any `vsock:` without the
  `vsock` feature → exit `2` (RFC 0006 scheme-supported check, per element);
- the build-time key probe (RFC 0006 §6) runs **per endpoint that names a key** —
  a named-but-unset key on *any* listed endpoint is exit `2` (we fail fast rather
  than discover it on failover).

A **single-element list** constructs an `EndpointList` whose failover/breaker code
paths are never entered; it is RFC 0006 with one extra `AtomicUsize` read per call.

### 3.2 Per-endpoint credentials (decision 1 restated, binding)

Each endpoint resolves its own credential **by name** through `secrets::resolve`
(RFC 0006 §6) **per request**, so a file-backed rotating secret (k8s Secret mount /
Vault sidecar) is picked up with no reload. Endpoint-specific key *names* are set
via env, e.g. `AGENTD_INTELLIGENCE_TOKEN` (the default applied to every endpoint)
or per-endpoint overrides `AGENTD_INTELLIGENCE_TOKEN_1`, `_2`, … (1-indexed by list
position) when fallbacks live behind different gateways. **The list URI carries no
key**; the secret value is never in env-as-list, never in the file, never logged
(RFC 0006 §6 / RFC 0012). This is the only credential surface — RFC 0018 adds no
new secret source.

### 3.3 The failover decision (`intel/failover.rs`)

The client's `complete()` (RFC 0006 §7) is wrapped. The wrapper drives one bounded
**failover sweep** per logical `complete` request:

```rust
// intel/failover.rs  [feature = "intel-resilience"]
pub fn complete_resilient(list: &EndpointList, req: &Request) -> Result<Response> {
    let order = list.attempt_order();          // active first, then ascending index,
                                               // skipping circuit-open endpoints (§4.2)
    let mut last_err = None;
    for idx in order {                          // bounded: at most eps.len() endpoints
        let ep = list.ep(idx);
        match ep.complete_once(req) {           // == RFC 0006 §7 dial + round_trip, per-call retry inside
            Ok(resp)                       => { list.record_success(idx); list.prefer_lowest_healthy(); return Ok(resp); }
            Err(e) if e.is_failover_class() => { list.record_failure(idx, &e); last_err = Some(e); continue; }
            Err(e) /* non-failover */       => { return Err(e); }   // auth/4xx/malformed — same on every ep (decision 2)
        }
    }
    Err(IntelError::AllEndpointsDown(last_err.boxed()))  // §6
}
```

**Per-endpoint, per-call retry stays RFC 0006/0007's.** `complete_once` keeps RFC
0007 §3.6's transport-layer retry (429/5xx → bounded retry with backoff+jitter,
small N) *within* a single endpoint before this wrapper considers it failed. The
failover sweep is the **outer** loop: it advances endpoints only after an endpoint
has exhausted its own bounded retry. The two budgets compose but are bounded
independently; their product is capped by the run deadline (RFC 0007), and a
failover sweep never exceeds `eps.len()` distinct endpoints (each visited at most
once per `complete`).

**Failover classification** (`is_failover_class`) — extends RFC 0006 §3 / RFC 0007
§3.6, does not redefine:

| Outcome | Failover? | Rationale |
|---|---|---|
| connection refused / reset / connect timeout | **yes** | endpoint is down/moving — try the next |
| read/write timeout mid-request | **yes** | endpoint wedged — try the next |
| HTTP 502 / 503 / 504 (and 500 after the endpoint's own retry) | **yes** | upstream transiently unavailable |
| HTTP 429 after the endpoint's own retry budget | **yes** | this endpoint is saturated; a sibling may not be |
| circuit-open (endpoint skipped, §4.2) | **yes** | breaker says don't even dial |
| HTTP 401 / 403 (auth) | **no** | misconfig is identical on every endpoint → fatal (RFC 0006 → exit 4) |
| HTTP 4xx request error / malformed JSON body | **no** | a bad request is bad everywhere → RFC 0007 observation/abort |

**Sticky-primary.** `attempt_order()` yields the **active** index first (the
endpoint that last succeeded, or the lowest-index healthy one), then the remaining
healthy endpoints in **ascending list order**. On any success, `record_success`
sets `active = idx` *only if* `idx` is lower than the current active is unhealthy;
`prefer_lowest_healthy()` then walks the list and snaps `active` back to the lowest
healthy index — so once the primary's breaker re-closes (§4.2), the next call
returns to it. A fallback is **temporary by construction**.

### 3.4 What stays untouched

The wrapper is the *only* net-new control flow. `dial()`, `round_trip`,
`build_body`/`parse`, the framed-`complete` path, the per-request `resolve()`, the
`Connection: close` posture, and the `Token`-never-logged discipline are all RFC
0006 verbatim. There is **no connection pool and no keep-alive** — each
`complete_once` still dials fresh (RFC 0006 §7); the request rate is single-digit
per second per subagent, so the only state we keep between calls is the cheap
health/breaker record, never a live socket.

---

## 4. Mechanisms — endpoint health & circuit-breaking

### 4.1 The health record

Per endpoint, updated on every `complete_once` outcome and every discovery/breaker
probe. All fields are integers/atomics — no histogram library, no SDK (RFC 0010
§2: no metrics crate in the default build):

```rust
// intel/health.rs  [feature = "intel-resilience"]
pub struct HealthRecord {
    state:          AtomicU8,    // BreakerState: Closed=0 | Open=1 | HalfOpen=2 (§4.2)
    consec_fail:    AtomicU32,   // resets to 0 on success
    total_calls:    AtomicU64,
    total_fail:     AtomicU64,   // failover-class failures
    ewma_latency_us:AtomicU64,   // EWMA, alpha=1/8, updated on success
    last_ok_unix_ms:AtomicU64,   // last successful round-trip
    last_err_unix_ms:AtomicU64,
    last_err_kind:  AtomicU8,    // small enum: Refused|Reset|Timeout|Http5xx|Http429|Probe
    opened_unix_ms: AtomicU64,   // when the breaker last opened (cooldown clock, §4.2)
}
```

Error rate is derived (`total_fail / total_calls` over the process, plus the cheap
`consec_fail` for the breaker decision); a windowed rate is **not** kept in-binary
— agentctl computes rates from the scraped counters over its own window (RFC 0016),
keeping cardinality and state out of agentd (the same discipline as RFC 0010 §3.8:
agentd emits counters, the collector computes rates).

### 4.2 The circuit breaker

A three-state breaker per endpoint, decided synchronously on the call path (no
background timer thread — the supervisor reactor already wakes on a `recv_timeout`,
RFC 0002; we check the cooldown clock against `now` when an endpoint is consulted):

```
                consec_fail >= OPEN_THRESHOLD (default 3)
   CLOSED ───────────────────────────────────────────────► OPEN
     ▲                                                        │ now - opened_ms >= COOLDOWN (default 5s, ×2 backoff to 60s cap)
     │ probe Ok                                               ▼
     └──────────────── HALF_OPEN ◄───────────────────── (next consult promotes to half-open)
            probe Err → back to OPEN (cooldown ×2, capped)
```

- **CLOSED** — normal. `attempt_order()` includes it. `OPEN_THRESHOLD` (default 3)
  consecutive failover-class failures → **OPEN**, stamping `opened_unix_ms`.
- **OPEN** — `attempt_order()` **skips** it (a skip is itself a failover-class
  "don't dial" outcome, decision 3 / §3.3). When `now - opened_unix_ms >= cooldown`,
  the next consult promotes it to **HALF_OPEN**. Cooldown starts at 5s and doubles
  each consecutive open (5→10→20→40→60s cap) so a hard-down endpoint is probed
  ever-less-often.
- **HALF_OPEN** — eligible for **exactly one** probe (the next `complete` that
  reaches it, or a dedicated cheap probe if discovery is on, §5). Probe success →
  **CLOSED**, reset `consec_fail`/cooldown; probe failure → **OPEN**, cooldown ×2.

**The wedge this prevents (decision 3).** Without the breaker, a flapping endpoint
is dialed every call, each dial pays a full connect-timeout, and every subagent
turn stalls on it — the tree's effective throughput collapses even though healthy
fallbacks exist. With the breaker, a flapping endpoint is *removed from rotation*
after 3 strikes and only probed once per (growing) cooldown, so the active path
stays on a healthy endpoint and the loop never wedges. This is the intelligence-channel
analogue of RFC 0003's restart governor for crash-looping children.

Defaults (all overridable; names are the public contract):

| Knob | Env | Flag | Default |
|---|---|---|---|
| open threshold | `AGENTD_INTEL_BREAKER_THRESHOLD` | `--intel-breaker-threshold` | `3` |
| initial cooldown | `AGENTD_INTEL_BREAKER_COOLDOWN` | `--intel-breaker-cooldown` | `5s` |
| cooldown cap | `AGENTD_INTEL_BREAKER_COOLDOWN_MAX` | `--intel-breaker-cooldown-max` | `60s` |
| all-down backoff | `AGENTD_INTEL_ALLDOWN_BACKOFF` | `--intel-alldown-backoff` | `1s..30s` jittered (§6) |

### 4.3 Health as metrics (semantics; schema frozen in RFC 0016)

The endpoint health surfaces as metrics whose **names and label sets are frozen by
RFC 0016** (this RFC defines their *meaning*, RFC 0016 owns the schema so agentctl
can build dashboards/alerts against a stable contract). Per the RFC 0010 §3.8
cardinality rule, the only label is `endpoint` — a **bounded, list-index identity**
(`"0"`, `"1"`, …), *never* the URI (a URI can encode a moving CID and would blow
cardinality and leak topology):

| Metric | Type | Labels | Meaning |
|---|---|---|---|
| `agentd_intel_endpoint_up` | gauge 0/1 | `endpoint` | breaker not OPEN (i.e. in rotation) |
| `agentd_intel_endpoint_active` | gauge 0/1 | `endpoint` | currently the sticky-primary `active` index |
| `agentd_intel_calls_total` | counter | `endpoint`,`result` | `result` ∈ `ok`/`failover`/`fatal` |
| `agentd_intel_failovers_total` | counter | `from`,`to` | a sweep advanced from one index to another |
| `agentd_intel_breaker_opens_total` | counter | `endpoint` | breaker open transitions |
| `agentd_intel_endpoint_latency_ms` | gauge | `endpoint` | EWMA round-trip latency (success only) |
| `agentd_intel_all_down` | gauge 0/1 | — | every endpoint unavailable (§6) |

These extend, and do not collide with, the RFC 0010 §3.8 intelligence counters
(`agentd_intel_calls_total{model}` etc. remain; this set is `endpoint`-keyed and
RFC-0018-owned). The exact registration lives in RFC 0016's frozen schema.

### 4.4 Health as a resource — `agentd://intelligence`

A new readable + subscribable resource on the self-MCP surface (RFC 0005 §3.3
resource tree; the *operator profile* that lists it is RFC 0015's management
surface). It is the live, human/agentctl-readable view of §4.1/§4.2. **No payload
in the notification** — notify-then-read, exactly RFC 0005 §3.4:

| URI | Body (on `resources/read`) |
|---|---|
| `agentd://intelligence` | endpoint list, the active index, per-endpoint health/breaker/latency, all-down state, discovered models (§5) |

```jsonc
// resources/read agentd://intelligence  → contents[0].text (application/json)
{
  "active": 0,
  "all_down": false,
  "model": "claude-opus-4",
  "swap_policy": "finish-on-old",          // §5 (--model-swap)
  "endpoints": [
    { "index": 0, "transport": "vsock", "addr": "3:8080",
      "state": "closed", "active": true,
      "ewma_latency_ms": 41, "error_rate": 0.002,
      "last_ok_ms_ago": 120, "consec_fail": 0,
      "models": ["claude-opus-4","claude-haiku-4"] },   // §5 discovery, if available
    { "index": 1, "transport": "vsock", "addr": "3:8081",
      "state": "open", "active": false,
      "ewma_latency_ms": 0, "error_rate": 1.0,
      "last_err": "refused", "opened_ms_ago": 2300, "cooldown_ms": 10000,
      "models": [] }
  ]
}
```

**Emission rule (RFC 0005 §3.4, extended by the closed set below).** The supervisor
emits `notifications/resources/updated{uri:"agentd://intelligence"}` to every
subscribed peer on any of these transitions — and *only* these (no per-call spam):

| Transition | Emit? |
|---|---|
| breaker state change (closed↔open↔half-open) on any endpoint | yes |
| `active` index change (failover or snap-back to primary) | yes |
| all-down enter/exit (§6) | yes |
| model/endpoint hot-swap applied (§5) | yes |
| discovered-models set changes (§5) | yes |
| individual call success/failure (latency tick) | **no** — too chatty; latency is read on demand |

Subscriptions, gating, and the at-least-once/idempotent-via-re-read contract are
RFC 0005 §3.4 verbatim; this RFC only adds the URI and its transition set.

The **URI is never the endpoint identity in metrics** (§4.3) but it is fine in the
resource *body*, which is read on demand and not a cardinality surface — agentctl
reads it to render `kubectl agents describe <x>` intelligence health (RFC 0015).

---

## 5. Mechanisms — runtime hot-swap & model discovery

### 5.1 What triggers a swap (RFC 0017 owns the trigger)

A swap is just a **hot reload** (RFC 0017) whose diff touches `intelligence` (the
endpoint list) and/or `model`. RFC 0017 owns the `SIGHUP`/file-watch trigger,
re-validation, and the reloadable-field allowlist; this RFC owns **what
`intelligence`/`model` being reloadable *means* for in-flight work**. RFC 0011 §4.1
froze "no SIGHUP/reload in v1"; RFC 0017 lifts that for the allowlisted fields, and
`intelligence` + `model` are on that allowlist precisely so this RFC can act. The
unchanged-fields-stay-frozen and re-validate-before-apply discipline is RFC 0017's.

Two distinct swaps, both routed through the same choreography:

- **Endpoint repoint** — the model is the same, only the list/CID/host changed
  (host model service moved CID; rolling the gateway). **Always finish-on-old**;
  invisible to a run — a turn that started on the old list completes on it (or
  fails over within it), and the next turn dials the new list.
- **Model swap** — `model` changed (small↔large, cheaper tier). The seam is a
  turn boundary; **finish-on-old by default, restart-turn-on-new opt-in** (§5.3).

### 5.2 Quiesce → switch → resume (the atomic seam)

The intelligence config the loop reads is a single supervisor-held immutable
snapshot behind an `Arc`-swap-style pointer (one `AtomicPtr`/`arc_swap`-free swap
of an `Arc<IntelConfig>` — std `RwLock<Arc<…>>` suffices, no new dep):

```rust
// supervisor holds the source of truth; subagents read a cloned Arc per turn
struct IntelConfig { endpoints: EndpointList, model: String, swap_policy: SwapPolicy }
static LIVE: RwLock<Arc<IntelConfig>> = /* … */;   // swapped on reload (RFC 0017 apply step)
```

Choreography on a validated reload whose diff includes `intelligence`/`model`:

1. **Quiesce at the model-call boundary.** The loop reads `LIVE` **once per turn**,
   at the top of the turn, before assembling the request (RFC 0007 §3 loop step).
   A reload does **not** interrupt an in-flight `complete_once` — it cannot tear a
   request, because the loop only re-reads `LIVE` at the next turn boundary. There
   is nothing to "drain": the seam is the existing per-turn read.
2. **Switch (atomic).** The supervisor builds the new `Arc<IntelConfig>` (new
   `EndpointList` with **fresh** health records — a repointed endpoint starts
   CLOSED, no stale breaker state carries to a new CID) and swaps `LIVE` in one
   `RwLock` write. **This pointer swap is the only atomic operation**; everything
   else is eventual at turn boundaries.
3. **Resume.** The next turn of every in-flight subagent reads the new `Arc` and
   uses the new endpoints/model. No restart, no re-handshake of the *run* — only
   the cheap per-call dial changes target.

**What is atomic vs eventual (decision 4, binding):**

| Thing | Atomic? | Semantics |
|---|---|---|
| `LIVE` pointer swap | **atomic** | one `RwLock<Arc>` write; readers see old-or-new, never torn |
| in-flight `complete_once` | finishes on **old** config | a request mid-flight is never repointed |
| next turn's endpoint list | **new** | dials the new list |
| next turn's model (default) | **new** | finish-on-old: the *current* turn already completed on old model |
| in-flight run's transcript | preserved | a model swap does not reset context (see §5.3 caveat) |

### 5.3 What a model swap means for an in-flight run

A model swap mid-run is the subtle case (an endpoint repoint is invisible). The
**transcript is continuous** across the swap — agentd does not reset context — but
the next turn is served by a *different model*. Two policies, `--model-swap` /
`AGENTD_MODEL_SWAP`:

- **`finish-on-old` (default).** The turn in flight when the reload lands completes
  on the old model; the **next** turn uses the new model with the full existing
  transcript. Cheapest, no wasted work, mildly heterogeneous (one run's later turns
  ran on a different model than its earlier turns — acceptable and logged).
- **`restart-turn` (opt-in).** The turn in flight is allowed to finish (we never
  tear a `complete_once`), but its result is **discarded** and the turn is **re-run
  on the new model** from the same pre-turn transcript state. Use when an operator
  is deliberately upgrading small→large mid-run and wants the *current* reasoning
  step redone by the larger model. Costs one wasted turn; bounded by the loop's
  step budget (RFC 0007) like any other turn.

Neither policy changes the **terminal-status vocabulary** (RFC 0007 §3.4) or the
exit codes (RFC 0011 §5) — a swapped run still ends `Completed`/`Refused`/budget/etc.
A swap event is logged (`intel.swap`, §8) and emitted on `agentd://intelligence`
(§4.4). **Reactive warm sessions** (RFC 0008) read `LIVE` at each turn like any
loop, so a swap applies to all warm sessions at their next turn with no extra
machinery — and survives a daemon restart only via re-read of config (RFC 0011 §7;
warm-session checkpointing stays deferred, RFC 0013).

**Out of scope (stays in agentctl / RFC 0014 §6):** *deciding* to swap (small↔large
policy, cost/latency triggers, rolling a fleet's model) is policy — agentctl writes
the new ConfigMap and signals the reload (RFC 0017). agentd only executes the
quiesce-switch-resume primitive.

> **Resolved / implemented (§5.1–§5.3).** Shipped. RFC 0017 §5.1 now lists
> `intelligence`/`model`/`model_swap` as **reloadable via this RFC's swap
> primitive** (no longer restart-only). **Process-boundary adaptation:** the §5.2
> sketch models `LIVE` as a single supervisor-held `RwLock<Arc<IntelConfig>>` that
> "subagents read per turn" — but agentd re-execs each subagent as its own
> **process**, so a supervisor-side `RwLock` cannot reach a child's loop. The
> faithful implementation makes `LIVE` **child-local**: a new
> `ControlMsg::SwapIntel` (the same fan-out shape as `pause`/`resume`, with a
> payload) crosses the process boundary; the child's control-reader thread parks
> it, and the loop drains it **once per turn at the turn boundary** (exactly where
> `pause_wait` sits) — that IS the §5.2 "read LIVE once per turn", just
> process-local. The supervisor fans the swap to in-flight children on an
> `intelligence`/`model`-touching reload by **reusing the pause fan-out
> infrastructure** (`forward_pause` → a parallel `forward_swap`; warm `--continue`
> sessions via `w.sub.send`; served async runs via a per-run swap channel the
> run's reactor reads). A repointed endpoint is rebuilt with **fresh** health
> (`IntelClient::from_parts` → a new `HealthRecord` per endpoint — no stale breaker
> carries to a new CID, §5.2 step 2). **`finish-on-old` is implemented fully**
> (the natural turn-boundary behaviour). **`restart-turn` is implemented** for warm
> sessions (snapshot the pre-turn transcript, let the in-flight turn finish, then
> discard its appended messages and re-run on the new model from the pre-turn
> state — bounded by the step budget; a one-shot has a single turn so the policy is
> moot for it). The `intel.swap` event (§8) and the `agentd://intelligence` notify
> (§4.4) fire on a swap; **no secret/URL** appears in either (transport+index +
> non-secret model names + policy only — RFC 0012 §3.7). Zero new dependencies
> (std `Arc`/`Mutex`; no `arc_swap`). The endpoint **list** is now file-settable in
> the config schema so a ConfigMap repoint can act; the per-endpoint **credential**
> stays env/`_FILE`-only.

### 5.4 Optional model discovery (capability-negotiated)

agentd may learn what an endpoint serves via a tiny handshake — **off unless an
endpoint looks discovery-capable, and silent on failure** (decision 5):

- **Probe.** `GET /v1/models` (OpenAI-compatible, the dialect RFC 0006 already
  speaks; the `anthropic` dialect has no list endpoint → discovery simply yields
  the configured `model` and nothing else). One hand-rolled HTTP GET over the
  existing `Transport` (RFC 0006 §3) — **no new client, no streaming**.
- **When.** Lazily on first successful `complete_once` against an endpoint, and on
  a half-open re-close (the model set may have changed under a roll). Never at
  startup before a side effect (it is a network call; RFC 0011 §3.3 forbids
  side-effects pre-validation), never on the hot path.
- **Negotiation / degrade.** A 404, a connection-style failure, or a non-JSON body
  on the probe → **discovery unsupported for that endpoint**, recorded as
  `models: []`, **never** a failover-class failure and **never** fatal. An endpoint
  is fully usable with discovery unsupported — the configured `model` is dialed
  regardless.
- **Surface.** Discovered models populate `agentd://intelligence` (§4.4) and the
  **capabilities manifest** `intelligence.models` (RFC 0015 §capabilities), so
  agentctl does **model-aware placement** ("route the opus job to a pod whose
  endpoint serves opus"). The manifest field is **optional and additive** (RFC 0014
  §3 freeze-and-version; absent ⇒ agentctl assumes only the configured `model`).

```jsonc
// capabilities manifest delta (RFC 0015 owns the full schema; this is the intelligence block)
"intelligence": {
  "transport": "vsock",
  "endpoints": 2,
  "healthy": true,                 // any endpoint in rotation
  "active": 0,
  "model": "claude-opus-4",
  "swap_policy": "finish-on-old",
  "discovery": true,               // at least one endpoint answered /v1/models
  "models": ["claude-opus-4","claude-haiku-4"]   // union of discovered + configured; [] if none discovered
}
```

This block **extends** the RFC 0014 §5 manifest sketch (which already shows
`"intelligence": { "transport": "vsock", "endpoints": 2, "healthy": true }`) —
same field, more detail; the additive `active`/`models`/`discovery`/`swap_policy`
keys are version-gated by `contract_version` (RFC 0014 §3.4).

---

## 6. Failure semantics — all endpoints down

This is the load-bearing edge case. "All endpoints down" = the failover sweep
(§3.3) visited every endpoint and each was unavailable (open breaker, or a
failover-class failure on its probe), i.e. `attempt_order()` yields an empty
available set or every attempt failed-over.

**Mode-split, anchored to RFC 0011 §5 and RFC 0010 §3.7:**

| Mode | Behaviour on all-down |
|---|---|
| **`once`** | the run aborts with the RFC 0007 §3.6 *fatal-infrastructure* (intelligence-unreachable) outcome → **exit `4` `EXIT_INTELLIGENCE`** (RFC 0011 §5). The scheduler retries (often the host service is mid-roll). Identical to RFC 0006 today, now after exhausting the *list* not one endpoint. |
| **`loop` / `reactive`** | the daemon **does not exit** (RFC 0011 §5 — daemons exit only `0`/`143`/fatal-class). It enters **all-down backoff**: each turn that needs intelligence and finds all endpoints down yields a recoverable `intel.all_endpoints_down` and the supervisor re-arms with jittered backoff (`--intel-alldown-backoff`, default 1s→30s). It **resumes instantly** when any endpoint half-opens healthy (the next probe re-closes a breaker). A `once`-style fatal-4 exit for a transient host roll on a long-lived daemon would be exactly the spurious-crash-loop RFC 0014 §1 warns against. |

**Cross-cutting invariants during all-down:**

- **Liveness is NOT failed.** A dead *upstream* is not a wedged *supervisor*
  (RFC 0010 §3.7 — the supervisor heartbeat still advances; `/healthz` stays 200).
  Failing liveness here would restart a healthy pod and destroy in-flight work for
  a transient upstream blip.
- **Readiness flips not-ready** while all-down (RFC 0010 §3.7 readiness = able to
  do useful work). The orchestrator stops routing new work to the pod; it flips
  ready again on recovery. This is the right backpressure signal without a crash.
- **Surfaced as event + metric + resource.** `intel.all_endpoints_down` /
  `intel.recovered` events (§8), the `agentd_intel_all_down` gauge (§4.3), and an
  `agentd://intelligence` `updated` emission (§4.4) on enter/exit — so agentctl
  alerts on a *fleet-wide* all-down (likely the host service is down, not the pod)
  without any pod crashing.
- **Bounded by the run deadline.** All-down backoff for a `loop` run with a
  `--deadline` still trips `EXIT_TIMEOUT` (124) at the wall clock (RFC 0011 §5) —
  backoff does not extend a deadline.

**Credential failure is not all-down.** A 401/403 on *every* endpoint is the
non-failover auth class (§3.3): it is RFC 0006's fatal auth → **exit `4`**
immediately on `once`, and on a daemon it is logged as a fatal-class misconfig
(the operator fix is a credential, not a retry). We do not backoff-loop on an auth
error — that would mask a misconfiguration as a transient outage.

---

## 7. Security & minimalism boundary

- **Per-endpoint credentials stay env/flag/file (§3.2).** Resolved by *name* per
  request through `secrets::resolve` (RFC 0006 §6 / RFC 0012). The endpoint **list
  never carries a secret**, the config file never carries a secret (RFC 0011 §3.2),
  the URI never carries a key. `Token` is never logged/serialized (RFC 0006 §6).
- **SSRF / HTTPS-in-prod posture is unchanged (RFC 0012).** Every endpoint in the
  list — primary or fallback — passes the same `net/http.rs` SSRF checks and the
  HTTPS-in-prod enforcement RFC 0006 §2 / RFC 0012 define. A fallback cannot be a
  laxer endpoint; failover does not relax policy.
- **Discovery is read-only.** `GET /v1/models` mutates nothing, sends no secret it
  would not send on a `complete`, and is SSRF-checked like any dial.
- **Minimalism moat (RFC 0014 §3 / decision restated).** Everything here is behind
  the `intel-resilience` feature; the default serde+serde_json+libc build and any
  build without the gate is byte-for-byte RFC 0006. **No async runtime** (the
  breaker is checked synchronously against a clock on the existing reactor wake; no
  timer thread). **No connection pool** (still one dial per call). **No
  service-discovery / k8s / DNS-SRV client** — agentctl supplies the endpoint list
  and moves it via RFC 0017 reload; agentd only *uses* the list it is given (RFC
  0014 §6). **No new TLS/gRPC stack** — TLS stays the existing feature-gated rustls
  path (RFC 0006 §2). A resilience feature that would pull any of those is wrong by
  construction and belongs in agentctl.

---

## 8. Observability (events; schema owned by RFC 0010 / RFC 0016)

This RFC adds a small, closed set of events to the RFC 0010 §3.3 vocabulary
(`comp:"intel"`), and the §4.3 metrics. Adding events is cheap (RFC 0010 §3.3);
these are the complete RFC-0018 additions:

| Event | Fields (beyond canonical) |
|---|---|
| `intel.failover` | `from` (index), `to` (index), `err` (last-err kind) |
| `intel.breaker.open` | `endpoint`, `consec_fail`, `cooldown_ms` |
| `intel.breaker.halfopen` | `endpoint` |
| `intel.breaker.close` | `endpoint` |
| `intel.all_endpoints_down` | `endpoints` (count), `backoff_ms` |
| `intel.recovered` | `endpoint`, `down_ms` (total all-down duration) |
| `intel.swap` | `kind` (`endpoint`/`model`), `model_from?`, `model_to?`, `policy`, `applied_at_turn` |
| `intel.discovery` | `endpoint`, `models` (count), `supported` (bool) |

`endpoint`/`from`/`to` are **bounded list-index** labels in metrics (§4.3) and
appear by index in events (the URI only in the on-demand resource body, §4.4) — the
cardinality discipline of RFC 0010 §3.8. Secrets never appear (RFC 0010 §3.4
allowlist). These events also feed the OTLP GenAI mapping (RFC 0010 §3.9) where one
exists; no new span types are introduced (a swap/failover is an event on the
existing `chat` span, not a new span).

---

## 9. Interactions with other RFCs

- **RFC 0005 (self-MCP & control protocol).** `agentd://intelligence` (§4.4) is a
  new resource on the existing resource tree (§3.3) with the existing
  notify-then-read emission contract (§3.4); the management profile that *lists* it
  to operators is RFC 0015.
- **RFC 0006 (intelligence transport & wire) — the RFC this extends.** The list is
  parsed by its `parse_intelligence_uri`; failover wraps its `complete()`; the
  per-call dial, retry, secrets, and `Connection: close` are unchanged. RFC 0006
  owns one endpoint; this RFC owns the *list and its policy*.
- **RFC 0007 (agentic loop).** The failover classification extends its §3.6 error
  taxonomy (transport retry stays its); all-down on `once` is its §3.6
  fatal-infrastructure (intelligence-unreachable) outcome; the per-turn read of
  `LIVE` (§5.2) is at its loop-step boundary; terminal-status vocabulary (§3.4) is
  unchanged by a swap.
- **RFC 0010 (observability, health & telemetry).** Owns the log schema, the event
  vocabulary this adds to, the metric exposition, the cardinality rule, and the
  liveness-not-failed-on-stuck-upstream / readiness rules §6 leans on. This RFC
  adds events/metrics, not surfaces.
- **RFC 0011 (cloud-native contract).** Owns the exit-code table (all-down `once` →
  `4`; deadline → `124`), config precedence (the list is an env/flag scalar), and
  the daemons-exit-only-`0`/`143`/fatal rule §6 honours.
- **RFC 0012 (security posture).** Owns the SSRF/HTTPS-in-prod policy every endpoint
  passes, and the secrets-never-in-file rule the list obeys.
- **RFC 0013 (deferred v2).** Warm-session checkpointing stays deferred; a swap
  applies to warm sessions at their next turn but does not make them durable across
  a restart (§5.3).
- **RFC 0014 (control-plane umbrella).** This is sub-RFC 0018; the
  primitives-not-policy split (§3) governs the agentctl boundary (§5.3, §7);
  agentctl supplies/moves endpoints and decides swaps, agentd executes the
  primitives.
- **RFC 0015 (management & control surface).** Owns the capabilities manifest this
  extends (`intelligence` block, §5.4) and the operator MCP profile that lists
  `agentd://intelligence`.
- **RFC 0016 (telemetry & lifecycle contract).** **Freezes** the metric schema this
  defines the semantics of (§4.3); agentctl scrapes/alerts against that frozen set.
- **RFC 0017 (declarative config & hot reload).** Owns the reload trigger
  (`SIGHUP`/file-watch), re-validation, and the reloadable-field allowlist that
  must include `intelligence` and `model`; this RFC owns the quiesce-switch-resume
  *meaning* of reloading those two fields (§5).

---

## 10. Non-goals / Deferred

- **No load-balancing across healthy endpoints.** The list is **priority-ordered
  failover**, not round-robin/weighted balancing. Spreading load across replicas is
  agentctl's job (place pods on different endpoints); agentd prefers the
  lowest-index healthy endpoint (sticky-primary). Weighted/least-latency selection
  is a possible later flag, not v1.
- **No connection pooling or keep-alive.** One dial per call stays (RFC 0006);
  failover does not introduce a pool.
- **No background health-probe thread.** Breaker cooldown is checked against the
  clock on the existing call path / reactor wake; we do not actively poll endpoints
  on a timer (discovery is lazy, §5.4). An always-on active health-prober is
  rejected as it would add a thread and constant traffic for an idle daemon.
- **No service discovery.** agentd does not resolve DNS-SRV, watch a registry, or
  learn endpoints from the network — it uses the list it is given and is repointed
  by reload (RFC 0017). "Where is the model service" is agentctl's (RFC 0014 §6).
- **No mid-request repoint / no streaming-aware swap.** A swap seam is a turn
  boundary; an in-flight `complete_once` is never torn (RFC 0006 is non-streaming,
  so a request is short). If streaming `/chat/completions` is ever adopted (RFC 0006
  open item), swap-mid-stream is a follow-up, not v1.
- **No per-endpoint distinct dialect *negotiation*.** A per-endpoint dialect
  override is allowed in config (§3.1), but agentd does not *probe* an endpoint's
  dialect; misconfigured dialect is a non-failover request error (§3.3).
- **No durable health state across restart.** Health/breaker records are in-memory;
  a restarted pod starts every endpoint CLOSED and re-learns (cheap, and correct —
  a new pod has no reason to trust the old pod's breaker state). Consistent with
  RFC 0011 §7 statelessness.

---

## 11. Open items (for the umbrella author — RFC 0014 — to reconcile)

- **Per-endpoint key env naming (`AGENTD_INTELLIGENCE_TOKEN_<N>`).** §3.2 proposes
  1-indexed-by-list-position overrides. RFC 0017's file-based secret refs may want a
  different keying (by endpoint *name* rather than index). Reconcile the naming with
  RFC 0017's secret-ref convention so an operator has one mental model. A naming
  convention, not a design gap.
- **Metric `endpoint` label identity.** §4.3 uses the **list index** as the bounded
  label. If agentctl wants a stable label across a reordered list (a reload that
  inserts a new primary shifts every index), RFC 0016 may prefer a stable
  operator-assigned endpoint *name*. Index is the dependency-free default; a names
  list (`--intelligence-names a,b,c`) is a small addition if RFC 0016 needs it.
  Flagged for the frozen-schema decision.
- **Manifest `intelligence.models` union semantics.** §5.4 unions discovered +
  configured models. If agentctl placement wants per-endpoint model sets (which
  endpoint serves which model) rather than a union, the manifest block should carry
  a per-endpoint array. Deferred to RFC 0015's manifest schema owner; the resource
  body (§4.4) already exposes per-endpoint models, so this is a manifest-shape
  choice, not a capability gap.
- **`restart-turn` interaction with idempotency keys.** §5.3 `restart-turn`
  discards a turn's result and re-runs; if that turn already issued an MCP
  `tools/call` with the `RUN_ID` idempotency key (RFC 0011 §6), the re-run must
  reuse the same key (it does — the key is per-run, not per-turn), but the
  *re-issued* call relies on the backing service's dedupe. Confirm this is the
  intended contract with RFC 0011's idempotency owner; it does not block.
