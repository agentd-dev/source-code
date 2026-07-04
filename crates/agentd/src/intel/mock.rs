// SPDX-License-Identifier: Apache-2.0
//! A minimal built-in mock LLM for the observe-to-validate E2E suite (M7).
//! Hidden mode: `agentd --internal-mock-llm <addr-file> [script]`.
//!
//! Binds a **loopback TCP** listener on `127.0.0.1:0` and writes the bound
//! `host:port` into `<addr-file>` (atomically: tmp + rename) so the launching
//! harness discovers the endpoint by waiting for the file — the same
//! wait-for-path handshake the old unix-socket form had, except the file now
//! *carries* the address instead of being the socket. The harness then hands
//! agentd `--intelligence http://<addr>` (loopback plaintext is the dev/test
//! carve-out; production intelligence is HTTPS-only).
//!
//! Speaks just enough OpenAI-compatible `/chat/completions` over that listener
//! to drive a *real* agentic loop without a live model: it reads the request and
//! returns a scripted assistant turn — a final answer or a tool call — switching
//! to a final answer once a tool result appears in the transcript (so the ReAct
//! cycle closes). Scripts: `final` (answer at once), `read` (call `resource.read`
//! then answer), `schedule` (call the `schedule` self-tool then answer),
//! `subscribe` (call the `subscribe` self-tool then answer), `spawn-churn`
//! (call `subagent.spawn` on *every* turn — never converging — so a run issues
//! a rapid burst of spawns that trips the spawn-rate limiter, RFC 0009 §3.6);
//! `slow`/`hang` hold the response to exercise the stuck/deadline detectors.
//! Small enough to ship; it makes the loop + self-* tools observable end to end.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};

/// Serve the mock LLM until the process is killed, announcing the bound
/// loopback address through `addr_file`. Returns the exit code.
pub fn run(addr_file: &str, script: &str) -> i32 {
    let listener = match TcpListener::bind("127.0.0.1:0") {
        Ok(l) => l,
        Err(e) => {
            eprintln!("mock-llm: bind 127.0.0.1:0: {e}");
            return crate::exit::GENERIC;
        }
    };
    if let Err(e) = crate::announce_addr(addr_file, &listener) {
        eprintln!("mock-llm: write {addr_file}: {e}");
        return crate::exit::GENERIC;
    }
    // One request per connection (the intel client uses Connection: close) —
    // handled on its OWN thread, so a `slow`/`hang` script sleeps only its own
    // request. A sequential accept loop serialized every caller behind the
    // slowest in-flight one, which coupled concurrent tests' timing (the warm-
    // session flake) and would make two runs sharing one mock queue behind a
    // 12s hang.
    for stream in listener.incoming().flatten() {
        let script = script.to_string();
        std::thread::spawn(move || handle(stream, &script));
    }
    0
}

fn handle(mut stream: TcpStream, script: &str) {
    let Some(body) = read_request_body(&mut stream) else {
        return;
    };
    // A `role:tool` message means the model already called a tool, so the next
    // turn is a final answer. The `gate` script needs the COUNT (define → run →
    // final is a three-phase conversation).
    let tool_results =
        body.matches("\"role\":\"tool\"").count() + body.matches("\"role\": \"tool\"").count();
    let saw_tool_result = tool_results > 0;
    // `slow`/`hang`: hold the response so the calling subagent stays alive in the
    // model call — `slow` (5s) lets the chaos suite catch a live subagent before
    // collapsing the tree; `hang` (long) keeps a run alive so a cancel/drain test
    // proves it was the teardown (not natural completion) that ended it.
    let script = match script {
        "slow" => {
            std::thread::sleep(std::time::Duration::from_secs(5));
            "final"
        }
        "hang" => {
            // Long enough to outlive a cancel/drain (kill ladder ~7s) so a test
            // proves the teardown ended it — but bounded so a *leaked* (uncancelled)
            // hang run can't pin the process-wide supervise lock for long.
            std::thread::sleep(std::time::Duration::from_secs(12));
            "final"
        }
        other => other,
    };
    // RFC 0021 §7 e2e: the model AUTHORS a workflow with a `human` gate, runs
    // it (the run blocks while the gate awaits the A2A reply), then answers.
    let payload = if script == "gate" {
        match tool_results {
            0 => tool_call(
                "workflow.define",
                r#"{"workflow":{"start":"gate","nodes":{
                    "gate":{"kind":"human","payload":{"question":"approve the deploy?"},
                            "timeout_ms":30000,"writes":"verdict",
                            "edges":{"replied":"done","timeout":"esc"}},
                    "done":{"kind":"halt","status":"completed","result_from":"verdict"},
                    "esc":{"kind":"halt","status":"refused"}}}}"#,
            ),
            1 => tool_call("workflow.run", r#"{"workflow_id":"w1"}"#),
            _ => final_answer("gate flow complete"),
        }
    } else {
        response_json(script, saw_tool_result)
    };
    let resp = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        payload.len(),
        payload
    );
    let _ = stream.write_all(resp.as_bytes());
    let _ = stream.flush();
}

/// Read an HTTP/1.1 request: headers up to the blank line, then `Content-Length`
/// body bytes. Returns the request body (the chat-completions JSON).
fn read_request_body(stream: &mut TcpStream) -> Option<String> {
    let mut reader = BufReader::new(stream);
    let mut content_length = 0usize;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).ok()? == 0 {
            return None; // EOF mid-headers
        }
        let t = line.trim_end();
        if t.is_empty() {
            break; // end of headers
        }
        if let Some(v) = t
            .strip_prefix("Content-Length:")
            .or_else(|| t.strip_prefix("content-length:"))
        {
            content_length = v.trim().parse().unwrap_or(0);
        }
    }
    let mut body = vec![0u8; content_length];
    reader.read_exact(&mut body).ok()?;
    Some(String::from_utf8_lossy(&body).into_owned())
}

