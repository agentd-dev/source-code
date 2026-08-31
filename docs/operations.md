# Operations

`agentd` is one process running one agent, but a fleet of them is a *control
plane*. This page is for the operator (and the tooling it drives): how to talk
to a running instance, the commands that steer it without restarting it, how a
controller discovers what an instance can do, and how to push a config change
into a live daemon.

Almost everything here rides one surface — the **A2A listener** (`a2a.listen`,
`--features a2a`) — which is off by default. A pure one-shot CLI run carries
none of it. The exceptions are the three side-effect-free probes
(`--capabilities`, `--config-schema`, `--validate-config`), which need no
listener, no network, and no config beyond the files you point them at.

Every surface here is self-describing: `--capabilities` reports the methods,
admin family and command ops this instance actually serves, `--config-schema`
reports the settings document it accepts, and the telemetry event and metric
names are stable. A controller drives an instance from what the instance
declares, never from an assumption about the build it is talking to.

---

## 1. The A2A listener

`a2a.listen` (flag `--listen`, env `AGENTD_A2A_LISTEN`) arms an HTTPS listener
that speaks **A2A JSON-RPC 2.0 over POST**. It is the instance's only external
channel: peers, display clients and operators all arrive here.

| Form | Meaning | Auth |
|---|---|---|
| `https://0.0.0.0:8443` + `a2a.tls.cert`/`.key`/`.client_ca` | TLS with **mutual-TLS** client auth | a verified client cert → matched against `a2a.principals` |
| `https://0.0.0.0:8443` + `a2a.bearer` | TLS with a **bearer token** | a constant-time-matched `Authorization: Bearer …` → operator (unless a principal claims it) |
| `http://127.0.0.1:8080` | **loopback only**, no auth (dev) | any loopback peer → operator, while `a2a.principals` is empty |

Trust is never derived from the transport alone. Validation refuses to start if:

- `a2a.listen` is `https://` but `a2a.tls.cert` / `a2a.tls.key` are unset;
- the bind is **non-loopback** and none of `a2a.tls.client_ca`, `a2a.bearer` or
  `interface.pairing` is configured — there is no open control plane;
- the bind is non-loopback and the scheme is plaintext `http://`.

Arming the listener makes the instance a **daemon**, and a daemon must be
durable: naming no `store` section gets it `kind: file` on the local
filesystem, while a fleet that shares one backend needs `kind: mcp` or
`kind: http`. An explicit `store.kind: none` on a long-lived instance is a
configuration error, not a warning — a long-lived agent that cannot checkpoint
loses every in-flight run to the next restart.

```yaml
# /etc/agentd/ops.yaml
agent:
  instruction: reconcile the desired state
intelligence:
  endpoints: https://gw.example/v1
mcp:
  servers:
    - name: state
      endpoint: https://mcp-state.internal/mcp
store:
  kind: mcp
  mcp:
    server: state
a2a:
  listen: https://0.0.0.0:8443
  tls:
    cert: /etc/agentd/tls/server.crt
    key: /etc/agentd/tls/server.key
    client_ca: /etc/agentd/tls/clients-ca.crt
  principals:
    - match: { san: "spiffe://prod/ns/ops/sa/agentctl" }
      role: operator
lifecycle:
  drain_timeout: 25s
  watch_config: true
observability:
  audit:
    sink: [log, store]
```

**Certificate rotation is live.** The serve identity is read from the
`a2a.tls.cert` / `.key` / `.client_ca` **paths** and re-stat'ed (throttled) on
accept: swapping the mounted files in place — a cert-manager renewal rotating a
Kubernetes Secret mount — is served on the next connection with **no restart, no
rebind, no dropped listener**. A bad intermediate write degrades to the last-good
identity (never down); the auth *posture* (whether client certs are required) is
fixed at startup — only the PEM contents rotate.

### 1.1 Principals — the trust gate

Every request resolves to a **principal**: an identity (mTLS SAN or subject, a
matched bearer, an AAuth agent id) plus a **role**. The role decides what the
caller may do; there is no in-band flag a caller can set.

