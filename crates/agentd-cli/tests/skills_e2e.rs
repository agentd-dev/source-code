// SPDX-License-Identifier: AGPL-3.0-only
//! Skills from the MCP Skills extension (RFC 0051, SEP-2640), end to end,
//! against the built-in mock's 2026-07-28 edge, which declares
//! `io.modelcontextprotocol/skills`, lists eight skills three to a page,
//! answers `skills/get` for those and two more, and serves each skill's
//! files — one of them with other bytes than its entry lists, one past 8
//! MiB. Every request leaves a line on the mock's side, so what reached the
//! server is counted there.
//!
//! - A listed skill loads once its `SKILL.md` matches its digest and its
//!   frontmatter its entry; its other files wait to be asked for. Every page
//!   of the listing is read.
//! - A tampered `SKILL.md`, a frontmatter that is not the entry's, an entry
//!   with no `resources` and a `dynamic` skill are declined, each said by its
//!   server and `uri`; `dynamic: allow` loads the last, unverified.
//! - A skill in no listing is got by its `uri`.
//! - Two servers serving one name are told apart by the server, and read
//!   only from the one named.
//! - A server that does not declare the extension is never asked, and a
//!   prompt or a `skill://` resource is not a skill.
//! - A model reads a skill's file through the skill's own server, and never
//!   one whose bytes do not match, and a file past the 8 MiB other answers
//!   are held to whole; a step's skills sit in its prompt inside their origin
//!   tag, and one that loaded nothing is said there.
//! - A context names a skill, and reads it, by the reference it loaded it
//!   under, whatever the catalogue calls it since.
//! - The repository's own Agent Skill passes the rules a folder skill is
//!   held to.
#![cfg(unix)]

mod common;

use serde_json::{Value, json};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

fn events(log: &str, name: &str) -> Vec<Value> {
    log.lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter(|v| v["event"] == name)
        .collect()
}

/// The mock's `<marker> …` lines, the marker stripped.
fn marked(mock: &common::MockMcp, marker: &str) -> Vec<String> {
    let with = format!("{marker} ");
    mock.log()
        .lines()
        .filter_map(|l| l.strip_prefix(&with).map(str::to_string))
        .collect()
}

fn mock() -> common::MockMcp {
    common::spawn_mock_mcp_at("2026-07-28", "mock://res", false)
}

/// A config whose instruction is `instruction`, with the MCP servers
/// `servers` (`name: endpoint` pairs) and `extra` at the top, running one
/// `once` run of `steps` (a finish when empty) to idle.
fn config(instruction: &str, servers: &[(&str, &str)], steps: &str, extra: &str) -> String {
    let servers: String = servers
        .iter()
        .map(|(n, e)| format!("    - name: {n}\n      endpoint: {e}/mcp\n"))
        .collect();
    let steps = if steps.is_empty() {
        "      f: {kind: finish, depends_on: [s], status: completed}\n".to_string()
    } else {
        steps.to_string()
    };
    format!(
        "agent:\n  name: skills\n  instruction: \"{instruction}\"\nstore:\n  kind: memory\n\
         mcp:\n  servers:\n{servers}{extra}\
         workflows:\n  - name: job\n    steps:\n      s: {{kind: once}}\n{steps}\
         lifecycle:\n  run_until: idle\n  idle_grace: 300ms\n\
         observability:\n  log_level: info\n  log_content: true\n"
    )
}

/// `skills.sources` naming `servers`, each with `with` (`, dynamic: allow`).
fn sources(servers: &[&str], with: &str) -> String {
    let list: String = servers
        .iter()
        .map(|s| format!("    - {{server: {s}{with}}}\n"))
        .collect();
    format!("skills:\n  sources:\n{list}")
}

/// Run a config to its exit, within a minute; (exit code, log).
fn life(tag: &str, cfg: &str) -> (Option<i32>, String) {
    let path = common::unique_path(tag, "yaml");
    std::fs::write(&path, cfg).unwrap();
    let err_path = common::unique_path(tag, "err");
    let errf = std::fs::File::create(&err_path).unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_agentd"))
        .args(["--config", &path])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(errf))
        .spawn()
        .expect("spawn agentd");
    let deadline = Instant::now() + Duration::from_secs(60);
    let code = loop {
        if let Some(s) = child.try_wait().unwrap() {
            break s.code();
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            break None;
        }
        std::thread::sleep(Duration::from_millis(25));
    };
    let log = std::fs::read_to_string(&err_path).unwrap_or_default();
    let _ = std::fs::remove_file(&err_path);
    let _ = std::fs::remove_file(&path);
    (code, log)
}