/// The scripted assistant turn as an OpenAI chat-completion body.
fn response_json(script: &str, saw_tool_result: bool) -> String {
    match (script, saw_tool_result) {
        ("read", false) => tool_call("resource.read", r#"{"uri":"file:///in.json"}"#),
        ("read", true) => final_answer("read complete"),
        ("schedule", false) => tool_call(
            "schedule",
            r#"{"after_seconds":1,"instruction":"follow up"}"#,
        ),
        ("schedule", true) => final_answer("scheduled a follow-up"),
        ("subscribe", false) => tool_call("subscribe", r#"{"uri":"file:///watch.json"}"#),
        ("subscribe", true) => final_answer("now watching the resource"),
        // Delegate an objective to a declared remote A2A peer named "peer"
        // (RFC 0020 §3), then — once the distillate comes back as a tool result —
        // answer. Drives the agentd-as-A2A-client path end to end.
        ("a2a-delegate", false) => tool_call(
            "a2a.delegate",
            r#"{"peer":"peer","objective":"summarize the mesh","output_contract":"one line"}"#,
        ),
        ("a2a-delegate", true) => final_answer("delegated over a2a"),
        // Unlike read/schedule (which answer once a tool result is seen),
        // spawn-churn ignores `saw_tool_result` and keeps emitting a
        // `subagent.spawn` call every turn — so an in-loop run fires many rapid,
        // detached spawns and exercises the spawn-rate limiter end to end. detach
        // keeps each accepted spawn fire-and-forget so the burst stays rapid.
        ("spawn-churn", _) => tool_call(
            "subagent.spawn",
            r#"{"instruction":"do a trivial subtask","detach":true}"#,
        ),
        // A structured JSON answer for the workflow `infer` node tests: the exec
        // parses + schema-checks this object.
        ("json", _) => final_answer(r#"{"verdict":"approve","score":9}"#),
        _ => final_answer("mock-llm done"),
    }
}

fn final_answer(text: &str) -> String {
    serde_json::json!({
        "choices": [{"message": {"role": "assistant", "content": text}, "finish_reason": "stop"}],
        "usage": {"prompt_tokens": 11, "completion_tokens": 5}
    })
    .to_string()
}

fn tool_call(name: &str, args: &str) -> String {
    serde_json::json!({
        "choices": [{
            "message": {
                "role": "assistant",
                "content": serde_json::Value::Null,
                "tool_calls": [{"id": "call_1", "type": "function", "function": {"name": name, "arguments": args}}]
            },
            "finish_reason": "tool_calls"
        }],
        "usage": {"prompt_tokens": 11, "completion_tokens": 7}
    })
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::intel::openai;

    #[test]
    fn final_script_parses_to_a_completed_answer() {
        let resp = openai::parse_response(response_json("final", false).as_bytes()).unwrap();
        assert_eq!(resp.text.as_deref(), Some("mock-llm done"));
        assert!(resp.tool_calls.is_empty());
    }

    #[test]
    fn read_script_calls_then_answers() {
        // turn 1: a resource.read tool call
        let turn1 = openai::parse_response(response_json("read", false).as_bytes()).unwrap();
        assert!(turn1.wants_tools());
        assert_eq!(turn1.tool_calls[0].name, "resource.read");
        assert_eq!(turn1.tool_calls[0].arguments["uri"], "file:///in.json");
        // turn 2 (a tool result was seen): the final answer
        let turn2 = openai::parse_response(response_json("read", true).as_bytes()).unwrap();
        assert!(!turn2.wants_tools());
        assert_eq!(turn2.text.as_deref(), Some("read complete"));
    }

    #[test]
    fn schedule_script_calls_the_schedule_tool() {
        let turn1 = openai::parse_response(response_json("schedule", false).as_bytes()).unwrap();
        assert_eq!(turn1.tool_calls[0].name, "schedule");
        assert_eq!(turn1.tool_calls[0].arguments["after_seconds"], 1);
    }

    #[test]
    fn spawn_churn_never_converges() {
        // Every turn — whether or not a tool result was seen — is another
        // subagent.spawn with a valid (non-empty) instruction, so the run keeps
        // hammering the chokepoint instead of answering.
        for saw_tool in [false, true] {
            let turn =
                openai::parse_response(response_json("spawn-churn", saw_tool).as_bytes()).unwrap();
            assert!(turn.wants_tools(), "spawn-churn must keep calling tools");
            assert_eq!(turn.tool_calls[0].name, "subagent.spawn");
            assert_eq!(
                turn.tool_calls[0].arguments["instruction"],
                "do a trivial subtask"
            );
        }
    }

    #[test]
    fn subscribe_script_calls_the_subscribe_tool() {
        let turn1 = openai::parse_response(response_json("subscribe", false).as_bytes()).unwrap();
        assert_eq!(turn1.tool_calls[0].name, "subscribe");
        assert_eq!(turn1.tool_calls[0].arguments["uri"], "file:///watch.json");
        let turn2 = openai::parse_response(response_json("subscribe", true).as_bytes()).unwrap();
        assert!(!turn2.wants_tools());
    }
}
