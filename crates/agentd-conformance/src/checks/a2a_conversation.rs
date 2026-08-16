// SPDX-License-Identifier: Apache-2.0
//! A2A conversations, principals & commands (RFC 0029). A v2 daemon binds the
//! real A2A listener over plaintext loopback (⇒ the `operator` principal, so no
//! cert plumbing is needed to exercise the wiring). A JSON-RPC peer drives the
//! surface: a `status` command DataPart completes deterministically without a
//! model turn; the agent card is discoverable; a natural-language message runs a
//! turn worker and the answer lands as the task's artifact, readable back through
//! `GetTask` and enumerable through `ListTasks`.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::checks::util::{mock_llm, write_file};
use crate::{Category, Check, Harness, Outcome};

pub fn checks() -> Vec<Check> {
    vec![
        Check {
            id: "a2a-conversation/status-command-no-model-turn",
            category: Category::A2aConversation,
            desc: "a `status` command DataPart returns a completed task deterministically",
            run: status_command,
        },
        Check {
            id: "a2a-conversation/agent-card",
            category: Category::A2aConversation,
            desc: "the agent card is discoverable and advertises streaming",
            run: agent_card,
        },
        Check {
            id: "a2a-conversation/nl-message-becomes-task-artifact",
            category: Category::A2aConversation,
            desc: "a natural-language message runs a turn; the answer is the task artifact, readable via GetTask/ListTasks",
            run: nl_message_artifact,
        },
    ]
}

/// A free loopback port (bind :0, read it, drop). agentd rebinds within ms.
fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .expect("bind ephemeral")
        .local_addr()
        .expect("local_addr")
        .port()
}

/// One HTTP POST of a JSON-RPC body over loopback; returns the response body.
fn post_raw(addr: &str, body: &str) -> String {
    let mut s = TcpStream::connect(addr).expect("connect a2a http");
    s.set_read_timeout(Some(Duration::from_secs(30))).ok();
    let head = format!(
        "POST / HTTP/1.1\r\nHost: x\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    s.write_all(head.as_bytes()).unwrap();
    s.write_all(body.as_bytes()).unwrap();
    s.flush().unwrap();
    let mut reader = BufReader::new(s);
    let mut status = String::new();
    reader.read_line(&mut status).unwrap();
    loop {
        let mut l = String::new();
        reader.read_line(&mut l).unwrap();
        if l.trim().is_empty() {
            break;
        }
    }
    let mut b = String::new();
    reader.read_to_string(&mut b).unwrap();
    b
}

/// A JSON-RPC call over A2A; returns the `result` (panics — caught by
/// `run_check` and reported as a failure — on a transport / RPC error).
fn rpc(addr: &str, id: i64, method: &str, params: Value) -> Value {
    let body = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}).to_string();
    let resp = post_raw(addr, &body);
    let v: Value =
        serde_json::from_str(&resp).unwrap_or_else(|_| panic!("non-JSON A2A response: {resp:?}"));
    assert!(v.get("error").is_none(), "A2A rpc error for {method}: {v}");
    v["result"].clone()
}

/// Block until the listener accepts a connection, or fail past the deadline.
fn wait_ready(addr: &str) {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if TcpStream::connect(addr).is_ok() {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "a2a listener never became connectable at {addr}"
        );
        std::thread::sleep(Duration::from_millis(25));
    }
}

fn a2a_config(llm: &str, port: u16, extra: &str) -> String {
    format!(
        "config_version: \"2\"\n\
         agent:\n  name: a2a-conf\n  instruction: You are a helpful test agent.\n  preflight: never\n\
         intelligence:\n  endpoints: {llm}\n  model: mock\n\
         store:\n  kind: memory\n\
         a2a:\n  listen: http://127.0.0.1:{port}\n\
         lifecycle:\n  run_until: drained\n\
         observability:\n  log_level: info\n  log_content: true\n{extra}"
    )
}

