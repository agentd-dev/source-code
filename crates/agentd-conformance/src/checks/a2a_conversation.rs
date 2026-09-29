// SPDX-License-Identifier: AGPL-3.0-only
//! The core A2A surface, as a peer that knows only the specification meets it.
//! The daemon binds the real A2A listener over plaintext loopback with no
//! credential configured (so the caller is the implicit operator and no cert
//! plumbing is needed). A JSON-RPC peer drives it: a request states the
//! protocol version it speaks and names a method of the specification; a read
//! command is answered as a Message without a task; a natural-language message
//! runs a turn whose answer lands as the task's artifact and whose history
//! holds the conversation; a turn that asks a human waits in `input-required`
//! on the core wire alone; a registered webhook is sent the task; and the card
//! promises only what the listener does.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::checks::util::{
    Headers, command_uri, declared_extensions, free_port, get_card, header, mock_llm, post_stating,
    rpc, rpc_body, rpc_value as rpc_raw, send_command, send_text, stream, stream_command,
    text_params, wait_ready, write_file,
};
use crate::{Category, Check, Harness, Outcome};

pub fn checks() -> Vec<Check> {
    vec![
        Check {
            id: "a2a-conversation/version-and-vocabulary",
            category: Category::A2aConversation,
            desc: "a request whose A2A-Version is absent, empty, 0.3 or 1.1 is -32009 VERSION_NOT_SUPPORTED with its id echoed (a 1.0 patch is served); a method outside the specification's and the declared extensions' — a made-up name, the A2A 0.3 `message/send` — is -32601; neither creates a task",
            run: version_and_vocabulary,
        },
        Check {
            id: "a2a-conversation/read-commands-create-no-task",
            category: Category::A2aConversation,
            desc: "every command the card lists as a `message` reply is answered with an agent Message or refused, never with a task — streamed, one message frame — and the task list stays empty",
            run: read_commands,
        },
        Check {
            id: "a2a-conversation/agent-card",
            category: Category::A2aConversation,
            desc: "the agent card is discoverable, names the configured agent, advertises streaming and a JSONRPC interface at A2A 1.0",
            run: agent_card,
        },
        Check {
            id: "a2a-conversation/card-declares-only-what-it-implements",
            category: Category::A2aConversation,
            desc: "streaming, which the card advertises, streams a task to a terminal state; push notifications, which it disclaims, are refused by CreateTaskPushNotificationConfig with -32003",
            run: card_honesty,
        },
        Check {
            id: "a2a-conversation/push-delivers-a-stream-response",
            category: Category::A2aConversation,
            desc: "with a2a.push.enabled the card advertises push notifications, and a webhook registered with CreateTaskPushNotificationConfig is POSTed the task as a StreamResponse {task} (application/a2a+json) through to its terminal state",
            run: push_delivers,
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
        Check {
            id: "a2a-conversation/input-required-is-core",
            category: Category::A2aConversation,
            desc: "with no extension enabled, ask_human puts the task in TASK_STATE_INPUT_REQUIRED carrying the question, and a SendMessage naming the taskId resumes the turn to completion",
            run: input_required,
        },
        Check {
            id: "a2a-conversation/task-history-carries-the-turn",
            category: Category::A2aConversation,
            desc: "Task.history holds, in order, the caller's prompt under its own messageId, the agent's superseded question and the caller's answer, never the current status; historyLength 0 omits it and 1 keeps the newest",
            run: task_history,
        },
    ]
}

fn a2a_config(llm: &str, port: u16, extra: &str) -> String {
    format!(
        "\
         agent:\n  name: a2a-conf\n  instruction: You are a helpful test agent.\n  preflight: never\n\
         intelligence:\n  endpoints: {llm}\n  model: mock\n\
         store:\n  kind: memory\n\
         a2a:\n  listen: http://127.0.0.1:{port}\n{extra}\
         lifecycle:\n  run_until: drained\n\
         observability:\n  log_level: info\n  log_content: true\n"
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

fn task_count(addr: &str) -> usize {
    rpc(addr, 90, "ListTasks", json!({}))["tasks"]
        .as_array()
        .map_or(0, Vec::len)
}

fn version_and_vocabulary(h: &Harness) -> Outcome {
    let tmp = h.tempdir();
    let llm = mock_llm(h, &tmp, &json!({"turns": [{"content": "unused"}]}));
    let port = free_port();
    let addr = format!("127.0.0.1:{port}");
    let cfg = write_file(&tmp, "agentd.yaml", &a2a_config(&llm.uri, port, ""));
    let _daemon = h.spawn(&["--config", &cfg]);
    wait_ready(&addr);

    // The version gate stands before the method: a send the server would
    // otherwise run is refused, and runs nothing (A2A 1.0.1 §3.6.2).
    let send = rpc_body(1, "SendMessage", text_params("hi", None, false));
    for version in [None, Some(""), Some("0.3"), Some("1.1")] {
        let env = post_stating(&addr, version, &send);
        let reason = env["error"]["data"]
            .as_array()
            .into_iter()
            .flatten()
            .find_map(|d| d["reason"].as_str());
        if env["error"]["code"] != -32009
            || env["id"] != 1
            || reason != Some("VERSION_NOT_SUPPORTED")
        {
            return Outcome::fail(format!(
                "A2A-Version {version:?} should be -32009 VERSION_NOT_SUPPORTED echoing the id: {env}"
            ));
        }
    }
    // A patch does not change the protocol: 1.0.1 is 1.0.
    let patch = post_stating(&addr, Some("1.0.1"), &rpc_body(2, "ListTasks", json!({})));
    if patch.get("error").is_some() || !patch["result"].is_object() {
        return Outcome::fail(format!(
            "A2A-Version 1.0.1 should be served as 1.0: {patch}"
        ));
    }

    // A name the specification does not define — invented, or an older
    // protocol's spelling of a send — is not a method, whatever it carries.
    let older = json!({"message": {"role": "user", "messageId": "conf-0.3", "parts": [{"kind": "text", "text": "hi"}]}});
    for (method, params) in [
        ("NoSuchMethod", text_params("hi", None, false)),
        ("message/send", older),
    ] {
        let env = rpc_raw(&addr, 3, method, params);
        if env["error"]["code"] != -32601 || env["id"] != 3 {
            return Outcome::fail(format!("{method} should be -32601 method not found: {env}"));
        }
    }
    Outcome::require(
        task_count(&addr) == 0,
        "a refused version or method must not create a task".to_string(),
    )
}

/// A read is the spec's immediate reply: a Message, and no task to track. The
/// card says which commands are reads; each is sent bare, and is either
/// answered as one or refused (one that needs arguments) — never run as a task.
fn read_commands(h: &Harness) -> Outcome {
    let tmp = h.tempdir();
    let llm = mock_llm(h, &tmp, &json!({"turns": [{"content": "unused"}]}));
    let port = free_port();
    let addr = format!("127.0.0.1:{port}");
    let cfg = write_file(&tmp, "agentd.yaml", &a2a_config(&llm.uri, port, ""));
    let _daemon = h.spawn(&["--config", &cfg]);
    wait_ready(&addr);

    let card = get_card(&addr);
    let uri = command_uri(&card);
    let reads: Vec<String> = card["capabilities"]["extensions"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|e| e["uri"] == uri.as_str())
        .flat_map(|e| e["params"]["ops"].as_array().cloned().unwrap_or_default())
        .filter(|o| o["reply"] == "message")
        .filter_map(|o| o["op"].as_str().map(str::to_string))
        .collect();
    if !reads.iter().any(|op| op == "status") {
        return Outcome::fail(format!("the card should list `status` as a read: {card}"));
    }

    for (n, op) in reads.iter().enumerate() {
        let sent = send_command(&addr, 10 + n as i64, op);
        let answered = sent["result"]["message"]["role"] == "ROLE_AGENT";
        if sent["result"].get("task").is_some() || !(answered || sent.get("error").is_some()) {
            return Outcome::fail(format!(
                "the read `{op}` should be answered with an agent Message or refused, never a task: {sent}"
            ));
        }
        if op == "status" {
            let doc = &sent["result"]["message"]["parts"][0]["data"];
            if !(answered && doc["conversations"].is_array() && doc["runs"].is_array()) {
                return Outcome::fail(format!(
                    "`status` should answer with the document listing conversations + runs: {sent}"
                ));
            }
        }
    }

    let (status, _, frames) = stream_command(&addr, 5, "status");
    if !(status == 200 && frames.len() == 1 && frames[0].data["result"].get("message").is_some()) {
        return Outcome::fail(format!(
            "a streamed read should be exactly one message frame: {status} {frames:?}"
        ));
    }
    Outcome::require(
        task_count(&addr) == 0,
        "reads should create no tasks".to_string(),
    )
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
    // The name is the one the operator configured (`agent.name`), not the
    // product's: a registry listing several agents must tell them apart.
    Outcome::require(
        card["name"] == "a2a-conf",
        format!("the card name should be the configured agent.name: {card}"),
    )
    .and(|| {
        Outcome::require(
            card["capabilities"]["streaming"] == true,
            format!("the card should advertise streaming: {card}"),
        )
    })
    .and(|| {
        // The interface declares the protocol version it actually serves, as
        // Major.Minor: a 1.0 client selects an interface by binding and
        // version, and passes over one that claims another.
        let iface = &card["supportedInterfaces"][0];
        Outcome::require(
            iface["protocolBinding"] == "JSONRPC" && iface["protocolVersion"] == "1.0",
            format!("the interface should be JSONRPC at A2A 1.0: {card}"),
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
    // This daemon's card makes one promise each way; a card that made neither
    // would leave the rest of the check nothing to hold it to.
    if caps["streaming"] != true || caps["pushNotifications"] != false {
        return Outcome::fail(format!(
            "a daemon without a2a.push should advertise streaming and disclaim push: {card}"
        ));
    }

    // Advertised as available ⇒ it must work: the spec's update frames arrive
    // and reach a terminal state, which is exactly what "streaming: true"
    // promises a caller.
    let (_, _, frames) = stream(
        &addr,
        &rpc_body(2, "SendStreamingMessage", text_params("hi", None, false)),
        &[],
    );
    let results: Vec<&Value> = frames.iter().map(|f| &f.data["result"]).collect();
    let task_id = results
        .first()
        .and_then(|r| r["task"]["id"].as_str())
        .unwrap_or("")
        .to_string();
    if task_id.is_empty()
        || frames.iter().any(|f| f.data.get("error").is_some())
        || !results.iter().any(|r| r.get("statusUpdate").is_some())
        || !results
            .iter()
            .any(|r| r["statusUpdate"]["status"]["state"] == "TASK_STATE_COMPLETED")
    {
        return Outcome::fail(format!(
            "the card advertises streaming, so a streamed send should open with its task and carry statusUpdate frames to TASK_STATE_COMPLETED: {frames:?}"
        ));
    }

    // Advertised as ABSENT ⇒ the specification's method for it answers with
    // the specification's refusal (§3.3.4: PushNotificationNotSupportedError),
    // for a task that exists — so the answer cannot be "no such task" either.
    let env = rpc_raw(
        &addr,
        3,
        "CreateTaskPushNotificationConfig",
        json!({"taskId": task_id, "url": "https://x.example/hook"}),
    );
    Outcome::require(
        env["error"]["code"] == -32003,
        format!(
            "pushNotifications is disclaimed, so CreateTaskPushNotificationConfig should be -32003: {env}"
        ),
    )
}

/// The other direction of the push promise: advertised, a registered webhook
/// is sent the task — the spec's `StreamResponse`, the union a streaming caller
/// reads — until its terminal state.
fn push_delivers(h: &Harness) -> Outcome {
    let tmp = h.tempdir();
    // A slow turn, so the webhook is registered while the task still works
    // and the terminal transition is a real delivery, not a race with one.
    let llm = mock_llm(
        h,
        &tmp,
        &json!({"turns": [{"content": "done at last", "delay_ms": 1500}]}),
    );
    let hook = Hook::spawn();
    let port = free_port();
    let addr = format!("127.0.0.1:{port}");
    // `allow_private` because the receiver is loopback — the decision the
    // flag exists to make explicit.
    let push = "  push:\n    enabled: true\n    allow_private: true\n";
    let cfg = write_file(&tmp, "agentd.yaml", &a2a_config(&llm.uri, port, push));
    let _daemon = h.spawn(&["--config", &cfg]);
    wait_ready(&addr);

    let card = get_card(&addr);
    if card["capabilities"]["pushNotifications"] != true {
        return Outcome::fail(format!(
            "with a2a.push.enabled the card should advertise push notifications: {card}"
        ));
    }
    let sent = send_text(&addr, 1, "take your time", true);
    let Some(task_id) = sent["result"]["task"]["id"].as_str() else {
        return Outcome::fail(format!("a send should return its task: {sent}"));
    };
    let registered = rpc_raw(
        &addr,
        2,
        "CreateTaskPushNotificationConfig",
        json!({"taskId": task_id, "url": hook.url}),
    );
    if registered.get("error").is_some() || registered["result"]["taskId"] != task_id {
        return Outcome::fail(format!("the webhook should be registered: {registered}"));
    }

    let deadline = Instant::now() + Duration::from_secs(20);
    let deliveries = loop {
        let seen = hook.seen.lock().unwrap().clone();
        if seen
            .iter()
            .any(|(_, b)| b["task"]["status"]["state"] == "TASK_STATE_COMPLETED")
        {
            break seen;
        }
        if Instant::now() >= deadline {
            return Outcome::fail(format!(
                "the webhook should be sent the completed task: {seen:?}"
            ));
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    for (headers, body) in &deliveries {
        let members: Vec<&String> = body
            .as_object()
            .map(|o| o.keys().collect())
            .unwrap_or_default();
        if members != ["task"]
            || body["task"]["id"] != task_id
            || header(headers, "content-type") != Some("application/a2a+json")
        {
            return Outcome::fail(format!(
                "a delivery should be a StreamResponse {{task}} for the registered task, as application/a2a+json: {headers:?} {body}"
            ));
        }
    }
    Outcome::pass()
}

/// One delivery, as the receiver saw it: the headers, then the body.
type Delivery = (Headers, Value);

/// A webhook receiver on loopback that records every delivery and answers
/// 204. Its thread outlives the check harmlessly: nothing dials it after.
struct Hook {
    url: String,
    seen: Arc<Mutex<Vec<Delivery>>>,
}

impl Hook {
    fn spawn() -> Hook {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind the webhook receiver");
        let url = format!("http://{}/hook", listener.local_addr().expect("local_addr"));
        let seen: Arc<Mutex<Vec<Delivery>>> = Arc::default();
        let out = Arc::clone(&seen);
        std::thread::spawn(move || {
            for conn in listener.incoming().flatten() {
                conn.set_read_timeout(Some(Duration::from_secs(5))).ok();
                let Ok(mut w) = conn.try_clone() else {
                    continue;
                };
                let mut r = BufReader::new(conn);
                let mut start = String::new();
                if r.read_line(&mut start).unwrap_or(0) == 0 {
                    continue;
                }
                let mut headers = Vec::new();
                let mut len = 0usize;
                loop {
                    let mut l = String::new();
                    if r.read_line(&mut l).unwrap_or(0) == 0 || l.trim().is_empty() {
                        break;
                    }
                    if let Some((k, v)) = l.split_once(':') {
                        let k = k.trim().to_ascii_lowercase();
                        if k == "content-length" {
                            len = v.trim().parse().unwrap_or(0);
                        }
                        headers.push((k, v.trim().to_string()));
                    }
                }
                let mut body = vec![0u8; len];
                r.read_exact(&mut body).ok();
                let v = serde_json::from_slice(&body).unwrap_or(Value::Null);
                out.lock().unwrap().push((headers, v));
                let _ = w.write_all(
                    b"HTTP/1.1 204 No Content\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                );
            }
        });
        Hook { url, seen }
    }
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

/// A playbook whose turn asks a human, then answers once it is told.
fn asking_playbook() -> Value {
    json!({
        "turns": [
            {"tool_calls": [{"name": "ask_human", "arguments": {"question": "Proceed?"}}]},
            {"content": "Proceeded."}
        ]
    })
}

/// Poll `GetTask` until the task reaches `state`; the task, or why not.
fn await_state(addr: &str, task_id: &str, state: &str) -> Result<Value, String> {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let t = rpc(addr, 40, "GetTask", json!({"id": task_id}));
        if t["status"]["state"] == state {
            return Ok(t);
        }
        if Instant::now() >= deadline {
            return Err(format!("the task never reached {state}: {t}"));
        }
        std::thread::sleep(Duration::from_millis(80));
    }
}

/// A daemon running the asking playbook with no extension switched on: the
/// guards, its address, and the id of the task its first send made — with
/// that prompt's `messageId`.
struct Asked {
    _daemon: crate::harness::Daemon,
    _llm: crate::harness::MockLlm,
    _tmp: crate::harness::TempDir,
    addr: String,
    task_id: String,
    prompt_id: Value,
}

fn ask(h: &Harness) -> Asked {
    let tmp = h.tempdir();
    let llm = mock_llm(h, &tmp, &asking_playbook());
    let port = free_port();
    let addr = format!("127.0.0.1:{port}");
    let cfg = write_file(&tmp, "agentd.yaml", &a2a_config(&llm.uri, port, ""));
    let daemon = h.spawn(&["--config", &cfg]);
    wait_ready(&addr);
    let params = text_params("Do the thing", None, true);
    let prompt_id = params["message"]["messageId"].clone();
    let sent = rpc(&addr, 1, "SendMessage", params);
    let task_id = sent["task"]["id"].as_str().expect("task id").to_string();
    Asked {
        _daemon: daemon,
        _llm: llm,
        _tmp: tmp,
        addr,
        task_id,
        prompt_id,
    }
}

/// `input-required` is A2A's own state, so a turn that asks a human waits in
/// it on the core wire — no feed, no extension — and a reply naming the task
/// is the answer.
fn input_required(h: &Harness) -> Outcome {
    let a = ask(h);
    let card = get_card(&a.addr);
    if declared_extensions(&card)
        .iter()
        .any(|u| u == crate::checks::util::EVENTS)
    {
        return Outcome::fail(format!(
            "this daemon enables no feed, so the gate below would not prove the core carries it: {card}"
        ));
    }
    let gated = match await_state(&a.addr, &a.task_id, "TASK_STATE_INPUT_REQUIRED") {
        Ok(t) => t,
        Err(e) => return Outcome::fail(e),
    };
    Outcome::require(
        gated["status"]["message"]["parts"][0]["text"]
            .as_str()
            .is_some_and(|q| q.contains("Proceed?")),
        format!("the gate should carry the question: {gated}"),
    )
    .and(|| {
        rpc(
            &a.addr,
            3,
            "SendMessage",
            text_params("yes", Some(&a.task_id), true),
        );
        match await_state(&a.addr, &a.task_id, "TASK_STATE_COMPLETED") {
            Ok(t) => Outcome::require(
                artifact_answer(&t).contains("Proceeded"),
                format!("the resumed turn's answer should land as the artifact: {t}"),
            ),
            Err(e) => Outcome::fail(format!("the reply should resume the turn: {e}")),
        }
    })
}

/// The task carries its conversation: what the caller said, under the ids it
/// said them with, and what the agent asked on the way — so a peer that
/// reconnects reads the whole exchange from `GetTask`, not only its end.
fn task_history(h: &Harness) -> Outcome {
    let a = ask(h);
    if let Err(e) = await_state(&a.addr, &a.task_id, "TASK_STATE_INPUT_REQUIRED") {
        return Outcome::fail(e);
    }
    let reply = text_params("yes", Some(&a.task_id), true);
    let reply_id = reply["message"]["messageId"].clone();
    rpc(&a.addr, 3, "SendMessage", reply);
    let done = match await_state(&a.addr, &a.task_id, "TASK_STATE_COMPLETED") {
        Ok(t) => t,
        Err(e) => return Outcome::fail(e),
    };
    let history = done["history"].as_array().cloned().unwrap_or_default();

    // In order: the prompt, the question it raised, the answer.
    let position = |pred: &dyn Fn(&Value) -> bool| history.iter().position(pred);
    let prompt = position(&|m| {
        m["messageId"] == a.prompt_id
            && m["role"] == "ROLE_USER"
            && m["parts"][0]["text"] == "Do the thing"
    });
    let question = position(&|m| {
        m["role"] == "ROLE_AGENT"
            && m["parts"][0]["text"]
                .as_str()
                .is_some_and(|t| t.contains("Proceed?"))
    });
    let answer = position(&|m| {
        m["messageId"] == reply_id && m["role"] == "ROLE_USER" && m["parts"][0]["text"] == "yes"
    });
    let in_order =
        matches!((prompt, question, answer), (Some(p), Some(q), Some(r)) if p < q && q < r);
    let current = &done["status"]["message"]["messageId"];
    Outcome::require(
        in_order,
        format!(
            "the history should hold the prompt, the question and the answer, in order: {done}"
        ),
    )
    .and(|| {
        Outcome::require(
            current.is_null() || !history.iter().any(|m| &m["messageId"] == current),
            format!("the current status message is the status, not history: {done}"),
        )
    })
    .and(|| {
        let none = rpc(
            &a.addr,
            4,
            "GetTask",
            json!({"id": a.task_id, "historyLength": 0}),
        );
        let one = rpc(
            &a.addr,
            5,
            "GetTask",
            json!({"id": a.task_id, "historyLength": 1}),
        );
        Outcome::require(
            none["history"].as_array().is_none_or(Vec::is_empty)
                && one["history"].as_array().map(Vec::len) == Some(1)
                && one["history"][0] == history[history.len() - 1],
            format!("historyLength 0 should omit the history and 1 keep the newest: {none} {one}"),
        )
    })
}