/// The `skills.loaded` line of the reference `skill`.
fn loaded<'a>(all: &'a [Value], skill: &str) -> Option<&'a Value> {
    all.iter().find(|e| e["skill"] == skill)
}

/// The `skills.declined` line of the skill at `uri`.
fn declined_at(log: &str, uri: &str) -> Value {
    events(log, "skills.declined")
        .into_iter()
        .find(|e| e["uri"] == uri)
        .unwrap_or_else(|| panic!("no skills.declined for {uri}:\n{log}"))
}

#[test]
fn a_listed_skill_loads_once_its_digest_and_frontmatter_check_out() {
    let mock = mock();
    let cfg = config(
        "Follow @skill:git-workflow, @skill:refunds and @skill:pdf-processing.",
        &[("mock", &mock.uri())],
        "",
        &sources(&["mock"], ""),
    );
    let (code, log) = life("skills-listed", &cfg);
    assert_eq!(code, Some(0), "{log}\nmock:\n{}", mock.log());
    // Every page of the listing, by its cursor.
    let lists = marked(&mock, "MOCK_SKILLS")
        .into_iter()
        .filter(|l| l.starts_with("skills/list"))
        .collect::<Vec<_>>();
    assert_eq!(
        lists,
        [
            "skills/list {}",
            "skills/list {\"cursor\":\"page-1\"}",
            "skills/list {\"cursor\":\"page-2\"}"
        ],
        "{}",
        mock.log()
    );
    let disc = &events(&log, "skills.discovered")[0];
    assert_eq!(disc["server"], "mock");
    let names: Vec<&str> = disc["skills"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["name"].as_str().unwrap())
        .collect();
    assert_eq!(
        names,
        [
            "git-workflow",
            "pdf-processing",
            "refunds",
            "handbook",
            "tampered",
            "mismatch"
        ],
        "{disc}"
    );
    assert!(
        disc["skills"]
            .as_array()
            .unwrap()
            .iter()
            .any(|s| s["uri"] == "skill://acme/billing/refunds/SKILL.md"),
        "{disc}"
    );
    // The entries that break a rule are declined at discovery, by uri.
    assert!(
        declined_at(&log, "skill://broken/SKILL.md")["reason"]
            .as_str()
            .unwrap()
            .contains("no `resources`")
    );
    assert!(
        declined_at(&log, "skill://reports/daily/SKILL.md")["reason"]
            .as_str()
            .unwrap()
            .contains("dynamic")
    );
    let all = events(&log, "skills.loaded");
    for (skill, uri) in [
        ("git-workflow", "skill://git-workflow/SKILL.md"),
        ("refunds", "skill://acme/billing/refunds/SKILL.md"),
        ("pdf-processing", "skill://pdf-processing/SKILL.md"),
    ] {
        let e = loaded(&all, skill).unwrap_or_else(|| panic!("{skill} not loaded:\n{log}"));
        assert_eq!(e["server"], "mock", "{e}");
        assert_eq!(e["uri"], uri, "{e}");
        assert_eq!(e["verified"], true, "{e}");
        assert_eq!(mock.reads(uri), 1, "SKILL.md read once: {}", mock.log());
    }
    // Files wait to be asked for; nothing needed a refresh.
    assert_eq!(mock.reads("skill://pdf-processing/templates/invoice.md"), 0);
    assert!(
        !marked(&mock, "MOCK_SKILLS")
            .iter()
            .any(|l| l.starts_with("skills/get")),
        "{}",
        mock.log()
    );
    assert_eq!(mock.refused(), 0, "{}", mock.log());
}

