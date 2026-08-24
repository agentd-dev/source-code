// SPDX-License-Identifier: AGPL-3.0-only
//! agentd runtime end to end: a configuration drives the event loop — the
//! `--instruction` sugar workflow (`once → agent → finish`) runs a turn worker
//! against the built-in mock LLM, internal tools round-trip to the supervisor,
//! tool overrides map onto the mock MCP server, and a SIGKILLed instance
//! restores its durable state from the mock MCP store and finishes the job on
//! restart.

mod common;

use std::io::Write;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Launch the mock LLM with a `file:` playbook; returns `(guard, http uri)`.
struct MockLlm {
    child: std::process::Child,
    addr_file: String,
    uri: String,
}
impl Drop for MockLlm {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_file(&self.addr_file);
    }
}

fn spawn_mock_llm(playbook: &serde_json::Value) -> MockLlm {
    let pb = common::unique_path("playbook", "json");
    std::fs::write(&pb, playbook.to_string()).unwrap();
    let addr_file = common::unique_path("mock-llm", "addr");
    let _ = std::fs::remove_file(&addr_file);
    let child = Command::new(env!("CARGO_BIN_EXE_agentd"))
        .args(["--internal-mock-llm", &addr_file, &format!("file:{pb}")])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn mock llm");
    let addr = common::read_addr_file(&addr_file);
    MockLlm {
        child,
        addr_file,
        uri: format!("http://{addr}"),
    }
}

