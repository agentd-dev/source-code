# Embedding — the agentd engine in your app

agentd ships as **two published crates around one engine**: `agentd-core` (the
library — lib name `agentd`) and `agentd-cli` (the thin binary shell that
produces the stock `agentd` command). Everything the stock CLI does, it does by
calling the library; your binary can do the same — with **your own native Rust
tools** registered into the agent.

| You want… | Use |
|---|---|
| the stock agent runtime | `cargo install agentd-cli` (or the release binaries/image) |
| your own CLI with native tools | depend on `agentd-core`, follow this page |
| just MCP client/server or transports | `agentd-mcp` / `agentd-net` |
| to drive agentd from another program | the process contract ([operations.md](operations.md)) or the served MCP/A2A wire ([mcp.md](mcp.md)) — no linking needed |

> The normative contract is **RFC 0022** (obligations, precedence, stability
> tiers). The compile-guaranteed reference is
> [`crates/agentd/examples/custom-cli.rs`](../crates/agentd/examples/custom-cli.rs)
> — run it with `cargo run -p agentd-core --example custom-cli --features workflow`.

## The three obligations

```rust
fn main() {
    // 1. THE RE-EXEC DISPATCH, FIRST. Subagents re-exec current_exe() — YOUR
    //    binary. Without this, any spawn re-runs your CLI as a confused parent.
    if std::env::var_os(agentd::subagent::protocol::SUBAGENT_ENV).is_some() {
        std::process::exit(agentd::subagent::control::run());
    }

    // 2. REGISTER CODE TOOLS — before anything runs, so every re-exec'd child
    //    process registers them too (that is the whole visibility mechanism).
    agentd::tools::register(agentd::tools::CodeTool::new(
        "shout",
        "Uppercase the input text.",
        serde_json::json!({"type": "object",
                           "properties": {"text": {"type": "string"}},
                           "required": ["text"]}),
        |args| {
            let text = args.get("text").and_then(serde_json::Value::as_str).unwrap_or("");
            Ok(serde_json::json!({ "text": text.to_uppercase() }))
        },
    )).expect("unique tool name");

    // 3. RUN — either the full stock stack (parse a Config and drive a mode,
    //    exactly like agentd-cli/src/main.rs), or the engine directly.
    // …
}
```

One more rule: **one process = one agent runtime.** The tool registry, signal
handling, metrics, and the live-workflow slot are process-global by design (the
re-exec model requires it).

## What a registered tool can do

Once registered, `shout` is:

