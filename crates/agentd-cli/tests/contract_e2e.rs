// SPDX-License-Identifier: AGPL-3.0-only
//! **What agentd.dev publishes for each extension URI is what the binary
//! speaks.**
//!
//! A2A's extension guidance says a third-party extension's specification
//! should be hosted at its URI, so each URI agentd declares is also a page on
//! agentd.dev with a schema bundle beside it. Those files are generated from
//! the binary and committed (the site build has no Rust toolchain), which
//! makes them a copy — and a copy is only worth publishing while it cannot
//! drift. Two claims hold it:
//!
//! 1. the committed registry and bundles are exactly what the binary prints,
//!    each bundle is complete for the vocabulary it covers and every `$ref`
//!    in it resolves, and every hand-written golden example beside a bundle
//!    validates against it;
//! 2. a running daemon's feed frames, its read replies, its command results
//!    and the task annotations it writes all validate against the COMMITTED
//!    files — the ones a peer downloads — not merely against the functions
//!    that generate them.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use serde_json::{Value, json};

/// The workspace root: where `web/` and `docs/` live.
fn repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn read_json(rel: &str) -> Value {
    let path = repo().join(rel);
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{rel}: {e}"));
    serde_json::from_str(&text).unwrap_or_else(|e| panic!("{rel}: {e}"))
}

/// Run the binary from the workspace root, as `scripts/gen-schemas.sh` does.
fn agentd(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_agentd"))
        .args(args)
        .current_dir(repo())
        .output()
        .expect("run agentd")
}