fn write_config(yaml: &str) -> String {
    let path = common::unique_path("agentd-v2", "yaml");
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

#[test]
fn the_instruction_job_runs_a_turn_with_tool_round_trips_through_the_new_loop() {
    // Turn 0: memory.set (internal ⇒ round-trips to the supervisor) + fs-less
    // second call to an unknown tool (an error result the model sees);
    // turn 1 (after 2 tool results): plan.create; turn 2: the final answer.
    let llm = spawn_mock_llm(&serde_json::json!({"turns": [
        {"tool_calls": [{"name": "memory.set", "arguments": {"key": "greeting", "value": "hello"}}, {"name": "no.such.tool", "arguments": {}}]},
        {"content": "unused"},
        {"tool_calls": [{"name": "plan.create", "arguments": {"goal": "greet", "items": ["say hello"]}}]},
        {"content": "final answer: greeted"}
    ]}));
    let cfg = write_config(&format!(
        "config_version: \"1\"\nagent:\n  instruction: greet the user\nintelligence:\n  endpoints: {}\n  model: mock\nobservability:\n  log_level: info\n",
        llm.uri
    ));
    let out = run_agentd(&cfg, &[]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert_eq!(out.status.code(), Some(0), "stderr:\n{stderr}");
    assert!(
        stdout.contains("final answer: greeted"),
        "the job prints its result: {stdout}"
    );
    // The runtime ran the sugar workflow: once → agent step → finish.
    assert_eq!(events(&stderr, "run.start").len(), 1, "{stderr}");
    let done = events(&stderr, "run.done");
    assert_eq!(done.len(), 1);
    assert_eq!(done[0]["status"], "completed");
    // Internal tools round-tripped: memory.set + plan.create requests; the
    // unknown tool was answered as an error by the child itself.
    let reqs: Vec<String> = events(&stderr, "tool.request")
        .iter()
        .map(|e| e["tool"].as_str().unwrap().to_string())
        .collect();
    assert!(reqs.contains(&"memory.set".to_string()), "{reqs:?}");
    assert!(reqs.contains(&"plan.create".to_string()), "{reqs:?}");
    assert!(
        events(&stderr, "plan.updated")
            .iter()
            .any(|e| e["op"] == "create")
    );
    assert!(events(&stderr, "proc.exit").iter().any(|e| e["code"] == 0));
    // No secret-shaped content, and the event loop drove the job: no per-mode
    // driver line in the log.
    assert!(
        !stderr.contains("\"mode\":\"once\""),
        "the 2.0 runtime, not the 1.x once driver"
    );
}

#[test]
fn tool_overrides_map_an_internal_contract_onto_the_mock_mcp_server() {
    // The mock MCP server's `state.get` implements `memory.get` via an override;
    // the value is planted through the store profile first.
    let mock = common::spawn_mock_mcp("mock://noop", false);
    let llm = spawn_mock_llm(&serde_json::json!({"turns": [
        {"tool_calls": [{"name": "memory.get", "arguments": {"key": "planted"}}]},
        {"content": "the value is {{tool}}"}
    ]}));
    // Plant a record the override will read (state.get {key} → {state}).
    let client = {
        let mut c = agentd::mcp::client::McpClient::connect(
            "m",
            &mock.uri(),
            vec![],
            Duration::from_secs(5),
        )
        .unwrap();
        c.initialize().unwrap();
        c
    };
    client
        .call_tool(
            "state.put",
            Some(serde_json::json!({"key": "planted", "seq": 1, "state": {"answer": 42}})),
        )
        .unwrap();
    let cfg = write_config(&format!(
        "config_version: \"1\"\nagent:\n  instruction: read memory\nintelligence:\n  endpoints: {}\n  model: mock\nmcp:\n  servers:\n    - name: mock\n      endpoint: {}\ntools:\n  overrides:\n    memory.get:\n      server: mock\n      tool: state.get\n      args: '{{\"key\": \"{{{{args.key}}}}\"}}'\n      result: '{{\"found\": true, \"key\": \"{{{{args.key}}}}\", \"value\": {{{{result.structuredContent.state}}}}}}'\nobservability:\n  log_level: info\n  log_content: true\n",
        llm.uri,
        mock.uri()
    ));
    let out = run_agentd(&cfg, &[]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(0), "stderr:\n{stderr}");
    // The request went to the runtime, which executed the mapped tool on the mock.
    assert!(
        events(&stderr, "tool.request")
            .iter()
            .any(|e| e["tool"] == "memory.get"),
        "{stderr}"
    );
    let results = events(&stderr, "tool.result");
    let mg = results
        .iter()
        .find(|e| e["tool"] == "memory.get")
        .expect("memory.get result in the worker log");
    assert_eq!(mg["is_error"], false, "{mg}");
    // The mock saw a state.get for the planted key.
    let ops = client
        .call_tool("mock.ops", Some(serde_json::json!({})))
        .unwrap()
        .text();
    assert!(ops.contains("state.get"), "{ops}");
}

#[test]
fn a_sigkilled_instance_restores_from_the_store_and_finishes_the_job() {
    let mock = common::spawn_mock_mcp("mock://noop", false);
    let llm = spawn_mock_llm(&serde_json::json!({"turns": [{"content": "done after restart"}]}));
    let cfg = write_config(&format!(
        "config_version: \"1\"\nagent:\n  name: chaos\n  instruction: finish the job\nintelligence:\n  endpoints: {}\n  model: mock\nmcp:\n  servers:\n    - name: mock\n      endpoint: {}\nstore:\n  kind: mcp\n  mcp:\n    server: mock\nobservability:\n  log_level: info\n",
        llm.uri,
        mock.uri()
    ));
    // Life 1: die right after the start event is durable (before the run).
    let out1 = run_agentd(&cfg, &[("AGENTD_TEST_KILL_AT", "inbox.after_put")]);
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        assert_eq!(
            out1.status.signal(),
            Some(libc::SIGKILL),
            "life 1 must die at the kill point: {:?}",
            String::from_utf8_lossy(&out1.stderr)
        );
    }
    // Life 2: die while the agent step is running (its `running` record is durable).
    let out2 = run_agentd(&cfg, &[("AGENTD_TEST_KILL_AT", "step.running")]);
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        assert_eq!(
            out2.status.signal(),
            Some(libc::SIGKILL),
            "life 2 must die at the kill point"
        );
    }
    let stderr2 = String::from_utf8_lossy(&out2.stderr);
    assert!(
        events(&stderr2, "restore.done").len() == 1,
        "life 2 restored the pending inbox event: {stderr2}"
    );
    assert_eq!(
        events(&stderr2, "run.start").len(),
        1,
        "the replayed event started the run: {stderr2}"
    );
    // Life 3: restore the run, replay the step, finish.
    let out3 = run_agentd(&cfg, &[]);
    let stderr3 = String::from_utf8_lossy(&out3.stderr);
    let stdout3 = String::from_utf8_lossy(&out3.stdout);
    assert_eq!(out3.status.code(), Some(0), "stderr:\n{stderr3}");
    assert!(events(&stderr3, "restore.done").len() == 1, "{stderr3}");
    assert!(
        events(&stderr3, "run.start").is_empty(),
        "the once start does not fire again for a restored run: {stderr3}"
    );
    let done = events(&stderr3, "run.done");
    assert_eq!(done.len(), 1, "{stderr3}");
    assert_eq!(done[0]["status"], "completed");
    assert!(stdout3.contains("done after restart"), "{stdout3}");
    // Life 4: nothing left to do — the ensured `once` run is complete; exit 0 quickly.
    let t = Instant::now();
    let out4 = run_agentd(&cfg, &[]);
    assert_eq!(out4.status.code(), Some(0));
    assert!(t.elapsed() < Duration::from_secs(20));
    let stderr4 = String::from_utf8_lossy(&out4.stderr);
    assert!(events(&stderr4, "run.start").is_empty(), "{stderr4}");
    assert!(
        events(&stderr4, "start.once.skipped").len() == 1,
        "{stderr4}"
    );
}

