# Analysis: pressure, idempotency, config variables, streaming inputs

Grounded in the tree at `2.4.0`. Every "exists" below was checked in the code,
not remembered.

---

## 1. Backpressure, resource pressure, self-healing

### What exists

| Control | Where | Scope |
|---|---|---|
| Spawn-rate limiter | `supervisor/tree.rs` `TokenBucket` (8 burst, 2/s) | subagent spawns only |
| Memory pressure | `supervisor/cgroup.rs::under_memory_pressure()` at 95% of `memory.high` | **consulted in exactly one place** — `runtime/subagents.rs:164` |
| Per-route concurrency | webhook `parallelism` + `on_overflow` | inbound webhooks |
| Per-workflow concurrency | `concurrency.{max_runs,on_overflow}` | runs |
| Global run ceiling | `limits.max_runs` (default 8) | runs |
| Per-principal quota | `a2a.principals[].quotas.rate` | A2A callers |
| Run retention | `store.retention.runs` (2.4.0) | durable records |
| Fan-out ceiling | `limits.workflow.fan_out` (2.3.0) | `foreach`/`batch` |

### The gaps, ranked by what actually bites

**1. Disk pressure is completely unhandled — and it is the failure that stops
everything.** The file store writes until `ENOSPC`; a checkpoint failure is a
halting condition, so a full disk stops the agent *after* the fact with no
warning before it. There is no free-space check anywhere. (This bit three times
during one development session, which is a fair sample.)

*Recommendation.* Check free space where the store already writes, and act in
two stages: **warn** below a soft threshold (`store.file.min_free`, default
maybe 512 MiB or 5%), then **shed** below a hard one — refuse to admit new runs
and new webhook events while continuing to drain what is in flight, because a
daemon that stops accepting work but finishes its current job degrades far
better than one that dies mid-checkpoint. Retention (2.4.0) is the pressure
valve that makes shedding recoverable rather than terminal.

**2. Memory pressure is consulted at one admission gate out of four.** A daemon
at its `memory.high` will refuse to spawn a subagent and then happily accept a
webhook, start a run, and dispatch a turn. The function already exists and is
cheap; the fix is calling it at the other three gates and reporting *which* gate
shed, so an operator can see the shape of the pressure.

**3. Webhooks have concurrency control but no RATE control.** `parallelism`
bounds how many run at once; nothing bounds how fast they arrive. Ten thousand
POSTs are each written to the durable inbox as fast as the socket delivers them
— which converts an inbound burst directly into disk pressure (gap 1). The
`TokenBucket` already in the tree is the right primitive; per-route
`rate: "<burst>/<per>s"` matching the A2A quota spelling would be consistent
with what operators already write.

**4. No CPU or load awareness, and no "working norms".** I would not build
adaptive control here. The honest first step is *measurement*: the `metrics`
feature already exists, so publish RSS, run depth, inbox depth, checkpoint
latency and disk headroom as gauges, and let an operator set thresholds against
observed values. A control loop that guesses its own baseline is how you get an
agent that throttles itself for reasons nobody can explain.

**5. Self-healing is passive, and mostly right.** Restore/replay, timer repair,
the kill ladder and `on_error: goto` are real recovery. What is missing is
*active* remediation under pressure — pausing start-node arming rather than
failing runs, so a schedule stops firing while the daemon is unwell and resumes
without operator action. `workflow.pause` already exists and is durable; the
piece missing is something that pauses on a pressure signal and un-pauses on
recovery, with both transitions on the audit stream.

---

## 2. Idempotency for outbound requests

### The key's lifecycle

The proposed shape is **generate once, persist, reuse**: mint a random key at
the step's first attempt, store it durably, and every retry or crash-replay
carries the stored key. That is correct — it is how API clients classically do
it — with one load-bearing ordering requirement: the key must be **durable
before the request leaves**. Mint → send → persist has a crash window between
send and persist, and a replay through that window mints a fresh key — which is
precisely the duplicate the mechanism exists to prevent. In agentd the ordering
comes for free if the key is minted in `begin_step`: the step already
checkpoints `running` *before* the effect (RFC 0025 §7), so the key rides a
write that already happens, and no new checkpoint is needed.

There is an alternative that gets the same property with **no state at all**:
derive the key from identity the retry already shares —
`sha256(run_id + "." + step_id)`, hex or UUID-formatted. Stable across attempts
by arithmetic rather than by invariant; unique across runs because run ids are
(ULIDs); and **opaque to the remote**, which matters — a raw `run.step` key
would leak ULID timestamps and internal step names to every API that logs its
idempotency keys, and wanting the key random is the right instinct about
exactly that. Hashing keeps the opacity without the persistence. (Scoped step
ids — `each[0].work` — make fan-out iterations distinct keys automatically.)

Both schemes are sound. The difference is *where correctness lives*: the
derived key is correct unconditionally, the persisted key is correct as long as
the mint-before-checkpoint ordering survives every future refactor. In a
codebase where this month's review found several declared-but-dead knobs, I
would default to the derived-hashed key and expose `value:` as the override.

One more rung above both: an **application key**, when one exists —
`value: "{{steps.fetch.output.order_id}}"`. Run-identity keys make *retries of
one run* safe; an application key also makes *two different runs* attempting
the same real-world operation collide, which is usually what the business rule
actually wants. So the ladder is: application key > derived key > random key,
with the field accepting all three.

### What is missing

`env` currently exposes `instance`, `run`, `ts`, `instruction`, `prompt`
(`engine/run.rs:569`). There is **no step id and no attempt number**, so a
workflow author cannot build the key by hand today either.

