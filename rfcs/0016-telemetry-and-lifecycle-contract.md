# RFC 0016: Telemetry & lifecycle contract — frozen metrics, run reports, the event stream, exit-code & liveness contracts

**Status:** Proposed (agentctl control-plane track)
**Author:** Andrii Tsok
**Date:** 2026-06-27
**Part of:** the agent rewrite — control-plane track (RFC 0014); extends observability (RFC 0010) and the exit-code contract (RFC 0011 §5)

---

## 1. Problem / Context

RFC 0010 gave agent a complete *self-contained* observability story: a closed
JSON-lines log schema, a closed `event` vocabulary, mode-aware health, W3C
trace-context propagation, and a default "metrics-from-logs" posture with an
optional hand-rolled Prometheus exposition behind the `metrics` feature. RFC
0011 §5 gave it a public exit-code table a `podFailurePolicy` can branch on.
Both were written for an operator reading *one* instance.

A **control plane** (RFC 0014: `agentctl`, its operator, and the `kubectl
agent[s]` plugin) reads a *fleet*. It builds dashboards, alert rules, and
autoscalers; it authors `podFailurePolicy` rules; it renders `kubectl agents
results` and `kubectl agents top`; it tails live activity across pods. Every one
of those couples *tightly* to the exact spelling of a metric name, a label set,
an exit code, a report field, an event name. The moment any of those is a
"documented default" rather than a **frozen, versioned contract**, the control
plane breaks silently on an agent upgrade — the exact failure RFC 0014 §3.4
("freeze and version what agentctl builds against") exists to prevent.

This RFC does **not** add a new telemetry mechanism. RFC 0010 already owns the
log schema, the event vocabulary, health, and propagation; RFC 0011 §5 owns the
exit-code table. This RFC **freezes, versions, and exposes** those primitives as
a contract surface agentctl can build against:

1. a **frozen Prometheus metrics schema** (`metrics_schema` major.minor) — the
   exact names + labels dashboards/alerts/scalers key off (the autoscaling
   inputs RFC 0019 consumes);
2. the **exit-code *contract*** around RFC 0011 §5's table — frozen, versioned,
   surfaced in the manifest, and mapped to `podFailurePolicy` intent;
3. **machine-readable run-outcome reports** for `once`/`Job` runs (`--report-file`
   and `agent://run/{run_id}`) — what powers `kubectl agents results`;
4. the **`agent://events` stream** — a subscribable live event resource so
   agentctl tails activity over the self-MCP without scraping stderr;
5. **stuck-liveness** — surfacing RFC 0003's internal stuck-detector through
   `/healthz` so k8s restarts a *wedged* instance (a live PID is not a live
   agent); and
6. **correlation** — restating that W3C `traceparent` in/out (RFC 0010 §3.6)
   stitches a multi-pod agent flow into one trace, so agentctl need invent
   nothing.

**The minimalism moat is non-negotiable** (RFC 0014 §3.3). Everything here is
feature-gated and dependency-free: the default build stays `serde` + `serde_json`
+ `libc`; the metrics exposition is the hand-written Prometheus text RFC 0010 §3.8
already specified (no `prometheus`/`metrics` crate); the event stream and run
report reuse the existing `serde_json` serializer and the existing self-MCP
transports. **Nothing here pulls an async runtime, a Kubernetes client, or a
TLS/gRPC stack.** The Kubernetes-facing translation — the dashboards, the
alert-rule YAML, the HPA/KEDA scaler objects, the `podFailurePolicy` documents,
the `kubectl` rendering — lives entirely in agentctl. agent exposes the
primitives; agentctl owns the policy (RFC 0014 §3).

---

## 2. Decision

1. **Freeze a versioned Prometheus metrics schema (`metrics_schema`, §4).** The
   metric names and label *keys* RFC 0010 §3.8 enumerated become a **frozen
   public API**, given a `major.minor` version surfaced in the manifest
   (`surfaces.metrics_schema`, RFC 0014 §5). agentctl builds dashboards, alerts,
   and scalers against this schema and branches on its version. The schema is
   *additive within a major*; a removed/renamed metric or a removed/renamed
   label **bumps the major** (§8). This RFC enumerates the binding set; the
   exposition mechanism is unchanged from RFC 0010 §3.8 (hand-written Prometheus
   text under the `metrics` feature — **no metrics crate**).

2. **The exit-code table is owned by RFC 0011 §5; this RFC freezes the *contract*
   around it (§5).** It does **not** redefine the table. It (a) declares the table
   versioned (`exit_codes` contract version, surfaced in the manifest), (b) maps
   each code to a **`podFailurePolicy` intent** — `terminal` (non-retriable) vs
   `retriable` vs `policy` — for agentctl to compile into `onExitCodes` rules, and
   (c) pins the OS-set `137`/`143` semantics so agentctl distinguishes
   infra-kill from a clean drain. Any change to the table is breaking and bumps
   the exit-code contract major.

3. **Emit a machine-readable run-outcome report for terminating runs (§6).** A
   `once`/`loop`-bounded/`schedule`-tick run writes a single JSON object —
   `{run_id, status, exit_code, usage{tokens_in,tokens_out,steps}, duration_ms,
   distillate_ref, started_at, ended_at, …}` — to `--report-file PATH` (atomic
   write) when configured, and serves the same object as `agent://run/{run_id}`
   (RFC 0005 §3.3). `status` is the RFC 0007 §3.4 terminal-status string;
   `exit_code` is the RFC 0011 §5 code. This is the structured backend for
   `kubectl agents results`. **Reactive daemons do not emit a final report**
   (they have no single terminal outcome); their per-reaction outcomes live in
   the event stream and metrics.

4. **Serve `agent://events`: a subscribable, bounded live-event resource (§7).**
   A new read-only `agent://` resource (RFC 0005 scheme) carrying the RFC 0010
   §3.2 line schema over the RFC 0005 **notify-then-read** semantics, backed by a
   **bounded in-memory ring** the subscriber drains. agentctl tails live activity
   over the self-MCP (vsock or unix), never by scraping container stderr. It is
   a *projection* of the same stderr stream — same lines, same closed vocabulary
   — not a second telemetry path. Gated behind `serve-mcp` + `events`.