fn write_inbox(events: &serde_json::Value) -> String {
    let path = common::unique_path("inbox", "json");
    std::fs::write(&path, events.to_string()).unwrap();
    path
}

/// A document with a `manual` workflow (so no `--instruction` sugar fires) and
/// `run_until: idle` — the harness for driving conversation turns from a
/// seeded inbox.
fn conversation_config(llm: &str, mock: &str, extra: &str) -> String {
    write_config(&format!(
        "config_version: \"1\"\nagent:\n  name: convo\n  instruction: You help the team.\n  preflight: always\nintelligence:\n  endpoints: {llm}\n  model: mock\nmcp:\n  servers:\n    - name: mock\n      endpoint: {mock}\nknowledge:\n  server: mock\n  auto_context:\n    on: turn\n    top_k: 3\nskills:\n  sources:\n    - server: mock\n      discover: auto\nworkflows:\n  - name: idle\n    steps:\n      s: {{kind: manual}}\n      f: {{kind: finish, depends_on: [s]}}\nlifecycle:\n  run_until: idle\n  idle_grace: 1s\nobservability:\n  log_level: info\n  log_content: true\n{extra}"
    ))
}

#[test]
fn a_conversation_turn_runs_preflight_skills_and_knowledge_context() {
    let mock = common::spawn_mock_mcp("mock://noop", false);
    let llm = spawn_mock_llm(&serde_json::json!({
        "turns": [{"content": "plain answer"}],
        "match": [
            {"when_contains": "PREFLIGHT", "content": {"intent": "task", "needs_plan": true, "plan": [{"title": "read the policy"}, {"title": "review"}], "risk": "low", "skills": ["deploy-safely"]}},
            {"when_contains": "Retrieved knowledge", "content": "answer with knowledge + skills"}
        ]
    }));
    let cfg = conversation_config(&llm.uri, &mock.uri(), "");
    let inbox = write_inbox(&serde_json::json!([
        {"kind": "a2a_message", "principal": "user:alice", "payload": {"context_id": "c1", "text": "Please review the deployment policy for the canary. @skill:review-pr"}}
    ]));
    let out = run_agentd(&cfg, &[("AGENTD_TEST_INBOX_FILE", &inbox)]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(0), "stderr:\n{stderr}");
    // Skills were discovered from prompts + resources.
    let disc = events(&stderr, "skills.discovered");
    assert!(
        disc.iter().any(|e| e["skills"]
            .as_array()
            .unwrap()
            .iter()
            .any(|s| s == "review-pr")
            && e["skills"]
                .as_array()
                .unwrap()
                .iter()
                .any(|s| s == "incident-runbook")),
        "{stderr}"
    );
    // Preflight ran and its verdict was recorded; it named a skill to preload.
    let verdict = events(&stderr, "preflight.verdict");
    assert_eq!(verdict.len(), 1, "{stderr}");
    assert_eq!(verdict[0]["intent"], "task");
    let loaded: Vec<String> = events(&stderr, "skill.loaded")
        .iter()
        .map(|e| e["skill"].as_str().unwrap().to_string())
        .collect();
    assert!(
        loaded.contains(&"review-pr".to_string()),
        "the @skill reference preloaded: {loaded:?}"
    );
    assert!(
        loaded.contains(&"deploy-safely".to_string()),
        "the preflight's skill preloaded: {loaded:?}"
    );
    // Knowledge auto-context found the deployment policy and the model saw it.
    let k = events(&stderr, "knowledge.auto_context");
    assert_eq!(k.len(), 1, "{stderr}");
    assert_eq!(k[0]["hit"], true);
    let reply = events(&stderr, "turn.reply");
    assert!(
        reply
            .iter()
            .any(|r| r["text"] == "answer with knowledge + skills"),
        "{stderr}"
    );
    // The plan was seeded and the conversation context is durable + idle-exited.
    assert!(
        events(&stderr, "plan.updated")
            .iter()
            .any(|e| e["op"] == "preflight" && e["progress"] == "0/2 done"),
        "plan seeded: {stderr}"
    );
    assert!(
        events(&stderr, "lifecycle.idle_exit").len() == 1,
        "{stderr}"
    );
}

