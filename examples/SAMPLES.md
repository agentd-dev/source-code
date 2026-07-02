# agentd examples

Runnable samples for the three operational shapes of **agentd** — a one-shot
run, an event-reactive daemon, and a polling/work-until-done loop — plus the
instruction files and MCP server config they use.

> **Status.** Implemented and released (v2.0.1). The agentic ReAct loop, the
> supervisor + subagent process tree, the MCP client, served self-MCP over
> HTTP(S), and all four run modes ship; the commands below run real agent runs
> (given an intelligence endpoint + MCP servers). Every flag and env var used
> here exists in `crates/agentd/src/config.rs` (the authoritative surface).

---

## What's here

| File | What it is |
|---|---|
| `instructions/triage.md` | An instruction file with an output contract — classify an inbox item, take one action, emit JSON. Used by the reactive and loop samples. |
| `instructions/research.md` | An instruction file with an output contract — research a topic to a single sourced answer. Used by the once sample. |
| `mcp-servers.json` | An illustrative MCP server config (name + remote `endpoint` + auth `headers` + `tags`), the shape a `--config` JSON file carries. |
| `run-once.sh` | `--mode once`: run the instruction to a terminal status, then exit. Job / CLI shape. |
| `run-reactive.sh` | `--mode reactive`: idle, wake on MCP resource changes, never exit on its own. Deployment shape. |
| `run-loop.sh` | `--mode loop`: re-enter on a cadence until a bound or a drain signal. Job-with-deadline / Deployment shape. |

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
validates everything up front (e.g. `--mode reactive` with no `--subscribe`, or
an intelligence URI with an unsupported scheme, both fail fast).

---

## The instruction files

Instructions are plain text passed with `--instruction "<text>"`,
`--instruction-file <path>`, or the `INSTRUCTION` env var. A good instruction
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

`mcp-servers.json` shows the shape of a declarative MCP server list: each server
has a `name`, a remote `endpoint` (an `https://host/mcp` Streamable-HTTP URL),
optional auth `headers` (carrying `{{secret:NAME}}` references resolved at connect
time, never inlined or logged), and `tags` that scope the Rule-of-Two trust budget
(RFC 0009/0012). Load it with **`--config <path>`** (or `AGENTD_CONFIG`); the
intelligence token still stays env/flag only.

> **Config precedence.** `--config` is the lowest non-default layer
> (`default < FILE < env < flag`, RFC 0017 §3). Repeatable list flags like `--mcp`
> **add** to the file's `mcp_servers`, so a file can declare the base set and a
> flag can append one for a one-off run.

The `--mcp` flag equivalents of the sample file (agentd connects to each URL):

```bash
--mcp "fs=https://mcp-fs.internal/mcp" \
--mcp "search=https://mcp-search.internal/mcp" \
--mcp "tickets=https://mcp-tickets.internal/mcp" \
--mcp "inbox=https://mcp-inbox.internal/mcp"
```

---

## Sample 1 — `run-once.sh` (mode: once)

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
  --mode once \
  --instruction-file instructions/research.md \
  --model claude-opus-4 \
  --mcp "search=https://mcp-search.internal/mcp" \
  --mcp "fs=https://mcp-fs.internal/mcp" \
  --max-steps 40 --max-tokens 150000 --deadline 5m \
  --run-id "research-20260625-101500"
```

The exit code maps the root subagent's terminal status: `completed`→`0`,
`refused`→`5`, budget/exhausted (steps / tokens / the run's own `deadline`)→`7`
(RFC 0007/0011). Exit `124` is reserved for the supervisor's hard-kill backstop —
a child that won't self-terminate — not the deadline terminal status itself.
Setting an explicit `--run-id` makes retries idempotent.

---

## Sample 2 — `run-reactive.sh` (mode: reactive)

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
  --mode reactive \
  --instruction-file instructions/triage.md \
  --model claude-opus-4 \
  --mcp "inbox=https://mcp-inbox.internal/mcp" \
  --mcp "tickets=https://mcp-tickets.internal/mcp" \
  --subscribe "inbox:///items/new" \
  --max-steps 25 --max-tokens 2000000 \
  --health-file /run/agentd/health --drain-timeout 25s
```

`--mode reactive` **requires** at least one `--subscribe <uri>`; without it the
config fails validation and exits `2`. The token ceiling is tree-wide and
lifetime-scoped — it is the ultimate backpressure. `--health-file` gives an
orchestrator a liveness heartbeat to probe; `--drain-timeout` (default 25s)
bounds graceful shutdown and should stay under the pod's termination grace.

> **How reactivity works.** agentd subscribes over the MCP servers'
> Streamable-HTTP transport and wakes on pushed `notifications/resources/updated`
> (HTTP/SSE) — the subscribed servers are the same remote HTTP endpoints declared
> with `--mcp` (RFC 0013).

---

## Sample 3 — `run-loop.sh` (mode: loop)

Re-enter the instruction on a cadence until a bound — max iterations (via the
step cap), the wall-clock `--deadline`, or the tree-wide token ceiling — or a
drain signal. The Job-with-deadline / Deployment shape.

```bash
export AGENT_INTELLIGENCE=https://gw.example/v1
export AGENT_INTELLIGENCE_TOKEN=...
./run-loop.sh
```

