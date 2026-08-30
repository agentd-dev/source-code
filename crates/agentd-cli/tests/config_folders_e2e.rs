// SPDX-License-Identifier: AGPL-3.0-only
//! Config discovery as a CHAIN, and the conventional folders beside it.
//!
//! Two mechanisms, both about the same thing: what agentd does when nobody
//! told it anything. Discovery walks `~/.config/agentd/config.yml` →
//! `./agentd.yml` → `./agentd.local.yml`, and the folders `workflows/`,
//! `subagents/` and `context/` fill in settings the operator did not write.
//!
//! Every case here runs the REAL binary from a temp directory, because the
//! whole feature is about filesystem layout and process environment — a unit
//! test over the loader would assert the parts and miss the arrangement.
#![cfg(unix)]

mod common;

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// Run agentd in `cwd` with a controlled HOME and no inherited config env.
fn run_in(cwd: &Path, home: &Path, args: &[&str]) -> (Option<i32>, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_agentd"))
        .args(args)
        .current_dir(cwd)
        .env_remove("AGENT_CONFIG")
        .env_remove("AGENTD_CONFIG")
        .env_remove("XDG_CONFIG_HOME")
        .env("HOME", home)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .output()
        .expect("run");
    (
        out.status.code(),
        String::from_utf8_lossy(&out.stderr).to_string(),
    )
}

/// A scratch project: `<root>/home` for the user rung, `<root>/work` for the
/// project one.
fn project(tag: &str) -> (PathBuf, PathBuf, PathBuf) {
    let root = PathBuf::from(common::unique_path(tag, "d"));
    let (home, work) = (root.join("home"), root.join("work"));
    std::fs::create_dir_all(home.join(".config/agentd")).unwrap();
    std::fs::create_dir_all(&work).unwrap();
    (root, home, work)
}

const BASE: &str = "config_version: \"1\"\n\
     agent: { name: conv, instruction: conventions }\n\
     intelligence: { endpoints: \"mock:final\", model: mock }\n\
     store: { kind: memory }\n\
     observability: { log_level: info }\n\
     lifecycle: { run_until: idle, idle_grace: 1s }\n";

fn wf(name: &str) -> String {
    format!(
        "name: {name}\nsteps:\n  s: {{ kind: manual }}\n  \
         f: {{ kind: finish, depends_on: [s], status: completed }}\n"
    )
}

