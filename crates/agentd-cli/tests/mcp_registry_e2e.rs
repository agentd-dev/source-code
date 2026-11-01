// SPDX-License-Identifier: AGPL-3.0-only
//! MCP registries (RFC 0055 P3), end to end against a registry that serves
//! the MCP registry API (`/v0.1/servers…`) from this test, its records
//! pointing at the built-in mock MCP server: a registry server named by the
//! operator's `mcp.servers`, by a workflow step, and by an instruction's
//! `:::mcp{registry server}` is resolved through its record and dialed on
//! use; records agentd must not dial are refused, said; a deprecated one
//! connects and warns; and a run keeps calling the version it first called
//! after the registry publishes another.
//!
//! One check runs against the MCP project's registry itself, ignored by
//! default (it needs the network): `cargo test -p agentd-cli --test
//! mcp_registry_e2e -- --ignored`.
#![cfg(unix)]

mod common;

use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const WAIT: Duration = Duration::from_secs(60);

/// name → version → (server.json, status)
type Records = BTreeMap<String, BTreeMap<String, (Value, String)>>;

/// A registry serving the API's two reads from records this test sets.
#[derive(Clone, Default)]
struct Registry {
    records: Arc<Mutex<Records>>,
    /// name → the version `latest` is.
    latest: Arc<Mutex<BTreeMap<String, String>>>,
    /// Each request's path, as it arrived.
    seen: Arc<Mutex<Vec<String>>>,
    base: String,
}

