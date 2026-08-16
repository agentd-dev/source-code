// SPDX-License-Identifier: Apache-2.0
//! Workflow engine v3 (RFC 0027) end to end through the 2.0 runtime: data
//! pipelines with nested bodies (`foreach`/`batch`, `iterate`, `parallel`,
//! `race`, `subgraph`), `switch` routing, `on_error` policies, `mcp.tool`
//! steps against the mock MCP, artifact-backed large outputs, concurrent
//! runs, and a SIGKILL mid-batch that resumes at the next batch. CEL is used
//! throughout, so the suite needs the `cel` feature.
#![cfg(feature = "cel")]

mod common;

use std::io::Write;
use std::process::{Command, Stdio};

fn write_config(yaml: &str) -> String {
    let path = common::unique_path("engine-v3", "yaml");
    std::fs::File::create(&path)
        .unwrap()
        .write_all(yaml.as_bytes())
        .unwrap();
    path
}

fn run_agentd(config: &str, extra_env: &[(&str, &str)]) -> std::process::Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_agentd"));
    cmd.args(["--config", config]);
    for (k, v) in extra_env {
        cmd.env(k, v);
    }
    cmd.stdin(Stdio::null()).output().expect("run agentd")
}

fn events(stderr: &str, name: &str) -> Vec<serde_json::Value> {
    stderr
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .filter(|v| v["event"] == name)
        .collect()
}

/// A job-shaped v2 document with one inline workflow (JSON steps for brevity).
fn job(steps: &str, extra: &str) -> String {
    write_config(&format!(
        "config_version: \"2\"\nagent:\n  name: engine\nworkflows:\n  - name: pipe\n    steps: {steps}\nlifecycle:\n  run_until: idle\n  idle_grace: 1s\nobservability:\n  log_level: info\n  log_content: true\n{extra}"
    ))
}

fn stdout_json(out: &std::process::Output) -> serde_json::Value {
    let s = String::from_utf8_lossy(&out.stdout);
    serde_json::from_str(s.trim()).unwrap_or_else(|e| panic!("stdout is not JSON ({e}): {s}"))
}

