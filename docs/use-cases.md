# Use cases

agentd is a **runtime, not an application**. You don't configure features — you
hand it three things and it runs the agentic loop:

1. an **instruction** (what to do, ending in an explicit output contract),
2. an **intelligence** endpoint (`--intelligence`, the one LLM it talks to),
3. **tools and resources over MCP** (`--mcp name=command …`),

and a **mode** that decides *when* the loop runs. Everything below is the same
binary with those four knobs turned differently. No plugins, no SDK, no
per-use-case code — the use case lives in the instruction and the wiring.

There are two axes to think along:

- **agentd as a single agent** — one supervised subagent runs a task to a
  terminal status. Pick a mode for the trigger shape (run-once, poll, react).
- **agentd orchestrating subagents** — the root agent delegates through the
  `subagent.spawn` chokepoint into a **supervised process tree**: each child gets
  a narrowed objective, a subset of the tools, and a slice of the budget, and
  returns a small distilled result. The process tree *is* the agent tree.

The two compose: a reactive single agent can fan a hard task out to subagents,
and an orchestrator can drive another agentd that is itself reactive.

## Picking a shape

| You want to… | Mode | Deployment shape | Subagents? |
|---|---|---|---|
| Run a task once and exit with a status | `once` | k8s `Job` / CLI / CI step | optional |
| Watch a queue/inbox/resource and act on change | `reactive` | k8s `Deployment` | optional |
| Re-run on a cadence or work-until-done | `loop` | `Deployment` / bounded `Job` | optional |
| Fire on a clock with no orchestrator | `schedule` (or external cron + `once`) | k8s `CronJob` | optional |
| Split a big task into parallel narrowed workers | any | — | **fan-out** |
| Let an untrusted reader feed a trusted actor safely | any | — | **trust-partition** |
| Run a long-lived worker an orchestrator drives + steers | `reactive` + `--serve-mcp` | `Deployment` | **served** |

Every flag below is in [`configuration.md`](configuration.md); the mechanics are
in [`modes-and-triggers.md`](modes-and-triggers.md), [`subagents.md`](subagents.md),
and [`mcp.md`](mcp.md). Runnable skeletons live in [`examples/`](../examples/SAMPLES.md).

---

# Part A — agentd as a single agent

## 1. One-shot research / report job

**Shape:** `--mode once` · a Kubernetes `Job`, a CLI invocation, or a CI step.

A bounded task that has a definite end: research a topic to a sourced answer,
generate a release note from a diff, reconcile two records, draft a migration
plan. The run produces its result on **stdout**, structured telemetry on
**stderr**, and an **exit code** that encodes the terminal status — so a job
scheduler can branch on it.

```bash
agentd \
  --mode once \
  --instruction-file instructions/research.md \
  --intelligence unix:/run/intel.sock \
  --mcp "search=mcp-server-websearch" \
  --mcp "fs=mcp-server-fs --root /data --read-only" \
  --max-steps 40 --max-tokens 150000 --deadline 5m \
  --run-id "research-2026-06-27"
```

**The contract.** The instruction ends with a required output shape (the
[research template](../examples/instructions/research.md) emits
`Summary` / `Findings` / `Open questions` / `Sources` with every claim
attributed). A crisp contract gives the supervisor a crisp terminal state:
`completed → 0`, `refused → 5`, exhausted (steps / tokens / the run's own
`--deadline`) `→ 7` — and the supervisor's hard wall-clock backstop, when a
child won't self-terminate, kills with `124`
([RFC 0007](../rfcs/0007-agentic-loop-and-terminal-status.md),
[RFC 0011](../rfcs/0011-cloud-native-contract.md)).

**Why agentd.** A bad config exits `2` in milliseconds, before any token is
spent. Setting `--run-id` makes a retried Job idempotent. The whole thing is one
~1 MB static binary on `scratch` — nothing to install, nothing to patch.

