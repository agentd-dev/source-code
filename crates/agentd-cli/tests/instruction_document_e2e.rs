// SPDX-License-Identifier: AGPL-3.0-only
//! **The instruction document as the whole agent, end to end.**
//!
//! One instruction/1 document — guidance, people, an MCP server, an A2A
//! agent, a workflow, a configuration block, a skill — is loaded by the real
//! binary. It asserts what the unit tests cannot: that the document validates
//! through the actual config loader, that what it declares is visible in
//! `--capabilities`, that the agent text is the instruction.md SDK's byte for
//! byte, and that what agentd will not activate stays text and is said.
#![cfg(unix)]

use std::process::Command;

use serde_json::{Value, json};

fn load(instruction: &str, capabilities: &[&str]) -> (bool, String, Value) {
    load_with(instruction, capabilities, Value::Null)
}

/// The configuration a document is loaded under, with `extra` top-level
/// sections — what the deployment around the document provides.
fn config_for(instruction: &str, capabilities: &[&str], extra: Value) -> Value {
    let mut cfg = json!({
        "agent": {
            "name": "idoc-e2e", "preflight": "never",
            "instruction": instruction,
            "document_capabilities": capabilities,
        },
        "intelligence": {"endpoints": ["http://127.0.0.1:1/v1"], "model": "mock"},
        "store": {"kind": "memory"},
    });
    if let (Some(cfg), Some(extra)) = (cfg.as_object_mut(), extra.as_object()) {
        cfg.extend(extra.clone());
    }
    cfg
}

fn scratch(tag: &str) -> std::path::PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    std::env::temp_dir().join(format!(
        "idoc-e2e-{tag}-{}-{}.json",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    ))
}

