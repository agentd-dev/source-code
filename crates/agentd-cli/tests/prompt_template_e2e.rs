// SPDX-License-Identifier: AGPL-3.0-only
//! The system-prompt template, end to end — loops, conditions and limits over
//! the environment data, a per-node named template, the built-in default's
//! KV-cache ordering, and the fail-closed guards at config load.
//!
//! The daemon runs against the in-process mock intelligence, which echoes the
//! system prompt it was given, so these assert on what a model ACTUALLY
//! receives rather than on an internal function's return value.
#![cfg(all(unix, feature = "workflow"))]

mod common;

use std::process::{Command, Stdio};

fn run_cfg(cfg_text: &str) -> (Option<i32>, String) {
    let dir = common::unique_path("tpl-prompt", "d");
    std::fs::create_dir_all(&dir).unwrap();
    let cfg = format!("{dir}/c.yaml");
    std::fs::write(&cfg, cfg_text).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_agentd"))
        .args(["--config", &cfg])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .env("AGENTD_STATE_DIR", format!("{dir}/state"))
        .output()
        .expect("run");
    let log = String::from_utf8_lossy(&out.stderr).to_string();
    let _ = std::fs::remove_dir_all(&dir);
    (out.status.code(), log)
}

/// `--validate-config` only: exit code + the diagnostics on stderr.
fn validate(cfg_text: &str) -> (Option<i32>, String) {
    let dir = common::unique_path("tpl-val", "d");
    std::fs::create_dir_all(&dir).unwrap();
    let cfg = format!("{dir}/c.yaml");
    std::fs::write(&cfg, cfg_text).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_agentd"))
        .args(["--config", &cfg, "--validate-config"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .output()
        .expect("run");
    let log = String::from_utf8_lossy(&out.stderr).to_string();
    let _ = std::fs::remove_dir_all(&dir);
    (out.status.code(), log)
}

const BASE: &str = "config_version: \"1\"\nagent: { name: tpl }\nstore: { kind: memory }\n\
     intelligence: { endpoints: \"mock:echo-system\", model: mock }\n\
     lifecycle: { run_until: idle, idle_grace: 900ms }\n\
     observability: { log_level: info, log_content: true }\n";

#[test]
fn the_builtin_template_is_printable_and_parses() {
    // An override starts as a copy of the default, so the default must be
    // printable — and feeding it straight back must validate.
    let out = Command::new(env!("CARGO_BIN_EXE_agentd"))
        .arg("--context-template")
        .output()
        .expect("run");
    assert_eq!(out.status.code(), Some(0));
    let tpl = String::from_utf8_lossy(&out.stdout).to_string();
    assert!(
        tpl.contains("{{#each"),
        "the default uses the language it offers:\n{tpl}"
    );
    assert!(tpl.contains("{{instruction}}"), "{tpl}");

    let indented: String = tpl.lines().map(|l| format!("    {l}\n")).collect();
    let (code, log) = validate(&format!("{BASE}context:\n  template: |\n{indented}"));
    assert_eq!(code, Some(0), "the default template round-trips:\n{log}");
}

#[test]
fn a_template_does_loops_and_conditions_on_any_build() {
    // Bare paths only — the case that must work WITHOUT `--features cel`,
    // which is also why the built-in default is written this way.
    let (code, log) = run_cfg(&format!(
        "{BASE}services:\n\
        \x20 a: {{ endpoint: \"https://a.example/mcp\", tags: {{ \"*\": [sensitive] }} }}\n\
        \x20 b: {{ endpoint: \"https://b.example/mcp\" }}\n\
         context:\n\
        \x20 template: |\n\
        \x20   AGENT {{{{instance}}}}\n\
        \x20   {{{{instruction}}}}\n\
        \x20   {{{{#if services}}}}SERVICES:{{{{/if}}}}\n\
        \x20   {{{{#each services}}}}<{{{{@index}}}}:{{{{this.name}}}}:{{{{this.tags_text}}}}>{{{{/each}}}}\n\
        \x20   {{{{#if egress_closed}}}}CLOSED{{{{else}}}}OPEN{{{{/if}}}}\n\
         workflows:\n  - name: w\n    steps:\n\
        \x20     s: {{ kind: once }}\n\
        \x20     a: {{ kind: agent, depends_on: [s], instruction: \"hi\" }}\n\
        \x20     f: {{ kind: finish, depends_on: [a], status: completed, output: \"{{{{steps.a.output}}}}\" }}\n"
    ));
    assert_eq!(code, Some(0), "{log}");
    assert!(
        log.contains("AGENT tpl"),
        "interpolation reached the model:\n{log}"
    );
    assert!(
        log.contains("SERVICES:"),
        "#if fired on a non-empty list:\n{log}"
    );
    assert!(
        log.contains("<0:a:sensitive><1:b:>"),
        "#each gave `this`, `@index` and the pre-joined tag text:\n{log}"
    );
    assert!(
        log.contains("OPEN"),
        "#if/else picked the right branch:\n{log}"
    );
}

/// The same template with a CEL expression — limits via `take()`. On a build
/// without the feature this config is refused at LOAD (asserted separately),
/// so the test itself is feature-gated.
#[cfg(feature = "cel")]
#[test]
fn a_template_limits_a_list_with_cel() {
    let (code, log) = run_cfg(&format!(
        "{BASE}services:\n\
        \x20 a: {{ endpoint: \"https://a.example/mcp\" }}\n\
        \x20 b: {{ endpoint: \"https://b.example/mcp\" }}\n\
        \x20 c: {{ endpoint: \"https://c.example/mcp\" }}\n\
         context:\n\
        \x20 template: |\n\
        \x20   {{{{instruction}}}}\n\
        \x20   {{{{#each take(services, 2)}}}}<{{{{this.name}}}}>{{{{/each}}}}\n\
         workflows:\n  - name: w\n    steps:\n\
        \x20     s: {{ kind: once }}\n\
        \x20     a: {{ kind: agent, depends_on: [s], instruction: \"hi\" }}\n\
        \x20     f: {{ kind: finish, depends_on: [a], status: completed, output: \"{{{{steps.a.output}}}}\" }}\n"
    ));
    assert_eq!(code, Some(0), "{log}");
    assert!(
        log.contains("<a><b>") && !log.contains("<c>"),
        "take() capped the list at 2:\n{log}"
    );
}

/// Without the feature, a CEL-using template is refused at config load with
/// the feature named — never mis-rendered at turn time.
#[cfg(not(feature = "cel"))]
#[test]
fn a_cel_template_is_refused_at_load_without_the_feature() {
    let (code, log) = validate(&format!(
        "{BASE}context:\n  template: \"{{{{#each take(services, 2)}}}}x{{{{/each}}}}{{{{instruction}}}}\"\n"
    ));
    assert_eq!(code, Some(2), "{log}");
    assert!(log.contains("'cel' build feature"), "{log}");
}

#[test]
fn a_node_selects_a_named_template() {
    let (code, log) = run_cfg(&format!(
        "{BASE}context:\n\
        \x20 templates:\n\
        \x20   minimal: |\n\
        \x20     MINIMAL {{{{instance}}}}: {{{{instruction}}}}\n\
         workflows:\n  - name: w\n    steps:\n\
        \x20     s: {{ kind: once }}\n\
        \x20     a: {{ kind: agent, depends_on: [s], instruction: \"hi\", context: {{ template: minimal }} }}\n\
        \x20     f: {{ kind: finish, depends_on: [a], status: completed, output: \"{{{{steps.a.output}}}}\" }}\n"
    ));
    assert_eq!(code, Some(0), "{log}");
    assert!(
        log.contains("MINIMAL tpl"),
        "the step's named template rendered instead of the default:\n{log}"
    );
    assert!(
        !log.contains("Be concise and factual"),
        "the default's persona did NOT leak into the named template:\n{log}"
    );
}

#[test]
fn malformed_templates_and_unknown_references_are_refused_at_load() {
    for (tpl, want) in [
        ("{{#each services}}oops", "unclosed block"),
        ("{{#while x}}{{/while}}", "unknown block tag"),
        ("{{ghost}}", "unknown reference"),
    ] {
        let (code, log) = validate(&format!(
            "{BASE}context:\n  template: \"{}\"\n",
            tpl.replace('"', "\\\"")
        ));
        assert_eq!(code, Some(2), "{tpl:?} must refuse startup:\n{log}");
        assert!(log.contains(want), "{tpl:?} → wanted {want:?}:\n{log}");
    }
}

#[test]
fn a_template_that_drops_the_instruction_warns_loudly() {
    // Losing standing policy is the failure that still looks like a working
    // agent, so it is a warning on every boot — not silence.
    let (code, log) = validate(&format!(
        "{BASE}context:\n  template: \"just {{{{instance}}}}\"\n"
    ));
    assert_eq!(code, Some(0), "it is legal, just unwise:\n{log}");
    assert!(
        log.contains("never references") && log.contains("standing policy"),
        "the warning names the risk:\n{log}"
    );
}

#[test]
fn the_default_orders_stable_sections_before_volatile_ones() {
    // KV caching: a section that changes between turns invalidates everything
    // after it, so live state must come last in the shipped default.
    let out = Command::new(env!("CARGO_BIN_EXE_agentd"))
        .arg("--context-template")
        .output()
        .expect("run");
    let t = String::from_utf8_lossy(&out.stdout).to_string();
    let at = |needle: &str| {
        t.find(needle)
            .unwrap_or_else(|| panic!("missing {needle}: {t}"))
    };
    let instruction = at("## Instruction");
    let services = at("## Services");
    let signals = at("## Signals");
    let memory = at("## Memory");
    assert!(
        instruction < services && services < signals && signals < memory,
        "stable → volatile ordering: instruction {instruction}, services {services}, signals {signals}, memory {memory}"
    );
}
