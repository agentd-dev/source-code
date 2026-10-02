// SPDX-License-Identifier: AGPL-3.0-only
//! **The instruction document as the whole agent, end to end.**
//!
//! A single dialect-2 instruction document — prose, core machinery, and blocks
//! from every gated family — is loaded by the real binary. It asserts three
//! things the unit tests cannot: that the document validates through the actual
//! config loader, that every element is visible in `--capabilities`, and that
//! the trust ladder refuses an ungranted family naming the exact grant to add.
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

/// [`load`] under [`config_for`]`(…, extra)`.
fn load_with(instruction: &str, capabilities: &[&str], extra: Value) -> (bool, String, Value) {
    let cfg = config_for(instruction, capabilities, extra);
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
    assert_eq!(doc["spec"], "instruction/1");
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

/// Every family, granted.
const ALL: [&str; 7] = [
    "material",
    "knowledge",
    "interface",
    "identity",
    "compute",
    "infra",
    "compose",
];

/// Load `instruction` through agentd's own config loader — the one the
/// daemon runs — with every family granted, under [`config_for`].
fn settings(instruction: &str, extra: Value) -> agentd::config::settings::Settings {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let path = std::env::temp_dir().join(format!(
        "idoc-e2e-settings-{}-{}.json",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    let cfg = config_for(instruction, &ALL, extra);
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

/// Every `human` step of every workflow, as `(workflow, step, to)`.
fn human_steps(s: &agentd::config::settings::Settings) -> Vec<(String, String, Value)> {
    s.workflows
        .iter()
        .flat_map(|w| {
            let name = w["name"].as_str().unwrap_or("?").to_string();
            w["steps"]
                .as_object()
                .into_iter()
                .flatten()
                .filter(|(_, st)| st["kind"] == "human")
                .map(move |(id, st)| (name.clone(), id.clone(), st["to"].clone()))
                .collect::<Vec<_>>()
        })
        .collect()
}

/// The spec's own conformance examples address their `human` steps to the
/// document's `::!human`s, which declare a `channel` and no principal. They
/// load through agentd, each such gate answered by the operators — exactly as
/// `to: operator` — and announced on the channel the document declared. The
/// instruction crate's fold, and so the corpus's expected output, is
/// untouched: agentd reads the principal and the channel from the parsed
/// document rather than from the one folded string.
#[test]
fn the_spec_corpus_documents_load_with_their_gates_answered_by_operators() {
    let corpus = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../instruction/tests/conformance/corpus"
    );
    for (case, want) in [
        (
            "spec-example",
            vec![("approve-refund", "ask", "@channel/ops")],
        ),
        (
            "support-agent",
            vec![
                ("approve-refund", "ask", "@channel/ops"),
                ("reopen", "notify", "@channel/ops"),
            ],
        ),
        (
            "deploy-runbook",
            vec![
                ("deploy", "gate1", "@channel/deploys"),
                ("deploy", "gate2", "@channel/deploys"),
                ("rollback", "page", "@channel/oncall"),
            ],
        ),
        (
            "orchestrator",
            vec![("reroute", "which", "@channel/dispatch")],
        ),
        ("coding-agent", vec![("ci-triage", "ask", "@channel/eng")]),
    ] {
        // What a deployment provides around a document that declares an
        // endpoint and a mounted secret: the webhook listener, and the file
        // the secret is mounted at — here a scratch copy, so the one path the
        // document names outside itself is the only thing that differs.
        let secret = std::env::temp_dir().join(format!("idoc-e2e-secret-{}", std::process::id()));
        std::fs::write(&secret, "s3cret").unwrap();
        let doc = std::fs::read_to_string(format!("{corpus}/{case}/doc.md"))
            .unwrap()
            .replace("/var/run/secrets/deployer", secret.to_str().unwrap());
        // Validated, never bound: nothing here starts the daemon.
        let env = json!({"webhooks": {"listen": "http://127.0.0.1:18081"}});
        // The binary's own validation, as a deployment runs it.
        let (valid, err, _) = load_with(&doc, &ALL, env.clone());
        // Three of the documents' workflows carry `when:` CEL conditions, and
        // two schedule with `cron:`; a build without `cel` or `cron` refuses
        // them at load rather than run them half-armed. Such a row checks
        // exactly that — refused, naming a feature it lacks — so no row
        // passes a case over in silence. `cron` is read off the document
        // itself, so a corpus that gains a schedule cannot slip past.
        let mut missing: Vec<&str> = Vec::new();
        if ["deploy-runbook", "orchestrator", "coding-agent"].contains(&case)
            && !cfg!(feature = "cel")
        {
            missing.push("cel");
        }
        if doc.contains("cron:") && !cfg!(feature = "cron") {
            missing.push("cron");
        }
        if !missing.is_empty() {
            assert!(
                !valid
                    && missing
                        .iter()
                        .any(|f| err.contains(&format!("the '{f}' build feature"))),
                "{case} without {missing:?} is refused naming one of them:\n{err}"
            );
            let _ = std::fs::remove_file(&secret);
            continue;
        }
        assert!(valid, "{case} must load through agentd:\n{err}");
        let s = settings(&doc, env);
        let steps = human_steps(&s);
        assert!(!steps.is_empty(), "{case}: no human step loaded");
        for (wf, step, to) in &steps {
            assert_eq!(
                to,
                &json!("operator"),
                "{case}: {wf}.{step} is answered by the operators"
            );
        }
        let mut got: Vec<(String, String, String)> = s
            .agent
            .document_gate_channels
            .iter()
            .flat_map(|(wf, steps)| {
                steps
                    .iter()
                    .map(move |(st, ch)| (wf.clone(), st.clone(), ch.clone()))
            })
            .collect();
        got.sort();
        let mut want: Vec<(String, String, String)> = want
            .into_iter()
            .map(|(w, st, ch)| (w.to_string(), st.to_string(), ch.to_string()))
            .collect();
        want.sort();
        assert_eq!(got, want, "{case}: each gate carries its declared channel");
        let _ = std::fs::remove_file(&secret);
    }
}

/// A `::!human` that names a `principal` addresses the gate to it, and the
/// gate rule applies unchanged: the operator loads (its channel still
/// carried), anyone else is refused with the usual reason. A reference to a
/// human the document never declared stays a load error.
#[test]
fn a_documents_human_with_a_principal_is_held_to_the_gate_rule() {
    let doc = |human: &str, to: &str| {
        format!(
            "---\nspec: \"1\"\n---\n# Refunds\n\n{human}\n\n\
             :::!workflow{{name=w}}\n\
             steps:\n\
             \x20 s:   {{kind: manual}}\n\
             \x20 ask: {{kind: human, question: \"ok?\", to: \"{to}\", depends_on: [s]}}\n\
             \x20 f:   {{kind: finish, depends_on: [ask]}}\n\
             :::\n"
        )
    };
    let operator = doc(
        ":::!human{name=oncall principal=operator channel=#ops}\n:::",
        "@human/oncall",
    );
    let (valid, err, _) = load(&operator, &ALL);
    assert!(
        valid,
        "a human who is the operator is a gate's decider:\n{err}"
    );
    let s = settings(&operator, Value::Null);
    assert_eq!(human_steps(&s)[0].2, json!("operator"));
    assert_eq!(
        s.agent.document_gate_channels["w"]["ask"], "#ops",
        "and the channel it declares is carried"
    );

    let (valid, err, _) = load(
        &doc(
            ":::!human{name=oncall principal=user:alice channel=#ops}\n:::",
            "@human/oncall",
        ),
        &ALL,
    );
    assert!(!valid, "user:alice could never see the gate's task");
    assert!(
        err.contains("`to` names user:alice, who could never see the task")
            && !err.contains("channel"),
        "the refusal is the gate rule's own, about her:\n{err}"
    );

    let (valid, err, _) = load(
        &doc(":::!human{name=oncall channel=#ops}\n:::", "@human/nobody"),
        &ALL,
    );
    assert!(!valid, "a reference to an undeclared human must not load");
    assert!(
        err.contains("`to` names @human/nobody, who could never see the task")
            && err.contains("resolves only inside the document that declares"),
        "the refusal says the reference names no declared human:\n{err}"
    );
}

/// A gate nested in a body is the same document's reference to the same
/// human, and loads like a top-level one: answered by the operators, its
/// channel recorded under the step's path. Before, the loader resolved
/// top-level steps only, and the nested reference was refused with a reason
/// that said it resolves only inside the document that declares the human —
/// the document it was in. (A body that runs copies at once — `foreach`,
/// `batch`, `parallel`, `race` — holds no gate at all; the engine refuses
/// that for its own reason.)
#[test]
fn a_documents_nested_gate_loads_like_a_top_level_one() {
    let doc = "---\nspec: \"1\"\n---\n# Refunds\n\n\
               :::!channel{name=ops}\n:::\n\n\
               :::!human{name=oncall channel=@channel/ops}\n:::\n\n\
               :::!workflow{name=w}\n\
               steps:\n\
               \x20 s: {kind: manual}\n\
               \x20 loop:\n\
               \x20   kind: iterate\n\
               \x20   max_iterations: 2\n\
               \x20   depends_on: [s]\n\
               \x20   body: {steps: {ask: {kind: human, question: \"ok?\", to: \"@human/oncall\"}}}\n\
               \x20 sub:\n\
               \x20   kind: subgraph\n\
               \x20   depends_on: [loop]\n\
               \x20   body: {steps: {ask: {kind: human, question: \"ok?\", to: \"@human/oncall\"}}}\n\
               \x20 f: {kind: finish, depends_on: [sub]}\n\
               :::\n";
    let (valid, err, _) = load(doc, &ALL);
    assert!(valid, "a nested document gate must load:\n{err}");
    let s = settings(doc, Value::Null);
    let w = s
        .workflows
        .iter()
        .find(|w| w["name"] == "w")
        .expect("the document's workflow");
    assert_eq!(w["steps"]["loop"]["body"]["steps"]["ask"]["to"], "operator");
    assert_eq!(w["steps"]["sub"]["body"]["steps"]["ask"]["to"], "operator");
    let got: Vec<(&str, &str)> = s.agent.document_gate_channels["w"]
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    assert_eq!(
        got,
        [("loop.ask", "@channel/ops"), ("sub.ask", "@channel/ops")]
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
    assert!(
        err.contains(":::!workflow") && err.contains("is a machinery kind"),
        "names the fix:\n{err}"
    );
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
