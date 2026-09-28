// SPDX-License-Identifier: AGPL-3.0-only
//! A2A conversations, principals & commands. The daemon binds the
//! real A2A listener over plaintext loopback (⇒ the `operator` principal, so no
//! cert plumbing is needed to exercise the wiring). A JSON-RPC peer drives the
//! surface: a `status` command DataPart is answered deterministically, as a
//! Message, without a model turn or a task; the agent card is discoverable; a natural-language message runs a
//! turn worker and the answer lands as the task's artifact, readable back through
//! `GetTask` and enumerable through `ListTasks`.

use std::net::TcpListener;

use serde_json::{Value, json};

use crate::checks::util::{
    get_card, mock_llm, post, rpc, rpc_body, rpc_value as rpc_raw, send_command, send_text,
    text_params, wait_ready, write_file,
};
use crate::{Category, Check, Harness, Outcome};

pub fn checks() -> Vec<Check> {
    vec![
        Check {
            id: "a2a-conversation/status-command-no-model-turn",
            category: Category::A2aConversation,
            desc: "a `status` command DataPart is answered deterministically with a Message and no task",
            run: status_command,
        },
        Check {
            id: "a2a-conversation/agent-card",
            category: Category::A2aConversation,
            desc: "the agent card is discoverable and advertises streaming",
            run: agent_card,
        },
        Check {
            id: "a2a-conversation/card-declares-only-what-it-implements",
            category: Category::A2aConversation,
            desc: "every capability the agent card advertises is exercisable, and one it disclaims is refused cleanly rather than half-served",
            run: card_honesty,
        },
        Check {
            id: "a2a-conversation/protocol-errors-use-the-specified-codes",
            category: Category::A2aConversation,
            desc: "an unknown method is -32601 and an unknown task is -32001, rather than a generic failure",
            run: protocol_errors,
        },
        Check {
            id: "a2a-conversation/tasks-are-proto3-json-on-every-path",
            category: Category::A2aConversation,
            desc: "SendMessage, GetTask and ListTasks all return the same proto3-JSON `Task`: state under `status`, an RFC 3339 timestamp, `ROLE_AGENT`",
            run: task_shape,
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

fn a2a_config(llm: &str, port: u16, extra: &str) -> String {
    format!(
        "config_version: \"1\"\n\
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

    let sent = send_command(&addr, 1, "status");
    if sent.get("error").is_some() {
        return Outcome::fail(format!("a status command should be answered: {sent}"));
    }
    // A read is the spec's immediate reply: a Message, and no task to track.
    let message = &sent["result"]["message"];
    Outcome::require(
        sent["result"].get("task").is_none() && message["role"] == "ROLE_AGENT",
        format!("a status command should answer with an agent Message and no task: {sent}"),
    )
    .and(|| {
        let doc = &message["parts"][0]["data"];
        Outcome::require(
            doc["conversations"].is_array() && doc["runs"].is_array(),
            format!("the status document should list conversations + runs: {doc}"),
        )
    })
    .and(|| {
        // …so polling it leaves the task list as it found it.
        let _ = send_command(&addr, 2, "status");
        let listed = rpc(&addr, 3, "ListTasks", json!({}));
        Outcome::require(
            listed["tasks"].as_array().is_none_or(Vec::is_empty),
            format!("status reads should create no tasks: {listed}"),
        )
    })
}

/// The A2A wire is **proto3 JSON**, and the ways to get that subtly wrong are
/// all silent: a peer's generated types reject `"agent"` where `ROLE_AGENT` is
/// expected, an integer where a `google.protobuf.Timestamp` string is expected,
/// or a flat `state` where a `TaskStatus` is expected — and the failure lands in
/// the peer, not here. Every path that emits a task is checked, because they are
/// separate code (full projection, listing projection) and nothing forces them
/// to stay in agreement — a divergence would otherwise surface only at a peer.
fn task_shape(h: &Harness) -> Outcome {
    let tmp = h.tempdir();
    let llm = mock_llm(h, &tmp, &json!({"turns": [{"content": "unused"}]}));
    let port = free_port();
    let addr = format!("127.0.0.1:{port}");
    let cfg = write_file(&tmp, "agentd.yaml", &a2a_config(&llm.uri, port, ""));
    let _daemon = h.spawn(&["--config", &cfg]);
    wait_ready(&addr);

    // A natural-language send is what makes a task (a read makes none).
    let sent = send_text(&addr, 1, "hello", false);
    let task = sent["result"]["task"].clone();
    let id = task["id"].as_str().unwrap_or("").to_string();

    let listed = rpc(&addr, 2, "ListTasks", json!({}));
    let got = rpc(&addr, 3, "GetTask", json!({"id": id}));
    let from_list = listed["tasks"]
        .as_array()
        .and_then(|a| a.iter().find(|t| t["id"] == id.as_str()))
        .cloned()
        .unwrap_or(Value::Null);

    for (what, t) in [
        ("SendMessage", &task),
        ("GetTask", &got),
        ("ListTasks", &from_list),
    ] {
        if t["status"]["state"].as_str().is_none() {
            return Outcome::fail(format!(
                "{what} should carry the state in `status.state`: {t}"
            ));
        }
        if !t["state"].is_null() {
            return Outcome::fail(format!(
                "{what} puts a bare `state` at the top level; `Task` has no such field, so a peer reads the task as stateless: {t}"
            ));
        }
        // `TaskStatus.timestamp` is a `google.protobuf.Timestamp` ⇒ RFC 3339.
        match t["status"]["timestamp"].as_str() {
            Some(ts) if ts.ends_with('Z') && ts.contains('T') => {}
            _ => {
                return Outcome::fail(format!(
                    "{what}: `status.timestamp` must be an RFC 3339 string, not epoch millis: {t}"
                ));
            }
        }
        let role = &t["status"]["message"]["role"];
        if !role.is_null() && role != "ROLE_AGENT" {
            return Outcome::fail(format!(
                "{what}: the role is a proto enum name (`ROLE_AGENT`), not the English word: {t}"
            ));
        }
    }

    // The listing envelope carries the paging fields. Proto3 JSON omits a field
    // at its default value, so `nextPageToken` is absent exactly when there is
    // no next page — its absence is the answer, not an omission. The counts are
    // asserted because they are non-default here: a task exists.
    if listed["totalSize"].as_u64().unwrap_or(0) == 0 {
        return Outcome::fail(format!(
            "ListTasks should report how many tasks it found: {listed}"
        ));
    }
    Outcome::pass()
}

fn agent_card(h: &Harness) -> Outcome {
    let tmp = h.tempdir();
    let llm = mock_llm(h, &tmp, &json!({"turns": [{"content": "unused"}]}));
    let port = free_port();
    let addr = format!("127.0.0.1:{port}");
    let cfg = write_file(&tmp, "agentd.yaml", &a2a_config(&llm.uri, port, ""));
    let _daemon = h.spawn(&["--config", &cfg]);
    wait_ready(&addr);

    let card = get_card(&addr);
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

/// The card is a promise. This checks both directions of it: a capability
/// advertised as available actually works, and one advertised as absent is
/// refused with a real JSON-RPC error rather than half-served or crashed.
///
/// The failure this guards against is the expensive kind — a peer that reads
/// the card, believes it, and builds against something that is not there.
fn card_honesty(h: &Harness) -> Outcome {
    let tmp = h.tempdir();
    let llm = mock_llm(h, &tmp, &json!({"turns": [{"content": "ok"}]}));
    let port = free_port();
    let addr = format!("127.0.0.1:{port}");
    let cfg = write_file(&tmp, "agentd.yaml", &a2a_config(&llm.uri, port, ""));
    let _daemon = h.spawn(&["--config", &cfg]);
    wait_ready(&addr);

    let card = get_card(&addr);
    let caps = &card["capabilities"];

    // Advertised as available ⇒ it must work. `streaming` is the one the card
    // claims, so a streaming send must produce a task rather than an error.
    if caps["streaming"] == true {
        // A streaming send answers with SSE, not a JSON body — so assert on the
        // stream: the spec's update frames must arrive and reach a terminal
        // state, which is exactly what "streaming: true" promises a caller.
        let body = rpc_body(2, "SendStreamingMessage", text_params("hi", None, false));
        let raw = post(&addr, &body, &[]);
        if raw.contains("\"error\"") {
            return Outcome::fail(format!(
                "the card advertises streaming but the stream carried an error: {raw}"
            ));
        }
        if !raw.contains("statusUpdate") {
            return Outcome::fail(format!(
                "a streaming send should emit statusUpdate frames: {raw}"
            ));
        }
        if !raw.contains("TASK_STATE_COMPLETED") {
            return Outcome::fail(format!("the stream should reach a terminal state: {raw}"));
        }
    }

    // Advertised as ABSENT ⇒ asking for it is a clean refusal. A capability we
    // disclaim must not be silently half-implemented, and must not 500.
    if caps["pushNotifications"] == false {
        let env = rpc_raw(
            &addr,
            3,
            "SetTaskPushNotificationConfig",
            json!({"taskId": "t", "pushNotificationConfig": {"url": "https://x.example/hook"}}),
        );
        let code = env["error"]["code"].as_i64();
        if code.is_none() {
            return Outcome::fail(format!(
                "pushNotifications is disclaimed, so the method must be refused: {env}"
            ));
        }
        // -32601 (unknown method) and -32004 (unsupported operation) both say
        // "not here" honestly; anything else is a surprise for a caller.
        if !matches!(code, Some(-32601) | Some(-32004)) {
            return Outcome::fail(format!(
                "a disclaimed capability should refuse with -32601/-32004, got: {env}"
            ));
        }
    }

    Outcome::pass()
}

/// JSON-RPC and A2A both specify codes for the two failures a client actually
/// branches on. Returning a generic error instead forces peers to string-match
/// our messages, which then become an accidental API.
fn protocol_errors(h: &Harness) -> Outcome {
    let tmp = h.tempdir();
    let llm = mock_llm(h, &tmp, &json!({"turns": [{"content": "ok"}]}));
    let port = free_port();
    let addr = format!("127.0.0.1:{port}");
    let cfg = write_file(&tmp, "agentd.yaml", &a2a_config(&llm.uri, port, ""));
    let _daemon = h.spawn(&["--config", &cfg]);
    wait_ready(&addr);

    let unknown = rpc_raw(&addr, 1, "DefinitelyNotAMethod", json!({}));
    if unknown["error"]["code"].as_i64() != Some(-32601) {
        return Outcome::fail(format!(
            "an unknown method should be -32601 (method not found): {unknown}"
        ));
    }

    let missing = rpc_raw(&addr, 2, "GetTask", json!({"id": "task-does-not-exist"}));
    if missing["error"]["code"].as_i64() != Some(-32001) {
        return Outcome::fail(format!(
            "an unknown task should be -32001 (task not found): {missing}"
        ));
    }

    Outcome::pass()
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
    let sent = send_text(&addr, 1, "Say hello", false);
    let result = &sent["result"];
    if sent.get("error").is_some() {
        return Outcome::fail(format!("SendMessage should succeed: {sent}"));
    }
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
