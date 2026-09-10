# agentd container image

A multi-stage build that produces a tiny, nonroot `agentd` image: a musl
static binary on `gcr.io/distroless/static-debian12:nonroot` (no shell, no
package manager, UID 65532).

> **Status:** implemented and released (v2.0.0). The supervisor, agentic loop,
> MCP client, and served self-MCP over HTTP(S) all ship; the commands below run
> real agent runs given an intelligence endpoint + MCP servers.

## Build

Default (minimalism) build — **no TLS**, no async runtime, no C toolchain:

```sh
docker build -f examples/docker/Dockerfile -t agentd:latest .
```

Add features at build time with `--build-arg FEATURES=...`:

```sh
# direct https:// intelligence/MCP without a TLS-terminating sidecar
docker build -f examples/docker/Dockerfile \
  --build-arg FEATURES=tls -t agentd:tls .

# multiple features
docker build -f examples/docker/Dockerfile \
  --build-arg FEATURES=a2a,cron,metrics,otel -t agentd:full .
```

Feature flags map to the crate's `[features]` (see `crates/agentd-cli/Cargo.toml`,
which forwards 1:1 to `crates/agentd/Cargo.toml`): `tls` (rustls+ring, bundled roots
— **on by default**, it is the transport), `a2a`, `cron`, `metrics`, `otel`,
`hot-reload`, `config-watch`, `cel`, `aauth`, `sign`, `oci`, `decrypt`, `oauth`,
`exec`. There is also a declared `workflow` feature, deliberately left out of that
list: the engine compiles unconditionally, so turning it on changes nothing you can
observe — which is why the released binaries do not set it either.

## Run — one-shot (`once`)

Mode defaults to `once`: run the instruction to a terminal status, then exit.
Intelligence and the instruction are supplied per run.

```sh
docker run --rm \
  -e INSTRUCTION="Summarize /data/report.txt and write the summary via the fs MCP server." \
  -e AGENT_INTELLIGENCE="https://api.example/v1" \
  -e AGENT_INTELLIGENCE_TOKEN="$TOKEN" \
  -e AGENT_MODEL="claude-sonnet-4-5" \
  agentd:tls
```

`AGENT_INTELLIGENCE_TOKEN` is a secret — it is **never** logged and is
redacted in any debug output (the `Secret` newtype's `Debug` writes `***` —
`crates/agentd/src/config/v2/mod.rs`; resolution never touches a log line —
`crates/agentd/src/sec/secret.rs`). Pass it via
env or `--intelligence-token`, never via a config file.

The default image (no `FEATURES`) already keeps TLS out — point it at a
**same-host sidecar over loopback** that terminates TLS:

```sh
docker run --rm \
  -e INSTRUCTION="…" \
  -e AGENT_INTELLIGENCE="http://127.0.0.1:4000/v1" \
  agentd:no-tls
```

## MCP servers are remote HTTP endpoints

agentd ships no tools of its own and runs no local code by default. The one
exception is `exec`, the guarded local command runner, off at build time (the `exec`
cargo feature, not in the released binaries) and again at run time
(`security.exec.enabled`); without both it is a mapping-only contract whose
execution is delegated off-box through `tools.overrides`. **Every other** tool comes
from an MCP server that agentd reaches over **Streamable HTTP** — it connects to a
URL, it spawns no process:

```sh
agentd \
  --mcp fs=https://mcp-fs.internal/mcp \
  --mcp queue=https://mcp-queue.internal/mcp
```

Because a server is a remote HTTP endpoint (not a child process), **nothing
MCP-related is bundled into the agentd image**. Deploy each MCP server as its own
service and point `--mcp` at its URL; per-server auth headers go in the config file.

## Scope notes

- **Reactivity rides the MCP servers' Streamable-HTTP subscriptions** — agentd
  subscribes and reacts to pushed `notifications/resources/updated` over HTTP/SSE.
- **Serving agentd's own MCP** (`--serve-mcp https://host:port`, which sets
  `a2a.listen` and needs the `a2a` feature) is over HTTP(S) with mTLS/bearer auth
  (loopback `http://` for dev).
- **Agent-authored cyclic workflows** are in every build — the engine is
  unconditional; `cel` is what `when:` / `until:` / `filter:` need, and it is in the
  released binaries.
- MCP **tasks / sampling / roots** are deferred (rfcs/0013).

## Exit codes

agentd's exit codes are a public, machine-actionable contract (rfcs/0011 §5)
that a Kubernetes `podFailurePolicy` can branch on — e.g. `2` (usage/config)
is non-retriable, `4` (intelligence unreachable) and `6` (MCP down) are
retriable. The k8s samples in [`examples/k8s/`](../k8s/) wire these up.

The Kubernetes operator that schedules, replicates, and rolls these pods is
**external and not part of this project** (rfcs/0011 §1). agentd only honors
the contract; orchestration lives in your cluster.