#[test]
fn a_status_intent_is_answered_deterministically_and_the_root_can_delegate() {
    let mock = common::spawn_mock_mcp("mock://noop", false);
    let llm = spawn_mock_llm(&serde_json::json!({
        "turns": [
            {"tool_calls": [{"name": "subagent.run", "arguments": {"instruction": "count to three", "mode": "sync"}}]},
            {"content": "delegated and done"}
        ],
        "match": [
            {"when_contains": "PREFLIGHT", "content": {"intent": "status", "needs_plan": false, "risk": "low"}, "delay_ms": 0},
            {"when_contains": "You are agentd, an autonomous agent.", "content": "three"}
        ]
    }));
    // First message: preflight says `status` → deterministic reply, no model turn.
    let cfg = conversation_config(&llm.uri, &mock.uri(), "");
    let inbox = write_inbox(&serde_json::json!([
        {"kind": "a2a_message", "principal": "user:bob", "payload": {"context_id": "c2", "text": "status?"}}
    ]));
    let out = run_agentd(&cfg, &[("AGENTD_TEST_INBOX_FILE", &inbox)]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(0), "stderr:\n{stderr}");
    let reply = events(&stderr, "turn.reply");
    assert!(
        reply
            .iter()
            .any(|r| r["deterministic"] == true
                && r["text"].as_str().unwrap().starts_with("Status:")),
        "{stderr}"
    );
    assert!(
        events(&stderr, "turn.spawn").is_empty(),
        "no model turn for a status intent: {stderr}"
    );
    // Second run: preflight off → the root delegates to a sync subagent.
    let cfg2 = write_config(
        &std::fs::read_to_string(&cfg)
            .unwrap()
            .replace("preflight: always", "preflight: never"),
    );
    let inbox2 = write_inbox(&serde_json::json!([
        {"kind": "a2a_message", "principal": "user:bob", "payload": {"context_id": "c3", "text": "count for me"}}
    ]));
    let out = run_agentd(&cfg2, &[("AGENTD_TEST_INBOX_FILE", &inbox2)]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(0), "stderr:\n{stderr}");
    assert!(events(&stderr, "subagent.spawn").len() == 1, "{stderr}");
    let res = events(&stderr, "subagent.result");
    assert_eq!(res.len(), 1, "{stderr}");
    assert_eq!(res[0]["status"], "completed");
    assert!(
        events(&stderr, "turn.reply")
            .iter()
            .any(|r| r["text"] == "delegated and done"),
        "{stderr}"
    );
}

