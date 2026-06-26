//! A minimal built-in mock LLM for the observe-to-validate E2E suite (M7).
//! Hidden mode: `agentd --internal-mock-llm <socket> [script]`.
//!
//! Speaks just enough OpenAI-compatible `/chat/completions` over a unix socket
//! to drive a *real* agentic loop without a live model: it reads the request and
//! returns a scripted assistant turn — a final answer or a tool call — switching
//! to a final answer once a tool result appears in the transcript (so the ReAct
//! cycle closes). Scripts: `final` (answer at once), `read` (call `resource.read`
//! then answer), `schedule` (call the `schedule` self-tool then answer). Small
//! enough to ship; it makes the loop + self-* tools observable end to end.

use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};

/// Serve the mock LLM until the process is killed. Returns the exit code.
pub fn run(socket: &str, script: &str) -> i32 {
    let _ = std::fs::remove_file(socket);
    let listener = match UnixListener::bind(socket) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("mock-llm: bind {socket}: {e}");
            return crate::exit::GENERIC;
        }
    };
    // One request per connection (the intel client uses Connection: close).
    for stream in listener.incoming().flatten() {
        handle(stream, script);
    }
    0
}

fn handle(mut stream: UnixStream, script: &str) {
    let Some(body) = read_request_body(&mut stream) else { return };
    // A `role:tool` message means the model already called a tool, so the next
    // turn is a final answer.
    let saw_tool_result = body.contains("\"role\":\"tool\"") || body.contains("\"role\": \"tool\"");
    let payload = response_json(script, saw_tool_result);
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
fn read_request_body(stream: &mut UnixStream) -> Option<String> {
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
        if let Some(v) = t.strip_prefix("Content-Length:").or_else(|| t.strip_prefix("content-length:")) {
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
        ("schedule", false) => tool_call("schedule", r#"{"after_seconds":1,"instruction":"follow up"}"#),
        ("schedule", true) => final_answer("scheduled a follow-up"),
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
}
