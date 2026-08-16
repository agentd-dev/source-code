// SPDX-License-Identifier: Apache-2.0
//! The tool-registry contract (RFC 0028): internal tools round-trip to the
//! supervisor and take effect; an unknown tool the model invents is answered as
//! an error by the child without sinking the run; and the introspected surface
//! (`--capabilities`) lists the internal tools. Driven black-box against the real
//! binary with the built-in mock LLM.

use crate::checks::util::{mock_llm, write_file};
use crate::{Category, Check, Harness, Outcome};
use serde_json::json;

pub fn checks() -> Vec<Check> {
    vec![
        Check {
            id: "tools/internal-round-trip",
            category: Category::Tools,
            desc: "internal tools (memory.set, plan.create) round-trip to the supervisor and take effect",
            run: internal_round_trip,
        },
        Check {
            id: "tools/unknown-tool-errors-not-crashes",
            category: Category::Tools,
            desc: "an unknown tool the model invents is answered as an error; the run still completes",
            run: unknown_tool_errors,
        },
        Check {
            id: "tools/registry-introspection",
            category: Category::Tools,
            desc: "--capabilities lists the internal tool registry (2.0 runtime)",
            run: registry_introspection,
        },
    ]
}

fn job_config(llm: &str) -> String {
    format!(
        "config_version: \"2\"\n\
         agent:\n  instruction: do a thing\n\
         intelligence:\n  endpoints: {llm}\n  model: mock\n\
         observability:\n  log_level: info\n"
    )
}

fn internal_round_trip(h: &Harness) -> Outcome {
    let tmp = h.tempdir();
    let llm = mock_llm(
        h,
        &tmp,
        &json!({"turns": [
            {"tool_calls": [{"name": "memory.set", "arguments": {"key": "greeting", "value": "hello"}}]},
            {"tool_calls": [{"name": "plan.create", "arguments": {"goal": "greet", "items": ["say hello"]}}]},
            {"content": "done"}
        ]}),
    );
    let cfg = write_file(&tmp, "agentd.yaml", &job_config(&llm.uri));
    let r = h.run(&["--config", &cfg]);
    let reqs: Vec<String> = r
        .events()
        .iter()
        .filter(|e| e["event"] == "tool.request")
        .filter_map(|e| e["tool"].as_str().map(String::from))
        .collect();
    Outcome::require(
        r.code == Some(0),
        format!("want exit 0, got {:?}; stderr:\n{}", r.code, r.stderr),
    )
    .and(|| {
        Outcome::require(
            reqs.iter().any(|t| t == "memory.set") && reqs.iter().any(|t| t == "plan.create"),
            format!("internal tool requests should round-trip; saw {reqs:?}"),
        )
    })
    .and(|| {
        Outcome::require(
            r.events()
                .iter()
                .any(|e| e["event"] == "plan.updated" && e["op"] == "create"),
            "plan.create should take effect (a plan.updated op=create event)".to_string(),
        )
    })
}

fn unknown_tool_errors(h: &Harness) -> Outcome {
    let tmp = h.tempdir();
    let llm = mock_llm(
        h,
        &tmp,
        &json!({"turns": [
            {"tool_calls": [{"name": "no.such.tool", "arguments": {}}]},
            {"content": "recovered"}
        ]}),
    );
    let cfg = write_file(&tmp, "agentd.yaml", &job_config(&llm.uri));
    let r = h.run(&["--config", &cfg]);
    // The child answers the unknown tool itself and keeps going: the run still
    // reaches a completed terminal status and exits 0.
    Outcome::require(
        r.code == Some(0),
        format!(
            "an unknown tool must not crash the run; want exit 0, got {:?}; stderr:\n{}",
            r.code, r.stderr
        ),
    )
    .and(|| {
        let done = r
            .events()
            .into_iter()
            .filter(|e| e["event"] == "run.done")
            .collect::<Vec<_>>();
        Outcome::require(
            done.len() == 1 && done[0]["status"] == "completed",
            format!("the run should complete despite the unknown tool: {done:?}"),
        )
    })
}

fn registry_introspection(h: &Harness) -> Outcome {
    let tmp = h.tempdir();
    // --capabilities is pure config introspection: no reachable intelligence needed.
    let cfg = write_file(&tmp, "agentd.yaml", &job_config("https://127.0.0.1:9"));
    let r = h.run(&["--config", &cfg, "--capabilities"]);
    if r.code != Some(0) {
        return Outcome::fail(format!(
            "--capabilities should exit 0, got {:?}; stderr:\n{}",
            r.code, r.stderr
        ));
    }
    let v: serde_json::Value = match serde_json::from_str(&r.stdout) {
        Ok(v) => v,
        Err(e) => {
            return Outcome::fail(format!("capabilities not JSON: {e}; stdout:\n{}", r.stdout));
        }
    };
    Outcome::require(v["runtime"] == "2.0", format!("runtime should be 2.0: {v}")).and(|| {
        let tools = v["internal_tools"].as_array().cloned().unwrap_or_default();
        Outcome::require(
            tools.iter().any(|t| t == "workflow.run"),
            format!("internal tool registry should list workflow.run: {tools:?}"),
        )
    })
}
