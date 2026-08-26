// SPDX-License-Identifier: AGPL-3.0-only
//! Workflow-as-tool: a workflow registered as a first-class contract.
//!
//! The shapes already matched — a workflow carries a description, an input
//! schema, an output schema and a definition hash, which is a tool contract.
//! What it adds is what only the engine has: a "call" that takes thirty
//! minutes, survives a restart, and has retry, breaker, idempotency and a
//! human gate inside it. And it is strictly better for the trifecta fold: a
//! subagent handed `billing.refund` spends its legs on one reviewed procedure
//! rather than a whole server's tool surface.
//!
//! The safety argument is startup-only registration plus DERIVED tags, and
//! both are tested here.
#![cfg(all(unix, feature = "workflow"))]

mod common;

use std::process::{Command, Stdio};

fn run(cfg_text: &str) -> (Option<i32>, String) {
    let dir = common::unique_path("wftool", "d");
    std::fs::create_dir_all(&dir).unwrap();
    let cfg = format!("{dir}/c.yaml");
    std::fs::write(&cfg, cfg_text.replace("__STATE__", &format!("{dir}/state"))).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_agentd"))
        .args(["--config", &cfg])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .output()
        .expect("run");
    let log = String::from_utf8_lossy(&out.stderr).to_string();
    let _ = std::fs::remove_dir_all(&dir);
    (out.status.code(), log)
}

const BASE: &str = "config_version: \"1\"\n\
     store: { kind: file, file: { path: __STATE__ } }\n\
     observability: { log_level: info, log_content: true }\n\
     intelligence: { endpoints: \"mock:final\", model: mock }\n\
     lifecycle: { run_until: idle, idle_grace: 2s }\n";

/// A workflow with a `tool:` block becomes a registered contract at startup.
#[test]
fn a_workflow_with_a_tool_block_is_registered() {
    let (code, log) = run(&format!(
        "{BASE}agent: {{ name: wt }}\n\
         workflows:\n\
        \x20 - name: issue-refund\n\
        \x20   description: Refund an order.\n\
        \x20   tool: {{ name: billing.refund, mode: sync }}\n\
        \x20   inputs: {{ schema: {{ type: object, required: [order_id], properties: {{ order_id: {{ type: string }} }} }} }}\n    steps:\n\
        \x20     s: {{ kind: manual }}\n\
        \x20     f: {{ kind: finish, depends_on: [s], status: completed, output: refunded }}\n"
    ));
    assert_eq!(code, Some(0), "{log}");
    assert!(
        log.contains("\"event\":\"registry.workflow_tools\"") && log.contains("billing.refund"),
        "the workflow should have registered its tool\n{log}"
    );
}

/// Registered is not the same as CALLABLE. A workflow tool mutates runtime
/// state — it starts a run — so only the state owner may dispatch it; a turn
/// worker has to round-trip. Left out of both the round-trip list and the MCP
/// route map it would be advertised to the model and then fail to dispatch in
/// the child, which is the worst of both.
#[test]
fn a_registered_workflow_tool_is_callable_from_a_turn() {
    let dir = common::unique_path("wfcall", "d");
    std::fs::create_dir_all(&dir).unwrap();
    let cfg = format!("{dir}/c.yaml");
    std::fs::write(
        &cfg,
        format!(
            "config_version: \"1\"\n\
             agent: {{ name: wt, prompt: \"refund order A1\" }}\n\
             store: {{ kind: file, file: {{ path: {dir}/state }} }}\n\
             observability: {{ log_level: info, log_content: true }}\n\
             intelligence: {{ endpoints: \"mock:wf-tool\", model: mock }}\n\
             lifecycle: {{ run_until: idle, idle_grace: 3s }}\n\
             workflows:\n\
            \x20 - name: issue-refund\n\
            \x20   description: Refund an order.\n\
            \x20   tool: {{ name: billing.refund, mode: async }}\n\
            \x20   inputs: {{ schema: {{ type: object, required: [order_id], properties: {{ order_id: {{ type: string }} }} }} }}\n    steps:\n\
            \x20     s: {{ kind: manual }}\n\
            \x20     f: {{ kind: finish, depends_on: [s], status: completed, output: refunded }}\n"
        ),
    )
    .unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_agentd"))
        .args(["--config", &cfg])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .output()
        .expect("run");
    let log = String::from_utf8_lossy(&out.stderr).to_string();
    let _ = std::fs::remove_dir_all(&dir);
    // The model called it by its TOOL name...
    assert!(
        log.contains("\"tool\":\"billing.refund\""),
        "the model should have been able to call the tool\n{log}"
    );
    // ...and it started the workflow, which is what makes it a tool at all.
    assert!(
        log.contains("\"workflow\":\"issue-refund\"") && log.contains("\"event\":\"run.start\""),
        "calling the tool should have started the workflow\n{log}"
    );
    assert!(
        !log.contains("is unavailable") && !log.contains("no such tool"),
        "the tool must not be advertised without being dispatchable\n{log}"
    );
}

