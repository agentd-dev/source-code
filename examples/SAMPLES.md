# agentd examples

Runnable samples for the three operational shapes of **agentd** — a one-shot
run, an event-reactive daemon, and a polling/work-until-done loop — plus the
instruction files and MCP server config they use.

> **Status.** Implemented and released (v2.0.1). The agentic ReAct loop, the
> supervisor + subagent process tree, the MCP client, served self-MCP, and all
> four run modes ship; the commands below run real agent runs (given an
> intelligence endpoint + MCP servers). Every flag and env var used here exists
> in `crates/agentd/src/config.rs` (the authoritative surface).

---

## What's here

| File | What it is |
|---|---|
| `instructions/triage.md` | An instruction file with an output contract — classify an inbox item, take one action, emit JSON. Used by the reactive and loop samples. |
| `instructions/research.md` | An instruction file with an output contract — research a topic to a single sourced answer. Used by the once sample. |
| `mcp-servers.json` | An illustrative MCP server config (name + stdio launch argv + optional allowlists), the shape the config-file layer takes. |
| `run-once.sh` | `--mode once`: run the instruction to a terminal status, then exit. Job / CLI shape. |
| `run-reactive.sh` | `--mode reactive`: idle, wake on MCP resource changes, never exit on its own. Deployment shape. |
| `run-loop.sh` | `--mode loop`: re-enter on a cadence until a bound or a drain signal. Job-with-deadline / Deployment shape. |

All three scripts assume `agentd` is on `$PATH` (override with `AGENTD=/path/to/agentd`)
and that an intelligence endpoint is reachable. Build the binary with
`cargo build --release`; the binary is at `target/release/agentd`.

---

## Prerequisites

agentd ships **no tools of its own** (except a gated `exec`, off by default) and
talks to **one** intelligence endpoint. So every sample needs two things wired:

1. **An intelligence endpoint** — `--intelligence <URI>` or `AGENTD_INTELLIGENCE`.
   One of:
   - `unix:/run/intel.sock` — a gateway/sidecar over a unix socket (core build).
   - `https://host/v1/...` — a direct HTTPS endpoint (`tls` feature).
   - `vsock:<cid>:<port>` — a host LLM service from inside an enclave/microVM
     (`vsock` feature, roadmap M4).

   The wire is OpenAI-compatible `/chat/completions` with native tool-calling
   (RFC 0006). The credential is passed by **env/flag only**, never read from a
   config file, and is redacted everywhere agentd logs:

   ```bash
   export AGENTD_INTELLIGENCE=unix:/run/intel.sock
   export AGENTD_INTELLIGENCE_TOKEN=...        # or --intelligence-token
   ```

2. **MCP servers for the tools/resources the instruction needs** — declared with
   the repeatable `--mcp name=command arg arg` flag (stdio transport, RFC 0004).
   For example `--mcp "fs=mcp-server-fs --root /data"`. The launch argv is
   **trusted config** and is never built from model- or server-controlled
   strings.

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
has a `name`, a `command` argv launched over **stdio**, and optional `tools` /
`resources` allowlists that scope what a subagent may see and call
(RFC 0009/0012). This is the verbose structural config the file layer carries —
secrets never go here (env/flag only).

> **v1 vs roadmap.** In v1 the stable, wired surface for declaring servers is the
> repeatable **`--mcp name=command arg arg`** flag (see `config.rs`). The
> JSON config file (loaded via a config-file layer / `AGENTD_MCP_CONFIG`) and the
> per-server allowlist fields are **(roadmap)** — the file layer slots between
> built-in defaults and env in a later milestone (`docs/design/PLAN.md`). The
> sample scripts therefore use `--mcp` flags directly so they match what v1
> actually parses; `mcp-servers.json` documents where that config is headed.

The `--mcp` equivalents of the sample file:

```bash
--mcp "fs=mcp-server-fs --root /data --read-only" \
--mcp "search=mcp-server-websearch" \
--mcp "tickets=mcp-server-tickets --project OPS" \
--mcp "inbox=mcp-server-inbox --queue /var/run/inbox"
```

---

## Sample 1 — `run-once.sh` (mode: once)

Run an instruction to a terminal status, then exit. This is the Job / CLI shape:
result on stdout, telemetry on stderr, no daemon, no socket.

```bash
export AGENTD_INTELLIGENCE=unix:/run/intel.sock
export AGENTD_INTELLIGENCE_TOKEN=...
./run-once.sh
```

The script runs (abbreviated):

```bash
agentd \
  --mode once \
  --instruction-file instructions/research.md \
  --model claude-opus-4 \
  --mcp "search=mcp-server-websearch" \
  --mcp "fs=mcp-server-fs --root /data --read-only" \
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
export AGENTD_INTELLIGENCE=unix:/run/intel.sock
export AGENTD_INTELLIGENCE_TOKEN=...
./run-reactive.sh
```