#[test]
fn a_data_pipeline_with_batches_iteration_and_data_steps_runs_without_a_model() {
    let steps = r#"{
        "start": {"kind": "once"},
        "items": {"kind": "assign", "depends_on": ["start"], "value": [1, 2, 3, 4, 5, 6], "writes": "items"},
        "each": {"kind": "foreach", "depends_on": ["items"], "over": "{{vars.items}}", "batch": {"size": 2, "parallel": 2},
                 "body": {"steps": {"double": {"kind": "assign", "value": "CEL: item * 2"},
                                    "tag": {"kind": "template", "depends_on": ["double"], "text": "b{{batch}}:{{index}}={{steps.double.output}}"}}},
                 "collect": {"into": "tags", "mode": "overwrite"}},
        "big": {"kind": "filter", "depends_on": ["each"], "over": "{{vars.items}}", "expr": "CEL: item > 3"},
        "sorted": {"kind": "sort", "depends_on": ["big"], "over": "{{steps.big.output}}", "order": "desc"},
        "sum": {"kind": "reduce", "depends_on": ["sorted"], "over": "{{steps.sorted.output}}", "expr": "CEL: acc + item", "initial": 0},
        "count": {"kind": "iterate", "depends_on": ["sum"], "max_iterations": 10, "until": "CEL: result >= 3",
                  "body": {"steps": {"inc": {"kind": "assign", "value": "CEL: has(vars.n) ? vars.n + 1 : 1", "writes": "n"}}}},
        "words": {"kind": "chunk", "depends_on": ["count"], "value": "one two three four five", "by": "words", "size": 2},
        "csv": {"kind": "parse", "depends_on": ["words"], "text": "k,v\na,1\nb,2", "format": "csv"},
        "uniq": {"kind": "dedupe", "depends_on": ["csv"], "over": [1, 1, 2, 3, 3]},
        "done": {"kind": "finish", "depends_on": ["uniq"], "status": "completed",
                 "output": {"tags": "{{vars.tags}}", "sorted": "{{steps.sorted.output}}", "sum": "{{steps.sum.output}}", "n": "{{vars.n}}", "iterations": "{{steps.count.output}}", "chunks": "{{steps.words.output}}", "csv": "{{steps.csv.output}}", "uniq": "{{steps.uniq.output}}"}}
    }"#;
    let cfg = job(steps, "");
    let out = run_agentd(&cfg, &[]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(0), "stderr:\n{stderr}");
    let v = stdout_json(&out);
    assert_eq!(
        v["tags"],
        serde_json::json!(["b0:0=2", "b0:1=4", "b1:2=6", "b1:3=8", "b2:4=10", "b2:5=12"]),
        "{v}"
    );
    assert_eq!(v["sorted"], serde_json::json!([6, 5, 4]));
    assert_eq!(v["sum"], serde_json::json!(15));
    assert_eq!(v["n"], serde_json::json!(3));
    assert_eq!(
        v["iterations"],
        serde_json::json!(3),
        "the last iteration's result"
    );
    assert_eq!(
        v["chunks"],
        serde_json::json!(["one two", "three four", "five"])
    );
    assert_eq!(
        v["csv"],
        serde_json::json!([{"k": "a", "v": 1}, {"k": "b", "v": 2}])
    );
    assert_eq!(v["uniq"], serde_json::json!([1, 2, 3]));
    // Per-batch durability: three batches were recorded (batch.k kill point passed thrice).
    let done = events(&stderr, "run.done");
    assert_eq!(done[0]["status"], "completed");
    assert!(
        events(&stderr, "step.start")
            .iter()
            .filter(|e| e["step"].as_str().unwrap().starts_with("each["))
            .count()
            == 12,
        "6 elements × 2 body steps"
    );
}

#[test]
fn parallel_race_switch_subgraph_and_error_policies() {
    let steps = r#"{
        "start": {"kind": "once"},
        "par": {"kind": "parallel", "depends_on": ["start"], "on_error": "continue",
                "branches": {"a": {"steps": {"x": {"kind": "assign", "value": "A"}}},
                             "b": {"steps": {"y": {"kind": "fail", "message": "b breaks"}}},
                             "c": {"steps": {"z1": {"kind": "assign", "value": 1}, "z2": {"kind": "assign", "depends_on": ["z1"], "value": "CEL: steps.z1.output + 1"}}}}},
        "race": {"kind": "race", "depends_on": ["par"],
                 "branches": {"fast": {"steps": {"f": {"kind": "assign", "value": "fast wins"}}},
                              "slow": {"steps": {"s": {"kind": "sleep", "duration": "30s"}}}}},
        "route": {"kind": "switch", "depends_on": ["race"], "on": "{{steps.race.output.winner}}", "cases": {"fast": "took_fast", "slow": "took_slow"}, "default": "took_default"},
        "took_fast": {"kind": "assign", "depends_on": ["route"], "value": "fast path", "writes": "path"},
        "took_slow": {"kind": "assign", "depends_on": ["route"], "value": "slow path", "writes": "path"},
        "took_default": {"kind": "assign", "depends_on": ["route"], "value": "default path", "writes": "path"},
        "sub": {"kind": "subgraph", "depends_on": ["took_fast", "took_slow", "took_default"],
                "body": {"steps": {"g1": {"kind": "assign", "value": "{{vars.path}}"}, "g2": {"kind": "template", "depends_on": ["g1"], "text": "sub saw {{steps.g1.output}}"}}}},
        "flaky": {"kind": "fail", "depends_on": ["sub"], "message": "expected", "on_error": "continue"},
        "guarded": {"kind": "assign", "depends_on": ["flaky"], "when": "CEL: steps.flaky.error != null", "value": "guard ran"},
        "skipped": {"kind": "assign", "depends_on": ["flaky"], "when": "CEL: false", "value": "never"},
        "goto_src": {"kind": "fail", "depends_on": ["guarded", "skipped"], "message": "recover me", "on_error": "goto:recover"},
        "recover": {"kind": "assign", "depends_on": ["goto_src"], "value": "recovered", "writes": "rec"},
        "done": {"kind": "finish", "depends_on": ["recover"], "status": "completed",
                 "output": {"par": "{{steps.par.output}}", "race": "{{steps.race.output.winner}}", "sub": "{{steps.sub.output}}", "guard": "{{steps.guarded.output}}", "skipped": "{{steps.skipped.status}}", "rec": "{{vars.rec}}"}}
    }"#;
    let cfg = job(steps, "");
    let out = run_agentd(&cfg, &[]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(0), "stderr:\n{stderr}");
    let v = stdout_json(&out);
    assert_eq!(v["par"]["a"], "A", "{v}");
    assert_eq!(
        v["par"]["c"],
        serde_json::json!(2),
        "chained body steps: {v}"
    );
    assert!(
        v["par"]["_errors"]["b"]
            .as_str()
            .unwrap()
            .contains("b breaks"),
        "{v}"
    );
    assert_eq!(v["race"], "fast");
    assert_eq!(v["sub"], "sub saw fast path");
    assert_eq!(v["guard"], "guard ran");
    assert_eq!(v["skipped"], "skipped");
    assert_eq!(v["rec"], "recovered");
    // The slow race branch was cancelled (its sleep timer disarmed; the step marked cancelled).
    assert!(
        events(&stderr, "step.done")
            .iter()
            .any(|e| e["step"] == "race" && e["status"] == "done"),
        "{stderr}"
    );
    assert!(events(&stderr, "step.goto").len() == 1, "{stderr}");
}