/// Shadowing an internal contract would silently reroute a built-in to a
/// workflow — exactly the surprise a fail-closed runtime exists to prevent.
#[test]
fn a_tool_name_that_shadows_an_internal_contract_is_refused() {
    let (code, log) = run(&format!(
        "{BASE}agent: {{ name: wt }}\n\
         workflows:\n\
        \x20 - name: sneaky\n    tool: {{ name: memory.get }}\n    steps:\n\
        \x20     s: {{ kind: manual }}\n\
        \x20     f: {{ kind: finish, depends_on: [s], status: completed }}\n"
    ));
    assert_eq!(code, Some(2), "{log}");
    assert!(
        log.contains("shadows an internal contract"),
        "the refusal should name the collision\n{log}"
    );
}

/// Two workflows claiming one tool name is a config error, not a race decided
/// by map iteration order.
#[test]
fn two_workflows_cannot_claim_the_same_tool_name() {
    let (code, log) = run(&format!(
        "{BASE}agent: {{ name: wt }}\n\
         workflows:\n\
        \x20 - name: a\n    tool: {{ name: shared.thing }}\n    steps:\n\
        \x20     s: {{ kind: manual }}\n\
        \x20     f: {{ kind: finish, depends_on: [s], status: completed }}\n\
        \x20 - name: b\n    tool: {{ name: shared.thing }}\n    steps:\n\
        \x20     s: {{ kind: manual }}\n\
        \x20     f: {{ kind: finish, depends_on: [s], status: completed }}\n"
    ));
    assert_eq!(code, Some(2), "{log}");
    assert!(log.contains("collides with an existing"), "{log}");
}

/// A tool block must name its tool — an unnamed registration would be a
/// silently-ignored config block.
#[test]
fn a_tool_block_without_a_name_is_refused() {
    let (code, log) = run(&format!(
        "{BASE}agent: {{ name: wt }}\n\
         workflows:\n\
        \x20 - name: a\n    tool: {{ mode: sync }}\n    steps:\n\
        \x20     s: {{ kind: manual }}\n\
        \x20     f: {{ kind: finish, depends_on: [s], status: completed }}\n"
    ));
    assert_eq!(code, Some(2), "{log}");
    assert!(log.contains("tool.name is required"), "{log}");
}

/// An unknown mode is refused rather than silently treated as sync — the
/// difference decides whether the caller blocks.
#[test]
fn an_unknown_mode_is_refused() {
    let (code, log) = run(&format!(
        "{BASE}agent: {{ name: wt }}\n\
         workflows:\n\
        \x20 - name: a\n    tool: {{ name: x.y, mode: eventually }}\n    steps:\n\
        \x20     s: {{ kind: manual }}\n\
        \x20     f: {{ kind: finish, depends_on: [s], status: completed }}\n"
    ));
    assert_eq!(code, Some(2), "{log}");
    assert!(log.contains("must be sync|async"), "{log}");
}

/// A workflow tool takes the workflow's declared INPUT SCHEMA as its
/// arguments, so a model finally sees the shape of what it is starting —
/// `workflow.run` could only offer a free-form object whose contents the
/// prompt never stated.
#[test]
fn the_tools_arguments_are_the_workflows_declared_inputs() {
    let (code, log) = run(&format!(
        "{BASE}agent: {{ name: wt }}\n\
         workflows:\n\
        \x20 - name: issue-refund\n\
        \x20   description: Refund an order above the threshold.\n\
        \x20   tool: {{ name: billing.refund }}\n\
        \x20   inputs: {{ schema: {{ type: object, required: [order_id], properties: {{ order_id: {{ type: string }} }} }} }}\n    steps:\n\
        \x20     s: {{ kind: manual }}\n\
        \x20     f: {{ kind: finish, depends_on: [s], status: completed }}\n"
    ));
    assert_eq!(code, Some(0), "{log}");
    assert!(
        log.contains("order_id"),
        "the workflow's input schema should be the tool's arguments\n{log}"
    );
}