| Role | May do |
|---|---|
| `operator` | everything: the admin family, every command op, every read |
| `user` | conversations and their own tasks, plus `workflow.run` / `workflow.status` / `workflow.cancel` / `subagent.send` / `subagent.status` / `plan.get` / `ask_human` |
| `agent` | conversations and their own tasks, plus `workflow.run` / `workflow.status` |
| `anonymous` | nothing (only the pairing handshake, when `interface.pairing` is on) |

`status` is granted to every non-anonymous role. `a2a.principals[].grants` adds
explicit tool-name patterns on top of a role's defaults, and
`a2a.principals[].quotas` attaches a per-principal rate limit and token budget.

Matching order is: the configured `a2a.principals` rules in order, first match
wins; then the operator defaults (a transport-authenticated peer when
`a2a.bearer` is set, or a loopback peer while no principals are configured);
then anonymous. Declaring **any** principal turns the loopback-operator default
off — which is what you want in production.

The **admin family is operator-only**. A `user` or `agent` principal that calls
one is refused; an anonymous caller is refused before dispatch. So a delegating
peer can never drain or pause the instance it is talking to.

---

## 2. The operator admin methods

These five **A2A admin methods** steer a running instance without an in-band
config change. An operator invokes them as JSON-RPC methods on the listener, and
each returns its body directly; a refusal is a JSON-RPC error, not a result. The
names are also reported in the capabilities manifest at `a2a.admin`, so what an
instance advertises and what it serves cannot diverge.

| Method | What it does | Exits the process? |
|---|---|---|
| `a2a.drain` | Begin a graceful drain (identical to SIGTERM) → exit `0` | yes, eventually |
| `a2a.lameduck` | Accepted as an alias of `a2a.drain` | yes, eventually |
| `a2a.pause` | Hold the whole instance, or one run, at a safe boundary | no |
| `a2a.resume` | Clear a prior `a2a.pause` | no |
| `a2a.cancel` | Cancel one run by id | no |

The bare spellings (`drain`, `pause`, …) are accepted too, and admin-method
matching is case-insensitive. Every call takes an optional `reason` string
(default `"operator request"`), which is carried into the logs and the audit
record.

### 2.1 `a2a.drain` — graceful shutdown for a rolling update

`drain` trips the same one-way latch a `SIGTERM` does: readiness flips to
NotReady, in-flight work winds down at its boundaries, state is checkpointed,
then the process exits **`0`** (a clean drain is `0`, never `143`). It returns
**immediately** with an acknowledgement — it does **not** block until exit.

```jsonc
// params are the args directly (no nested "arguments")
{ "jsonrpc":"2.0", "id":1, "method":"a2a.drain", "params":{ "reason":"rolling update" } }
// result
{ "ok":true, "state":"draining", "reason":"rolling update" }
```

The drain budget is `lifecycle.drain_timeout` — a call cannot push the drain past
it. `drain` is idempotent: a second `drain` (or a later SIGTERM) is a no-op on an
already-draining instance.

> **To drain a pod for a rolling update:** call `a2a.drain`, then let the
> orchestrator wait out `terminationGracePeriodSeconds` (keep
> `lifecycle.drain_timeout` strictly below it — see
> [configuration §9](configuration.md)). The instance leaves on its own.

### 2.2 `a2a.pause` / `a2a.resume` — hold work without leaving

With **no** `run` parameter, `pause` holds the **whole instance**: no new
conversation turns dispatch and no workflow steps schedule. Intake keeps
running — the listener still answers, the inbox still fills, tasks still
accept — so nothing is lost; the work simply queues until `resume`. Use it for
live debugging, or to hold an instance still while you swap the model service
underneath it.

With a `run` id, it flips just that run between `Paused` and `Running`; the
scheduler skips paused runs and every other run keeps moving.

```jsonc
{ "method":"a2a.pause",  "params":{} }
{ "ok":true, "state":"paused", "reason":"operator request" }

{ "method":"a2a.pause",  "params":{ "run":"reconcile-01J8…" } }
{ "ok":true, "paused":"reconcile-01J8…" }

{ "method":"a2a.resume", "params":{} }
{ "ok":true, "state":"running" }
```

