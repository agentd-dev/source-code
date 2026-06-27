# RFC 0014: agentd as a managed workload — the control-plane (agentctl) contract

**Status:** Proposed (agentctl control-plane track)
**Author:** Andrii Tsok
**Date:** 2026-06-27
**Part of:** the agentd rewrite — extends the cloud-native contract (RFC 0011); umbrella for RFCs 0015–0019

---

## 1. Problem / Context

`agentd` is the **data plane**: a single static binary that takes an instruction
+ tools (MCP) + an intelligence endpoint and runs the agentic loop, supervised
and bounded. A separate project — **`agentctl`** (a CLI, a `kubectl` plugin
exposing `kubectl agent[s] …`, and a Kubernetes operator) — is the **control
plane**: it provisions agentd instances into a cluster, supplies intelligence by
mounting **vsock** to a host-side model service, scales the fleet, and collects
monitoring / observes / manages the running instances.

RFC 0011 §1 deliberately scoped the orchestrator **out**: *"composition is MCP,
not a control plane we own"* (assessment §2.11). This RFC track **does not revoke
that boundary — it sharpens it.** agentd still does not become a control plane,
does not learn about Kubernetes, CRDs, scheduling, or dashboards, and grows no
cluster dependency. What it gains is a **clean, frozen, machine-readable contract
surface** so that an external control plane can drive a *fleet* of agentd
instances correctly. The asymmetry is the whole design:

> **agentd exposes primitives; agentctl owns policy.** Every capability in this
> track is a primitive (a manifest to read, a resource to subscribe, a metric to
> scrape, a config to reload, a signal to honour) reachable over agentd's
> existing transports. The Kubernetes-facing translation — CRDs, the operator
> reconcile loop, the `kubectl` plugin, autoscalers, Grafana dashboards, alert
> rules — lives entirely in `agentctl`.

This RFC is the umbrella: it states the architecture, the boundary, the shared
spine (the capabilities manifest + contract versioning every sub-RFC keys off),
and indexes the five concrete contracts that follow.

---

## 2. The data-plane / control-plane split

```
        ┌──────────────────────────  agentctl (separate repo)  ──────────────────────────┐
        │  kubectl agent[s] …  │  agentctl CLI  │  k8s operator (CRDs: Agent / AgentFleet) │
        └───────────────┬───────────────────────────────┬──────────────────────────────────┘
                        │ kube-apiserver proxy           │ reconcile → Pods/Deployments/Jobs
                        ▼                                 ▼
        ┌─────────────────────  agentctl node-agent (DaemonSet, per node)  ─────────────────┐
        │   talks to each local agentd pod over **vsock** — control + telemetry             │
        └───────────────────────────────────┬──────────────────────────────────────────────┘
              vsock (mgmt: serve-mcp)  ▲      │   ▲  vsock (intelligence: dial-out)
                                       │      ▼   │
        ┌──────────────────────────────┴──────────┴───────────────────────────────────────┐
        │   agentd pod  (data plane)  — may run with NO cluster network at all             │
        │   · serves its self-MCP over vsock (mgmt profile)  · dials intelligence over vsock│
        └──────────────────────────────────────────────────────────────────────────────────┘
```

The load-bearing idea is to make **vsock bidirectional**. agentd already dials
*out* over vsock for intelligence (RFC 0006). If agentd can also **serve** its
self-MCP over vsock, then an `agentctl` node-agent DaemonSet manages every agentd
pod on its node host-side, over vsock, for both control and telemetry — and the
agentd pod can run with **no cluster networking at all** (vsock-out for the model,
vsock-in for management). That is the strongest isolation posture agentd can
offer and the natural backend for `kubectl agent`. Cluster-network serving (TCP /
Streamable-HTTP, RFC 0013) remains an alternative transport for the same surface,
not a precondition.

---

## 3. Decision — four principles

1. **Primitives, not policy.** agentd ships read/subscribe/reload/signal
   primitives. agentctl composes them into reconciliation, scaling, and UX. No
   Kubernetes concept enters agentd.

2. **Reuse MCP; invent no new protocol.** The management surface is a *profile*
   of the existing self-MCP (RFC 0005) — more `agentd://` resources and a small
   set of operator-facing tools — served over the existing transports (unix /
   vsock / future HTTP). Work-distribution reuses MCP resources + a claim tool,
   not a bespoke queue. One dialect, everywhere.

3. **Hold the minimalism moat.** Every surface here is **feature-gated and
   dependency-free** (the default build stays serde + serde_json + libc; the
   cloud-native image set stays dep-free). A control-plane feature that would
   pull an async runtime, a k8s client, or a TLS/gRPC stack is wrong by
   construction — it belongs in agentctl.

4. **Freeze and version what agentctl builds against.** A control plane couples
   tightly to three things: the **capabilities manifest** (§5), the **metrics
   schema** and **exit-code table** (RFC 0016, RFC 0011 §5), and the **management
   tool/resource names** (RFC 0015). These are a public API: each carries a
   `contract_version`, changes additively within a major, and breaking changes
   bump the major. agentctl negotiates on the manifest's version.

