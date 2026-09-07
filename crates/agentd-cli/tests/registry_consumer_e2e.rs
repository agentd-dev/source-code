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
#![cfg(all(
    unix,
    feature = "internal-mocks",
    feature = "sign",
    feature = "workflow"
))]

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

/// Boot the real daemon against the mock registry and capture its log.
fn boot(instruction: &str, sources: Value) -> Run {
    let mock = common::spawn_mock_mcp("mock://watched", false);
    let cfg = json!({
        "config_version": "1",
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
        if log.contains("proc.ready")
            || log.contains("proc.exit")
            || child.try_wait().is_ok_and(|s| s.is_some())
        {
            break;
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            panic!("daemon neither became ready nor exited:\n{log}");
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

fn event(log: &str, name: &str) -> Option<Value> {
    log.lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .find(|v| v["event"] == name)
}

#[test]
fn a_registry_read_captures_its_version_and_reports_the_binding() {
    let r = boot("instruction://ins_mock@stable", json!([]));
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
    assert!(
        event(&r.log, "instruction.binding.created").is_some(),
        "no binding created:\n{}",
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
    // BOUNDARY, asserted so it cannot change silently: a registry document
    // resolved at RUNTIME contributes its DELIVERED TEXT only — its machinery
    // does not join the runtime config, because config folding happens at
    // LOAD (the `oci://` path folds for exactly that reason). So the workflow
    // this document declares is acknowledged to the model but not armed here.
    assert!(
        !r.log.contains("workflow.loaded") || !r.log.contains("mock-drain"),
        "runtime-resolved registry machinery is not expected to arm; if this \
         now folds, the boundary changed and this test should assert the new \
         behaviour:\n{}",
        r.log
    );
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
        r.log.contains("does not verify") || r.log.contains("no author verification key"),
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
