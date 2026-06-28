# Deploying agent

`agent` is one binary that runs **one agent**. An external scheduler starts,
stops, replicates, and watches it; the binary itself owns no control plane
(RFC 0011 §1). This page is a set of deployment recipes for the v1 target:

1. [Standalone CLI — one-shot](#1-standalone-cli--one-shot)
2. [Long-lived reactive daemon](#2-long-lived-reactive-daemon)
3. [Container — minimal scratch/distroless image](#3-container--minimal-scratchdistroless-image)
4. [Scheduled by an external orchestrator (Kubernetes)](#4-scheduled-by-an-external-orchestrator-kubernetes)

The same supervisor loop backs every shape; they differ only in the **exit
predicate** — and therefore in which exit codes are reachable (RFC 0011 §7). A
unit of work (`once`) goes empty-and-final and exits; a reactive daemon
(`reactive`) idles on a subscription stream and exits only on signal or a fatal
class.

> **Build status.** The runtime is implemented — config precedence +
> validate-at-startup (exit `2`), the agentic loop, the supervisor + subagent
> process tree, the MCP client, all four run modes (`once`/`loop`/`reactive`/
> `schedule`), reactive routing, and the served self-MCP all ship. The examples
> below describe real behaviour.

Every flag and env var on this page is taken verbatim from
[`crates/agent/src/config.rs`](../crates/agent/src/config.rs) (`agent --help`).
If a flag is not in `--help`, it does not exist. See
[`configuration.md`](configuration.md) for the **complete** flag/env reference,
the config-file schema, and the reloadable-vs-restart-only partition.

---

## The config surface you will actually use

Precedence, top wins: **built-in default < config file < env var < CLI flag**.
The config file (`--config`/`AGENT_CONFIG`) is **live** (RFC 0017) — a local JSON
document for structural config (MCP inventory, subscriptions, limits, the
intelligence endpoint list + headers). Everything else is env-settable;
**secrets are env/flag/mounted-file only, never inline in the config file**
(RFC 0011 §3.2; the file may carry `{{secret:NAME}}` / `{{secret-file:PATH}}`
*references*).

| Concern | Env | Flag |
|---|---|---|
| Instruction | `INSTRUCTION` | `--instruction <TEXT>` / `--instruction-file <PATH>` |
| Config file | `AGENT_CONFIG` | `--config <PATH>` |
| Intelligence list | `AGENT_INTELLIGENCE` | `--intelligence unix:/… │ https://… │ vsock:cid:port` (comma-list = failover) |
| Intelligence creds | `AGENT_INTELLIGENCE_TOKEN` / `…_FILE`, `…_<N>` / `…_<N>_FILE` | `--intelligence-token <T>` / `--intelligence-token-file <PATH>` |
| Model / swap policy | `AGENT_MODEL` / `AGENT_MODEL_SWAP` | `--model <NAME>` / `--model-swap finish-on-old│restart-turn` |
| MCP server | — | `--mcp name=command …` (repeatable, stdio) |
| Serve self-MCP | `AGENT_SERVE_MCP` | `--serve-mcp unix:/… │ vsock:PORT │ vsock:CID:PORT` (`serve-mcp` feat.) |
| A2A peer | `AGENT_A2A_PEER` | `--a2a-peer name=endpoint` (repeatable; `a2a` feat.) |
| Enable exec tool | `AGENT_ENABLE_EXEC` (`:`-list) | `--enable-exec <abs-path>` (repeatable allowlist) |
| Mode | `AGENT_MODE` | `--mode once│loop│reactive│schedule` |
| Subscriptions | — | `--subscribe <uri>` / `--continue <uri>` (repeatable; reactive) |
| Interval / cron | `AGENT_CRON` | `--interval <dur>` / `--cron <5-field>` (`cron` feat.) |
| **Sharding** | `AGENT_SHARD`, `AGENT_SHARD_TIMER` | `--shard K/N` (`cluster` feat.) |
| **Work-claim** | `AGENT_CLAIM_TTL`, `AGENT_CLAIM_RENEW_FRACTION` | `--claim <uri>=<srv>[:tool]`, `--claim-ttl`, `--claim-renew-fraction` (`cluster` feat.) |
| **Standby** | `AGENT_STANDBY`, `AGENT_ASSIGN_FROM`, `AGENT_WARM_INTEL` | `--standby`, `--assign-from <srv>:<uri>` (`cluster` feat.) |
| Max steps | `AGENT_MAX_STEPS` | `--max-steps <N>` (default 50) |
| Max tokens | `AGENT_MAX_TOKENS` | `--max-tokens <N>` (default 200000) |
| Deadline | `AGENT_DEADLINE` | `--deadline <dur>` (default 600s) |
| Max depth | — | `--max-depth <N>` (default 4) |
| **Run ID** | `AGENT_RUN_ID` | `--run-id <ID>` (idempotency key) |
| Log level | `AGENT_LOG_LEVEL` | `--log-level trace│debug│info│warn│error` |
| Log content | `AGENT_LOG_CONTENT` | `--log-content` |
| **Drain timeout** | `AGENT_DRAIN_TIMEOUT` | `--drain-timeout <dur>` (default 25s) |
| Health file | — | `--health-file <PATH>` |
| Metrics/probes | `AGENT_METRICS_ADDR` | `--metrics-addr host:port` (`metrics` feat.) |
| Per-run cgroup | `AGENT_CGROUP` / `…_MEMORY_MAX` / `…_PIDS_MAX` | `--cgroup auto│PATH`, `--cgroup-memory-max`, `--cgroup-pids-max` |
| Report / events | `AGENT_REPORT_FILE`, `AGENT_EVENTS_RING` | `--report-file <PATH>`, `--events-ring <N>` |
| **Hot reload** | `AGENT_WATCH_CONFIG` | `--watch-config` (`config-watch` feat.) + SIGHUP (`hot-reload` feat.) |

Durations accept `ms`/`s`/`m`/`h` or a bare integer (seconds): `600s`, `5m`,
`2h`, `250ms`, `30`. Each intelligence list element must be `unix:/path`,
`https://host/…`, or `vsock:cid:port` (`http://` is dev-only and the client
warns). Config is validated **before any side effect** — a typo'd flag, a
feature-gated flag in a build without its feature, or an unresolvable secret
reference exits `2` in milliseconds, not after an LLM round-trip.

> **Scope markers.** Reactivity is **stdio MCP only** (subscriptions ride stdio
> MCP server children); self-MCP serving is **unix/vsock** (no HTTP serving of
> the self-MCP); MCP tasks/sampling/roots are deferred (RFC 0013). The
> `cluster` work-claim `:resource` style is a stub (`:tool` is the working
> style), and `AGENT_WARM_INTEL` is forward-compat only — see
> [`configuration.md`](configuration.md) §13. Items below are tagged where they
> do not ship.

---

## 1. Standalone CLI — one-shot

The default mode (`--mode once`, the default). Run an instruction to a terminal
status, emit the result on **stdout**, write telemetry to **stderr**, exit with
a code from the [exit-code table](#the-exit-code-contract).

```bash
agent \
  --instruction "Summarise today's open incidents and post a digest." \
  --intelligence unix:/run/intelligence.sock \
  --model my-model \
  --mcp incidents="mcp-server-http --base https://incidents.internal" \
  --mcp slack="mcp-server-slack" \
  --deadline 5m \
  --max-steps 40
```

stdout carries the agent's final result; stderr carries one NDJSON event per
line. The canonical fields are
`ts level event run_id agent_id agent_path comp pid …` (RFC 0010):

```json
{"ts":"2026-06-25T18:30:01.412Z","level":"info","event":"proc.start","run_id":"0197f3c4a01abcd","agent_id":"sup","agent_path":"0","comp":"supervisor","pid":4711,"version":"2.0.0","mode":"once","mcp_servers":2,"subscribe":0}
```

Because stdout is the result and stderr is telemetry, you compose with ordinary
shell tooling:

```bash
agent --instruction "$(cat task.md)" --intelligence unix:/run/intel.sock \
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
agent --run-id "nightly-digest-2026-06-25" \
  --instruction "$(cat task.md)" --intelligence unix:/run/intel.sock --mcp …
```

The key rides in the `_meta` of every outbound MCP `tools/call`; a backing
service that honours idempotency keys collapses a retried effect to one. agent
itself writes nothing durable except its log streams, so a re-run is safe by
construction.

---

## 2. Long-lived reactive daemon

`--mode reactive` idles cheaply and wakes on MCP **resource subscription**
updates (RFC 0008). It exits only on a signal or a fatal class — never on an
individual reaction failing.

```bash
agent \
  --mode reactive \
  --instruction "When a ticket is filed, triage it and assign an owner." \
  --intelligence unix:/run/intelligence.sock \
  --model my-model \
  --mcp tickets="mcp-server-tickets --watch" \
  --subscribe "tickets://queue/inbound" \
  --drain-timeout 25s \
  --health-file /run/agent/health
```

`--mode reactive` **requires at least one `--subscribe <uri>`** (validated;
omitting it exits `2`). On restart the daemon re-reads config, re-handshakes
MCP, re-subscribes every declared subscription, and does a mandatory
read-after-subscribe so a change that happened while it was down is still acted
on (RFC 0003 §3.11). No persistence layer — a restart is a cold start that
reconciles.

> **(roadmap)** v1 reactivity is **stdio MCP only**: subscriptions are served by
> stdio MCP server children, not over HTTP. Reactive-over-HTTP is deferred.

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
Environment=AGENT_INTELLIGENCE=unix:/run/intelligence.sock
Environment=AGENT_INTELLIGENCE_TOKEN=
EnvironmentFile=/etc/agent/triage.env
ExecStart=/usr/local/bin/agent \
  --mode reactive \
  --instruction-file /etc/agent/triage.txt \
  --mcp tickets=mcp-server-tickets \
  --subscribe tickets://queue/inbound \
  --drain-timeout 25s
# Give the drain room: must exceed AGENT_DRAIN_TIMEOUT.
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

agent is `std` + `libc`, statically linkable, with no async runtime, no C
toolchain, and **no built-in tools** — so the image is tiny (~1.3 MB on
`scratch`). The recommended entrypoint is `agent` itself: it sets
`PR_SET_CHILD_SUBREAPER` and reaps orphans, acting as a tini-class init for its
own process tree (RFC 0003 §3.1). You do **not** need an external `tini`.

The published image (`Dockerfile` at the repo root) ships the **dependency-free
cloud-native feature set** by default —
`FEATURES="metrics,serve-mcp,cron,otel,cluster,hot-reload,config-watch"`. Every
one is hand-rolled and adds **no** dependency (serde/serde_json + libc only, 3
deps; no async runtime, no TLS, no C toolchain), so the binary stays the
minimalism target. What each adds:

| Feature | Adds |
|---|---|
| `metrics` | The `/metrics` + `/healthz` + `/readyz` HTTP probe surface (`--metrics-addr`) — so k8s liveness/readiness probes work against a shell-less scratch image. |
| `serve-mcp` | agent serving its own MCP (`--serve-mcp`) so other agents compose with it; also the substrate for `events`/`a2a`/the capacity surface. |
| `cron` | UTC 5-field cron scheduling for `--mode schedule` (`--cron`). |
| `otel` | OTLP-over-HTTP/JSON span export + GenAI semconv (hand-rolled, no protobuf/opentelemetry deps). |
| `cluster` | Horizontal scaling: `--shard K/N` partitioning, work-claim leases (`--claim`), standby pools (`--standby`/`--assign-from`), the autoscaling signal set, and the `agent://capacity` read surface. |
| `hot-reload` | SIGHUP-triggered, validate-first reload of the reloadable config subset at a reactive quiesce boundary. |
| `config-watch` | The `inotify` file-watch reload trigger (`--watch-config`) — a ConfigMap volume swap reloads in place. Implies `hot-reload`. |

Build a narrower (or wider) surface with `--build-arg FEATURES=…`. `tls` and
`vsock` are **not** in the default set (they change the dial transport — see the
TLS note below); `events`/`a2a` ride `serve-mcp`; `FEATURES=` builds the pure,
flag-free minimal binary.

```dockerfile
# syntax=docker/dockerfile:1
# Static musl binary on scratch — the dependency-free cloud-native feature set.
FROM rust:1-alpine AS build
ARG FEATURES="metrics,serve-mcp,cron,otel,cluster,hot-reload,config-watch"
RUN apk add --no-cache musl-dev
WORKDIR /src
COPY . .
# Alpine's host target IS <arch>-unknown-linux-musl, so the release binary is
# static; one Dockerfile yields native-static amd64 AND arm64 via buildx.
RUN if [ -n "$FEATURES" ]; then \
      cargo build --release --locked -p agentd --features "$FEATURES"; \
    else \
      cargo build --release --locked -p agentd; \
    fi

# scratch: nothing but the binary. (Swap for gcr.io/distroless/static if you
# want a CA bundle + /etc/passwd without managing them yourself.)
FROM scratch
COPY --from=build /src/target/release/agent /agent
# Non-root by uid (scratch has no /etc/passwd; the kernel uses the number).
USER 65532:65532
# MCP server binaries are part of the agent's toolset — add them alongside:
# COPY --from=build /path/to/mcp-server-tickets /usr/local/bin/
ENTRYPOINT ["/agent"]
```

> **Build-arg, not flag.** `FEATURES` selects what the **binary** can do; it is a
> compile-time choice, not a runtime flag. A runtime flag that needs an unbuilt
> feature exits `2` — e.g. `--shard 2/8` on an image built without `cluster`.

### TLS is off by default — terminate it in a sidecar

The default build has **no TLS**. The intended container posture is **plaintext
inside the pod, TLS at the boundary**:

- Point `--intelligence` at a **`unix:` socket** shared with a sidecar (or the
  host) that terminates TLS to the real endpoint, or
- Use **`vsock:cid:port`** to reach an intelligence endpoint on the host /
  enclave (build with **`--features vsock`**), or
- Build with **`--features tls`** to dial `https://` directly (rustls + bundled
  roots; adds the one heavier dependency).

```bash
# In-pod: agent talks plaintext over a unix socket to a TLS-terminating sidecar.
agent --intelligence unix:/run/intel/intel.sock --instruction-file /etc/task.txt --mcp …
```

This keeps the default image at scratch-size with no certificate management in
the agent process.

### Health surface

Two options, both live:

- **`--metrics-addr host:port`** (`metrics` feature, in the default image) serves
  `/healthz` + `/readyz` + `/metrics` over HTTP. This is the right choice for the
  **scratch image**, which has no shell to run an exec probe: point the k8s
  liveness probe at `/healthz` and readiness at `/readyz`. The bare `:port` form
  binds all IPv4 interfaces so the kubelet reaches it at the pod IP. (See the K8s
  probes below.)
- **`--health-file <PATH>`** — agent heartbeats it while the reactor is live, so
  an exec-style probe can `test` its freshness. Useful where you do not want an
  HTTP listener at all.

`/healthz` returns 200 while the **supervisor** tick is fresh and 503 once it
goes stale; `/readyz` flips to not-ready on drain so the pod leaves rotation. An
idle reactive agent is healthy — liveness tracks the supervisor, not whether work
is flowing.

> **Self-MCP scope.** `--serve-mcp` lets other agents compose with this one over
> MCP and is **unix/vsock only** (`serve-mcp` + optionally `vsock` features) —
> there is no HTTP serving of the self-MCP (RFC 0013). The same unix/vsock
> listener also carries the management transport and (with `--features a2a`) the
> A2A method surface — see [the management transport](#management-over-vsock--a-node-agent)
> below. Do not expose `--serve-mcp` over a TCP port.

---

## 4. Scheduled by an external orchestrator (Kubernetes)

The orchestrator (a K8s operator, Knative, Nomad, a bare-metal supervisor) is
**not part of this project** (RFC 0011 §1). agent just honours a contract:
config from env/flags, signal-driven drain, and a public exit-code table a
`podFailurePolicy` can branch on. Below are the three shapes; runnable manifests
live in [`examples/`](../examples/).

### The exit-code contract

This table is a **stable, machine-actionable API** — author `podFailurePolicy`
against it (RFC 0011 §5; constants in
[`crates/agent/src/exit.rs`](../crates/agent/src/exit.rs)):

| Code | Meaning | Scheduler hint |
|---|---|---|
| `0` | success — one-shot done / clean bound / **clean SIGTERM drain** | Complete |
| `1` | generic / unspecified failure | retriable |
| `2` | config / usage error (validation failed) | **non-retriable** → `FailJob` |
| `3` | partial result (useful output, some sub-tasks failed) | policy |
| `4` | intelligence endpoint unreachable / auth after retries | retriable |
| `5` | agent ran correctly but the task **cannot** be done / refused | **non-retriable** |
| `6` | a required MCP server failed to connect / handshake / died | retriable |
| `7` | budget exceeded (steps / tokens / deadline / tree) | policy |
| `124` | hard wall-clock deadline (`--deadline`) tripped | — |
| `137` | killed by `SIGKILL` (OOM / kubelet) — OS-set | raise memory limit |
| `143` | killed by `SIGTERM` **without** clean drain — OS-set | distinguishes ungraceful from `0` |

agent never `exit(137)`/`exit(143)` itself — the kernel sets those when it
kills the process. A clean drain returns `0`.

### The top footgun: drain timeout < grace

> **`AGENT_DRAIN_TIMEOUT` (default 25s) MUST be `<`
> `terminationGracePeriodSeconds` (default 30s).**

If your drain budget is `>=` the pod's grace period, the kubelet sends
`SIGKILL` **before** agent finishes draining — you lose the clean exit (it
becomes `137`/`143`), in-flight subagents are not wound down at turn boundaries,
and a rolled `Deployment` shows failures instead of clean `0`s. Always keep the
internal budget the **smaller** number, with headroom for the kill-ladder rung
plus the log flush.

agent validates this where it can: a `drain_timeout >= 30s` (the K8s default
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

`--mode once`; the Job runs to a terminal status and exits. Use
`podFailurePolicy` to turn the exit-code table into retry decisions:

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
          image: ghcr.io/example/agent:2.0.0
          args:
            - --mode=once
            - --instruction-file=/etc/agent/task.txt
            - --intelligence=unix:/run/intel/intel.sock
            - --drain-timeout=25s
          env:
            - { name: AGENT_INTELLIGENCE_TOKEN, valueFrom: { secretKeyRef: { name: intel, key: token } } }
            - { name: AGENT_RUN_ID, value: "digest-2026-06-25" }   # stable → idempotent retries
```

Pin `AGENT_RUN_ID` to a stable per-unit-of-work value (e.g. derived from the
Job name) so retries dedupe through your MCP backing services.

### 4b. CronJob — on a schedule

Prefer an **external** `CronJob` firing `--mode once` per tick over the internal
`--mode schedule`/`--interval` — it is more robust, observable, and 12-factor
(RFC 0011 §9). agent's internal scheduler is a standalone convenience, not a
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
              image: ghcr.io/example/agent:2.0.0
              args:
                - --mode=once
                - --instruction-file=/etc/agent/nightly.txt
                - --intelligence=unix:/run/intel/intel.sock
                - --drain-timeout=25s
```

### 4c. Deployment — reactive daemon

`--mode reactive`. A long-lived Pod that idles on subscriptions and survives
rolls cleanly because a clean drain exits `0`.

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
          image: ghcr.io/example/agent:2.0.0
          args:
            - --mode=reactive
            - --instruction-file=/etc/agent/triage.txt
            - --intelligence=unix:/run/intel/intel.sock
            - --subscribe=tickets://queue/inbound
            - --drain-timeout=25s
            - --health-file=/run/agent/health
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

### 4d. StatefulSet — a sharded reactive fleet (`cluster` feature)

To process a shared workload across N replicas without duplicating it, run a
**sharded fleet** (RFC 0019; needs an image built with `--features cluster`). The
idiom maps a **StatefulSet pod ordinal → `AGENT_SHARD=K/N`**: each pod owns shard
`K` of `N`, and `replicas` is `N`. The ordinal is in the pod's hostname
(`agent-shard-0`, `-1`, …), so the container derives `K` from it; agentctl
injects `AGENT_SHARD` the same way. Shard identity is **restart-only** — a
reload never changes it (a re-shard is a rolling restart).

```yaml
apiVersion: apps/v1
kind: StatefulSet
metadata:
  name: agent-shard
spec:
  serviceName: agent-shard
  replicas: 8                       # == N (the shard count)
  selector: { matchLabels: { app: agent-shard } }
  template:
    metadata: { labels: { app: agent-shard } }
    spec:
      terminationGracePeriodSeconds: 30   # > --drain-timeout
      containers:
        - name: agent
          image: ghcr.io/agentd-dev/agent:2.7.0   # built with `cluster`
          # Derive K from the StatefulSet ordinal (the trailing -N of the hostname)
          # and export AGENT_SHARD=K/8 before exec'ing agent.
          command: ["/bin/sh", "-c"]   # use a shell image, or bake this into an entrypoint
          args:
            - |
              K="${HOSTNAME##*-}"
              exec /agent --mode reactive \
                --instruction-file /etc/agent/task.txt \
                --intelligence unix:/run/intel/intel.sock \
                --subscribe tickets://queue/inbound \
                --metrics-addr :9090 --drain-timeout 25s
          env:
            - name: AGENT_SHARD
              value: "0/8"            # overwritten below from the ordinal…
          # …or, on the scratch image (no shell), set AGENT_SHARD directly per
          # pod via a small admission/operator that reads the ordinal. The shard
          # gate is the only sharding wiring agent needs.
```

Because shard ownership is a pure FNV-1a gate over the resource URI/key, the
replicas are share-nothing: each independently subscribes to the full set and
**processes only the items hashing to its shard**. For at-most-once processing of
an item that more than one replica could see, layer **work-claim** on top
(`--claim <uri>=<coord-server>`, [`configuration.md`](configuration.md) §13) so a
lease — not just the hash — decides ownership. `AGENT_SHARD_TIMER` (`shard0` |
`keyed`) controls timer firing for a sharded `schedule`/`loop` fleet.

### 4e. Hot reload via a ConfigMap (`hot-reload` / `config-watch` features)

A reactive daemon can apply a new **structural** config without a restart
(RFC 0017 §5; see [`configuration.md`](configuration.md) §11 for the
reloadable-vs-restart-only partition). Mount the config file from a ConfigMap and
either send `SIGHUP` or run `--watch-config`:

- **`--watch-config`** (`config-watch` feature) arms an `inotify` watch on the
  config file's directory. A `kubectl apply` of the ConfigMap is an atomic
  volume-symlink swap, which the watch sees — agent re-reads, **validates**, and
  applies the reloadable subset (model, the intelligence endpoint list, limits,
  `subscribe`, `log_level`, `mcp_servers` re-handshake) in place. An invalid
  candidate keeps the running config — nothing is half-applied. A diff that
  touches a **restart-only** field (mode, run-id, serve-mcp, exec, drain, shard,
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
          image: ghcr.io/agentd-dev/agent:2.7.0   # built with config-watch
          args:
            - --mode=reactive
            - --config=/etc/agent/config.json      # mounted from the ConfigMap
            - --watch-config                        # reload on a ConfigMap update
            - --instruction-file=/etc/agent/task.txt
            - --metrics-addr=:9090
            - --drain-timeout=25s
          volumeMounts:
            - { name: config, mountPath: /etc/agent, readOnly: true }
      volumes:
        - name: config
          configMap: { name: agent-config }        # holds config.json (+ task.txt)
```

Secrets never live in the ConfigMap: the file carries only structural config and
`{{secret:NAME}}` / `{{secret-file:PATH}}` references, resolved from env vars or
mounted Secret files at load/reload ([`configuration.md`](configuration.md) §12).

### Management over vsock + a node-agent

The same `--serve-mcp` listener that exposes the self-MCP also carries the
**management transport** — status, subagent introspection, and (with
`--features a2a`) the A2A method surface — over a **unix socket or AF_VSOCK port**
(`--serve-mcp vsock:PORT`, `--features vsock`). vsock is the right transport when
a host/enclave **node-agent** drives a guest agent across a VM boundary
(Firecracker/Kata): no shared filesystem, no TCP port to firewall. There is **no
HTTP** management surface — keep this off any TCP port. The node-agent (the thing
that issues management RPCs, signals reloads, and reads `agent://` resources) is
**external** and not part of agent; agent only honours the transport contract.

---

## Runnable manifests

See [`examples/k8s/`](../examples/k8s/) for the manifests above as standalone
files:

- `examples/k8s/job-once.yaml` — one-shot `Job` with `podFailurePolicy`
- `examples/k8s/cronjob-schedule.yaml` — scheduled `CronJob`
- `examples/k8s/deployment-reactive.yaml` — reactive `Deployment` with HTTP probes
- `examples/docker/Dockerfile` — the static-on-scratch image
- `examples/systemd-agent.service` — reactive systemd unit

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
