# agentd-core

[agentd](https://agentd.dev) as a library: the agentic loop, the supervisor that
owns it, durable workflows, the A2A surface, and the config system — everything
the `agentd` binary is, minus the binary.

Most people want the binary. Reach for this crate when you need agentd's loop
*inside* your own process — an embedded agent in a larger service, a custom
front end over the same runtime, or tools registered in Rust rather than behind
an MCP server.

```toml
[dependencies]
agentd-core = "1.16"
```

```rust,no_run
use agentd::tools;

// Register a tool the model can call, in-process — no MCP server needed.
tools::register("deploy.status", "Current deployment status", |_args| {
    Ok(serde_json::json!({"env": "prod", "healthy": true}))
});
```

See `examples/embedded-agent.rs` in the repository for a complete one.

## What you get

- **The ReAct loop** — think, call a tool, observe, repeat, until an answer or a
  budget, with a context window that compacts rather than truncates.
- **A supervisor holding no model.** Lifecycle, limits and the process tree live
  in a component that cannot be prompted; the reasoning runs in child processes
  it can always kill. Cancellation is `killpg`, not a dropped future.
- **Durable workflows** — a graph checkpointed before every effect, so a
  restarted process rebuilds in-flight runs from the store and continues.
- **MCP and A2A**, implemented by the official SDKs —
  [`rmcp`](https://crates.io/crates/rmcp) and the A2A project's
  [`a2a-server-lf`](https://crates.io/crates/a2a-server-lf) — over
  [`agentd-net`](https://crates.io/crates/agentd-net)'s credentialed transport.
- **A security posture that is checked, not documented** — tools tagged
  untrusted-input / sensitive / egress, with the lethal trifecta refused at
  startup; secrets by reference only, never inline, never logged.

## Features

Capability is a compile-time decision. `tls` is on by default; `a2a`, `workflow`,
`cron`, `metrics`, `otel`, `oauth`, `aauth`, `hot-reload`, `config-watch`, `cel`
and `exec` are opt-in, and a flag whose feature is absent exits `2` loudly rather
than being ignored. MSRV is 1.96.

The build is pure Rust with every feature: the crypto provider is `ring`
throughout, and the A2A SDK's protobuf compiler comes vendored.

Full documentation: <https://agentd.dev/docs/>. Licensed under Apache-2.0.