5. **Surface RFC 0003's stuck-detector through `/healthz` (§5-liveness, owned by
   RFC 0010 §3.7).** A *wedged supervisor reactor* fails liveness so k8s restarts
   the pod; a *stuck subagent* must **not** fail pod liveness (RFC 0003 detects
   and kills it; the tree stays live). This RFC only states the control-plane
   *intent* — "a live PID is not a live agent, probe the reactor heartbeat" — and
   defers the surface entirely to RFC 0010 §3.7. No new health mechanism.

6. **Correlation is RFC 0010 §3.6, restated for the fleet (§9).** W3C
   `traceparent` adopted-or-minted on ingest, propagated on every MCP call, LLM
   call, and spawn payload, means a multi-pod agentctl-driven flow stitches into
   one trace with no new agent work. This RFC adds nothing; it points agentctl
   at the existing fields.

7. **Hold the moat and version everything agentctl couples to (§8).** Every
   surface here is feature-gated and dependency-free. The three coupling points —
   `metrics_schema`, the exit-code contract, and the report/event schemas — each
   carry a version in the manifest, change additively within a major, and bump
   the major on any removal/rename. agentctl negotiates on the manifest before it
   drives an instance.

These decisions are final for the control-plane track. Each defers to RFC 0010 /
RFC 0011 / RFC 0007 / RFC 0003 where those own the underlying mechanism — this
RFC freezes and exposes, it does not re-implement.

---

## 3. What this RFC owns vs reuses (the boundary table)

| Concern | Owner | This RFC's role |
|---|---|---|
| JSON-lines log schema + `event` vocabulary | **RFC 0010 §3.2/§3.3** | reuse verbatim as the `agent://events` body and the report's provenance |
| Metric exposition mechanism (Prometheus text) | **RFC 0010 §3.8** | reuse; **freeze + version** the *name/label set* |
| Health surface (`/healthz`, `/readyz`, health file) | **RFC 0010 §3.7** | reuse; state the control-plane *liveness intent* only |
| W3C trace-context propagation | **RFC 0010 §3.6** | reuse; restate for multi-pod stitching |
| Exit-code table | **RFC 0011 §5** | reuse; **freeze + version**, add `podFailurePolicy` intent |
| Drain choreography / signals | **RFC 0011 §4** | reuse; report a clean drain as `0` and `drain`/`restart` as metrics |
| Terminal-status vocabulary | **RFC 0007 §3.4** | reuse as `report.status` and the `agent_runs_total{status}` label |
| Stuck-detector (3-detector model, EOF×pong) | **RFC 0003 §3.2–3.5** | reuse; surface the wedged-reactor verdict to `/healthz` and a metric |
| MCP wire / codec | **RFC 0004** | reuse for the `agent://events` notify-then-read frames |
| Self-MCP `agent://` resources + control protocol | **RFC 0005** | reuse the scheme + notify-then-read; add `agent://events` |
| Capabilities manifest | **RFC 0014 §5 / RFC 0015** | extend `surfaces` with `metrics_schema` + `exit_codes` versions |

If a row says "reuse," this RFC must not redefine it. The new artifacts this RFC
*introduces* are exactly four: the frozen `metrics_schema` enumeration (§4), the
exit-code→`podFailurePolicy` intent mapping + versioning (§5), the run-outcome
report object + `--report-file` flag (§6), and the `agent://events` resource +
its ring (§7).

---

## 4. The frozen Prometheus metrics schema (`metrics_schema`)

### 4.1 Status, version, and exposition

`metrics_schema` starts at **`1.0`** and is surfaced in the manifest at
`surfaces.metrics_schema` (RFC 0014 §5). The exposition is exactly RFC 0010 §3.8:
a tiny in-process table of atomic counters/gauges/histograms rendered as
**Prometheus 0.0.4 text** (`# HELP`/`# TYPE` + `name{labels} value`) on the
already-opt-in `/metrics` surface (`--health-http ADDR` or the unix/vsock
self-MCP socket). **No `prometheus` crate, no `metrics` crate, no async, no SDK.**
In the default build (no `metrics` feature) the same series are *derivable from
the JSON logs* (RFC 0010 §3.8) — the schema below is the contract for both the
derived and the directly-emitted forms, so an agentctl collector that scrapes
`/metrics` and one that maps logs see identical series.

What this RFC adds over RFC 0010 §3.8 is **the freeze**: the names and label keys
below are a public API. A dashboard/alert/scaler in agentctl is authored against
this exact spelling and against `metrics_schema` major `1`.

### 4.2 Cardinality discipline (binding, inherited from RFC 0010 §3.8)

**Never** put `run_id`, `agent_id`, `agent_path`, `call_id`, `session_id`, or a
resource URI into a metric label — they are unbounded and live in logs/traces
only. Labels use **bounded** values only: `status`, `model`, `type`, `server`,
`tool`, `kind`, `route`, `reason`, `limit`, `signal`, `phase`, `transport`, `ok`.
This is a hard rule; a control plane that needs per-run granularity reads the run
report (§6) or the event stream (§7), never a metric.

### 4.3 The enumerated set (the contract)

Grouped by what an agentctl dashboard/alert/scaler consumes. `agent_up` and
`agent_ready` are the liveness/readiness gauges; everything else is keyed to a
closed-vocabulary event (RFC 0010 §3.3) so the derived and emitted forms agree.

**Run lifecycle & terminal-status (the `kubectl agents results`/alert inputs):**

| Metric | Type | Labels | Source event |
|---|---|---|---|
| `agent_up` | gauge | — | `1` while the process is alive |
| `agent_ready` | gauge | — | `0/1`; `proc.ready` / drain (RFC 0010 §3.7) |
| `agent_runs_total` | counter | `status` | `loop.final` / one-shot terminal — `status` ∈ RFC 0007 §3.4 closed set |
| `agent_run_duration_ms` | histogram | `status` | run start → terminal |
| `agent_loop_steps_total` | counter | — | `loop.step` |