#[test]
fn a_skill_whose_bytes_or_frontmatter_do_not_match_is_declined() {
    let mock = mock();
    let cfg = config(
        "Follow @skill:tampered and @skill:mismatch.",
        &[("mock", &mock.uri())],
        "",
        &sources(&["mock"], ""),
    );
    let (code, log) = life("skills-tampered", &cfg);
    assert_eq!(code, Some(0), "{log}");
    assert!(events(&log, "skills.loaded").is_empty(), "{log}");
    let t = declined_at(&log, "skill://tampered/SKILL.md");
    assert_eq!(t["server"], "mock");
    assert_eq!(t["skill"], "tampered");
    assert_eq!(t["ctx"], "root", "{t}");
    let why = t["reason"].as_str().unwrap();
    assert!(
        why.contains("does not match its digest") || why.contains("bytes, not the"),
        "{t}"
    );
    // The entry was fetched again before the skill was declined.
    assert!(
        marked(&mock, "MOCK_SKILLS")
            .iter()
            .any(|l| l == "skills/get {\"uri\":\"skill://tampered/SKILL.md\"}"),
        "{}",
        mock.log()
    );
    let m = declined_at(&log, "skill://mismatch/SKILL.md");
    assert!(
        m["reason"]
            .as_str()
            .unwrap()
            .contains("frontmatter differs from its entry at `description`"),
        "{m}"
    );
}

#[test]
fn a_dynamic_skill_is_declined_unless_its_source_allows_it() {
    let mock = mock();
    let (code, log) = life(
        "skills-dynamic-refused",
        &config(
            "Follow @skill:daily.",
            &[("mock", &mock.uri())],
            "",
            &sources(&["mock"], ""),
        ),
    );
    assert_eq!(code, Some(0), "{log}");
    assert!(events(&log, "skills.loaded").is_empty(), "{log}");
    // The instruction's reference is told why: declined at discovery.
    assert!(
        events(&log, "skills.declined")
            .iter()
            .any(|e| e["uri"] == "skill://reports/daily/SKILL.md"
                && e["skill"] == "daily"
                && e["ctx"] == "root"),
        "{log}"
    );
    let (code, log) = life(
        "skills-dynamic-allowed",
        &config(
            "Follow @skill:daily.",
            &[("mock", &mock.uri())],
            "",
            &sources(&["mock"], ", dynamic: allow"),
        ),
    );
    assert_eq!(code, Some(0), "{log}");
    let e = loaded(&events(&log, "skills.loaded"), "daily")
        .cloned()
        .unwrap_or_else(|| panic!("daily not loaded:\n{log}"));
    assert_eq!(e["verified"], false, "{e}");
    assert_eq!(e["uri"], "skill://reports/daily/SKILL.md");
    // Its files are listed through the directory reads the server declares,
    // its subdirectory read in turn.
    let dirs: Vec<String> = marked(&mock, "MOCK_SKILLS")
        .into_iter()
        .filter(|l| l.starts_with("resources/directory/read"))
        .collect();
    assert_eq!(
        dirs,
        [
            "resources/directory/read {\"uri\":\"skill://reports/daily\"}",
            "resources/directory/read {\"uri\":\"skill://reports/daily/templates\"}"
        ],
        "{}",
        mock.log()
    );
}

#[test]
fn a_skill_in_no_listing_is_got_by_its_uri() {
    let mock = mock();
    let (code, log) = life(
        "skills-hidden",
        &config(
            "Follow @skill:mock/hidden.",
            &[("mock", &mock.uri())],
            "",
            &sources(&["mock"], ""),
        ),
    );
    assert_eq!(code, Some(0), "{log}");
    assert!(
        marked(&mock, "MOCK_SKILLS")
            .iter()
            .any(|l| l == "skills/get {\"uri\":\"skill://hidden/SKILL.md\"}"),
        "{}",
        mock.log()
    );
    let e = loaded(&events(&log, "skills.loaded"), "hidden")
        .cloned()
        .unwrap_or_else(|| panic!("hidden not loaded:\n{log}"));
    assert_eq!(e["uri"], "skill://hidden/SKILL.md");
}

