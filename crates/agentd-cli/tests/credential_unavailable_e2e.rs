// SPDX-License-Identifier: AGPL-3.0-only
//! A credential agentd is configured to present and cannot produce fails the
//! requests that need it, unsent, and the log says so once rather than once a
//! call: a model connection whose device login was never made, a turn worker
//! whose AWS source has no keys, an MCP server whose secret file is gone, a
//! header whose secret is not set. Each endpoint here answers whatever reaches
//! it, so a request that went out without its credential would succeed — and
//! fail the test.
#![cfg(all(unix, feature = "oauth"))]

mod common;

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use agentd::mcp::mock_http::Options;
use serde_json::{Value, json};

/// One request as it reached an endpoint: its request line, its
/// `Authorization` header and its body.
type Seen = Arc<Mutex<Vec<(String, Option<String>, String)>>>;

/// One request read off a connection.
struct Request {
    /// The request line and headers, as they arrived.
    head: Vec<u8>,
    body: Vec<u8>,
    line: String,
    auth: Option<String>,
}

/// Read one request off `conn`.
fn read_request(conn: &TcpStream) -> Option<Request> {
    conn.set_read_timeout(Some(Duration::from_secs(10))).ok();
    let mut r = BufReader::new(conn.try_clone().ok()?);
    let mut line = String::new();
    if r.read_line(&mut line).ok()? == 0 {
        return None;
    }
    let mut head = line.clone().into_bytes();
    let (mut auth, mut len) = (None, 0usize);
    loop {
        let mut h = String::new();
        if r.read_line(&mut h).unwrap_or(0) == 0 {
            break;
        }
        head.extend_from_slice(h.as_bytes());
        if h.trim().is_empty() {
            break;
        }
        if let Some((k, v)) = h.split_once(':') {
            if k.trim().eq_ignore_ascii_case("authorization") {
                auth = Some(v.trim().to_string());
            }
            if k.trim().eq_ignore_ascii_case("content-length") {
                len = v.trim().parse().unwrap_or(0);
            }
        }
    }
    let mut body = vec![0u8; len];
    r.read_exact(&mut body).ok();
    Some(Request {
        head,
        body,
        line: line.trim_end().to_string(),
        auth,
    })
}

/// A model endpoint that records every request and answers each with a
/// completion.
fn spawn_model() -> (String, Seen) {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let uri = format!("http://{}", l.local_addr().unwrap());
    let seen: Seen = Arc::default();
    let rec = Arc::clone(&seen);
    std::thread::spawn(move || {
        for mut c in l.incoming().flatten() {
            let Some(req) = read_request(&c) else {
                continue;
            };
            rec.lock().unwrap().push((
                req.line,
                req.auth,
                String::from_utf8_lossy(&req.body).into_owned(),
            ));
            let answer = r#"{"choices":[{"message":{"content":"done"},"finish_reason":"stop"}],"usage":{"prompt_tokens":1,"completion_tokens":1}}"#;
            let _ = write!(
                c,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{answer}",
                answer.len()
            );
        }
    });
    (uri, seen)
}

/// A proxy in front of `upstream` that records each request before passing it
/// on, so a test sees exactly what reached the server.
fn spawn_capture_proxy(upstream: &str) -> (String, Seen) {
    let up = upstream.trim_start_matches("http://").to_string();
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let uri = format!("http://{}", l.local_addr().unwrap());
    let seen: Seen = Arc::default();
    let rec = Arc::clone(&seen);
    std::thread::spawn(move || {
        for c in l.incoming().flatten() {
            let (rec, up) = (Arc::clone(&rec), up.clone());
            std::thread::spawn(move || {
                let Some(req) = read_request(&c) else {
                    return;
                };
                rec.lock().unwrap().push((
                    req.line.clone(),
                    req.auth.clone(),
                    String::from_utf8_lossy(&req.body).into_owned(),
                ));
                let Ok(mut server) = TcpStream::connect(&up) else {
                    return;
                };
                let _ = server
                    .write_all(&req.head)
                    .and_then(|()| server.write_all(&req.body));
                let mut back = c;
                let _ = std::io::copy(&mut server, &mut back);
            });
        }
    });
    (uri, seen)
}

/// The `tools/call` requests `seen` holds, with the `Authorization` each
/// carried.
fn tool_calls(seen: &Seen) -> Vec<Option<String>> {
    seen.lock()
        .unwrap()
        .iter()
        .filter(|(_, _, body)| body.contains("\"tools/call\""))
        .map(|(_, auth, _)| auth.clone())
        .collect()
}

