// SPDX-License-Identifier: Apache-2.0
//! The SSRF guard **at the call sites** — the two surfaces whose URL agentd did
//! not choose: an A2A push target a peer registered, and the `url` of an `http`
//! workflow node a model (or a workflow author) filled in.
//!
//! `crates/net/tests/ssrf_rebinding.rs` proves the primitive. This file proves
//! the thing that actually ships: that the callers *use* it. A classifier that
//! nobody composes is decorative, and the shape it replaced —
//! `ssrf::guard_host(host)` followed by `http::connect_tcp(host)` — is two
//! resolutions of the same name, so an attacker who controls the authoritative
//! DNS answers the check `93.184.216.34` and the connect `169.254.169.254`.
//!
//! A hermetic test cannot install a lying nameserver under `getaddrinfo`, so the
//! rebinding assertion runs through the resolver seam
//! (`ssrf::connect_vetted_with`) on the exact *sequence* the push path now
//! performs — guard, then vetted dial — and contrasts it with a reconstruction
//! of the sequence that was there before, which really does reach the address
//! the second answer named. The real callers are then driven for their own
//! sake: a refusal must not dial, and — the other half of the bar — an allowed
//! target must still be reached, with SNI/`Host` untouched.
#![cfg(all(unix, feature = "a2a", feature = "workflow"))]

mod common;

use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU16, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use agentd::a2a::push;
use agentd::a2a::tasks::PushTarget;
use agentd::net::ssrf;
use serde_json::{Value, json};

/// Short: every dial here is either refused before the syscall or aimed at a
/// listener on this machine, so nothing should ever wait on the network.
const T: Duration = Duration::from_millis(500);

/// A public address the guard must accept — the historic `example.com` A
/// record, used as a literal so no test in this file touches real DNS.
fn public() -> IpAddr {
    IpAddr::V4(Ipv4Addr::new(93, 184, 216, 34))
}

// ---------------------------------------------------------------------------
// Witnesses: a listener nobody may reach, and a webhook receiver that records.
// ---------------------------------------------------------------------------

/// The private thing an attacker wants reached. Non-blocking, so "nobody came"
/// is a clean `WouldBlock` rather than a hang.
fn honeypot() -> (TcpListener, SocketAddr) {
    let l = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("bind loopback honeypot");
    l.set_nonblocking(true).expect("nonblocking honeypot");
    let a = l.local_addr().expect("honeypot addr");
    (l, a)
}

fn untouched(l: &TcpListener) -> bool {
    matches!(l.accept(), Err(ref e) if e.kind() == io::ErrorKind::WouldBlock)
}

/// One request a mock receiver saw, down to the `Host` header — the delivery
/// connects by address now, and the header must still carry the *name*.
#[derive(Clone, Default)]
struct Received {
    method: String,
    path: String,
    host: String,
    content_type: String,
    token: String,
    body: String,
}

/// A loopback HTTP receiver that records one request and answers `200`.
fn spawn_receiver() -> (u16, Arc<Mutex<Option<Received>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind receiver");
    let port = listener.local_addr().expect("receiver addr").port();
    let slot: Arc<Mutex<Option<Received>>> = Arc::new(Mutex::new(None));
    let seen = slot.clone();
    std::thread::spawn(move || {
        for _ in 0..4 {
            let Ok((stream, _)) = listener.accept() else {
                return;
            };
            serve_one(stream, &seen);
        }
    });
    (port, slot)
}

fn serve_one(stream: TcpStream, seen: &Arc<Mutex<Option<Received>>>) {
    stream.set_read_timeout(Some(Duration::from_secs(5))).ok();
    let mut reader = BufReader::new(stream);
    let mut start = String::new();
    if reader.read_line(&mut start).is_err() || start.is_empty() {
        return;
    }
    let mut parts = start.split_whitespace();
    let mut got = Received {
        method: parts.next().unwrap_or_default().to_string(),
        path: parts.next().unwrap_or_default().to_string(),
        ..Received::default()
    };
    let mut len = 0usize;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).is_err() || line.trim().is_empty() {
            break;
        }
        let (name, value) = match line.split_once(':') {
            Some((n, v)) => (n.trim().to_ascii_lowercase(), v.trim().to_string()),
            None => continue,
        };
        match name.as_str() {
            "content-length" => len = value.parse().unwrap_or(0),
            "content-type" => got.content_type = value,
            "host" => got.host = value,
            "x-a2a-notification-token" => got.token = value,
            _ => {}
        }
    }
    let mut buf = vec![0u8; len];
    if reader.read_exact(&mut buf).is_ok() {
        got.body = String::from_utf8_lossy(&buf).to_string();
    }
    *seen.lock().expect("receiver slot") = Some(got);
    let body = br#"{"ok":true}"#;
    let mut s = reader.into_inner();
    let head = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    let _ = s.write_all(head.as_bytes());
    let _ = s.write_all(body);
    let _ = s.flush();
}