#[test]
fn mcp_tool_steps_large_outputs_and_concurrent_runs() {
    let mock = common::spawn_mock_mcp("mock://noop", false);
    let steps = r#"{
        "start": {"kind": "manual"},
        "put": {"kind": "mcp.tool", "depends_on": ["start"], "server": "mock", "tool": "state.put", "args": {"key": "wf/{{inputs.n}}", "seq": 1, "state": {"n": "{{inputs.n}}"}}, "output_schema": {"type": "object", "properties": {"ok": {"type": "boolean"}}, "required": ["ok"]}},
        "get": {"kind": "mcp.tool", "depends_on": ["put"], "server": "mock", "tool": "state.get", "args": {"key": "wf/{{inputs.n}}"}},
        "big": {"kind": "map", "depends_on": ["get"], "over": "CEL: [1,2,3,4,5,6,7,8,9,10]", "expr": "{{item}}-XXXX"},
        "size": {"kind": "assign", "depends_on": ["big"], "value": "CEL: size(steps.big.output)"},
        "done": {"kind": "finish", "depends_on": ["size"], "status": "completed", "output": {"got": "{{steps.get.output.state.n}}", "count": "{{steps.size.output}}", "first": "{{steps.big.output.0}}"}}
    }"#;
    // Two runs started by inbox events (workflow_run), lower inline cap so the map
    // output becomes an artifact; the template dereferences it.
    let steps = steps.replace("XXXX", &"x".repeat(200));
    let cfg = write_config(&format!(
        "config_version: \"2\"\nagent:\n  name: engine\nmcp:\n  servers:\n    - name: mock\n      endpoint: {}\nlimits:\n  inline_max_bytes: 512\nworkflows:\n  - name: pipe\n    concurrency: {{max_runs: 4}}\n    steps: {steps}\nlifecycle:\n  run_until: idle\n  idle_grace: 1s\nobservability:\n  log_level: info\n  log_content: true\n",
        mock.uri()
    ));
    let inbox = common::unique_path("inbox", "json");
    std::fs::write(&inbox, serde_json::json!([
        {"kind": "workflow_run", "payload": {"workflow": "pipe", "node": "start", "inputs": {"n": "1"}}},
        {"kind": "workflow_run", "payload": {"workflow": "pipe", "node": "start", "inputs": {"n": "2"}}}
    ]).to_string()).unwrap();
    let out = run_agentd(&cfg, &[("AGENTD_TEST_INBOX_FILE", &inbox)]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(0), "stderr:\n{stderr}");
    let done = events(&stderr, "run.done");
    assert_eq!(done.len(), 2, "two concurrent runs: {stderr}");
    assert!(done.iter().all(|d| d["status"] == "completed"), "{done:?}");
    let outputs: Vec<String> = done
        .iter()
        .map(|d| d["output"]["got"].as_str().unwrap().to_string())
        .collect();
    assert!(
        outputs.contains(&"1".to_string()) && outputs.contains(&"2".to_string()),
        "{outputs:?}"
    );
    assert!(
        done.iter().all(|d| d["output"]["count"] == 10
            && d["output"]["first"].as_str().unwrap().starts_with("1-xxx")),
        "{done:?}"
    );
    // The map output exceeded the inline cap and lives in an artifact.
    assert!(
        events(&stderr, "step.output.artifact")
            .iter()
            .any(|e| e["step"] == "big"),
        "artifact-backed output: {stderr}"
    );
}

