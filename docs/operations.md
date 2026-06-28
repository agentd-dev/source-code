# Operations

`agentd` is one process running one agent, but a fleet of them is a *control
plane*. This page is for the operator (and the `agentctl` it drives): how to
talk to a running instance, the tools that steer it without restarting it, how a
controller discovers what an instance can do, and how to push a config change
into a live daemon.

Everything here rides one extra surface — the **management transport** — which
is off by default. A pure one-shot CLI run carries none of it. You opt in with
`--serve-mcp`, and the operator tools, the live control resources, and the
config-reload notifications come with it.

> **Status.** The management transport, the operator tools
> (`drain` / `lame-duck` / `pause` / `resume` / `cancel`), the capabilities
> manifest, and hot reload are implemented and tested behind their feature gates
> (`serve-mcp`, `hot-reload`, `config-watch`, `cluster`). The examples below
> describe shipped behaviour; feature-gated pieces are flagged inline. The
> control-plane contracts are owned by RFCs
> [0014](../rfcs/0014-control-plane-contract.md),
> [0015](../rfcs/0015-management-surface.md), and
> [0017](../rfcs/0017-config-validation-and-hot-reload.md).

---

## 1. The management transport

Two MCP surfaces speak the *same* dialect but live in different trust domains:

- **Stdio** — the process's own stdin/stdout. This is the driving harness (the
  parent that spawned this agentd, or the `subagent.*` control path). Every
  agentd serves the self-MCP over stdio always.
- **Management** — a peer that connected to a `--serve-mcp` listener. This is
  the operator / `agentctl` channel.

You arm the management transport with `--serve-mcp` (env `AGENTD_SERVE_MCP`):

| Form | Meaning | Needs |
|---|---|---|
| `--serve-mcp unix:/run/agentd.sock` | a unix-domain socket | `--features serve-mcp` |
| `--serve-mcp vsock:PORT` | vsock, wildcard CID (`VMADDR_CID_ANY`) | `--features serve-mcp,vsock` |
| `--serve-mcp vsock:CID:PORT` | vsock, explicit CID:port (a Firecracker/Kata guest) | `--features serve-mcp,vsock` |

```console
$ agentd \
    --instruction 'reconcile on change' \
    --intelligence unix:/run/intel.sock \
    --mode reactive --subscribe file:///data/desired.json \
    --serve-mcp unix:/run/agentd.sock
```

Both unix and vsock are served by the same blocking, thread-per-connection
listener (no async, no new framing) — vsock is "the unix server with the socket
type swapped". A peer on either is in the **Management** origin; the process's
own stdio is the **Stdio** origin.

### 1.1 The origin gate

The trust split is enforced by *transport origin*, not an in-band flag:

- **Operator tools** (`drain`, `lame-duck`, `pause`, `resume`, `cancel`) are
  listed and callable **only** to a Management peer. A Stdio peer (a spawned
  subagent driving its own loop) cannot even *see* them in `tools/list`, and a
  `tools/call` for one falls through to an unknown-tool error. So a subagent can
  never drain or pause its own supervisor.
- The **operator-facing resources** (`agentd://inventory`,
  `agentd://intelligence`, `agentd://config/effective`, `agentd://capacity`,
  `agentd://events`) are likewise Management-only — listed, readable, and
  subscribable only from the management transport. A Stdio read of one 404s like
  any unknown URI.
- The **base** self-MCP surface (the `subagent.*` tools, `status`,
  `agentd://status`, `agentd://capabilities`, `agentd://run/<id>`,
  `agentd://subagent/<handle>`) is readable on *every* origin.

The capabilities manifest reports the management address at
`surfaces.management` (its address string when configured, `false` otherwise),
so a controller knows whether an instance even *has* a management channel before
it tries to use one.

---

## 2. The operator tools

These five tools steer a running instance without an in-band config change. They
are listed to a Management peer by `tools/list` and invoked by `tools/call`.
Each returns a JSON `structuredContent` body alongside human text. The names are
a single frozen constant shared with the capabilities manifest
(`capabilities::OPERATOR_TOOLS`), so what an instance *advertises* and what it
*serves* can never drift.

| Tool | What it does | Exits the process? | Readiness |
|---|---|---|---|
| `drain` | Begin a graceful drain (identical to SIGTERM) → exit `0` | yes, eventually | → NotReady |
| `lame-duck` | Advertise NotReady without draining or exiting | no | → NotReady (reversible) |
| `pause` | Suspend the whole agentic tree at turn boundaries | no | unchanged |
| `resume` | Clear a prior `pause` | no | unchanged |
| `cancel` | Cancel one run/subtree by handle | no | unchanged |

### 2.1 `drain` — graceful shutdown for a rolling update