fn events(stderr: &str, name: &str) -> Vec<Value> {
    stderr
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter(|v| v["event"] == name)
        .collect()
}

fn write_config(yaml: &str) -> String {
    let path = common::unique_path("cred-unavailable", "yaml");
    std::fs::write(&path, yaml).unwrap();
    path
}

/// A model connection with a device-grant `auth:` and no login: the job's
/// model calls fail without reaching the model, the error names the login
/// that fixes it, the log warns once for all of them, and the job exits 4.
#[test]
fn a_model_connection_with_no_login_fails_its_calls_unsent_and_warns_once() {
    let (model, seen) = spawn_model();
    let creds = tempfile::tempdir().unwrap();
    let cfg = write_config(&format!(
        "\
         agent:\n  name: no-login\n  instruction: You do the work.\n  preflight: never\n\
         intelligence:\n  endpoints: {model}\n  model: m\n  auth: {{kind: oauth2, grant: device, client_id: agentd, token_url: \"http://127.0.0.1:9/token\", device_authorization_url: \"http://127.0.0.1:9/device\"}}\n\
         store:\n  kind: memory\n\
         workflows:\n  - name: work\n    steps:\n      s: {{kind: once}}\n      a: {{kind: agent, depends_on: [s], instruction: \"one\"}}\n      b: {{kind: agent, depends_on: [s], instruction: \"two\"}}\n      c: {{kind: agent, depends_on: [s], instruction: \"three\"}}\n      f: {{kind: finish, depends_on: [a, b, c]}}\n\
         observability:\n  log_level: info\n"
    ));
    let out = Command::new(env!("CARGO_BIN_EXE_agentd"))
        .args(["--config", &cfg])
        .env("AGENTD_CRED_DIR", creds.path())
        .output()
        .expect("run agentd");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(4), "stderr:\n{stderr}");
    assert!(
        stderr.contains("no cached credential — run `agentd --login intelligence`"),
        "stderr:\n{stderr}"
    );
    // Each of the three turns resolved the model in the supervisor before
    // the first failure came back.
    assert_eq!(
        events(&stderr, "step.turn.spawn").len(),
        3,
        "three turns asked for the credential:\n{stderr}"
    );
    assert!(
        events(&stderr, "step.done").iter().any(|e| e["err"]
            .as_str()
            .is_some_and(|m| m.contains("intelligence request not sent"))),
        "a turn failed on it:\n{stderr}"
    );
    let warned = events(&stderr, "auth.unavailable");
    assert_eq!(warned.len(), 1, "one warning, not one a call:\n{stderr}");
    assert_eq!(warned[0]["target"], "intelligence");
    assert_eq!(warned[0]["level"], "warn");
    assert!(
        seen.lock().unwrap().is_empty(),
        "nothing may reach the model: {:?}",
        seen.lock().unwrap()
    );
    std::fs::remove_file(&cfg).ok();
}

/// Turn workers sign their own model calls with SigV4; with no AWS keys in
/// their environment each call fails, unsent, and the job exits 4. A worker
/// is a process a turn — here one for each of the step's three attempts —
/// and each hands its failure up, so the supervisor says it once for all of
/// them rather than each worker once.
#[test]
fn turn_workers_with_no_aws_credentials_send_no_unsigned_call_and_warn_once() {
    let (model, seen) = spawn_model();
    let cfg = write_config(&format!(
        "\
         agent:\n  name: no-keys\n  instruction: You do the work.\n  preflight: never\n\
         intelligence:\n  endpoints: {model}\n  model: m\n  auth: {{kind: aws, region: us-east-1, service: execute-api, source: env}}\n\
         store:\n  kind: memory\n\
         workflows:\n  - name: work\n    steps:\n      s: {{kind: once}}\n      a: {{kind: agent, depends_on: [s], instruction: \"one\", retry: {{max: 2, backoff: 10ms}}}}\n      f: {{kind: finish, depends_on: [a]}}\n\
         observability:\n  log_level: info\n"
    ));
    let out = Command::new(env!("CARGO_BIN_EXE_agentd"))
        .args(["--config", &cfg])
        .env_remove("AWS_ACCESS_KEY_ID")
        .env_remove("AWS_SECRET_ACCESS_KEY")
        .env_remove("AWS_SESSION_TOKEN")
        .output()
        .expect("run agentd");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(4), "stderr:\n{stderr}");
    let failed = events(&stderr, "step.done")
        .into_iter()
        .filter(|e| {
            e["err"]
                .as_str()
                .is_some_and(|m| m.contains("AWS_ACCESS_KEY_ID is not set"))
        })
        .count();
    assert_eq!(failed, 3, "each attempt's worker failed on it:\n{stderr}");
    let warned = events(&stderr, "auth.unavailable");
    assert_eq!(warned.len(), 1, "one warning for the tree:\n{stderr}");
    assert_eq!(warned[0]["target"], "intelligence");
    assert_eq!(
        warned[0]["comp"], "supervisor",
        "the supervisor says it, not the worker:\n{stderr}"
    );
    assert!(
        seen.lock().unwrap().is_empty(),
        "no unsigned call may reach the model: {:?}",
        seen.lock().unwrap()
    );
    std::fs::remove_file(&cfg).ok();
}