Abbreviated:

```bash
agentd \
  --mode reactive \
  --instruction-file instructions/triage.md \
  --model claude-opus-4 \
  --mcp "inbox=mcp-server-inbox --queue /var/run/inbox" \
  --mcp "tickets=mcp-server-tickets --project OPS" \
  --subscribe "inbox:///items/new" \
  --max-steps 25 --max-tokens 2000000 \
  --health-file /run/agentd/health --drain-timeout 25s
```

`--mode reactive` **requires** at least one `--subscribe <uri>`; without it the
config fails validation and exits `2`. The token ceiling is tree-wide and
lifetime-scoped — it is the ultimate backpressure. `--health-file` gives an
orchestrator a liveness heartbeat to probe; `--drain-timeout` (default 25s)
bounds graceful shutdown and should stay under the pod's termination grace.

> **v1 boundary.** Reactivity is **stdio-only** in v1 — the subscribed servers
> must be stdio MCP servers. Reactive-over-HTTP (an SSE GET stream) is
> **(roadmap)**, RFC 0013.

---

## Sample 3 — `run-loop.sh` (mode: loop)

Re-enter the instruction on a cadence until a bound — max iterations (via the
step cap), the wall-clock `--deadline`, or the tree-wide token ceiling — or a
drain signal. The Job-with-deadline / Deployment shape.

```bash
export AGENTD_INTELLIGENCE=unix:/run/intel.sock
export AGENTD_INTELLIGENCE_TOKEN=...
./run-loop.sh
```

Abbreviated:

```bash
agentd \
  --mode loop \
  --interval 5m \
  --instruction-file instructions/triage.md \
  --model claude-opus-4 \
  --mcp "inbox=mcp-server-inbox --queue /var/run/inbox" \
  --mcp "tickets=mcp-server-tickets --project OPS" \
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

agentd emits structured JSON lines on stderr (one event per line). The intended
v1 shape, illustrative:

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
full list. Anything env-settable (12-factor) is shown with its env var.

| Flag | Env | Meaning |
|---|---|---|
| `--instruction <TEXT>` | `INSTRUCTION` | the task |
| `--instruction-file <PATH>` | — | read the instruction from a file |
| `--intelligence <URI>` | `AGENTD_INTELLIGENCE` | `unix:/path` \| `https://host/…` \| `vsock:cid:port` |
| `--intelligence-token <T>` | `AGENTD_INTELLIGENCE_TOKEN` | bearer / api key (redacted) |
| `--model <NAME>` | `AGENTD_MODEL` | model id |
| `--mcp name=command` | — | declare an MCP server (repeatable; stdio) |
| `--mode once\|loop\|reactive\|schedule` | `AGENTD_MODE` | the driver (default `once`) |
| `--subscribe <uri>` | — | subscribe to an MCP resource (repeatable; required for `reactive`) |
| `--interval <dur>` | — | loop/schedule cadence (e.g. `5m`, `0`=immediate) |
| `--max-steps <N>` | `AGENTD_MAX_STEPS` | per-run step cap (default 50) |
| `--max-tokens <N>` | `AGENTD_MAX_TOKENS` | token budget (default 200000) |
| `--deadline <dur>` | `AGENTD_DEADLINE` | wall-clock deadline (default `600s`) |
| `--max-depth <N>` | — | subagent tree depth cap (default 4) |
| `--run-id <ID>` | `AGENTD_RUN_ID` | idempotency key (auto-generated if unset) |
| `--log-level <L>` | `AGENTD_LOG_LEVEL` | `trace\|debug\|info\|warn\|error` (default `info`) |
| `--drain-timeout <dur>` | `AGENTD_DRAIN_TIMEOUT` | graceful drain budget (default `25s`) |
| `--health-file <PATH>` | — | liveness heartbeat file |
| `--serve-mcp <unix:/path>` | `AGENTD_SERVE_MCP` | serve agentd's own MCP (stdio/unix; HTTP serving is roadmap) |
| `--enable-exec` | `AGENTD_ENABLE_EXEC` | expose the gated `exec` tool (off by default) |

Durations accept `ms` / `s` / `m` / `h`, or a bare integer (seconds): `250ms`,
`30`, `5m`, `2h`.

---

## Roadmap boundaries (don't expect these in v1)

- **Reactivity is stdio-only.** Reactive-over-HTTP / SSE GET — **(roadmap)**, RFC 0013.
- **Self-MCP serving is stdio/unix only** (`--serve-mcp unix:/path`). HTTP
  serving — **(roadmap)**.
- **MCP `tasks` / `sampling` / `roots`** are not used as a client — **(roadmap)**, RFC 0013.
- **Config file / `AGENTD_MCP_CONFIG`** layer and per-server allowlists —
  **(roadmap)**; v1 uses `--mcp` flags. See `docs/design/PLAN.md`.