fn artifact_answer(task: &Value) -> String {
    task["artifacts"]
        .as_array()
        .and_then(|a| {
            a.iter().find(|x| {
                x["artifactId"]
                    .as_str()
                    .is_some_and(|s| s.ends_with(".result"))
            })
        })
        .and_then(|x| x["parts"][0]["text"].as_str())
        .unwrap_or("")
        .to_string()
}

fn status_command(h: &Harness) -> Outcome {
    let tmp = h.tempdir();
    let llm = mock_llm(h, &tmp, &json!({"turns": [{"content": "unused"}]}));
    let port = free_port();
    let addr = format!("127.0.0.1:{port}");
    let cfg = write_file(&tmp, "agentd.yaml", &a2a_config(&llm.uri, port, ""));
    let _daemon = h.spawn(&["--config", &cfg]);
    wait_ready(&addr);

    let params =
        json!({"message": {"messageId": "m1", "parts": [{"data": {"agentd": {"op": "status"}}}]}});
    let result = rpc(&addr, 1, "SendMessage", params);
    let task = &result["task"];
    Outcome::require(
        task["status"]["state"] == "TASK_STATE_COMPLETED",
        format!("a status command should complete: {task}"),
    )
    .and(|| {
        let text = task["status"]["message"]["parts"][0]["text"]
            .as_str()
            .unwrap_or("");
        Outcome::require(
            text.contains("conversations") && text.contains("runs"),
            format!("the status summary should name conversations + runs: {text:?}"),
        )
    })
}

fn agent_card(h: &Harness) -> Outcome {
    let tmp = h.tempdir();
    let llm = mock_llm(h, &tmp, &json!({"turns": [{"content": "unused"}]}));
    let port = free_port();
    let addr = format!("127.0.0.1:{port}");
    let cfg = write_file(&tmp, "agentd.yaml", &a2a_config(&llm.uri, port, ""));
    let _daemon = h.spawn(&["--config", &cfg]);
    wait_ready(&addr);

    let card = rpc(&addr, 1, "GetAgentCard", json!({}));
    Outcome::require(
        card["name"] == "agentd",
        format!("card name should be agentd: {card}"),
    )
    .and(|| {
        Outcome::require(
            card["capabilities"]["streaming"] == true,
            format!("the card should advertise streaming: {card}"),
        )
    })
}

fn nl_message_artifact(h: &Harness) -> Outcome {
    let tmp = h.tempdir();
    let llm = mock_llm(h, &tmp, &json!({"turns": [{"content": "Hello over A2A!"}]}));
    let port = free_port();
    let addr = format!("127.0.0.1:{port}");
    let cfg = write_file(&tmp, "agentd.yaml", &a2a_config(&llm.uri, port, ""));
    let _daemon = h.spawn(&["--config", &cfg]);
    wait_ready(&addr);

    // A natural-language message → a conversation turn → a completed task whose
    // artifact carries the model's answer (a blocking send waits for it).
    let params = json!({"message": {"messageId": "m1", "parts": [{"text": "Say hello"}]}});
    let result = rpc(&addr, 1, "SendMessage", params);
    let task = &result["task"];
    let task_id = match task["id"].as_str() {
        Some(id) => id.to_string(),
        None => {
            return Outcome::fail(format!(
                "SendMessage should return a task with an id: {result}"
            ));
        }
    };
    if task["status"]["state"] != "TASK_STATE_COMPLETED" {
        return Outcome::fail(format!("the nl task should complete: {task}"));
    }
    if !artifact_answer(task).contains("Hello over A2A") {
        return Outcome::fail(format!("the answer should be the task artifact: {task}"));
    }

    // GetTask reads the same terminal task back.
    let got = rpc(&addr, 2, "GetTask", json!({"id": task_id}));
    if got["status"]["state"] != "TASK_STATE_COMPLETED" || got["id"] != task_id {
        return Outcome::fail(format!("GetTask should return the terminal task: {got}"));
    }

    // ListTasks enumerates it (operator sees it).
    let listed = rpc(&addr, 3, "ListTasks", json!({}));
    Outcome::require(
        listed["tasks"]
            .as_array()
            .is_some_and(|a| a.iter().any(|t| t["id"] == task_id)),
        format!("the task should be listed: {listed}"),
    )
}
