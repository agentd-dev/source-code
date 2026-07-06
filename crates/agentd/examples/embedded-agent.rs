// SPDX-License-Identifier: Apache-2.0
//! The AGENTIC LOOP embedded in a host application (RFC 0022) — not a CLI:
//! your program calls `run_loop` directly and gets `(Outcome, Usage)` back as
//! ordinary Rust values. A CODE-REGISTERED tool rides along, so the model can
//! call into YOUR functions mid-reasoning. Compiled by CI; running it needs a
//! live OpenAI-compatible endpoint:
//!
//! ```console
//! AGENT_INTELLIGENCE=https://gw.example/v1 \
//! AGENT_INTELLIGENCE_TOKEN=… AGENT_MODEL=my-model \
//! cargo run -p agentd-core --example embedded-agent
//! ```
//!
//! Optionally add remote MCP tools too: `AGENT_MCP=fs=https://mcp-fs.internal/mcp`.
//!
//! Trade-off to understand (RFC 0022 §3): this runs the reasoning IN YOUR
//! PROCESS — simplest integration, no process isolation. When you want the
//! supervisor's kill-ladder/limits around the model (the stock posture), spawn
//! the run as a supervised subtree instead: build a `SpawnPayload` and call
//! `supervisor::reactor::supervise_once` (exactly what `agentd-cli` does), and
//! install the re-exec dispatch at the top of your `main`.

use agentd::agentloop::action::SelfHandler;
use agentd::agentloop::runner::{LoopInput, run_loop};
use agentd::intel::client::IntelClient;
use agentd::mcp::client::McpClient;
use agentd::obs::log::{Comp, Level, LogCtx, Logger};
use agentd::wire::intel::ToolDef;
use serde_json::{Value, json};
use std::time::{Duration, Instant};

/// This embedder offers the model no orchestration self-tools (no subagent
/// spawning from a host app that didn't install the re-exec dispatch) — the
/// loop then runs pure think→tool→observe over MCP + code tools.
struct NoSelfTools;
impl SelfHandler for NoSelfTools {
    fn tools(&self) -> Vec<ToolDef> {
        Vec::new()
    }
    fn handle(&mut self, _name: &str, _args: &Value) -> Option<(String, bool)> {
        None
    }
}

fn main() {
    // ── 1. Native tools first: plain Rust the model can call mid-reasoning.
    agentd::tools::register(agentd::tools::CodeTool::new(
        "word_count",
        "Count the words in a text.",
        json!({"type": "object",
               "properties": {"text": {"type": "string"}},
               "required": ["text"]}),
        |args| {
            let text = args.get("text").and_then(Value::as_str).unwrap_or("");
            Ok(json!({ "words": text.split_whitespace().count() }))
        },
    ))
    .expect("unique tool name");

    // ── 2. Wiring from the environment (the same names the stock CLI honors).
    let Ok(intel_uri) = std::env::var("AGENT_INTELLIGENCE") else {
        eprintln!("set AGENT_INTELLIGENCE (and optionally AGENT_INTELLIGENCE_TOKEN,");
        eprintln!("AGENT_MODEL, AGENT_MCP=name=https://…) to run this example");
        return;
    };
    let intel = IntelClient::from_parts(&intel_uri, std::env::var("AGENT_INTELLIGENCE_TOKEN").ok())
        .expect("intelligence endpoint");
    let mut servers: Vec<McpClient> = Vec::new();
    if let Ok(spec) = std::env::var("AGENT_MCP") {
        let (name, endpoint) = spec.split_once('=').expect("AGENT_MCP=name=endpoint");
        let server_spec = agentd::config::McpServerSpec {
            name: name.into(),
            endpoint: endpoint.into(),
            ..Default::default()
        };
        let mut client =
            agentd::mcp::from_spec(&server_spec, Duration::from_secs(60)).expect("mcp connect");
        client.initialize().expect("mcp initialize");
        servers.push(client);
    }
    let log = Logger::new(
        LogCtx {
            run_id: "embedded-example".into(),
            agent_id: "0".into(),
            agent_path: "0".into(),
            comp: Comp::Agent,
            pid: std::process::id(),
            trace_id: None,
        },
        Level::Info,
    );

    // ── 3. One agentic run, as a function call. The model sees `word_count`
    //       (your code) next to any MCP tools, and the whole run is bounded by
    //       the same budget machinery the stock CLI uses.
    let input = LoopInput {
        instruction: "How many words are in the sentence \
                      'the quick brown fox jumps over the lazy dog'? \
                      Use the word_count tool, then answer with just the number."
            .into(),
        output_contract: Some("Answer with a single integer.".into()),
        seed: Vec::new(),
        model: std::env::var("AGENT_MODEL").unwrap_or_default(),
        max_steps: 10,
        max_tokens: 20_000,
        deadline: Instant::now() + Duration::from_secs(120),
        cancel: None,
    };
    match run_loop(&intel, &servers, &input, &mut NoSelfTools, &log) {
        Ok((outcome, usage)) => {
            println!("status:  {:?}", outcome.status);
            println!("result:  {}", outcome.result);
            println!(
                "tokens:  {} in / {} out",
                usage.input_tokens, usage.output_tokens
            );
        }
        Err(abort) => eprintln!("infrastructure failure: {abort:?}"),
    }
}