/// The chain layers user → project → local, and the most specific wins. The
/// three rungs exist so a person's defaults, a checkout's settings and one
/// machine's overrides can each live where they belong.
#[test]
fn the_chain_layers_and_the_most_specific_rung_wins() {
    let (root, home, work) = project("chain");
    std::fs::write(home.join(".config/agentd/config.yml"), BASE).unwrap();
    let (code, log) = run_in(&work, &home, &["--validate-config"]);
    assert_eq!(code, Some(0), "{log}");
    assert!(
        log.contains("config.yml"),
        "the user rung should load\n{log}"
    );

    std::fs::write(work.join("agentd.yml"), "agent: { name: from_project }\n").unwrap();
    std::fs::write(
        work.join("agentd.local.yml"),
        "agent: { name: from_local }\n",
    )
    .unwrap();
    let (code, log) = run_in(&work, &home, &[]);
    assert_eq!(code, Some(0), "{log}");
    assert!(
        log.contains("\"instance\":\"from_local\""),
        "the local rung should win\n{log}"
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// Two spellings of ONE rung is a coin toss; two DIFFERENT rungs is the design.
#[test]
fn ambiguity_is_per_rung_not_across_the_chain() {
    let (root, home, work) = project("ambig");
    std::fs::write(work.join("agentd.yml"), BASE).unwrap();
    std::fs::write(work.join("agentd.local.yml"), "agent: { name: ok }\n").unwrap();
    let (code, log) = run_in(&work, &home, &["--validate-config"]);
    assert_eq!(code, Some(0), "project + local must compose\n{log}");

    std::fs::write(work.join("agentd.yaml"), BASE).unwrap();
    let (code, log) = run_in(&work, &home, &["--validate-config"]);
    assert_eq!(code, Some(2), "{log}");
    assert!(
        log.contains("project config is ambiguous"),
        "the refusal should name the rung\n{log}"
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// Naming a config means the caller decided: no rung is merged underneath it.
/// Silently folding a stray `agentd.local.yml` into a named production config
/// is the surprise discovery must never spring.
#[test]
fn an_explicit_config_suppresses_the_whole_chain() {
    let (root, home, work) = project("explicit");
    std::fs::write(work.join("agentd.yml"), BASE).unwrap();
    std::fs::write(
        work.join("agentd.local.yml"),
        "agent: { name: must_not_win }\n",
    )
    .unwrap();
    let (code, log) = run_in(&work, &home, &["-c", "agentd.yml", "--validate-config"]);
    assert_eq!(code, Some(0), "{log}");
    assert!(
        !log.contains("agentd.local.yml"),
        "a named config must not adopt the local rung\n{log}"
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// `workflows/` beside the config, loaded in FILENAME order — which is the
/// only ordering an operator can see without reading the loader.
#[test]
fn a_workflows_folder_is_adopted_in_filename_order() {
    let (root, home, work) = project("wfdir");
    std::fs::write(work.join("agentd.yml"), BASE).unwrap();
    std::fs::create_dir_all(work.join("workflows")).unwrap();
    // Written out of order; the numeric prefixes decide.
    std::fs::write(work.join("workflows/30-charlie.yaml"), wf("charlie")).unwrap();
    std::fs::write(work.join("workflows/10-alpha.yaml"), wf("alpha")).unwrap();
    std::fs::write(work.join("workflows/20-bravo.yaml"), wf("bravo")).unwrap();

    let (code, log) = run_in(&work, &home, &[]);
    assert_eq!(code, Some(0), "{log}");
    let order: Vec<&str> = log
        .lines()
        .filter(|l| l.contains("\"workflow.loaded\""))
        .filter_map(|l| {
            ["alpha", "bravo", "charlie"]
                .into_iter()
                .find(|n| l.contains(&format!("\"name\":\"{n}\"")))
        })
        .collect();
    assert_eq!(order, ["alpha", "bravo", "charlie"], "{log}");
    let _ = std::fs::remove_dir_all(&root);
}

/// The folder is a CONVENTION: it fills in a setting nobody wrote, and never
/// argues with one that was. An explicit `workflows:` — including an empty
/// list meaning "none" — is the operator's decision.
#[test]
fn an_explicit_workflows_setting_suppresses_the_folder() {
    let (root, home, work) = project("wfexplicit");
    std::fs::write(work.join("agentd.yml"), format!("{BASE}workflows: []\n")).unwrap();
    std::fs::create_dir_all(work.join("workflows")).unwrap();
    std::fs::write(work.join("workflows/a.yaml"), wf("should_not_load")).unwrap();

    let (code, log) = run_in(&work, &home, &[]);
    assert_eq!(code, Some(0), "{log}");
    assert!(
        !log.contains("should_not_load"),
        "an explicit empty list means none\n{log}"
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// A missing or empty folder is silence, not exit 2. A NAMED `dir:` with no
/// match is an error because you asked for it; a convention that did the same
/// would make agentd unrunnable in any directory without a `subagents/`.
#[test]
fn absent_folders_are_silent() {
    let (root, home, work) = project("empty");
    std::fs::write(work.join("agentd.yml"), BASE).unwrap();
    std::fs::create_dir_all(work.join("workflows")).unwrap(); // present but EMPTY
    let (code, log) = run_in(&work, &home, &["--validate-config"]);
    assert_eq!(
        code,
        Some(0),
        "an empty conventional folder must not fail\n{log}"
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// `subagents/` and `context/`: one reviewed template per file, named by stem,
/// typed and validated exactly as an inline entry would be.
#[test]
fn subagent_and_context_templates_load_from_their_folders() {
    let (root, home, work) = project("tpls");
    std::fs::write(work.join("agentd.yml"), BASE).unwrap();
    std::fs::create_dir_all(work.join("subagents")).unwrap();
    std::fs::create_dir_all(work.join("context")).unwrap();
    std::fs::write(
        work.join("subagents/reviewer.yaml"),
        "instruction: |\n  Review what you are given.\nparams:\n  target: { type: string }\n",
    )
    .unwrap();
    std::fs::write(work.join("context/minimal.md"), "You are {{instance}}.\n").unwrap();

    // The templates ARE loaded: this rule fires only when some exist.
    let (code, log) = run_in(&work, &home, &["--validate-config"]);
    assert_eq!(code, Some(0), "{log}");
    assert!(
        log.contains("subagents.templates are declared but a2a.listen is unset"),
        "the subagents folder should have populated templates\n{log}"
    );

    // And a context template is validated like any other: a bad reference is
    // caught at load, naming the template the folder supplied.
    std::fs::write(work.join("context/minimal.md"), "You are {{nope}}.\n").unwrap();
    let (code, log) = run_in(&work, &home, &["--validate-config"]);
    assert_eq!(code, Some(2), "{log}");
    assert!(
        log.contains("context.templates.minimal"),
        "the refusal should name the folder-loaded template\n{log}"
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// `skills/` beside the config: prose the model reads, with no MCP server
/// between it and the agent. Both layouts, and a bare markdown file too.
#[test]
fn a_skills_folder_is_adopted_without_a_server() {
    let (root, home, work) = project("skdir");
    std::fs::write(work.join("agentd.yml"), BASE).unwrap();
    std::fs::create_dir_all(work.join("skills/runbook")).unwrap();
    std::fs::write(
        work.join("skills/triage.md"),
        "---\nname: triage\ndescription: Triage an issue. Use when: it has no labels\n---\n\nRead it, label it.\n",
    )
    .unwrap();
    std::fs::write(
        work.join("skills/runbook/SKILL.md"),
        "---\nname: incident\ndescription: Handle an incident\n---\n\nAcknowledge, then mitigate.\n",
    )
    .unwrap();
    std::fs::write(
        work.join("skills/deploy.md"),
        "# Deploy safely\n\nAlways deploy behind a flag.\n",
    )
    .unwrap();

    let (code, log) = run_in(&work, &home, &[]);
    assert_eq!(code, Some(0), "{log}");
    assert!(
        log.contains("\"server\":\"file\"") && log.contains("\"count\":3"),
        "all three layouts should register\n{log}"
    );
    for name in ["triage", "incident", "deploy"] {
        assert!(log.contains(name), "{name} missing\n{log}");
    }
    let _ = std::fs::remove_dir_all(&root);
}

/// An explicit `skills.dir` is the operator's decision; the convention only
/// fills in what nobody wrote.
#[test]
fn an_explicit_skills_dir_suppresses_the_convention() {
    let (root, home, work) = project("skexplicit");
    std::fs::create_dir_all(work.join("skills")).unwrap();
    std::fs::create_dir_all(work.join("elsewhere")).unwrap();
    std::fs::write(
        work.join("skills/ignored.md"),
        "# Ignored\n\nnot this one.\n",
    )
    .unwrap();
    std::fs::write(work.join("elsewhere/chosen.md"), "# Chosen\n\nthis one.\n").unwrap();
    std::fs::write(
        work.join("agentd.yml"),
        format!("{BASE}skills: {{ dir: ./elsewhere }}\n"),
    )
    .unwrap();

    let (code, log) = run_in(&work, &home, &[]);
    assert_eq!(code, Some(0), "{log}");
    assert!(log.contains("chosen"), "{log}");
    assert!(
        !log.contains("ignored"),
        "the convention must not override\n{log}"
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// `{{config.*}}` in a workflow REFERENCE — `dir:` or `file:`.
///
/// Workflow documents are deliberately excluded from the settings-wide
/// substitution so inline, file, url and dir entries are all folded alike at
/// LOAD time. But `dir:` was consumed by the directory expansion before that
/// fold ran, and validation checked `file:` existence before it too, so both
/// went to the filesystem as the literal token `{{config.wf_dir}}`.
///
/// It matters because config layers REPLACE lists: var-indirection is the only
/// way an overlay can redirect a workflow folder without restating every entry
/// the base config declared.
#[test]
fn a_config_var_resolves_in_a_workflow_dir_and_file_reference() {
    let (root, home, work) = project("wfvar");
    std::fs::create_dir_all(work.join("default_wf")).unwrap();
    std::fs::create_dir_all(work.join("site_wf")).unwrap();
    std::fs::write(work.join("default_wf/a.yaml"), wf("default_extra")).unwrap();
    std::fs::write(work.join("site_wf/a.yaml"), wf("site_extra")).unwrap();
    std::fs::write(work.join("standalone.yaml"), wf("via_file_var")).unwrap();
    // A raw string, because the shape of this YAML is the point and escaping
    // it through format! is how the indentation went wrong the first time.
    let cfg = r#"vars:
  wf_dir: ./default_wf
  wf_file: ./standalone.yaml
workflows:
  - name: from_base
    steps:
      s: { kind: manual }
      f: { kind: finish, depends_on: [s], status: completed }
  - dir: "{{config.wf_dir}}"
    glob: "*.yaml"
  - name: via_file
    file: "{{config.wf_file}}"
"#;
    std::fs::write(work.join("agentd.yml"), format!("{BASE}{cfg}")).unwrap();

    let (code, log) = run_in(&work, &home, &[]);
    assert_eq!(code, Some(0), "{log}");
    for name in ["from_base", "default_extra", "via_file_var"] {
        assert!(
            log.contains(&format!("\"name\":\"{name}\"")),
            "{name} should have loaded\n{log}"
        );
    }

    // The point of the fix: an overlay redirects the FOLDER by setting one var,
    // without restating the list the base config declared. `vars` is a map, so
    // it merges key by key where the `workflows` list would be replaced whole.
    std::fs::write(
        work.join("agentd.local.yml"),
        "vars: { wf_dir: ./site_wf }\n",
    )
    .unwrap();
    let (code, log) = run_in(&work, &home, &[]);
    assert_eq!(code, Some(0), "{log}");
    assert!(
        log.contains("site_extra"),
        "the overlay should redirect\n{log}"
    );
    assert!(
        log.contains("from_base") && log.contains("via_file_var"),
        "and must not drop what the base declared\n{log}"
    );
    assert!(
        !log.contains("default_extra"),
        "the old folder should no longer load\n{log}"
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// An UNDEFINED var in a reference is named once — not once by validation and
/// again by the loader. Two messages for one typo is worse than one.
#[test]
fn an_undefined_var_in_a_reference_is_reported_once() {
    let (root, home, work) = project("wfvarbad");
    std::fs::create_dir_all(work.join("wfs")).unwrap();
    std::fs::write(work.join("wfs/a.yaml"), wf("never_loads")).unwrap();
    std::fs::write(
        work.join("agentd.yml"),
        format!("{BASE}workflows:\n  - dir: \"{{{{config.nope}}}}\"\n"),
    )
    .unwrap();
    let (_code, log) = run_in(&work, &home, &[]);
    let hits = log.matches("config.nope is not defined").count();
    assert_eq!(hits, 1, "expected exactly one message\n{log}");
    let _ = std::fs::remove_dir_all(&root);
}

/// A thin overlay that lives somewhere else must not hide the project's
/// folders.
///
/// `agentd -c ./agentd.yml -c /tmp/over.yml` is an ordinary shape, and keying
/// the folder search on the LAST config file alone found nothing and fell
/// through to the sugar `main` loop — in silence, which is worse than any
/// ordering question. Candidates are searched most-specific-first, so the
/// project's folders are still found when the overlay has none.
#[test]
fn an_overlay_elsewhere_does_not_hide_the_projects_folders() {
    let (root, home, work) = project("overlaydir");
    std::fs::write(work.join("agentd.yml"), BASE).unwrap();
    std::fs::create_dir_all(work.join("workflows")).unwrap();
    std::fs::write(work.join("workflows/a.yaml"), wf("from_folder")).unwrap();

    // The overlay sits in a sibling directory with no folders of its own.
    let over = root.join("elsewhere");
    std::fs::create_dir_all(&over).unwrap();
    let over_cfg = over.join("over.yml");
    std::fs::write(&over_cfg, "agent: { name: overlaid }\n").unwrap();

    let (code, log) = run_in(
        &work,
        &home,
        &["-c", "agentd.yml", "-c", over_cfg.to_str().unwrap()],
    );
    assert_eq!(code, Some(0), "{log}");
    assert!(
        log.contains("from_folder"),
        "the project's workflows folder should still be found\n{log}"
    );
    assert!(
        log.contains("\"instance\":\"overlaid\""),
        "and the overlay still wins on the keys it sets\n{log}"
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// The working directory is NOT a candidate when a config was named.
///
/// `agentd -c some/where/agentd.yml` run from a directory that happens to have
/// a `skills/` folder adopted THAT folder — a stray directory modifying a run
/// the caller spelled out, which is the surprise the whole discovery design
/// refuses. Naming a config decides the folders beside it too.
#[test]
fn the_working_directory_is_not_searched_for_a_named_config() {
    let (root, home, work) = project("cwdleak");
    // The project lives in a subdirectory; the config is named by path.
    let proj = work.join("proj");
    std::fs::create_dir_all(&proj).unwrap();
    std::fs::write(proj.join("agentd.yml"), BASE).unwrap();

    // A `skills/` folder sits in the CWD, belonging to something else entirely.
    std::fs::create_dir_all(work.join("skills")).unwrap();
    std::fs::write(
        work.join("skills/not-mine.md"),
        "# Not mine\n\nbelongs to whatever lives in this directory.\n",
    )
    .unwrap();

    let (code, log) = run_in(&work, &home, &["-c", "proj/agentd.yml"]);
    assert_eq!(code, Some(0), "{log}");
    assert!(
        !log.contains("not-mine"),
        "a named config must not adopt the working directory's folders\n{log}"
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// With NO config file at all, the working directory is the only sensible
/// place to look — and it is still searched.
#[test]
fn the_working_directory_is_searched_when_no_config_exists() {
    let (root, home, work) = project("cwdonly");
    std::fs::create_dir_all(work.join("workflows")).unwrap();
    std::fs::write(work.join("workflows/a.yaml"), wf("bare_cwd")).unwrap();

    let (_code, log) = run_in(
        &work,
        &home,
        &[
            "--intelligence",
            "mock:final",
            "--model",
            "mock",
            "--validate-config",
        ],
    );
    assert!(
        log.contains("config.valid") || log.contains("bare_cwd"),
        "a config-less run should still find ./workflows\n{log}"
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// `--validate-config` must refuse exactly what STARTUP refuses.
///
/// The credential check ran at startup over the whole document, but validation
/// checked only `intelligence.headers` — the check rode along with a lint that
/// only header maps have. So the idiomatic spellings passed validation and then
/// exited 2 at startup: a validator green-lighting a config the daemon refuses,
/// which is the one failure mode it exists to prevent.
#[test]
fn validate_refuses_every_credential_reference_startup_refuses() {
    let (root, home, work) = project("credrefs");
    let cases: [(&str, &str); 4] = [
        (
            "intelligence.token",
            "intelligence: { endpoints: \"https://x/v1\", model: m, token: \"{{secret:ABSENT_ONE}}\" }\n",
        ),
        (
            "intelligence.headers",
            "intelligence: { endpoints: \"https://x/v1\", model: m, headers: { authorization: \"Bearer {{secret:ABSENT_ONE}}\" } }\n",
        ),
        (
            "mcp auth.token",
            "intelligence: { endpoints: \"https://x/v1\", model: m }\nmcp: { servers: [ { name: s, endpoint: \"https://x/mcp\", auth: { kind: static, token: \"{{secret:ABSENT_ONE}}\" } } ] }\n",
        ),
        (
            "principal bearer_ref",
            "intelligence: { endpoints: \"https://x/v1\", model: m }\na2a: { listen: \"http://127.0.0.1:8477\", principals: [ { match: { bearer_ref: \"{{secret:ABSENT_ONE}}\" }, role: user } ] }\n",
        ),
    ];
    for (what, body) in cases {
        let cfg = work.join("c.yml");
        std::fs::write(
            &cfg,
            format!("config_version: \"1\"\nagent: {{ name: c, instruction: x }}\nstore: {{ kind: memory }}\n{body}"),
        )
        .unwrap();
        let (code, log) = run_in(&work, &home, &["-c", "c.yml", "--validate-config"]);
        assert_eq!(
            code,
            Some(2),
            "{what} should be refused at validate time\n{log}"
        );
        assert!(
            log.contains("ABSENT_ONE") && log.contains("is not set in the environment"),
            "{what}: the refusal should name the reference\n{log}"
        );
    }
    let _ = std::fs::remove_dir_all(&root);
}

/// And a resolvable one still passes — the check is about resolution, not
/// about the presence of a reference.
#[test]
fn a_resolvable_credential_reference_validates() {
    let (root, home, work) = project("credok");
    std::fs::write(
        work.join("c.yml"),
        "config_version: \"1\"\nagent: { name: c, instruction: x }\nstore: { kind: memory }\nintelligence: { endpoints: \"https://x/v1\", model: m, token: \"{{secret:PRESENT_ONE}}\" }\n",
    )
    .unwrap();
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_agentd"))
        .args(["-c", "c.yml", "--validate-config"])
        .current_dir(&work)
        .env_remove("AGENT_CONFIG")
        .env_remove("AGENTD_CONFIG")
        .env("HOME", &home)
        .env("PRESENT_ONE", "a-value")
        .output()
        .expect("run");
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let _ = std::fs::remove_dir_all(&root);
}
