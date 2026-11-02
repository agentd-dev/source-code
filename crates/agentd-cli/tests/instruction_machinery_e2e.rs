// SPDX-License-Identifier: AGPL-3.0-only
//! **A served document's machinery applies, end to end.** The real binary
//! follows an `instruction://…` document on the in-tree mock registry. The
//! document declares MCP servers, inline skills and a workflow that calls a
//! declared server — which config load cannot fold, because MCP is not
//! connected yet. The first read at boot runs the staged reload that folds
//! it; a re-delivered version whose machinery moved applies at once under
//! `agent.instruction.machinery: live` (the default), or waits for SIGHUP
//! under `reload`.
//!
//! The arbiter of "callable" is the SERVER's log: each declared server's
//! mock writes `MOCK_CALL` for every call of its tools that reached it.
#![cfg(unix)]

mod common;

use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

const URI: &str = "instruction://ins_live@stable";
const WAIT: Duration = Duration::from_secs(30);

struct Daemon {
    child: Child,
    cfg_path: String,
    err_path: String,
}

impl Daemon {
    fn log(&self) -> String {
        std::fs::read_to_string(&self.err_path).unwrap_or_default()
    }

    fn wait_for(&mut self, what: &str, cond: impl Fn(&str) -> bool) -> String {
        let end = Instant::now() + WAIT;
        loop {
            let log = self.log();
            if cond(&log) {
                return log;
            }
            if Instant::now() > end {
                panic!("timed out waiting for {what}:\n{log}");
            }
            if let Ok(Some(status)) = self.child.try_wait() {
                panic!("the daemon exited ({status}) before {what}:\n{log}");
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    #[cfg(feature = "hot-reload")]
    fn sighup(&self) {
        unsafe { libc::kill(self.child.id() as i32, libc::SIGHUP) };
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_file(&self.cfg_path);
        let _ = std::fs::remove_file(&self.err_path);
    }
}

fn boot(cfg: Value) -> Daemon {
    let cfg_path = common::unique_path("machinery", "json");
    std::fs::write(&cfg_path, serde_json::to_vec(&cfg).unwrap()).unwrap();
    let err_path = common::unique_path("machinery", "log");
    let errf = std::fs::File::create(&err_path).unwrap();
    let child = Command::new(env!("CARGO_BIN_EXE_agentd"))
        .args(["-c", &cfg_path])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(errf)
        .spawn()
        .unwrap();
    Daemon {
        child,
        cfg_path,
        err_path,
    }
}

/// The operator's configuration: the registry, the served instruction (re-read
/// every second), and a quiet schedule that keeps the daemon alive before the
/// document's machinery has joined.
fn config(registry: &common::MockMcp, machinery: Option<&str>) -> Value {
    let mut instruction = json!({"mcp": {"server": "registry", "resource": URI}, "refresh": "1s"});
    if let Some(m) = machinery {
        instruction["machinery"] = json!(m);
    }
    json!({
        "agent": {"name": "machinery", "preflight": "never",
                  "instruction": instruction,
                  "document_capabilities": ["compute"]},
        "mcp": {"servers": [{"name": "registry", "endpoint": format!("{}/mcp", registry.uri())}]},
        "intelligence": {"endpoints": ["http://127.0.0.1:1/v1"], "model": "mock"},
        "store": {"kind": "memory"},
        "workflows": [{"name": "tick", "steps": {
            "every": {"kind": "schedule", "every": "1h"},
            "done": {"kind": "finish", "depends_on": ["every"], "status": "completed"}
        }}],
    })
}

/// A server, a skill and a workflow calling the server every 200ms.
fn block(id: &str, mock: &common::MockMcp, skill: &str) -> String {
    format!(
        ":::mcp{{id={id} title=\"{id}\" src=\"{}/mcp\"}}\nA served server.\n:::\n\n\
         :::skill{{id={skill} title=\"{skill}\" trigger=\"always\"}}\nBe brief.\n:::\n\n\
         :::workflow{{id=ping-{id} dialect=\"instruction/workflow\"}}\n\
         name: ping-{id}\nsteps:\n  s: {{kind: schedule, every: 200ms}}\n  \
         c: {{kind: mcp.tool, depends_on: [s], server: {id}, tool: mock.ops}}\n  \
         f: {{kind: finish, depends_on: [c], status: completed}}\n:::\n\n",
        mock.uri()
    )
}

fn document(blocks: &[String]) -> String {
    format!(
        "---\nspec: \"1\"\n---\n# Machinery agent\n\nServe the mock.\n\n{}",
        blocks.concat()
    )
}

fn serve(registry: &common::MockMcp, text: &str) {
    registry.control("mock.serve_document", json!({"text": text}));
}

fn events(log: &str, name: &str) -> Vec<Value> {
    log.lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter(|v| v["event"] == name)
        .collect()
}

fn names(v: &Value) -> Vec<String> {
    v.as_array()
        .into_iter()
        .flatten()
        .filter_map(|n| n.as_str().map(str::to_string))
        .collect()
}

/// `MOCK_CALL`s of `mock.ops` that reached `mock`.
fn calls(mock: &common::MockMcp) -> usize {
    mock.log()
        .lines()
        .filter(|l| l.starts_with("MOCK_CALL ") && l.contains("\"name\":\"mock.ops\""))
        .count()
}

fn wait_calls(mock: &common::MockMcp, at_least: usize, what: &str, d: &Daemon) {
    let end = Instant::now() + WAIT;
    while calls(mock) < at_least {
        assert!(
            Instant::now() < end,
            "timed out waiting for {what}:\n{}",
            d.log()
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// `skills.discovered` from the instruction, its latest line's names, sorted.
fn inline_skills(log: &str) -> Vec<String> {
    events(log, "skills.discovered")
        .into_iter()
        .rfind(|e| e["origin"] == "instruction")
        .map(|e| {
            e["skills"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|s| s["name"].as_str().map(str::to_string))
                .collect::<std::collections::BTreeSet<_>>()
                .into_iter()
                .collect()
        })
        .unwrap_or_default()
}

/// The `n`th `instruction.machinery.applied`, once it is logged.
fn applied(d: &mut Daemon, n: usize) -> Value {
    let log = d.wait_for("instruction.machinery.applied", |l| {
        events(l, "instruction.machinery.applied").len() > n
    });
    events(&log, "instruction.machinery.applied")[n].clone()
}

/// Boot on a document declaring server `one`: the first read folds it, so
/// `one` is dialed and its tools called, and its skill registered.
fn booted(machinery: Option<&str>) -> (Daemon, common::MockMcp, common::MockMcp) {
    let registry = common::spawn_mock_mcp("mock://unused", false);
    let one = common::spawn_mock_mcp("mock://unused", false);
    serve(&registry, &document(&[block("one", &one, "tone")]));
    let mut d = boot(config(&registry, machinery));
    let first = applied(&mut d, 0);
    assert_eq!(names(&first["servers_added"]), ["one"], "{first}");
    assert_eq!(names(&first["skills_added"]), ["tone"], "{first}");
    assert_eq!(first["version"], "ver_live_1", "{first}");
    let log = d.log();
    assert!(
        events(&log, "config.reloaded")
            .iter()
            .any(|e| e["trigger"] == "instruction"),
        "the first read runs the staged reload:\n{log}"
    );
    assert!(
        events(&log, "mcp.connect")
            .iter()
            .any(|e| e["server"] == "one"),
        "{log}"
    );
    assert_eq!(inline_skills(&log), ["tone"], "{log}");
    wait_calls(&one, 1, "server one's tool called", &d);
    (d, registry, one)
}

/// (a) at boot, then (b) live: a version that adds server `two` and a skill
/// applies without a restart, and the one that takes them away removes them.
#[test]
fn a_served_documents_machinery_applies_at_boot_and_live() {
    let (mut d, registry, one) = booted(None);
    let two = common::spawn_mock_mcp("mock://unused", false);
    serve(
        &registry,
        &document(&[block("one", &one, "tone"), block("two", &two, "second")]),
    );
    let added = applied(&mut d, 1);
    assert_eq!(names(&added["servers_added"]), ["two"], "{added}");
    assert_eq!(names(&added["skills_added"]), ["second"], "{added}");
    assert!(names(&added["servers_removed"]).is_empty(), "{added}");
    assert_eq!(added["version"], "ver_live_2");
    wait_calls(&two, 1, "server two's tool called, no restart", &d);
    let log = d.log();
    assert_eq!(inline_skills(&log), ["second", "tone"], "{log}");
    assert!(
        events(&log, "instruction.machinery.pending").is_empty(),
        "{log}"
    );

    serve(&registry, &document(&[block("one", &one, "tone")]));
    let removed = applied(&mut d, 2);
    assert_eq!(names(&removed["servers_removed"]), ["two"], "{removed}");
    assert_eq!(names(&removed["skills_removed"]), ["second"], "{removed}");
    let log = d.wait_for("server two disconnected", |l| {
        events(l, "mcp.disconnect")
            .iter()
            .any(|e| e["server"] == "two")
    });
    assert_eq!(inline_skills(&log), ["tone"], "{log}");
    // Its workflow went with it: nothing calls server two any more.
    std::thread::sleep(Duration::from_millis(400));
    let settled = calls(&two);
    std::thread::sleep(Duration::from_millis(800));
    assert_eq!(
        calls(&two),
        settled,
        "server two is still called:\n{}",
        d.log()
    );
    // …while server one, kept, still is.
    let before = calls(&one);
    wait_calls(&one, before + 1, "server one still called", &d);
}

/// A version that changes only the prose is adopted as text, with no reload.
#[test]
fn a_text_only_edit_runs_no_reload() {
    let (mut d, registry, one) = booted(None);
    let text = document(&[block("one", &one, "tone")]).replace("Serve the mock.", "Serve it well.");
    serve(&registry, &text);
    d.wait_for("the edit adopted", |l| {
        events(l, "instruction.applied")
            .iter()
            .any(|e| e["new_version_id"] == "ver_live_2")
    });
    std::thread::sleep(Duration::from_millis(1500));
    let log = d.log();
    assert_eq!(
        events(&log, "instruction.machinery.applied").len(),
        1,
        "{log}"
    );
    assert_eq!(events(&log, "config.reloaded").len(), 1, "{log}");
}

/// (c) `machinery: reload`: the same publish is pending — said once — and
/// nothing changes until SIGHUP, which applies it.
#[cfg(feature = "hot-reload")]
#[test]
fn under_reload_a_changed_machinery_waits_for_sighup() {
    let (mut d, registry, one) = booted(Some("reload"));
    let two = common::spawn_mock_mcp("mock://unused", false);
    serve(
        &registry,
        &document(&[block("one", &one, "tone"), block("two", &two, "second")]),
    );
    let log = d.wait_for("instruction.machinery.pending", |l| {
        !events(l, "instruction.machinery.pending").is_empty()
    });
    let pending = &events(&log, "instruction.machinery.pending")[0];
    assert_eq!(pending["version_id"], "ver_live_2", "{pending}");
    // Several re-reads later: said once, and nothing applied.
    std::thread::sleep(Duration::from_millis(2500));
    let log = d.log();
    assert_eq!(
        events(&log, "instruction.machinery.pending").len(),
        1,
        "{log}"
    );
    assert_eq!(
        events(&log, "instruction.machinery.applied").len(),
        1,
        "{log}"
    );
    assert_eq!(calls(&two), 0, "{log}");
    assert_eq!(inline_skills(&log), ["tone"], "{log}");

    d.sighup();
    let added = applied(&mut d, 1);
    assert_eq!(names(&added["servers_added"]), ["two"], "{added}");
    assert_eq!(names(&added["skills_added"]), ["second"], "{added}");
    let log = d.log();
    assert!(
        events(&log, "config.reloaded")
            .iter()
            .any(|e| e["trigger"] == "sighup"),
        "{log}"
    );
    wait_calls(&two, 1, "server two's tool called after SIGHUP", &d);
}

/// Serve `body` at `path` on a loopback port, 404 for every other path;
/// returns the server's origin.
fn serve_skill(path: &'static str, body: &'static str) -> String {
    use std::io::{Read, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for mut conn in listener.incoming().flatten() {
            let mut buf = [0u8; 4096];
            let n = conn.read(&mut buf).unwrap_or(0);
            let head = String::from_utf8_lossy(&buf[..n]);
            let asked = head.split_whitespace().nth(1).unwrap_or_default();
            let (status, body) = if asked == path {
                ("200 OK", body)
            } else {
                ("404 Not Found", "")
            };
            let _ = write!(
                conn,
                "HTTP/1.1 {status}\r\nContent-Type: text/markdown\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
        }
    });
    format!("http://127.0.0.1:{port}")
}

/// A `::skill{src}` naming a SKILL.md is fetched when the document's
/// machinery applies: registered under the document's id, logged as
/// `skills.fetched` with its digest, and counted as added. One whose fetch
/// fails is a warning and is left out; the rest of the document applies.
#[test]
fn a_skill_named_by_src_is_fetched_and_one_that_fails_is_left_out() {
    const SKILL: &str = "---\nname: house-style\ndescription: Write in the house style.\n---\n# House style\n\nShort sentences.\n";
    let origin = serve_skill("/skills/house-style/SKILL.md", SKILL);
    let registry = common::spawn_mock_mcp("mock://unused", false);
    let one = common::spawn_mock_mcp("mock://unused", false);
    let fetched = format!(
        "::skill{{id=house-style title=\"House style\" src=\"{origin}/skills/house-style/SKILL.md\"}}\n\n\
         ::skill{{id=missing title=\"Missing\" src=\"{origin}/skills/missing/SKILL.md\"}}\n\n"
    );
    serve(&registry, &document(&[block("one", &one, "tone"), fetched]));
    let mut d = boot(config(&registry, None));
    let first = applied(&mut d, 0);
    assert_eq!(
        names(&first["skills_added"]),
        ["house-style", "tone"],
        "{first}"
    );
    let log = d.log();
    let got = events(&log, "skills.fetched");
    assert_eq!(got.len(), 1, "{log}");
    assert_eq!(got[0]["name"], "house-style");
    assert_eq!(got[0]["bytes"], SKILL.len());
    assert!(
        got[0]["digest"]
            .as_str()
            .is_some_and(|d| d.starts_with("sha256:")),
        "{}",
        got[0]
    );
    assert_eq!(inline_skills(&log), ["house-style", "tone"], "{log}");
    assert!(
        events(&log, "config.warning").iter().any(|e| e["warning"]
            .as_str()
            .is_some_and(|w| w.contains("\"missing\"") && w.contains("left out"))),
        "{log}"
    );
    wait_calls(&one, 1, "the rest of the document applies", &d);
}

/// `agent.on_instruction_updated: think`: a released edit starts a turn of
/// the root conversation by itself — nobody sends anything — and says so
/// (`instruction.woke`). Under the default (`note`) the same edit leaves a
/// note and starts nothing.
#[test]
fn a_changed_instruction_starts_a_turn_under_think_and_only_then() {
    for think in [true, false] {
        let registry = common::spawn_mock_mcp("mock://unused", false);
        serve(&registry, &document(&[]));
        let mut cfg = config(&registry, None);
        cfg["intelligence"] = json!({"endpoints": "mock:final", "model": "mock"});
        cfg["agent"]["wake_on"] = json!(["instruction_updated"]);
        if think {
            cfg["agent"]["on_instruction_updated"] = json!("think");
        }
        let mut d = boot(cfg);
        d.wait_for("the first version applied", |l| {
            !events(l, "instruction.applied").is_empty()
        });
        serve(
            &registry,
            &document(&[]).replace("Serve the mock.", "Keep the living room at 24.5."),
        );
        d.wait_for("the edit adopted", |l| {
            !events(l, "instruction.updated").is_empty()
        });
        std::thread::sleep(Duration::from_millis(2500));
        let log = d.log();
        let woke = events(&log, "instruction.woke");
        let turns = events(&log, "turn.done");
        if think {
            assert_eq!(woke.len(), 1, "think wakes the root once:\n{log}");
            assert!(
                !turns.is_empty(),
                "and a turn runs with nobody prompting it:\n{log}"
            );
        } else {
            assert!(woke.is_empty(), "note starts nothing:\n{log}");
            assert!(turns.is_empty(), "no turn runs:\n{log}");
        }
    }
}

/// The operator already declares the server a served document names, at the
/// same endpoint, as the demo kits do: the document's server IS the operator's
/// (RFC 0055 §3.1), so the first live apply folds into it and is no
/// duplicate.
#[test]
fn a_served_server_the_operator_declares_is_the_operators() {
    let registry = common::spawn_mock_mcp("mock://unused", false);
    let one = common::spawn_mock_mcp("mock://unused", false);
    serve(&registry, &document(&[block("one", &one, "tone")]));
    let mut cfg = config(&registry, None);
    cfg["mcp"]["servers"].as_array_mut().unwrap().push(
        json!({"name": "one", "endpoint": format!("{}/mcp", one.uri()),
                     "ns": "one", "allow": ["mock.ops"], "tags": {"*": ["trusted"]}}),
    );
    cfg["vars"] = json!({"doc": URI});
    cfg["agent"]["instruction"]["mcp"]["resource"] = json!("{{config.doc}}");
    let mut d = boot(cfg);
    let first = applied(&mut d, 0);
    let log = d.log();
    assert!(
        events(&log, "config.reload.invalid").is_empty(),
        "the first apply was refused:\n{log}"
    );
    let _ = first;
    wait_calls(
        &one,
        1,
        "the operator's server one called by the document's workflow",
        &d,
    );
}

/// The demo kits' case: the operator declares, under the document's names, a
/// server at another endpoint and its own workflow. The operator's stand,
/// the document's blocks are held as text, said, and the live apply is not
/// refused — the rest of the document's machinery (its skill) joins.
#[test]
fn the_operators_names_stand_over_a_served_documents_and_the_apply_goes_on() {
    let registry = common::spawn_mock_mcp("mock://unused", false);
    let one = common::spawn_mock_mcp("mock://unused", false);
    let elsewhere = common::spawn_mock_mcp("mock://unused", false);
    serve(&registry, &document(&[block("one", &elsewhere, "tone")]));
    let mut cfg = config(&registry, None);
    cfg["mcp"]["servers"]
        .as_array_mut()
        .unwrap()
        .push(json!({"name": "one", "endpoint": format!("{}/mcp", one.uri())}));
    cfg["workflows"].as_array_mut().unwrap().push(json!({
        "name": "ping-one", "steps": {
            "s": {"kind": "schedule", "every": "200ms"},
            "c": {"kind": "mcp.tool", "depends_on": ["s"], "server": "one", "tool": "mock.ops"},
            "f": {"kind": "finish", "depends_on": ["c"], "status": "completed"}}}));
    let mut d = boot(cfg);
    let first = applied(&mut d, 0);
    let log = d.log();
    assert!(
        events(&log, "config.reload.invalid").is_empty(),
        "the apply was refused:\n{log}"
    );
    assert_eq!(names(&first["skills_added"]), ["tone"], "{first}");
    for held in ["mcp server \"one\"", "workflow \"ping-one\""] {
        assert!(
            events(&log, "config.warning").iter().any(|w| w["warning"]
                .as_str()
                .is_some_and(|w| w.contains(held) && w.contains("operator's stands"))),
            "{held} is said to be held:\n{log}"
        );
    }
    // The operator's server is the one called; the document's endpoint never.
    wait_calls(
        &one,
        1,
        "the operator's server one called by its own workflow",
        &d,
    );
    assert_eq!(
        calls(&elsewhere),
        0,
        "the document's endpoint was dialed:\n{log}"
    );
}