`status` is the **RFC 0007 §3.4** closed vocabulary verbatim —
`completed`/`refused`/`exhausted_steps`/`exhausted_tokens`/`deadline`/`stalled`/
`loop_detected`/`cancelled`/`crashed`. agentctl's "terminal-status histogram"
dashboard is `sum by (status) (agent_runs_total)`. No new status strings are
minted here; introducing one is RFC 0007's job, and would bump `metrics_schema`
only if it changes the closed label domain (it does — see §8).

**Token / cost accounting (the cost/quota-aggregation inputs):**

| Metric | Type | Labels | Source |
|---|---|---|---|
| `agent_tokens_total` | counter | `model`, `type` | `intel.result.usage`; `type` ∈ `in`\|`out` |
| `agent_intel_calls_total` | counter | `model` | `intel.call` |
| `agent_intel_call_duration_ms` | histogram | `model` | `intel.call`→`intel.result` |

Token accounting honesty (RFC 0010 §3.9): tokens come from the intelligence
`usage` (RFC 0006); when a gateway omits it, the counter is **not incremented
with a guess** — absence is `0`, never an estimate, so `agent_tokens_total`
stays trustworthy for cost aggregation. agent emits *tokens*, not currency;
**cost = tokens × a price table agentctl owns** — agent never learns a price
(no pricing in the data plane).

**Refusal / bound counters by reason (the safety + alert inputs):**

| Metric | Type | Labels | Source |
|---|---|---|---|
| `agent_refusals_total` | counter | `reason` | the model/loop refused or a guard tripped |
| `agent_limit_exceeded_total` | counter | `limit` | `limit.exceeded` (RFC 0010 §3.3) |

`agent_refusals_total{reason}` is the headline safety metric. `reason` is a
**closed** label domain spanning the trust-budget and bound refusals:

```
trifecta   — a Rule-of-Two / trifecta scope refusal (RFC 0012)
rate        — a spawn-rate / restart-storm cap (RFC 0003/0009)
budget      — a token/step/tree-budget refusal (RFC 0007/0003)
depth       — a max-depth spawn refusal (RFC 0009)
mcp         — an MCP scope / unavailable-tool refusal (RFC 0007 §3.6)
```

`agent_limit_exceeded_total{limit}` mirrors the `limit.exceeded` event's
`limit` field (`steps`/`tokens`/`deadline`/`depth`/`tree_tokens`/`restart_storm`/
`spawn_rate`) — these are the *hard bound* trips; `refusals_total` is the
*model/guard verdict* layer. They are kept distinct so an alert can separate "the
model declined" from "a safety cap fired."

**Subagent-tree gauges (the tree-shape + saturation inputs):**

| Metric | Type | Labels | Source |
|---|---|---|---|
| `agent_active_subagents` | gauge | — | `subagent.spawn`/`exit` delta |
| `agent_tree_depth` | gauge | — | current max depth |
| `agent_tree_breadth` | gauge | — | current max siblings at any node |
| `agent_subagents_spawned_total` | counter | — | `subagent.spawn` |
| `agent_subagents_exited_total` | counter | `status` | `subagent.exit` (RFC 0007 §3.4 status) |
| `agent_subagent_restarts_total` | counter | `reason` | `subagent.restart` (RFC 0003 §3.7) |
| `agent_subagent_stuck_kills_total` | counter | `signal` | `subagent.stuck` — the reliability headline (RFC 0003) |

**Intelligence health (the model-endpoint inputs RFC 0018 also reads):**

| Metric | Type | Labels | Source |
|---|---|---|---|
| `agent_intel_up` | gauge | — | `0/1` — intelligence endpoint reachable (RFC 0006/0018) |
| `agent_intel_errors_total` | counter | `reason` | `unreachable`\|`auth`\|`timeout`\|`5xx` |

`agent_intel_call_duration_ms` (above) doubles as the intelligence latency
histogram. RFC 0018 owns multi-endpoint failover; this RFC only freezes
`agent_intel_up`/`_errors_total` so an alert/scaler has them at `metrics_schema`
`1.0` regardless of whether the `intelligence-resilience` feature is built.

**MCP server health (the dependency inputs):**

| Metric | Type | Labels | Source |
|---|---|---|---|
| `agent_mcp_up` | gauge | `server` | `0/1` per declared MCP server (RFC 0004) |
| `agent_mcp_connect_failures_total` | counter | `server` | `mcp.connect.fail` |
| `agent_tool_calls_total` | counter | `server`, `tool`, `ok` | `tool.result` |
| `agent_tool_call_duration_ms` | histogram | `server`, `tool` | `tool.call`→`tool.result` |

**Lifecycle events (the rollout / drain inputs):**

| Metric | Type | Labels | Source |
|---|---|---|---|
| `agent_drains_total` | counter | `phase` | drain entered/completed (RFC 0011 §4) |
| `agent_restarts_total` | counter | — | supervisor process restarts observed (rebuild+reconcile, RFC 0003 §3.11) |
| `agent_reactor_stalls_total` | counter | — | wedged-reactor liveness trips (RFC 0003/§5 below) |

`agent_drains_total{phase}` with `phase` ∈ `started`\|`completed`\|`forced`
lets agentctl distinguish a clean rolling drain (`completed`, exit `0`) from a
forced one (`forced`, exit `143`) — the dashboard counterpart to the exit-code
distinction in §5.

**Reactive backlog / pending gauges (the RFC 0019 autoscaling inputs — load-bearing):**

| Metric | Type | Labels | Source |
|---|---|---|---|
| `agent_pending_events` | gauge | — | events received but not yet routed to a turn (RFC 0008) |
| `agent_inflight_reactions` | gauge | — | reactions currently executing |
| `agent_subscriptions_active` | gauge | — | reconciled declared subscriptions (RFC 0008) |
| `agent_reaction_lag_ms` | gauge | — | age of the oldest un-routed pending event |

These four are the **scaling signal set** RFC 0019 builds a KEDA/HPA scaler on
(scale on `agent_pending_events` / `agent_reaction_lag_ms`). They are frozen
*here* so RFC 0019's scaler is authored against a stable name at `metrics_schema`
`1.0`. agent exposes the gauge; **the scaler object, the target value, and the
scale-up/down policy are agentctl's** (RFC 0019). agent never learns it is being
scaled.