### Proposal

1. Add `env.step` and `env.attempt` to the template environment, and
   `env.idempotency_key` precomputed as `<run>.<step>` — the last one so the
   common case is one token and nobody derives it slightly differently.
2. Give the outbound nodes a declarative `idempotency` field:

   ```yaml
   call:
     kind: http
     url: "https://api.example.com/charges"
     method: POST
     idempotency: { header: "Idempotency-Key" }     # value defaults to env.idempotency_key
   ```

   with `{ query: "request_id" }` for APIs that take it in the URL, and an
   explicit `value:` for a remote that demands a particular format. Interpolation
   comes free — the field renders like every other.

3. **Which nodes.** `http` (header or query) and `mcp.tool` (via the call's
   `_meta`, which the protocol already carries) are the clear cases. `a2a.send`
   and `a2a.delegate` should set the message id from the same key, since the A2A
   spec already dedups on `messageId` — that makes agentd-to-agentd retries safe
   with no new field at all. `tool` inherits whatever the underlying tool does.

4. **The symmetry is worth noting**: agentd already does *inbound* idempotency
   for webhooks (dedup by key, replay returns the first response). This is the
   outbound mirror of a concept the codebase already commits to, which is a good
   sign the shape is right.

---

## 3. Config variables, preflight resolution, interactive entry

### What exists

Secrets resolve through `{{secret:NAME}}` / `{{secret-file:PATH}}` and a missing
one is **exit 2 naming the reference** — but only for the paths that call
`resolve_headers` at startup. There is no general variable mechanism, and no
single pass that proves *everything* referenced resolves before the first
effect.

### Proposal

**A `vars` block, referenced as `{{config.NAME}}`** — deliberately *not*
`{{vars.…}}`, which already means a workflow run's variables. Two things named
`vars` in one template language would be a permanent source of confusion.

```yaml
config:
  vars:
    region: eu-west-1
    fleet: triage
workflows:
  - name: x
    steps:
      call: { kind: http, url: "https://{{config.region}}.api.internal/x" }
```

**A resolution preflight.** One pass over the whole assembled document *and*
every loaded workflow, collecting every `{{secret:…}}`, `{{secret-file:…}}` and
`{{config.…}}` reference, and reporting **all** unresolved ones together. The
existing behaviour fails on the first one it happens to evaluate, which turns
configuring a new deployment into a guessing game played one restart at a time.
This is the highest-value part of this section and the cheapest.

**Interactive entry, carefully.** Reusing the `agentd login` pattern is right,
with two hard constraints:

- **Opt-in only** (`--prompt-missing`), and **refused when stdin is not a TTY**.
  A daemon under systemd or in a pod must never block on a prompt nobody can
  answer — that is an outage that looks like a hang.
- Prompted values go to the **credential cache**, never back into the config
  file. Writing a secret into the file the operator is editing is how secrets
  end up in git.

---

## 4. Streaming and socket inputs (the hardware-driver case)

### The architectural answer

agentd deliberately reaches the world through MCP, and a device is not an
exception: **the driver gets an MCP server, and the workflow subscribes to it.**
That is not a workaround — a `subscribe` start node with `debounce_ms`,
`coalesce` and `filter` is a good fit for exactly the problem hardware streams
have, which is that they produce far more samples than anyone wants runs.

```yaml
sensor:
  kind: subscribe
  server: driver
  uri: "device://thermocouple/3"
  debounce_ms: 250
  coalesce: true          # 1000 samples/sec become one run per quarter second
  filter: "value > 90"    # …and only when it matters
```

Three ways to wire the device, in order of preference:

1. **MCP server owning the device** — the driver process holds the serial port,
   CAN bus or GPIO, and exposes it as a subscribable resource. All the
   capability, credentials and failure modes of talking to hardware stay in the
   process that already has to handle them.
2. **The driver POSTs to a webhook** — works today with no changes, and is the
   right answer when the device speaks HTTP or sits behind something that does.
3. **A `socket` start node in agentd** — reading a unix socket or TCP line
   stream directly. I would **not** recommend this: it puts device I/O in the
   daemon, which is the one thing agentd's design says it does not do, and it
   would need its own framing, reconnect and backpressure story that the MCP
   transport already has.

### The one real gap

`subscribe` coalesces to the *latest* value. Hardware work often wants a
**window** — the last N samples, or a rolling mean — because the interesting
signal is a trend, not a reading. Today that means the MCP server does the
windowing and publishes an already-aggregated resource, which is defensible:
the driver knows the sample semantics and agentd does not.

If it belongs in agentd at all, it belongs as a `subscribe` option
(`window: {samples: 64}` delivering an array) rather than a new node kind —
worth deciding deliberately rather than drifting into.

---

## Recommended order

1. **Disk headroom + shed** — the failure that stops everything, currently
   silent (§1.1).
2. **Reference preflight** — cheapest, and it turns configuring a deployment
   from guesswork into a list (§3).
3. **`env.step` / `env.attempt` / `env.idempotency_key` + the `idempotency`
   field on `http` and `mcp.tool`** (§2).
4. **Memory pressure at the remaining admission gates**, and webhook rate
   limiting (§1.2, §1.3).
5. **`config.vars`** (§3) — pleasant, not urgent.
6. **Metrics for the norms** before any adaptive control (§1.4).

Interactive prompting and streaming windows are deliberately last: both are
easy to build in a shape that is hard to withdraw.
