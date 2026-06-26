# Deploying agentd

`agentd` is one binary that runs **one agent**. An external scheduler starts,
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
[`crates/agentd/src/config.rs`](../crates/agentd/src/config.rs) (`agentd --help`).
If a flag is not in `--help`, it does not exist in v1.

---

## The config surface you will actually use

Precedence, top wins: **built-in default < env var < CLI flag** (a config-file
layer slots between default and env in a later milestone). Everything is
env-settable; **secrets are env/flag only, never a file** (RFC 0011 §3.2).

| Concern | Env | Flag |
|---|---|---|
| Instruction | `INSTRUCTION` | `--instruction <TEXT>` / `--instruction-file <PATH>` |
| Intelligence transport | `AGENTD_INTELLIGENCE` | `--intelligence unix:/… │ https://… │ vsock:cid:port` |
| Intelligence creds | `AGENTD_INTELLIGENCE_TOKEN` | `--intelligence-token <T>` |
| Model | `AGENTD_MODEL` | `--model <NAME>` |
| MCP server | — | `--mcp name=command …` (repeatable, stdio) |
| Serve self-MCP | `AGENTD_SERVE_MCP` | `--serve-mcp unix:/…` |
| Enable exec tool | `AGENTD_ENABLE_EXEC` | `--enable-exec` |
| Mode | `AGENTD_MODE` | `--mode once│loop│reactive│schedule` |
| Subscriptions | — | `--subscribe <uri>` (repeatable; reactive) |
| Interval | — | `--interval <dur>` (loop/schedule) |
| Max steps | `AGENTD_MAX_STEPS` | `--max-steps <N>` (default 50) |
| Max tokens | `AGENTD_MAX_TOKENS` | `--max-tokens <N>` (default 200000) |
| Deadline | `AGENTD_DEADLINE` | `--deadline <dur>` (default 600s) |
| Max depth | — | `--max-depth <N>` (default 4) |
| **Run ID** | `AGENTD_RUN_ID` | `--run-id <ID>` (idempotency key) |
| Log level | `AGENTD_LOG_LEVEL` | `--log-level trace│debug│info│warn│error` |
| **Drain timeout** | `AGENTD_DRAIN_TIMEOUT` | `--drain-timeout <dur>` (default 25s) |
| Health file | — | `--health-file <PATH>` |

Durations accept `ms`/`s`/`m`/`h` or a bare integer (seconds): `600s`, `5m`,
`2h`, `250ms`, `30`. The intelligence URI must be `unix:/path`,
`https://host/…`, or `vsock:cid:port` (`http://` is dev-only and the client
warns). Config is validated **before any side effect** — a typo'd flag exits `2`
in milliseconds, not after an LLM round-trip.

> **Roadmap markers.** v1 reactivity is **stdio-only** (no reactive-over-HTTP);
> self-MCP serving is **stdio/unix only** (HTTP serving deferred); MCP
> tasks/sampling/roots are deferred (RFC 0013). Items below are tagged
> **(roadmap)** where they do not ship in v1.

---

## 1. Standalone CLI — one-shot