#[test]
fn a_fail_budget_tactic_fails_the_job_with_the_budget_exit_code() {
    let llm = spawn_mock_llm(&serde_json::json!({"turns": [{"content": "never admitted"}]}));
    let cfg = write_config(&format!(
        "config_version: \"1\"\nagent:\n  instruction: do work\nintelligence:\n  endpoints: {}\n  model: mock\n  budget:\n    windows:\n      - per: hour\n        tokens: 10\n    on_exhausted: fail\nobservability:\n  log_level: info\n",
        llm.uri
    ));
    let out = run_agentd(&cfg, &[]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(
        out.status.code(),
        Some(7),
        "budget exit code; stderr:\n{stderr}"
    );
    let done = events(&stderr, "run.done");
    assert_eq!(done.len(), 1);
    assert_eq!(done[0]["status"], "failed");
    assert!(
        done[0]["err"].as_str().unwrap().contains("budget"),
        "{}",
        done[0]
    );
    assert!(
        events(&stderr, "turn.spawn").is_empty() && events(&stderr, "step.turn.spawn").is_empty(),
        "no worker was admitted: {stderr}"
    );
}

#[test]
fn a_long_conversation_compacts_its_context_and_restores_the_summary() {
    // Three messages on one conversation, a tiny model window: after the
    // turns push the estimate past compact_at × window, a compaction think
    // folds the older messages into the summary block (a fresh instance
    // restores it).
    let mock = common::spawn_mock_mcp("mock://noop", false);
    let llm = spawn_mock_llm(&serde_json::json!({
        "turns": [{"content": "a reasonably long answer that adds a fair number of tokens to the transcript of this conversation, so the estimate grows quickly across turns and crosses the threshold"}],
        "match": [
            {"when_contains": "compact an agent's conversation memory", "content": {"goals": ["help the team"], "decisions": ["compacted"], "open": [], "facts": ["three messages arrived"]}}
        ]
    }));
    let cfg = write_config(&format!(
        "config_version: \"1\"\nagent:\n  name: compact\n  instruction: You help the team.\n  preflight: never\nintelligence:\n  endpoints: {}\n  model: mock\nmcp:\n  servers:\n    - name: mock\n      endpoint: {}\nstore:\n  kind: mcp\n  mcp:\n    server: mock\ncontext:\n  model_window: 300\n  compact_at: 0.5\n  keep_last: 2\nworkflows:\n  - name: idle\n    steps:\n      s: {{kind: manual}}\n      f: {{kind: finish, depends_on: [s]}}\nlifecycle:\n  run_until: idle\n  idle_grace: 1s\nobservability:\n  log_level: info\n",
        llm.uri,
        mock.uri()
    ));
    let inbox = write_inbox(&serde_json::json!([
        {"kind": "a2a_message", "principal": "user:eve", "payload": {"context_id": "long", "text": "first: tell me about the deployment process in some detail please"}},
        {"kind": "a2a_message", "principal": "user:eve", "payload": {"context_id": "long", "text": "second: and what about incidents, how do we handle them end to end"}},
        {"kind": "a2a_message", "principal": "user:eve", "payload": {"context_id": "long", "text": "third: summarize everything you told me so far in one paragraph"}}
    ]));
    let out = run_agentd(&cfg, &[("AGENTD_TEST_INBOX_FILE", &inbox)]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(0), "stderr:\n{stderr}");
    assert_eq!(
        events(&stderr, "turn.spawn").len(),
        3,
        "three serialized turns on one context: {stderr}"
    );
    let compacted = events(&stderr, "context.compacted");
    assert!(!compacted.is_empty(), "a compaction happened: {stderr}");
    assert!(
        compacted.iter().all(|c| c["fallback"] != true),
        "the summarizer think produced the summary: {compacted:?}"
    );
    assert!(compacted[0]["after"].as_u64().unwrap() < compacted[0]["before"].as_u64().unwrap());
    // A fresh life restores the compacted context (summary + version) from the store.
    let out2 = run_agentd(&cfg, &[]);
    let stderr2 = String::from_utf8_lossy(&out2.stderr);
    assert_eq!(out2.status.code(), Some(0), "stderr:\n{stderr2}");
    let adopted = events(&stderr2, "restore.adopted");
    assert_eq!(adopted.len(), 1, "{stderr2}");
    assert!(
        adopted[0]["contexts"].as_u64().unwrap() >= 1,
        "{}",
        adopted[0]
    );
}