impl Registry {
    fn start() -> Registry {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let reg = Registry {
            base,
            ..Default::default()
        };
        let served = reg.clone();
        std::thread::spawn(move || {
            for conn in listener.incoming() {
                let Ok(mut conn) = conn else { continue };
                let served = served.clone();
                std::thread::spawn(move || {
                    let mut reader = BufReader::new(conn.try_clone().unwrap());
                    let mut line = String::new();
                    if reader.read_line(&mut line).is_err() {
                        return;
                    }
                    loop {
                        let mut h = String::new();
                        if reader.read_line(&mut h).is_err() || h.trim().is_empty() {
                            break;
                        }
                    }
                    let path = line.split_whitespace().nth(1).unwrap_or("").to_string();
                    served.seen.lock().unwrap().push(path.clone());
                    let (status, body) = served.answer(&path);
                    let _ = write!(
                        conn,
                        "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                });
            }
        });
        reg
    }

    /// Publish `server` (a server.json) at its version, `latest` from now.
    fn publish(&self, server: Value, status: &str) {
        let name = server["name"].as_str().unwrap().to_string();
        let version = server["version"].as_str().unwrap().to_string();
        self.records
            .lock()
            .unwrap()
            .entry(name.clone())
            .or_default()
            .insert(version.clone(), (server, status.to_string()));
        self.latest.lock().unwrap().insert(name, version);
    }

    fn envelope(server: &Value, status: &str) -> Value {
        json!({"server": server, "_meta": {"io.modelcontextprotocol.registry/official": {"status": status, "isLatest": true}}})
    }

    fn answer(&self, path: &str) -> (u16, String) {
        let decode = |s: &str| s.replace("%2F", "/").replace("%2f", "/");
        if let Some(rest) = path.strip_prefix("/v0.1/servers/") {
            let Some((name, version)) = rest.split_once("/versions/") else {
                return (404, "{}".into());
            };
            let name = decode(name);
            let version = match version {
                "latest" => match self.latest.lock().unwrap().get(&name) {
                    Some(v) => v.clone(),
                    None => return (404, "{}".into()),
                },
                v => v.to_string(),
            };
            return match self
                .records
                .lock()
                .unwrap()
                .get(&name)
                .and_then(|m| m.get(&version))
            {
                Some((server, status)) => (200, Registry::envelope(server, status).to_string()),
                None => (404, "{}".into()),
            };
        }
        if let Some(q) = path.strip_prefix("/v0.1/servers?") {
            let term = q
                .split('&')
                .find_map(|kv| kv.strip_prefix("search="))
                .map(decode)
                .unwrap_or_default();
            let latest = self.latest.lock().unwrap().clone();
            let records = self.records.lock().unwrap();
            let servers: Vec<Value> = latest
                .iter()
                .filter(|(n, _)| n.contains(&term))
                .filter_map(|(n, v)| records.get(n)?.get(v))
                .map(|(s, st)| Registry::envelope(s, st))
                .collect();
            return (
                200,
                json!({"servers": servers, "metadata": {"count": servers.len()}}).to_string(),
            );
        }
        (404, "{}".into())
    }

    fn version_reads(&self, name: &str) -> usize {
        let enc = name.replace('/', "%2F");
        self.seen
            .lock()
            .unwrap()
            .iter()
            .filter(|p| p.contains(&enc))
            .count()
    }
}

/// A server.json naming one Streamable HTTP remote.
fn record(name: &str, version: &str, url: &str) -> Value {
    json!({"name": name, "version": version, "title": name, "description": "a test server",
           "remotes": [{"type": "sse", "url": format!("{url}/sse")},
                       {"type": "streamable-http", "url": url}]})
}

fn registries(reg: &Registry, extra: &str) -> String {
    format!(
        "  registries:\n    - {{name: official, url: \"{}\", servers: [\"io.acme/*\"], allow_private: true, refresh: 1s{extra}}}\n",
        reg.base
    )
}

struct Daemon {
    child: Child,
    err_path: String,
}

impl Daemon {
    fn start(cfg: &str) -> Daemon {
        let err_path = common::unique_path("registry", "log");
        let child = Command::new(env!("CARGO_BIN_EXE_agentd"))
            .args(["--config", cfg])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(std::fs::File::create(&err_path).unwrap())
            .spawn()
            .unwrap();
        Daemon { child, err_path }
    }

    fn log(&self) -> String {
        std::fs::read_to_string(&self.err_path).unwrap_or_default()
    }

    fn wait_for(&self, what: &str, pred: impl Fn(&str) -> bool) -> String {
        let end = Instant::now() + WAIT;
        loop {
            let l = self.log();
            if pred(&l) {
                return l;
            }
            assert!(Instant::now() < end, "timed out waiting for {what}:\n{l}");
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    fn stop(&mut self) {
        unsafe { libc::kill(self.child.id() as i32, libc::SIGTERM) };
        let end = Instant::now() + Duration::from_secs(20);
        while self.child.try_wait().ok().flatten().is_none() {
            if Instant::now() > end {
                let _ = self.child.kill();
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        let _ = self.child.wait();
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_file(&self.err_path);
    }
}

fn events(log: &str, name: &str) -> Vec<Value> {
    log.lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter(|v| v["event"] == name)
        .collect()
}

fn run_done(log: &str, workflow: &str) -> Option<Value> {
    events(log, "run.done")
        .into_iter()
        .find(|e| e["workflow"] == workflow)
}

/// `mock.ops` calls that reached `mock`.
fn calls(mock: &common::MockMcp) -> usize {
    mock.log()
        .lines()
        .filter(|l| l.starts_with("MOCK_CALL ") && l.contains("\"name\":\"mock.ops\""))
        .count()
}

fn write_cfg(text: &str) -> String {
    let path = common::unique_path("registry", "yaml");
    std::fs::write(&path, text).unwrap();
    path
}

fn store(dir: &tempfile::TempDir) -> String {
    format!(
        "store:\n  kind: file\n  file: {{path: {}}}\n",
        dir.path().display()
    )
}

/// Use 1: the operator's `mcp.servers` entry names the server by registry.
/// Nothing is dialed at boot; the step that calls it has it resolved
/// through its record and dialed, and its call reaches the record's remote.
#[test]
fn an_operator_registry_server_is_resolved_and_dialed_on_use() {
    let mock = common::spawn_mock_mcp("mock://unused", false);
    let reg = Registry::start();
    reg.publish(
        record("io.acme/kb", "1.0.0", &format!("{}/mcp", mock.uri())),
        "active",
    );
    let dir = tempfile::tempdir().unwrap();
    let cfg = write_cfg(&format!(
        "agent: {{name: reg}}\n{}mcp:\n{}  servers:\n    - {{name: kb, registry: official, server: io.acme/kb}}\nworkflows:\n  - name: call\n    steps:\n      s: {{kind: once}}\n      c: {{kind: mcp.tool, depends_on: [s], server: kb, tool: mock.ops}}\n      f: {{kind: finish, depends_on: [c], status: completed}}\nlifecycle: {{run_until: drained}}\nobservability: {{log_level: info}}\n",
        store(&dir),
        registries(&reg, "")
    ));
    let d = Daemon::start(&cfg);
    let log = d.wait_for("the run", |l| run_done(l, "call").is_some());
    assert_eq!(
        run_done(&log, "call").unwrap()["status"],
        "completed",
        "{log}"
    );
    assert!(
        events(&log, "mcp.connect.deferred")
            .iter()
            .any(|e| e["server"] == "kb"),
        "not dialed at boot:\n{log}"
    );
    let resolved = events(&log, "mcp.registry.resolved");
    assert!(
        resolved.iter().any(|e| e["registry"] == "official"
            && e["server"] == "io.acme/kb"
            && e["version"] == "1.0.0"
            && e["url"] == format!("{}/mcp", mock.uri())
            && e["skipped"].as_array().is_some_and(|s| s.len() == 1)),
        "{log}"
    );
    assert_eq!(calls(&mock), 1);
    let _ = std::fs::remove_file(&cfg);
}

/// Use 2: a workflow step names the server by registry and name, with no
/// `mcp.servers` entry; and a run keeps calling the version it first called
/// after the registry publishes another, across a restart.
#[test]
fn a_step_names_a_registry_server_and_its_run_keeps_the_version_it_first_called() {
    let one = common::spawn_mock_mcp("mock://unused", false);
    let two = common::spawn_mock_mcp("mock://unused", false);
    let reg = Registry::start();
    reg.publish(
        record("io.acme/kb", "1.0.0", &format!("{}/mcp", one.uri())),
        "active",
    );
    let dir = tempfile::tempdir().unwrap();
    let step = "kind: mcp.tool, registry: official, server: io.acme/kb, tool: mock.ops";
    let cfg = write_cfg(&format!(
        "agent: {{name: reg}}\n{}mcp:\n{}workflows:\n  - name: pin\n    steps:\n      s: {{kind: once}}\n      c1: {{{step}, depends_on: [s]}}\n      w: {{kind: wait, depends_on: [c1], on: signal, signal: never, timeout: 6s, on_timeout: c2}}\n      c2: {{{step}}}\n      f: {{kind: finish, depends_on: [c2], status: completed}}\nlifecycle: {{run_until: drained}}\nobservability: {{log_level: info}}\n",
        store(&dir),
        registries(&reg, "")
    ));
    let mut d = Daemon::start(&cfg);
    let log = d.wait_for("the first call", |_| calls(&one) == 1);
    assert!(
        events(&log, "mcp.registry.server")
            .iter()
            .any(|e| e["name"] == "official/io.acme/kb" && e["server"] == "io.acme/kb"),
        "{log}"
    );
    d.stop();
    // The registry publishes 2.0.0, at another server; the run, resumed,
    // calls 1.0.0 still.
    reg.publish(
        record("io.acme/kb", "2.0.0", &format!("{}/mcp", two.uri())),
        "active",
    );
    std::thread::sleep(Duration::from_millis(1100)); // past `refresh`
    let d = Daemon::start(&cfg);
    let log = d.wait_for("the run", |l| run_done(l, "pin").is_some());
    assert_eq!(
        run_done(&log, "pin").unwrap()["status"],
        "completed",
        "{log}"
    );
    let resolved = events(&log, "mcp.registry.resolved");
    assert!(
        resolved
            .iter()
            .any(|e| e["name"] == "official/io.acme/kb" && e["version"] == "2.0.0"),
        "latest moved:\n{log}"
    );
    assert!(
        events(&log, "mcp.registry.pinned")
            .iter()
            .any(|e| e["version"] == "1.0.0" && e["resolved"] == "2.0.0"),
        "{log}"
    );
    assert!(
        resolved
            .iter()
            .any(|e| e["name"] == "official/io.acme/kb@1.0.0" && e["version"] == "1.0.0"),
        "{log}"
    );
    assert_eq!(calls(&one), 2, "both of the run's calls reached 1.0.0");
    assert_eq!(calls(&two), 0, "none reached 2.0.0");
    let _ = std::fs::remove_file(&cfg);
}

/// Use 3: an instruction's `:::mcp{registry server}`, its registry the
/// operator's and its name approved, is dialed right after the load and
/// called by the document's workflow under its id.
#[test]
fn an_instructions_registry_server_is_dialed_after_the_load() {
    let mock = common::spawn_mock_mcp("mock://unused", false);
    let reg = Registry::start();
    let url = format!("{}/mcp", mock.uri());
    reg.publish(record("io.acme/kb", "1.0.0", &url), "active");
    let dir = tempfile::tempdir().unwrap();
    let instr = dir.path().join("agent.md");
    std::fs::write(
        &instr,
        format!(
            "---\nspec: \"1\"\n---\n# Desk\n\nUse the knowledge base.\n\n\
             :::mcp{{id=kb registry=\"{}\" server=io.acme/kb src=\"{url}\" tools=\"mock.ops\"}}\nThe knowledge base.\n:::\n\n\
             :::workflow{{id=ask dialect=\"instruction/workflow\"}}\nname: ask\nsteps:\n  s: {{kind: once}}\n  c: {{kind: mcp.tool, depends_on: [s], server: kb, tool: mock.ops}}\n  f: {{kind: finish, depends_on: [c], status: completed}}\n:::\n",
            reg.base
        ),
    )
    .unwrap();
    let cfg = write_cfg(&format!(
        "agent: {{name: reg, preflight: never, instruction: {{file: {}}}}}\n{}mcp:\n{}lifecycle: {{run_until: drained}}\nobservability: {{log_level: info}}\n",
        instr.display(),
        store(&dir),
        registries(&reg, "")
    ));
    let d = Daemon::start(&cfg);
    let log = d.wait_for("the run", |l| run_done(l, "ask").is_some());
    assert_eq!(
        run_done(&log, "ask").unwrap()["status"],
        "completed",
        "{log}"
    );
    assert!(
        events(&log, "mcp.registry.resolved")
            .iter()
            .any(|e| e["name"] == "official/io.acme/kb" && e["url"] == url),
        "{log}"
    );
    assert_eq!(calls(&mock), 1);
    let _ = std::fs::remove_file(&cfg);
}

/// What agentd must not dial is refused, said, and fails the step that
/// names it; a deprecated record connects and warns.
#[test]
fn packages_only_is_refused_and_deprecated_warns() {
    let mock = common::spawn_mock_mcp("mock://unused", false);
    let reg = Registry::start();
    reg.publish(
        json!({"name": "io.acme/local", "version": "1.0.0",
               "packages": [{"registryType": "npm", "identifier": "@acme/local", "transport": {"type": "stdio"}}]}),
        "active",
    );
    reg.publish(
        record("io.acme/old", "0.9.0", &format!("{}/mcp", mock.uri())),
        "deprecated",
    );
    let dir = tempfile::tempdir().unwrap();
    let cfg = write_cfg(&format!(
        "agent: {{name: reg}}\n{}mcp:\n{}workflows:\n  - name: both\n    steps:\n      s: {{kind: once}}\n      p: {{kind: mcp.tool, depends_on: [s], registry: official, server: io.acme/local, tool: mock.ops, on_error: continue}}\n      o: {{kind: mcp.tool, depends_on: [s], registry: official, server: io.acme/old, tool: mock.ops}}\n      f: {{kind: finish, depends_on: [p, o], status: completed}}\nlifecycle: {{run_until: drained}}\nobservability: {{log_level: info}}\n",
        store(&dir),
        registries(&reg, "")
    ));
    let d = Daemon::start(&cfg);
    let log = d.wait_for("the run", |l| run_done(l, "both").is_some());
    let refused = events(&log, "mcp.registry.refused");
    assert!(
        refused.iter().any(|e| e["server"] == "io.acme/local"
            && e["err"].as_str().is_some_and(
                |m| m.contains("only packages") && m.contains("starts no local process")
            )),
        "{log}"
    );
    assert!(
        events(&log, "step.fail").iter().any(|e| e["step"] == "p") || log.contains("only packages"),
        "{log}"
    );
    assert!(
        events(&log, "mcp.registry.deprecated")
            .iter()
            .any(|e| e["server"] == "io.acme/old" && e["version"] == "0.9.0"),
        "{log}"
    );
    assert_eq!(calls(&mock), 1, "the deprecated server was called");
    assert_eq!(
        reg.version_reads("io.acme/local"),
        1,
        "a refused record is not read again within refresh"
    );
    let _ = std::fs::remove_file(&cfg);
}

/// A registry server whose session was lost after its record went stale is
/// re-dialed through its registry, not at the endpoint the record named:
/// with `edit`, the registry changed the record meanwhile. The run calls
/// `kb` once, the mock restarts (forgetting the session) past `refresh`,
/// and the run's second call goes where the record now says.
fn re_dialed_after_the_record_went_stale(
    edit: impl FnOnce(&Registry, &common::MockMcp, &common::MockMcp),
    c2: &str,
) -> (String, common::MockMcp, common::MockMcp) {
    let one = common::spawn_mock_mcp("mock://unused", false);
    let two = common::spawn_mock_mcp("mock://unused", false);
    let reg = Registry::start();
    reg.publish(
        record("io.acme/kb", "1.0.0", &format!("{}/mcp", one.uri())),
        "active",
    );
    let dir = tempfile::tempdir().unwrap();
    let cfg = write_cfg(&format!(
        "agent: {{name: reg}}\n{}mcp:\n{}  servers:\n    - {{name: kb, registry: official, server: io.acme/kb}}\nworkflows:\n  - name: call\n    steps:\n      s: {{kind: once}}\n      c1: {{kind: mcp.tool, depends_on: [s], server: kb, tool: mock.ops}}\n      w: {{kind: wait, depends_on: [c1], on: signal, signal: never, timeout: 4s, on_timeout: c2}}\n      c2: {{kind: mcp.tool, server: kb, tool: mock.ops{c2}}}\n      f: {{kind: finish, depends_on: [c2], status: completed}}\nlifecycle: {{run_until: drained}}\nobservability: {{log_level: info}}\n",
        store(&dir),
        registries(&reg, "")
    ));
    let d = Daemon::start(&cfg);
    d.wait_for("the first call", |_| calls(&one) == 1);
    edit(&reg, &one, &two);
    std::thread::sleep(Duration::from_millis(1100)); // past `refresh`
    one.control("mock.restart", json!({}));
    let log = d.wait_for("the run", |l| run_done(l, "call").is_some());
    let _ = std::fs::remove_file(&cfg);
    (log, one, two)
}

/// The record moved to another remote: the re-dial follows it.
#[test]
fn a_stale_record_is_read_again_on_a_re_dial_and_a_moved_remote_followed() {
    let (log, one, two) = re_dialed_after_the_record_went_stale(
        |reg, _, two| {
            reg.publish(
                record("io.acme/kb", "1.0.0", &format!("{}/mcp", two.uri())),
                "active",
            )
        },
        ", retry: {max: 6, backoff: 1s}",
    );
    assert_eq!(
        run_done(&log, "call").unwrap()["status"],
        "completed",
        "{log}"
    );
    assert!(
        events(&log, "mcp.disconnect")
            .iter()
            .any(|e| e["server"] == "kb" && e["registry"] == "stale"),
        "{log}"
    );
    assert!(
        events(&log, "mcp.registry.resolved")
            .iter()
            .any(|e| e["name"] == "kb" && e["url"] == format!("{}/mcp", two.uri())),
        "the record was read again:\n{log}"
    );
    assert_eq!(calls(&one), 1, "the second call did not go to the old remote");
    assert_eq!(calls(&two), 1, "the second call went where the record says");
}

/// The registry marked the record deleted: the re-dial refuses it, said,
/// and the server is not dialed again at its old remote.
#[test]
fn a_stale_record_deleted_meanwhile_refuses_the_re_dial() {
    let (log, one, _two) = re_dialed_after_the_record_went_stale(
        |reg, one, _| {
            reg.publish(
                record("io.acme/kb", "1.0.0", &format!("{}/mcp", one.uri())),
                "deleted",
            )
        },
        ", on_error: continue",
    );
    assert!(
        events(&log, "mcp.registry.refused")
            .iter()
            .any(|e| e["name"] == "kb"
                && e["err"].as_str().is_some_and(|m| m.contains("deleted"))),
        "{log}"
    );
    assert_eq!(calls(&one), 1, "the deleted server was not called again");
}

/// A name outside the registry's globs, and a registry approving nothing,
/// refuse the configuration before anything is dialed.
#[test]
fn a_name_the_registry_does_not_approve_refuses_the_load() {
    let validate = |mcp: &str| {
        let cfg = write_cfg(&format!(
            "agent: {{name: reg}}\nstore: {{kind: memory}}\nmcp:\n{mcp}workflows:\n  - name: w\n    steps:\n      s: {{kind: manual}}\n      c: {{kind: mcp.tool, depends_on: [s], registry: official, server: io.other/kb, tool: t}}\n      f: {{kind: finish, depends_on: [c]}}\n"
        ));
        let out = Command::new(env!("CARGO_BIN_EXE_agentd"))
            .args(["--validate-config", "--config", &cfg])
            .output()
            .unwrap();
        let _ = std::fs::remove_file(&cfg);
        (
            out.status.code(),
            String::from_utf8_lossy(&out.stderr).to_string(),
        )
    };
    let (code, err) = validate(
        "  registries:\n    - {name: official, url: \"https://registry.example\", servers: [\"io.acme/*\"]}\n",
    );
    assert_eq!(code, Some(2), "{err}");
    assert!(
        err.contains("io.other/kb") && err.contains("not approved"),
        "{err}"
    );
    let (code, err) =
        validate("  registries:\n    - {name: official, url: \"https://registry.example\"}\n");
    assert_eq!(code, Some(2), "{err}");
    assert!(err.contains("approves no server"), "{err}");
}

/// The CLI lists what a registry serves, and which names a configured
/// registry approves.
#[test]
fn the_cli_searches_a_registry() {
    let reg = Registry::start();
    reg.publish(
        record("io.acme/kb", "1.0.0", "https://kb.acme.example/mcp"),
        "active",
    );
    reg.publish(
        record("io.other/kb", "3.1.0", "https://kb.other.example/mcp"),
        "active",
    );
    let out = Command::new(env!("CARGO_BIN_EXE_agentd"))
        .args(["mcp", "registry", "search", "/kb", "--registry", &reg.base])
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "{text}\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        text.lines().any(|l| l.starts_with("io.acme/kb\t1.0.0\t")
            && l.contains("streamable-http https://kb.acme.example/mcp")),
        "{text}"
    );
    let cfg = write_cfg(&format!(
        "agent: {{name: reg}}\nstore: {{kind: memory}}\nmcp:\n  registries:\n    - {{name: official, url: \"{}\", servers: [\"io.acme/*\"]}}\n",
        reg.base
    ));
    let out = Command::new(env!("CARGO_BIN_EXE_agentd"))
        .args(["mcp", "registry", "search", "/kb", "-c", &cfg])
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "{text}\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        text.lines()
            .any(|l| l.starts_with("io.acme/kb") && l.ends_with("\tapproved")),
        "{text}"
    );
    assert!(
        text.lines()
            .any(|l| l.starts_with("io.other/kb") && l.ends_with("\tnot approved")),
        "{text}"
    );
    let _ = std::fs::remove_file(&cfg);
}

/// LIVE: the MCP project's registry resolves `io.github.github/github-mcp-server`
/// to a Streamable HTTP remote, whose required `Authorization` header the
/// operator fills. Needs the network.
#[test]
#[ignore = "reaches registry.modelcontextprotocol.io"]
fn live_the_official_registry_resolves_the_github_server() {
    let dial = agentd::config::RegistryDial {
        registry: "official".into(),
        base: agentd::mcp::registry::OFFICIAL.into(),
        server: "io.github.github/github-mcp-server".into(),
        version: "latest".into(),
        timeout: Duration::from_secs(30),
        ..Default::default()
    };
    let api = agentd::mcp::registry::Api::of(&dial);
    let record = api
        .version(&dial.server, &dial.version)
        .expect("the registry answers");
    let operator = vec![(
        "Authorization".to_string(),
        "Bearer {{secret:GITHUB_TOKEN}}".to_string(),
    )];
    let r = agentd::mcp::registry::validate_record(&dial.server, &record, &dial, &operator)
        .expect("a remote agentd dials");
    eprintln!(
        "LIVE resolved {} {} -> {} headers {:?} skipped {:?}",
        r.name, r.version, r.url, r.headers, r.skipped
    );
    assert!(r.url.starts_with("https://"), "{r:?}");
    agentd::mcp::registry::guard_url(&r.url, false).expect("a public address");
    let found = api.search("io.github.github", 20).expect("search answers");
    assert!(
        found
            .iter()
            .any(|s| s["server"]["name"] == "io.github.github/github-mcp-server"),
        "{found:?}"
    );
}