### 4.4 Example exposition

```
# HELP agent_runs_total Runs by terminal status (RFC 0007 §3.4).
# TYPE agent_runs_total counter
agent_runs_total{status="completed"} 184
agent_runs_total{status="refused"} 3
agent_runs_total{status="deadline"} 1
# HELP agent_tokens_total Model tokens by direction and model.
# TYPE agent_tokens_total counter
agent_tokens_total{model="claude-opus-4",type="in"} 9412233
agent_tokens_total{model="claude-opus-4",type="out"} 412044
# HELP agent_pending_events Reactive events received but not yet routed.
# TYPE agent_pending_events gauge
agent_pending_events 7
# HELP agent_refusals_total Refusals/guard trips by reason.
# TYPE agent_refusals_total counter
agent_refusals_total{reason="trifecta"} 2
agent_refusals_total{reason="depth"} 1
```

A scrape carries no `metrics_schema` label (it would be unbounded churn across a
fleet of versions). agentctl reads the version **from the manifest** (§8), not
from `/metrics`. The `/metrics` body is pure exposition.

---

## 5. The exit-code contract (around RFC 0011 §5)

**RFC 0011 §5 owns the exit-code table. This section does not reproduce or
redefine it.** It specifies the *contract* a control plane needs around it.

### 5.1 Freeze + version

The RFC 0011 §5 table is a **frozen public API** (RFC 0011 §5 already says so).
This RFC pins it a version, `exit_codes`, surfaced in the manifest at
`surfaces.exit_codes` (RFC 0014 §5 shows `"exit_codes": "RFC-0011-§5"`; this RFC
makes that a `major.minor`, e.g. `"exit_codes": "1.0"`, with the RFC reference as
prose). Any change to the table's code→meaning mapping is **breaking** and bumps
the `exit_codes` major; agentctl refuses to compile `podFailurePolicy` rules for
a major it does not understand.

### 5.2 `podFailurePolicy` intent (the mapping agentctl compiles)

Each RFC 0011 §5 code carries a **control-plane intent** — what a
`podFailurePolicy` *should* do — so agentctl can mechanically emit `onExitCodes`
rules. agent emits the code; agentctl owns the actual `FailJob`/`Ignore`/`Count`
choice and any operator override.

| Code (RFC 0011 §5) | Name | Intent | agentctl `podFailurePolicy` hint |
|---|---|---|---|
| `0` | `EXIT_OK` | **complete** | not a failure; do not retry |
| `1` | `EXIT_FAILURE` | **retriable** | `Count` (let `backoffLimit` retry) |
| `2` | `EXIT_USAGE` | **terminal** | `FailJob` — config error, retry never helps |
| `3` | `EXIT_PARTIAL` | **policy** | default `Count`; operator may `FailJob` via `--budget-exit-code` (RFC 0011 §5.2) |
| `4` | `EXIT_INTELLIGENCE` | **retriable** | `Count` — usually transient upstream |
| `5` | `EXIT_SEMANTIC` | **terminal** | `FailJob` — deterministic refusal, retry never helps |
| `6` | `EXIT_MCP` | **retriable** | `Count` — sidecar may be racing up |
| `7` | `EXIT_BUDGET` | **policy** | default `Count`; operator may remap (RFC 0011 §5.2) |
| `124` | `EXIT_TIMEOUT` | **policy** | deadline tripped; default `Count` |
| `137` | `128+SIGKILL` (OS) | **infra** | OOM/kubelet kill — *raise memory limit*, not a retry knob |
| `143` | `128+SIGTERM` (OS) | **infra** | ungraceful SIGTERM exit — see §5.3 |

`terminal` ⇒ agentctl emits `onExitCodes … operator:In … action:FailJob`.
`retriable` ⇒ left to `backoffLimit` (`Count`). `policy` ⇒ agentctl's default is
`Count` but the operator's `--budget-exit-code` remap (RFC 0011 §5.2) is honoured
when present. `infra` codes are never authored as a retry rule — they signal a
*resource/config* fix (memory, grace period), surfaced as a distinct alert.

### 5.3 OS-set 137/143 semantics (the clean-drain distinction)

`137` and `143` are **kernel-set** (`128 + signo`); agent never returns them
(RFC 0011 §5.1 — `ExitCode` enum tops out at `124`). The control-plane-critical
invariant, owned by RFC 0011 §4: **a clean SIGTERM drain returns `0`, not `143`**.
Therefore:

- **`0` after SIGTERM** = the drain completed within `AGENT_DRAIN_TIMEOUT`
  (RFC 0011 §4.2). A rolling `Deployment` update must look like `0` in agentctl
  dashboards, paired with `agent_drains_total{phase="completed"}`.
- **`143`** = SIGTERM forced *past* the drain budget (the kubelet's own SIGKILL,
  or our force-collapse) — ungraceful, paired with
  `agent_drains_total{phase="forced"}`. agentctl alerts on `143` (and on
  `agent_drains_total{phase="forced"} > 0`) because it means
  `terminationGracePeriodSeconds` is too tight relative to `AGENT_DRAIN_TIMEOUT`
  (RFC 0011 §3.3) — a config fix, not a retry.
- **`137`** = OOM or kubelet hard-kill. agentctl maps it to "raise
  `resources.limits.memory`," distinct from any retry rule.

These three are surfaced as metrics (`agent_drains_total`,
`agent_reactor_stalls_total`, and the kernel exit code via the report's
`exit_code`) so an agentctl alert never has to parse pod-termination reasons by
hand.

### 5.4 Mode reachability (which codes appear where)

Restated from RFC 0011 §7 so agentctl authors per-shape policies:

- **`once`/`Job`** reaches `0/1/3/4/5/6/7/124` (+ kernel `137`). The report (§6)
  carries the precise terminal status; the exit code is the coarse projection.
- **`reactive`/`Deployment`** reaches **only** `0/143` + fatal `4/6/137`. A
  reactive daemon that exits `5` or `7` is a contract violation agentctl should
  flag (it should never refuse-to-exit on a single reaction — RFC 0011 §5.2).
  No run report is emitted for a reactive daemon (§6); its outcomes are in
  metrics + events.

