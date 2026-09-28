// SPDX-License-Identifier: AGPL-3.0-only
//! The display-client interface. A daemon with `a2a.events.enabled` serves the
//! observation plane on its A2A listener: the `SubscribeToEvents` feed
//! (hello → events, cursor replay), the taskless reads, and the
//! human-in-the-loop gate (`ask_human` → `input-required` → a `taskId` reply
//! resumes the asker). With the interface OFF the surface refuses and the core
//! A2A wire still answers — the default-OFF contract, so enabling an
//! observation plane is always a deliberate act and never a side effect.

use std::io::{BufRead, BufReader};
use std::net::TcpListener;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::checks::util::{
    feed_method, mock_llm, open, rpc, rpc_body, send_command, send_text, text_params, wait_ready,
    write_file,
};
use crate::{Category, Check, Harness, Outcome};

pub fn checks() -> Vec<Check> {
    vec![
        Check {
            id: "interface/default-off-gate",
            category: Category::Interface,
            desc: "without a2a.events.enabled the surface refuses (-32004) and the core answers",
            run: default_off,
        },
        Check {
            id: "interface/feed-hello-and-replay",
            category: Category::Interface,
            desc: "SubscribeToEvents opens with a hello and replays a prompt's message event from seq 0",
            run: feed_replay,
        },
        Check {
            id: "interface/hitl-gate-roundtrip",
            category: Category::Interface,
            desc: "ask_human gates the task as input-required; a taskId reply resumes the turn",
            run: hitl_roundtrip,
        },
    ]
}

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .expect("bind ephemeral")
        .local_addr()
        .expect("local_addr")
        .port()
}

fn config(llm: &str, port: u16, feed: bool) -> String {
    let feed = if feed {
        "  events:\n    enabled: true\n"
    } else {
        ""
    };
    format!(
        "config_version: \"1\"\n\
         agent:\n  name: iface-conf\n  instruction: You are a helpful test agent.\n  preflight: never\n\
         intelligence:\n  endpoints: {llm}\n  model: mock\n\
         store:\n  kind: memory\n\
         a2a:\n  listen: http://127.0.0.1:{port}\n{feed}\
         lifecycle:\n  run_until: drained\n"
    )
}

fn default_off(h: &Harness) -> Outcome {
    let tmp = h.tempdir();
    let llm = mock_llm(h, &tmp, &json!({"turns": [{"content": "unused"}]}));
    let port = free_port();
    let addr = format!("127.0.0.1:{port}");
    let cfg = write_file(&tmp, "agentd.yaml", &config(&llm.uri, port, false));
    let _daemon = h.spawn(&["--config", &cfg]);
    wait_ready(&addr);

    let refused = send_command(&addr, 1, "interface.info");
    Outcome::require(
        refused["error"]["code"] == -32004,
        format!("interface.info should refuse with -32004 while disabled: {refused}"),
    )
    .and(|| {
        // The core surface is untouched: status still answers.
        let st = send_command(&addr, 2, "status");
        Outcome::require(
            st["result"]["task"]["status"]["state"] == "TASK_STATE_COMPLETED",
            format!("the core status command should still answer: {st}"),
        )
    })
}

fn feed_replay(h: &Harness) -> Outcome {
    let tmp = h.tempdir();
    let llm = mock_llm(h, &tmp, &json!({"turns": [{"content": "The reply."}]}));
    let port = free_port();
    let addr = format!("127.0.0.1:{port}");
    let cfg = write_file(&tmp, "agentd.yaml", &config(&llm.uri, port, true));
    let _daemon = h.spawn(&["--config", &cfg]);
    wait_ready(&addr);

    // Create history FIRST, then subscribe from seq 0 — the ring must replay
    // the prompt's `message` event to the late joiner.
    let sent = send_text(&addr, 1, "Replay me", false);
    assert_eq!(
        sent["result"]["task"]["status"]["state"],
        "TASK_STATE_COMPLETED"
    );

    let body = rpc_body(9, feed_method(), json!({"fromSeq": 0}));
    let s = open(&addr, &body, &[]);
    s.set_read_timeout(Some(Duration::from_secs(10))).ok();
    let mut reader = BufReader::new(s);
    let mut saw_hello = false;
    let mut saw_message = false;
    let deadline = Instant::now() + Duration::from_secs(8);
    while Instant::now() < deadline && !(saw_hello && saw_message) {
        let mut line = String::new();
        match reader.read_line(&mut line) {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        if let Some(data) = line.strip_prefix("data:")
            && let Ok(v) = serde_json::from_str::<Value>(data.trim())
        {
            let r = &v["result"];
            if r.get("hello").is_some() {
                saw_hello = true;
            }
            if r["event"]["kind"] == "message"
                && r["event"]["data"]["text"]
                    .as_str()
                    .is_some_and(|t| t.contains("Replay me"))
            {
                saw_message = true;
            }
        }
    }
    Outcome::require(
        saw_hello,
        "the stream should open with a hello frame".to_string(),
    )
    .and(|| {
        Outcome::require(
            saw_message,
            "the ring should replay the prompt's message event from seq 0".to_string(),
        )
    })
}

fn hitl_roundtrip(h: &Harness) -> Outcome {
    let tmp = h.tempdir();
    let llm = mock_llm(
        h,
        &tmp,
        &json!({
            "turns": [
                {"tool_calls": [{"name": "ask_human", "arguments": {"question": "Proceed?"}}]},
                {"content": "Proceeded."}
            ]
        }),
    );
    let port = free_port();
    let addr = format!("127.0.0.1:{port}");
    let cfg = write_file(&tmp, "agentd.yaml", &config(&llm.uri, port, true));
    let _daemon = h.spawn(&["--config", &cfg]);
    wait_ready(&addr);

    let sent = rpc(
        &addr,
        1,
        "SendMessage",
        text_params("Do the thing", None, true),
    );
    let task_id = sent["task"]["id"].as_str().expect("task id").to_string();
    // The gate appears…
    let deadline = Instant::now() + Duration::from_secs(8);
    let gated = loop {
        let t = rpc(&addr, 2, "GetTask", json!({"id": task_id}));
        if t["status"]["state"] == "TASK_STATE_INPUT_REQUIRED" {
            break t;
        }
        assert!(Instant::now() < deadline, "no gate: {t}");
        std::thread::sleep(Duration::from_millis(80));
    };
    Outcome::require(
        gated["status"]["message"]["parts"][0]["text"]
            .as_str()
            .is_some_and(|q| q.contains("Proceed?")),
        format!("the gate should carry the question: {gated}"),
    )
    .and(|| {
        // …and the taskId reply resumes the turn to completion.
        rpc(
            &addr,
            3,
            "SendMessage",
            text_params("yes", Some(&task_id), true),
        );
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let t = rpc(&addr, 4, "GetTask", json!({"id": task_id}));
            if t["status"]["state"] == "TASK_STATE_COMPLETED" {
                return Outcome::require(
                    t["artifacts"][0]["parts"][0]["text"]
                        .as_str()
                        .is_some_and(|a| a.contains("Proceeded")),
                    format!("the resumed turn's answer should land as the artifact: {t}"),
                );
            }
            if Instant::now() >= deadline {
                return Outcome::fail(format!("the reply should resume the turn: {t}"));
            }
            std::thread::sleep(Duration::from_millis(80));
        }
    })
}
