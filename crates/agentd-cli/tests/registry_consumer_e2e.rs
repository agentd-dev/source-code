// SPDX-License-Identifier: AGPL-3.0-only
//! **The registry-consumer contract, without a registry** (RFC-0016 §6,
//! RFC-0028 §3.3, spec §7.6): the real binary reads an `instruction://…`
//! resource from the in-tree mock MCP server, which serves the same
//! `md.instruction/*` alignment metadata and author/delivery attestations a
//! registry serves — signed with a fixed test seed.
//!
//! These behaviours were developed against a live gateway. This suite is what
//! keeps them honest in CI, where no gateway exists: the apply boundary, the
//! consumer binding + report, and §7.6 verification including its fail-closed
//! refusals.
#![cfg(all(unix, feature = "internal-mocks", feature = "sign"))]

mod common;

use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

/// The mock signs with this seed; the test pins the publisher key derived
/// from it. Kept in sync by construction — both read the same constant.
fn publisher_key_file() -> String {
    let key = agentd::aauth::AgentKey::from_seed(&agentd::mcp::mock_http::MOCK_SIGN_SEED)
        .expect("test seed");
    // The key FILE form the loader accepts: raw 32 public bytes.
    let path = common::unique_path("mock-pub", "key");
    std::fs::write(&path, key.public_bytes()).unwrap();
    path
}

struct Run {
    log: String,
}

/// Boot the real daemon against the mock registry and capture its log, to
/// `proc.ready` (or its exit).
fn boot(instruction: &str, sources: Value) -> Run {
    boot_until(instruction, sources, |log| log.contains("proc.ready"))
}

