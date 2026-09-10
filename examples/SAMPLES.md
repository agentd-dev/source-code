# agentd examples

> ### Read this first
>
> **The whole-system samples:**
>
> | File | What it is |
> |---|---|
> | **`coding-agent.yaml`** | **A coding agent you pair with** — the config behind [docs/coding-agent.md](../docs/coding-agent.md). `agentd tui --config examples/coding-agent.yaml` starts the daemon *and* a terminal UI. Validated against the binary. |
> | **`startup/`** | **A software company with two employees** — eleven instances run every role except CEO/CTO: ticket + SMS/voice support, engineering with CI-signal gates, QA, SRE incident lifecycle, sales cadences, finance dunning, marketing calendar + outreach, an egress-choke-point outbox, and a chief of staff. Third-party webhooks, signals, durable streams, MCP trust tags, HITL gates, A2A mesh, conversational interface — all of it, validated against the binary. The reasoning is written up as [docs/two-person-company.md](../docs/two-person-company.md). |
> | **`hiring/`** | **A hiring agent, end to end** — two instances split by the lethal-trifecta gate: the CV-reading intake and the mail-sending actions side. |
> | **`tail/`** | **Processing lines as they arrive** — react to appended CSV/text lines with a durable byte cursor that survives a restart, partial-line hold-back, and rotation detection. A directory-shaped project (`agentd.yml` + `workflows/`). |
> | **`voice/`** | **A voice agent, end to end** — wake word, speech, and a room full of people who are not authenticated. Two instances split by the lethal-trifecta gate (the ears that hear vs. the hands that unlock), plus a stdlib-only reference MCP server for the microphone and the speaker. Runs with `--fake` and no hardware. |
>
> **Coming from agentd 1.x?** The `--mode`, `--subscribe` and `--interval`
> spellings the 1.x samples used were **removed in 2.0**: naming one now fails
> the load with a hint naming its replacement, so a stale command line is a loud
> error rather than a flag that quietly does nothing. They survive in the flag
> table below only as that migration map. What replaced them is one durable
> runtime (`lifecycle.run_until` = job or daemon) triggered by workflow **start
> nodes** (`once` / `loop` / `schedule` / `subscribe` / `signal` / `event` /
> `a2a`) — which is what the runner scripts and the k8s manifests here run on
> today. See [modes-and-triggers.md](../docs/modes-and-triggers.md) and
> [getting-started.md](../docs/getting-started.md).

---

## What's here

| File | What it is |
|---|---|
| `coding-agent.yaml` | **(2.0)** A pair-programming agent for a repository: the `exec` fence, approvals, budgets, and the display surface the TUI/web UI attach to. |
| `voice/` | **(2.0)** A voice-controlled household agent: `subscribe` start on an MCP resource, `window` for conversational context, `on_overflow: replace` as barge-in, `event: human.asked` to speak every gate aloud, and an addressed gate a voice cannot answer. |
| `instructions/triage.md` | An instruction file with an output contract — classify an inbox item, take one action, emit JSON. Used by the reactive and loop samples. |
| `instructions/research.md` | An instruction file with an output contract — research a topic to a single sourced answer. Used by the once sample. |
| `mcp-servers.fragment.json` | A server-list **fragment**, not a runnable config: the shape of `mcp.servers` (name + remote `endpoint` + auth `headers` + `tags`), meant to be layered under a config with a second `-c`. |
| `run-once.sh` | Run the instruction to a terminal status, then exit. Flags only, no config file — so agentd synthesizes the one-shot `main` workflow (a `once` start, an `agent` step, a `finish`). Job / CLI shape. |
| `run-reactive.sh` | Idle, wake on MCP resource changes, never exit on its own. The trigger is the `subscribe` start node in [`reactive-triage.yaml`](reactive-triage.yaml), which the script passes with `--config`. Deployment shape. |
| `run-loop.sh` | Re-enter on a cadence until a bound or a drain signal. The trigger is the `loop` start node in [`loop-triage.yaml`](loop-triage.yaml), which the script passes with `--config`. Job-with-deadline / Deployment shape. |

All three scripts assume `agentd` is on `$PATH` (override with `AGENTD=/path/to/agentd`)
and that an intelligence endpoint is reachable. Build the binary with
`cargo build --release`; the binary is at `target/release/agentd`.

---

## Prerequisites

agentd ships **no tools of its own** and runs **no local code** — every tool comes
from an MCP server it reaches over the network. It talks to **one** intelligence
endpoint. So every sample needs two things wired:

1. **An intelligence endpoint** — `--intelligence <URI>` or `AGENT_INTELLIGENCE`,
   one **HTTPS** URI:
   - `https://host/v1/...` — a direct HTTPS endpoint (`tls` feature, on by default).
   - `http://127.0.0.1:PORT/v1` — a **loopback-only** plaintext carve-out for a
     same-host TLS-terminating sidecar (dev / no-TLS image). A non-loopback
     `http://` is rejected at startup (exit `2`).

   The wire is OpenAI-compatible `/chat/completions` with native tool-calling
   (RFC 0006). The credential is passed by **env/flag only**, never read from a
   config file, and is redacted everywhere agentd logs:

   ```bash
   export AGENT_INTELLIGENCE=https://gw.example/v1
   export AGENT_INTELLIGENCE_TOKEN=...        # or --intelligence-token
   ```

2. **MCP servers for the tools/resources the instruction needs** — declared with
   the repeatable `--mcp name=<endpoint>` flag (remote **Streamable HTTP**,
   RFC 0004). For example `--mcp "fs=https://mcp-fs.internal/mcp"`. agentd
   **connects** to that URL and speaks JSON-RPC 2.0 over HTTP(S); it spawns no
   process. The endpoint is **trusted config** and is never built from model- or
   server-controlled strings (RFC 0012).

A bad config exits `2` in milliseconds, before any LLM round-trip — agentd
validates everything up front (e.g. a `subscribe` start node with no `server`,
or an intelligence URI with an unsupported scheme, both fail fast).

---

## The instruction files

Instructions are plain text passed with `--instruction "<text>"`,
`--instruction.file <path>`, or the `INSTRUCTION` env var. A good instruction
ends with an explicit **output contract** so the run has a crisp terminal state
the supervisor can map to an exit code.

- **`instructions/triage.md`** — reads the changed resource's *current* state
  (the wake notification carries only a URI, never a body — RFC 0004/0008),
  classifies it, takes exactly one action, and emits a single JSON object as its
  final message. It also treats the item's text as untrusted data, not as
  instructions — the right posture for anything reactive.

- **`instructions/research.md`** — gathers sources over MCP, cross-checks
  load-bearing claims, and emits a fixed Markdown structure
  (`Summary` / `Findings` / `Open questions` / `Sources`) with every claim
  attributed. Use it as a template: pass the concrete topic with
  `--instruction` or edit the `<TOPIC>` placeholder.

---

## The MCP server config

`mcp-servers.fragment.json` is an **inventory, not an agent**. It carries
`mcp.servers` and nothing else — no instruction, no intelligence endpoint — so it
is a layer to put *under* a config, not a config to run:

```bash
agentd -c ./my-agent.yaml -c examples/mcp-servers.fragment.json
```

`--config` is repeatable and merges left to right (RFC 7396, later files win per
leaf), which is what makes a shared server list a real deployment pattern:
`examples/startup/` uses the same shape for its service catalogue. The file
deliberately carries no `config_version` — it is not claiming to be a whole
document.

Each server has a `name`, a remote `endpoint` (an `https://host/mcp`
Streamable-HTTP URL that agentd CONNECTS to — it spawns no process), optional auth
`headers` (carrying `{{secret:NAME}}` references resolved at connect time, never
inlined or logged), and `tags` that scope the Rule-of-Two trust budget
(RFC 0009/0012). The `endpoint` is trusted config and is never built from model- or
server-controlled strings. The intelligence token stays env/flag only.

> **These four servers together hold all three trifecta legs**, so merging the
> whole file into one agent is refused at startup — `untrusted_input + sensitive
> + egress` in one grant:
>
> ```text
> lethal-trifecta refused: the root grant wires untrusted_input + sensitive +
> egress into one agent; narrow the tags or set security.allow_trifecta (audited)
> ```
>
> That refusal is the point of the sample, not a defect in it, and a test pins
> it. Take the two servers a given agent actually needs — a real deployment
> splits the legs across two instances, as [`hiring/`](hiring/) and
> [`voice/`](voice/) do.

> **Config precedence.** `--config` is the lowest non-default layer
> (`default < FILE < env < flag`, RFC 0017 §3). Repeatable list flags like `--mcp`
> **add** to the file's `mcp.servers`, so a file can declare the base set and a
> flag can append one for a one-off run.

The `--mcp` flag equivalents of the sample file (agentd connects to each URL):