Abbreviated:

```bash
agentd \
  --mode loop \
  --interval 5m \
  --instruction-file instructions/triage.md \
  --model claude-opus-4 \
  --mcp "inbox=https://mcp-inbox.internal/mcp" \
  --mcp "tickets=https://mcp-tickets.internal/mcp" \
  --max-steps 25 --max-tokens 1000000 --deadline 2h \
  --drain-timeout 25s
```

`--interval D` sets the re-entry cadence: `D>0` polls every `D`; `D=0`
re-enters immediately on completion (work-until-done). A `--deadline` turns the
loop into a bounded run; omit it (and let the orchestrator own lifecycle) for a
kept-alive Deployment.

> **Scheduling note.** For production cron, the **recommended** path is an
> external scheduler (e.g. a k8s CronJob) invoking `agentd --mode once …` — robust
> to clock skew and restart. agentd also has a `--mode schedule` (per-fire
> identical to `once`, requires `--interval <dur>` or `--cron <expr>`) for
> non-orchestrated deployments (RFC 0008).

---

## What a run logs

agentd emits structured JSON lines on stderr (one event per line), illustrative:

```json
{"ts":"2026-06-25T10:15:00.142Z","level":"info","event":"run.start","run_id":"research-20260625-101500","mode":"once","model":"claude-opus-4"}
{"ts":"2026-06-25T10:15:00.310Z","level":"info","event":"mcp.connect","server":"search","proto":"2025-11-25"}
{"ts":"2026-06-25T10:15:02.880Z","level":"info","event":"subagent.spawn","route":"root","depth":0}
{"ts":"2026-06-25T10:15:09.501Z","level":"info","event":"run.exit","run_id":"research-20260625-101500","status":"completed","exit_code":0}
```

Credentials never appear in any log line — the `--intelligence-token` value is
redacted (`***`) in all agentd output, including panic messages.

---

## Flag reference (used by these samples)

Every flag below is in `crates/agentd/src/config.rs`; run `agentd --help` for the
full list. Anything env-settable (12-factor) is shown with its env var. The
neutral `AGENT_*` env prefix is accepted as an alias for the branded `AGENTD_*`
one (branded wins on conflict).

| Flag | Env | Meaning |
|---|---|---|
| `--instruction <TEXT>` | `INSTRUCTION` | the task |
| `--instruction-file <PATH>` | — | read the instruction from a file |
| `--intelligence <URI>` | `AGENT_INTELLIGENCE` | `https://host/…` (or loopback `http://127.0.0.1:PORT` for a dev sidecar) |
| `--intelligence-token <T>` | `AGENT_INTELLIGENCE_TOKEN` | bearer / api key (redacted) |
| `--model <NAME>` | `AGENT_MODEL` | model id |
| `--mcp name=<endpoint>` | — | declare a remote MCP server URL (repeatable; Streamable HTTP) |
| `--config <PATH>` | `AGENT_CONFIG` | load a declarative JSON config file (`mcp_servers[]`, limits, …) |
| `--mode once\|loop\|reactive\|schedule` | `AGENT_MODE` | the driver (default `once`) |
| `--subscribe <uri>` | — | subscribe to an MCP resource (repeatable; required for `reactive`) |
| `--interval <dur>` | — | loop/schedule cadence (e.g. `5m`, `0`=immediate) |
| `--max-steps <N>` | `AGENT_MAX_STEPS` | per-run step cap (default 50) |
| `--max-tokens <N>` | `AGENT_MAX_TOKENS` | token budget (default 200000) |
| `--deadline <dur>` | `AGENT_DEADLINE` | wall-clock deadline (default `600s`) |
| `--max-depth <N>` | — | subagent tree depth cap (default 4) |
| `--run-id <ID>` | `AGENT_RUN_ID` | idempotency key (auto-generated if unset) |
| `--log-level <L>` | `AGENT_LOG_LEVEL` | `trace\|debug\|info\|warn\|error` (default `info`) |
| `--drain-timeout <dur>` | `AGENT_DRAIN_TIMEOUT` | graceful drain budget (default `25s`) |
| `--health-file <PATH>` | — | liveness heartbeat file |
| `--serve-mcp https://host:port` | `AGENT_SERVE_MCP` | serve agentd's own MCP over HTTP(S) with mTLS/bearer (`serve-https`; loopback `http://` for dev) |

Durations accept `ms` / `s` / `m` / `h`, or a bare integer (seconds): `250ms`,
`30`, `5m`, `2h`.

---

## Boundaries

- **All transports are HTTP(S).** Intelligence, the MCP client, the served
  self-MCP, and A2A / operator control are HTTP(S) with mTLS/bearer auth;
  plaintext `http://` is a **loopback-only** dev carve-out. agentd links no
  unix/vsock of its own.
- **agentd ships no tools and runs no local code.** There is no `exec` tool; every
  tool comes from a remote MCP server it connects to.
- **Agent-authored cyclic workflows** ship under `--features workflow` — the
  model self-authors a `Graph` and agentd drives it (see
  [`docs/workflows.md`](../docs/workflows.md)).
- **MCP `tasks` / `sampling` / `roots`** as a client are **(deferred)**, RFC 0013.
