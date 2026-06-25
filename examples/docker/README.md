# agentd container image

A multi-stage build that produces a tiny, nonroot `agentd` image: a musl
static binary on `gcr.io/distroless/static-debian12:nonroot` (no shell, no
package manager, UID 65532).

> **Status:** the binary currently validates config, sets up logging, and
> exits with a scaffold notice for the run modes — the supervisor, agentic
> loop, and MCP client land across milestones M1–M3. See
> [`docs/design/PLAN.md`](../../docs/design/PLAN.md). The commands below are
> the intended v1 behavior; today they will validate config and print the
> scaffold notice rather than run an agent.

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
  --build-arg FEATURES=tls,vsock,cron -t agentd:full .
```

Feature flags map to the crate's `[features]` (see `crates/agentd/Cargo.toml`):
`tls` (rustls+ring, bundled roots), `vsock`, `serve-mcp`, `cron`, `metrics`,
`otel`. The default build has none of these on.

## Run — one-shot (`once`)

Mode defaults to `once`: run the instruction to a terminal status, then exit.
Intelligence and the instruction are supplied per run.

```sh
docker run --rm \
  -e INSTRUCTION="Summarize /data/report.txt and write the summary via the fs MCP server." \
  -e AGENTD_INTELLIGENCE="https://api.example/v1" \
  -e AGENTD_INTELLIGENCE_TOKEN="$TOKEN" \
  -e AGENTD_MODEL="claude-sonnet-4-5" \
  agentd:tls
```

`AGENTD_INTELLIGENCE_TOKEN` is a secret — it is **never** logged and is
redacted in any debug output (see `crates/agentd/src/config.rs`). Pass it via
env or `--intelligence-token`, never via a config file.

With the default (no-TLS) image, point at a plaintext-terminating endpoint:

```sh
docker run --rm \
  -e INSTRUCTION="…" \
  -e AGENTD_INTELLIGENCE="unix:/run/intel.sock" \
  -v /run/intel.sock:/run/intel.sock \
  agentd:latest
```

## MCP servers are stdio — bundle or co-locate

agentd ships no tools of its own except a gated `exec` (off by default;
`--enable-exec` / `AGENTD_ENABLE_EXEC`). **All** other tools come from MCP
servers that agentd spawns over **stdio**:

```sh
agentd \
  --mcp fs=/usr/local/bin/mcp-server-fs --root /data \
  --mcp queue=/usr/local/bin/mcp-server-queue
```

Because stdio is a child process (not a network call), each server binary must
be reachable **inside the container's process namespace**. So either:

1. **Bundle** the MCP server binaries into the image (extend the runtime stage
   with `COPY` lines) and reference them by absolute path in `--mcp`; or
2. **Co-locate** them in the same pod. stdio does not cross a container
   boundary on its own, so this needs `shareProcessNamespace: true` (and a way
   to exec the sidecar's binary), which is more involved — bundling is the
   simpler v1 path.

The example `Dockerfile` bundles nothing; add `COPY` lines for the servers you
need, or build a downstream image `FROM agentd:latest`.

## v1 scope notes

- **Reactivity is stdio-only in v1** (no reactive-over-HTTP). The
  `reactive`-mode subscriptions ride stdio MCP servers. *(roadmap: reactive
  over HTTP.)*
- **Serving agentd's own MCP** (`--serve-mcp unix:/path`, requires the
  `serve-mcp` feature) is **stdio/unix only** in v1. *(roadmap: HTTP serving.)*
- **Async subagents** land in M3; v1 spawn is synchronous. *(roadmap.)*
- MCP **tasks / sampling / roots** are deferred (rfcs/0013). *(roadmap.)*

## Exit codes

agentd's exit codes are a public, machine-actionable contract (rfcs/0011 §5)
that a Kubernetes `podFailurePolicy` can branch on — e.g. `2` (usage/config)
is non-retriable, `4` (intelligence unreachable) and `6` (MCP down) are
retriable. The k8s samples in [`examples/k8s/`](../k8s/) wire these up.

The Kubernetes operator that schedules, replicates, and rolls these pods is
**external and not part of this project** (rfcs/0011 §1). agentd only honors
the contract; orchestration lives in your cluster.