Pause is **reversible** and is **not** a drain: readiness is unchanged and the
instance stays a member of the fleet. Pausing an already-terminal run is an
`INVALID_PARAMS` error; resuming a run that is not paused is too; an unknown run
id is a task-not-found error. The instance-wide hold is reported as
`paused: true` in the `status` view.

### 2.3 `a2a.cancel` — kill one run, keep the pod

`cancel` cancels one run **by id**, walking its live steps down — but it leaves
the pod running (unlike `drain`, which also exits).

```jsonc
{ "method":"a2a.cancel", "params":{ "run":"reconcile-01J8…", "reason":"superseded" } }
{ "ok":true, "cancelled":"reconcile-01J8…" }
```

Omitting `run` is an `INVALID_PARAMS` error (`cancel needs a run id`). To cancel
a *task* rather than a run — one conversation turn or one delegated unit of work
— use the standard A2A `CancelTask` method instead, which any non-anonymous
principal may call on its own tasks.

---

## 3. Reading live state

The read surface is the same listener, and every read resolves to a principal
first. There is no unauthenticated status port.

| Read | How | Who |
|---|---|---|
| instance status | `SendMessage` with a `status` command DataPart | any non-anonymous role |
| effective config | `SendMessage` with a `config` command DataPart | operator |
| one task | `GetTask` | the task's owner (operator sees all) |
| all tasks | `ListTasks` | as above |
| task updates | `SubscribeToTask` (SSE) | as above |
| identity + skills | `GetAgentCard` | pre-auth |
| the live event feed | `SubscribeToEvents` (SSE) | any non-anonymous role, with `interface.enabled` |
| the log ring | `debug.events` command DataPart | operator, with `interface.debug` |

A command is a **DataPart** on an ordinary A2A message — `{"data": {"agentd":
{"op": "<name>", …args}}}` — so one method (`SendMessage`) carries both natural
language and the machine control surface:

```console
$ curl -sS --cert ops.crt --key ops.key --cacert ca.crt https://agent.internal:8443 \
    -H 'content-type: application/json' \
    -d '{"jsonrpc":"2.0","id":1,"method":"SendMessage","params":
         {"message":{"messageId":"m1","parts":[{"data":{"agentd":{"op":"status"}}}]}}}'
```

**`status`** answers with the instance view: `instance`, `run_id`, `uptime_ms`,
`draining`, `paused`, the durable `store` (kind, degraded flag, generation), the
armed `workflows`, live `runs`, `conversations`, `subagents`, OS `children`,
`timers`, `inbox_pending`, the token `budget`, registered `tools`, loaded
`skills`, the lifetime `counters`, the current `instruction` (source/version/size,
never the text), the active `model`, and recent `activity`.

**`config`** answers with the effective merged settings document — the same
document `--config-schema` describes. It carries `{{secret:…}}` **references**,
never resolved secret values, which is why it is operator-only. Use it to confirm
*what* an instance is actually running after a reload, without ever exposing a
credential.

**`debug.events`** is a cursor read of the live log ring — the operator live-tail,
without a collector round-trip. It takes `{after?, limit?, level?, prefix?}` and
returns `{events, oldest_seq, newest_seq, dropped}`; the ring is bounded
(`observability.events_ring`, 1024 lines by default), lossy by design, and
never blocks the loop — a slow reader loses old lines and sees it in `dropped`.
It requires `interface.debug`, which also installs the ring.

---

## 4. Discovery — what this binary is and what it serves

A controller does not assume what an instance can do — it **reads** it. A handful
of flags answer that, all side-effect-free: no socket bind, no MCP connect, no
LLM call, no discovery probe. They are the admission probes you run against the
*image* and the *file* before you schedule anything.

### 4.1 `--capabilities`

Prints the capability manifest to stdout and exits `0`. It reflects the
configuration — what this binary is set up to do — not live state.