- **in the agent loop's catalogue** — the model calls it like any tool; if a
  remote MCP server publishes a colliding name, **your code tool wins** (a
  server cannot steal a first-party tool's calls);
- **addressable from workflows** as the reserved server name `code`:

  ```json
  { "kind": "tool", "server": "code", "tool": "shout",
    "args": { "text": { "$from": "input", "pointer": "/text" } },
    "writes": "loud", "edges": { "ok": "next", "error": "fail" } }
  ```

- **callable by your own executors** via `agentd::tools::call(name, &args)`;
- **visible in the manifest** — `--capabilities` shows
  `surfaces.code_tools: N` (absent on the stock CLI, which registers nothing —
  its no-local-code posture is preserved by construction).

Handlers are plain Rust (`Fn(&Value) -> Result<Value, String> + Send + Sync`),
may run concurrently (loop + workflow lanes), and `Err(reason)` is the normal
tool-error path — the model sees a failed call; a workflow takes the `error`
edge. Registration refuses duplicates and agentd's own self/control names
(`subagent.*`, `workflow.*`, …) — the orchestration surface is unshadowable.

Trust: a code tool is **your compiled code** — first-party like the rest of
your binary, outside the `--mcp-tags` trifecta accounting. You own what it
touches.

## Recipes — agentic logic inside your app

Four levels, thinnest first. The first two are shipped as **compile-guaranteed
examples** (CI builds them; the snippets below are excerpts of real files).

### Recipe 1 — one agentic run as a function call

Your app calls the loop directly and gets `(Outcome, Usage)` back as plain
Rust values — the model sees your code tools next to any MCP tools. Full file:
[`crates/agentd/examples/embedded-agent.rs`](../crates/agentd/examples/embedded-agent.rs).

```rust
use agentd::agentloop::runner::{run_loop, LoopInput};
use agentd::intel::client::IntelClient;

// native tools first (see “The three obligations”)
agentd::tools::register(agentd::tools::CodeTool::new(
    "word_count", "Count the words in a text.",
    json!({"type":"object","properties":{"text":{"type":"string"}},"required":["text"]}),
    |args| Ok(json!({ "words": args["text"].as_str().unwrap_or("").split_whitespace().count() })),
))?;

let intel = IntelClient::from_parts("https://gw.example/v1", token)?;
let input = LoopInput {
    instruction: "Count the words in this review and summarize it.".into(),
    output_contract: Some("JSON: {words, summary}".into()),
    seed: vec![],                       // narrowed context, (role, content) pairs
    model: "my-model".into(),
    max_steps: 10, max_tokens: 20_000,
    deadline: Instant::now() + Duration::from_secs(120),
    cancel: None,                       // or an Arc<AtomicBool> you flip
};
let (outcome, usage) = run_loop(&intel, &servers, &input, &mut NoSelfTools, &log)?;
println!("{} ({} tokens)", outcome.result, usage.input_tokens + usage.output_tokens);
```

The run is bounded by the same budget machinery the stock CLI uses
(steps/tokens/deadline + a cooperative cancel flag). Trade-off: the reasoning
runs **in your process** — no supervisor isolation; when you want the kill
ladder around the model, use Recipe 3. (CI compiles this example; it was
verified end-to-end against the built-in mock intelligence.)

### Recipe 2 — a workflow (deterministic + intelligent steps) in your app

Author a dialect-2 graph as data, drive it with your own executor — the whole
RFC 0021 surface (reducers, `parallel`, `human` gates, the checkpointer) works
from an embedder. Full file:
[`crates/agentd/examples/custom-cli.rs`](../crates/agentd/examples/custom-cli.rs).

```rust
let graph = agentd::graph::parse_graph(&json!({
    "start": "seed",
    "nodes": {
        "seed":  { "kind": "assign", "value": { "text": "ship it" }, "writes": "input",
                   "edges": { "ok": "shout", "error": "fail" } },
        "shout": { "kind": "tool", "server": "code", "tool": "shout",
                   "args": { "text": { "$from": "input", "pointer": "/text" } },
                   "writes": "loud", "edges": { "ok": "done", "error": "fail" } },
        "done":  { "kind": "halt", "status": "completed", "result_from": "loud" },
        "fail":  { "kind": "halt", "status": "crashed" }
    }
}))?;
match agentd::graph::drive(&graph, &mut my_exec, 50) {
    DriveResult::Done(outcome) => println!("{:?}: {}", outcome.status, outcome.result),
    DriveResult::Suspended(s) => { /* arm s.on_uri / s.gate, resume() later */ }
}
```

Your executor implements `GraphExec` — two required methods (`run_agent`,
`call_tool`; everything else has safe defaults), so you decide what an `agent`
node or an MCP call means in your app. The production executor
(`agentd::graph::SessionExec`) is available when you want the stock behavior,
including checkpointing and parallel lanes.

### Recipe 3 — the full supervised stack (the stock posture)

When you want the kill ladder, cgroup limits, liveness, and the exit-code
contract AROUND the model, do what `agentd-cli/src/main.rs` does: install the
re-exec dispatch, build a `SpawnPayload`, and call
`agentd::supervisor::reactor::supervise_once` — the reasoning then runs in a
killable child of *your* binary, and everything in this documentation set
(modes, workflows, serving, A2A) applies unchanged. The CLI's `main.rs` is
deliberately small enough to read as the reference (~900 lines including all
five modes).

### Recipe 4 — just the pieces

- `agentd-mcp`: the MCP client (dual-era, Streamable HTTP) and server machinery
  — use agentd's MCP stack without the agent.
- `agentd-net`: the blocking HTTP/1.1+SSE client, TLS, SSRF guard.
- `agentd::intel::client::IntelClient`: the OpenAI-compatible client with
  endpoint-list failover and breakers.

## Depending on the crates

```toml
[dependencies]
# lib name is `agentd`, so code reads `use agentd::…`
agentd = { package = "agentd-core", version = "1.2", features = ["workflow"] }
```

(The crates.io name `agentd` belongs to an unrelated project — hence the
`-core` package name with the `agentd` lib name.) Features mirror the build
features in [configuration.md](configuration.md); the feature graph is the
same one the stock CLI forwards.

## What is stable

- **Frozen with the product**: the process contract (exit codes, reports), the
  wire contracts (MCP/A2A), the workflow dialect JSON, the manifest shape.
- **Semver-honored embedding seams**: `agentd::tools::*`, the workflow engine
  (`parse_graph`/`drive`/`GraphExec`/…), the re-exec dispatch pair
  (`SUBAGENT_ENV` + `subagent::control::run`), `Config::load`, `exit::*`.
- **Everything else `pub`** is visible but unstable — it exists for the CLI
  and the test suites. Pin a version. RFC 0022 §5 is the authoritative list.