#[test]
fn two_servers_serving_one_name_are_told_apart_by_their_server() {
    let (a, b) = (mock(), mock());
    let cfg = config(
        "Follow @skill:git-workflow, then @skill:b/git-workflow.",
        &[("a", &a.uri()), ("b", &b.uri())],
        "",
        &sources(&["a", "b"], ""),
    );
    let (code, log) = life("skills-two-servers", &cfg);
    assert_eq!(code, Some(0), "{log}");
    let unknown = events(&log, "skills.unknown");
    let bare = unknown
        .iter()
        .find(|e| e["skill"] == "git-workflow")
        .unwrap_or_else(|| panic!("the bare name loaded something:\n{log}"));
    assert_eq!(
        bare["candidates"],
        json!(["a/git-workflow", "b/git-workflow"]),
        "{bare}"
    );
    let all = events(&log, "skills.loaded");
    let e = loaded(&all, "b/git-workflow").unwrap_or_else(|| panic!("{log}"));
    assert_eq!(e["server"], "b");
    assert_eq!(e["uri"], "skill://git-workflow/SKILL.md");
    assert_eq!(all.len(), 1, "{log}");
    assert_eq!(b.reads("skill://git-workflow/SKILL.md"), 1);
    assert_eq!(a.reads("skill://git-workflow/SKILL.md"), 0, "never a's");
    // The shared names are said, each by the reference that reaches it.
    assert!(
        events(&log, "skills.collision")
            .iter()
            .any(|c| c["name"] == "git-workflow"
                && c["candidates"] == json!(["a/git-workflow", "b/git-workflow"])),
        "{log}"
    );
}

#[test]
fn a_server_that_does_not_declare_the_extension_is_never_asked() {
    let quiet = common::spawn_mock_mcp_with("2026-07-28", "mock://res", false, &["--no-skills"]);
    let old = common::spawn_mock_mcp_at("2025-11-25", "mock://res", false);
    let cfg = config(
        "Follow @skill:git-workflow.",
        &[("quiet", &quiet.uri()), ("old", &old.uri())],
        "",
        &sources(&["quiet", "old"], ""),
    );
    let (code, log) = life("skills-undeclared", &cfg);
    assert_eq!(code, Some(0), "{log}");
    let unavailable = events(&log, "skills.source.unavailable");
    // The 2025-11-25 mock names the extension too: below 2026-07-28 that is
    // not a declaration.
    for (server, why) in [
        ("quiet", "does not declare io.modelcontextprotocol/skills"),
        (
            "old",
            "reached MCP 2025-11-25; the Skills extension is defined only from 2026-07-28",
        ),
    ] {
        let e = unavailable
            .iter()
            .find(|e| e["server"] == server)
            .unwrap_or_else(|| panic!("{server}:\n{log}"));
        assert!(e["reason"].as_str().unwrap().contains(why), "{e}");
    }
    for m in [&quiet, &old] {
        assert!(marked(m, "MOCK_SKILLS").is_empty(), "{}", m.log());
        assert!(
            m.metas().iter().all(|r| !r["method"]
                .as_str()
                .unwrap_or_default()
                .starts_with("skills/")),
            "{}",
            m.log()
        );
    }
    assert!(events(&log, "skills.loaded").is_empty());
}

/// A server's prompts and its `skill://` resources are what they are: a
/// prompt and a resource. Neither is listed, read or loaded as a skill.
#[test]
fn prompts_and_skill_resources_are_not_skills() {
    for revision in ["2025-11-25", "2026-07-28"] {
        let m = common::spawn_mock_mcp_at(revision, "mock://res", false);
        let cfg = config(
            "Follow @skill:review-pr and @skill:incident-runbook.",
            &[("mock", &m.uri())],
            "",
            &sources(&["mock"], ""),
        );
        let (code, log) = life("skills-not-prompts", &cfg);
        assert_eq!(code, Some(0), "{revision}: {log}");
        assert!(
            events(&log, "skills.loaded").is_empty(),
            "{revision}: {log}"
        );
        let unknown: Vec<String> = events(&log, "skills.unknown")
            .iter()
            .filter_map(|e| e["skill"].as_str().map(str::to_string))
            .collect();
        assert_eq!(
            unknown,
            ["review-pr", "incident-runbook"],
            "{revision}: {log}"
        );
        let asked: Vec<String> = m
            .metas()
            .iter()
            .filter_map(|r| r["method"].as_str().map(str::to_string))
            .collect();
        assert!(
            !asked
                .iter()
                .any(|x| x == "prompts/list" || x == "prompts/get"),
            "{revision}: {asked:?}"
        );
        assert_eq!(m.reads("skill://incident-runbook"), 0, "{revision}");
    }
}

