# RFC 0022: Embedding — the library crates, the CLI shell, and code-registered tools

**Status:** Implemented (2026-07-06)
**Author:** Andrii Tsok
**Date:** 2026-07-06
**Part of:** the product's second consumption surface. RFC 0011 defined the *process* contract (agentd as a unit of work); this RFC defines the *library* contract (agentd as an engine another binary embeds). RFC 0012's no-local-code posture and RFC 0021's workflow dialect are load-bearing here.

---

## 1. Problem / Context

agentd's business logic — the agentic loop, the supervisor, workflows, the MCP
client/server, the intelligence client — lived in one `lib + bin` crate that was
`publish = false` and whose Rust surface carried no stability promise. Teams
wanting to **build their own CLI on the agentic engine** (with their own tools,
their own branding, their own distribution) had only two options: fork, or drive
the binary over the process/wire contracts. Both are right for some consumers;
neither is a library story.

Separately, the engine had no way to accept **native tools registered by code**:
every task tool had to be a remote MCP server. That is exactly right for the
*stock* binary (the no-local-code posture IS the product), but an embedder's own
compiled code is first-party by definition — refusing it protects nothing (the
embedder already controls the process) and forces an MCP round-trip for what is
a function call.

**This RFC owns:** the crate split and its naming, the embedder's obligations
(the re-exec dispatch), the code-tool registration surface and its trust/
precedence rules, and what is and is not API-stable. **It does not own:** the
process contract (RFC 0011), the wire contracts (RFC 0004/0005/0020), or the
workflow dialect (RFC 0021) — embedders get all three unchanged.

## 2. The crates (naming pinned by crates.io availability)

| Package (crates.io) | Lib/bin name | What it is |
|---|---|---|
| `agentd-core` | lib **`agentd`** | THE ENGINE: every module the old crate had (`agentloop`, `supervisor`, `graph`, `intel`, `mcp`, `triggers`, `tools`, …). Publishable. |
| `agentd-cli` | bin **`agentd`** | the thin CLI shell: argv dispatch + exit codes; ~900 lines, zero business logic. Publishable (`cargo install agentd-cli` yields the `agentd` binary). |
| `agentd-mcp` | lib `mcp` | the reusable MCP library (wire, dual-era client, Streamable-HTTP server). Publishable. |
| `agentd-net` | lib `net` | transport primitives (blocking HTTP/1.1+SSE, TLS, SSRF guard). Publishable. |

The name `agentd` on crates.io is **taken by an unrelated project** — hence
`agentd-core`, with `[lib] name = "agentd"` so embedders still write
`use agentd::…` (and in-tree consumers use Cargo dependency-renaming:
`agentd = { package = "agentd-core", … }`). The CLI crate's feature set is a
1:1 forward of the library's AND mirrors its internal feature graph
(`serve-https` implies `serve-mcp`, …) because `main.rs`'s own `cfg` gates
evaluate against the CLI crate's features; CI compiles both crates per matrix
row so a graph drift fails loudly.

## 3. The embedder's obligations (binding)

An embedder building a binary on `agentd-core`:

1. **MUST install the subagent re-exec dispatch first thing in `main`** when it
   uses anything that spawns (subagents, async subgraphs, served runs,
   `--mode workflow`):

   ```rust
   if std::env::var_os(agentd::subagent::protocol::SUBAGENT_ENV).is_some() {
       std::process::exit(agentd::subagent::control::run());
   }
   ```

   Subagents re-exec `current_exe()` — the **embedder's** binary. Without the
   dispatch, a spawn re-runs the embedder's CLI as a confused supervisor.
2. **MUST register code tools before that dispatch reaches run** (i.e. at the
   top of `main`): registration is how a tool exists in every re-exec'd process
   of the tree. Registration after spawning is visible only in the current
   process — supported (the registry is live) but almost never what you want.
3. **SHOULD treat one process = one agent runtime.** `signals`, `obs::metrics`,
   `graph::live`, the gate bus, and the tool registry are process-global by
   design (the re-exec model requires it). Embedding several isolated runtimes
   in one process is out of contract.