/// Tags are DERIVED from what the steps actually reach, never declared by the
/// workflow author. Letting a workflow assert `tags: [sensitive, egress]`
/// about itself would make the one static instance-wide security gate
/// something the agent-editable half of the config says about itself.
#[test]
fn tags_are_derived_from_what_the_steps_reach() {
    // An `http` step reaches outside by construction, so the tool carries
    // `egress` without anyone writing it down.
    let (code, log) = run(&format!(
        "{BASE}agent: {{ name: wt }}\n\
         workflows:\n\
        \x20 - name: notify\n    tool: {{ name: ops.notify }}\n    steps:\n\
        \x20     s: {{ kind: manual }}\n\
        \x20     call: {{ kind: http, url: \"https://example.invalid/hook\", method: POST, depends_on: [s] }}\n\
        \x20     f: {{ kind: finish, depends_on: [call], status: completed }}\n"
    ));
    assert_eq!(code, Some(0), "{log}");
    assert!(
        log.contains("\"tags\":[\"egress\"]"),
        "an http step should have contributed the egress tag\n{log}"
    );
    // And a workflow that reaches nothing carries no tags.
    let (_c, quiet) = run(&format!(
        "{BASE}agent: {{ name: wt }}\n\
         workflows:\n\
        \x20 - name: pure\n    tool: {{ name: ops.pure }}\n    steps:\n\
        \x20     s: {{ kind: manual }}\n\
        \x20     f: {{ kind: finish, depends_on: [s], status: completed }}\n"
    ));
    assert!(
        quiet.contains("\"tool\":\"ops.pure\"") && !quiet.contains("\"tags\":[\"egress\"]"),
        "a workflow that reaches nothing should carry no tags\n{quiet}"
    );
}

/// A `tool:` block may only be declared in the startup config. The registry is
/// built once and validated fail-closed, and `workflow.create` is
/// root-callable — a root turn minting or shadowing a tool name at runtime
/// would put no operator in the loop.
///
/// Tested by contrast, so the refusal is attributable to the tool block and
/// not to anything else about the definition: the SAME workflow is accepted
/// once the block is removed.
#[test]
fn workflow_create_refuses_a_tool_block() {
    fn attempt(tool_block: &str) -> String {
        let dir = common::unique_path("wfcreate", "d");
        std::fs::create_dir_all(&dir).unwrap();
        let play = format!("{dir}/play.json");
        std::fs::write(
            &play,
            format!(
                r#"{{"turns": [
                 {{"tool_calls": [{{"name": "workflow.create", "arguments": {{"definition":
                   {{"name": "minted", {tool_block}
                    "steps": {{"s": {{"kind": "manual"}},
                              "f": {{"kind": "finish", "depends_on": ["s"], "status": "completed"}}}}}}}}}}]}},
                 {{"content": "done"}}]}}"#
            ),
        )
        .unwrap();
        let cfg = format!("{dir}/c.yaml");
        std::fs::write(
            &cfg,
            format!(
                "config_version: \"1\"\n\
                 agent: {{ name: wt, prompt: \"define a workflow\" }}\n\
                 store: {{ kind: file, file: {{ path: {dir}/state }} }}\n\
                 observability: {{ log_level: info, log_content: true }}\n\
                 intelligence: {{ endpoints: \"mock:file:{play}\", model: mock }}\n\
                 lifecycle: {{ run_until: idle, idle_grace: 2s }}\n"
            ),
        )
        .unwrap();
        let out = Command::new(env!("CARGO_BIN_EXE_agentd"))
            .args(["--config", &cfg])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .output()
            .expect("run");
        let log = String::from_utf8_lossy(&out.stderr).to_string();
        let _ = std::fs::remove_dir_all(&dir);
        log
    }

    let with_tool = attempt(r#""tool": {"name": "sneaky.thing"},"#);
    assert!(
        with_tool.contains("\"tool\":\"workflow.create\",\"trace_id\"")
            || with_tool.contains("workflow.create"),
        "the call should have happened\n{with_tool}"
    );
    assert!(
        with_tool.contains("\"is_error\":true"),
        "a definition carrying a tool block must be refused\n{with_tool}"
    );
    assert!(
        !with_tool.contains("\"name\":\"minted\",\"...\"")
            && !with_tool.contains("\"event\":\"workflow.created\""),
        "and no workflow should have been minted\n{with_tool}"
    );

    // The same definition WITHOUT the block is accepted, which is what makes
    // the refusal above attributable to the block rather than to the shape.
    let without = attempt("");
    assert!(
        !without.contains("\"is_error\":true"),
        "the same definition without a tool block should be accepted\n{without}"
    );
}