/// A model reads a skill's files with `skills.read`: through the skill's own
/// server, and only bytes that match its entry.
#[test]
fn a_model_reads_a_skills_file_through_its_server_and_never_a_tampered_one() {
    let mock = mock();
    let playbook = common::unique_path("skills-pb", "json");
    std::fs::write(
        &playbook,
        json!({"turns": [
            {"tool_calls": [{"name": "skills.load", "arguments": {"name": "handbook"}}]},
            {"tool_calls": [{"name": "skills.read", "arguments": {"name": "handbook", "path": "references/hours.md"}}]},
            {"tool_calls": [{"name": "skills.read", "arguments": {"name": "handbook", "path": "references/policy.md"}}]},
            {"tool_calls": [{"name": "skills.read", "arguments": {"name": "handbook", "path": "references/unlisted.md"}}]},
            {"tool_calls": [{"name": "skills.load", "arguments": {"name": "tampered"}}]},
            {"echo_tool_result": true}
        ]})
        .to_string(),
    )
    .unwrap();
    let steps = "      a: {kind: agent, depends_on: [s], instruction: \"Answer from the handbook.\"}\n\
                 \x20     f: {kind: finish, depends_on: [a], status: completed, output: \"{{ steps.a.output }}\"}\n";
    let extra = format!(
        "intelligence:\n  endpoints: \"mock:file:{playbook}\"\n  model: mock\n{}",
        sources(&["mock"], "")
    );
    let (code, log) = life(
        "skills-read",
        &config("Help.", &[("mock", &mock.uri())], steps, &extra),
    );
    let _ = std::fs::remove_file(&playbook);
    assert_eq!(code, Some(0), "{log}\nmock:\n{}", mock.log());
    assert_eq!(
        mock.reads("skill://handbook/references/hours.md"),
        1,
        "{}",
        mock.log()
    );
    assert_eq!(mock.reads("skill://handbook/references/policy.md"), 1);
    assert_eq!(
        mock.reads("skill://handbook/references/unlisted.md"),
        0,
        "an unlisted file is not read"
    );
    let declined = events(&log, "skills.declined");
    let policy = declined
        .iter()
        .find(|e| e["file"] == "skill://handbook/references/policy.md")
        .unwrap_or_else(|| panic!("the tampered file was not declined:\n{log}"));
    assert_eq!(policy["server"], "mock");
    assert_eq!(policy["uri"], "skill://handbook/SKILL.md");
    assert!(
        declined
            .iter()
            .any(|e| e["file"] == "skill://handbook/references/unlisted.md"),
        "{log}"
    );
    // The tool call that loaded nothing is told so, as its result.
    let out = events(&log, "run.done")[0]["output"].to_string();
    assert!(
        out.contains("skills.declined") && out.contains("tampered"),
        "{out}"
    );
    assert!(
        loaded(&events(&log, "skills.loaded"), "handbook").is_some(),
        "{log}"
    );
}

/// A skill may hold one file of 16 MiB, which every host must read: an
/// answer to `resources/read` is not held to the 8 MiB other answers are.
#[test]
fn a_file_past_eight_mib_is_read_whole_and_checked() {
    let mock = mock();
    let playbook = common::unique_path("skills-big-pb", "json");
    std::fs::write(
        &playbook,
        json!({"turns": [
            {"tool_calls": [{"name": "skills.load", "arguments": {"server": "mock", "uri": "skill://big/SKILL.md"}}]},
            {"tool_calls": [{"name": "skills.read", "arguments": {"name": "big", "path": "assets/data.txt"}}]},
            {"echo_tool_result": true}
        ]})
        .to_string(),
    )
    .unwrap();
    let steps = "      a: {kind: agent, depends_on: [s], instruction: \"Read the data.\"}\n\
                 \x20     f: {kind: finish, depends_on: [a], status: completed, output: \"{{ steps.a.output }}\"}\n";
    let extra = format!(
        "intelligence:\n  endpoints: \"mock:file:{playbook}\"\n  model: mock\n{}",
        sources(&["mock"], "")
    );
    let (code, log) = life(
        "skills-big",
        &config("Help.", &[("mock", &mock.uri())], steps, &extra),
    );
    let _ = std::fs::remove_file(&playbook);
    assert_eq!(code, Some(0), "{log}");
    assert_eq!(
        mock.reads("skill://big/assets/data.txt"),
        1,
        "{}",
        mock.log()
    );
    let out = match &events(&log, "run.done")[0]["output"] {
        Value::String(s) => serde_json::from_str(s).unwrap_or_else(|e| panic!("{e}:\n{log}")),
        other => other.clone(),
    };
    assert_eq!(out["size"], 8 * 1024 * 1024 + 16, "{out}");
    assert_eq!(out["verified"], true, "{out}");
    assert!(
        out["text"]
            .as_str()
            .is_some_and(|t| t.starts_with("0123456789abcdef")),
        "{out}"
    );
}

