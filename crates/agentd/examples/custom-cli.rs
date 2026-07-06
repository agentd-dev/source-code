// SPDX-License-Identifier: Apache-2.0
//! A minimal EMBEDDER (RFC 0022): a custom CLI built on the `agentd-core`
//! library that registers a native Rust tool and drives a workflow calling it —
//! **no network, no LLM, no MCP server**; the deterministic subset runs
//! self-contained. Compiled by CI (`cargo test` builds examples), so this file
//! is the living, compile-guaranteed embedding contract.
//!
//! Run: `cargo run -p agentd-core --example custom-cli --features workflow`
//!
//! The three obligations of every embedder, in order:
//!   1. the SUBAGENT RE-EXEC DISPATCH first — subagents re-exec `current_exe()`
//!      (YOUR binary), and the child must take the subagent path;
//!   2. register code tools BEFORE running anything — registration in `main`
//!      is what makes a tool visible in every re-exec'd process of the tree;
//!   3. then run: hand agentd a config (the stock CLI shape) or drive the
//!      engine directly (this example drives a workflow with its own executor).

use agentd::graph::{Blackboard, DriveResult, GraphExec};
use serde_json::{Value, json};

fn main() {
    // ── 1. The re-exec dispatch (REQUIRED for any embedder using subagents,
    //       async subgraphs, or served runs). Harmless otherwise.
    if std::env::var_os(agentd::subagent::protocol::SUBAGENT_ENV).is_some() {
        std::process::exit(agentd::subagent::control::run());
    }

    // ── 2. Register native tools. This is the "tools by code" seam: the
    //       closure is ordinary Rust — your crates, your I/O, your rules. It
    //       becomes callable from the agent loop, from workflow `tool` nodes
    //       (server `code`), and over the served MCP catalogue.
    agentd::tools::register(agentd::tools::CodeTool::new(
        "shout",
        "Uppercase the input text.",
        json!({"type": "object",
               "properties": {"text": {"type": "string"}},
               "required": ["text"]}),
        |args| {
            let text = args.get("text").and_then(Value::as_str).unwrap_or("");
            Ok(json!({ "text": text.to_uppercase() }))
        },
    ))
    .expect("unique tool name");

    // ── 3. Drive a workflow that calls it. A real embedder would usually build
    //       a Config and run the full supervised stack exactly like
    //       `agentd-cli/src/main.rs`; here we drive the graph engine directly
    //       with our own executor to stay offline.
    let graph = agentd::graph::parse_graph(&json!({
        "start": "seed",
        "nodes": {
            "seed": { "kind": "assign", "value": { "text": "ship it" }, "writes": "input",
                      "edges": { "ok": "shout", "error": "fail" } },
            "shout": { "kind": "tool", "server": "code", "tool": "shout",
                       "args": { "text": { "$from": "input", "pointer": "/text" } },
                       "writes": "loud", "edges": { "ok": "done", "error": "fail" } },
            "done": { "kind": "halt", "status": "completed", "result_from": "loud" },
            "fail": { "kind": "halt", "status": "crashed" }
        }
    }))
    .expect("valid workflow");

    /// The embedder's executor: code tools route through the public
    /// [`agentd::tools::call`]; everything else is refused (this example wires
    /// no LLM and no MCP servers). The production executor is
    /// `agentd::graph::SessionExec`, which does all of this and more.
    struct OfflineExec;
    impl GraphExec for OfflineExec {
        fn run_agent(
            &mut self,
            _instruction: &str,
            _output_contract: Option<&str>,
            _blackboard: &Blackboard,
            _reads: &[String],
        ) -> (Value, bool) {
            (json!("no intelligence wired in this example"), true)
        }
        fn call_tool(&mut self, server: &str, tool: &str, args: &Value) -> (Value, bool) {
            if server == "code" {
                return match agentd::tools::call(tool, args) {
                    Some(Ok(v)) => (v, false),
                    Some(Err(e)) => (Value::String(e), true),
                    None => (json!(format!("no such code tool {tool:?}")), true),
                };
            }
            (
                json!(format!("no MCP servers wired (asked for {server:?})")),
                true,
            )
        }
    }

    match agentd::graph::drive(&graph, &mut OfflineExec, 50) {
        DriveResult::Done(outcome) => {
            println!("workflow: {:?}", outcome.status);
            println!("result:   {}", outcome.result);
            assert_eq!(outcome.result["text"], json!("SHIP IT"));
        }
        DriveResult::Suspended(s) => unreachable!("no waits in this graph: {}", s.on_uri),
    }
}