---

## 6. Run-outcome reports

### 6.1 Why a report and not "just the exit code"

The exit code is a coarse, lossy projection of a rich terminal status (RFC 0011
§5.2 collapses nine `TerminalStatus` variants + partials into ten codes).
`kubectl agents results` needs the *full* outcome: which terminal status, how
many tokens/steps, how long, and a pointer to the distilled result. A `Job`'s
pod is gone seconds after it exits, so the outcome must be captured to a
**durable, machine-readable** place at exit, not inferred from a vanished pod.

### 6.2 The report object (frozen schema, versioned with `report_schema`)

```jsonc
{
  "report_schema": "1.0",                 // bumped on breaking field change (§8)
  "run_id": "01J8Z3K2Qn7…",               // RFC 0010 run_id (the unit of work)
  "instance": "pod-abc",                  // downward-API identity when present (RFC 0014 §5)
  "mode": "once",                         // once | loop | schedule (never "reactive" — §6.4)
  "status": "completed",                  // RFC 0007 §3.4 terminal-status string (the authority)
  "exit_code": 0,                         // RFC 0011 §5 code (the coarse projection of `status`)
  "has_usable_partial": false,            // drives the 3-vs-7 split (RFC 0011 §5.2)
  "usage": {
    "tokens_in": 9412233,
    "tokens_out": 412044,
    "steps": 37,                          // loop steps across the tree
    "subagents": 4                        // total spawned in the run
  },
  "duration_ms": 84213,
  "started_at": "2026-06-27T10:00:00.012Z",
  "ended_at":   "2026-06-27T10:01:24.225Z",
  "distillate_ref": "agent://subagent/0/result",  // RFC 0005 §3.3 — where the result body lives
  "trace_id": "4bf92f3577b34da6a3ce929d0e0e4736",   // RFC 0010 §3.6 — stitch to the trace
  "refusals": { "trifecta": 0, "rate": 0, "budget": 0, "depth": 0, "mcp": 0 }  // §4.3 reasons, this run
}
```

Field rules:

- **`status` is the RFC 0007 §3.4 string, not a synonym.** `exit_code` is the
  RFC 0011 §5 projection. They are *both* present so agentctl can show the
  precise status and still author exit-code policy.
- **`distillate_ref` points; it does not embed.** The result *body* is the
  distilled return (RFC 0007 §3.9 / RFC 0005 §3.3 `agent://subagent/{handle}/result`).
  The report stays small and bounded; the body is read on demand. For a one-shot
  CLI run the body is also on **stdout** (RFC 0010 §2: stdout = the agent's
  result); `distillate_ref` is the structured handle for fleet readers.
- **`refusals` is a per-run roll-up** of the §4.3 reasons (the metric counters
  are fleet-cumulative; the report is this-run), so `kubectl agents results`
  shows "this run hit 1 depth refusal" without a metrics query.
- Secrets never appear (RFC 0010 §3.4 allowlist applies); the report carries
  hashes/counts/refs, never raw content.

### 6.3 Where it is written (`--report-file` + the resource)

Two delivery surfaces, both optional, both off for a bare CLI run:

1. **`--report-file PATH`** (env `AGENT_REPORT_FILE`; a new config row that
   slots into RFC 0011 §3.2's table). On reaching a terminal status, the
   supervisor writes the §6.2 object via an **atomic write** (temp + `rename`,
   the same primitive RFC 0010 §3.7 uses for the health file) so a reader never
   sees a torn file. agentctl mounts an `emptyDir`/PVC at `PATH` and reads it
   after the pod terminates (or a node-agent reads it over vsock). Written
   **once**, at the terminal transition, *before* the `proc.exit` log line.
2. **`agent://run/{run_id}`** (RFC 0005 §3.3 — *already* a declared resource
   carrying "run-level status, mode, root handle, aggregate usage,
   exit-disposition"). This RFC **freezes that resource's body to the §6.2
   schema** and emits a final `notifications/resources/updated` on it at the
   terminal transition, so a still-connected agentctl reader (vsock mgmt
   profile) learns the outcome without a file. For a `once` run the resource is
   served only while the process is alive; the durable copy is `--report-file`.

A run report is **idempotent to read** and carries the same `run_id` across a
retried Job (when the operator sets a stable `AGENT_RUN_ID`, RFC 0011 §6), so
agentctl can collapse a retried unit's reports.

### 6.4 Reactive daemons emit no final report

A `reactive` `Deployment` has **no single terminal outcome** — it processes an
unbounded stream and exits only on drain (`0`) or fatal infra (RFC 0011 §5.4). It
therefore writes **no** `--report-file`. Its per-reaction outcomes are in (a) the
metrics (`agent_runs_total{status}` increments per reaction), and (b) the event
stream (§7). `kubectl agents results` for a reactive workload reads the event
stream / metrics, not a report file. Attempting `--report-file` with `--mode
reactive` is a **config warning** (not a hard error) at startup (RFC 0011 §3.3
validate step) — the flag is simply inert.

---

## 7. The `agent://events` stream

### 7.1 Purpose and the anti-goal

agentctl wants to **tail live activity** — loop steps, refusals, spawns, drain
progress — across a fleet, over the same self-MCP channel it already uses for
control (RFC 0015), **without** scraping each pod's container stderr (which on a
vsock-only pod, RFC 0014 §2, it cannot even reach). The anti-goal is equally
firm: this is **not a second telemetry path**. `agent://events` is a *projection
of the same stderr stream* — identical lines, identical closed vocabulary (RFC
0010 §3.2/§3.3) — surfaced as a subscribable resource. stderr remains the source
of truth and the durable record; `agent://events` is the live-tail convenience.

### 7.2 The resource (RFC 0005 scheme, notify-then-read, bounded ring)

`agent://events` is a read-only `agent://` resource (RFC 0005 §3.3 scheme)
added to the served resource list, gated behind `serve-mcp` + a new `events`
feature. agent's reactive substrate is **notify-then-read** (RFC 0005 §3.3: the
notification carries *no payload*; the peer `resources/read`s to learn new
state). Because a live event stream is a *sequence* (not a single current-state
value), the read returns a **bounded window from an in-memory ring**, and the
subscriber drives a cursor:

- **Backing store:** a fixed-size in-memory ring buffer of the last **N** emitted
  log lines (default `AGENT_EVENTS_RING = 1024`, env-tunable). It is the same
  `serde_json::Value` lines written to stderr, captured into the ring as they are
  emitted. Bounded ⇒ no unbounded memory growth on a slow subscriber; **lossy by
  design** — an overrun drops the oldest and bumps a `dropped` counter the read
  surfaces (the subscriber learns it fell behind and can re-baseline). This is the
  reactive-substrate caveat the prompt names: events may need a bounded ring the
  subscriber reads, precisely because notify-then-read is current-state, not a
  durable queue.
- **Notify:** on each new event (or a small coalescing batch), emit
  `notifications/resources/updated{uri:"agent://events"}` to subscribed peers
  (RFC 0005 §3.3). No payload in the notification.
- **Read:** `resources/read("agent://events?after=<seq>")` returns the ring
  slice with `seq > after`, plus the current `dropped` count and the ring's
  oldest/newest `seq`. The subscriber advances `after` to the last `seq` it saw.
  This is the standard MCP cursor pattern (RFC 0004 pagination), reused — **no new
  protocol** (RFC 0014 §3.2).

```jsonc
// resources/read("agent://events?after=4821") result body
{
  "events_schema": "1.0",                 // bumped only on a breaking envelope change (§8)
  "oldest_seq": 3801, "newest_seq": 4840, // ring window currently held
  "dropped": 0,                           // lines evicted since last read (subscriber fell behind if >0)
  "events": [
    // each entry is an RFC 0010 §3.2 line VERBATIM, plus a monotonic `seq`:
    {"seq":4822,"ts":"2026-06-27T10:00:01.5Z","level":"info","event":"loop.step",
     "run_id":"01J…","agent_id":"01J…c","agent_path":"0.2","comp":"agent","pid":1457,
     "step":12,"tokens_in":880,"tokens_out":40},
    {"seq":4823,"ts":"2026-06-27T10:00:01.6Z","level":"warn","event":"limit.exceeded",
     "run_id":"01J…","agent_id":"sup","agent_path":"0","comp":"supervisor","pid":1421,
     "limit":"spawn_rate","value":12,"cap":10}
  ]
}
```

The event entries are RFC 0010 §3.2 lines **unchanged** (the only added field is
the ring `seq`); the `event` strings are the RFC 0010 §3.3 closed vocabulary
unchanged. This RFC introduces **no new event names** — adding one is RFC 0010's
job (cheap, additive). The `events_schema` versions only the *envelope*
(`oldest_seq`/`newest_seq`/`dropped`/`events` shape), not the line schema (RFC
0010 owns that and versions it via its own breaking-change rule).

### 7.3 Filtering, fan-out, and what stays out

- **Server-side filter (bounded, optional):** the read may carry
  `?level=warn` or `?event=subagent.,limit.,security.` (comma-list of dotted
  prefixes) so a subscriber tailing only security/lifecycle events does not pull
  every `loop.step`. The filter is a cheap prefix match over the ring; no query
  engine.
- **Fan-out is the subscriber's job.** agent serves *one instance's* ring.
  Cross-instance aggregation, a fleet event bus, long-term storage, and replay
  beyond the ring are **agentctl's** (RFC 0014 §6 non-goals). agent never
  buffers beyond N lines and never ships events anywhere — it serves a read.
- **No new transport.** The resource rides the existing self-MCP over unix/vsock
  (and future HTTP, RFC 0013); a vsock-only pod's node-agent (RFC 0014 §2) tails
  it host-side. No async runtime, no streaming framework.

### 7.4 Relationship to `--aggregate-logs`

RFC 0010 §3.5 mode B (`--aggregate-logs`) forwards *child* telemetry **up to the
supervisor's stderr** for single-stream capture. `agent://events` is orthogonal
and complementary: mode B is about *getting the whole tree onto one stderr*;
`agent://events` is about *exposing that one stream as a subscribable resource*.
With both on, the supervisor's ring already contains the forwarded child lines,
so a subscriber sees the full tree over `agent://events` with no extra wiring.

---

## 8. Failure semantics & versioning

### 8.1 The three coupling points and their versions

agentctl couples to three frozen surfaces this RFC owns or co-owns; each carries
an independent version, surfaced in the manifest (RFC 0014 §5):

| Surface | Manifest key | Owner | Breaking change ⇒ |
|---|---|---|---|
| Metrics schema | `surfaces.metrics_schema` | this RFC §4 | bump `metrics_schema` major |
| Exit-code table | `surfaces.exit_codes` | RFC 0011 §5 | bump `exit_codes` major |
| Run report | `surfaces.report_schema` | this RFC §6 | bump `report_schema` major |
| Event envelope | `surfaces.events_schema` | this RFC §7 | bump `events_schema` major |

### 8.2 Additive-within-major, break-bumps-major (binding)

**Additive (minor bump, agentctl keeps working):**
- a **new** metric series, or a **new** bounded label *value* within an existing
  closed domain (e.g. a new `refusals_total{reason=…}`, a new
  `intel_errors_total{reason=…}`);