```bash
--mcp "fs=https://mcp-fs.internal/mcp" \
--mcp "search=https://mcp-search.internal/mcp" \
--mcp "tickets=https://mcp-tickets.internal/mcp" \
--mcp "inbox=https://mcp-inbox.internal/mcp"
```

---

## Sample 1 — `run-once.sh` (a `once` start node)

Run an instruction to a terminal status, then exit. This is the Job / CLI shape:
result on stdout, telemetry on stderr, no daemon, no served surface.

```bash
export AGENT_INTELLIGENCE=https://gw.example/v1
export AGENT_INTELLIGENCE_TOKEN=...
./run-once.sh
```

The script runs (abbreviated):

```bash
agentd \
  --instruction.file instructions/research.md \
  --model claude-opus-4 \
  --mcp search=https://mcp-search.internal/mcp \
  --mcp fs=https://mcp-fs.internal/mcp \
  --max-steps 40 --max-tokens 150000 --deadline 5m \
  --log-level info \
  --run-id "research-$(date +%Y%m%d-%H%M%S)"
```

There is no config file and no workflow here, and none is needed: an instruction
that arrives without one gets the sugar workflow agentd synthesizes for it —
`start: {kind: once}`, an `agent` step carrying the instruction, a `finish` — so
the flags above are the whole configuration for a job that runs once.