```console
$ agentd --capabilities -c /etc/agentd/ops.yaml
{ "runtime":"1", "version":"1.1.0",
  "agent":{ "name":"agentd", "instruction":true, "preflight":"auto" },
  "intelligence":{ "model":null, "endpoints":1 },
  "mcp_servers":["state"], "internal_tools":[…], "tools":{ "overrides":[], "disabled":[] },
  "workflows":[…], "knowledge":{…}, "search":{…}, "skills":{ "sources":0 },
  "a2a":{ "listen":"https://0.0.0.0:8443", "tls":true, "mtls":true, "bearer":false,
          "methods":["SendMessage","SendStreamingMessage","GetTask","CancelTask",
                     "ListTasks","SubscribeToTask","GetAgentCard"],
          "admin":["a2a.drain","a2a.lameduck","a2a.cancel","a2a.pause","a2a.resume"],
          "command_ops":["status","config","workflow.run",…],
          "principals":[…], "loopback_operator":false },
  "interface":{…}, "store":"mcp",
  "lifecycle":{ "run_until":"auto", "daemon":true } }
```

The three fields a controller branches on:

- **`a2a`** — `null` when no listener is configured. Its presence is the
  graceful-degradation contract: `methods`, `admin` and `command_ops` are exactly
  what this instance serves, so a controller drives only what is declared.
- **`lifecycle.daemon`** — `true` when the instance is long-lived (a listener,
  or a workflow with a `loop` / `schedule` / `subscribe` / `signal` / `event`
  start node). A `false` here means a Job, not a Deployment.
- **`store`** — the durability backing (`mcp` / `http` / `memory` / `none`).

**No secrets, ever.** The manifest carries no token, no resolved `{{secret:NAME}}`
value, and no endpoint URL (which can embed credentials) — `intelligence` is
structural: model name plus endpoint *count*. Principal matchers are described,
never dumped: a `bearer_ref` renders as `***`.

Not every feature is in the released binary. `a2a`, `metrics`, `cron`, `otel`,
`hot-reload`, `config-watch`, `aauth`, `oauth` and `cel` ship in the published
builds; `exec` is the one build-from-source opt-in. The manifest reflects the
binary you actually have.

### 4.2 `--config-schema` and `--validate-config`

`--config-schema` prints the settings **JSON Schema** (Draft 2020-12) and exits
`0` — every path, its type, its enum domain. `--workflow-schema` does the same for
the workflow dialect plus the node registry. Both are how a controller (or an
editor, or an admission webhook) learns the config surface without parsing docs.

`--validate-config` loads and validates the whole merged configuration, prints
the verdict as one JSON line, and exits `0` or `2`. It runs the **same** checks
startup runs, so a bad file fails in CI instead of at rollout:

```console
$ agentd --validate-config -c /etc/agentd/ops.yaml
{"event":"config.valid","files":["/etc/agentd/ops.yaml"],"schema":"1"}

$ agentd --validate-config -c /etc/agentd/broken.yaml
{"event":"config.invalid","msg":"a2a.listen on a non-loopback address needs client auth: a2a.bearer, interface.pairing, or a2a.tls.client_ca (mTLS — then EVERY caller needs a client certificate, bearer-only and paired included)"}
```

Both flags are in every build.

---

## 5. Hot reload

A `hot-reload` build re-reads its configuration **in place** — no restart, no
dropped in-flight work — for the *reloadable* subset of settings. The reload is
**validate-first and all-or-nothing**: a bad or restart-only candidate is a clean
**no-op** (the running configuration is kept verbatim), never a partial apply.

### 5.1 The two triggers

Both funnel into one identical routine:

- **SIGHUP** (the portable default; `hot-reload` feature). The async-signal-safe
  handler sets a latch and wakes the loop; the reload runs on the loop thread at
  a tick boundary. Without the feature, SIGHUP keeps its default disposition
  (terminate).
- **`lifecycle.watch_config`** (flag `--watch-config`; the `config-watch`
  feature). A raw-inotify watch on the config files' directories, so a Kubernetes
  ConfigMap volume swap reloads in place. It sets the *same* latch SIGHUP does,
  plus an attribution flag, so the reload is labelled `trigger:"watch"`.
  `lifecycle.watch_config` **requires** a config file; watching nothing is a
  usage error (exit `2`).