`drain` trips the same one-way `DRAINING` latch a `SIGTERM` does: readiness flips
to NotReady, no new work is accepted, in-flight subagents wind down at their turn
boundaries, then the process exits **`0`** (a clean drain is `0`, never `143`).
It returns **immediately** with a snapshot — it does **not** block until exit.

```jsonc
// tools/call → drain
{ "name":"drain", "arguments":{ "deadline_ms": 20000 } }   // deadline_ms optional
// result.structuredContent
{ "draining":true, "in_flight":2, "eta_ms":20000,
  "drain_timeout_ms":25000, "started_at":"2026-06-28T10:00:00.123Z" }
```

`deadline_ms` is **clamped** to the configured `--drain-timeout` — a tool call
can never push the drain past the pod's grace period. `drain` is
idempotent/monotonic: a second `drain` (or a later SIGTERM) just re-reports; it
never escalates to the second-signal SIGKILL force path.

> **To drain a pod for a rolling update:** call `drain`, then let the orchestrator
> wait out `terminationGracePeriodSeconds` (keep `--drain-timeout` strictly below
> it — see [configuration §9](configuration.md)). The instance leaves on its own.

### 2.2 `lame-duck` — stop taking new work without leaving

`lame-duck` flips `/readyz` to NotReady **without** draining or exiting: the
instance keeps running and serving in-flight work but advertises "don't send me
new work". It is the rolling-update primitive when you want to bleed an instance
off the load path *before* you drain or replace it.

```jsonc
{ "name":"lame-duck", "arguments":{} }                 // default: NotReady
{ "name":"lame-duck", "arguments":{ "ready":true } }   // clear the override
// result.structuredContent
{ "ready":false, "since":"2026-06-28T10:00:00.123Z", "in_flight":2 }
```

The override only ever pushes *toward* NotReady. `ready:true` clears it and
restores the genuine computed readiness — but it can't assert Ready over a
not-ready supervisor: if a `drain` is already in progress, `ready:true` is
**refused** (an error result) rather than silently flipping a flag with no
effect, because the drain latch is one-way.

### 2.3 `pause` / `resume` — freeze the tree at turn boundaries