fn target(url: &str) -> PushTarget {
    PushTarget {
        id: "pc-1".into(),
        url: url.into(),
        token: "caller-token".into(),
        bearer: None,
    }
}

// ---------------------------------------------------------------------------
// 1. The A2A push path: a peer-chosen target it must not reach.
// ---------------------------------------------------------------------------

#[test]
fn a_peer_supplied_push_target_inside_the_appliance_is_refused_without_being_dialled() {
    let (listener, addr) = honeypot();
    let err = push::deliver(
        &target(&format!("http://{addr}/hook")),
        &json!({"e": 1}),
        false,
    )
    .expect_err("a loopback webhook is refused even though the scheme rule allows it");
    assert!(
        err.contains("SSRF guard") && err.contains("loopback"),
        "the refusal names the class: {err}"
    );
    assert!(
        untouched(&listener),
        "the delivery opened a socket to a target it had just refused"
    );

    // The headline pivot, and the one the peer would actually ask for.
    let meta = push::deliver(
        &target("https://169.254.169.254/latest/meta-data/"),
        &json!({"e": 1}),
        false,
    )
    .expect_err("the cloud metadata endpoint is refused");
    assert!(meta.contains("link-local"), "{meta}");
}

#[test]
fn a_target_the_operator_allowed_still_arrives_with_the_host_header_on_the_name() {
    // The other half of the bar: a guard that refuses everything is not a fix.
    // `security.a2a.push.allow_private` is the operator's decision, and on a
    // cluster where the receiver really is on a private address the delivery
    // still has to happen — through the same vetted dial.
    let (port, seen) = spawn_receiver();
    let url = format!("http://127.0.0.1:{port}/hook");
    push::deliver(&target(&url), &json!({"kind": "status-update"}), true)
        .expect("the allowed delivery reaches the receiver");

    let got = seen
        .lock()
        .expect("receiver slot")
        .clone()
        .expect("the receiver saw the delivery");
    assert_eq!(got.method, "POST");
    assert_eq!(got.path, "/hook");
    // Connect by address, verify/address by name: the dial now takes a vetted
    // `SocketAddr`, but the `Host` header (and, over TLS, the SNI name) must
    // still be the authority from the URL — swapping in the IP would break
    // virtual hosting and certificate validation alike.
    assert_eq!(
        got.host,
        format!("127.0.0.1:{port}"),
        "the Host header stayed on the URL's authority"
    );
    assert!(
        got.content_type.contains("application/json"),
        "{got:?}",
        got = got.content_type
    );
    assert_eq!(got.token, "caller-token", "the caller's token rode along");
    let body: Value = serde_json::from_str(&got.body).expect("the event is the body");
    assert_eq!(body["kind"], "status-update");
}

// ---------------------------------------------------------------------------
// 2. The rebinding property, on the sequence the push path now performs.
// ---------------------------------------------------------------------------

static CALLS: AtomicUsize = AtomicUsize::new(0);
static REBIND_PORT: AtomicU16 = AtomicU16::new(0);

/// The hostile authoritative server: the first query gets a public address, and
/// every query after it gets the honeypot on loopback. Statics rather than a
/// closure because `ResolveFn` is a bare `fn` pointer — deliberately, so the
/// seam cannot smuggle state into production.
fn rebinding_resolver(_host: &str, port: u16) -> io::Result<Vec<SocketAddr>> {
    if CALLS.fetch_add(1, Ordering::SeqCst) == 0 {
        Ok(vec![SocketAddr::new(public(), port)])
    } else {
        Ok(vec![SocketAddr::new(
            IpAddr::V4(Ipv4Addr::LOCALHOST),
            REBIND_PORT.load(Ordering::SeqCst),
        )])
    }
}

