<div align="center">

```
                                 █████     █████
                                ░░███     ░░███
  ██████    ███████  ██████  ████████   ███████   ██████████
 ░░░░░███  ███░░███ ███░░███░░███░░███ ░░░███░   ███░░███░░███
  ███████ ░███ ░███░███████  ░███ ░███   ░███   ░███ ░███ ░███
 ███░░███ ░███ ░███░███░░░   ░███ ░███   ░███ ███░███ ░███ ░███
░░████████░░███████░░██████  ████ █████  ░░█████ ░░████████████
 ░░░░░░░░  ░░░░░███ ░░░░░░  ░░░░ ░░░░░    ░░░░░   ░░░░░░░░░░░░
           ███ ░███
          ░░██████      the bounded agent runtime
           ░░░░░░
```

**A predeclared DAG walks. An LLM fills one node. Nothing improvises.**

[![ci](https://github.com/agentd-dev/source-code/actions/workflows/ci.yml/badge.svg)](https://github.com/agentd-dev/source-code/actions/workflows/ci.yml)
[![license: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
[![rust](https://img.shields.io/badge/rust-1.85%2B-orange.svg)](crates/agentd/Cargo.toml)

[Quick start](#quick-start) · [The loop](#the-agent-loop) · [Capabilities](#capabilities) · [Security model](#the-security-model) · [When not to use it](#when-not-to-use-agentd) · [Docs](docs/README.md)

</div>

---

`agentd` is a single-binary runtime for **bounded intelligence workflows**.
You author a directed acyclic graph of typed nodes in TOML. The runtime
validates it (at compile time if you bake it in, always at load time), then
executes it — one-shot from the CLI, or as a daemon reacting to HTTP
webhooks, cron schedules, and filesystem events.

The industry calls systems like this *workflows* — "LLMs and tools
orchestrated through predefined code paths" — as opposed to *agents*, where
the model directs its own process. agentd is deliberately, structurally on
the workflow side of that line: the LLM is **one node type** (`llm_infer`),
a bounded reasoning step with a templated prompt and an optional JSON
output contract. It cannot add nodes, choose edges, or invent tool calls.
Routing on its output is the `switch` node's job, declared by you.

That inversion is the whole point. Production experience across the
industry has converged on explicit control flow with the model doing local
reasoning — because the graph boundary is also the audit boundary, the
security boundary, and the cost ceiling.

```
your workflow.toml ──► validate (build-time + load-time)
                              │
   trigger ───────────────────▼──────────────────────────────┐
   (HTTP / cron / fs-watch /  │   ENGINE: walk the DAG        │
    manual --input)           │   one node at a time          │
                              │                               │
        ┌─────────────────────┼───────────────────────┐      │
        │ read_file  read_env │ parse_json  json_select│      │
        │ template_render     │ diff_compute           │      │
        │ llm_infer ◄─────────┤ bounded reasoning step │      │
        │ write_file  http_request  call_mcp_tool      │      │
        │ shell_run (allowlisted)                      │      │
        │ switch / condition / merge / fail / terminate│      │
        └─────────────────────┬───────────────────────┘      │
                              │ every side effect gated by    │
                              │ [policy] + budgets + deadline │
                              ▼                               │
                    outcome JSON + execution trace ◄──────────┘
                    (what actually ran, node by node)
```

## Quick start

```bash
cargo build --release -p agentd

# Validate a workflow and exit
./target/release/agentd --config examples/webhook-receiver.toml --validate-only

# Run one-shot with a payload
./target/release/agentd --config examples/llm-classifier.toml \
    --intel-unix /run/intel.sock --input doc.json

# Serve mode is inferred: [[http_routes]] in the TOML → HTTP daemon
GITHUB_WEBHOOK_SECRET=s3cret \
./target/release/agentd --config examples/webhook-receiver.toml --bind 127.0.0.1:8080

# Walk the whole graph, skip every side effect
./target/release/agentd --config examples/cron-poller.toml --start on_tick --dry-run
```

A complete workflow, 30 seconds of reading:

```toml
name = "review"

[[start_nodes]]
name = "main"
source = "manual"
entry_node = "analyze"

[[nodes]]
id = "analyze"
type = "llm_infer"            # the bounded reasoning step
backend = "default"
prompt = "Summarize: {{body}}. Reply as JSON {\"verdict\": \"ship\"|\"hold\"}"
input_from = "trigger"
output_schema = "schemas/verdict.json"

[[nodes]]
id = "decide"
type = "switch"               # routing is declared, not improvised
expr = "analyze.parsed.verdict"

[[nodes]]
id = "record"
type = "write_file"
path_from = "out.rendered"
content_from = "analyze.content"

[[nodes]]
id = "out"
type = "template_render"
template = "/tmp/review/verdict.json"

[[nodes]]
id = "halt"
type = "terminate"

[[edges]]
from = "analyze"
to = "decide"

[[edges]]
from = "decide"
when = "ship"
to = "out"

[[edges]]
from = "decide"
when = "hold"
to = "halt"

[[edges]]
from = "out"
to = "record"

[[edges]]
from = "record"
to = "halt"
```

## The agent loop

The engine is a sequential interpreter over your graph. There is exactly
one loop in the system, and you wrote it:

1. **Trigger** — an HTTP request (bearer / HMAC / mTLS / OIDC verified,
   rate-limited), a cron tick, a filesystem event, or `--input` from the
   CLI. The payload becomes the reserved `trigger` context entry.
2. **Resolve + validate** — start node → entry node; the DAG was already
   checked for duplicate ids, dangling edges, cycles (Kahn), reachability,
   and unknown server refs before any of this.
3. **Walk** — for each node: check the deadline, dispatch to the node's
   handler (with per-node retry/backoff/jitter if declared), record the
   output under the node's id, follow the matching edge. Handlers never
   choose successors — they emit values and optional branch labels; the
   engine resolves edges.
4. **Stop** — `terminate` (success), `fail` (declared failure), deadline
   (`timed_out`), or a dead end (completion with the last value). A
   `MAX_STEPS` cap backstops the validator's acyclicity proof.
5. **Report** — outcome JSON on stdout / HTTP response, plus an execution
   trace: the exact node path with outcomes and branch labels. Metrics
   counters and `agentd::audit` events stream alongside.

`llm_infer` fits inside step 3 like any other node: render prompt →
one request through the intelligence client (Unix socket or HTTP JSON-RPC)
→ optionally require valid JSON → store `{content, parsed, usage}`.
Whether the workflow continues, branches, or stops is encoded in edges the
model never sees.

## Capabilities

| Surface | What ships |
|---|---|
| **Node kinds** | `read_file` `read_env` `read_mcp_resource` `parse_json` · `template_render` `json_select` `diff_compute` · `llm_infer` · `write_file` `create_dir` `http_request` `call_mcp_tool` `shell_run` · `condition` `switch` `merge` `fail` `terminate` |
| **Triggers** | HTTP/1.1 server (hand-rolled, keep-alive, drain-on-SIGTERM), cron + interval, fs-watch (debounced), manual |
| **Auth** | Bearer (constant-time), HMAC-SHA256 webhooks (GitHub/Stripe pattern), mTLS (fingerprint + CN/SAN principals), OIDC/JWT against a pinned JWKS |
| **Policy** | Fail-closed allowlists per family (fs paths, env keys, HTTP URLs+methods, shell commands, MCP tools/resources) + optional Rego layered as a logical AND |
| **Budgets** | Memory (RLIMIT_AS / Job Objects), CPU time, wall-clock per run (clamps the CLI flag), cumulative fs-write bytes |
| **Reliability** | Per-node retry (linear backoff, jitter, transient-only filters), per-run deadlines, graceful drain, SIGHUP / touch-file hot reload of TLS·auth·policy·routes·MCP·intel |
| **Observability** | Structured spans (`workflow.run` → `node.execute`), Prometheus `/metrics`, `/healthz`, dedicated audit JSONL sink with redaction, W3C traceparent propagation in and out, optional OTLP gRPC export |
| **Supply chain** | ed25519-signed workflows (verified before parsing trust begins), embedded configs validated at compile time |

Every row above that touches the outside world is a **Cargo feature**.
The default build has no outbound HTTP and no shell. A sealed webhook
appliance compiles like this:

```bash
cargo build --release -p agentd \
  --no-default-features \
  --features "tools-fs,tools-data,trigger-http,auth,server-tls"
```

That binary *cannot* make an outbound request or spawn a process — not
"is configured not to", it does not contain the code. CI builds and tests
each canonical feature set on its own.

## The security model

Prompt injection is the defining attack on tool-using LLM systems, and
prompt-level defenses are brittle by consensus. agentd's answer is
architectural, not prompt-engineered:

- **Control-flow integrity by construction.** The research literature
  calls the strongest design "plan-then-execute": fix the control flow
  before untrusted data is ever read, so a hostile document can corrupt a
  *value* but never the *program*. agentd's plan is the signed TOML —
  there is no code path by which tool output, webhook bodies, or model
  text add nodes, edges, or capabilities at runtime.
- **The graph boundary is the audit boundary.** Everything the process
  can possibly do is enumerable from the workflow file plus the feature
  flags. Reviews, threat models, and diffs operate on one declarative
  artifact.
- **Cut the lethal trifecta at compile time.** Private data + untrusted
  content + external communication is the exfiltration recipe; a build
  without `tools-http`/`tools-shell` removes the third leg in a way no
  runtime misconfiguration can restore.
- **Least privilege is the default.** Empty policy sections deny. A
  server exposing an MCP tool doesn't make it callable; the allowlist
  does. Deny messages name the operation and the path that was blocked,
  and land in the audit stream.

## When NOT to use agentd

Honesty section. A frozen graph is the wrong tool when:

- **The step count is genuinely unpredictable** — open-ended research,
  exploratory debugging, "keep going until it works". That's the
  documented domain of model-driven agents; use one (ideally sandboxed),
  not this.
- **The workflow must restructure itself from observations at runtime.**
  agentd's switch nodes select among *declared* paths; they cannot invent
  a new branch. Distribution shift that invalidates your graph means
  editing the TOML — by design, that edit is reviewable and signable.
- **You need durable, resumable multi-day executions.** Runs are
  in-memory and bounded; a crash re-runs the workflow rather than
  resuming mid-graph. (Checkpoint/resume is on the roadmap; Temporal-class
  durability is not the target.)

The hybrid pattern the ecosystem converged on — a deterministic outer
process with bounded model-driven inner steps — is exactly what `llm_infer`
+ `switch` give you today, and where the roadmap deepens next.

## Project layout

```
crates/agentd/     the runtime (lib + bin)
docs/              architecture · capabilities · configuration · operations · maturity
rfcs/              0001 bounded workflow runtime · 0002 signed workflows
examples/          validated, runnable workflow TOMLs
packaging/         systemd unit + debian scripts (deb/rpm via cargo-deb/generate-rpm)
web/               documentation site
```

```bash
cargo test -p agentd                  # default suite
cargo test -p agentd --all-features   # everything
cargo test -p agentd --test cli_smoke # end-to-end binary + sockets
```

Design record: [`rfcs/0001-bounded-workflow-runtime.md`](rfcs/0001-bounded-workflow-runtime.md).
Operator docs: [`docs/`](docs/README.md). Maturity, with named gaps:
[`docs/maturity.md`](docs/maturity.md).

## License

MIT. See [LICENSE](LICENSE).