`pause` suspends the **whole agentic tree** at turn boundaries: every in-flight
root subagent finishes its *current* turn and then waits. It fans `ctrl/pause`
to each live subtree (warm sessions directly; async runs via a per-run pause
flag the run's supervisor reactor forwards). It is **not** instant and **not** a
deadline — a loop mid-turn finishes that turn first.

Critically, `pause` is **neither a drain nor a lame-duck**: the tree freezes but
stays intact, **readiness is unchanged**, and the supervisor reactor + the
liveness heartbeat keep running (the instance still answers `ping`, still serves
management, still bumps liveness). Use it for live debugging, or to hold a tree
still while you swap the model service underneath it.

```jsonc
{ "name":"pause", "arguments":{} }
// result.structuredContent
{ "paused":true, "affected":3 }     // 3 live subtrees suspending at their next turn
{ "name":"resume", "arguments":{} }
{ "paused":false, "affected":3 }
```

`affected` counts only the live subtrees that took the message. `pause` sets an
instance-wide flag, so:

- `agentd://inventory` reports `paused:true` (and each live node mirrors it);
- the `agentd_paused` gauge reads `1` (see [observability](observability.md));
- a run launched *while paused* starts paused.

Pause is explicitly **not** readiness — a paused instance can still be ready (the
readiness gauge tracks only drain / lame-duck, never pause).

### 2.4 `cancel` — kill one run, keep the pod

`cancel` is the management-transport, instance-scoped wrapper over the served
`subagent.cancel` path: it cancels one tracked warm session or async run **by
handle**, walking the kill ladder over that run's subtree — but it leaves the pod
running (unlike `drain`, which also exits).

```jsonc
{ "name":"cancel", "arguments":{ "handle":"served.2", "reason":"superseded" } }
// result.structuredContent
{ "handle":"served.2", "cancelled":true }
```

An **unknown handle** is reported as an `isError:true` result (a racing reap may
have already removed it), *not* a JSON-RPC protocol error — the same observation
-vs-fault distinction agentd honours everywhere ([mcp §1.4](mcp.md)). A handle
that is already terminal returns `cancelled:false, reason:"already finished"`.
`reason` is surfaced into the `ctrl/cancel` frame and the logs.

---

## 3. The capabilities manifest — control-plane discovery

A controller does not assume what an instance can do — it **reads** it. The
capabilities manifest is the machine-readable description of *what this binary is
and what it serves right now*: contract/build versions, identity, the configured
run shape, and the `surfaces{}` block (the graceful-degradation contract).

It is exposed two ways, from **one** builder so they never drift:

- **`agentd --capabilities`** — a one-shot that prints the manifest to stdout and
  exits `0`. It is **side-effect-free and network-free**: no socket bind, no MCP
  connect, no LLM call, no discovery probe. This is the admission probe a
  controller runs against the *image* before it schedules anything.
- **`agentd://capabilities`** — the live resource on the management transport,
  built from the running daemon (it overlays a lazily-probed, cached model
  discovery onto `intelligence.models`).

```console
$ agentd --instruction x --intelligence unix:/run/intel.sock --capabilities
{ "contract_version":"1.0", "agentd_version":"…", "build_features":[…],
  "identity":{…}, "mode":"once", "model":null,
  "intelligence":{ "transport":"unix", "endpoints":1, "healthy":"unknown", … },
  "mcp_servers":[…], "limits":{…}, "surfaces":{…} }
```

`contract_version` is `1.0` — the agentctl↔agentd contract version. A controller
refuses an instance whose *major* it does not understand.

**No secrets, ever.** The manifest carries no token, no resolved `{{secret:NAME}}`
value, and no endpoint URL (which can embed credentials) — `intelligence` is
structural: transport scheme + endpoint *count* only.

### 3.1 The `surfaces{}` block

`surfaces{}` reports, honestly for **this** build and config, which control-plane
surfaces are served. A surface that isn't built/configured is reported `false`
(or, for the `claim` style, the key is omitted entirely). This is how `agentctl`
degrades gracefully: it drives only what is declared.

| Key | Value | Meaning |
|---|---|---|
| `management` | address string \| `false` | the `--serve-mcp` address, or not served |
| `operator_tools` | `["drain","lame-duck","pause","resume","cancel"]` \| `[]` | the operator tools served (non-empty only with `serve-mcp`) |
| `a2a` | object \| `false` | the A2A surface (`a2a` feature) — version, streaming, method set |
| `metrics` | address string \| `false` | the `--metrics-addr` for `/metrics`+`/healthz`+`/readyz` |
| `metrics_schema` | `"1.0"` | the frozen metrics-schema version |
| `events` | bool | `agentd://events` served (needs `events` + a management transport) |
| `report_schema` | `"1.0"` | the run-outcome report schema this binary writes |
| `exit_codes` | `"1.0"` | the frozen exit-code contract version |
| `intelligence` | bool | `agentd://intelligence` health resource served (needs `serve-mcp`) |
| `config_validate` | `true` | `--validate-config` available (always, default build) |
| `config_schema` | `true` | `--config-schema` available (always, default build) |
| `hot_reload` | bool | hot reload served (needs the `hot-reload` feature) |
| `config_effective` | bool | `agentd://config/effective` served (needs `serve-mcp`) |
| `cluster` | bool | sharding + the capacity resource present (`cluster` feature) |
| `shard` | `"K/N"` \| `null` | this instance's shard identity, or null when unsharded |
| `standby` | bool | reflects `--standby` (a directed-assignment target) |
| `claim` | `{ "styles":[…] }` *(key present only in a `cluster` build)* | the claim styles this instance speaks |

The frozen schema versions (`metrics_schema`, `report_schema`, `exit_codes`,
`contract_version`) let a controller author dashboards/alerts/scalers against a
stable contract and detect a major bump.

---

## 4. Hot reload

A `hot-reload` build can re-read its config **in place** — no restart, no dropped
in-flight work — for the *reloadable* subset of settings. The reload is
**validate-first and all-or-nothing**: a bad or restart-only candidate is a clean
**no-op** (the running config is kept verbatim), never a partial apply.

### 4.1 The two triggers

Both funnel into one identical reload routine:

- **SIGHUP** (the portable default; `hot-reload` feature). The async-signal-safe
  handler sets a latch and wakes the reactor; the reload runs on the reactor
  thread at a turn/tick boundary. Without the feature, SIGHUP keeps its default
  disposition (terminate). *(Note: a plain config build with no hot-reload still
  drops SIGHUP — config is a frozen snapshot there.)*
- **`--watch-config`** (the `config-watch` feature). A raw-inotify watch on the
  config file's directory, so a Kubernetes ConfigMap volume swap reloads the file
  in place. It sets the *same* latch SIGHUP does, plus a watch-attribution flag,
  so the reload is labelled `trigger:"watch"`. `--watch-config` **requires** a
  config file (`--config` / `AGENTD_CONFIG`); watching nothing is a usage error
  (exit `2`).

### 4.2 What is reloadable vs restart-only

Only the FILE is re-read on a reload; the env+flag layers are the process's
fixed inputs, so a flag still overrides the new file. The partition is owned by
`RESTART_ONLY_FIELDS` in `config.rs`:

| Reloadable (applied in place) | Restart-only (a diff is rejected) |
|---|---|
| `model` | `mode` |
| `max_tokens` | `run_id` |
| `intelligence_headers` | `serve_mcp` (transport) |
| `limits` (`max_steps` / `max_depth` / `deadline`) | `enable_exec` |
| `log_level` | `drain_timeout` |
| `subscribe` (the reactive subscription set) | `shard` |
| **`mcp_servers`** (live re-handshake) | `claim_routes` |
| **`intelligence`** (endpoint list + token + swap policy) | `standby` |
| | `assign_from` |
| | `continue_subscribe` |

`mcp_servers` reloads via a live re-handshake at the quiesce boundary (add /
remove / edit a server). `intelligence` reloads via the runtime hot-swap
(§4.4) — a change repoints **new** spawns and is fanned to in-flight children as
`ctrl/swap_intel`, applied at each one's next turn boundary.

A reload whose diff touches **any** restart-only field is **rejected** as a clean
no-op (`config.reload_rejected{reason:"restart_required",field}`); `agentctl`
reads the field and rolls a restart instead of a reload.

### 4.3 Validate-first, all-or-nothing

The routine is, in order:

1. **Re-load + re-validate** the candidate (pure-CPU, no side effect) — a now-
   invalid file is the same `Usage` error startup would raise → reject.
2. **Coherence check** — restart-only-diff rejection, plus reloadable-subset
   internal consistency (unique server names; claim/assignment routes reference a
   declared server).
3. **Quiesce** — set a tree-wide guard so the served `subagent.spawn` chokepoint
   transiently refuses *new* spawns. In-flight work is **not** cancelled.
4. **Apply** the reloadable diff, ordered lowest-risk first: value swaps, then the
   MCP server re-handshake, then the subscription reconcile (read-after-subscribe
   on adds), then (cluster) claim re-resolution. A contained runtime failure (an
   added MCP server that won't connect) is logged and the server is simply absent
   — it never rolls back the already-applied steps or kills the daemon.
5. **Refresh the served surface** — `notifications/tools/list_changed` if the
   server set changed; swap the live `agentd://config/effective` view and fire
   `resources/updated` to its subscribers.

`agentd --validate-config` runs the **same** coherence check as an admission
gate before you ship the file — a bad file fails fast (exit `2`) instead of at
reload time. `agentd --config-schema` prints the file schema. Both are default-
build flags (always available).

### 4.4 Observing a reload

A successful reload emits `config.reloaded{changed,applied_ms}` (the `changed`
list uses the reloadable group labels: `model`, `limits`, `log_level`,
`subscribe`, `mcp_servers`, `intelligence`), bumps `agentd_config_generation`,
records `agentd_config_reload_total{result:"applied"}`, and fires
`resources/updated` for `agentd://config/effective`. An intelligence hot-swap
additionally emits `intel.swap{kind,model_from,model_to,endpoint_change,policy}`
and notifies `agentd://intelligence`. A rejected reload emits
`config.reload_rejected{reason,field}` and `…{result:"rejected"}` and leaves the
generation unchanged. (Metric/event names are detailed in
[observability §3](observability.md).)

> **To reload a ConfigMap:** run with `--watch-config --config /etc/agentd/config.json`
> over a ConfigMap volume mount; the kubelet's atomic symlink swap fires the
> inotify watch and the reloadable subset applies in place. A controller can poll
> `agentd_config_generation` (or subscribe `agentd://config/effective`) to confirm
> generation N landed. If the change touches a restart-only field, the reload is
> rejected and you roll a restart.

### 4.5 `agentd://config/effective`

The live, **redacted** view of the running daemon's reloadable config —
Management-only and subscribable. It carries `model`, `swap_policy`, `max_tokens`,
`limits`, `log_level`, the `subscribe` set, structural `mcp_servers`
(name + tags, never the spawn command), and intelligence header **names** only.

It carries **no** token, **no** URL, and **no** resolved `{{secret:…}}` value — a
header whose value is a secret reference is exposed by *name* only. A subscriber
gets a `resources/updated` on every applied reload (notify-then-read), then reads
the post-reload view. Use it to confirm *what* an instance is actually running
after a reload, without ever exposing a credential.

---

## See also

- [Configuration reference](configuration.md) — the flag/env surface,
  `--serve-mcp`, `--drain-timeout`, `--config`, the validate-at-startup contract.
- [Observability & health](observability.md) — the metrics, events, and
  resources this page emits/exposes, plus `/healthz`+`/readyz`+`/metrics`.
- [Deploying agentd](deployment.md) — the pod/scheduler model the drain,
  lame-duck, and reload primitives plug into.
- [Intelligence](intelligence.md) — the endpoint list + the runtime hot-swap that
  an `intelligence` reload drives.
- [MCP: the universal interface](mcp.md) — the self-MCP dialect, the served
  `subagent.*` tools, and the `agentd://` resource scheme these tools extend.
- [Horizontal scaling](scaling.md) — sharding, work-claim leases, standby, and the
  autoscaling signals; `drain` releasing held claims is the scale-down-safety seam.
