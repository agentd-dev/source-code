// SPDX-License-Identifier: AGPL-3.0-only
//! **The instruction document as the whole agent, end to end.**
//!
//! A single dialect-2 instruction document — prose, core machinery, and blocks
//! from every gated family — is loaded by the real binary. It asserts three
//! things the unit tests cannot: that the document validates through the actual
//! config loader, that every element is visible in `--capabilities`, and that
//! the trust ladder refuses an ungranted family naming the exact grant to add.
#![cfg(all(unix, feature = "workflow"))]

use std::process::Command;

use serde_json::{Value, json};

fn load(instruction: &str, capabilities: &[&str]) -> (bool, String, Value) {
    let cfg = json!({
        "config_version": "1",
        "agent": {
            "name": "idoc-e2e", "preflight": "never",
            "instruction": instruction,
            "document_capabilities": capabilities,
        },
        "intelligence": {"endpoints": ["http://127.0.0.1:1/v1"], "model": "mock"},
        "store": {"kind": "memory"},
    });
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let path = std::env::temp_dir().join(format!(
        "idoc-e2e-{}-{}.json",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::write(&path, serde_json::to_vec(&cfg).unwrap()).unwrap();
    let v = Command::new(env!("CARGO_BIN_EXE_agentd"))
        .args(["-c", path.to_str().unwrap(), "--validate-config"])
        .output()
        .unwrap();
    let valid = v.status.success();
    let errtext = format!(
        "{}{}",
        String::from_utf8_lossy(&v.stdout),
        String::from_utf8_lossy(&v.stderr)
    );
    let caps = if valid {
        let c = Command::new(env!("CARGO_BIN_EXE_agentd"))
            .args(["-c", path.to_str().unwrap(), "--capabilities"])
            .output()
            .unwrap();
        serde_json::from_slice(&c.stdout).unwrap_or(Value::Null)
    } else {
        Value::Null
    };
    let _ = std::fs::remove_file(&path);
    (valid, errtext, caps)
}

/// The whole surface, one document: it validates, and every element loads.
const FULL: &str = r#"---
spec: "1"
---
# Support triage

You triage tickets. Be brief.

:::must
Never promise a refund.
:::

:::!config
limits: { max_runs: 8 }
:::

:::!stream{name=tickets}
retention: { max_events: 500 }
:::

:::!mcp{name=search}
endpoint: https://mcp.internal/search
:::

:::!workflow{name=drain}
steps:
  take: {kind: stream, stream: tickets, subject: "t.*", from: earliest}
  f:    {kind: finish, depends_on: [take]}
:::

:::!file{name=readme path=README.md}
# generated
:::

:::!data{name=slo}
tiers: [gold, silver]
:::

:::!knowledge{name=kb}
server: kb
:::

:::!runtime{name=py}
image: ghcr.io/acme/py@sha256:abc
service: sandbox
:::

::::!function{name=lint runtime=@runtime/py}
doc: lint a diff
::::

:::!human{name=oncall}
role: approver
:::

:::!peer{name=deployer}
endpoint: https://deploy.internal:8443
:::

:::!agent{name=reviewer}
template: code-reviewer
:::

:::context{title="SLA"}
Enterprise: 1 hour.
:::
"#;

#[test]
fn a_full_document_loads_every_element() {
    let all = [
        "material",
        "knowledge",
        "interface",
        "identity",
        "compute",
        "infra",
        "compose",
    ];
    let (valid, err, caps) = load(FULL, &all);
    assert!(valid, "the document did not validate:\n{err}");

    // Core machinery folded into real config.
    let wf: Vec<&str> = caps["workflows"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|w| w["name"].as_str())
        .collect();
    assert_eq!(wf, ["drain"], "the workflow loaded");
    assert_eq!(
        caps["mcp_servers"].as_array().unwrap().len(),
        1,
        "the mcp server loaded"
    );

    // The document surface reports what was granted and what loaded.
    let doc = &caps["document"];
    assert_eq!(doc["spec"], "instruction-document/2");
    let decl = doc["declarations"]
        .as_object()
        .expect("declarations present");
    for kind in [
        "file",
        "data",
        "knowledge",
        "runtime",
        "function",
        "human",
        "agent",
    ] {
        assert!(
            decl.contains_key(kind),
            "{kind} did not load into the document surface: {decl:?}"
        );
    }
    // `peer` folds into real a2a config rather than the declaration surface.
    assert!(
        !caps["document"]["declarations"]
            .as_object()
            .unwrap()
            .contains_key("peer")
    );
}

#[test]
fn an_ungranted_family_is_refused_naming_the_grant() {
    // The same document with NO grants: the gated families are refused, each
    // naming the capability to add. The default rung (workflow/mcp/prose) is
    // never the reason.
    let (valid, err, _) = load(FULL, &[]);
    assert!(
        !valid,
        "a document using gated families with no grant must be refused"
    );
    for grant in [
        "material",
        "compute",
        "interface",
        "compose",
        "knowledge",
        "identity",
    ] {
        assert!(
            err.contains(&format!("`{grant}` capability")),
            "the refusal should name the {grant} grant:\n{err}"
        );
    }
}

#[test]
fn a_forgotten_sigil_is_refused_not_silently_demoted() {
    // Bare `:::workflow` (machinery without its sigil) is the trap dialect 2
    // closes: refused, pointing at the sigiled form — never silently prose.
    let (valid, err, _) = load(
        "---\nspec: \"1\"\n---\n:::workflow{name=w}\nsteps: {f: {kind: finish}}\n:::",
        &[],
    );
    assert!(!valid);
    assert!(err.contains(":::!workflow"), "names the fix:\n{err}");
}

#[test]
fn prose_degrades_into_the_delivered_instruction() {
    // A pure-prose dialect-2 document validates and its guidance survives — the
    // degradation contract, black-box.
    let (valid, err, _) = load(
        "---\nspec: \"1\"\n---\n:::must\nAlways cite sources.\n:::",
        &[],
    );
    assert!(valid, "{err}");
}