---

## 4. The track (sub-RFCs)

| RFC | Contract | What agentctl gets |
|---|---|---|
| **0015** Management & control surface | `--serve-mcp vsock:PORT`; the operator MCP profile (`agentd://capabilities` / `inventory` / `events`; tools `drain`, `lame-duck`, `pause`/`resume`, `cancel`, `attach`→`subagent.send`); instance identity from the downward API | the backend for `kubectl agent <x> describe / tree / logs / attach / drain` over vsock |
| **0016** Telemetry & lifecycle contract | the **frozen** Prometheus metrics schema, the versioned exit-code table (extends RFC 0011 §5 / RFC 0010), machine-readable run-outcome reports, the `agentd://events` stream, and stuck-detector → `/healthz` liveness | standard dashboards, alert rules, `podFailurePolicy`, cost/quota aggregation, `kubectl agents results/top` |
| **0017** Declarative config & hot reload | a declarative config file (lands the RFC 0011/0013 file layer), `agentd --validate-config` + a JSON-schema export, `SIGHUP`/file-watch **hot reload** (MCP servers / subscriptions / model), file-based secret refs | ConfigMap-driven, admission-validated, restart-free reconfiguration |
| **0018** Intelligence transport resilience | multi-endpoint failover (`--intelligence vsock:a,vsock:b`), endpoint health as a metric/resource, **runtime model/endpoint hot-swap**, an optional model-discovery handshake | survive a host model-service move/upgrade with no restart; model-aware placement |
| **0019** Horizontal scaling | work-**claim/lease** on reactive events (so N replicas don't double-process), `--shard K/N` partitioning, the autoscaling signal set, an optional warm-pool/standby mode | KEDA/HPA-driven horizontal scale of reactive workers; warm fast-start |

**Durability / checkpoint-restore** for long-lived stateful fleet agents (resume
a warm session/run after a pod reschedule, rather than re-trigger idempotently)
is the highest-effort item and remains tracked as the **deferred** "MCP-backed
session checkpointing" line in **RFC 0013**; the control-plane track references it
but does not pull it forward.

---

## 5. The shared spine: the capabilities manifest

Everything in this track hangs off one primitive — a **machine-readable
capabilities manifest** an agentd instance reports, both as a one-shot
(`agentd --capabilities` → stdout JSON, exits 0) and as a live resource
(`agentd://capabilities` over the served self-MCP). It is how agentctl discovers
what an instance is, what it can do, and which contract versions it speaks:

```jsonc
{
  "contract_version": "1.0",            // the agentctl↔agentd contract major.minor (§3.4)
  "agentd_version": "2.1.0",            // CARGO_PKG_VERSION
  "build_features": ["metrics","serve-mcp","cron","otel","vsock"],
  "identity": {                          // from the k8s downward API env when present
    "run_id": "01J…", "instance": "pod-abc", "node": "n3", "namespace": "agents"
  },
  "mode": "reactive",
  "model": "claude-opus-4",
  "intelligence": { "transport": "vsock", "endpoints": 2, "healthy": true },
  "mcp_servers": [ { "name": "fs", "tags": ["untrusted_input"] } ],
  "limits": { "max_depth": 4, "max_tokens": 2000000, "max_total_subagents": 64 },
  "surfaces": {                          // which contracts this binary actually serves
    "management": true, "metrics": ":9090", "events": true, "hot_reload": true,
    "exit_codes": "RFC-0011-§5", "metrics_schema": "1.0"
  }
}
```

agentctl reads the manifest first on every instance, schedules work to instances
whose `build_features` / `model` / `mcp_servers` fit, renders `kubectl agents get
-o wide`, and refuses to drive an instance whose `contract_version` major it does
not understand. The manifest's exact schema is owned by **RFC 0015 §capabilities**.

---

## 6. Ratified cross-cutting conventions

The sub-RFCs surface shared concerns that this umbrella settles once, so no two
of them drift. These are binding on RFCs 0015–0019.

### 6.1 Ownership map (one definition, one home)

A thing is **defined once**, by its owning RFC; others **name** it and defer.

| Surface | Owner | Others |
|---|---|---|
| Management transport (`--serve-mcp vsock:PORT`), the operator tool/resource **namespace** (every `agentd://…` operator resource and operator tool — incl. any `work.*`/`assign`/`reload` verb agentd *exposes*), the **capabilities-manifest schema**, instance identity, `PeerOrigin` gating | **0015** | sub-RFCs register new operator tools/resources by referencing 0015's namespace, never a parallel surface |
| The **frozen metric set** (every metric name/label/HELP), the **exit-code contract** framing around RFC 0011 §5, **run-outcome reports**, `agentd://events` | **0016** | other RFCs *name* a metric they emit; its definition lives in 0016 |
| The **config-file schema**, `--validate-config` / `--config-schema`, the **hot-reload** mechanism + the **reloadable-field allowlist**, file-based secret refs | **0017** | 0018 adds `intelligence`/`model` to the allowlist by reference |
| Intelligence multi-endpoint / failover / health / hot-swap / model-discovery | **0018** | — |
| Work-claim/lease + shard **semantics**, autoscaling **intent** | **0019** | metrics it scales on are defined in 0016; `work.*` are conventions on the *coordination* MCP server (agentd participates), not a server agentd runs |

### 6.2 `surfaces{}` is the single discovery point

The manifest's `surfaces{}` block (§5) is the **only** place an instance advertises
which control-plane contracts it serves. No sub-RFC adds a parallel discovery
mechanism. Its keys and owners: `management`,`operator_tools` (0015); `metrics`,
`metrics_schema`,`events`,`report_schema`,`exit_codes` (0016); `hot_reload`,
`config_validate`,`config_schema` (0017); `intelligence` (0018); `claim`,`shard`
(0019). A key absent ⇒ the surface is unbuilt/off ⇒ agentctl degrades gracefully
(§8).

### 6.3 Versioning

- **`contract_version`** (major.minor) is the overall agentctl↔agentd contract.
  **Additive** (a new optional manifest key, optional tool/resource, or new
  metric) ⇒ **minor**. **Breaking** (a removed/renamed key or tool, or changed
  semantics) ⇒ **major**. agentctl refuses an instance whose **major** it does
  not know; it reads minor + the sub-schema versions to branch.
- Sub-schemas version **independently** and are surfaced inside `surfaces{}`:
  `metrics_schema`, `report_schema` (0016), `config_schema` (0017). `exit_codes`
  names the RFC 0011 §5 table by its version tag (0011 owns the table; 0016 owns
  the *contract* around it). Same additive-minor / breaking-major rule each.

### 6.4 The Kubernetes env convention (downward API, read-only)

agentd reads these from the environment **only** — `valueFrom.fieldRef` the
operator injects — and **never** calls the kube API (no client, no in-cluster
config, no service-account read). All optional; descriptive, not load-bearing.

| Env | Source | Used by |
|---|---|---|
| `AGENTD_POD_NAME` | `metadata.name` | identity (0015) |
| `AGENTD_POD_UID` | `metadata.uid` | identity |
| `AGENTD_POD_NAMESPACE` | `metadata.namespace` | identity |
| `AGENTD_NODE_NAME` | `spec.nodeName` | identity |
| `AGENTD_POD_GRACE_SECONDS` | `= terminationGracePeriodSeconds` | drain (RFC 0011) |
| `AGENTD_SHARD` | `"K/N"` (from the StatefulSet ordinal) | sharding (0019) |

Per-endpoint intelligence credentials use one keying scheme across 0017/0018:
`AGENTD_INTELLIGENCE_TOKEN` (≡ endpoint 1), then `AGENTD_INTELLIGENCE_TOKEN_2`,
`_3`, … (1-indexed); each has a `…_FILE` variant for a mounted-secret path. The
config file never carries a credential (RFC 0011, RFC 0012).

---

## 7. Non-goals (these stay in agentctl)

- Kubernetes CRDs, the operator reconcile loop, leader election, the `kubectl`
  plugin, RBAC.
- Scheduling/placement decisions, autoscaler logic (HPA/KEDA scalers), bin-packing.
- Dashboards, alert-rule definitions, long-term metric/trace storage.
- Cross-instance aggregation, fleet-wide views, a fleet control database.
- Provisioning the vsock device / host model service (the node-agent's job);
  agentd only *uses* the vsock CID/port it is given.

agentd's contribution is to make each of those **cheap to build** by exposing the
right primitive — never to implement them.

---

## 8. Rollout & compatibility

- The track is **purely additive** and entirely behind existing/!new feature
  gates; a default `agentd` build is unchanged, and an instance that ships none
  of the control-plane surfaces simply reports them `false` in its manifest, so
  agentctl degrades gracefully (it manages what it can: liveness + exit codes +
  logs, exactly as today).
- `contract_version` starts at **1.0** and moves additively; agentctl and agentd
  negotiate on it. The metrics schema and exit-code table version independently
  but are surfaced in the manifest (§5) so a scraper/dashboard can branch.
- Each sub-RFC lands independently in milestone order (RFC 0015 first — it
  unlocks the transport + manifest the rest depend on), so agentctl can begin
  integrating against a partial, version-advertised surface.

---

## 9. References

- RFC 0005 — self-MCP server & control protocol (the surface this track profiles)
- RFC 0006 — intelligence transport & wire (the vsock client this track makes resilient)
- RFC 0008 — execution modes & reactive routing (what RFC 0019 scales)
- RFC 0010 — observability, health & telemetry (what RFC 0016 freezes)
- RFC 0011 — cloud-native contract (the exit-code/signal contract this extends)
- RFC 0012 — security posture (trifecta tags surfaced in the manifest)
- RFC 0013 — deferred v2 surface (HTTP serving; session checkpointing)
