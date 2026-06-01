# agentd — bounded workflow runtime

One binary, one entry point. Behaviour is inferred from the workflow
config — a workflow with `[[http_routes]]` starts an HTTP server;
anything else runs one-shot and exits. No subcommand dispatch, no
choose-your-own-mode — the workflow file is the source of truth.

## Documentation

Authoritative, task-oriented docs live under
[`docs/`](../../docs/):

- [`README.md`](../../docs/README.md) — master index.
- [`architecture.md`](../../docs/architecture.md) — mental
  model, module layout, lifecycle.
- [`capabilities.md`](../../docs/capabilities.md) — every
  `NodeKind`, policy / auth / TLS / rate-limit grammars, triggers,
  execution outcome.
- [`configuration.md`](../../docs/configuration.md) — every
  TOML field, CLI flag, env var, Cargo feature.
- [`operations.md`](../../docs/operations.md) — deploy
  shapes, TLS setup, shutdown semantics, runbook basics.
- [`maturity.md`](../../docs/maturity.md) — production
  readiness snapshot with named gaps.

Design record: [`rfcs/0001-bounded-workflow-runtime.md`](../../rfcs/0001-bounded-workflow-runtime.md).

## Invocation

```
agentd [--config FILE]            AGENTD_CONFIG            required unless embedded
      [--input FILE]             AGENTD_INPUT             one-shot trigger payload
      [--start NAME]             AGENTD_START             explicit start node
      [--mode once|serve]        AGENTD_MODE              override auto-inferred mode
      [--bind HOST:PORT]         AGENTD_HTTP_BIND         server-mode bind (default 127.0.0.1:8080)
      [--timeout-secs N]         AGENTD_TIMEOUT_SECS      per-run deadline (default 120)
      [--drain-timeout-secs N]   AGENTD_DRAIN_TIMEOUT_SECS  graceful-shutdown grace (default 30)
      [--intel-unix PATH]        AGENTD_INTEL_UNIX
      [--mcp-stdio "CMD ARGS"]   AGENTD_MCP_STDIO
      [--dry-run]                AGENTD_DRY_RUN=1
      [--validate-only]          AGENTD_VALIDATE_ONLY=1
      [--log-level LEVEL]        AGENTD_LOG               (default warn)
      [--log-format text|json]   AGENTD_LOG_FORMAT        (default text)
      [--log-target TARGET]      AGENTD_LOG_TARGET        stderr | stdout | file:PATH
      [--quiet]                  AGENTD_QUIET=1
      [--version] [--help]
```

## Typical uses

```bash
# Validate a workflow and exit
agentd --config my.toml --validate-only

# Run one workflow once; write outcome JSON to stdout
agentd --config my.toml --start main --input payload.json

# Start an HTTP server (mode auto-inferred from [[http_routes]])
agentd --config my.toml --bind 127.0.0.1:8080

# Dry-run — walk the DAG, skip every side effect
agentd --config my.toml --start main --dry-run

# Use a baked-in config (build with AGENTD_EMBED_CONFIG set) — no
# --config needed at runtime; `agent` just works.
agentd --start main
```

## Embedded vs external config

```bash
# Embed the config at build time
AGENTD_EMBED_CONFIG=./my-workflow.toml cargo build --release -p agentd

# Resulting binary starts doing its work on run — no config flag,
# no config file to ship alongside.
./target/release/agentd --input payload.json
```

`build.rs` validates the embedded config at compile time. Typos,
dangling edges, duplicate node ids fail the build with a clear error;
they never become a runtime surprise. The full validator (cycles,
reachability, fan-in/out, start-node shape, policy refs) still runs
at load time — build-time is a strict subset to keep compile fast.

## Mode inference

- Workflow has `[[http_routes]]` → **serve mode**. Binds a TCP
  listener, routes requests, drains on SIGTERM/SIGINT within
  `--drain-timeout-secs`, then exits.
- No HTTP routes → **one-shot mode**. Reads `--input`, runs once
  from the chosen start node, prints outcome, exits.

Override with `--mode serve` or `--mode once` if the default is wrong
for the deployment. Exit codes: `0` success, `2` usage, `5` validation
/ engine / drain-timeout failure (see
[`operations.md §5.4`](../../docs/operations.md)).

## Cargo features

| Feature | Effect | Default |
|---|---|---|
| `tools-fs` | `read_file`, `write_file`, `create_dir` | **on** |
| `tools-env` | `read_env` | **on** |
| `tools-data` | `parse_json`, `json_select`, `template_render` | **on** |
| `tools-http` | Outbound `http_request` | off |
| `tools-shell` | Allowlisted `shell_run` | off |
| `tools-mcp` | MCP outbound `call_mcp_tool`, `read_mcp_resource` | off |
| `trigger-http` | HTTP server mode | **on** |
| `trigger-mcp` | MCP trigger | off |
| `intel-unix` | Intelligence JSON-RPC over Unix socket | off |
| `intel-http` | Intelligence JSON-RPC over HTTP | off |
| `auth` | Bearer + HMAC-SHA256 webhook verification | **on** |
| `server-tls` | In-process TLS + optional mTLS (implies `auth`) | off |

Build a sealed, capability-pruned binary:

```bash
# Hardened webhook receiver: TLS + HMAC + fs writes only
cargo build --release -p agentd \
  --no-default-features \
  --features "tools-fs,tools-data,trigger-http,auth,server-tls"
```

See [`operations.md §2`](../../docs/operations.md) for the
four canonical build recipes (default / hardened-webhook /
kitchen-sink / embedded-appliance).

## Tests

```bash
cargo test -p agentd                             # default suite (~276 tests)
cargo test -p agentd --all-features              # exercise every feature
cargo test -p agentd --test fixture_suite        # workflow fixtures
cargo test -p agentd --test cli_smoke            # end-to-end CLI + HTTP server
cargo test -p agentd --test build_time_validation  # build.rs paths
cargo test -p agentd --test tracing_smoke        # subscriber install
```

Fixture authors: drop a directory under `tests/fixtures/<name>/` with
`workflow.toml` + `fixture.toml`; auto-discovery picks it up.
External consumers call `agentd::testing::run_fixture("path")` from
their own tests.

## Module layout

```
src/
├── main.rs              one line: agentd::runtime::run(argv)
├── lib.rs               module registry
├── runtime.rs           single-entry dispatcher (mode inference, arg parsing)
├── embedded.rs          EMBEDDED_CONFIG under cfg(embed_config)
├── error.rs             runtime Error enum
├── policy.rs            ManifestPolicy + glob matcher
├── server_config.rs     [server.tls] + [server.tls.client_auth]
├── ratelimit.rs         per-route TokenBucket
├── signals.rs           SIGTERM/SIGINT via sigaction → AtomicBool
├── workflow/            model + TOML parse + DAG validator
├── engine/              runner, context, outcome+trace, handlers
├── tools/               fs / env / data / http / shell (feature-gated)
├── auth/                bearer / HMAC / mTLS + config + verifier trait
├── intelligence/        client trait + UnixClient + MockClient + LlmInferHandler
├── mcp/                 client trait + StdioMcpClient + allowlist + handlers
├── triggers/
│   ├── http.rs          HTTP/1.1 server (hand-rolled; generic over Read+Write)
│   └── http_tls.rs      rustls server config + accept (server-tls feature)
├── observability/       tracing init + Metrics + CapturingWriter + LogTarget
└── testing/             Fixture format + runner + discover_fixtures
```

## License

MIT. See [`LICENSE`](../../LICENSE).
