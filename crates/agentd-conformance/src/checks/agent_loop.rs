// SPDX-License-Identifier: Apache-2.0
//! The agentic ReAct loop end-to-end: the agent must take the LLM's tool calls,
//! execute them, feed results back, and converge on a final answer. Driven with
//! the built-in mock LLM (scripted tool calls) + the mock MCP (a resource to
//! read), validated by the run's exit code, its printed result, and its
//! `tool.call` / `tool.result` / `loop.final` telemetry.

use crate::harness::RunResult;
use crate::{Category, Check, Harness, Outcome};
use serde_json::Value;

pub fn checks() -> Vec<Check> {
    vec![
        Check {
            id: "agent-loop/direct-final-answer",
            category: Category::AgentLoop,
            desc: "an LLM that answers immediately completes in one step",
            run: direct_final_answer,
        },
        Check {
            id: "agent-loop/tool-call-and-result",
            category: Category::AgentLoop,
            desc: "the agent executes an LLM tool call and feeds the result back",
            run: tool_call_and_result,
        },
        Check {
            id: "agent-loop/multi-step-converges",
            category: Category::AgentLoop,
            desc: "a tool-call turn + an answer turn converge on the post-tool result",
            run: multi_step_converges,
        },
        Check {
            id: "agent-loop/budget-bounds-the-loop",
            category: Category::AgentLoop,
            desc: "--max-steps cuts off a non-converging loop → exit 7, exhausted_steps",
            run: budget_bounds_the_loop,
        },
    ]
}

/// Run once-mode against the mock LLM `script`, optionally wiring the mock MCP so
/// `resource.read` of `file:///in.json` resolves. `--log-level info` surfaces the
/// loop telemetry.
fn run_loop(h: &Harness, script: &str, with_mcp: bool) -> (RunResult, crate::harness::MockLlm) {
    let llm = h.mock_llm(script);
    let mut args: Vec<String> = vec![
        "--instruction".into(),
        "use the resource if needed".into(),
        "--intelligence".into(),
        llm.uri.clone(),
        "--model".into(),
        "m".into(),
        "--log-level".into(),
        "info".into(),
    ];
    if with_mcp {
        args.push("--mcp".into());
        args.push(format!(
            "{} --no-emit",
            h.mock_mcp_spec("mock", "file:///in.json")
        ));
    }
    let argref: Vec<&str> = args.iter().map(String::as_str).collect();
    (h.run(&argref), llm)
}

fn final_event(r: &RunResult) -> Option<Value> {
    r.events().into_iter().find(|e| e["event"] == "loop.final")
}

fn direct_final_answer(h: &Harness) -> Outcome {
    let (r, _llm) = run_loop(h, "final", false);
    Outcome::require(
        r.code == Some(0),
        format!("want exit 0, got {:?}; stderr:\n{}", r.code, r.stderr),
    )
    .and(|| {
        Outcome::require(
            !r.stdout.trim().is_empty(),
            "no final answer printed to stdout".to_string(),
        )
    })
    .and(|| {
        let f = final_event(&r);
        Outcome::require(
            f.as_ref()
                .map(|f| f["status"] == "completed")
                .unwrap_or(false),
            format!("no loop.final completed event: {:?}", f),
        )
    })
}

fn tool_call_and_result(h: &Harness) -> Outcome {
    let (r, _llm) = run_loop(h, "read", true);
    if r.code != Some(0) {
        return Outcome::fail(format!(
            "want exit 0, got {:?}; stderr:\n{}",
            r.code, r.stderr
        ));
    }
    let events = r.events();
    let called = events
        .iter()
        .any(|e| e["event"] == "tool.call" && e["tool"] == "resource.read");
    if !called {
        return Outcome::fail("agent never issued the resource.read tool call".to_string());
    }
    let result_ok = events
        .iter()
        .any(|e| e["event"] == "tool.result" && e["is_error"] == false);
    Outcome::require(
        result_ok,
        "no successful tool.result was fed back".to_string(),
    )
}

fn multi_step_converges(h: &Harness) -> Outcome {
    let (r, _llm) = run_loop(h, "read", true);
    if r.code != Some(0) {
        return Outcome::fail(format!(
            "want exit 0, got {:?}; stderr:\n{}",
            r.code, r.stderr
        ));
    }
    // The read script is a 2-turn ReAct: call resource.read, then answer.
    let steps = final_event(&r)
        .and_then(|f| f["steps"].as_u64())
        .unwrap_or(0);
    Outcome::require(
        steps >= 2,
        format!("expected ≥2 steps (tool + answer), saw {steps}"),
    )
    .and(|| {
        Outcome::require(
            r.stdout.contains("read complete"),
            format!(
                "final answer didn't reflect the post-tool result; stdout: {:?}",
                r.stdout.trim()
            ),
        )
    })
}

fn budget_bounds_the_loop(h: &Harness) -> Outcome {
    // The read script needs 2 steps (tool, then answer); capped at 1 it can't
    // converge, so the loop must stop on its step budget rather than run away.
    let llm = h.mock_llm("read");
    let mcp = format!("{} --no-emit", h.mock_mcp_spec("mock", "file:///in.json"));
    let r = h.run(&[
        "--instruction",
        "use the resource",
        "--intelligence",
        &llm.uri,
        "--model",
        "m",
        "--max-steps",
        "1",
        "--mcp",
        &mcp,
        "--log-level",
        "info",
    ]);
    Outcome::require(
        r.code == Some(7),
        format!(
            "want exit 7 (budget), got {:?}; stderr:\n{}",
            r.code, r.stderr
        ),
    )
    .and(|| {
        Outcome::require(
            r.events()
                .iter()
                .any(|e| e["event"] == "loop.final" && e["status"] == "exhausted_steps"),
            "no exhausted_steps terminal event".to_string(),
        )
    })
}