#[test]
fn the_guard_then_dial_sequence_refuses_the_second_answer_where_the_old_one_dialled_it() {
    // Phase 1 — a reconstruction of the composition that used to be here:
    // guard the NAME, then hand the NAME to `connect_tcp`, which resolves it a
    // second time. This is not live code; it is the control. If it did NOT
    // reach the honeypot the rest of this test would prove nothing.
    let (old_shape, old_addr) = honeypot();
    CALLS.store(0, Ordering::SeqCst);
    REBIND_PORT.store(old_addr.port(), Ordering::SeqCst);
    let approved = ssrf::resolve_guarded_with("rebind.example", 443, false, rebinding_resolver)
        .expect("the hostile first answer is public, so the check passes");
    assert_eq!(approved, vec![SocketAddr::new(public(), 443)]);
    let second = rebinding_resolver("rebind.example", 443).expect("the connect's own lookup");
    let _ = TcpStream::connect_timeout(&second[0], T);
    assert!(
        !untouched(&old_shape),
        "the control must reach the honeypot — otherwise this test cannot tell \
         the two compositions apart"
    );

    // Phase 2 — the sequence `a2a::push::deliver` performs today: `check_url`
    // (which still resolves, for the yes/no admission answer), then a dial that
    // resolves ONCE and classifies what it is about to connect to. The hostile
    // server answers the dial's lookup with the honeypot, and that answer is
    // the one the classifier sees, so it is refused at the syscall boundary.
    let (new_shape, new_addr) = honeypot();
    CALLS.store(0, Ordering::SeqCst);
    REBIND_PORT.store(new_addr.port(), Ordering::SeqCst);
    ssrf::resolve_guarded_with("rebind.example", 443, false, rebinding_resolver)
        .expect("same first answer, same approval");
    let err = ssrf::connect_vetted_with("rebind.example", 443, T, false, rebinding_resolver)
        .expect_err("the dial's own answer is loopback, and it is dialled by nobody");
    assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);
    assert!(err.to_string().contains("SSRF guard"), "{err}");
    assert!(
        untouched(&new_shape),
        "the rebound address was reached: the dial is still trusting a lookup \
         it did not classify"
    );
    assert_eq!(
        CALLS.load(Ordering::SeqCst),
        2,
        "the dial resolves exactly once; a second lookup inside it would be a \
         fresh rebinding window"
    );
}

/// A benign resolver: one public address for any name.
fn public_resolver(_host: &str, port: u16) -> io::Result<Vec<SocketAddr>> {
    Ok(vec![SocketAddr::new(public(), port)])
}

#[test]
fn the_vetted_dial_approves_public_names_and_still_opens_sockets() {
    // Approval, with no I/O: a public answer passes and carries its port, so a
    // guarded surface pointed at a real endpoint is not broken by the fix.
    let addrs = ssrf::resolve_guarded_with("api.example.com", 8443, false, public_resolver)
        .expect("a public name must pass");
    assert_eq!(addrs, vec![SocketAddr::new(public(), 8443)]);

    // …and the dial really connects rather than always erroring. Loopback (so
    // the test stays hermetic) via the operator escape hatch.
    let (listener, addr) = honeypot();
    listener.set_nonblocking(false).expect("blocking accept");
    let stream = ssrf::connect_vetted("127.0.0.1", addr.port(), T, true)
        .expect("the escape hatch dial connects");
    assert_eq!(stream.peer_addr().expect("peer addr"), addr);
    listener.accept().expect("the listener saw the connection");
}

// ---------------------------------------------------------------------------
// 3. The `http` workflow node, through the real daemon.
// ---------------------------------------------------------------------------