### 5.2 What is reloadable vs restart-only

Only the **files** are re-read; the env and flag layers are the process's fixed
inputs, so a flag still overrides the new file. `RESTART_ONLY_PATHS` in
`config/v2` is the authoritative partition:

| Reloadable (applied in place) | Restart-only (a diff is refused) |
|---|---|
| `intelligence` (endpoints, model, token) | `config_version`, `agent.name` |
| `intelligence.budget` (windows; counters carry over) | `store.kind`, `store.prefix`, `store.mcp`, `store.http`, `store.file` |
| `agent.instruction` (static text or a resource URI) | `lifecycle.run_until`, `.drain_timeout`, `.run_id`, `.exit_code_map`, `.watch_config` |
| `agent` (preflight, wake_on, tools, parallelism, budget) | `a2a.listen`, `a2a.tls`, `a2a.bearer` |
| `mcp` (live re-handshake) | `observability.otel`, `.metrics_addr`, `.health_file`, `.events_ring`, `.traceparent` |
| `tools`, `knowledge`, `search` (registry rebuild) | `security` |
| `skills` (sources re-discovered) | — |
| `workflows` (live runs stay pinned to their hash) | |
| `limits`, `lifecycle.idle_grace`, `observability.log_level` / `.log_content`, `memory`, `context` | |

`mcp` reloads via a live re-handshake: removed servers disconnect, added and
edited servers connect and hand-shake, unchanged servers are left alone. A
contained runtime failure (an added server that will not connect) is logged and
that server is simply absent — it never rolls back the already-applied steps or
kills the daemon. `intelligence` repoints the next unit of work: every turn
worker is spawned fresh from the live settings, so a new endpoint, model, budget
or tool override takes effect at the next turn.

A reload whose diff touches **any** restart-only path is refused as a clean
no-op, naming the paths, so a controller reads them and rolls a restart instead.

### 5.3 Validate-first, all-or-nothing

The routine is, in order:

1. **Re-merge + re-validate** the candidate through the same `load` pipeline
   startup uses (built-in < files < env < flags) — a candidate that fails
   validation raises exactly the error startup would, and the running config is
   kept.
2. **Restart-only diff** — any changed restart-only path refuses the reload
   before anything is applied.
3. **Apply** the reloadable diff, lowest-risk first: value swaps, the MCP
   re-handshake, the registry rebuild, skills re-discovery, then the workflow
   reload (live runs keep the definition they started with, pinned by hash).
4. A registry or workflow document that fails to build refuses the whole reload
   and restores the previous tool settings.

`agentd --validate-config` runs the same validation as an admission gate before
you ship the file, so a bad candidate fails fast (exit `2`) rather than at reload
time.

### 5.4 Observing a reload

A successful reload emits `config.reloaded{trigger,changed}` — where `changed`
is the list of reloadable groups that actually moved (`intelligence`,
`intelligence.budget`, `agent.instruction`, `agent`, `mcp`, `tools`, `skills`,
`workflows`, `limits/lifecycle/observability/memory/context`, or `nothing`) —
bumps the durable manifest's `config_generation`, and records
`agent_config_reload_total`. A refusal emits `config.reload.invalid{trigger,error}`
or `config.reload.restart_required{trigger,paths}` and leaves the generation
unchanged. The file watcher itself emits `config.watch.armed` / `config.watch.fired`
/ `config.watch.error`. (Metric and event names are detailed in
[observability](observability.md).)

> **To reload a ConfigMap:** run with `lifecycle.watch_config: true` and a
> `--config` path on a ConfigMap volume mount; the kubelet's atomic symlink swap
> fires the inotify watch and the reloadable subset applies in place. A
> controller confirms the change landed by watching for the `config.reloaded` log
> line, or by reading the `config` command back. If the change touches a
> restart-only path, the reload is refused and you roll a restart.

---

## 6. The audit stream