/// A context names each loaded skill by the reference it loaded it under,
/// and `skills.read` finds it by that or by what the catalogue calls it now.
/// The root gets server a's `hidden` while it is the only one; a
/// conversation then gets b's — so the name is shared and a's is
/// `a/hidden` — and loads a's from the cache. The root reads the folder
/// skill it loaded as `notes` by its full reference, `local/notes`.
#[test]
fn a_context_names_and_reads_a_skill_by_the_reference_it_loaded() {
    let (a, b) = (mock(), mock());
    let dir = common::unique_path("skills-label", "d");
    std::fs::create_dir_all(format!("{dir}/notes/references")).unwrap();
    std::fs::write(
        format!("{dir}/notes/SKILL.md"),
        "---\nname: notes\ndescription: The folder's\n---\n\nThe folder's body.\n",
    )
    .unwrap();
    std::fs::write(format!("{dir}/notes/references/a.md"), "THE-FOLDERS-FILE\n").unwrap();
    let playbook = common::unique_path("skills-label-pb", "json");
    std::fs::write(
        &playbook,
        json!({
            "match": [{"when_contains": "Skill: a/hidden — from MCP server", "content": "named a/hidden"}],
            "turns": [
                {"tool_calls": [{"name": "skills.read", "arguments": {"name": "local/notes", "path": "references/a.md"}}]},
                {"echo_tool_result": true}
            ]
        })
        .to_string(),
    )
    .unwrap();
    let cfg = common::unique_path("skills-label", "yaml");
    std::fs::write(
        &cfg,
        format!(
            "agent:\n  name: label\n  instruction: \"Use @skill:a/hidden and @skill:notes.\"\n  preflight: never\n\
             intelligence:\n  endpoints: \"mock:file:{playbook}\"\n  model: mock\n\
             store:\n  kind: memory\n\
             mcp:\n  servers:\n    - name: a\n      endpoint: {}/mcp\n    - name: b\n      endpoint: {}/mcp\n\
             skills:\n  dir: {dir}\n  sources:\n    - {{server: a}}\n    - {{server: b}}\n\
             workflows:\n  - name: idle\n    steps:\n      s: {{kind: manual}}\n      f: {{kind: finish, depends_on: [s]}}\n\
             lifecycle:\n  run_until: idle\n  idle_grace: 1s\n\
             observability:\n  log_level: info\n  log_content: true\n",
            a.uri(),
            b.uri()
        ),
    )
    .unwrap();
    let inbox = common::unique_path("skills-label-inbox", "json");
    std::fs::write(
        &inbox,
        json!([
            {"kind": "a2a_message", "principal": "user:alice", "payload": {"context_id": "c1", "text": "Use @skill:b/hidden and @skill:a/hidden."}},
            {"kind": "a2a_message", "principal": "operator", "payload": {"context_id": "root", "text": "Read the folder's file."}}
        ])
        .to_string(),
    )
    .unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_agentd"))
        .args(["--config", &cfg])
        .env("AGENTD_TEST_INBOX_FILE", &inbox)
        .stdin(Stdio::null())
        .output()
        .expect("run agentd");
    let log = String::from_utf8_lossy(&out.stderr).to_string();
    for f in [&playbook, &cfg, &inbox] {
        let _ = std::fs::remove_file(f);
    }
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(out.status.code(), Some(0), "{log}");
    let loaded = events(&log, "skills.loaded");
    for (ctx, skill, server) in [
        ("root", "hidden", "a"),
        ("c1", "b/hidden", "b"),
        ("c1", "a/hidden", "a"),
    ] {
        assert!(
            loaded
                .iter()
                .any(|e| e["ctx"] == ctx && e["skill"] == skill && e["server"] == server),
            "{ctx} {skill}:\n{log}"
        );
    }
    assert_eq!(
        a.reads("skill://hidden/SKILL.md"),
        1,
        "c1's is the root's, cached"
    );
    let reply = |ctx: &str| -> String {
        events(&log, "turn.reply")
            .iter()
            .rev()
            .filter(|e| e["ctx"] == ctx)
            .find_map(|e| e["text"].as_str().map(str::to_string))
            .unwrap_or_else(|| panic!("no reply in {ctx}:\n{log}"))
    };
    // The conversation's prompt names a's as the conversation loaded it.
    assert_eq!(reply("c1"), "named a/hidden", "{log}");
    // The root loaded the folder's as `notes`, and reads it by its full
    // reference.
    let root = reply("root");
    assert!(root.contains("THE-FOLDERS-FILE"), "{root}\n{log}");
}