The compile-guaranteed reference embedder is
[`crates/agentd/examples/custom-cli.rs`](../crates/agentd/examples/custom-cli.rs)
(built by CI; runs offline).

## 4. Code-registered tools

`agentd::tools` is the seam (module: `crates/agentd/src/tools.rs`):

```rust
agentd::tools::register(agentd::tools::CodeTool::new(
    "shout", "Uppercase the input text.",
    json!({"type":"object","properties":{"text":{"type":"string"}},"required":["text"]}),
    |args| Ok(json!({ "text": args["text"].as_str().unwrap_or("").to_uppercase() })),
))?;
```

Semantics (binding):

- **Dispatch priority: self-tools → code tools → MCP.** Registration REFUSES
  the self/control names (`SELF_CONTROL_TOOLS` — the orchestration surface is
  unshadowable) and duplicates. A code tool **wins a name collision with a
  remote MCP tool** — in the catalogue (one def per name; the MCP entry is
  dropped) and in dispatch — so a rogue or coincidental server cannot steal a
  first-party tool's calls. `ToolClass` gains a `Code` variant matching this.
- **Workflows address code tools as the reserved server name `code`**:
  `{"kind":"tool","server":"code","tool":"shout",…}`. Config validation refuses
  `--mcp code=…`. An unregistered name is a normal tool-error (`error` edge).
- **Handlers**: `Fn(&Value) -> Result<Value, String> + Send + Sync`, called
  from the loop, workflow nodes, and parallel lanes concurrently; run outside
  the registry lock. `Err` is the tool-error path, never a panic.
- **The stock CLI registers nothing.** `agentd-cli` contains no registration
  call, so every stock binary's registry is empty — the no-local-code posture
  is preserved *by construction*, not by policy. The capabilities manifest
  surfaces `surfaces.code_tools: N` only when N > 0
  (capability-absence-not-error), so an embedder's binary is honestly
  distinguishable from stock.
- **Trust:** a code tool is the embedder's own compiled code — first-party like
  the binary itself, OUTSIDE the `--mcp-tags` trifecta accounting (RFC 0012 §3).
  The embedder owns that risk the way it owns the rest of its binary.

## 5. API stability (what an embedder may rely on)

Three tiers, narrowest promise first:

1. **Frozen with the product contracts** (change = major): the process contract
   (RFC 0011), the wire contracts (0004/0005/0020), the workflow dialect JSON
   (RFC 0021), the capabilities manifest shape (RFC 0014 §5 additive rules).
2. **The embedding seams — semver-honored from first publish**: `tools::{
   CodeTool, register, unregister, call, count}`; `graph::{parse_graph, drive,
   drive_from, resume, GraphExec, Graph, GraphOutcome, GraphState, DriveResult,
   WaitOutcome}`; `subagent::protocol::SUBAGENT_ENV` + `subagent::control::run`;
   `config::Config::load`; `exit::*`. Breaking these bumps the crates' major.
3. **Everything else `pub`** exists for the CLI, the tests, and the conformance
   suite. It is visible, usable, and UNSTABLE — pin a version.

## 6. What deliberately did NOT change

- The stock binary's behavior, features, size, and posture: `agentd-cli`
  produces byte-equivalent behavior to the pre-split binary.
- The minimalism moat: the split adds zero dependencies; the CI minimalism gate
  now counts `agentd-core`'s direct externals (still exactly 3).
- The conformance suite: black-box against the built binary, now `-p agentd-cli`.

## 7. Deferred

- **Trifecta tags on code tools** (`CodeTool::with_tags(…)` feeding the RFC
  0012 gate): the gate runs at config validation; registration order relative
  to `Config::load` is the embedder's, so wiring this needs a registration
  deadline or a re-check — deferred until an embedder needs it.
- Code tools as served-MCP *resources* (only tools are surfaced today).
- A `#[agentd::tool]` derive macro (would add a proc-macro dependency — weigh
  against the moat when asked for).
- Publishing automation (`cargo publish` order: net → mcp → core → cli) is a
  release-runbook note, not CI, until the user's first publish.