- a **new** field in the run report or the event envelope;
- a **new** event name (RFC 0010's additive rule) appearing in the stream.

**Breaking (major bump, agentctl branches or refuses):**
- a **removed or renamed** metric, or a **removed or renamed** label *key*;
- a **removed or renamed** report/envelope field;
- a change to the `status` closed domain that drops/renames a value (RFC 0007 §3.4
  — adding a status is additive, *removing/renaming* one is breaking and bumps
  `metrics_schema` because it changes the `agent_runs_total{status}` label
  domain agentctl built a dashboard on);
- any change to the RFC 0011 §5 exit-code→meaning mapping (bumps `exit_codes`).

### 8.3 How agentctl branches

agentctl reads the manifest **first** on every instance (RFC 0014 §5/§7). For
each surface it compares the major it understands against the advertised major:

- **major match** ⇒ drive normally; tolerate unknown *additive* series/fields
  (forward-compatible — ignore what it does not recognise).
- **advertised major newer** ⇒ degrade: scrape the metrics it still recognises,
  honour the exit codes it knows, skip unknown report fields; **refuse** to author
  a `podFailurePolicy` against an `exit_codes` major it does not understand
  (RFC 0014 §7 graceful-degradation posture).
- **surface absent** (`surfaces.metrics_schema` missing, `events:false`, etc.)
  ⇒ the binary did not build that feature; agentctl manages what remains
  (liveness + exit code + logs), exactly as RFC 0014 §7 specifies.

### 8.4 Telemetry-path failure semantics (agent never dies for telemetry)

Inherited from RFC 0010 §3.1 and made explicit for the new surfaces:

- **A stderr/log write error is swallowed** (best-effort); telemetry never takes
  down the supervisor. SIGPIPE is ignored (RFC 0011 §4.1 / RFC 0003 §3.1).
- **The events ring is lossy and bounded**, never blocking. A slow/dead
  subscriber cannot back-pressure the supervisor; it simply drops oldest and
  increments `dropped` (§7.2). The ring is dropped wholesale on a SIGKILL — it is
  in-memory, like all v1 supervisor state (RFC 0011 §7); the durable record is
  stderr + `--report-file`.
- **The report write is best-effort-but-loud:** if the atomic write to
  `--report-file` fails (disk full, RO mount), the supervisor logs `report.write.fail`
  (a `warn` line) and **still exits with the correct exit code** — the exit code
  is the floor contract (RFC 0011 §5) and never depends on the report landing.
- **`/metrics`, `agent://events`, and `--report-file` are side-effect-free with
  respect to the run** — disabling any of them changes nothing about the agentic
  result, only what a fleet reader can see (RFC 0010 §3.7 side-effect-free
  principle).

---

## 9. Correlation across a multi-pod flow (RFC 0010 §3.6, restated)

agentctl-driven flows span pods: a reactive pod spawns work that hands off (over
MCP) to a `Job` pod another agentctl reconcile created. **RFC 0010 §3.6 already
makes these one trace** — this RFC adds nothing, it points agentctl at the
existing fields:

- **Ingest (mint-or-adopt).** agent adopts an inbound `traceparent` — on an
  inbound self-MCP request (RFC 0005, the surface agentctl drives) or via
  `AGENT_TRACEPARENT` when agentctl starts the pod — else mints one per `run_id`
  (RFC 0010 §3.6). So agentctl sets `AGENT_TRACEPARENT` (or the inbound `_meta`)
  once at the flow root and **every downstream pod's trace stitches in**.
- **Propagate.** Every outbound MCP call carries `_meta.traceparent`, the LLM
  call carries the `traceparent` header, and the spawn payload carries
  `{trace_id, parent_span_id}` (RFC 0010 §3.6) — so a hand-off from pod A to pod
  B over an MCP backing service continues the same `trace_id`.
- **Surface.** `trace_id` is in every log line (RFC 0010 §3.2), in the run report
  (§6.2 `trace_id`), and in each event-stream entry (§7.2). agentctl renders "all
  pods in this flow" by `trace_id`; subtree scoping within a pod is `agent_path`
  prefix (RFC 0010 §3.2), no backend join.

Span **export** stays gated behind `otel` (RFC 0010 §3.9); propagation is
on-by-default and dependency-free. agentctl gets cross-pod correlation in the
default build, with no OTLP collector required (it can correlate logs/reports by
`trace_id` alone).

---

## 10. Liveness for the fleet (RFC 0010 §3.7, restated as intent)

The control-plane requirement: **a live PID is not a live agent.** A reactive
daemon idles for hours by design (RFC 0010 §1); k8s must restart a *wedged*
instance but must **not** restart a healthy-idle one or a healthy tree with one
stuck child. RFC 0010 §3.7 already owns the surface; this RFC states only the
fleet intent and the mapping to k8s probes:

- **`/healthz` (liveness)** = the supervisor *reactor heartbeat* (RFC 0010 §3.7:
  `last_loop_tick` advances on every wake, including idle `recv_timeout`
  expiries). Stale ⇒ the **reactor itself is wedged** ⇒ `503` ⇒ k8s restarts the
  pod. agentctl wires a `livenessProbe` to `/healthz`. The wedged-reactor trip is
  also a metric, `agent_reactor_stalls_total` (§4.3), so an alert fires even
  where the probe's restart masks it.
- **A stuck *subagent* must NOT flip `/healthz`** (RFC 0010 §3.7, RFC 0003). The
  supervisor detects it (RFC 0003's 3-detector model + EOF×pong classifier) and
  kills it (the kill ladder), emitting `subagent.stuck` →
  `agent_subagent_stuck_kills_total{signal}` (§4.3). The pod stays live; failing
  liveness here would destroy a whole healthy tree for one wedged leaf. agentctl
  alerts on the stuck-kill *metric*, it does not restart the pod.
- **`/readyz` (readiness)** = `proc.ready` reached and declared subscriptions
  reconciled (RFC 0010 §3.7). agentctl wires a `readinessProbe` to `/readyz` and
  flips it to not-ready on drain (RFC 0011 §4.2 step 1) so a rolling update stops
  routing before teardown.
- **Mode-awareness** (RFC 0010 §3.7): `once` has no liveness probe (the run *is*
  the readiness; exit code is the whole signal). agentctl wires probes only for
  `loop`/`reactive`. The HTTP `/healthz`/`/readyz` surface is opt-in
  (`--health-http`); the default daemon surface is the `--health-file` exec probe
  (RFC 0010 §3.7) — agentctl chooses per its probe style.

No new health mechanism is introduced here. This section is the contract *reading*
of RFC 0010 §3.7 for a fleet operator.

---

## 11. Config additions (slot into RFC 0011 §3.2)

This RFC adds exactly the rows below to RFC 0011 §3.2's canonical config table.
All are env-settable (12-factor III); flag overrides env overrides file overrides
default (RFC 0011 §2.1). All are off by default for a bare one-shot CLI run.

| Concern | Env | Flag | Notes |
|---|---|---|---|
| Run report file | `AGENT_REPORT_FILE` | `--report-file PATH` | atomic write at terminal status (§6.3); inert for `reactive` (warn) |
| Events ring size | `AGENT_EVENTS_RING` | `--events-ring N` | default 1024; bounds `agent://events` memory (§7.2) |
| Events surface | `AGENT_SERVE_EVENTS` | (implied by `--serve-mcp` + `events` feature) | gates `agent://events` (§7.2) |

`/metrics`, `/healthz`, `/readyz`, `--health-file`, `--health-http`,
`AGENT_TRACEPARENT`, and `OTEL_EXPORTER_OTLP_ENDPOINT` are **already** RFC 0010
config rows — not re-added here. `--budget-exit-code` (the §5.2 `policy` override)
is **already** an RFC 0011 §5.2 row.

---

## 12. Non-goals (these stay in agentctl)

- **Dashboards, alert-rule YAML, recording rules.** agent freezes the metric
  names; the Grafana/Prometheus rule objects are agentctl's (RFC 0014 §6).
- **HPA/KEDA scaler objects, target values, scale policies.** agent exposes
  `agent_pending_events`/`agent_reaction_lag_ms` (§4.3); the scaler that reads
  them is RFC 0019 / agentctl.
- **`podFailurePolicy` documents.** agent freezes the exit-code intent (§5.2);
  the compiled `onExitCodes` rules and any operator override are agentctl's.
- **A cost/price table.** agent emits `agent_tokens_total`; tokens × price =
  cost is agentctl's table (no pricing in the data plane, §4.3).
- **Cross-instance aggregation, a fleet event bus, long-term metric/trace/event
  storage, replay beyond the ring.** RFC 0014 §6 non-goals; agent serves one
  instance's metrics/ring/report.
- **An OTLP collector / metrics backend.** Span/metric *export* is the `otel`
  feature pushing to a sidecar (RFC 0010 §3.9); the collector is infra agentctl
  provisions.
- **A new telemetry mechanism.** Everything here projects RFC 0010's existing
  log/event/health/trace primitives. No `tracing`, no metrics crate, no async
  runtime in the default or cloud-native build (RFC 0014 §3.3).

---

## 13. Rollout & compatibility

- **Purely additive, fully feature-gated.** The metrics freeze is a *contract
  statement* over series RFC 0010 §3.8 already defined — no code change beyond
  emitting the new `agent_pending_events`/`agent_reaction_lag_ms`/`agent_intel_up`
  /`agent_mcp_up`/`agent_drains_total` gauges/counters under the existing
  `metrics` feature. `--report-file` and `agent://events` are new, each behind a
  gate (`metrics` is unaffected; `events` is new; the report rides the default
  serializer). A binary that builds none of them reports them absent in the
  manifest and agentctl degrades (§8.3).
- **Versions start at `1.0`.** `metrics_schema`, `report_schema`, `events_schema`
  all `1.0`; `exit_codes` `1.0` (referencing RFC 0011 §5). All surfaced in the
  manifest (§8.1), all move additively within major, all bump major on
  removal/rename (§8.2).
- **Lands in the track after RFC 0015** (which unlocks `--serve-mcp` + the
  manifest the version surfacing depends on, RFC 0014 §4/§7). The metrics freeze
  and exit-code contract can land independently of `agent://events` (which needs
  the served surface); the run report needs only the existing supervisor exit
  path.

---

## 14. References

- **RFC 0003** — process supervision & recovery: owns the 3-detector stuck model
  + EOF×pong classifier (§10 surfaces the wedged-reactor verdict) and
  rebuild+reconcile (the `agent_restarts_total` source).
- **RFC 0004** — MCP client subset & codec: the wire/codec the `agent://events`
  notify-then-read frames and the cursor pagination reuse.
- **RFC 0005** — self-MCP server & control protocol: owns the `agent://` scheme,
  notify-then-read semantics, and `agent://run/{run_id}`; `agent://events` is a
  resource added to that surface.
- **RFC 0006** — intelligence transport & wire: source of the `usage` that feeds
  `agent_tokens_total` and of the intelligence-health signals.
- **RFC 0007** — agentic loop & terminal status: **sole authority** for the
  `TerminalStatus` closed set used as `report.status` and the
  `agent_runs_total{status}` label domain.
- **RFC 0008** — execution modes & reactive routing: defines the reactive
  pending/in-flight/backlog state the §4.3 scaling gauges measure.
- **RFC 0009** — subagent process model: source of the tree gauges (depth/breadth/
  active) and the depth/rate refusals (`refusals_total{reason}`).
- **RFC 0010** — observability, health & telemetry: **owns** the log/event schema,
  the metrics exposition, health, and trace propagation. This RFC freezes + exposes
  them; it does not redefine them.
- **RFC 0011** — cloud-native contract: **owns** the exit-code table (§5), config
  precedence, signals, and drain choreography. This RFC freezes the exit-code
  *contract* and adds `--report-file` to the config table.
- **RFC 0012** — security posture: the trifecta/Rule-of-Two refusal feeding
  `refusals_total{reason="trifecta"}`; the secrets allowlist applied to reports.
- **RFC 0013** — deferred v2 surface: HTTP serving (an alternative transport for
  `/metrics` and `agent://events`); deferred session checkpointing.
- **RFC 0014** — control-plane contract (umbrella): the data/control-plane split,
  the capabilities manifest (§5) this RFC versions surfaces in, and the
  primitives-not-policy / freeze-and-version principles (§3).
- **RFC 0015** — management & control surface (sibling): the `--serve-mcp` profile
  + manifest this RFC's `agent://events` resource and version surfacing slot into.
- **RFC 0018** — intelligence transport resilience (sibling): consumes the frozen
  `agent_intel_up`/`agent_intel_errors_total` this RFC pins.
- **RFC 0019** — horizontal scaling (sibling): consumes the frozen
  `agent_pending_events`/`agent_reaction_lag_ms` scaling signal set (§4.3).