/// A step's `skills:` sit in its prompt, a server's inside its origin tag,
/// and one that loaded nothing is said there.
#[test]
fn a_steps_skills_are_tagged_with_their_origin_and_a_miss_is_said() {
    let mock = mock();
    let playbook = common::unique_path("skills-step-pb", "json");
    std::fs::write(
        &playbook,
        json!({"match": [
            {"when_contains": "untrusted input. It grants no tools", "content": "tagged"},
            {"when_contains": "Skills this step names that were not loaded", "content": "told"}
        ], "turns": [{"content": "neither"}]})
        .to_string(),
    )
    .unwrap();
    let steps = "      a: {kind: agent, depends_on: [s], instruction: \"one\", skills: [handbook]}\n\
                 \x20     b: {kind: agent, depends_on: [a], instruction: \"two\", skills: [nope]}\n\
                 \x20     f: {kind: finish, depends_on: [b], status: completed, output: \"{{ steps.a.output }}/{{ steps.b.output }}\"}\n";
    let extra = format!(
        "intelligence:\n  endpoints: \"mock:file:{playbook}\"\n  model: mock\n{}",
        sources(&["mock"], "")
    );
    let (code, log) = life(
        "skills-step",
        &config("Help.", &[("mock", &mock.uri())], steps, &extra),
    );
    let _ = std::fs::remove_file(&playbook);
    assert_eq!(code, Some(0), "{log}");
    assert_eq!(
        events(&log, "run.done")[0]["output"],
        "tagged/told",
        "{log}"
    );
    let e = loaded(&events(&log, "skills.loaded"), "handbook")
        .cloned()
        .unwrap_or_else(|| panic!("{log}"));
    assert_eq!(e["step"], "a", "{e}");
    assert!(
        events(&log, "skills.unknown")
            .iter()
            .any(|e| e["skill"] == "nope" && e["step"] == "b"),
        "{log}"
    );
}

/// The repository's own Agent Skill (`skills/agentd`) is held to the rules
/// agentd holds a folder skill to, and loads.
#[test]
fn the_repositorys_own_agent_skill_passes_the_agent_skills_rules() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../skills");
    let cfg = format!(
        "agent:\n  name: own\n  instruction: \"Use @skill:agentd.\"\nstore:\n  kind: memory\n\
         skills:\n  dir: {}\n\
         workflows:\n  - name: job\n    steps:\n      s: {{kind: once}}\n\
         \x20     f: {{kind: finish, depends_on: [s], status: completed}}\n\
         lifecycle:\n  run_until: idle\n  idle_grace: 200ms\n\
         observability:\n  log_level: info\n",
        dir.display()
    );
    let (code, log) = life("skills-own", &cfg);
    assert_eq!(code, Some(0), "{log}");
    let e = loaded(&events(&log, "skills.loaded"), "agentd")
        .cloned()
        .unwrap_or_else(|| panic!("{log}"));
    assert!(
        e["path"]
            .as_str()
            .unwrap()
            .ends_with("skills/agentd/SKILL.md"),
        "{e}"
    );
}
