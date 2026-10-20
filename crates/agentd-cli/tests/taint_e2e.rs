// SPDX-License-Identifier: AGPL-3.0-only
//! The taint engine (RFC 0044 §5.13 item 5): outside text a run reads —
//! here an `untrusted_input` server's result, through `mcp.tool`, a built-in
//! `search.*` step or `mcp.resource` — marks it tainted before any later
//! step reads it; a memory entry, an artifact or a cached step output the
//! run leaves keeps that, and a run that reads it in another life of the
//! daemon is tainted by it. The policy fact `tainted` holds exactly that
//! work, and leaves clean work alone.
#![cfg(unix)]

mod common;

use std::process::{Command, Stdio};

/// The daemon on `cfg_text` in `dir` (its file store under `dir/state`),
/// run to idle: its exit code and log.
fn run(dir: &str, cfg_text: &str) -> (Option<i32>, String) {
    let cfg = format!("{dir}/c.yaml");
    std::fs::write(&cfg, cfg_text.replace("__STATE__", &format!("{dir}/state"))).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_agentd"))
        .args(["--config", &cfg])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .output()
        .expect("run");
    (
        out.status.code(),
        String::from_utf8_lossy(&out.stderr).to_string(),
    )
}

/// The configuration: a server whose results are outside text — the one
/// `search.*` is mapped to — the same server again serving no tool at all,
/// only resources, a rule that refuses `memory.set` to tainted work, and
/// `workflows`.
fn config(web: &str, workflows: &str) -> String {
    format!(
        "agent: {{ name: taint }}\n\
         store: {{ kind: file, file: {{ path: __STATE__ }} }}\n\
         observability: {{ log_level: info }}\n\
         lifecycle: {{ run_until: idle, idle_grace: 1s }}\n\
         search: {{ server: web }}\n\
         mcp:\n  servers:\n\
         \x20   - {{ name: web, endpoint: \"{web}\", tags: {{ \"*\": [untrusted_input] }} }}\n\
         \x20   - {{ name: docs, endpoint: \"{web}\", tags: {{ \"*\": [untrusted_input] }}, exclude: [\"*\"] }}\n\
         security:\n  policies:\n\
         \x20   - match: {{ tool: \"memory.set\", tainted: true }}\n      action: deny\n\
         workflows:\n{workflows}"
    )
}

const DENIED: &str = "denied by security.policies[0]";

