// SPDX-License-Identifier: AGPL-3.0-only
//! agentd as a Personal Agent (rfcs/0047 §9, P1), end to end: the binary,
//! its configuration and the fake Company (`--internal-mock-company`) as
//! separate processes, the client identity printed with
//! `--print-company-client` and hosted statically. What the fake saw is read
//! from its own record, and what crossed to a child from the frames the
//! supervisor wrote (`AGENTD_TEST_FRAMES_FILE`).
#![cfg(all(unix, feature = "companies"))]

mod common;

use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../net/tests/fixtures")
        .join(name)
}

fn trust_the_test_ca() {
    static ONCE: OnceLock<()> = OnceLock::new();
    ONCE.get_or_init(|| {
        agentd::net::tls::install_extra_ca(&std::fs::read(fixture("ca.pem")).unwrap()).unwrap();
    });
}

fn events(log: &str, name: &str) -> Vec<Value> {
    log.lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter(|v| v["event"] == name)
        .collect()
}

/// Wait until `log`'s file holds a line that `pred` accepts.
fn wait_for(log: &Path, what: &str, pred: impl Fn(&str) -> bool) -> String {
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let l = std::fs::read_to_string(log).unwrap_or_default();
        if pred(&l) {
            return l;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {what}:\n{l}"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// The fake Company, a process of its own.
struct Fake {
    child: Child,
    origin: String,
}

impl Drop for Fake {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn fake(dir: &Path, variants: &[&str], listen: Option<&str>) -> Fake {
    trust_the_test_ca();
    let addr = dir.join(format!("fake-{}.addr", common::ephemeral_low()));
    let mut args: Vec<String> = vec![
        "--internal-mock-company".into(),
        addr.display().to_string(),
        "--cert".into(),
        fixture("server.pem").display().to_string(),
        "--key".into(),
        fixture("server.key").display().to_string(),
        "--ca".into(),
        fixture("ca.pem").display().to_string(),
    ];
    for v in variants {
        args.extend(["--variant".to_string(), v.to_string()]);
    }
    if let Some(l) = listen {
        args.extend(["--listen".to_string(), l.to_string()]);
    }
    let child = Command::new(env!("CARGO_BIN_EXE_agentd"))
        .args(&args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let bound = common::read_addr_file(&addr.display().to_string());
    let port = bound.rsplit(':').next().unwrap().to_string();
    Fake {
        child,
        origin: format!("https://127.0.0.1:{port}"),
    }
}

impl Fake {
    /// What reached the fake, its own steering and reading aside.
    fn record(&self) -> Vec<Value> {
        let r = agentd::auth::http::get(
            &format!("{}/_mock/record", self.origin),
            &[],
            Duration::from_secs(10),
            agentd::auth::http::Dial::Named,
        )
        .unwrap();
        let v: Value = serde_json::from_slice(&r.body).unwrap();
        v["records"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|r| {
                !r["path"]
                    .as_str()
                    .unwrap_or_default()
                    .starts_with("/_mock/")
            })
            .cloned()
            .collect()
    }

    fn control(&self, v: Value) {
        agentd::auth::http::send(
            "POST",
            &format!("{}/_mock/control", self.origin),
            &[("Content-Type", "application/json")],
            v.to_string().as_bytes(),
            Duration::from_secs(10),
            agentd::auth::http::Dial::Named,
        )
        .unwrap();
    }

    /// Every token the fake issued.
    #[cfg(feature = "a2a")]
    fn issued(&self) -> Vec<String> {
        self.record()
            .iter()
            .filter_map(|r| r["issued"]["access_token"].as_str().map(str::to_string))
            .collect()
    }
}

/// An HTTPS server of the files in a directory, at `/companies/<file>`: the
/// static hosting of what `--print-company-client` writes.
struct Host {
    origin: String,
    dir: Arc<Mutex<Option<PathBuf>>>,
}

fn host() -> Host {
    trust_the_test_ca();
    let identity = agentd::net::tls::ServerIdentity::from_pem(
        &std::fs::read(fixture("server.pem")).unwrap(),
        &std::fs::read(fixture("server.key")).unwrap(),
    )
    .unwrap();
    let acceptor = Arc::new(agentd::net::tls::TlsAcceptor::new(identity, None).unwrap());
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let origin = format!("https://{}", listener.local_addr().unwrap());
    let dir: Arc<Mutex<Option<PathBuf>>> = Arc::default();
    let served = Arc::clone(&dir);
    std::thread::spawn(move || {
        for tcp in listener.incoming().flatten() {
            let (acceptor, served) = (Arc::clone(&acceptor), Arc::clone(&served));
            std::thread::spawn(move || {
                let Ok(mut tls) = acceptor.accept(tcp) else {
                    return;
                };
                let mut line = String::new();
                let mut r = BufReader::new(&mut tls);
                if r.read_line(&mut line).is_err() {
                    return;
                }
                loop {
                    let mut h = String::new();
                    if r.read_line(&mut h).unwrap_or(0) == 0 || h.trim().is_empty() {
                        break;
                    }
                }
                let path = line.split_whitespace().nth(1).unwrap_or("").to_string();
                let file = path.strip_prefix("/companies/").and_then(|f| {
                    let d = served.lock().unwrap().clone()?;
                    std::fs::read(d.join(f)).ok()
                });
                let (status, body) = match file {
                    Some(b) => ("200 OK", b),
                    None => ("404 Not Found", Vec::new()),
                };
                let _ = write!(
                    tls,
                    "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = tls.write_all(&body);
                let _ = tls.flush();
            });
        }
    });
    Host { origin, dir }
}

/// One agent's configuration: a scratch dir, the Company at `fake`, its
/// identity on `host`.
struct Setup {
    dir: PathBuf,
    cfg: PathBuf,
}

#[cfg(feature = "a2a")]
const OPERATOR: &str = "companies-e2e-operator-token";

fn setup(host: &Host, fake: &str, company: &str, top: &str) -> Setup {
    let dir = tempfile::tempdir().unwrap().keep();
    let cfg = dir.join("agent.yaml");
    std::fs::write(dir.join("play.json"), play().to_string()).unwrap();
    // A file store, unless `top` names another.
    let store = if top.contains("\nstore:") {
        String::new()
    } else {
        format!(
            "store: {{kind: file, file: {{path: {}/state}}}}\n",
            dir.display()
        )
    };
    std::fs::write(
        &cfg,
        format!(
            "agent:\n  name: shopper\n  instruction: Shop for the User.\n  prompt: Wait.\n  preflight: never\n\
             intelligence: {{ endpoints: \"mock:file:{d}/play.json\", model: mock }}\n\
             {store}\
             lifecycle: {{run_until: drained}}\n\
             observability:\n  log_level: debug\n  log_content: true\n\
             security:\n  tls_ca: {ca}\n  companies:\n    client: {{id: \"{h}/companies/client.json\", name: \"Test agent\", key_file: \"{d}/client-key\"}}\n{top}\
             services:\n  shop: {{kind: company, endpoint: \"{fake}\", allow_private: true{company}}}\n",
            d = dir.display(),
            ca = fixture("ca.pem").display(),
            h = host.origin,
        ),
    )
    .unwrap();
    // The identity, printed and hosted.
    let out = Command::new(env!("CARGO_BIN_EXE_agentd"))
        .args([
            "--config",
            &cfg.display().to_string(),
            "--print-company-client",
        ])
        .arg(dir.join("published"))
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "--print-company-client: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    *host.dir.lock().unwrap() = Some(dir.join("published"));
    Setup { dir, cfg }
}

/// The model: a message naming what to call has it called, and its result
/// echoed. A subagent's task is told apart by its instruction.
fn play() -> Value {
    json!({
        "match": [
            {"when_contains": "needs a signed-in Session", "echo_tool_result": true},
            {"when_contains": "coupons are not sold here", "echo_tool_result": true},
            {"when_contains": "never read from one", "echo_tool_result": true},
            {"when_contains": "serves it", "echo_tool_result": true},
            {"when_contains": "SUBSCRIBE-SHOP", "tool_calls": [{"name": "instruction.subscribe", "arguments": {"uri": "mcp://shop/instruction"}}]},
            {"when_contains": "SUBSCRIBE-BARE", "tool_calls": [{"name": "instruction.subscribe", "arguments": {"uri": "x://instruction"}}]},
            {"when_contains": "refuses this agent", "echo_tool_result": true},
            {"when_contains": "stormline", "echo_tool_result": true},
            {"when_contains": "no such tool", "echo_tool_result": true},
            {"when_contains": "SUBTASK", "tool_calls": [{"name": "shop.search", "arguments": {"query": "sub"}}]},
            {"when_contains": "DELEGATE", "tool_calls": [{"name": "subagent.run", "arguments": {"instruction": "SUBTASK: search", "servers": ["shop"], "mode": "sync"}}]},
            {"when_contains": "CALL-ORDER", "tool_calls": [{"name": "shop.order_status", "arguments": {"order_id": "o1"}}]},
            {"when_contains": "CALL-COUPON", "tool_calls": [{"name": "shop.search", "arguments": {"query": "coupon"}}]},
            {"when_contains": "CALL-SEARCH", "tool_calls": [{"name": "shop.search", "arguments": {"query": "boots"}}]}
        ],
        "turns": [{"content": "nothing to do"}]
    })
}

/// A daemon, stopped (SIGTERM) when dropped.
struct Daemon {
    child: Child,
    log: PathBuf,
}

impl Daemon {
    #[cfg(feature = "a2a")]
    fn read(&self) -> String {
        std::fs::read_to_string(&self.log).unwrap_or_default()
    }
    fn stop(&mut self) {
        unsafe { libc::kill(self.child.id() as i32, libc::SIGTERM) };
        let deadline = Instant::now() + Duration::from_secs(10);
        while self.child.try_wait().ok().flatten().is_none() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        self.stop();
    }
}

fn start(s: &Setup, name: &str, env: &[(&str, &str)]) -> Daemon {
    let log = s.dir.join(format!("{name}.log"));
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_agentd"));
    cmd.args(["--config", &s.cfg.display().to_string()])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(std::fs::File::create(&log).unwrap());
    for (k, v) in env {
        cmd.env(k, v);
    }
    Daemon {
        child: cmd.spawn().unwrap(),
        log,
    }
}

/// The keys of agentd's own correlators in a `tools/call`'s `_meta`, as the
/// fake recorded them: `agent/*` and the trace context (the protocol's own
/// keys aside).
fn correlators(call: &Value) -> Vec<String> {
    call["meta"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .filter(|k| k.starts_with("agent/") || k.starts_with("trace"))
        .map(str::to_string)
        .collect()
}

fn joined(log: &str, server: &str) -> bool {
    events(log, "mcp.joined")
        .iter()
        .any(|e| e["server"] == server)
}

/// `--validate-config` dials nothing: the Company's endpoint — a listener
/// that counts every connection, completed or not — is never connected to.
/// And `--print-company-client` writes documents that hold to the kit, with
/// the key it created `0600`.
#[test]
fn validate_config_opens_no_socket_and_the_printed_identity_validates() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let company = format!("https://{}", listener.local_addr().unwrap());
    let connections = Arc::new(Mutex::new(0usize));
    let counted = Arc::clone(&connections);
    std::thread::spawn(move || {
        for _ in listener.incoming() {
            *counted.lock().unwrap() += 1;
        }
    });
    let h = host();
    let s = setup(&h, &company, "", "");
    let out = Command::new(env!("CARGO_BIN_EXE_agentd"))
        .args([
            "--config",
            &s.cfg.display().to_string(),
            "--validate-config",
        ])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    std::thread::sleep(Duration::from_millis(200));
    assert_eq!(
        *connections.lock().unwrap(),
        0,
        "--validate-config or --print-company-client opened a socket to the Company"
    );
    let kit = agentd::companies::mock::kit::kit();
    let doc: Value =
        serde_json::from_slice(&std::fs::read(s.dir.join("published/client.json")).unwrap())
            .unwrap();
    kit.validate("client-metadata.schema.json", &doc).unwrap();
    assert_eq!(doc["redirect_uris"], json!([]));
    assert_eq!(
        doc["client_id"],
        format!("{}/companies/client.json", h.origin)
    );
    let jwks: Value =
        serde_json::from_slice(&std::fs::read(s.dir.join("published/jwks.json")).unwrap()).unwrap();
    assert!(jwks["keys"][0].get("d").is_none(), "public halves only");
    use std::os::unix::fs::PermissionsExt;
    let mode = std::fs::metadata(s.dir.join("client-key"))
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o600);
    // A configuration that refuses to load prints nothing, exit 2.
    let bad = s.dir.join("bad.yaml");
    std::fs::write(
        &bad,
        std::fs::read_to_string(&s.cfg).unwrap().replace(
            "allow_private: true",
            "allow_private: true, headers: {X-A: b}",
        ),
    )
    .unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_agentd"))
        .args([
            "--config",
            &bad.display().to_string(),
            "--print-company-client",
        ])
        .arg(s.dir.join("again"))
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    assert!(!s.dir.join("again").exists());
}

/// A Company that is down at boot joins when it comes up: its server's
/// tools are listed then, with no restart, and `auth.unavailable` said
/// meanwhile for target `company:shop`.
#[test]
fn a_company_down_at_boot_joins_when_it_comes_up() {
    let dir = tempfile::tempdir().unwrap();
    let port = common::free_port();
    let h = host();
    let s = setup(&h, &format!("https://127.0.0.1:{port}"), "", "");
    let d = start(&s, "boot", &[]);
    wait_for(&d.log, "the Company unavailable", |l| {
        events(l, "auth.unavailable")
            .iter()
            .any(|e| e["target"] == "company:shop")
    });
    let _f = fake(dir.path(), &[], Some(&format!("127.0.0.1:{port}")));
    let l = wait_for(&d.log, "the Company's server joined", |l| joined(l, "shop"));
    let connect = events(&l, "mcp.connect");
    assert!(
        connect
            .iter()
            .any(|e| e["server"] == "shop" && e["reason"] == "joined" && e["tools"] == 2),
        "{l}"
    );
    assert_eq!(events(&l, "company.verified").len(), 1, "{l}");
}

/// A Company whose documents verify while its token endpoint refuses has
/// not recovered: `auth.unavailable` is said for target `company:shop` and
/// `auth.recovered` is not, however often the join tries — until a token is
/// issued and the server joins, when it is said once.
#[test]
fn a_company_whose_token_endpoint_refuses_has_not_recovered() {
    let dir = tempfile::tempdir().unwrap();
    let f = fake(dir.path(), &[], None);
    f.control(json!({"refuse_grants": 1000}));
    let h = host();
    let s = setup(&h, &f.origin, "", "");
    let d = start(&s, "refused", &[]);
    // The join tries at once, then a second and two seconds later.
    let deadline = Instant::now() + Duration::from_secs(60);
    while f
        .record()
        .iter()
        .filter(|r| r["path"] == "/auth/token" && r["error"] == "invalid_client")
        .count()
        < 3
    {
        assert!(
            Instant::now() < deadline,
            "{}",
            std::fs::read_to_string(&d.log).unwrap_or_default()
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    let said = |l: &str, event: &str| {
        events(l, event)
            .iter()
            .filter(|e| e["target"] == "company:shop")
            .count()
    };
    let l = std::fs::read_to_string(&d.log).unwrap();
    assert_eq!(said(&l, "auth.unavailable"), 1, "{l}");
    assert_eq!(said(&l, "auth.recovered"), 0, "{l}");
    f.control(json!({"refuse_grants": 0}));
    let l = wait_for(&d.log, "the Company's server joined", |l| joined(l, "shop"));
    assert_eq!(said(&l, "auth.recovered"), 1, "{l}");
}

/// Two workflows started by a loop — work nobody owns: one calls the
/// Company's tool with a `tool` step, the other names its server in an
/// `mcp.tool` step through a template. With the User `user:dana`, the first
/// is refused for whom the work is for and the second for naming a Company's
/// server, and nothing reaches the Company. With the User `operator`, the
/// `tool` step reaches it, carrying none of agentd's correlators.
#[test]
fn a_workflow_reaches_a_company_only_through_its_tools() {
    const FLOWS: &str = "workflows:\n\
        \x20 - name: unowned\n    steps:\n\
        \x20     s: {kind: loop, interval: 300ms, max_iterations: 200}\n\
        \x20     ut: {kind: tool, name: shop.search, args: {query: wf}, depends_on: [s]}\n\
        \x20     f: {kind: finish, depends_on: [ut], status: completed}\n\
        \x20 - name: templated\n    steps:\n\
        \x20     s: {kind: loop, interval: 300ms, max_iterations: 200, inputs: {srv: shop}}\n\
        \x20     tm: {kind: mcp.tool, server: \"{{ inputs.srv }}\", tool: search, args: {query: t}, depends_on: [s]}\n\
        \x20     f: {kind: finish, depends_on: [tm], status: completed}\n";
    let refused_for = |l: &str, step: &str, why: &str| {
        events(l, "step.done")
            .iter()
            .any(|e| e["step"] == step && e["err"].as_str().is_some_and(|x| x.contains(why)))
    };
    let dir = tempfile::tempdir().unwrap();
    let f = fake(dir.path(), &[], None);
    let h = host();
    let s = setup(
        &h,
        &f.origin,
        "",
        &format!("    user: \"user:dana\"\n{FLOWS}"),
    );
    let d = start(&s, "dana", &[]);
    wait_for(&d.log, "both steps refused once the Company joined", |l| {
        joined(l, "shop")
            && refused_for(
                l,
                "ut",
                "company tools act for user:dana; this work is owned by nobody",
            )
            && refused_for(
                l,
                "tm",
                "mcp server \"shop\" is a Company's: call its tools with a `tool` step",
            )
    });
    drop(d);
    let calls = |r: &[Value]| -> Vec<Value> {
        r.iter()
            .filter(|r| r["mcp"] == "tools/call")
            .cloned()
            .collect()
    };
    assert!(calls(&f.record()).is_empty(), "{:#?}", f.record());
    let s = setup(&h, &f.origin, "", FLOWS);
    let d = start(&s, "operator", &[]);
    let deadline = Instant::now() + Duration::from_secs(60);
    while calls(&f.record()).is_empty() {
        assert!(
            Instant::now() < deadline,
            "{}",
            std::fs::read_to_string(&d.log).unwrap_or_default()
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    for c in calls(&f.record()) {
        assert!(c["meta"].is_array(), "{c}");
        assert!(
            correlators(&c).is_empty(),
            "a strict Company sees no correlator: {c}"
        );
    }
}

/// A Company listing two MCP APIs with no `mcp_names` is refused, naming
/// why; with them, each joins under its name.
#[test]
fn two_mcp_apis_need_their_names() {
    let dir = tempfile::tempdir().unwrap();
    let f = fake(dir.path(), &["two-mcp-apis"], None);
    let h = host();
    let s = setup(&h, &f.origin, "", "");
    let d = start(&s, "unnamed", &[]);
    let l = wait_for(&d.log, "the Company refused", |l| {
        !events(l, "company.refused").is_empty()
    });
    let refused = &events(&l, "company.refused")[0];
    assert_eq!(refused["reason"], "mcp_names", "{refused}");
    assert!(
        refused["error"].as_str().unwrap().contains("/mcp/orders"),
        "{refused}"
    );
    drop(d);
    let names = format!(
        ", mcp_names: {{\"{o}/mcp\": search, \"{o}/mcp/orders\": orders}}",
        o = f.origin
    );
    let s = setup(&h, &f.origin, &names, "");
    let d = start(&s, "named", &[]);
    let l = wait_for(&d.log, "both servers joined", |l| {
        joined(l, "search") && joined(l, "orders")
    });
    assert!(!joined(&l, "shop"), "{l}");
}

/// After a restart the Company sees the same User ID and the same Session,
/// renewed with a new DPoP key; with a seal key, the store holds no User ID
/// in plaintext.
#[test]
fn a_restart_keeps_the_user_and_the_session_and_the_store_holds_them_sealed() {
    let dir = tempfile::tempdir().unwrap();
    let f = fake(dir.path(), &[], None);
    let h = host();
    let seal = dir.path().join("seal");
    std::fs::write(&seal, "ab".repeat(32)).unwrap();
    let s = setup(
        &h,
        &f.origin,
        "",
        &format!("    seal_key: \"{{{{secret-file:{}}}}}\"\n", seal.display()),
    );
    let mut d = start(&s, "first", &[]);
    wait_for(&d.log, "joined", |l| joined(l, "shop"));
    d.stop();
    let first = f.record();
    let user_of = |r: &[Value]| {
        r.iter()
            .find_map(|t| t["assertion"]["sub"].as_str().map(str::to_string))
            .expect("a Session assertion")
    };
    holds_no(&s.dir.join("state"), &user_of(&first));
    let mut d = start(&s, "second", &[]);
    wait_for(&d.log, "joined again", |l| joined(l, "shop"));
    d.stop();
    let record = f.record();
    let second = &record[first.len()..];
    let tok = |r: &&Value| r["path"] == "/auth/token" && r["status"] == 200;
    let started = first.iter().find(|r| tok(r) && r["dpop"] == true).unwrap();
    let user = started["assertion"]["sub"].as_str().unwrap();
    let sid = first
        .iter()
        .find(|r| tok(r) && r["resource"].is_string())
        .unwrap()["session_id"]
        .clone();
    let renewed = second.iter().find(|r| tok(r) && r["dpop"] == true).unwrap();
    assert_eq!(renewed["assertion"]["sub"], user, "the same User ID");
    assert_eq!(renewed["session_id"], sid, "the same Session");
    assert_ne!(renewed["jkt"], started["jkt"], "a new DPoP key");
    holds_no(&s.dir.join("state"), user);
    // And the record the store holds sealed is the one a third life opens.
    let mut d = start(&s, "third", &[]);
    wait_for(&d.log, "joined a third time", |l| joined(l, "shop"));
    d.stop();
    let all = f.record();
    assert_eq!(user_of(&all[record.len()..]), user, "the same User ID");
    holds_no(&s.dir.join("state"), user);
}

/// No file under `dir` names `secret`, the User ID.
fn holds_no(dir: &Path, secret: &str) {
    let mut stack = vec![dir.to_path_buf()];
    while let Some(p) = stack.pop() {
        for e in std::fs::read_dir(&p).into_iter().flatten().flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
            } else {
                let text = String::from_utf8_lossy(&std::fs::read(&p).unwrap()).into_owned();
                assert!(!text.contains(secret), "{} holds the User ID", p.display());
            }
        }
    }
}

/// An identity the Company could not fetch as configured — here, nothing
/// published at the client_id — makes every Company unavailable, saying
/// why, and no assertion is sent.
#[test]
fn an_unpublished_identity_makes_the_company_unavailable() {
    let dir = tempfile::tempdir().unwrap();
    let f = fake(dir.path(), &[], None);
    let h = host();
    let s = setup(&h, &f.origin, "", "");
    *h.dir.lock().unwrap() = Some(dir.path().join("nothing-here"));
    let d = start(&s, "unpublished", &[]);
    let l = wait_for(&d.log, "company.identity.unpublished", |l| {
        !events(l, "company.identity.unpublished").is_empty()
    });
    let e = &events(&l, "company.identity.unpublished")[0];
    assert_eq!(
        e["url"],
        format!("{}/companies/client.json", h.origin),
        "{e}"
    );
    wait_for(&d.log, "auth.unavailable", |l| {
        events(l, "auth.unavailable")
            .iter()
            .any(|e| e["target"] == "company:shop")
    });
    assert!(
        f.record().iter().all(|r| r["path"] != "/auth/token"),
        "an assertion went out: {:?}",
        f.record()
    );
}

/// A remote store (here the mock MCP server's state tools) holds the User ID
/// record sealed: no key or value it was given names the User ID.
#[test]
fn a_remote_store_holds_the_user_id_sealed() {
    let dir = tempfile::tempdir().unwrap();
    let f = fake(dir.path(), &[], None);
    let h = host();
    let mock = common::spawn_mock_mcp("mock://res", false);
    let seal = dir.path().join("seal");
    std::fs::write(&seal, "cd".repeat(32)).unwrap();
    let s = setup(
        &h,
        &f.origin,
        "",
        &format!(
            "    seal_key: \"{{{{secret-file:{}}}}}\"\nstore: {{kind: mcp, mcp: {{server: state}}}}\nmcp:\n  servers:\n    - {{name: state, endpoint: \"{}\"}}\n",
            seal.display(),
            mock.uri()
        ),
    );
    let mut d = start(&s, "remote", &[]);
    wait_for(&d.log, "joined", |l| joined(l, "shop"));
    d.stop();
    let user = f
        .record()
        .iter()
        .find_map(|r| r["assertion"]["sub"].as_str().map(str::to_string))
        .expect("a Session assertion reached the fake");
    let keys = mock.control("state.list", json!({"prefix": ""}));
    let keys: Vec<String> = keys["keys"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|k| k["key"].as_str().map(str::to_string))
        .collect();
    assert!(keys.iter().any(|k| k.contains("/cred/")), "{keys:?}");
    for k in &keys {
        let v = mock.control("state.get", json!({"key": k}));
        let text = v.to_string();
        assert!(!text.contains(&user), "{k} holds the User ID in plaintext");
        assert!(!k.contains(&user));
    }
}

#[cfg(feature = "a2a")]
mod over_a2a {
    use super::*;

    /// `client.serve`: the A2A listener serves the client metadata document
    /// and key set at their URLs' paths, cacheable, and a Company fetches
    /// them there and accepts what they sign.
    #[test]
    fn the_listener_serves_the_identity_companies_fetch() {
        let dir = tempfile::tempdir().unwrap();
        let f = fake(dir.path(), &[], None);
        let s = tempfile::tempdir().unwrap().keep();
        let cfg = s.join("agent.yaml");
        let log = s.join("served.log");
        let (mut d, addr) = common::spawn_bound(|port| {
            std::fs::write(
                &cfg,
                format!(
                    "agent:\n  name: served\n  instruction: Shop.\n  preflight: never\n\
                     intelligence: {{ endpoints: \"mock:final\", model: mock }}\n\
                     store: {{kind: file, file: {{path: {d}/state}}}}\n\
                     lifecycle: {{run_until: drained}}\n\
                     observability: {{log_level: info}}\n\
                     a2a:\n  listen: https://127.0.0.1:{port}\n  url: https://127.0.0.1:{port}\n  tls: {{cert: {cert}, key: {key}}}\n\
                     security:\n  tls_ca: {ca}\n  companies:\n    client: {{id: \"https://127.0.0.1:{port}/companies/client.json\", name: \"Served agent\", key_file: \"{d}/client-key\", serve: true}}\n\
                     services:\n  shop: {{kind: company, endpoint: \"{o}\", allow_private: true}}\n",
                    d = s.display(),
                    cert = fixture("server.pem").display(),
                    key = fixture("server.key").display(),
                    ca = fixture("ca.pem").display(),
                    o = f.origin,
                ),
            )
            .unwrap();
            let child = Command::new(env!("CARGO_BIN_EXE_agentd"))
                .args(["--config", &cfg.display().to_string()])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(std::fs::File::create(&log).unwrap())
                .spawn()
                .unwrap();
            (
                Daemon {
                    child,
                    log: log.clone(),
                },
                log.display().to_string(),
            )
        });
        wait_for(&d.log, "joined", |l| joined(l, "shop"));
        let r = agentd::auth::http::get(
            &format!("https://{addr}/companies/client.json"),
            &[],
            Duration::from_secs(10),
            agentd::auth::http::Dial::Named,
        )
        .unwrap();
        assert_eq!(r.status, 200);
        assert_eq!(r.header("cache-control"), Some("max-age=300"));
        let doc: Value = serde_json::from_slice(&r.body).unwrap();
        agentd::companies::mock::kit::kit()
            .validate("client-metadata.schema.json", &doc)
            .unwrap();
        assert_eq!(
            doc["client_id"],
            format!("https://{addr}/companies/client.json")
        );
        let r = agentd::auth::http::get(
            &format!("https://{addr}/companies/jwks.json"),
            &[],
            Duration::from_secs(10),
            agentd::auth::http::Dial::Named,
        )
        .unwrap();
        assert_eq!(r.status, 200);
        // The fake fetched them from the listener and took what they sign.
        let record = f.record();
        assert!(
            record
                .iter()
                .any(|r| r["path"] == "/auth/token" && r["status"] == 200),
            "{record:#?}"
        );
        d.stop();
    }

    /// What the user `user:dana` presents.
    const DANA: &str = "companies-e2e-dana-token";

    /// A daemon with an A2A listener the test drives as the operator, or as
    /// the user `user:dana`.
    fn start_listening(s: &Setup, name: &str, env: &[(&str, &str)]) -> (Daemon, String) {
        let cfg = std::fs::read_to_string(&s.cfg).unwrap();
        common::spawn_bound(|port| {
            std::fs::write(
                &s.cfg,
                format!(
                    "{cfg}a2a:\n  listen: http://127.0.0.1:{port}\n  principals:\n    - {{id: ops, match: {{bearer_ref: \"{{{{secret:COMPANIES_E2E_OPERATOR}}}}\"}}, role: operator}}\n    - {{id: dana, match: {{bearer_ref: \"{{{{secret:COMPANIES_E2E_DANA}}}}\"}}, role: user}}\n"
                ),
            )
            .unwrap();
            let mut env: Vec<(&str, &str)> = env.to_vec();
            env.push(("COMPANIES_E2E_OPERATOR", OPERATOR));
            env.push(("COMPANIES_E2E_DANA", DANA));
            let d = start(s, name, &env);
            let log = d.log.display().to_string();
            (d, log)
        })
    }

    fn ask(addr: &str, text: &str) -> Value {
        common::SendMessage::text(text).bearer(OPERATOR).post(addr)
    }

    /// Company tools act for the configured User: with `user: user:dana`,
    /// a turn the operator's message starts is offered none of them and a
    /// call to one reaches nothing, while dana's own turn reaches the
    /// Company.
    #[test]
    fn company_tools_act_for_their_user_only() {
        let dir = tempfile::tempdir().unwrap();
        let f = fake(dir.path(), &[], None);
        let h = host();
        let s = setup(&h, &f.origin, "", "    user: \"user:dana\"\n");
        let frames = s.dir.join("frames.jsonl");
        let (d, addr) = start_listening(
            &s,
            "owned",
            &[("AGENTD_TEST_FRAMES_FILE", &frames.display().to_string())],
        );
        wait_for(&d.log, "joined", |l| joined(l, "shop"));
        let calls = || {
            f.record()
                .iter()
                .filter(|r| r["mcp"] == "tools/call")
                .count()
        };
        ask(&addr, "CALL-SEARCH as the operator");
        let l = wait_for(&d.log, "the operator's call refused", |l| {
            replies(l)
                .iter()
                .any(|r| r.contains("no such tool 'shop.search'"))
        });
        assert_eq!(calls(), 0, "{l}");
        let offered: Vec<Value> = std::fs::read_to_string(&frames)
            .unwrap()
            .lines()
            .filter_map(|l| serde_json::from_str::<Value>(l).ok())
            .filter(|f| f["dir"] == "spawn")
            .flat_map(|f| {
                f["frame"]["payload"]["turn"]["tools"]
                    .as_array()
                    .cloned()
                    .unwrap_or_default()
            })
            .collect();
        assert!(!offered.is_empty());
        assert!(
            offered
                .iter()
                .all(|t| !t["name"].as_str().unwrap_or_default().starts_with("shop.")),
            "a Company tool was offered in the operator's work"
        );
        let v = common::SendMessage::text("CALL-SEARCH as dana")
            .bearer(DANA)
            .post(&addr);
        assert!(v.get("error").is_none(), "{v}");
        wait_for(&d.log, "dana's search answered", |l| {
            replies(l).iter().any(|r| r.contains("stormline"))
        });
        assert_eq!(calls(), 1);
        // Under the strict profile the Company is not told who dana is, nor
        // which instance or run asks.
        let call = f
            .record()
            .into_iter()
            .find(|r| r["mcp"] == "tools/call")
            .unwrap();
        assert!(call["meta"].is_array(), "{call}");
        assert!(correlators(&call).is_empty(), "{call}");
    }

    /// The instruction is never read from a Company's server: one named is
    /// refused as a Company's, and a bare URI is not asked of it — the fake
    /// sees no resource read.
    #[test]
    fn no_instruction_is_read_from_a_company() {
        let dir = tempfile::tempdir().unwrap();
        let f = fake(dir.path(), &[], None);
        let h = host();
        let s = setup(&h, &f.origin, "", "");
        let (d, addr) = start_listening(&s, "instruction", &[]);
        wait_for(&d.log, "joined", |l| joined(l, "shop"));
        for (ask_it, want) in [
            (
                "SUBSCRIBE-SHOP",
                "mcp server \"shop\" is a Company's, and an instruction is never read from one",
            ),
            ("SUBSCRIBE-BARE", "no connected MCP server serves it"),
        ] {
            let before = replies(&d.read()).len();
            ask(&addr, ask_it);
            let l = wait_for(&d.log, ask_it, |l| replies(l).len() > before);
            let answer = &replies(&l)[before];
            assert!(answer.contains(want), "{ask_it}: {answer}");
        }
        let read: Vec<Value> = f
            .record()
            .into_iter()
            .filter(|r| r["mcp"] == "resources/read")
            .collect();
        assert!(read.is_empty(), "{read:#?}");
    }

    fn replies(log: &str) -> Vec<String> {
        events(log, "turn.reply")
            .iter()
            .map(|e| e["text"].as_str().unwrap_or_default().to_string())
            .collect()
    }

    /// Custody (RFC 0047 §5.6): a turn worker and a subagent granted the
    /// Company's server — with `security.policies` empty — reach its tools
    /// only through the supervisor. No spawn payload carries a Company
    /// server's spec or route, the subagent is handed the tools' definitions
    /// and gated names, and none of the tokens the fake issued is in any
    /// frame, any child's environment, any log line or any file the store
    /// wrote.
    #[test]
    fn company_tokens_and_servers_stay_with_the_supervisor() {
        let dir = tempfile::tempdir().unwrap();
        let f = fake(dir.path(), &[], None);
        let h = host();
        let s = setup(&h, &f.origin, "", "");
        let frames = s.dir.join("frames.jsonl");
        let (d, addr) = start_listening(
            &s,
            "custody",
            &[("AGENTD_TEST_FRAMES_FILE", &frames.display().to_string())],
        );
        wait_for(&d.log, "joined", |l| joined(l, "shop"));
        let calls = || {
            f.record()
                .iter()
                .filter(|r| r["mcp"] == "tools/call")
                .count()
        };
        // The subagent's call, then the turn worker's: each reaches the fake
        // over the supervisor's connection.
        let v = ask(&addr, "DELEGATE please");
        assert!(v.get("error").is_none(), "{v}");
        assert_eq!(calls(), 1, "{}", d.read());
        let v = ask(&addr, "CALL-SEARCH");
        assert!(v.get("error").is_none(), "{v}");
        assert_eq!(calls(), 2, "{}", d.read());
        let frames: Vec<Value> = std::fs::read_to_string(&frames)
            .unwrap()
            .lines()
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect();
        let spawns: Vec<&Value> = frames.iter().filter(|f| f["dir"] == "spawn").collect();
        assert!(
            spawns.len() >= 2,
            "a turn worker and a subagent: {spawns:#?}"
        );
        let (mut subagent_node, mut routed_turns) = (None, 0);
        for sp in &spawns {
            let p = &sp["frame"]["payload"];
            let payload = if p["payload"].is_object() {
                &p["payload"]
            } else {
                p
            };
            for spec in payload["mcp_servers"].as_array().into_iter().flatten() {
                assert_ne!(
                    spec["name"], "shop",
                    "a Company server spec crossed: {spec}"
                );
                assert!(spec.get("company").is_none(), "{spec}");
            }
            for routes in [&payload["mcp_routes"], &payload["turn"]["mcp_routes"]] {
                for (name, _) in routes.as_object().into_iter().flatten() {
                    assert!(
                        !name.starts_with("shop."),
                        "a Company route crossed: {name}"
                    );
                }
            }
            if payload["gated_tools"]
                .as_array()
                .is_some_and(|g| g.iter().any(|t| t == "shop.search"))
            {
                assert!(
                    payload["supervisor_tools"]
                        .as_array()
                        .is_some_and(|t| t.iter().any(|d| d["name"] == "shop.search")),
                    "the subagent is handed the definition: {payload}"
                );
                subagent_node = sp["node"].as_u64();
            }
            // A turn planned once the Company joined routes its tools to the
            // supervisor (the boot prompt's turn came before).
            if payload["turn"]["internal"]
                .as_array()
                .is_some_and(|i| i.iter().any(|t| t == "shop.search"))
            {
                routed_turns += 1;
            }
        }
        assert!(
            routed_turns >= 2,
            "the DELEGATE and CALL-SEARCH turns route shop.search up"
        );
        let node = subagent_node.expect("a subagent granted the Company's tools");
        assert!(
            frames.iter().any(|f| f["node"] == node
                && f["dir"] == "up"
                && f["frame"]["name"] == "shop.search"),
            "the subagent's call came up to the supervisor"
        );
        // No token anywhere a child, the log or the store could hold it.
        let tokens = f.issued();
        assert!(tokens.len() >= 2, "{tokens:?}");
        let mut places: Vec<(String, String)> = vec![
            (
                "the frames".into(),
                std::fs::read_to_string(s.dir.join("frames.jsonl")).unwrap(),
            ),
            ("the log".into(), d.read()),
        ];
        let mut stack = vec![s.dir.join("state")];
        while let Some(p) = stack.pop() {
            for e in std::fs::read_dir(&p).into_iter().flatten().flatten() {
                let p = e.path();
                if p.is_dir() {
                    stack.push(p);
                } else {
                    places.push((
                        p.display().to_string(),
                        String::from_utf8_lossy(&std::fs::read(&p).unwrap()).into_owned(),
                    ));
                }
            }
        }
        for t in &tokens {
            for (place, text) in &places {
                assert!(!text.contains(t.as_str()), "a token is in {place}");
            }
        }
    }

    /// What the supervisor does with each answer a Company's MCP server
    /// gives, seen by the model: `403 sign_in_required` with no Account Token
    /// is a typed tool error; a tool's own error is the Company's words, as
    /// it wrote them; `extension_required` makes the server unusable, and the
    /// next call is refused unsent. A refetch that changes the MCP APIs logs
    /// `company.apis_changed`, once, and changes no server. And with
    /// `privacy: standard` the calls carry agentd's correlators.
    #[test]
    fn company_answers_reach_the_model_typed() {
        let dir = tempfile::tempdir().unwrap();
        let f = fake(dir.path(), &[], None);
        let h = host();
        let s = setup(&h, &f.origin, ", privacy: standard", "");
        let (d, addr) =
            start_listening(&s, "answers", &[("AGENTD_TEST_COMPANY_MAX_AGE_MS", "500")]);
        wait_for(&d.log, "joined", |l| joined(l, "shop"));
        f.control(json!({"extra_mcp": true}));
        std::thread::sleep(Duration::from_millis(700));
        ask(&addr, "CALL-SEARCH");
        let l = wait_for(&d.log, "apis_changed", |l| {
            !events(l, "company.apis_changed").is_empty()
        });
        let changed = &events(&l, "company.apis_changed")[0];
        assert_eq!(
            changed["added"],
            json!([format!("{}/mcp/orders", f.origin)]),
            "{changed}"
        );
        wait_for(&d.log, "the search answered", |l| {
            replies(l).iter().any(|r| r.contains("stormline"))
        });
        ask(&addr, "CALL-ORDER");
        let l = wait_for(&d.log, "sign_in_required typed", |l| {
            replies(l)
                .iter()
                .any(|r| r.contains("needs a signed-in Session"))
        });
        assert!(
            replies(&l)
                .iter()
                .any(|r| r.contains("company shop needs a signed-in Session")),
            "{:?}",
            replies(&l)
        );
        // Past the documents' lifetime, so the next call refetches them.
        std::thread::sleep(Duration::from_millis(700));
        let before = replies(&l).len();
        ask(&addr, "CALL-COUPON");
        let l = wait_for(&d.log, "the coupon answered", |l| replies(l).len() > before);
        let answer = &replies(&l)[before];
        assert!(
            answer.contains(
                "coupons are not sold here: sign_in_required and extension_required are the tool's words"
            ),
            "{answer}"
        );
        assert!(events(&l, "company.interface.refused").is_empty(), "{l}");
        f.control(json!({"extension_required": "operations"}));
        ask(&addr, "CALL-SEARCH again");
        let l = wait_for(&d.log, "extension_required typed", |l| {
            replies(l).iter().any(|r| r.contains("refuses this agent"))
        });
        assert!(!events(&l, "company.interface.refused").is_empty(), "{l}");
        let before = f.record().len();
        ask(&addr, "CALL-SEARCH once more");
        wait_for(&d.log, "the second refusal", |l| {
            replies(l)
                .iter()
                .filter(|r| r.contains("refuses this agent"))
                .count()
                >= 2
        });
        let after: Vec<Value> = f.record()[before..].to_vec();
        assert!(
            after.iter().all(|r| r["path"] != "/mcp"),
            "refused unsent: {after:#?}"
        );
        let l = d.read();
        assert_eq!(
            events(&l, "mcp.joined").len(),
            1,
            "no server was added: {l}"
        );
        assert_eq!(events(&l, "company.apis_changed").len(), 1, "{l}");
        assert_eq!(events(&l, "company.privacy.standard").len(), 1, "{l}");
        for call in f.record().iter().filter(|r| r["mcp"] == "tools/call") {
            let keys = correlators(call);
            for k in [
                "agent/instance",
                "agent/acting_for",
                "agent/idempotency_key",
            ] {
                assert!(keys.iter().any(|x| x == k), "{k} in {call}");
            }
        }
    }
}