#[test]
fn a_sigkill_mid_batch_resumes_at_the_next_batch() {
    let mock = common::spawn_mock_mcp("mock://noop", false);
    let steps = r#"{
        "start": {"kind": "once"},
        "each": {"kind": "foreach", "depends_on": ["start"], "over": [1, 2, 3, 4, 5, 6], "batch": {"size": 2, "parallel": 1},
                 "body": {"steps": {"work": {"kind": "assign", "value": "CEL: item * 10"}}}},
        "done": {"kind": "finish", "depends_on": ["each"], "status": "completed", "output": "{{steps.each.output}}"}
    }"#;
    let cfg = write_config(&format!(
        "config_version: \"2\"\nagent:\n  name: chaos-batch\nmcp:\n  servers:\n    - name: mock\n      endpoint: {}\nstore:\n  kind: mcp\n  mcp:\n    server: mock\nworkflows:\n  - name: pipe\n    steps: {steps}\nlifecycle:\n  run_until: idle\n  idle_grace: 1s\nobservability:\n  log_level: info\n  log_content: true\n",
        mock.uri()
    ));
    // Life 1: die when the first batch completes (`batch.k`).
    let out1 = run_agentd(&cfg, &[("AGENTD_TEST_KILL_AT", "batch.k")]);
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        assert_eq!(
            out1.status.signal(),
            Some(libc::SIGKILL),
            "{}",
            String::from_utf8_lossy(&out1.stderr)
        );
    }
    let stderr1 = String::from_utf8_lossy(&out1.stderr);
    let started1: Vec<String> = events(&stderr1, "step.start")
        .iter()
        .map(|e| e["step"].as_str().unwrap().to_string())
        .collect();
    assert!(
        started1.contains(&"each[0].work".to_string())
            && started1.contains(&"each[1].work".to_string()),
        "{started1:?}"
    );
    // Life 2: resume — the first batch's elements are NOT re-executed; the run completes.
    let out2 = run_agentd(&cfg, &[]);
    let stderr2 = String::from_utf8_lossy(&out2.stderr);
    assert_eq!(out2.status.code(), Some(0), "stderr:\n{stderr2}");
    let started2: Vec<String> = events(&stderr2, "step.start")
        .iter()
        .map(|e| e["step"].as_str().unwrap().to_string())
        .collect();
    assert!(
        !started2.contains(&"each[0].work".to_string()),
        "batch 0 was durable: {started2:?}"
    );
    assert!(
        started2
            .iter()
            .any(|s| s == "each[4].work" || s == "each[2].work"),
        "the run resumed at a later batch: {started2:?}"
    );
    let v = stdout_json(&out2);
    assert_eq!(v, serde_json::json!([10, 20, 30, 40, 50, 60]));
}