/// Validate `cfg` with the binary, then read its `--capabilities`.
fn validate(cfg: &Value) -> (bool, String, Value) {
    let path = scratch("cfg");
    std::fs::write(&path, serde_json::to_vec(cfg).unwrap()).unwrap();
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

/// [`load`] under [`config_for`]`(…, extra)`.
fn load_with(instruction: &str, capabilities: &[&str], extra: Value) -> (bool, String, Value) {
    validate(&config_for(instruction, capabilities, extra))
}

/// Load `instruction` through agentd's own config loader — the one the
/// daemon runs — under [`config_for`].
fn settings(instruction: &str, capabilities: &[&str]) -> agentd::config::settings::Settings {
    let path = scratch("settings");
    let cfg = config_for(instruction, capabilities, Value::Null);
    std::fs::write(&path, serde_json::to_vec(&cfg).unwrap()).unwrap();
    let loaded = agentd::config::settings::load(
        &["--config".to_string(), path.to_str().unwrap().to_string()],
        &[],
    );
    let _ = std::fs::remove_file(&path);
    loaded
        .unwrap_or_else(|e| panic!("the document did not load: {e:?}"))
        .0
        .settings
}

/// The whole surface, one document.
const FULL: &str = r#"---
spec: "1"
title: Support triage
parameters:
  sla: { type: string, default: "1 hour" }
---
# Support triage

You triage tickets with the :mcp[search]{ref=#search} and hand deploys to the
:agent[deployer]{ref=#deployer}. Escalations go to :person[the on-call]{ref=#oncall}.

::person{id=oncall title="On call" role=approver}

:::mcp{id=search title="Search" src="https://mcp.internal.example/search" tools="query,fetch" deny="fetch"}
The ticket index. Read only.
:::

::agent{id=deployer title="Deployer" protocol=a2a endpoint="https://deploy.internal.example/a2a" binding=JSONRPC}

::tool{id=lookup title="Lookup" server=search name=query}

MUST: answer enterprise tickets within ${sla}.
BECAUSE: it is in the contract.

:::policy{id=refunds title="Refunds"}
NEVER: promise a refund.
:::

:::config{id=runtime dialect="agentd/config"}
limits: { max_runs: 8 }
streams:
  tickets: { retention: { max_events: 500 } }
:::

:::workflow{id=triage dialect="instruction/workflow"}
name: triage
steps:
  take: {kind: stream, stream: tickets, subject: "t.*", from: earliest}
  look: {kind: mcp.tool, depends_on: [take], server: search, tool: query}
  ask:  {kind: human, depends_on: [look], to: oncall, question: "Escalate?"}
  f:    {kind: finish, depends_on: [ask]}
:::

:::skill{id=tone title="House tone" trigger="writing to a customer"}
Be brief and kind.
:::

:::example{title="A good reply"}
Thanks — we are on it.
:::
"#;

#[test]
fn a_full_document_loads_every_element() {
    let (valid, err, caps) = load(FULL, &["identity"]);
    assert!(valid, "the document did not validate:\n{err}");
    let wf: Vec<&str> = caps["workflows"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|w| w["name"].as_str())
        .collect();
    assert_eq!(wf, ["triage"], "the workflow loaded");
    assert_eq!(
        caps["mcp_servers"].as_array().map(Vec::len),
        Some(1),
        "the mcp server loaded: {}",
        caps["mcp_servers"]
    );
    // A connected object agentd records but does not activate.
    assert_eq!(caps["document"]["declarations"]["tool"], 1, "{caps}");
    assert!(
        !err.contains("host-policy"),
        "nothing was held back:\n{err}"
    );

    let s = settings(FULL, &["identity"]);
    let search = s.mcp.servers.iter().find(|m| m.name == "search").unwrap();
    assert_eq!(search.endpoint, "https://mcp.internal.example/search");
    assert_eq!(
        search.allow.as_deref(),
        Some(&["query".to_string(), "fetch".to_string()][..])
    );
    assert_eq!(search.exclude, ["fetch"]);
    let peer = s.a2a.peers.iter().find(|p| p.name == "deployer").unwrap();
    assert_eq!(peer.endpoint, "https://deploy.internal.example/a2a");
    assert_eq!(s.limits.max_runs, Some(8));
    assert!(s.streams.contains_key("tickets"));
    assert_eq!(s.agent.inline_skills[0].name, "tone");
    let w = s.workflows.iter().find(|w| w["name"] == "triage").unwrap();
    assert_eq!(
        w["steps"]["ask"]["to"], "operator",
        "a gate to a person the document declares is the operators'"
    );
}

/// The agent text is the instruction.md SDK's, byte for byte: agentd adds
/// nothing, drops nothing, re-renders nothing.
#[test]
fn the_agent_text_is_the_sdks_byte_for_byte() {
    let s = settings(FULL, &["identity"]);
    let (sdk, diagnostics) = instruction_md::deliver(FULL, None);
    assert!(diagnostics.is_empty(), "{diagnostics:?}");
    assert_eq!(s.agent.instruction.as_deref(), Some(sdk.as_str()));
    // Declared defaults are substituted; metadata never reaches the model.
    assert!(sdk.contains("within 1 hour."), "{sdk}");
    assert!(!sdk.contains("spec: \"1\""), "{sdk}");
}

/// An A2A agent outside the grants is not activated: it stays in the agent
/// text, unresolved, and the load says so as a host-policy diagnostic (§17).
#[test]
fn an_ungranted_agent_stays_text_and_is_said() {
    let (valid, err, caps) = load(FULL, &[]);
    assert!(valid, "{err}");
    assert!(
        err.contains("agent \\\"deployer\\\" is not activated")
            || err.contains("agent \"deployer\" is not activated"),
        "{err}"
    );
    assert!(
        err.contains("host-policy") && err.contains("identity"),
        "{err}"
    );
    assert_eq!(caps["document"]["declarations"]["agent"], 1, "{caps}");
    let s = settings(FULL, &[]);
    assert!(s.a2a.peers.is_empty());
    assert!(
        s.agent
            .instruction
            .as_deref()
            .is_some_and(|t| t.contains("Deployer")),
        "the agent stays in the text"
    );
}

/// A gate to a person the document declares is the operators'; one naming
/// nobody it declares is refused by the gate rule.
#[test]
fn a_gate_to_a_person_nobody_declared_is_refused() {
    let doc = FULL.replace("to: oncall", "to: nobody");
    let (valid, err, _) = load(&doc, &["identity"]);
    assert!(!valid, "a gate to an undeclared person loaded");
    assert!(err.contains("`to` names nobody"), "{err}");
}

#[test]
fn a_workflow_without_a_dialect_is_refused_naming_the_attribute() {
    let (valid, err, _) = load(
        ":::workflow{id=w}\nname: w\nsteps: {f: {kind: finish}}\n:::\n",
        &[],
    );
    assert!(!valid);
    assert!(err.contains("needs dialect"), "{err}");
}

/// The 1.1 sigil is not instruction/1 syntax: such a document is refused,
/// naming the line with the SDK's legacy-syntax code, never delivered as
/// prose its author did not mean.
#[test]
fn a_sigiled_block_is_refused_as_legacy_syntax() {
    let (valid, err, _) = load(
        ":::!workflow{name=w}\nsteps: {s: {kind: once}, f: {kind: finish, depends_on: [s]}}\n:::\n",
        &[],
    );
    assert!(!valid, "a 1.1 document loaded");
    assert!(
        err.contains("line 1: ") && err.contains("(instruction.md/legacy-syntax)"),
        "{err}"
    );
}

/// An `instruction/workflow` body the language refuses is the SDK's
/// `invalid-workflow`, refusing the document before agentd's own check.
#[test]
fn a_workflow_the_language_refuses_is_the_sdks_diagnostic() {
    let (valid, err, _) = load(
        ":::workflow{id=w dialect=instruction/workflow}\nname: w\nsteps:\n  s: {kind: manual}\n  pause: {kind: wait, depends_on: [s], timeout: 1m}\n  f: {kind: finish, depends_on: [pause]}\n:::\n",
        &[],
    );
    assert!(!valid, "a wait with no `on` loaded");
    assert!(err.contains("(invalid-workflow)"), "{err}");
}

/// A document with no block at all: rules, a reason, a comment and
/// backmatter. Each is something the agent text renders or removes, so the
/// loader delivers it — read raw, the model would see the comment and the
/// record the spec says it never sees.
const FENCE_FREE: &str = "# Support

You answer tickets.

MUST (if customer-facing): Cite the ticket number.
BECAUSE: Customers quote it back to us.

<!-- AUTHOR-NOTE: tighten this before launch -->
SHOULD: Keep replies short.

---
owners: [support-team]
review: { next: \"2026-12-01\" }
";

#[test]
fn a_fence_free_document_is_delivered_not_read_raw() {
    let s = settings(FENCE_FREE, &[]);
    let delivered = s.agent.instruction.expect("an instruction");
    assert_eq!(delivered, instruction_md::deliver(FENCE_FREE, None).0);
    for rendered in [
        "MUST (if customer-facing): Cite the ticket number.",
        "BECAUSE: Customers quote it back to us.",
        "SHOULD: Keep replies short.",
    ] {
        assert!(
            delivered.contains(rendered),
            "{rendered:?} in:\n{delivered}"
        );
    }
    for gone in ["AUTHOR-NOTE", "<!--", "owners:", "support-team", "review:"] {
        assert!(
            !delivered.contains(gone),
            "{gone:?} delivered:\n{delivered}"
        );
    }
    // Plain prose is still the model's byte for byte.
    let prose = "You answer tickets.\n\nBe brief and stay polite.\n";
    let s = settings(prose, &[]);
    assert_eq!(s.agent.instruction.as_deref(), Some(prose));
}

/// Backmatter that is not a YAML mapping refuses the document.
#[test]
fn malformed_backmatter_refuses_the_document() {
    let (valid, err, _) = load("Be brief.\n\n---\nowners: [unclosed\n", &[]);
    assert!(!valid, "malformed backmatter loaded");
    assert!(err.contains("agent.instruction"), "{err}");
    assert!(err.contains("invalid-metadata"), "{err}");
}

/// Serve `body` to every GET on a loopback port; returns the document's URL.
fn serve_document(body: &'static str) -> String {
    use std::io::{Read, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for mut conn in listener.incoming().flatten() {
            let mut buf = [0u8; 4096];
            let _ = conn.read(&mut buf);
            let _ = write!(
                conn,
                "HTTP/1.1 200 OK\r\nContent-Type: text/markdown\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
        }
    });
    format!("http://127.0.0.1:{port}/agent.md")
}

/// The §7.8 floor: an A2A agent in a document fetched from a URL is never
/// activated, whatever the operator granted — it stays text, said. The same
/// text from the operator's own configuration activates under the grant.
#[test]
fn a_document_from_the_wire_activates_no_agent() {
    const AGENT: &str = "You review.\n\n::agent{id=reviewer title=\"Reviewer\" protocol=a2a endpoint=\"https://r.example/a2a\"}\n";
    let (valid, err, _) = load(AGENT, &["identity"]);
    assert!(valid, "the operator's own text did not load:\n{err}");
    assert!(!err.contains("host-policy"), "{err}");
    let mut cfg = config_for("", &["identity"], Value::Null);
    cfg["agent"]["instruction"] = json!({"url": serve_document(AGENT)});
    let (valid, err, _) = validate(&cfg);
    assert!(valid, "{err}");
    assert!(err.contains("§7.8") && err.contains("host-policy"), "{err}");
}

/// Nor does a fetched document reach a peer or a credential through a
/// configuration block: `a2a.peers`, or a secret in a server's headers,
/// which would send it to the document's endpoint.
#[test]
fn a_document_from_the_wire_reaches_no_peer_or_secret() {
    const PEERS: &str = "You help.\n\n:::config{id=c dialect=agentd/config}\na2a:\n  peers:\n    - {name: p, endpoint: \"https://p.example\"}\n:::\n";
    const SECRET: &str = "You help.\n\n:::config{id=c dialect=agentd/config}\nmcp:\n  servers:\n    - {name: s, endpoint: \"https://s.example/mcp\", headers: {Authorization: \"Bearer {{secret:TOKEN}}\"}}\n:::\n";
    for (body, says) in [(PEERS, "writes a2a.peers"), (SECRET, "names a secret")] {
        let mut cfg = config_for("", &[], Value::Null);
        cfg["agent"]["instruction"] = json!({"url": serve_document(body)});
        let path = scratch("wire-effects");
        std::fs::write(&path, serde_json::to_vec(&cfg).unwrap()).unwrap();
        let v = Command::new(env!("CARGO_BIN_EXE_agentd"))
            .args(["-c", path.to_str().unwrap(), "--validate-config"])
            .env("TOKEN", "t")
            .output()
            .unwrap();
        let _ = std::fs::remove_file(&path);
        let err = String::from_utf8_lossy(&v.stderr);
        assert_eq!(v.status.code(), Some(2), "{says}:\n{err}");
        assert!(err.contains("§7.8") && err.contains(says), "{err}");
    }
}
