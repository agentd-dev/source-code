# Quickstart

Five minutes from zero to a running, inspectable agent. No API key
required for the first run.

> New to the design? The [README](../README.md) explains *why* the graph
> is frozen. This page is about *doing*. For the full surface, see
> [capabilities](capabilities.md) and [configuration](configuration.md).

## 0. Install

```bash
# Linux x86_64 · macOS Apple Silicon — detects your platform
curl -fsSL https://agentd.dev/install.sh | sh

# …or build from source (Rust 1.88+)
cargo build --release -p agentd
```

Check it:

```bash
agentd --version
```

The rest of this page assumes `agentd` is on your `PATH`. From a source
build, substitute `./target/release/agentd`.

## 1. Run your first workflow

[`examples/hello.toml`](../examples/hello.toml) is the smallest useful
workflow: read a name from the environment, render a greeting, write it
to a file. No LLM, no network, nothing to configure.

```bash
AGENTD_NAME=world agentd --config examples/hello.toml
cat /tmp/agentd-hello/greeting.txt
# → Hello, world - your agent is alive.
```

You just ran a five-node DAG. The outcome is JSON on stdout:

```json
{ "status": "completed", "final_value": null, "last_node": "done" }
```

Open the file — it's 60 lines of TOML, and every line is a typed node, a
declared edge, or a fail-closed policy. The `[policy.env]` block lets the
run read exactly one env var; `[policy.fs]` lets it write under exactly
one directory. Everything else is denied.

## 2. See exactly what it did

Record a run, then replay it as a timeline:

```bash
AGENTD_NAME=beta agentd --config examples/hello.toml --record run.json
agentd inspect run.json
```

```
run exec-6a2a2df4-2f7b04-1  workflow=hello  status=completed
  start=main  0 ms  0 llm call(s) / 0 tokens  0 policy denial(s)
  path:
     1. who [read_env] continue  0 ms
        output: {"key":"AGENTD_NAME","value":"beta"}
     2. greeting [template_render] continue  0 ms
        output: {"rendered":"Hello, beta - your agent is alive.\n"}
     3. path [template_render] continue  0 ms
        output: {"rendered":"/tmp/agentd-hello/greeting.txt"}
     4. save [write_file] continue  0 ms
        output: {"bytes":35,"path":"...","written":true}
     5. done [terminate] terminate  0 ms
```

Every node's input, output, cost, timing, and any policy denial is in the
record. The same record drops into the browser inspector at `/inspect`.

## 3. Validate before you run

The graph is checked — duplicate ids, dangling edges, cycles,
unreachable nodes, unknown backends — *before* anything executes. Make it
the first thing your CI does:

```bash
agentd --config examples/hello.toml --validate-only
# → { "ok": true, "workflow": "hello" }
```

Want to walk the whole graph but skip every side effect? `--dry-run`
runs the traversal with all tool handlers stubbed:

```bash
agentd --config examples/hello.toml --dry-run
```

## 4. Add reasoning

A reasoning step is one node — `llm_infer` — not a loop that drives the
program. [`examples/llm-classifier.toml`](../examples/llm-classifier.toml)
classifies an input document:

```bash
# Point at any backend: a hosted API key, or a local Unix-socket model.
ANTHROPIC_API_KEY=sk-… agentd --config examples/llm-classifier.toml --input doc.json

# Or stub the model and walk the graph first:
agentd --config examples/llm-classifier.toml --input doc.json --dry-run
```

Backends are named in the TOML; keys come from env vars, never the file:

```toml
[[intelligence.backends]]
name     = "default"
provider = "anthropic"          # or openai · gemini · openai-compatible · unix · http
model    = "claude-opus-4-8"
api_key_env = "ANTHROPIC_API_KEY"
```

Give an `llm_infer` node an `output_schema` and (with the `schema`
feature) the model's JSON output is enforced against it, with bounded
`output_repairs` re-prompts on a violation. The model's text is *data* —
which edge the workflow takes next is decided by edges the model never
sees.

## 5. Serve it

Add `[[http_routes]]` and agentd infers serve mode — it becomes an HTTP
daemon instead of a one-shot.
[`examples/webhook-receiver.toml`](../examples/webhook-receiver.toml) is
an HMAC-verified webhook:

```bash
GITHUB_WEBHOOK_SECRET=s3cret \
  agentd --config examples/webhook-receiver.toml --bind 127.0.0.1:8080
```

In another shell:

```bash
curl -sS localhost:8080/healthz          # always-live, no auth
# signed requests hit the route; unsigned ones get a 401
```

Triggers can also be `cron`/`interval` (see
[`examples/cron-poller.toml`](../examples/cron-poller.toml)) or
`fs-watch`. The daemon drains in-flight runs on `SIGTERM` and hot-reloads
TLS / auth / policy / routes on `SIGHUP`.

## 6. Let the agent write the workflow

The most dynamic mode: hand agentd an instruction and it *compiles* a
workflow — capability-injected, validated, approval-gated — then runs it.

```bash
ANTHROPIC_API_KEY=… agentd --config prod.toml \
  --instruction "Audit access logs under /var/log/app and write a summary" \
  --plan-only                          # inspect the compiled workflow first
# Looks right? add --auto-approve to execute (headless refuses without it).
```

The compiled plan inherits `prod.toml`'s policy, budgets, and backends
verbatim — the agent **cannot widen its own policy**. Once a plan is
proven, `--promote out.toml` freezes it into a normal, signable workflow:
design-time dynamism that collapses to a production bound.

## Where to go next

| You want to… | Read |
|---|---|
| See every node kind, trigger, and knob | [capabilities.md](capabilities.md) |
| Write the full TOML (policy, budgets, auth, TLS) | [configuration.md](configuration.md) |
| Follow worked examples end-to-end | [SAMPLES.md](SAMPLES.md) · [`examples/`](../examples/) |
| Deploy (systemd, container, k8s, drain, hot-reload) | [operations.md](operations.md) |
| Gate autonomy on measured reliability | [CONFORMANCE.md](CONFORMANCE.md) |
| Understand the security boundary | [../SECURITY.md](../SECURITY.md) |
| Know what's production-ready and what isn't | [maturity.md](maturity.md) |
| See what's planned after 1.0 | [ROADMAP.md](ROADMAP.md) |