The exit code maps the root subagent's terminal status: `completed`→`0`,
`refused`→`5`, budget/exhausted (steps / tokens / the run's own `deadline`)→`7`
(RFC 0007/0011). Exit `124` is reserved for the supervisor's hard-kill backstop —
a child that won't self-terminate — not the deadline terminal status itself.
Setting an explicit `--run-id` makes retries idempotent.

---

## Sample 2 — `run-reactive.sh` (a `subscribe` start node)

Idle at near-zero CPU; wake on `notifications/resources/updated`; triage the
changed item; return to idle. The daemon **never exits on its own** — only a
drain signal (`SIGTERM`) or a fatal/limit class stops it. Deploy it as a
long-lived Deployment.

```bash
export AGENT_INTELLIGENCE=https://gw.example/v1
export AGENT_INTELLIGENCE_TOKEN=...
./run-reactive.sh
```

Abbreviated:

```bash
agentd \
  --config reactive-triage.yaml \
  --max-tokens 2000000 \
  --health-file /run/agentd/health \
  --log-level info
```

Everything structural — the instruction, the two MCP servers, the file store,
the drain budget and the subscription itself — lives in
[`reactive-triage.yaml`](reactive-triage.yaml); the flags are only the ceilings
and the things a deployment owns. The trigger is one start node:

```yaml
workflows:
  - name: triage
    steps:
      new_item: { kind: subscribe, server: inbox, uri: "inbox:///items/new" }
      work:     { kind: agent, depends_on: [new_item], instruction: "…" }
      done:     { kind: finish, depends_on: [work] }
```

A `subscribe` start node needs both a `server` and a `uri`; omit either and the
config fails validation and exits `2`, naming the missing field
(`kind "subscribe" requires field "server"`). `--max-tokens` is **per run**, not
per lifetime: it sets `limits.run.tokens`, the cap each triage run is measured
against on its own, and a subagent inherits the same figure as its own allowance
rather than drawing on a shared pool — so the 2,000,000 above (which is also the
built-in default) bounds one wake, not the week. The lifetime ceiling is a
different setting: `intelligence.budget.lifetime_tokens`
(`--budget-tokens-lifetime`, default `0` = unbounded), the instance-wide hard stop
the token governor enforces across every run, alongside
`intelligence.budget.windows[]` for per-hour or per-day pacing. That budget, not
the run cap, is the backpressure a daemon meant to stay up for weeks needs.

`--health-file` gives an orchestrator a liveness heartbeat to probe;
`lifecycle.drain_timeout` (25s in this config, which is also the default) bounds
graceful shutdown and should stay under the pod's termination grace.

> **How reactivity works.** agentd subscribes over the MCP servers'
> Streamable-HTTP transport and wakes on pushed `notifications/resources/updated`
> (HTTP/SSE) — the subscribed servers are the same remote HTTP endpoints declared
> with `--mcp` (RFC 0013).

---

## Sample 3 — `run-loop.sh` (a `loop` start node)

Re-enter the instruction on a cadence until a bound — `max_iterations` on the loop
node, or an `until` expression over the last outcome — or a drain signal. The
per-run ceilings are not loop bounds: `--max-steps` / `--max-tokens` (and this
config's own `limits.run.steps` of 25) fail the *iteration* that exhausts one,
after which the node re-arms on its backoff. The Job-with-deadline / Deployment
shape.

```bash
export AGENT_INTELLIGENCE=https://gw.example/v1
export AGENT_INTELLIGENCE_TOKEN=...
./run-loop.sh
```

Abbreviated:

```bash
agentd \
  --config loop-triage.yaml \
  --max-tokens 1000000 \
  --deadline 2h \
  --log-level info
```

The cadence is the start node in [`loop-triage.yaml`](loop-triage.yaml), beside
the same instruction and MCP servers the reactive sample declares:

```yaml
workflows:
  - name: triage
    steps:
      every_5m: { kind: loop, interval: 5m }
      work:     { kind: agent, depends_on: [every_5m], instruction: "…" }
      done:     { kind: finish, depends_on: [work] }
```

`interval` is the gap between runs, not a wall-clock period: the node re-arms
only when the previous run **finishes**, so two runs never overlap and a slow
run pushes the next one out. `interval: 0` (the default) re-enters immediately
on completion — work-until-done. The loop's own bounds sit on the same node:
`max_iterations` caps how many times it re-enters, and `until` — an expression
over the last outcome — stops it early. A drain signal stops it at any point,
which is what a kept-alive Deployment relies on.

> **Scheduling note.** For production cron, the **recommended** path is an
> external scheduler (e.g. a k8s CronJob) firing one `once`-start pod per tick —
> robust to clock skew and restart, and what
> [`k8s/cronjob-schedule.yaml`](k8s/cronjob-schedule.yaml) does. agentd also has
> a `schedule` start node (`every: <dur>`, `cron: <expr>` with the `cron`
> feature, or a one-shot `at: <dur>`), per fire identical to `once`, for
> non-orchestrated deployments (RFC 0008).

---

## What a run logs

agentd emits structured JSON lines on stderr (one event per line), illustrative:

```json
{"ts":"2026-06-25T10:15:00.142Z","level":"info","event":"run.start","run_id":"research-20260625-101500","run":"main-01K17S3P8QJ0V2WQ0K6R4M9Y3T","workflow":"main","node":"start","inbox_event":"01K17S3P8Q6F1B7C4D0E2G8H5J","acting_for":null,"key":null}
{"ts":"2026-06-25T10:15:00.310Z","level":"info","event":"mcp.connect","run_id":"research-20260625-101500","server":"search","tools":7}
{"ts":"2026-06-25T10:15:02.880Z","level":"info","event":"subagent.spawn","run_id":"research-20260625-101500","handle":"sub-1","mode":"sync","node":1,"depth":1,"servers":2}
{"ts":"2026-06-25T10:15:09.501Z","level":"info","event":"run.done","run_id":"research-20260625-101500","run":"main-01K17S3P8QJ0V2WQ0K6R4M9Y3T","workflow":"main","status":"completed","err":null}
```

Every line also carries `agent_id`, `agent_path`, `comp` (`supervisor` / `agent` /
`mcp` / `intel`) and `pid` — elided above for width — plus `trace_id` while a trace
is in flight; `subagent.spawn` adds `priority` and the child's `memory_bytes` /
`cpu_seconds` rlimits, and its `pid` is the **child's**, because per-event fields
are merged over the canonical ones and win on a name clash. Two ids look alike and
are not: the canonical `run_id` is the **instance's**, the one `--run-id` sets and
every line repeats, while `run` is the **workflow run's** own `<workflow>-<ULID>`
(here `main`, the workflow the instruction sugar generates). The terminal line is
`run.done` — there is no `run.exit` — carrying the run's terminal `status`
(`completed`, `failed`, `refused`, `cancelled` or `stalled`) and `err`; its
`output` field stays `null` unless `observability.log_content` is on.

Credentials never appear in any log line — the `--intelligence-token` value is
redacted (`***`) in all agentd output, including panic messages.

---

## Flag reference (used by these samples)

Most flags below are aliases in the `ALIASES` table in
`crates/agentd/src/config/v2/mod.rs`, which maps each spelling onto a config path;
`--config`/`-c` is parsed by hand instead, because the file layer has already
consumed it by the time the flag layer runs. `--mode`, `--subscribe` and
`--interval` are the 1.x spellings this file keeps for reference — they live in
`REMOVED_FLAGS`, so naming one now fails the load with a migration hint rather than
being quietly ignored. Run `agentd --help` for the current list. Anything
env-settable (12-factor) is shown with its env var. The neutral `AGENT_*` env
prefix is accepted as an alias for the branded `AGENTD_*` one (branded wins on
conflict).

| Flag | Env | Meaning |
|---|---|---|
| `--instruction <TEXT>` | `INSTRUCTION` | the task |
| `--instruction.file <PATH>` | — | read the instruction from a file |
| `--intelligence <URI>` | `AGENT_INTELLIGENCE` | `https://host/…` (or loopback `http://127.0.0.1:PORT` for a dev sidecar) |
| `--intelligence-token <T>` | `AGENT_INTELLIGENCE_TOKEN` | bearer / api key (redacted) |
| `--model <NAME>` | `AGENT_MODEL` | model id |
| `--mcp name=<endpoint>` | — | declare a remote MCP server URL (repeatable; Streamable HTTP) |
| `--config <PATH>` | `AGENT_CONFIG` | load a declarative config file (`mcp.servers`, limits, …) |
| `--mode …` | — | **removed in 2.0** — use a start node (`once` / `loop` / `schedule` / `subscribe` / …); `AGENT_MODE` is not read either |
| `--subscribe <uri>` | — | **removed in 2.0** — use a `subscribe` start node: `{kind: subscribe, server: <name>, uri: <uri>}` |
| `--interval <dur>` | — | **removed in 2.0** — `interval` on a `loop` start node, or `every` on a `schedule` start node |
| `--max-steps <N>` | `AGENT_MAX_STEPS` | per-run step cap (`limits.run.steps`, default 500) |
| `--max-tokens <N>` | `AGENT_MAX_TOKENS` | token budget for a **single run** (`limits.run.tokens`, default 2000000) |
| `--budget-tokens-lifetime <N>` | `AGENT_BUDGET_TOKENS` | the instance's cumulative ceiling across **all** runs (`intelligence.budget.lifetime_tokens`, default `0` = unbounded) |
| `--deadline <dur>` | `AGENT_DEADLINE` | wall-clock deadline (`limits.run.deadline`, default `3600s`) |
| `--max-depth <N>` | — | subagent tree depth cap (`limits.subagents.depth`, default 3) |
| `--run-id <ID>` | `AGENT_RUN_ID` | idempotency key (auto-generated if unset) |
| `--log-level <L>` | `AGENT_LOG_LEVEL` | `trace\|debug\|info\|warn\|error` (default `info`) |
| `--drain-timeout <dur>` | `AGENT_DRAIN_TIMEOUT` | graceful drain budget (default `25s`) |
| `--health-file <PATH>` | — | liveness heartbeat file |
| `--serve-mcp https://host:port` | `AGENT_SERVE_MCP` | serve agentd's own MCP over HTTP(S) with mTLS/bearer (sets `a2a.listen`; needs the `a2a` feature; loopback `http://` for dev) |

Durations accept `ms` / `s` / `m` / `h`, or a bare integer (seconds): `250ms`,
`30`, `5m`, `2h`.

---

## Boundaries

- **Every network transport is HTTP(S).** Intelligence, the MCP client, the served
  self-MCP, and A2A / operator control are HTTP(S) with mTLS/bearer auth; plaintext
  `http://` is a **loopback-only** dev carve-out. Off the network it speaks two
  things. A **unix domain socket** (`unix:///run/agentd/a2a.sock`) carries that same
  HTTP/1.1 + JSON-RPC without TLS, because the kernel authenticates the peer by uid
  — which is how a co-located A2A peer, an instance child reaching its parent, is
  wired. And between the supervisor and its subagents there is a length-framed
  JSON-RPC over the child's own stdio pipes: a 4-byte length prefix, so an
  instruction or a distilled result containing newlines survives the wire.
- **agentd ships no tools and runs no local code by default.** The one exception is
  `exec`, the guarded local command runner, off at build time (the `exec` cargo
  feature, deliberately not in the released binaries) and again at run time
  (`security.exec.enabled`); without both it is a mapping-only contract whose
  execution is delegated off-box through `tools.overrides` — see
  [`coding-agent.yaml`](coding-agent.yaml). Every other tool comes from a remote MCP
  server it connects to.
- **Agent-authored cyclic workflows** are in every build — the engine is
  unconditional, so the model self-authors a `Graph` and agentd drives it with no
  feature flag behind it; `cel` is what `when:` / `until:` / `filter:` need, and it
  is in the released binaries (see [`docs/workflows.md`](../docs/workflows.md)).
- **MCP `tasks` / `sampling` / `roots`** as a client are **(deferred)**, RFC 0013.
