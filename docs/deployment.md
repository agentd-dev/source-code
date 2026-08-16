# Deploying agentd

`agentd` 2.0 is one binary that runs **one durable agent**. An external scheduler
starts, stops, replicates, and watches it; the binary owns no control plane
(RFC 0011 §1). This page is a set of deployment recipes:

1. [Standalone CLI — one-shot job](#1-standalone-cli--one-shot)
2. [Long-lived A2A daemon](#2-long-lived-reactive-daemon)
3. [Container — minimal scratch/distroless image](#3-container--minimal-scratchdistroless-image)
4. [Scheduled by an external orchestrator (Kubernetes)](#4-scheduled-by-an-external-orchestrator-kubernetes)

The **same durable runtime** backs every shape (RFC 0026); they differ only in the
lifecycle shape (`lifecycle.run_until`) and what triggers runs. A one-shot **job**
goes empty-and-final and exits, mapping its outcome to the exit-code table
(RFC 0011 §7); a **daemon** idles on its triggers (an A2A listener, a `subscribe` /
`schedule` / `loop` start node) and exits only on a SIGTERM drain.

> **agentd 2.0.** Configuration is the nested `config_version: "2"` document (see
> [`configuration.md`](configuration.md)); the 1.x flat schema and the `--mode` /
> `--serve-mcp` / `--shard` / `--claim` flags were **removed** in the mode
> cut-over. The examples below use v2 config files.

---

## The config surface you will actually use

Configuration is a `config_version: "2"` document (`--config` / `AGENT_CONFIG`,
YAML or JSON, repeatable + merged). **Every path in the schema is also an env var
and a flag** (`limits.run.max_steps` ⇒ `AGENTD_LIMITS_RUN_MAX_STEPS` /
`--limits.run.max-steps`), so a container overrides at deploy time without editing
the file. Precedence, top wins: **built-in default < config file < env < flag**.
Secrets are **references** only (`{{secret:NAME}}` / `{{secret-file:PATH}}`),
resolved from env / mounted files — never inline values.

| Concern | Section / path | Legacy alias |
|---|---|---|
| Instruction | `agent.instruction` (text or a resource URI) | `--instruction` / `--instruction-file` |
| Intelligence | `intelligence.endpoints` (ordered failover), `.model`, `.token` | `--intelligence` / `--model` / `--intelligence-token` |
| Token budget | `intelligence.budget.windows` (rate-limit the burn, RFC 0025) | — |
| MCP servers | `mcp.servers: [{name, endpoint}]` | `--mcp name=<endpoint>` |
| Durable store | `store.kind: mcp\|http\|memory`, `store.mcp.server` | — |
| **A2A listener** | `a2a.listen`, `a2a.tls`, `a2a.principals`, `a2a.bearer` | — |
| **A2A peers** | `a2a.peers: [{name, endpoint}]` | — |
| Workflows / triggers | `workflows: [{name, steps}]` (start nodes: once/loop/schedule/subscribe/signal/event/a2a) | — |
| Limits | `limits.max_runs`, `limits.run.{max_steps,max_tokens,deadline}`, `limits.subagents.max_depth` | `--max-steps` / `--deadline` |
| Lifecycle | `lifecycle.run_until` (idle\|drained\|auto), `lifecycle.drain_timeout` | `--drain-timeout` |
| Run ID | `lifecycle.run_id` (idempotency key) | `--run-id` |
| Observability | `observability.log_level`, `.health_file`, `.metrics_addr`, `.audit`, `.otel` | `--log-level` |
| Security | `security.tls_ca`, `security.aauth`, `security.cgroup.{spec,memory_max,pids_max}` | — |

Durations accept `ms`/`s`/`m`/`h` or a bare integer (seconds). Each intelligence /
MCP endpoint must be `https://…` (or loopback `http://` for a same-host dev
gateway). Config is validated **before any side effect**:
`agentd --validate-config` (exit 2 on error), `agentd --config-schema=2` (the
machine-readable schema), `agentd --capabilities` (the effective surface).

> **Scope markers.** The external channel is **A2A** (`a2a.listen`, RFC 0029) —
> the v1 self-MCP server and the `cluster` sharding/work-claim path were removed.
> MCP tasks/sampling/roots are deferred (RFC 0013).

---

## 1. Standalone CLI — one-shot

A **job** (the default, `lifecycle.run_until: auto` with no listener). Run an
instruction to a terminal status, emit the result on **stdout**, write telemetry
to **stderr**, exit with a code from the
[exit-code table](#the-exit-code-contract).

```bash
agentd \
  --instruction "Summarise today's open incidents and post a digest." \
  --intelligence https://gw.example/v1 \
  --model my-model \
  --mcp incidents=https://mcp-incidents.internal/mcp \
  --mcp slack=https://mcp-slack.internal/mcp \
  --deadline 5m \
  --max-steps 40
```

stdout carries the agent's final result; stderr carries one NDJSON event per
line. The canonical fields are
`ts level event run_id agent_id agent_path comp pid …` (RFC 0010):

```json
{"ts":"2026-06-25T18:30:01.412Z","level":"info","event":"proc.start","run_id":"0197f3c4a01abcd","agent_id":"sup","agent_path":"0","comp":"supervisor","pid":4711,"version":"1.0.0","mode":"once","mcp_servers":2,"subscribe":0}
```

Because stdout is the result and stderr is telemetry, you compose with ordinary
shell tooling:

```bash
agentd --instruction "$(cat task.md)" --intelligence https://gw.example/v1 \
  2> >(jq -c 'select(.level=="error")') \
  | tee result.txt
```

Read the instruction from a file (handy for ConfigMap/Secret projection) with
`--instruction-file`, or set `INSTRUCTION` in the environment. The intelligence
token is **never** logged — pass it via `AGENT_INTELLIGENCE_TOKEN` or
`--intelligence-token`, not on a shared command line where it lands in `ps`.

**Idempotent retries.** A bare run mints a random `run_id` per process. For a
unit of work that a scheduler may retry, pin a **stable** key so backing MCP
services can dedupe the side effect (RFC 0011 §6):

```bash
agentd --run-id "nightly-digest-2026-06-25" \
  --instruction "$(cat task.md)" --intelligence https://gw.example/v1 --mcp …
```

The key rides in the `_meta` of every outbound MCP `tools/call`; a backing
service that honours idempotency keys collapses a retried effect to one. agentd
itself writes nothing durable except its log streams, so a re-run is safe by
construction.

---

## 2. Long-lived daemon (A2A + reactive triggers)

A **daemon** (`lifecycle.run_until: drained`) idles cheaply and wakes on its
triggers — an A2A message/command, or a `subscribe` / `schedule` / `loop` start
node. It exits only on a SIGTERM drain, never on an individual run failing. A
daemon needs a **durable store** (RFC 0025) so state survives a restart.

```yaml
# /etc/agentd/triage.yaml
config_version: "2"
agent: { instruction: "When a ticket is filed, triage it and assign an owner." }
intelligence: { endpoints: https://gw.example/v1, model: my-model }
mcp:
  servers:
    - { name: tickets, endpoint: https://mcp-tickets.internal/mcp }
    - { name: state,   endpoint: https://mcp-state.internal/mcp }
store: { kind: mcp, mcp: { server: state } }
a2a:   { listen: https://0.0.0.0:8443, tls: { cert: /tls/cert.pem, key: /tls/key.pem, client_ca: /tls/ca.pem } }
workflows:
  - name: triage
    steps:
      s: { kind: subscribe, server: tickets, uri: "tickets://queue/inbound" }
      t: { kind: agent, depends_on: [s], instruction: "Triage the new ticket." }
      f: { kind: finish, depends_on: [t] }
lifecycle: { run_until: drained, drain_timeout: 25s }
observability: { health_file: /run/agent/health }
```
```bash
agentd --config /etc/agentd/triage.yaml
```

On restart the daemon **restores its durable state** (runs, timers, subscriptions,
inbox), re-handshakes MCP, re-subscribes, and does a read-after-subscribe so a
change that happened while it was down is still acted on. A `subscribe` trigger
is notify-then-read over the MCP servers' **Streamable-HTTP** subscriptions.

### Graceful shutdown

On `SIGTERM`/`SIGINT` the daemon flips a one-way `DRAINING` latch and runs a
**bounded drain** (RFC 0011 §4, ladder in RFC 0003 §3.5):

1. Disarm triggers — stop routing new resource updates; reject new
   `subagent.spawn`; flip readiness to not-ready.
2. Wind down in-flight subagents at turn boundaries (cooperative cancel).
3. Ladder the stragglers — `SIGTERM` → ~5s grace → `SIGKILL` → reap.
4. Flush logs and `exit(0)`.

A **clean drain exits `0`, not `143`** — a rolled `Deployment` looks like a
clean shutdown in dashboards, not a failure. A **second** `SIGTERM`/`SIGINT`
forces immediate `SIGKILL` of all process groups.

The whole drain is bounded by `AGENT_DRAIN_TIMEOUT` (default 25s). **This MUST
be smaller than the orchestrator's shutdown grace** — see the
[footgun below](#the-top-footgun-drain-timeout--grace).

### As a systemd unit

```ini
# /etc/systemd/system/agent-triage.service
[Unit]
Description=agent ticket triage (reactive)
After=network.target

[Service]
EnvironmentFile=/etc/agentd/triage.env       # e.g. AGENT_INTELLIGENCE_TOKEN=…
ExecStart=/usr/local/bin/agentd --config /etc/agentd/triage.yaml
# Give the drain room: must exceed lifecycle.drain_timeout.
TimeoutStopSec=30
KillSignal=SIGTERM
Restart=on-failure

[Install]
WantedBy=multi-user.target
```

`TimeoutStopSec` is systemd's analogue of `terminationGracePeriodSeconds`: keep
it **larger** than `--drain-timeout`.

---

## 3. Container — minimal scratch/distroless image

agentd is `std` + `libc`, statically linkable, with no async runtime, no C
toolchain, and **no built-in tools** — so the image is tiny (~1.3 MB on
`scratch`). The recommended entrypoint is `agentd` itself: it sets
`PR_SET_CHILD_SUBREAPER` and reaps orphans, acting as a tini-class init for its
own process tree (RFC 0003 §3.1). You do **not** need an external `tini`.

The published image (`Dockerfile` at the repo root) ships the **cloud-native
feature set** by default —
`FEATURES="a2a,metrics,cron,otel,hot-reload,config-watch"`. All but `a2a`
(which pulls the TLS stack for the HTTPS listener) are hand-rolled and
dependency-free (serde/serde_json + libc + the two workspace crates), so the
binary stays the minimalism target. What each adds:

| Feature | Adds |
|---|---|
| `metrics` | The `/metrics` + `/healthz` + `/readyz` HTTP probe surface (`observability.metrics_addr`) — so k8s liveness/readiness probes work against a shell-less scratch image. |
| `a2a` | The A2A v2 HTTPS listener (`a2a.listen`, RFC 0029) — the external channel + outbound delegation peers. Pulls the TLS stack. |
| `cron` | UTC 5-field cron scheduling for the `schedule` start node. |
| `otel` | OTLP-over-HTTP/JSON trace + log export + GenAI semconv (hand-rolled, no protobuf/opentelemetry deps). |
| `hot-reload` | SIGHUP-triggered, validate-first reload of the reloadable config subset at a quiesce boundary. |
| `config-watch` | The `inotify` file-watch reload trigger (`lifecycle.watch_config`) — a ConfigMap volume swap reloads in place. Implies `hot-reload`. |

Build a narrower (or wider) surface with `--build-arg FEATURES=…`. `tls` is in the
**default** set (it is the transport — every network surface is HTTPS); `a2a`
rides it. `--no-default-features` drops TLS for the loopback-`http://`-to-a-sidecar
posture. (The 1.x `serve-mcp` / `serve-https` / `events` / `cluster` / `workflow`
features were removed with the mode cut-over.)

```dockerfile
# syntax=docker/dockerfile:1
# Static musl binary on scratch — the dependency-free cloud-native feature set.
FROM rust:1-alpine AS build
ARG FEATURES="serve-https,metrics,cron,otel,cluster,hot-reload,config-watch,workflow"
RUN apk add --no-cache musl-dev
WORKDIR /src
COPY . .
# Alpine's host target IS <arch>-unknown-linux-musl, so the release binary is
# static; one Dockerfile yields native-static amd64 AND arm64 via buildx.
RUN if [ -n "$FEATURES" ]; then \
      cargo build --release --locked -p agentd-cli --features "$FEATURES"; \
    else \
      cargo build --release --locked -p agentd-cli; \
    fi

# scratch: nothing but the binary. (Swap for gcr.io/distroless/static if you
# want a CA bundle + /etc/passwd without managing them yourself.)
FROM scratch
COPY --from=build /src/target/release/agentd /agent
# Non-root by uid (scratch has no /etc/passwd; the kernel uses the number).
USER 65532:65532
# MCP servers are remote HTTP endpoints (--mcp name=https://…), deployed as their
# own services — nothing MCP-related is bundled into the agentd image.
ENTRYPOINT ["/agentd"]
```

> **Build-arg, not flag.** `FEATURES` selects what the **binary** can do; it is a
> compile-time choice, not a runtime flag. A runtime flag that needs an unbuilt
> feature exits `2` — e.g. `--shard 2/8` on an image built without `cluster`.

### TLS is on by default — or terminate it in a sidecar

The default build links `tls` (rustls + bundled roots), so agentd dials `https://`
directly. Two postures:

- **Direct HTTPS (default):** `--intelligence https://…` (and `--mcp name=https://…`)
  reach real endpoints over TLS; agentd holds the trust roots.
- **Sidecar TLS termination:** build `--no-default-features` and point agentd at a
  **same-host sidecar over loopback** — `--intelligence http://127.0.0.1:PORT/…` —
  which terminates TLS + provider auth. A non-loopback `http://` is rejected.

```bash
# Direct HTTPS (default build):
agentd --intelligence https://gw.example/v1 --instruction-file /etc/task.txt \
  --mcp fs=https://mcp-fs.internal/mcp
```

This keeps the default image at scratch-size with no certificate management in
the agentd process.

### Health surface

Two options, both live:

- **`--metrics-addr host:port`** (`metrics` feature, in the default image) serves
  `/healthz` + `/readyz` + `/metrics` over HTTP. This is the right choice for the
  **scratch image**, which has no shell to run an exec probe: point the k8s
  liveness probe at `/healthz` and readiness at `/readyz`. The bare `:port` form
  binds all IPv4 interfaces so the kubelet reaches it at the pod IP. (See the K8s
  probes below.)
- **`--health-file <PATH>`** — agentd heartbeats it while the reactor is live, so
  an exec-style probe can `test` its freshness. Useful where you do not want an
  HTTP listener at all.

`/healthz` returns 200 while the **runtime** tick is fresh and 503 once it goes
stale; `/readyz` flips to not-ready on drain so the pod leaves rotation. An idle
daemon is healthy — liveness tracks the runtime, not whether work is flowing.

> **External channel.** The way other agents (and operators) reach this one is
> **A2A** (`a2a.listen`, `--features a2a`, RFC 0029) — an HTTPS listener with
> trust minted per request by mTLS or a bearer token, resolved to a **principal**
> and authorized against a role matrix; a non-loopback bind must authenticate.
> The 1.x self-MCP server was removed.

---

## 4. Scheduled by an external orchestrator (Kubernetes)

The orchestrator (a K8s operator, Knative, Nomad, a bare-metal supervisor) is
**not part of this project** (RFC 0011 §1). agentd just honours a contract:
config from env/flags, signal-driven drain, and a public exit-code table a
`podFailurePolicy` can branch on. Below are the three shapes; runnable manifests
live in [`examples/`](../examples/).

### The exit-code contract

This table is a **stable, machine-actionable API** — author `podFailurePolicy`
against it (RFC 0011 §5; constants in
[`crates/agentd/src/exit.rs`](../crates/agentd/src/exit.rs)):

| Code | Meaning | Scheduler hint |
|---|---|---|
| `0` | success — one-shot done / clean bound / **clean SIGTERM drain** | Complete |
| `1` | generic / unspecified failure | retriable |
| `2` | config / usage error (validation failed) | **non-retriable** → `FailJob` |
| `3` | partial result (useful output, some sub-tasks failed) | policy |
| `4` | intelligence endpoint unreachable / auth after retries | retriable |
| `5` | agentd ran correctly but the task **cannot** be done / refused | **non-retriable** |
| `6` | a required MCP server failed to connect / handshake / died | retriable |
| `7` | budget exceeded (steps / tokens / deadline / tree) | policy |
| `124` | supervisor hard-kill backstop (a child that won't self-terminate; a self-detected `--deadline` is `7`) | — |
| `137` | killed by `SIGKILL` (OOM / kubelet) — OS-set | raise memory limit |
| `143` | killed by `SIGTERM` **without** clean drain — OS-set | distinguishes ungraceful from `0` |

agentd never `exit(137)`/`exit(143)` itself — the kernel sets those when it
kills the process. A clean drain returns `0`.

### The top footgun: drain timeout < grace

> **`AGENT_DRAIN_TIMEOUT` (default 25s) MUST be `<`
> `terminationGracePeriodSeconds` (default 30s).**

If your drain budget is `>=` the pod's grace period, the kubelet sends
`SIGKILL` **before** agentd finishes draining — you lose the clean exit (it
becomes `137`/`143`), in-flight subagents are not wound down at turn boundaries,
and a rolled `Deployment` shows failures instead of clean `0`s. Always keep the
internal budget the **smaller** number, with headroom for the kill-ladder rung
plus the log flush.

agentd validates this where it can: a `drain_timeout >= 30s` (the K8s default
grace) emits a loud warning at startup (RFC 0011 §3.3). Set both explicitly and
keep the gap:

```yaml
spec:
  terminationGracePeriodSeconds: 30   # kubelet grace
  containers:
    - name: agent
      args: ["--drain-timeout", "25s", …]   # < 30s, with headroom
```

### 4a. Job — run once

A **job** (the default lifecycle — no `--mode` in 2.0); it runs to a terminal
status and exits. Use `podFailurePolicy` to turn the exit-code table into retry
decisions:

```yaml
apiVersion: batch/v1
kind: Job
metadata:
  name: agent-digest
spec:
  backoffLimit: 3
  podFailurePolicy:
    rules:
      # Config / usage error and a deterministic refusal are operator bugs —
      # never retry them.
      - action: FailJob
        onExitCodes: { operator: In, values: [2, 5] }
  template:
    spec:
      restartPolicy: Never
      terminationGracePeriodSeconds: 30
      containers:
        - name: agent
          image: ghcr.io/example/agent:1.0.0
          args:                          # a bare instruction = a `once` job
            - --instruction-file=/etc/agentd/task.txt
            - --intelligence=https://gw.example/v1
          env:
            - { name: AGENT_INTELLIGENCE_TOKEN, valueFrom: { secretKeyRef: { name: intel, key: token } } }
            - { name: AGENT_LIFECYCLE_RUN_ID, value: "digest-2026-06-25" }   # stable → idempotent retries
```

Pin `AGENT_RUN_ID` to a stable per-unit-of-work value (e.g. derived from the
Job name) so retries dedupe through your MCP backing services.

### 4b. CronJob — on a schedule

Prefer an **external** `CronJob` firing a `once` job per tick over an in-agent
`schedule` start node — it is more robust, observable, and 12-factor (RFC 0011
§9). agentd's internal `schedule` node is a standalone convenience, not a
calendar (no DST/missed-tick catch-up; UTC).

```yaml
apiVersion: batch/v1
kind: CronJob
metadata:
  name: agent-nightly
spec:
  schedule: "0 2 * * *"
  jobTemplate:
    spec:
      backoffLimit: 2
      template:
        spec:
          restartPolicy: Never
          terminationGracePeriodSeconds: 30
          containers:
            - name: agent
              image: ghcr.io/example/agent:1.0.0
              args:
                - --instruction-file=/etc/agentd/nightly.txt
                - --intelligence=https://gw.example/v1
```

### 4c. Deployment — a long-lived daemon

A long-lived Pod (`lifecycle.run_until: drained`) that idles on its triggers (an
A2A listener, a `subscribe` start node) and survives rolls cleanly because a
clean drain exits `0`. It mounts a v2 config (see §2, incl. a durable `store`).

```yaml
apiVersion: apps/v1
kind: Deployment
metadata:
  name: agent-triage
spec:
  replicas: 1
  selector: { matchLabels: { app: agent-triage } }
  template:
    metadata: { labels: { app: agent-triage } }
    spec:
      terminationGracePeriodSeconds: 30   # > --drain-timeout
      containers:
        - name: agent
          image: ghcr.io/example/agent:1.0.0
          args:
            - --config=/etc/agentd/triage.yaml   # the §2 daemon config (store + subscribe workflow)
          livenessProbe:
            # The reactor heartbeats the health file; a wedged reactor goes stale.
            exec: { command: ["/bin/sh", "-c", "test $(( $(date +%s) - $(stat -c %Y /run/agent/health) )) -lt 30"] }
            periodSeconds: 10
          # If built/served with the HTTP health surface (RFC 0010), use instead:
          #   httpGet: { path: /healthz, port: 8080 }
          resources:
            limits: { memory: "512Mi" }   # 137 on OOM → raise this
```

Note the liveness probe targets the **supervisor reactor**, not the agentic
work — a subagent legitimately busy on a long tool call must not flip pod
liveness (RFC 0003 §3.4, RFC 0010). Set `resources.limits.memory` deliberately:
aggregate subtree memory is a cgroup/pod concern, not enforced in-binary, so an
OOM surfaces as `137` and means "raise the limit" (RFC 0003 §3.8).

### 4d. StatefulSet — a fleet (client-side sharding removed in 2.0)

> The 1.x `cluster` feature — `--shard K/N` hash partitioning, work-claim leases,
> and standby pools (RFC 0019) — was **removed** in the mode cut-over. In 2.0,
> scale by running multiple daemon replicas that coordinate through their
> **durable store** — each work item is a durable entity with CAS, so two workers
> cannot both commit it (RFC 0025 §3.4) — and the **A2A** channel, rather than a
> client-side shard gate. A `StatefulSet` (stable identity, a distinct
> `lifecycle.run_id` per ordinal) is still the right shape for a fleet of stateful
> daemons; give each the same `--config` and let the store arbitrate ownership.

### 4e. Hot reload via a ConfigMap (`hot-reload` / `config-watch` features)

A daemon can apply a new **reloadable** config subset without a restart
(RFC 0017 §5; see [`configuration.md`](configuration.md) §11 for the
reloadable-vs-restart-only partition). Mount the config file from a ConfigMap and
either send `SIGHUP` or run `--watch-config`:

- **`--watch-config`** (`config-watch` feature) arms an `inotify` watch on the
  config file's directory. A `kubectl apply` of the ConfigMap is an atomic
  volume-symlink swap, which the watch sees — agentd re-reads, **validates**, and
  applies the reloadable subset (model, the intelligence endpoint list, limits,
  `subscribe`, `log_level`, `mcp_servers` re-handshake) in place. An invalid
  candidate keeps the running config — nothing is half-applied. A diff that
  touches a **restart-only** field (mode, run-id, serve-mcp, drain, shard,
  claim/standby, continue topology) is **refused** with `reason="restart_required"`
  (roll the pod).
- **`SIGHUP`** (`hot-reload` feature) is the portable trigger if you would rather
  signal than watch: `kubectl exec … -- kill -HUP 1`, or an operator that signals
  after editing the ConfigMap.

```yaml
spec:
  template:
    spec:
      containers:
        - name: agent
          image: ghcr.io/agentd-dev/agentd:1.0.0   # built with config-watch
          args:
            - --mode=reactive
            - --config=/etc/agentd/config.json      # mounted from the ConfigMap
            - --watch-config                        # reload on a ConfigMap update
            - --instruction-file=/etc/agentd/task.txt
            - --metrics-addr=:9090
            - --drain-timeout=25s
          volumeMounts:
            - { name: config, mountPath: /etc/agentd, readOnly: true }
      volumes:
        - name: config
          configMap: { name: agent-config }        # holds config.json (+ task.txt)
```

Secrets never live in the ConfigMap: the file carries only structural config and
`{{secret:NAME}}` / `{{secret-file:PATH}}` references, resolved from env vars or
mounted Secret files at load/reload ([`configuration.md`](configuration.md) §12).

### Management over HTTPS + a node-agent

The same `--serve-mcp` listener that exposes the self-MCP also carries the
**management transport** — status, subagent introspection, the operator control
family (`a2a.Drain`/`LameDuck`/`Pause`/`Resume`/`Cancel`), and (with `--features
a2a`) the A2A method surface — over **HTTP(S)**. Trust is minted per request by
**mutual TLS** (`--serve-cert`/`--serve-key`/`--serve-client-ca`) or a **bearer
token** (`--serve-bearer`), never by the transport; a non-loopback bind that
configures neither is a startup error (no open control plane). The controller (the
thing that issues management RPCs, signals reloads, and reads `agent://` resources —
e.g. `agentctl`) is **external** and not part of agentd; it presents a client cert
or bearer and agentd honours the authenticated-identity contract.

---

## Runnable manifests

See [`examples/k8s/`](../examples/k8s/) for the manifests above as standalone
files:

- `examples/k8s/job-once.yaml` — one-shot `Job` with `podFailurePolicy`
- `examples/k8s/cronjob-schedule.yaml` — scheduled `CronJob`
- `examples/k8s/deployment-reactive.yaml` — reactive `Deployment` with HTTP probes
- `examples/docker/Dockerfile` — the static-on-scratch image
- `examples/systemd-agentd.service` — reactive systemd unit

---

## See also

- [`docs/configuration.md`](configuration.md): the **complete** flag/env
  reference, the config-file schema (§12), the reloadable-vs-restart-only
  partition (§11), and the `cluster` sharding/claim/standby surface (§13).
- [RFC 0011 — cloud-native contract](../rfcs/0011-cloud-native-contract.md):
  config precedence, signals, the exit-code table, idempotency.
- [RFC 0003 — process supervision & recovery](../rfcs/0003-process-supervision-and-recovery.md):
  the kill ladder, reaping, restart governor, rebuild + reconcile.
- [RFC 0008 — modes & reactive routing](../rfcs/0008-execution-modes-and-reactive-routing.md):
  the exit predicates per mode, read-after-subscribe.
- [RFC 0017 — declarative config & hot reload](../rfcs/0017-declarative-config-and-hot-reload.md):
  the config file, `--validate-config`/`--config-schema`, SIGHUP/`--watch-config`.
- [RFC 0018 — intelligence transport resilience](../rfcs/0018-intelligence-transport-resilience.md):
  the endpoint list, per-endpoint creds, `--model-swap`.
- [RFC 0019 — horizontal scaling](../rfcs/0019-horizontal-scaling.md):
  sharding, work-claim leases, standby pools.
- [`docs/design/PLAN.md`](design/PLAN.md): current build status.