The default mode (`--mode once`, the default). Run an instruction to a terminal
status, emit the result on **stdout**, write telemetry to **stderr**, exit with
a code from the [exit-code table](#the-exit-code-contract).

```bash
agentd \
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
{"ts":"2026-06-25T18:30:01.412Z","level":"info","event":"proc.start","run_id":"0197f3c4a01abcd","agent_id":"sup","agent_path":"0","comp":"supervisor","pid":4711,"version":"0.1.0","mode":"once","mcp_servers":2,"subscribe":0}
```

Because stdout is the result and stderr is telemetry, you compose with ordinary
shell tooling:

```bash
agentd --instruction "$(cat task.md)" --intelligence unix:/run/intel.sock \
  2> >(jq -c 'select(.level=="error")') \
  | tee result.txt
```

Read the instruction from a file (handy for ConfigMap/Secret projection) with
`--instruction-file`, or set `INSTRUCTION` in the environment. The intelligence
token is **never** logged — pass it via `AGENTD_INTELLIGENCE_TOKEN` or
`--intelligence-token`, not on a shared command line where it lands in `ps`.

**Idempotent retries.** A bare run mints a random `run_id` per process. For a
unit of work that a scheduler may retry, pin a **stable** key so backing MCP
services can dedupe the side effect (RFC 0011 §6):

```bash
agentd --run-id "nightly-digest-2026-06-25" \
  --instruction "$(cat task.md)" --intelligence unix:/run/intel.sock --mcp …
```

The key rides in the `_meta` of every outbound MCP `tools/call`; a backing
service that honours idempotency keys collapses a retried effect to one. agentd
itself writes nothing durable except its log streams, so a re-run is safe by
construction.

---

## 2. Long-lived reactive daemon

`--mode reactive` idles cheaply and wakes on MCP **resource subscription**
updates (RFC 0008). It exits only on a signal or a fatal class — never on an
individual reaction failing.

```bash
agentd \
  --mode reactive \
  --instruction "When a ticket is filed, triage it and assign an owner." \
  --intelligence unix:/run/intelligence.sock \
  --model my-model \
  --mcp tickets="mcp-server-tickets --watch" \
  --subscribe "tickets://queue/inbound" \
  --drain-timeout 25s \
  --health-file /run/agentd/health
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

The whole drain is bounded by `AGENTD_DRAIN_TIMEOUT` (default 25s). **This MUST
be smaller than the orchestrator's shutdown grace** — see the
[footgun below](#the-top-footgun-drain-timeout--grace).

### As a systemd unit

```ini
# /etc/systemd/system/agentd-triage.service
[Unit]
Description=agentd ticket triage (reactive)
After=network.target

[Service]
Environment=AGENTD_INTELLIGENCE=unix:/run/intelligence.sock
Environment=AGENTD_INTELLIGENCE_TOKEN=
EnvironmentFile=/etc/agentd/triage.env
ExecStart=/usr/local/bin/agentd \
  --mode reactive \
  --instruction-file /etc/agentd/triage.txt \
  --mcp tickets=mcp-server-tickets \
  --subscribe tickets://queue/inbound \
  --drain-timeout 25s
# Give the drain room: must exceed AGENTD_DRAIN_TIMEOUT.
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
toolchain, and **no built-in tools** — so the image is tiny. The recommended
entrypoint is `agentd` itself: it sets `PR_SET_CHILD_SUBREAPER` and reaps
orphans, acting as a tini-class init for its own process tree (RFC 0003 §3.1).
You do **not** need an external `tini`.

```dockerfile
# Build a static musl binary, default feature set (no TLS, no async runtime).
FROM rust:1-alpine AS build
RUN apk add --no-cache musl-dev
WORKDIR /src
COPY . .
RUN cargo build --release -p agentd --target x86_64-unknown-linux-musl

# scratch: nothing but the binary. (Swap for gcr.io/distroless/static if you
# want a CA bundle + /etc/passwd without managing them yourself.)
FROM scratch
COPY --from=build /src/target/x86_64-unknown-linux-musl/release/agentd /agentd
# MCP server binaries are part of the agent's toolset — add them alongside:
# COPY --from=build /path/to/mcp-server-tickets /usr/local/bin/
ENTRYPOINT ["/agentd"]
```

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
# In-pod: agentd talks plaintext over a unix socket to a TLS-terminating sidecar.
agentd --intelligence unix:/run/intel/intel.sock --instruction-file /etc/task.txt --mcp …
```

This keeps the default image at scratch-size with no certificate management in
the agent process.

### Health surface

Pass `--health-file <PATH>`; agentd heartbeats it while the reactor is live, so
an exec-style probe can `test` its freshness. An HTTP `/healthz` listener is
part of the v1 observability surface (RFC 0010) for orchestrators that prefer
HTTP probes; prefer the health file for the scratch image since it needs no
extra listener. (See the K8s probes below.)

> **(roadmap)** `--serve-mcp` lets other agents compose with this one over MCP,
> but v1 serving is **stdio/unix only** — HTTP serving of the self-MCP is
> deferred (RFC 0013). Do not expose `--serve-mcp` over a TCP port in v1.

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
| `5` | agent ran correctly but the task **cannot** be done / refused | **non-retriable** |
| `6` | a required MCP server failed to connect / handshake / died | retriable |
| `7` | budget exceeded (steps / tokens / deadline / tree) | policy |
| `124` | hard wall-clock deadline (`--deadline`) tripped | — |
| `137` | killed by `SIGKILL` (OOM / kubelet) — OS-set | raise memory limit |
| `143` | killed by `SIGTERM` **without** clean drain — OS-set | distinguishes ungraceful from `0` |

agentd never `exit(137)`/`exit(143)` itself — the kernel sets those when it
kills the process. A clean drain returns `0`.

### The top footgun: drain timeout < grace

> **`AGENTD_DRAIN_TIMEOUT` (default 25s) MUST be `<`
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
    - name: agentd
      args: ["--drain-timeout", "25s", …]   # < 30s, with headroom
```

### 4a. Job — run once

`--mode once`; the Job runs to a terminal status and exits. Use
`podFailurePolicy` to turn the exit-code table into retry decisions:

```yaml
apiVersion: batch/v1
kind: Job
metadata:
  name: agentd-digest
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
        - name: agentd
          image: ghcr.io/example/agentd:0.1.0
          args:
            - --mode=once
            - --instruction-file=/etc/agentd/task.txt
            - --intelligence=unix:/run/intel/intel.sock
            - --drain-timeout=25s
          env:
            - { name: AGENTD_INTELLIGENCE_TOKEN, valueFrom: { secretKeyRef: { name: intel, key: token } } }
            - { name: AGENTD_RUN_ID, value: "digest-2026-06-25" }   # stable → idempotent retries
```

Pin `AGENTD_RUN_ID` to a stable per-unit-of-work value (e.g. derived from the
Job name) so retries dedupe through your MCP backing services.

### 4b. CronJob — on a schedule

Prefer an **external** `CronJob` firing `--mode once` per tick over the internal
`--mode schedule`/`--interval` — it is more robust, observable, and 12-factor
(RFC 0011 §9). agentd's internal scheduler is a standalone convenience, not a
calendar (no DST/missed-tick catch-up; UTC).

```yaml
apiVersion: batch/v1
kind: CronJob
metadata:
  name: agentd-nightly
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
            - name: agentd
              image: ghcr.io/example/agentd:0.1.0
              args:
                - --mode=once
                - --instruction-file=/etc/agentd/nightly.txt
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
  name: agentd-triage
spec:
  replicas: 1
  selector: { matchLabels: { app: agentd-triage } }
  template:
    metadata: { labels: { app: agentd-triage } }
    spec:
      terminationGracePeriodSeconds: 30   # > --drain-timeout
      containers:
        - name: agentd
          image: ghcr.io/example/agentd:0.1.0
          args:
            - --mode=reactive
            - --instruction-file=/etc/agentd/triage.txt
            - --intelligence=unix:/run/intel/intel.sock
            - --subscribe=tickets://queue/inbound
            - --drain-timeout=25s
            - --health-file=/run/agentd/health
          livenessProbe:
            # The reactor heartbeats the health file; a wedged reactor goes stale.
            exec: { command: ["/bin/sh", "-c", "test $(( $(date +%s) - $(stat -c %Y /run/agentd/health) )) -lt 30"] }
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

---

## Runnable manifests

See [`examples/`](../examples/) for the manifests above as standalone files:

- `examples/k8s-job.yaml` — one-shot `Job` with `podFailurePolicy`
- `examples/k8s-cronjob.yaml` — scheduled `CronJob`
- `examples/k8s-deployment-reactive.yaml` — reactive `Deployment` with probes
- `examples/Dockerfile` — scratch image
- `examples/systemd-agentd.service` — reactive systemd unit

---

## See also

- [RFC 0011 — cloud-native contract](../rfcs/0011-cloud-native-contract.md):
  config precedence, signals, the exit-code table, idempotency.
- [RFC 0003 — process supervision & recovery](../rfcs/0003-process-supervision-and-recovery.md):
  the kill ladder, reaping, restart governor, rebuild + reconcile.
- [RFC 0008 — modes & reactive routing](../rfcs/0008-execution-modes-and-reactive-routing.md):
  the exit predicates per mode, read-after-subscribe.
- [`docs/design/PLAN.md`](design/PLAN.md): current build status, M1–M3.
