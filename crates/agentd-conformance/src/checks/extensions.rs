// SPDX-License-Identifier: AGPL-3.0-only
//! The extension contract (A2A 1.0 §4.6), held from outside.
//!
//! An extension is something a client opts into: the `A2A-Extensions` request
//! header is the only way to activate one, and what an extension adds — a
//! method, a command envelope, facts on a task — is there only when it was
//! activated. Whatever an extension adds, a result is still one of the spec's
//! shapes, so a client that knows no extension can read every answer. And
//! agentd's own facts travel only under the URI of an extension the card
//! declares — never under a bare `agentd` key a client would have to guess the
//! meaning of.

use serde_json::{Value, json};

use crate::checks::events;
use crate::checks::util::{
    Frame, command_uri, declared_extensions, first_frame, free_port, get_card, header, mock_llm,
    post, rpc_body, send_command, send_command_activating, send_command_dressed, stream,
    stream_command, text_params, wait_ready, write_file,
};
use crate::harness::{Daemon, MockLlm, TempDir};
use crate::{Category, Check, Harness, Outcome};

/// The task-annotations extension's URI, as the card declares it.
const ANNOTATIONS: &str = "https://agentd.dev/a2a/ext/task-annotations";

pub fn checks() -> Vec<Check> {
    vec![
        Check {
            id: "extensions/command-requires-activation",
            category: Category::Extensions,
            desc: "a command whose extension the header does not activate is -32602 EXTENSION_NOT_ACTIVATED, one the message does not list is -32602 EXTENSION_NOT_MARKED, and neither runs nor creates a task",
            run: command_requires_activation,
        },
        Check {
            id: "extensions/method-requires-activation",
            category: Category::Extensions,
            desc: "a method an extension declares is -32601 EXTENSION_NOT_ACTIVATED unless the header names that extension's URI exactly — naming the others or a look-alike does not — and is served once it does",
            run: method_requires_activation,
        },
        Check {
            id: "extensions/send-results-are-task-or-message",
            category: Category::Extensions,
            desc: "every SendMessage result is exactly a task or a message, and every SendStreamingMessage frame exactly one of task, message, statusUpdate, artifactUpdate — for a conversation turn, a read command and a task command; a read streams as one message frame",
            run: results_are_spec_shaped,
        },
        Check {
            id: "extensions/no-bare-agentd-metadata",
            category: Category::Extensions,
            desc: "every metadata key in every reply is the URI of an extension the card declares; task annotations appear under theirs exactly when that extension is activated",
            run: no_bare_metadata,
        },
    ]
}

/// A booted daemon and what it runs on; everything stops on drop.
struct Booted {
    _daemon: Daemon,
    _llm: MockLlm,
    _tmp: TempDir,
    addr: String,
}

/// Boot a no-auth loopback daemon (so the caller is the operator and every
/// command is its to send), with the events feed on or off.
fn boot(h: &Harness, feed: bool) -> Booted {
    let tmp = h.tempdir();
    let llm = mock_llm(h, &tmp, &json!({"turns": [{"content": "ok"}]}));
    let port = free_port();
    let addr = format!("127.0.0.1:{port}");
    let cfg = write_file(&tmp, "agentd.yaml", &events::config(&llm.uri, port, feed));
    let daemon = h.spawn(&["--config", &cfg]);
    wait_ready(&addr);
    Booted {
        _daemon: daemon,
        _llm: llm,
        _tmp: tmp,
        addr,
    }
}

/// The `reason` of the refusal's agentd.dev `ErrorInfo`, if it carries one.
fn reason(env: &Value) -> Option<&str> {
    env["error"]["data"].as_array()?.iter().find_map(|d| {
        (d["domain"] == "agentd.dev")
            .then(|| d["reason"].as_str())
            .flatten()
    })
}

/// The number of tasks the listener holds.
fn task_count(addr: &str) -> usize {
    let listed: Value =
        serde_json::from_str(&post(addr, &rpc_body(99, "ListTasks", json!({})), &[]))
            .unwrap_or(Value::Null);
    listed["result"]["tasks"].as_array().map_or(0, Vec::len)
}

fn command_requires_activation(h: &Harness) -> Outcome {
    let booted = boot(h, false);
    let addr = booted.addr.as_str();

    // `admin.pause` is the probe because it would CHANGE something observable:
    // a refusal that still ran it would leave the instance paused.
    for (activate, mark, want) in [
        (false, true, "EXTENSION_NOT_ACTIVATED"),
        (true, false, "EXTENSION_NOT_MARKED"),
    ] {
        let env = send_command_dressed(addr, 1, "admin.pause", activate, mark);
        if env["error"]["code"] != -32602 || reason(&env) != Some(want) {
            return Outcome::fail(format!(
                "a command (activated: {activate}, marked: {mark}) should be -32602 {want}: {env}"
            ));
        }
    }
    let bare = send_command_dressed(addr, 2, "admin.pause", false, false);
    if bare["error"]["code"] != -32602 {
        return Outcome::fail(format!(
            "a command neither activated nor marked should be -32602: {bare}"
        ));
    }

    // Nothing ran: the instance is not paused, and no task was made.
    let status = send_command(addr, 3, "status");
    let doc = &status["result"]["message"]["parts"][0]["data"];
    Outcome::require(
        doc["paused"] == false,
        format!("a refused command must not run — the instance is paused: {status}"),
    )
    .and(|| {
        Outcome::require(
            task_count(addr) == 0,
            "a refused command must not create a task".to_string(),
        )
    })
}