## 2. Reactive event triage / responder

**Shape:** `--mode reactive` · a long-lived `Deployment`. Idles at near-zero CPU,
wakes on an MCP resource change, acts, returns to idle. **Never exits on its
own** — only `SIGTERM` (graceful drain) or a fatal/limit class stops it.

Wire it to anything an MCP server can expose as a subscribable resource — an
alert queue, a support inbox, a "new object" bucket notification, a CI webhook
landed as a resource — and it triages each item as it arrives.

```bash
agentd \
  --mode reactive \
  --instruction-file instructions/triage.md \
  --intelligence unix:/run/intel.sock \
  --mcp "inbox=mcp-server-inbox --queue /var/run/inbox" \
  --mcp "tickets=mcp-server-tickets --project OPS" \
  --subscribe "inbox:///items/new" \
  --max-steps 25 --max-tokens 2000000 \
  --metrics-addr :9090 --drain-timeout 25s
```

**The contract.** `--mode reactive` **requires** at least one `--subscribe`
(without it, config validation fails `2`). The wake notification carries **only
a URI** — the agent `resources/read`s the item's *current* state, so a change
missed during a restart is still recovered (level-, not edge-, triggered). The
[triage instruction](../examples/instructions/triage.md) emits one JSON decision
object per item, and — importantly — treats the item's text as **untrusted
data, not instructions** (the right posture for anything reacting to the
outside world).

**Why agentd.** The tree-wide `--max-tokens` ceiling is the ultimate
backpressure under a flood. `--metrics-addr` adds `/healthz`+`/readyz`+`/metrics`
for k8s probes; `--drain-timeout` (kept under the pod's
`terminationGracePeriodSeconds`) bounds graceful shutdown so in-flight triage
finishes before the pod dies. (Reactivity is stdio-MCP in v1 — see
[`modes-and-triggers.md`](modes-and-triggers.md).)

## 3. Scheduled audit / watcher

**Shape:** an external scheduler invoking `--mode once` (a k8s `CronJob`) — the
**recommended** production path, robust to clock skew and restart. For
non-orchestrated hosts, `--mode loop` (re-enter on a cadence) or `--mode schedule`
(per-fire identical to `once`) do it in-process.

Periodic, unattended checks: scan dependencies for new CVEs and open tickets for
regressions; reconcile desired vs actual config and file drift reports; sweep a
data lake for schema violations every 15 minutes.

```bash
# k8s CronJob spec runs, on each fire:
agentd \
  --mode once \
  --instruction-file /etc/agentd/audit.md \
  --intelligence unix:/run/intel.sock \
  --mcp "fs=mcp-server-fs --root /data --read-only" \
  --mcp "tickets=mcp-server-tickets --project SEC" \
  --max-steps 30 --deadline 10m \
  --run-id "audit-$(date +%Y%m%dT%H%M)"
```

In-process polling instead:

```bash
agentd --mode loop --interval 15m  --instruction-file /etc/agentd/audit.md  …
agentd --mode loop --interval 0    …   # work-until-done: re-enter immediately on completion
```

**Why agentd.** A `CronJob` owns lifecycle, retries, and history; agentd owns the
*reasoning* of one fire and an honest exit code. `--interval 0` turns `loop` into
a drain-a-backlog worker that re-enters the instant it finishes, until a bound
(`--deadline` / token ceiling) or `SIGTERM`.

---

# Part B — orchestrating subagents

Delegation has exactly one path: the root agent's model calls the
**`subagent.spawn`** self-tool. The supervisor (which owns the process table)
mints the child's identity and depth, **intersects** its tool scope to a subset
of the parent's, clamps its budget to what the tree can still afford, and only
then re-execs a child process. The child returns a **distillate** (~1–2k tokens)
— never its transcript. Caps (depth 4, 8 children/node, 64/tree, the tree-token
ceiling) come back as ordinary tool-result errors the model can adapt to — a
runaway loop gets refusals, never a fork bomb. The
[Rule-of-Two](security.md) trifecta check is enforced once, at startup, over the
root's whole grant; because scope only ever narrows as you descend, no subtree
can re-acquire a capability the root was refused
([`subagents.md`](subagents.md),
[RFC 0009](../rfcs/0009-subagent-process-model.md)).