/// [`boot`], capturing the log until `done` holds for it (or the daemon exits).
fn boot_until(instruction: &str, sources: Value, done: impl Fn(&str) -> bool) -> Run {
    let mock = common::spawn_mock_mcp("mock://watched", false);
    let cfg = json!({
        "agent": {"name": "reg-consumer", "preflight": "never",
                  // The pins live WITH the instruction they protect
                  // (`agent.instruction.trust`), so the long form carries both.
                  "instruction": {"mcp": instruction, "trust": sources},
                  "document_capabilities": ["compute"]},
        "mcp": {"servers": [{"name": "registry", "endpoint": format!("{}/mcp", mock.uri())}]},
        "intelligence": {"endpoints": ["http://127.0.0.1:1/v1"], "model": "mock"},
        "store": {"kind": "memory"},
    });
    let cfg_path = common::unique_path("reg-consumer", "json");
    std::fs::write(&cfg_path, serde_json::to_vec(&cfg).unwrap()).unwrap();
    let err_path = common::unique_path("reg-consumer", "log");
    let errf = std::fs::File::create(&err_path).unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_agentd"))
        .args(["-c", &cfg_path])
        .stdout(Stdio::null())
        .stderr(errf)
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(25);
    loop {
        let log = std::fs::read_to_string(&err_path).unwrap_or_default();
        if done(&log) || log.contains("proc.exit") || child.try_wait().is_ok_and(|s| s.is_some()) {
            break;
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            panic!("daemon neither got there nor exited:\n{log}");
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let _ = child.kill();
    let _ = child.wait();
    let log = std::fs::read_to_string(&err_path).unwrap_or_default();
    for p in [cfg_path, err_path] {
        let _ = std::fs::remove_file(&p);
    }
    Run { log }
}

/// The binding's report is in the log, or a binding call failed: it follows
/// `instruction.applied` by a registry round trip, off the loop.
fn reported_or_failed(log: &str) -> bool {
    log.contains("\"instruction.binding.reported\"") || log.contains("\"instruction.binding.fail\"")
}

fn event(log: &str, name: &str) -> Option<Value> {
    log.lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .find(|v| v["event"] == name)
}

#[test]
fn a_registry_read_captures_its_version_and_reports_the_binding() {
    // To the machinery's apply: with `agent.instruction.machinery: live` (the
    // default) the served document's workflow joins the running config through
    // the staged reload, after startup.
    // And to the binding's report, which follows the apply off the loop.
    let r = boot_until("instruction://ins_mock@stable", json!([]), |log| {
        log.contains("instruction.machinery.applied") && reported_or_failed(log)
    });
    let loaded = event(&r.log, "instruction.loaded")
        .unwrap_or_else(|| panic!("no instruction.loaded:\n{}", r.log));
    assert_eq!(
        loaded["version_id"], "ver_mock_1",
        "the registry version is captured"
    );

    let applied = event(&r.log, "instruction.applied")
        .unwrap_or_else(|| panic!("no apply boundary:\n{}", r.log));
    assert!(applied["old_version_id"].is_null());
    assert_eq!(applied["new_version_id"], "ver_mock_1");
    assert!(
        applied["delivered_digest"]
            .as_str()
            .is_some_and(|d| d.starts_with("sha256:")),
        "the apply line carries the delivered digest: {applied}"
    );

    // The consumer created ONE binding and reported the version it applied.
    // The binding follows the URI's ref BY NAME: the mock refuses, as the
    // registry does, a target that is not a ref name (`@stable`, the URI).
    let created = event(&r.log, "instruction.binding.created")
        .unwrap_or_else(|| panic!("no binding created:\n{}", r.log));
    assert_eq!(created["target"], "stable", "{created}");
    assert_eq!(created["mode"], "follow", "{created}");
    assert!(
        event(&r.log, "instruction.binding.fail").is_none(),
        "the registry refused the binding:\n{}",
        r.log
    );
    let reported = event(&r.log, "instruction.binding.reported")
        .unwrap_or_else(|| panic!("no binding report:\n{}", r.log));
    assert_eq!(reported["version_id"], "ver_mock_1");

    // The daemon started on the served document.
    assert!(
        r.log.contains("proc.ready"),
        "startup completed:\n{}",
        r.log
    );
    // A registry document resolved at RUNTIME arms its machinery live: the
    // workflow it declares is loaded beside the operator's, in one staged
    // reload attributed to the instruction.
    let armed = r
        .log
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .any(|v| v["event"] == "workflow.loaded" && v["name"] == "mock-drain");
    assert!(armed, "the document's workflow is armed live:\n{}", r.log);
    let reloaded =
        event(&r.log, "config.reloaded").unwrap_or_else(|| panic!("no staged reload:\n{}", r.log));
    assert_eq!(reloaded["trigger"], "instruction", "{reloaded}");
}

/// The binding's target as the registry takes it: a URI with no ref follows
/// `latest` by name, and one that names a version is pinned, to the version
/// the read applied. Either way the mock (like the registry) accepts it; a
/// target of `@latest` or the whole URI it refuses.
#[test]
fn a_binding_follows_a_ref_by_name_or_pins_the_version_applied() {
    for (uri, target, mode) in [
        ("instruction://ins_mock", "latest", "follow"),
        ("instruction://ins_mock@latest", "latest", "follow"),
        ("instruction://ins_mock@v1", "ver_mock_1", "pin"),
    ] {
        let r = boot_until(uri, json!([]), |log| {
            log.contains("proc.ready") && reported_or_failed(log)
        });
        let created = event(&r.log, "instruction.binding.created")
            .unwrap_or_else(|| panic!("{uri}: no binding created:\n{}", r.log));
        assert_eq!(created["target"], target, "{uri}: {created}");
        assert_eq!(created["mode"], mode, "{uri}: {created}");
        assert!(
            event(&r.log, "instruction.binding.fail").is_none(),
            "{uri}: the registry refused the binding:\n{}",
            r.log
        );
    }
}

#[test]
fn a_pinned_publisher_verifies_the_author_and_delivery_attestations() {
    let key = publisher_key_file();
    let r = boot(
        "instruction://ins_mock@stable",
        json!([{
            "uri": "instruction://ins_mock@stable",
            "publisher": "https://instruction.md/pub/mock",
            "author_keys": [key.clone()],
            // Deliberately EMPTY: the delivery key is the REGISTRY's, a
            // different key at its own document, so the verifier must
            // discover it from the read's `deliveryKeys` pointer. Pinning the
            // publisher key here would verify nothing about delivery.
            "delivery_keys": [],
            "reader": "agent://mock-reader",
            "max_capabilities": ["compute"],
        }]),
    );
    let v = event(&r.log, "instruction.verified")
        .unwrap_or_else(|| panic!("no verification:\n{}", r.log));
    assert_eq!(v["key_state"], "active");
    assert_eq!(v["publisher"], "https://instruction.md/pub/mock");
    assert_eq!(v["delivery_checked"], true, "the reader's aud was checked");
    // grant (`compute`) ∩ max_capabilities ∩ author ∩ delivery.
    assert_eq!(v["capabilities"], json!(["compute"]));
    // The delivery signature was verified with a DIFFERENT key from the
    // publisher's — discovered from the read's `deliveryKeys` pointer, which
    // is the shape a real registry serves (publisher keys attest authorship;
    // the registry's own key attests delivery).
    assert_ne!(
        agentd::mcp::mock_http::MOCK_SIGN_SEED,
        agentd::mcp::mock_http::MOCK_DELIVERY_SEED,
        "the mock must not sign delivery with the publisher key"
    );
    assert!(
        r.log.contains("proc.ready"),
        "startup completed:\n{}",
        r.log
    );
    let _ = std::fs::remove_file(&key);
}

/// A delivery attestation CAPS for its reader (§7.6 step 4: effective =
/// grant ∩ max_capabilities ∩ author ∩ delivery). `@narrow` is signed by an
/// author attesting `core` and `compute` and delivered attesting `core`
/// alone, so the read admits nothing the grant (`compute`) names — the set
/// the served document then folds under. A verifier that intersected only
/// the author's set would admit `compute`.
#[test]
fn a_delivery_attestation_narrows_what_the_read_admits() {
    let key = publisher_key_file();
    let r = boot(
        "instruction://ins_mock@narrow",
        json!([{
            "uri": "instruction://ins_mock@narrow",
            "publisher": "https://instruction.md/pub/mock",
            "author_keys": [key.clone()],
            "delivery_keys": [],
            "reader": "agent://mock-reader",
        }]),
    );
    let v = event(&r.log, "instruction.verified")
        .unwrap_or_else(|| panic!("no verification:\n{}", r.log));
    assert_eq!(v["author_capabilities"], json!(["core", "compute"]));
    assert_eq!(v["capabilities"], json!([]), "the delivery's cap applies");
    let _ = std::fs::remove_file(&key);
}

/// The author signature a delivery chains to is the one the read carries,
/// whose key state was checked: `@reauthored` serves a second valid author
/// signature beside a delivery that embeds the first, and is refused.
#[test]
fn a_delivery_chained_to_another_author_signature_is_refused() {
    let key = publisher_key_file();
    let r = boot(
        "instruction://ins_mock@reauthored",
        json!([{
            "uri": "instruction://ins_mock@reauthored",
            "publisher": "https://instruction.md/pub/mock",
            "author_keys": [key.clone()],
            "delivery_keys": [],
            "reader": "agent://mock-reader",
        }]),
    );
    assert!(!r.log.contains("proc.ready"), "must not start:\n{}", r.log);
    assert!(
        r.log.contains("a different author signature"),
        "the refusal names the chain:\n{}",
        r.log
    );
    let _ = std::fs::remove_file(&key);
}

/// A pin covers ONE document. `@foreign` serves, under the URI pinned for
/// `ins_mock`, another document the same publisher signed for its own id —
/// every signature valid, author and delivery, raw and resolved. Only the
/// signed `doc` claim says it is not the pinned document, and the read is
/// refused on it: with a `reader` and without, raw and resolved.
#[test]
fn a_document_signed_for_another_id_is_refused_under_the_pin() {
    let key = publisher_key_file();
    for (uri, reader) in [
        ("instruction://ins_mock@foreign", None),
        (
            "instruction://ins_mock@foreign",
            Some("agent://mock-reader"),
        ),
        (
            "instruction://ins_mock@foreign?resolved=1",
            Some("agent://mock-reader"),
        ),
    ] {
        let mut pin = json!({
            "uri": "instruction://ins_mock@foreign",
            "publisher": "https://instruction.md/pub/mock",
            "author_keys": [key.clone()],
            "delivery_keys": [],
        });
        if let Some(r) = reader {
            pin["reader"] = json!(r);
        }
        let r = boot(uri, json!([pin]));
        assert!(
            !r.log.contains("proc.ready"),
            "{uri} (reader {reader:?}) must not start:\n{}",
            r.log
        );
        assert!(
            r.log.contains("\\\"instruction://ins_other\\\" is not")
                || r.log
                    .contains("the signature is for \\\"instruction://ins_other\\\""),
            "{uri} (reader {reader:?}): the refusal names the document signed:\n{}",
            r.log
        );
    }
    let _ = std::fs::remove_file(&key);
}

#[test]
fn verification_fails_closed_on_a_wrong_key_and_a_wrong_reader() {
    // A key that did not sign this document: refuse, do not degrade.
    let wrong = common::unique_path("wrong-pub", "key");
    std::fs::write(&wrong, "a".repeat(43)).unwrap();
    let r = boot(
        "instruction://ins_mock@stable",
        json!([{
            "uri": "instruction://ins_mock@stable",
            "publisher": "https://instruction.md/pub/mock",
            "author_keys": [wrong.clone()],
        }]),
    );
    assert!(
        !r.log.contains("proc.ready"),
        "a bad signature must not start:\n{}",
        r.log
    );
    assert!(
        r.log.contains("signature verification failed")
            || r.log.contains("no author verification key"),
        "the refusal names the signature problem:\n{}",
        r.log
    );

    // The right key, but the delivery is addressed to a different reader.
    let key = publisher_key_file();
    let r = boot(
        "instruction://ins_mock@stable",
        json!([{
            "uri": "instruction://ins_mock@stable",
            "publisher": "https://instruction.md/pub/mock",
            "author_keys": [key.clone()],
            "delivery_keys": [],
            "reader": "agent://someone-else",
        }]),
    );
    assert!(!r.log.contains("proc.ready"));
    assert!(
        r.log.contains("not this reader"),
        "the refusal names the audience mismatch:\n{}",
        r.log
    );
    for p in [wrong, key] {
        let _ = std::fs::remove_file(&p);
    }
}

#[test]
fn an_unsigned_read_under_a_pinned_publisher_is_refused() {
    // `mock://instruction` is served WITHOUT attestations; pinning a
    // publisher for it must refuse rather than accept unsigned bytes.
    let r = boot(
        "mock://instruction",
        json!([{
            "uri": "mock://instruction",
            "publisher": "https://instruction.md/pub/mock",
        }]),
    );
    assert!(!r.log.contains("proc.ready"), "must not start:\n{}", r.log);
    assert!(
        r.log.contains("no author signature"),
        "the refusal names the missing signature:\n{}",
        r.log
    );
}

/// Run the real daemon on a registry instruction, with the in-process mock
/// intelligence echoing the system prompt it is sent and a context template
/// that is the instruction alone — so a run's output is exactly the text the
/// model received. A followed resource keeps the daemon from idling, so it is
/// stopped once the run is done (or it exits on its own).
fn run_echo(instruction: &str) -> String {
    run_echo_as(instruction, "mock")
}

/// [`run_echo`] with the operator's `intelligence.model` — the model the
/// instruction's variants are chosen for.
fn run_echo_as(instruction: &str, model: &str) -> String {
    let mock = common::spawn_mock_mcp("mock://watched", false);
    let cfg = json!({
        "agent": {"name": "reg-resolved", "preflight": "never",
                  "instruction": {"mcp": instruction}},
        "mcp": {"servers": [{"name": "registry", "endpoint": format!("{}/mcp", mock.uri())}]},
        "intelligence": {"endpoints": "mock:echo-system", "model": model},
        "context": {"template": "{{instruction}}"},
        "store": {"kind": "memory"},
        "observability": {"log_level": "info", "log_content": true},
        "workflows": [{"name": "w", "steps": {
            "s": {"kind": "once"},
            "a": {"kind": "agent", "depends_on": ["s"], "instruction": "go"},
            "f": {"kind": "finish", "depends_on": ["a"], "status": "completed",
                  "output": "{{steps.a.output}}"},
        }}],
    });
    let cfg_path = common::unique_path("reg-resolved", "json");
    std::fs::write(&cfg_path, serde_json::to_vec(&cfg).unwrap()).unwrap();
    let err_path = common::unique_path("reg-resolved", "log");
    let errf = std::fs::File::create(&err_path).unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_agentd"))
        .args(["-c", &cfg_path])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(errf)
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(25);
    loop {
        let log = std::fs::read_to_string(&err_path).unwrap_or_default();
        if log.contains("\"run.done\"")
            || log.contains("proc.exit")
            || child.try_wait().is_ok_and(|s| s.is_some())
        {
            break;
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            panic!("the run neither finished nor exited:\n{log}");
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let _ = child.kill();
    let _ = child.wait();
    let log = std::fs::read_to_string(&err_path).unwrap_or_default();
    for p in [cfg_path, err_path] {
        let _ = std::fs::remove_file(&p);
    }
    log
}

/// A RESOLVED read (RFC-0015 `md.instruction/resolution: resolved`) is text
/// the registry already delivered for this reader: it reaches the model byte
/// for byte. Delivered again, its quoted `:::must` fence would become a live
/// rule and its quoted `<!--` would become a note — what a consumer that
/// keyed on `deliveredDigest` alone (stamped on every read, raw ones too)
/// would also have done to a raw read's machinery, the other way round.
#[test]
fn a_resolved_read_reaches_the_model_as_served() {
    let log = run_echo("instruction://ins_mock@stable?resolved=1");
    let loaded = event(&log, "instruction.loaded")
        .unwrap_or_else(|| panic!("no instruction.loaded:\n{log}"));
    assert_eq!(loaded["resolution"], "resolved", "{loaded}");
    let done = event(&log, "run.done").unwrap_or_else(|| panic!("no run.done:\n{log}"));
    let resolved = agentd::mcp::mock_http::REGISTRY_RESOLVED_TEXT;
    assert_eq!(
        done["output"].as_str().map(str::trim_end),
        Some(resolved.trim_end()),
        "the model received the registry's delivered text unchanged:\n{log}"
    );
}

/// A raw read is delivered here, as before: the model reads the cleaned
/// document — its machinery acknowledged, never its fence.
#[test]
fn a_raw_read_is_still_delivered_here() {
    let log = run_echo("instruction://ins_mock@stable");
    let loaded = event(&log, "instruction.loaded")
        .unwrap_or_else(|| panic!("no instruction.loaded:\n{log}"));
    assert_eq!(loaded["resolution"], "raw", "{loaded}");
    let done = event(&log, "run.done").unwrap_or_else(|| panic!("no run.done:\n{log}"));
    let out = done["output"].as_str().unwrap_or_default();
    assert!(out.contains("Serve the mock."), "{log}");
    assert!(
        !out.contains(":::workflow"),
        "a raw read's machinery never reaches the model:\n{out}"
    );
}

/// A resolved read whose bytes are not the ones its `deliveredDigest` names
/// is refused, not adopted: at boot the daemon does not start on it.
#[test]
fn a_resolved_read_under_another_digest_is_refused() {
    let r = boot("instruction://ins_mock@stable?resolved=stale", json!([]));
    assert!(!r.log.contains("proc.ready"), "must not start:\n{}", r.log);
    assert!(
        r.log
            .contains("registry delivered text does not match its deliveredDigest"),
        "the refusal names the mismatch:\n{}",
        r.log
    );
}

#[test]
fn a_pinned_resolved_read_verifies_through_its_delivery_attestation() {
    let key = publisher_key_file();
    let r = boot(
        "instruction://ins_mock@stable?resolved=1",
        json!([{
            "uri": "instruction://ins_mock@stable",
            "publisher": "https://instruction.md/pub/mock",
            "author_keys": [key.clone()],
            "reader": "agent://mock-reader",
            "max_capabilities": ["compute"],
        }]),
    );
    let v = event(&r.log, "instruction.verified")
        .unwrap_or_else(|| panic!("no verification:\n{}", r.log));
    assert_eq!(v["delivery_checked"], true, "{v}");
    let loaded = event(&r.log, "instruction.loaded")
        .unwrap_or_else(|| panic!("no instruction.loaded:\n{}", r.log));
    assert_eq!(loaded["resolution"], "resolved", "{loaded}");
    assert!(
        r.log.contains("proc.ready"),
        "startup completed:\n{}",
        r.log
    );
    let _ = std::fs::remove_file(&key);
}

/// …and without a `reader`, nothing binds the delivered bytes to what the
/// author signed: the read is refused by that name, not as a digest
/// mismatch it is not.
#[test]
fn a_pinned_resolved_read_without_a_reader_is_refused_by_name() {
    let key = publisher_key_file();
    let r = boot(
        "instruction://ins_mock@stable?resolved=1",
        json!([{
            "uri": "instruction://ins_mock@stable",
            "publisher": "https://instruction.md/pub/mock",
            "author_keys": [key.clone()],
        }]),
    );
    assert!(!r.log.contains("proc.ready"), "must not start:\n{}", r.log);
    assert!(
        r.log
            .contains("a resolved read is the registry's delivered text"),
        "the refusal names the resolved read:\n{}",
        r.log
    );
    let _ = std::fs::remove_file(&key);
}

/// A registry slow to take the report holds nothing up: while it sits on
/// `instructions_bindings_report`, the daemon is ready and answers a status
/// read at once; then the binding's lines come, created before reported, and
/// the report follows the apply by the registry's round trip.
#[cfg(feature = "a2a")]
#[test]
fn a_slow_registry_report_holds_up_no_read() {
    const DELAY_MS: u64 = 3_000;
    let mock = common::spawn_mock_mcp("mock://watched", false);
    mock.control("mock.bindings_delay", json!({"ms": DELAY_MS}));
    let boot = |port: u16| {
        let cfg = json!({
            "agent": {"name": "reg-slow", "preflight": "never",
                      "instruction": {"mcp": "instruction://ins_mock@stable"},
                      "document_capabilities": ["compute"]},
            "mcp": {"servers": [{"name": "registry", "endpoint": format!("{}/mcp", mock.uri())}]},
            "intelligence": {"endpoints": ["http://127.0.0.1:1/v1"], "model": "mock"},
            "a2a": {"listen": format!("http://127.0.0.1:{port}")},
            "store": {"kind": "memory"},
            "observability": {"log_level": "info"},
        });
        let cfg_path = common::unique_path("reg-slow", "json");
        std::fs::write(&cfg_path, serde_json::to_vec(&cfg).unwrap()).unwrap();
        let err_path = common::unique_path("reg-slow", "log");
        let errf = std::fs::File::create(&err_path).unwrap();
        let child = Command::new(env!("CARGO_BIN_EXE_agentd"))
            .args(["-c", &cfg_path])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(errf)
            .spawn()
            .unwrap();
        (Child(child, cfg_path, err_path.clone()), err_path)
    };
    let (daemon, addr) = common::spawn_bound(boot);
    let log = || std::fs::read_to_string(&daemon.2).unwrap_or_default();
    // The registry has the report, and has not answered it.
    let deadline = Instant::now() + Duration::from_secs(20);
    while !mock
        .log()
        .contains("MOCK_BINDINGS instructions_bindings_report")
    {
        assert!(
            Instant::now() < deadline,
            "no report sent:\n{}\n{}",
            log(),
            mock.log()
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    let asked = Instant::now();
    let status = common::SendMessage::command("status", json!({})).result(&addr);
    let took = asked.elapsed();
    assert!(
        !mock.log().contains("MOCK_BINDINGS_ANSWERED"),
        "the status read waited for the report ({took:?}):\n{}",
        log()
    );
    assert!(
        took < Duration::from_millis(DELAY_MS / 2),
        "{took:?}: {status}"
    );
    while !reported_or_failed(&log()) {
        assert!(Instant::now() < deadline, "no report:\n{}", log());
        std::thread::sleep(Duration::from_millis(20));
    }
    let log = log();
    assert!(event(&log, "instruction.binding.fail").is_none(), "{log}");
    let at = |name: &str| {
        log.lines()
            .position(|l| l.contains(&format!("\"{name}\"")))
            .unwrap_or_else(|| panic!("no {name}:\n{log}"))
    };
    assert!(
        at("instruction.applied") < at("instruction.binding.created"),
        "{log}"
    );
    assert!(
        at("instruction.binding.created") < at("instruction.binding.reported"),
        "{log}"
    );
    assert!(
        at("proc.ready") < at("instruction.binding.reported"),
        "{log}"
    );
    assert_eq!(
        event(&log, "instruction.binding.reported").unwrap()["version_id"],
        "ver_mock_1"
    );
}

/// A daemon stopped when the test is done with it.
#[cfg(feature = "a2a")]
struct Child(std::process::Child, String, String);

#[cfg(feature = "a2a")]
impl Drop for Child {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
        let _ = std::fs::remove_file(&self.1);
        let _ = std::fs::remove_file(&self.2);
    }
}