/// A run that read an `untrusted_input` server's result meets the rule; a
/// run that read nothing from outside does not.
#[test]
fn an_untrusted_result_taints_its_run_and_the_tainted_fact_holds_it() {
    let mock = common::spawn_mock_mcp("mock://res", false);
    let dir = common::unique_path("taint-run", "d");
    std::fs::create_dir_all(&dir).unwrap();
    let clean = "  - name: clean\n    steps:\n\
                 \x20     s: { kind: once }\n\
                 \x20     m: { kind: memory.set, key: a, value: v, depends_on: [s] }\n\
                 \x20     f: { kind: finish, depends_on: [m], status: completed }\n";
    let (code, log) = run(&dir, &config(&mock.uri(), clean));
    assert_eq!(code, Some(0), "{log}");
    assert!(!log.contains(DENIED), "clean work is not tainted\n{log}");

    let dirty = "  - name: dirty\n    steps:\n\
                 \x20     s: { kind: once }\n\
                 \x20     q: { kind: mcp.tool, server: web, tool: search.query, args: { query: news }, depends_on: [s] }\n\
                 \x20     m: { kind: memory.set, key: b, value: v, depends_on: [q] }\n\
                 \x20     f: { kind: finish, depends_on: [m], status: completed }\n";
    let (_, log) = run(&dir, &config(&mock.uri(), dirty));
    assert!(
        log.contains(DENIED),
        "the step after the untrusted read meets the tainted rule\n{log}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Taint follows a memory entry: the entry a tainted run pushes keeps it,
/// across a restart, and the run that reads it — in the next life of the
/// daemon — is tainted by it, while a run reading a clean entry is not.
#[test]
fn taint_follows_a_memory_entry_into_its_reader() {
    let mock = common::spawn_mock_mcp("mock://res", false);
    let dir = common::unique_path("taint-memory", "d");
    std::fs::create_dir_all(&dir).unwrap();
    let writer = "  - name: writer\n    steps:\n\
                  \x20     s: { kind: once }\n\
                  \x20     q: { kind: mcp.tool, server: web, tool: search.query, args: { query: news }, depends_on: [s] }\n\
                  \x20     p: { kind: memory.push, key: inbox, value: read, depends_on: [q] }\n\
                  \x20     c: { kind: memory.push, key: plain, value: x, depends_on: [s] }\n\
                  \x20     f: { kind: finish, depends_on: [p, c], status: completed }\n";
    let (code, log) = run(&dir, &config(&mock.uri(), writer));
    assert_eq!(code, Some(0), "{log}");
    let reader = |key: &str| {
        format!(
            "  - name: read-{key}\n    steps:\n\
             \x20     s: {{ kind: once }}\n\
             \x20     g: {{ kind: memory.get, key: {key}, depends_on: [s] }}\n\
             \x20     m: {{ kind: memory.set, key: out-{key}, value: v, depends_on: [g] }}\n\
             \x20     f: {{ kind: finish, depends_on: [m], status: completed }}\n"
        )
    };
    let (code, log) = run(&dir, &config(&mock.uri(), &reader("plain")));
    assert_eq!(code, Some(0), "{log}");
    assert!(!log.contains(DENIED), "a clean entry taints nothing\n{log}");
    let (_, log) = run(&dir, &config(&mock.uri(), &reader("inbox")));
    assert!(
        log.contains(DENIED),
        "the reader of the tainted entry is tainted\n{log}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// A built-in `search.*` step answered by an `untrusted_input` server, and
/// an `mcp.resource` read from a server whose declared tags say its results
/// are outside text though it serves no tool, each taint their run.
#[test]
fn a_search_step_and_a_resource_read_taint_their_run() {
    let mock = common::spawn_mock_mcp("mock://res", false);
    let dir = common::unique_path("taint-kinds", "d");
    std::fs::create_dir_all(&dir).unwrap();
    for (name, step) in [
        (
            "fetch",
            "{ kind: search.fetch, url: \"https://news.example/a\", depends_on: [s] }",
        ),
        (
            "resource",
            "{ kind: mcp.resource, server: docs, op: read, uri: \"mock://res\", depends_on: [s] }",
        ),
    ] {
        let wf = format!(
            "  - name: {name}\n    steps:\n\
             \x20     s: {{ kind: once }}\n\
             \x20     q: {step}\n\
             \x20     m: {{ kind: memory.set, key: {name}, value: v, depends_on: [q] }}\n\
             \x20     f: {{ kind: finish, depends_on: [m], status: completed }}\n"
        );
        let (_, log) = run(&dir, &config(&mock.uri(), &wf));
        assert!(
            log.contains("\"event\":\"step.done\"") && log.contains(DENIED),
            "{name}: the step after the outside text meets the tainted rule\n{log}"
        );
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// The workflows that write one artifact, `inbox` after an untrusted read
/// and `plain` after none, and the one that reads `key`'s back.
fn artifact_writer(name: &str, read_first: bool) -> String {
    let (q, after) = if read_first {
        (
            "\x20     q: { kind: mcp.tool, server: web, tool: search.query, args: { query: news }, depends_on: [s] }\n",
            "q",
        )
    } else {
        ("", "s")
    };
    format!(
        "  - name: write-{name}\n    steps:\n\
         \x20     s: {{ kind: once }}\n{q}\
         \x20     a: {{ kind: artifact.create, name: {name}, content: read, depends_on: [{after}] }}\n\
         \x20     f: {{ kind: finish, depends_on: [a], status: completed }}\n"
    )
}

/// Taint follows an artifact as it does a memory entry: the artifact a
/// tainted run creates keeps it, across a restart, and the run that reads
/// it is tainted by it, while a run reading a clean one is not.
#[test]
fn taint_follows_an_artifact_into_its_reader() {
    let mock = common::spawn_mock_mcp("mock://res", false);
    let dir = common::unique_path("taint-artifact", "d");
    std::fs::create_dir_all(&dir).unwrap();
    for (name, dirty) in [("plain", false), ("inbox", true)] {
        let (code, log) = run(&dir, &config(&mock.uri(), &artifact_writer(name, dirty)));
        assert_eq!(code, Some(0), "{log}");
    }
    let reader = |name: &str| {
        format!(
            "  - name: read-{name}\n    steps:\n\
             \x20     s: {{ kind: once }}\n\
             \x20     l: {{ kind: tool, name: artifact.list, args: {{ prefix: {name} }}, depends_on: [s] }}\n\
             \x20     g: {{ kind: artifact.get, id: \"{{{{steps.l.output.artifacts.0.id}}}}\", depends_on: [l] }}\n\
             \x20     m: {{ kind: memory.set, key: out-{name}, value: v, depends_on: [g] }}\n\
             \x20     f: {{ kind: finish, depends_on: [m], status: completed }}\n"
        )
    };
    let (code, log) = run(&dir, &config(&mock.uri(), &reader("plain")));
    assert_eq!(code, Some(0), "{log}");
    assert!(
        log.contains("\"step\":\"g\"") && !log.contains(DENIED),
        "a clean artifact taints nothing\n{log}"
    );
    let (_, log) = run(&dir, &config(&mock.uri(), &reader("inbox")));
    assert!(
        log.contains(DENIED),
        "the reader of the tainted artifact is tainted\n{log}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// A cached step output keeps what the run that produced it carried: the
/// run that is handed it from the cache, in another life, reads nothing
/// itself and is tainted all the same.
#[test]
fn a_cached_output_keeps_its_taint() {
    let mock = common::spawn_mock_mcp("mock://res", false);
    let dir = common::unique_path("taint-cache", "d");
    std::fs::create_dir_all(&dir).unwrap();
    let writer = "  - name: writer\n    steps:\n\
                  \x20     s: { kind: once }\n\
                  \x20     q: { kind: mcp.tool, server: web, tool: search.query, args: { query: news }, depends_on: [s] }\n\
                  \x20     p: { kind: memory.push, key: inbox, value: read, depends_on: [q] }\n\
                  \x20     f: { kind: finish, depends_on: [p], status: completed }\n";
    let (code, log) = run(&dir, &config(&mock.uri(), writer));
    assert_eq!(code, Some(0), "{log}");
    // Two workflows, one `once` each, with the same cached step: its key is
    // the step's kind, id and spec.
    let reader = |n: u8| {
        format!(
            "  - name: reader-{n}\n    steps:\n\
             \x20     s: {{ kind: once }}\n\
             \x20     g: {{ kind: memory.get, key: inbox, cache: {{ ttl: 1h }}, depends_on: [s] }}\n\
             \x20     m: {{ kind: memory.set, key: out, value: v, depends_on: [g] }}\n\
             \x20     f: {{ kind: finish, depends_on: [m], status: completed }}\n"
        )
    };
    let (_, log) = run(&dir, &config(&mock.uri(), &reader(1)));
    assert!(
        log.contains(DENIED),
        "the first reader reads the entry and is tainted\n{log}"
    );
    let (_, log) = run(&dir, &config(&mock.uri(), &reader(2)));
    assert!(
        log.contains("\"event\":\"step.cache_hit\""),
        "the second reader is handed the cached output\n{log}"
    );
    assert!(
        log.contains(DENIED),
        "and is tainted by what the run that produced it read\n{log}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}