fn stdout_json(args: &[&str]) -> Value {
    let out = agentd(args);
    assert!(
        out.status.success(),
        "agentd {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).unwrap_or_else(|e| panic!("agentd {args:?}: {e}"))
}

/// The last segment of an entry's path: the name `--extension-schema` takes.
fn name_of(entry: &Value) -> &str {
    entry["path"].as_str().unwrap().rsplit('/').next().unwrap()
}

/// `bundle`, re-rooted at the JSON pointer `at`: the same `$defs`, so every
/// `$ref` inside the sub-schema resolves as it does in the published file.
#[cfg(all(unix, feature = "a2a"))]
fn at(bundle: &Value, at: &str) -> Value {
    json!({"$defs": bundle["$defs"], "$ref": format!("#{at}")})
}

fn validate(schema: &Value, value: &Value, what: &str) {
    if let Err(errs) = agentd::jsonschema::validate(schema, value) {
        panic!("{what} misses the published schema: {errs:?}\n{value}");
    }
}

/// Every `$ref` in `v`, wherever it sits.
fn refs(v: &Value, out: &mut Vec<String>) {
    match v {
        Value::Object(m) => {
            if let Some(Value::String(r)) = m.get("$ref") {
                out.push(r.clone());
            }
            m.values().for_each(|x| refs(x, out));
        }
        Value::Array(a) => a.iter().for_each(|x| refs(x, out)),
        _ => {}
    }
}

/// What each bundle must cover to be complete, from the tables the listener
/// holds requests and pushes to. An extension with no rule here fails, so a
/// new one cannot be published without saying what "complete" means for it.
fn assert_complete(name: &str, bundle: &Value) {
    use agentd::runtime::surface::events::FeedKind;
    use agentd::runtime::surface::{INSTANCE_OPS, static_vocabulary};
    let defs = &bundle["$defs"];
    match name {
        "command" => {
            for op in static_vocabulary() {
                let entry = &defs["ops"][op];
                for key in ["reply", "description", "args", "result"] {
                    assert!(entry.get(key).is_some(), "command: {op} has no {key}");
                }
                assert!(
                    defs["envelopes"]["$defs"].get(op).is_some(),
                    "command: {op}"
                );
            }
            for m in INSTANCE_OPS {
                assert!(
                    defs["reserved"].get(m.name).is_some(),
                    "command: {}",
                    m.name
                );
                assert!(
                    defs["envelopes"]["$defs"].get(m.name).is_some(),
                    "{}",
                    m.name
                );
            }
        }
        "events" => {
            let kinds: Vec<&str> = FeedKind::ALL.iter().map(|k| k.as_str()).collect();
            for k in &kinds {
                assert!(defs["kinds"]["$defs"].get(*k).is_some(), "events: {k}");
            }
            assert_eq!(defs["event"]["properties"]["kind"]["enum"], json!(kinds));
            for frame in ["params", "hello", "event", "goodbye"] {
                assert!(defs.get(frame).is_some(), "events: {frame}");
            }
        }
        "task-annotations" => {
            for key in ["link", "created", "statusHistory"] {
                assert!(
                    bundle["required"].as_array().unwrap().contains(&json!(key)),
                    "task-annotations: {key}"
                );
            }
        }
        other => panic!("{other}: say what a complete bundle covers"),
    }
}

/// The committed registry is the binary's; each extension's committed bundle
/// is the binary's, carries its own address as `$id`, covers its whole
/// vocabulary and resolves every `$ref`; every golden example validates; the
/// binding has a spec and no bundle; and nothing stale sits under the
/// published tree.
#[test]
fn schemas_are_complete_and_fixtures_validate() {
    let registry = stdout_json(&["--extensions"]);
    assert_eq!(
        registry,
        read_json("web/lib/extensions.json"),
        "web/lib/extensions.json is stale: run ./scripts/gen-schemas.sh"
    );
    let entries = registry.as_array().unwrap();
    let names: Vec<&str> = entries
        .iter()
        .filter(|e| !e["schema"].is_null())
        .map(name_of)
        .collect();
    assert!(names.len() >= 3, "{names:?}");

    for entry in entries {
        let name = name_of(entry);
        let path = entry["path"].as_str().unwrap();
        assert!(
            repo().join(entry["spec"].as_str().unwrap()).is_file(),
            "{entry}"
        );
        let published = repo().join("web/public").join(path).join("schema.json");
        if entry["schema"].is_null() {
            // A binding: its page only. Asking for its bundle is asking for
            // an extension that does not exist.
            assert!(!published.exists(), "{path} has a bundle");
            let out = agentd(&["--extension-schema", name]);
            assert_eq!(out.status.code(), Some(2), "{name}");
            continue;
        }
        let bundle = stdout_json(&["--extension-schema", name]);
        assert_eq!(
            bundle,
            read_json(&format!("web/public/{path}/schema.json")),
            "web/public/{path}/schema.json is stale: run ./scripts/gen-schemas.sh"
        );
        assert_eq!(bundle["$id"], entry["schema"], "{name}");
        assert_eq!(
            bundle["$schema"],
            "https://json-schema.org/draft/2020-12/schema"
        );
        assert_complete(name, &bundle);
        let mut all = Vec::new();
        refs(&bundle, &mut all);
        for r in all {
            let pointer = r.strip_prefix('#').expect("a local $ref");
            assert!(
                bundle.pointer(pointer).is_some(),
                "{name}: {r} resolves to nothing"
            );
        }

        let dir = repo().join("web/public").join(path).join("examples");
        let mut examples: Vec<PathBuf> = std::fs::read_dir(&dir)
            .unwrap_or_else(|e| panic!("{}: {e}", dir.display()))
            .map(|f| f.unwrap().path())
            .collect();
        examples.sort();
        assert!(!examples.is_empty(), "{name} publishes no example");
        for f in examples {
            let text = std::fs::read_to_string(&f).unwrap();
            let v: Value = serde_json::from_str(&text).unwrap();
            validate(&bundle, &v, &f.display().to_string());
        }
    }

    // An unknown name is a usage error that lists what does exist.
    let out = agentd(&["--extension-schema", "nope"]);
    assert_eq!(out.status.code(), Some(2));
    let err = String::from_utf8_lossy(&out.stderr);
    for name in &names {
        assert!(err.contains(name), "{err}");
    }

    // Nothing under the published tree belongs to no entry: a directory the
    // registry no longer names would keep serving a retired contract.
    let paths: Vec<&str> = entries
        .iter()
        .map(|e| e["path"].as_str().unwrap())
        .collect();
    for kind in ["ext", "binding"] {
        let Ok(dir) = std::fs::read_dir(repo().join("web/public/a2a").join(kind)) else {
            continue;
        };
        for d in dir {
            let d = d.unwrap().file_name().into_string().unwrap();
            let path = format!("a2a/{kind}/{d}");
            assert!(paths.contains(&path.as_str()), "web/public/{path} is stale");
        }
    }
}

#[cfg(all(unix, feature = "a2a"))]
mod common;

/// A running daemon, over its real listener, answered in documents the
/// published files accept: every frame of the feed (and the task annotations
/// its `task` events carry), every read an instance with this configuration
/// serves, and the result of each command it ran.
///
/// "Reachable" is what one instance can be driven to here: a conversation, a
/// workflow run, retention evicting a task, the instance paused and resumed,
/// a setting moved, a device sign-in asked for and approved, and the audit
/// mirror of all of it. The kinds that need a subagent or a child process
/// (`subagent`, `child` and their departures) are covered by the unit test
/// that validates a captured sample of every kind.
#[cfg(all(unix, feature = "a2a"))]
#[test]
fn every_reachable_feed_kind_and_read_reply_matches_the_published_schema() {
    use std::collections::BTreeSet;
    use std::io::{Read, Write};
    use std::process::Stdio;
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    use agentd::runtime::surface::{
        EVENTS_EXTENSION, OPS, Reply, TASK_ANNOTATIONS_EXTENSION, static_vocabulary,
    };
    use common::SendMessage;

    const OPS_TOKEN: &str = "contract-e2e-operator-token";
    let events = read_json("web/public/a2a/ext/events/schema.json");
    let command = read_json("web/public/a2a/ext/command/schema.json");
    let annotations = read_json("web/public/a2a/ext/task-annotations/schema.json");

    struct Daemon(std::process::Child, String, String);
    impl Drop for Daemon {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
            let _ = std::fs::remove_file(&self.1);
            let _ = std::fs::remove_file(&self.2);
        }
    }
    let mut spawned = None;
    for _ in 0..5 {
        let port = common::free_port();
        let cfg = common::unique_path("contract-e2e", "yaml");
        std::fs::write(
            &cfg,
            format!(
                "\
                 agent:\n  name: contract-e2e\n  instruction: You are a helpful test agent.\n  preflight: never\n\
                 intelligence:\n  endpoints: \"mock:final\"\n  model: mock\n\
                 store:\n  kind: memory\n  retention:\n    tasks:\n      keep_last: 1\n\
                 a2a:\n  listen: http://127.0.0.1:{port}\n  bearer: \"{{{{secret:AGENTD_CONTRACT_OPS}}}}\"\n\
                 \x20 device_grant:\n    enabled: true\n    scopes: [user, operator]\n\
                 \x20 events:\n    enabled: true\n\
                 \x20 introspection:\n    enabled: true\n\
                 workflows:\n\
                 \x20 - name: greet\n    steps:\n      s: {{kind: manual}}\n      f: {{kind: finish, depends_on: [s], output: \"done\"}}\n\
                 lifecycle:\n  run_until: drained\n\
                 observability:\n  log_level: info\n  audit:\n    sink: [log]\n"
            ),
        )
        .unwrap();
        let log = common::unique_path("contract-e2e", "log");
        let child = Command::new(env!("CARGO_BIN_EXE_agentd"))
            .args(["--config", &cfg])
            .env("AGENTD_CONTRACT_OPS", OPS_TOKEN)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::from(std::fs::File::create(&log).unwrap()))
            .spawn()
            .expect("spawn agentd");
        let d = Daemon(child, cfg, log);
        if let Some(addr) = common::try_a2a_bound(&d.2, Duration::from_secs(15)) {
            spawned = Some((d, addr));
            break;
        }
    }
    let (d, addr) = spawned.expect("the daemon never bound its listener (5 attempts)");

    // The feed, as the operator, with task annotations activated so a `task`
    // event carries them.
    let frames: Arc<Mutex<Vec<Value>>> = Arc::default();
    {
        let sink = Arc::clone(&frames);
        let addr = addr.clone();
        std::thread::spawn(move || {
            let body = common::rpc_body(7, common::feed_method(), json!({"fromSeq": 0}));
            let activate = format!("{EVENTS_EXTENSION}, {TASK_ANNOTATIONS_EXTENSION}");
            let auth = format!("Bearer {OPS_TOKEN}");
            let headers = [
                ("A2A-Extensions", activate.as_str()),
                ("Authorization", &auth),
            ];
            let mut reader = common::a2a_open(&addr, &body, &headers, Duration::from_secs(60));
            common::read_frames(&mut reader, |v| {
                sink.lock().unwrap().push(v["result"].clone());
                true
            });
        });
    }
    let wait_for = |what: &str, pred: &dyn Fn(&[Value]) -> bool| {
        let deadline = Instant::now() + Duration::from_secs(15);
        while !pred(&frames.lock().unwrap()) {
            assert!(
                Instant::now() < deadline,
                "the feed never carried {what}: {:?}\n{}",
                frames.lock().unwrap(),
                std::fs::read_to_string(&d.2).unwrap_or_default()
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    };
    wait_for("hello", &|f| f.iter().any(|v| v.get("hello").is_some()));

    // A command whose result is checked against its op's published result:
    // a read's Message document, or a task's result artifact.
    let run = |op: &str, args: Value| -> Value {
        let r = SendMessage::command(op, args)
            .bearer(OPS_TOKEN)
            .result(&addr);
        let spec = OPS.iter().find(|s| s.name == op).unwrap();
        let doc = match spec.reply {
            Reply::Message => {
                assert!(r.get("task").is_none(), "{op} made a task: {r}");
                r["message"]["parts"][0]["data"].clone()
            }
            Reply::Task => {
                let data = &r["task"]["artifacts"][0]["parts"][0]["data"];
                if data.is_null() {
                    return r;
                }
                data.clone()
            }
        };
        validate(
            &at(&command, &format!("/$defs/ops/{op}/result")),
            &doc,
            &format!("{op}'s result"),
        );
        r
    };

    // Work: a conversation, a workflow run, a setting moved, the instance
    // paused and resumed.
    SendMessage::text("hello")
        .context("c1")
        .bearer(OPS_TOKEN)
        .result(&addr);
    run("workflow.run", json!({"workflow": "greet"}));
    wait_for("the run finishing", &|f| {
        f.iter().any(|v| {
            v["event"]["kind"] == "run"
                && v["event"]["data"]["workflow"] == "greet"
                && v["event"]["data"]["finished"].is_u64()
        })
    });
    let run_id = frames
        .lock()
        .unwrap()
        .iter()
        .find(|v| v["event"]["kind"] == "run")
        .and_then(|v| v["event"]["data"]["id"].as_str().map(str::to_string))
        .expect("a run event names the run");
    run(
        "admin.set",
        json!({"path": "agent.approval", "value": "accept"}),
    );
    run("admin.pause", json!({"reason": "contract"}));
    run("admin.resume", json!({}));

    // A device sign-in, asked for and approved: the `auth` events.
    let form = "client_id=contract-e2e";
    let mut s = std::net::TcpStream::connect(&addr).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(10))).ok();
    write!(
        s,
        "POST /oauth2/device_authorization HTTP/1.1\r\nHost: x\r\n\
         Content-Type: application/x-www-form-urlencoded\r\nContent-Length: {}\r\n\
         Connection: close\r\n\r\n{form}",
        form.len()
    )
    .unwrap();
    let mut raw = String::new();
    s.read_to_string(&mut raw).ok();
    let code: Value = serde_json::from_str(raw.split_once("\r\n\r\n").unwrap().1).unwrap();
    let user_code = code["user_code"].as_str().expect("a user code").to_string();

    // Every read this instance serves, with arguments naming what exists.
    // A read with no object to name here is listed, so a new read op must
    // be given arguments before this passes.
    let mut read = BTreeSet::new();
    for op in static_vocabulary() {
        let spec = OPS.iter().find(|s| s.name == op).unwrap();
        if spec.reply != Reply::Message {
            continue;
        }
        let args = match op {
            "status" | "config" | "debug.events" | "auth.device.pending" | "auth.sessions" => {
                json!({})
            }
            "workflow.status" | "run.get" => json!({"run": run_id}),
            "plan.get" | "conversation.get" => json!({"id": "c1"}),
            // Nothing here spawns a subagent; the subagent reads are held
            // to their schemas by the tool schemas they publish verbatim.
            "subagent.status" | "subagent.get" => continue,
            other => panic!("the read {other} needs arguments in this test"),
        };
        run(op, args);
        read.insert(op);
    }
    assert!(read.len() >= 9, "{read:?}");
    run(
        "auth.device.approve",
        json!({"user_code": user_code, "as": "contract-device", "scope": "user"}),
    );

    // Every kind this instance was driven to push.
    let reachable = [
        "task",
        "task.removed",
        "run",
        "step",
        "conversation",
        "activity",
        "status",
        "lifecycle",
        "config",
        "audit",
        "auth",
    ];
    for kind in reachable {
        wait_for(kind, &|f| f.iter().any(|v| v["event"]["kind"] == kind));
    }

    // Every frame, against the published bundle; every task event's
    // annotations, against theirs.
    let frames = frames.lock().unwrap().clone();
    let mut seen = BTreeSet::new();
    for f in &frames {
        validate(&events, f, "a feed frame");
        if f["event"]["kind"] == "task" {
            let a = &f["event"]["data"]["task"]["metadata"][TASK_ANNOTATIONS_EXTENSION];
            assert!(a.is_object(), "an annotated subscriber's task event: {f}");
            validate(&annotations, a, "a task's annotations");
        }
        if let Some(k) = f["event"]["kind"].as_str() {
            seen.insert(k.to_string());
        }
    }
    for kind in reachable {
        assert!(seen.contains(kind), "{kind}: {seen:?}");
    }
}