struct Daemon {
    child: Child,
    stderr_path: String,
}
impl Daemon {
    fn stderr(&self) -> String {
        std::fs::read_to_string(&self.stderr_path).unwrap_or_default()
    }
    fn events(&self, name: &str) -> Vec<Value> {
        self.stderr()
            .lines()
            .filter_map(|l| serde_json::from_str::<Value>(l).ok())
            .filter(|v| v["event"] == name)
            .collect()
    }
}
impl Drop for Daemon {
    fn drop(&mut self) {
        unsafe {
            libc::kill(self.child.id() as i32, libc::SIGTERM);
        }
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline {
            if matches!(self.child.try_wait(), Ok(Some(_))) {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_file(&self.stderr_path);
    }
}

fn wait_for<F: Fn() -> bool>(f: F, secs: u64) -> bool {
    let deadline = Instant::now() + Duration::from_secs(secs);
    while Instant::now() < deadline {
        if f() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    false
}

#[test]
fn an_http_node_refuses_a_private_url_while_a_declared_one_still_reaches_its_server() {
    // Two workflows in one daemon, so the refusal and the delivery are observed
    // under identical conditions: `pivot` points at a loopback honeypot with no
    // `allow_private`, `declared` points at a real receiver with it.
    let (honey, honey_addr) = honeypot();
    let (port, seen) = spawn_receiver();
    let cfg_path = common::unique_path("agentd-ssrf-http", "yaml");
    let cfg = format!(
        "config_version: \"2\"\n\
         agent:\n  name: caller\n  instruction: make a call\n  preflight: never\n\
         intelligence:\n  endpoints: http://127.0.0.1:1\n  model: mock\n\
         store:\n  kind: memory\n\
         workflows:\n\
         \x20 - name: pivot\n    steps:\n\
         \x20     s:    {{kind: once}}\n\
         \x20     call: {{kind: http, depends_on: [s], method: GET, url: \"http://127.0.0.1:{honey_port}/latest/meta-data/\"}}\n\
         \x20     done: {{kind: finish, depends_on: [call]}}\n\
         \x20 - name: declared\n    steps:\n\
         \x20     s:    {{kind: once}}\n\
         \x20     call: {{kind: http, depends_on: [s], method: GET, url: \"http://127.0.0.1:{port}/ok\", allow_private: true}}\n\
         \x20     done: {{kind: finish, depends_on: [call], output: \"{{{{steps.call.output}}}}\"}}\n\
         lifecycle:\n  run_until: drained\n\
         observability:\n  log_level: info\n  log_content: true\n",
        honey_port = honey_addr.port()
    );
    std::fs::write(&cfg_path, &cfg).expect("write config");
    let stderr_path = common::unique_path("ssrf-http-daemon", "log");
    let errf = std::fs::File::create(&stderr_path).expect("create log");
    let child = Command::new(env!("CARGO_BIN_EXE_agentd"))
        .args(["--config", &cfg_path])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(errf))
        .spawn()
        .expect("spawn daemon");
    let daemon = Daemon { child, stderr_path };

    assert!(
        wait_for(|| daemon.events("run.done").len() >= 2, 20),
        "both workflows finished:\n{}",
        daemon.stderr()
    );
    let done = daemon.events("run.done");

    // The guarded one failed, and said why in the words the guard uses.
    let blocked = done
        .iter()
        .find(|e| e["workflow"] == "pivot")
        .expect("the pivot run reported");
    assert_eq!(blocked["status"], "failed", "{blocked}");
    let err = blocked["err"].as_str().unwrap_or_default();
    assert!(
        err.contains("SSRF guard") && err.contains("loopback"),
        "the step error carries the guard's wording: {blocked}"
    );
    assert!(
        untouched(&honey),
        "the http node connected to the address it had just refused"
    );

    // …and the declared-internal one went through, so the node is not simply
    // broken. Same code path, one config flag apart.
    let ok = done
        .iter()
        .find(|e| e["workflow"] == "declared")
        .expect("the declared run reported");
    assert_eq!(ok["status"], "completed", "{ok}");
    assert_eq!(ok["output"]["status"], 200, "{ok}");
    let got = seen
        .lock()
        .expect("receiver slot")
        .clone()
        .expect("the receiver saw the allowed call");
    assert_eq!(got.path, "/ok");
    assert_eq!(got.host, format!("127.0.0.1:{port}"));

    std::fs::remove_file(&cfg_path).ok();
}

// ---------------------------------------------------------------------------
// 4. The wiring itself.
// ---------------------------------------------------------------------------

/// A source file with its comments stripped, so an invariant about the *code*
/// is not satisfied (or broken) by prose that merely names a function.
fn code_of(path: &str) -> String {
    let src = std::fs::read_to_string(path).unwrap_or_else(|e| panic!("read {path}: {e}"));
    src.lines()
        .filter(|l| !l.trim_start().starts_with("//"))
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn the_guarded_surfaces_dial_the_vetted_addresses_and_not_the_name() {
    // This is the one assertion in this file that a revert would fail, and it
    // is deliberately structural: the two dials differ ONLY when a name resolves
    // to different addresses on two consecutive lookups, and a test cannot make
    // `getaddrinfo` do that without owning the machine's resolver. The property
    // itself is proven above through the seam; what remains to pin down is that
    // these surfaces are wired to the dial that has it — because the defect was
    // never in the classifier, it was that nobody's socket went through it.
    for path in [
        concat!(env!("CARGO_MANIFEST_DIR"), "/../agentd/src/a2a/push.rs"),
        concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../agentd/src/runtime/http_node.rs"
        ),
    ] {
        let code = code_of(path);
        assert!(
            code.contains("ssrf::connect_vetted("),
            "{path} must dial through the vetted-address connect"
        );
        assert!(
            !code.contains("connect_tcp("),
            "{path} dials by name: `http::connect_tcp` resolves again, so the \
             guard above it decides about one address and the socket goes to \
             another"
        );
    }
}