fn method_requires_activation(h: &Harness) -> Outcome {
    let booted = boot(h, true);
    let addr = booted.addr.as_str();
    let card = get_card(addr);
    let declared = declared_extensions(&card);
    let methods: Vec<(String, String)> = card["capabilities"]["extensions"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|e| {
            Some((
                e["uri"].as_str()?.to_string(),
                e["params"]["method"].as_str()?.to_string(),
            ))
        })
        .collect();
    if methods.is_empty() {
        return Outcome::fail(format!(
            "the feed is on, so the card should declare an extension with a method: {card}"
        ));
    }

    for (uri, method) in &methods {
        let body = rpc_body(1, method, json!({}));
        let others = declared
            .iter()
            .filter(|u| *u != uri)
            .cloned()
            .collect::<Vec<_>>()
            .join(", ");
        let look_alike = format!("{uri}/");
        for (what, activation) in [
            ("no A2A-Extensions header", None),
            (
                "the header naming only the other extensions",
                Some(others.as_str()),
            ),
            (
                "the header naming a look-alike URI",
                Some(look_alike.as_str()),
            ),
        ] {
            let headers: Vec<(&str, &str)> = activation
                .filter(|a| !a.is_empty())
                .map(|a| ("A2A-Extensions", a))
                .into_iter()
                .collect();
            let (_, _, env) = first_frame(addr, &body, &headers);
            if env["error"]["code"] != -32601
                || reason(&env) != Some("EXTENSION_NOT_ACTIVATED")
                || !env["error"]["data"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .any(|d| d["metadata"]["extension"] == uri.as_str())
            {
                return Outcome::fail(format!(
                    "{method} with {what} should be -32601 EXTENSION_NOT_ACTIVATED naming {uri}: {env}"
                ));
            }
        }
        let (status, headers, first) = first_frame(addr, &body, &[("A2A-Extensions", uri)]);
        let streamed =
            header(&headers, "content-type").is_some_and(|c| c.starts_with("text/event-stream"));
        if status != 200 || !streamed || first.get("error").is_some() || first["result"].is_null() {
            return Outcome::fail(format!(
                "{method} with {uri} activated should be served: {status} {headers:?} {first}"
            ));
        }
    }
    Outcome::pass()
}

/// The keys of a result object — for a spec result, exactly one.
fn keys(result: &Value) -> Vec<&str> {
    result
        .as_object()
        .map(|o| o.keys().map(String::as_str).collect())
        .unwrap_or_default()
}

/// Every frame's result is exactly one of the `StreamResponse` members.
fn frames_are_stream_responses(what: &str, frames: &[Frame]) -> Result<(), String> {
    if frames.is_empty() {
        return Err(format!("{what}: the stream carried no frames"));
    }
    for f in frames {
        let k = keys(&f.data["result"]);
        if !(k.len() == 1 && ["task", "message", "statusUpdate", "artifactUpdate"].contains(&k[0]))
        {
            return Err(format!(
                "{what}: a frame should be exactly one StreamResponse member: {}",
                f.data
            ));
        }
    }
    Ok(())
}

fn results_are_spec_shaped(h: &Harness) -> Outcome {
    let booted = boot(h, false);
    let addr = booted.addr.as_str();
    let uri = command_uri(&get_card(addr));

    // Unary: a conversation turn and a task command are a task; a read is a
    // message — the reply the command extension marks as its own.
    let turn = post(
        addr,
        &rpc_body(1, "SendMessage", text_params("hi", None, false)),
        &[],
    );
    let turn: Value = serde_json::from_str(&turn).unwrap_or(Value::Null);
    let read = send_command(addr, 2, "status");
    let task_op = send_command(addr, 3, "admin.resume");
    for (what, env, want) in [
        ("a conversation turn", &turn, "task"),
        ("a read command", &read, "message"),
        ("a task command", &task_op, "task"),
    ] {
        if keys(&env["result"]) != [want] {
            return Outcome::fail(format!(
                "{what}'s SendMessage result should be exactly a {want}: {env}"
            ));
        }
    }
    if read["result"]["message"]["extensions"] != json!([uri]) {
        return Outcome::fail(format!(
            "a read's Message should list the command extension: {read}"
        ));
    }

    // Streaming: every frame is one StreamResponse member. A turn and a task
    // command open with their task; a read is its one message, and no more.
    let (_, _, turn_frames) = stream(
        addr,
        &rpc_body(4, "SendStreamingMessage", text_params("hi", None, false)),
        &[],
    );
    let (_, _, op_frames) = stream_command(addr, 5, "admin.resume");
    let (_, _, read_frames) = stream_command(addr, 6, "status");
    for (what, frames) in [
        ("a streamed turn", &turn_frames),
        ("a streamed task command", &op_frames),
        ("a streamed read", &read_frames),
    ] {
        if let Err(e) = frames_are_stream_responses(what, frames) {
            return Outcome::fail(e);
        }
    }
    for (what, frames) in [
        ("a streamed turn", &turn_frames),
        ("a streamed task command", &op_frames),
    ] {
        if frames[0].data["result"].get("task").is_none() {
            return Outcome::fail(format!(
                "{what} should open with its task: {}",
                frames[0].data
            ));
        }
    }
    Outcome::require(
        read_frames.len() == 1
            && read_frames[0].data["result"].get("message").is_some()
            && read_frames[0].id.is_none()
            && read_frames[0].data["id"] == 6,
        format!(
            "a streamed read should be exactly one message frame, carrying the request id and no SSE id: {read_frames:?}"
        ),
    )
}

/// Every key of every `metadata` object anywhere in `v`.
fn metadata_keys(v: &Value, out: &mut Vec<String>) {
    match v {
        Value::Object(o) => {
            for (k, child) in o {
                if k == "metadata"
                    && let Some(m) = child.as_object()
                {
                    out.extend(m.keys().cloned());
                }
                metadata_keys(child, out);
            }
        }
        Value::Array(a) => a.iter().for_each(|x| metadata_keys(x, out)),
        _ => {}
    }
}

fn no_bare_metadata(h: &Harness) -> Outcome {
    let booted = boot(h, false);
    let addr = booted.addr.as_str();
    let card = get_card(addr);
    let declared = declared_extensions(&card);
    if !declared.iter().any(|u| u == ANNOTATIONS) {
        return Outcome::fail(format!(
            "the card should declare the task-annotations extension: {card}"
        ));
    }
    let uri = command_uri(&card);

    // The same replies twice: once activating task-annotations, once not.
    // A task exists before the reads, so GetTask and ListTasks have one to
    // annotate.
    let replies = |activate: bool| -> Vec<(String, Value)> {
        let with_command = format!("{uri}, {ANNOTATIONS}");
        let turn_h: &[(&str, &str)] = if activate {
            &[("A2A-Extensions", ANNOTATIONS)]
        } else {
            &[]
        };
        let json_of = |s: String| serde_json::from_str(&s).unwrap_or(Value::Null);
        let sent = json_of(post(
            addr,
            &rpc_body(1, "SendMessage", text_params("hi", None, false)),
            turn_h,
        ));
        let id = sent["result"]["task"]["id"].clone();
        let got = json_of(post(
            addr,
            &rpc_body(2, "GetTask", json!({"id": id})),
            turn_h,
        ));
        let listed = json_of(post(addr, &rpc_body(3, "ListTasks", json!({})), turn_h));
        let (_, _, frames) = stream(
            addr,
            &rpc_body(4, "SendStreamingMessage", text_params("hi", None, false)),
            turn_h,
        );
        let activation = if activate {
            with_command.as_str()
        } else {
            uri.as_str()
        };
        let op = send_command_activating(addr, 5, "admin.resume", activation);
        let mut out = vec![
            ("SendMessage".to_string(), sent),
            ("GetTask".to_string(), got),
            ("ListTasks".to_string(), listed),
            ("a task command".to_string(), op),
        ];
        out.extend(
            frames
                .into_iter()
                .map(|f| ("a SendStreamingMessage frame".to_string(), f.data)),
        );
        out
    };

    for activate in [true, false] {
        let mut annotated = 0;
        for (what, reply) in replies(activate) {
            if reply.get("error").is_some() || reply.is_null() {
                return Outcome::fail(format!("{what} should be answered: {reply}"));
            }
            let mut found = Vec::new();
            metadata_keys(&reply, &mut found);
            if let Some(stray) = found.iter().find(|k| !declared.contains(k)) {
                return Outcome::fail(format!(
                    "{what}: metadata key {stray:?} is not the URI of a declared extension: {reply}"
                ));
            }
            annotated += found.iter().filter(|k| *k == ANNOTATIONS).count();
        }
        if activate && annotated == 0 {
            return Outcome::fail(
                "with task-annotations activated, the tasks should carry annotations under its URI"
                    .to_string(),
            );
        }
        if !activate && annotated > 0 {
            return Outcome::fail(
                "without task-annotations activated, no task may carry its annotations".to_string(),
            );
        }
    }
    Outcome::pass()
}