`observability.audit.sink` turns on the append-only record of *who did what*:
every A2A call, every principal-driven command, config reloads, restores, store
conflicts and kills. Each record is
`{ts, instance, principal, role, action, target, outcome, request_id, trace}`.

Two sinks, independently selectable:

- **`log`** — one closed-vocabulary `audit` line on stderr, alongside the rest of
  the telemetry. Never content-suppressed: an audit trail is metadata, not
  conversation content.
- **`store`** — a durable, ULID-keyed record in the configured store. It is never
  compare-and-swapped and never listed, so it cannot be rewritten in place.
  A failed audit write is logged and never fails the audited action.

An A2A call's `action` is `a2a.<method>` — and `a2a.<method>:<op>` when the
message carried a command DataPart — so `a2a.SendMessage:workflow.run` and
`a2a.drain` are both first-class, filterable audit actions. This is the answer to
"why did the agent do that, and on whose authority?".

---

## 7. Resource pressure — shed new work, drain what is in flight

The failure this machinery exists for is disk: the file store writes until
`ENOSPC`, and a checkpoint failure is a **halting** condition. So the runtime
watches the store filesystem's headroom (plus the cgroup's `memory.high`, when
one is armed) and moves through three levels, assessed every ~2 s and logged
once per **transition**, never per refusal:

| level | event | what changes |
|---|---|---|
| ok | `pressure.cleared` | everything admits |
| warn (< 2× `min_free`) | `pressure.warn` | **`priority: low` work already sheds** (workflows and subagent spawns that declared it); everything else still admits — the operator is told while there is still time to act |
| shed (< `store.file.min_free`, default 256 MB) | `pressure.shed` | admission stops; in-flight work drains |

"Admission stops" is the same decision at every door, so a pressed daemon is
consistent rather than lucky:

- **starts** — `loop`/`schedule`/`subscribe`/`signal` firings are skipped with a
  `start.shed` line naming the cause (a schedule that quietly stopped while the
  disk filled is a story the log must tell);
- **webhooks** — `429 Too Many Requests` + `Retry-After: 30`, *after*
  authentication (an unauthenticated probe learns nothing about load), *before*
  the durable inbox write the disk may not be able to keep;
- **conversation turns** — new turns stay **queued**, nothing is dropped;
  dispatch resumes by itself when the level clears;
- **subagent spawns** and **`workflow.run`** (the tool and the A2A command) —
  refused with the cause in the error.

Nothing running is interrupted: an agent that finishes its current job and takes
no more has degraded; one that dies mid-checkpoint has corrupted its next
restart's starting point. Tune with `store.file.min_free` (`"0"` disables the
disk checks; a memory/mcp/http store never enables them — their durability does
not live on this disk).

On the wire, per-route **arrival throttling** composes with this:
`rate: "<burst>/<per>s"` on a `webhook` node answers `429` with a computed
`Retry-After` past its burst — `parallelism` bounds how many requests run at
once, `rate` bounds how fast they arrive, and pressure sheds regardless of
either.

With `--features metrics`, the levels are scrapeable (schema 1.2):
`agent_pressure_level` (0/1/2), `agent_disk_free_bytes` (absent without a file
store), `agent_runs_active`, `agent_turns_queued` — the last two are the
utilization pair to alert on *before* pressure does it for you.

---

## See also

- [Configuration reference](configuration.md) — the settings document, the
  precedence rules, and the validate-at-startup contract.
- [Observability & health](observability.md) — the metrics, events, and health
  surfaces this page emits, plus `/healthz`+`/readyz`+`/metrics`.
- [Deploying agentd](deployment.md) — the pod/scheduler model the drain and
  reload primitives plug into.
- [Intelligence](intelligence.md) — the endpoint list and the runtime hot-swap an
  `intelligence` reload drives.
- [MCP: the universal interface](mcp.md) — agentd as an MCP client, and as an A2A
  endpoint other agents call.
- [The interface — TUI & web UI](interface.md) — the display clients that ride
  this same listener.
- [Horizontal scaling](scaling.md) — partitioning work and the autoscaling signals; drain
  is the scale-down-safety seam.