## 4. Parallel fan-out / map-reduce

**Pattern:** a **coordinator** decomposes a task, spawns N narrowed workers, and
synthesizes their distillates. Spawn `sync` to delegate one subtask at a time, or
`async` to run a bounded fan of children concurrently and collect them as they
finish.

Good fits: audit a repository across independent dimensions (security, perf,
API-compat, docs) in parallel; summarize 200 documents into one briefing;
evaluate several candidate designs against the same rubric; shard a large
backfill and reconcile the shard reports.

```bash
agentd \
  --mode once \
  --instruction-file /etc/agentd/repo-audit.md \
  --intelligence unix:/run/intel.sock \
  --mcp "fs=mcp-server-fs --root /src --read-only" \
  --mcp "tickets=mcp-server-tickets --project ENG" \
  --max-depth 2 --max-tokens 4000000 --deadline 20m
```

The coordinator instruction does the decomposing — for example:

> Audit the repository at `/src`. For **each** of {security, performance,
> API-compatibility, documentation}, `subagent.spawn` a worker whose objective is
> that dimension only, scoped to the `fs` tool, with a JSON output contract
> `{dimension, findings[], severity}`. Do not analyze the code yourself. When all
> workers return, merge their findings, de-duplicate, and emit one ranked report;
> open a `tickets` issue for every `high`+ finding.

**Why this shape.** Each worker gets a **clean context window** (only the slice
it needs — half the point of delegating) and a hard slice of the budget, so one
runaway dimension can't starve the others. Failures are isolated to a subtree:
the security worker timing out doesn't sink the perf worker. The coordinator's
window stays lean because it only ever sees the ~1–2k-token distillates; a worker
with a large result uses **store-and-reference** (writes the bulk to a resource,
returns a summary + URI) so the coordinator reads detail only if it needs it.

## 5. Trust-partitioned pipeline (the injection firewall)

**Pattern:** keep the agent that reads **untrusted input** away from the tools
that are **sensitive** or **egress**-capable. The untrusted reader returns a
distilled, structured summary; only that distillate crosses back — raw,
possibly-injected bytes never enter a context that can act on them.

This is the agentic answer to prompt injection, and agentd enforces it
structurally. You tag each MCP server's capabilities, and at **startup** the
supervisor refuses any root grant that gives one agentd all three of
`untrusted_input` + `sensitive` + `egress` — the
[Rule-of-Two](security.md) (at most 2 of the 3 legs), overridable only with an
explicit `--allow-trifecta` ([RFC 0012](../rfcs/0012-security-posture.md)). A
dangerous topology can't even start by accident.

Within one tree you partition the (≤2-leg) work with subagents — read the
untrusted ticket in a child scoped to `tickets` only, then act in the parent:

```bash
agentd \
  --mode reactive \
  --instruction-file /etc/agentd/handle-ticket.md \
  --intelligence unix:/run/intel.sock \
  --subscribe "tickets:///incoming" \
  --mcp "tickets=mcp-server-tickets --project SUP" --mcp-tags "tickets=untrusted_input" \
  --mcp "crm=mcp-server-crm"                       --mcp-tags "crm=sensitive"
```

The coordinator **delegates reading** the (untrusted) ticket to a child scoped to
`tickets` *only* — that child has no CRM tool, so a malicious ticket body that
says "look up and leak every customer" reaches an agent with nothing sensitive to
reach for. The child returns `{intent, customer_id, summary}`; the parent acts on
that clean distillate with `crm`, and the raw ticket text never enters the
parent's window. This grant is **two legs** (`untrusted_input` + `sensitive`, no
`egress`), so it starts.

