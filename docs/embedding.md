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

> The compile-guaranteed reference is
> [`crates/agentd/examples/embedded-agent.rs`](../crates/agentd/examples/embedded-agent.rs)
> — run it with `cargo run -p agentd-core --example embedded-agent`.

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

    // 3. RUN — either the full stock stack (load a settings document and
    //    hand it to agentd::runtime::run, exactly like agentd-cli/src/main.rs),
    //    or the agent loop directly.
    // …
}
```

One more rule: **one process = one agent runtime.** The tool registry, signal
handling, and metrics are process-global by design (the re-exec model requires
it).

## What a registered tool can do

Once registered, `shout` is:

- **in the agent loop's catalogue** — the model calls it like any tool; if a
  remote MCP server publishes a colliding name, **your code tool wins** (a
  server cannot steal a first-party tool's calls);
- **addressable from a workflow** as a `tool` step, by the name you registered:

  ```yaml
  shout:
    kind: tool
    depends_on: [start]
    name: shout
    args: { text: "{{steps.start.output.text}}" }
  ```

- **callable by your own code** via `agentd::tools::call(name, &args)`;
- **counted by `agentd::tools::count()`** — zero on the stock CLI, which
  registers nothing, so its no-local-code posture is preserved by construction.

Handlers are plain Rust (`Fn(&Value) -> Result<Value, String> + Send + Sync`),
may run concurrently (loop + workflow lanes), and `Err(reason)` is the normal
tool-error path — the model sees a failed call; a workflow step applies its
`on_error` (`fail` | `continue` | `goto:<step>`) and any `retry`. Registration
refuses duplicates and agentd's own internal names (`subagent.*`, `workflow.*`,
…) — the orchestration surface is unshadowable.

Trust: a code tool is **your compiled code** — first-party like the rest of
your binary, outside the `--mcp-tags` trifecta accounting. You own what it
touches.

## Recipes — agentic logic inside your app

Four levels, thinnest first. Recipe 1 is shipped as a **compile-guaranteed
example** (CI builds it; the snippet below is an excerpt of the real file).

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

### Recipe 2 — workflows as data in your app

A workflow is a dialect-3 JSON/YAML document, and the engine that owns it is
`agentd::engine`: `parse_workflow` validates a document into a `Workflow` (or
returns every error at once), `workflow_schema()` hands you the same JSON Schema
`agentd --workflow-schema` prints, and `engine::run` is the **pure scheduler** —
`RunState` is the durable run record, `schedule(&wf, &mut run, &data)` answers
`Ready(ids)` / `Waiting` / `Stalled` / `Terminal`.

```rust
use agentd::engine::{parse_workflow, run::{schedule, Next, RunState, Start}};

let wf = parse_workflow(&json!({
    "name": "shouty",
    "steps": {
        "start": { "kind": "once" },
        "shout": { "kind": "tool", "depends_on": ["start"], "name": "shout",
                   "args": { "text": "{{steps.start.output.text}}" } },
        "done":  { "kind": "finish", "depends_on": ["shout"],
                   "output": "{{steps.shout.output}}" }
    }
})).map_err(|errs| errs.join("; "))?;

let mut run = RunState::new("run-1", &wf, Start::default(), json!({}));
let data = agentd::engine::template::Data::new();   // the template view: steps, vars, env
match schedule(&wf, &mut run, &data)? {
    Next::Ready(ids) => { /* execute these steps, record their outcomes */ }
    Next::Waiting | Next::Stalled | Next::Terminal => {}
}
```

Step **execution** — turn workers, MCP calls, internal tools, timers,
checkpoints — is the runtime's job, not the engine's. Embedding a workflow that
actually runs therefore means Recipe 3: hand the document to `agentd::runtime`
and let it drive the same durable machinery the stock CLI uses. Use the engine
directly when you want validation, the schema, or the scheduler's decisions
without owning a runtime.

### Recipe 3 — the full supervised stack (the stock posture)

When you want the kill ladder, cgroup limits, liveness, the durable store, and
the exit-code contract AROUND the model, do what `agentd-cli/src/main.rs` does:
install the re-exec dispatch, load a `config_version: "1"` document with
`agentd::config::v2::load`, and call `agentd::runtime::run(&loaded, args, env)`
— the reasoning then runs in killable children of *your* binary, and everything
in this documentation set (the lifecycle, workflow triggers, and A2A) applies
unchanged. The CLI's `main.rs` is deliberately small enough to read as the
reference: the re-exec dispatch, the early-exit asks (`--help`,
`--config-schema`, `--validate-config`, `--capabilities`, `--login`/`--logout`),
and the `run_v2` entrypoint.

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
agentd = { package = "agentd-core", version = "2.0", features = ["a2a", "metrics"] }
```

(The crates.io name `agentd` belongs to an unrelated project — hence the
`-core` package name with the `agentd` lib name.) Features mirror the build
features in [configuration.md](configuration.md); the feature graph is the
same one the stock CLI forwards.

## What is stable

- **Frozen with the product**: the process contract (exit codes, reports), the
  wire contracts (MCP/A2A), the workflow dialect JSON, the manifest shape.
- **Semver-honored embedding seams**: `agentd::tools::*`, the workflow engine
  (`engine::{parse_workflow, workflow_schema, Workflow, RunState}`), the re-exec
  dispatch pair (`SUBAGENT_ENV` + `subagent::control::run`),
  `config::v2::load`, `runtime::run`, `exit::*`.
- **Everything else `pub`** is visible but unstable — it exists for the CLI
  and the test suites, and may change in any release. Pin a version, and treat
  the two lists above as the whole of what you may depend on.