/// A static credential is read from its secret file at each request: while
/// the file is gone a call fails, unsent and naming the file, and the next
/// one after it is back carries the new value.
#[test]
fn an_mcp_server_whose_secret_file_is_gone_fails_the_call_unsent() {
    let upstream = common::mock_in_thread(Options {
        emit: false,
        hold_listen: false,
        ..Options::new("mock://res")
    });
    let (proxy, seen) = spawn_capture_proxy(&upstream);
    let dir = tempfile::tempdir().unwrap();
    let secret = dir.path().join("token");
    std::fs::write(&secret, "tok-static").unwrap();
    let spec = agentd::config::McpServerSpec {
        name: "vault".into(),
        endpoint: proxy,
        auth: Some(agentd::config::AuthSpec {
            kind: "static".into(),
            token: Some(format!("{{{{secret-file:{}}}}}", secret.display())),
            ..Default::default()
        }),
        ..Default::default()
    };
    let mut c = agentd::mcp::from_spec(&spec, Duration::from_secs(10)).expect("built");
    c.initialize().expect("connects with the secret");
    c.call_tool("state.list", Some(json!({})))
        .expect("a call with the secret");

    std::fs::remove_file(&secret).unwrap();
    let e = c
        .call_tool("state.list", Some(json!({})))
        .expect_err("no secret, no call");
    let e = e.to_string();
    assert!(
        e.contains("mcp:vault") && e.contains("cannot read secret file"),
        "{e}"
    );
    assert_eq!(tool_calls(&seen).len(), 1, "the failed call was not sent");

    std::fs::write(&secret, "tok-rotated").unwrap();
    c.call_tool("state.list", Some(json!({})))
        .expect("the file is back: the next call carries it");
    assert_eq!(
        tool_calls(&seen),
        vec![
            Some("Bearer tok-static".to_string()),
            Some("Bearer tok-rotated".to_string())
        ]
    );
    let all = seen.lock().unwrap().clone();
    assert!(
        all.iter().all(|(_, auth, _)| auth.is_some()),
        "every request that reached the server carried the credential: {all:?}"
    );
}

/// A header whose secret is not set is judged at load: the start is refused,
/// naming the reference, before anything is dialed. Validation refused this
/// before the connection learned to refuse it too, so this guards the
/// refusal the operator sees; the connection's own refusal, behind it, is
/// `a_header_whose_secret_does_not_resolve_refuses_the_connection` in the
/// engine's tests.
#[test]
fn a_header_whose_secret_is_not_set_is_refused_at_load() {
    let (model, seen) = spawn_model();
    let cfg = write_config(&format!(
        "\
         agent:\n  name: no-secret\n  instruction: Say hello.\n  preflight: never\n\
         intelligence:\n  endpoints: {model}\n  model: m\n  headers: {{X-Api-Key: \"{{{{secret:CRED_E2E_UNSET_KEY}}}}\"}}\n\
         store:\n  kind: memory\n"
    ));
    let out = Command::new(env!("CARGO_BIN_EXE_agentd"))
        .args(["--config", &cfg])
        .env_remove("CRED_E2E_UNSET_KEY")
        .output()
        .expect("run agentd");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(2), "stderr:\n{stderr}");
    assert!(stderr.contains("CRED_E2E_UNSET_KEY"), "stderr:\n{stderr}");
    assert!(seen.lock().unwrap().is_empty(), "nothing was dialed");
    std::fs::remove_file(&cfg).ok();
}