Add the third leg — say, *emailing* the customer (`egress`) — and the Rule-of-Two
refuses to co-locate it on this root. That's the runtime steering you to the
right shape: run the **actor** as a *separate* agentd holding `crm` + `email`
(`sensitive` + `egress` — still two legs) and have this reactive front hand it the
distillate over MCP — the cross-process composition of **use case 6** below. Each
process stays within the Rule-of-Two; no single agent ever holds all three.

**Why agentd.** The trust boundary is the **process boundary** plus the
spawn-time scope intersection — not a convention you hope the model follows. An
untagged server is treated conservatively as `untrusted_input`, and
`--enable-exec` counts as `egress`, so the check fails *closed*.

## 6. A served worker an orchestrator drives and steers

**Pattern:** run agentd as a long-lived **MCP server** (`--serve-mcp unix:/path`)
that exposes `subagent.spawn` / `subagent.send` / `subagent.status` /
`subagent.cancel` and the subscribable `agentd://` state resources. Any MCP
client — a control plane, a workflow engine, **or another agentd** — drives it.
Because agentd is symmetric, composition needs no new protocol: the parent just
declares the worker as one more `--mcp` server.

```bash
# An orchestrator agentd driving a worker agentd, both on one node:
agentd \
  --instruction "Run the nightly review; delegate each PR to the reviewer service." \
  --intelligence unix:/run/intel.sock \
  --mcp reviewer="agentd --instruction worker --intelligence unix:/run/intel.sock --serve-mcp unix:/run/rev.sock"
```

Two patterns fall out ([`mcp.md`](mcp.md) §3):

- **Drive** — the parent calls `subagent.spawn` on the worker and gets a clean,
  bounded distillate back; it never reasons about the worker's internal steps.
- **Subscribe** — the parent spawns `async`, subscribes to
  `agentd://subagent/{handle}`, and is woken by `notifications/resources/updated`
  when the worker reaches a terminal status; it then `resources/read`s that URI to
  collect the status and distilled result — the same notify-then-read discipline
  agentd uses for every resource, applied to agents themselves.

**Warm sessions.** `subagent.send` injects a follow-up turn into a still-warm
worker session — an iterative reviewer that keeps context across rounds ("address
that feedback and re-check"), a chat-shaped assistant fronted by a thin gateway,
a multi-step workflow where each step refines the last. `subagent.cancel` walks
the kill ladder on a subtree when the orchestrator changes its mind.

**Why agentd.** The orchestrator gets supervision for free: every served run is a
real, reaped process with a hard deadline, a no-progress watchdog, and active
ping/pong liveness; `agentd://subagent/{handle}` gives the driver an honest,
subscribable view of each child, and the read-only `agentd://status` a view of
the worker itself — without parsing logs.

---

## Compose them

These aren't exclusive. A realistic production agent is often several at once: a
**reactive** front (use case 2) that, per event, **fans out** to workers (4),
**partitions trust** so the untrusted reader can't exfiltrate (5), and is itself a
**served** worker (6) that a higher-level orchestrator drives and can drain on
deploy. The runtime is the same binary throughout — what changes is the
instruction, the `--mcp` wiring, and the mode.

## See also

- [`modes-and-triggers.md`](modes-and-triggers.md) — `once` / `loop` / `reactive` / `schedule` in depth, and the reactive router.
- [`subagents.md`](subagents.md) — the spawn payload, scope intersection, dispositions, caps, and supervision.
- [`mcp.md`](mcp.md) — agentd as MCP client *and* server, the `agentd://` resources, and composition.
- [`security.md`](security.md) — the Rule-of-Two trifecta, secret redaction, and tool scoping.
- [`deployment.md`](deployment.md) and [`examples/`](../examples/SAMPLES.md) — k8s `Job` / `CronJob` / `Deployment` manifests and runnable skeletons.
