// SPDX-License-Identifier: AGPL-3.0-only
//! The taint engine (RFC 0044 §5.13 item 5): outside text a run reads —
//! here an `untrusted_input` server's result, through `mcp.tool`, a built-in
//! `search.*` step or `mcp.resource` — marks it tainted before any later
//! step reads it; a memory entry, an artifact or a cached step output the
//! run leaves keeps that, and a run that reads it in another life of the
//! daemon is tainted by it. A server that declares no tag is an
//! `untrusted_input` one, through a step or a turn, until the operator tags
//! it — `trusted`, or its legs without `untrusted_input`; a server's skill
//! taints whatever the server is tagged. The policy fact `tainted` holds
//! exactly that work, and leaves clean work alone.
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

/// A configuration whose servers `servers` — `(name, tags)` pairs, each at
/// `uri` under the namespace of its name, `tags` written under it unless
/// empty — sit beside the rule that refuses `memory.set` to tainted work,
/// with `extra` at the top and `workflows`.
fn served(uri: &str, servers: &[(&str, &str)], extra: &str, workflows: &str) -> String {
    let servers: String = servers
        .iter()
        .map(|(name, tags)| {
            let tags = if tags.is_empty() {
                String::new()
            } else {
                format!(", tags: {{ \"*\": {tags} }}")
            };
            format!("\x20   - {{ name: {name}, ns: {name}, endpoint: \"{uri}\"{tags} }}\n")
        })
        .collect();
    format!(
        "agent: {{ name: taint }}\n\
         store: {{ kind: file, file: {{ path: __STATE__ }} }}\n\
         observability: {{ log_level: info }}\n\
         lifecycle: {{ run_until: idle, idle_grace: 1s }}\n{extra}\
         mcp:\n  servers:\n{servers}\
         security:\n  policies:\n\
         \x20   - match: {{ tool: \"memory.set\", tainted: true }}\n      action: deny\n\
         workflows:\n{workflows}"
    )
}

/// The servers every case below is offered: one that declares no tag, one
/// the operator vouched for with `trusted`, and one tagged with the leg it
/// carries, `untrusted_input` left out.
const SERVERS: [(&str, &str); 3] = [
    ("plain", ""),
    ("vouched", "[trusted]"),
    ("inner", "[sensitive]"),
];

/// A server that declares no tag is untrusted by default: what it answers
/// an `mcp.tool`, an `mcp.resource` or a `tool` step, and the resource a
/// `wait {on: resource}` reads from it, taints the run, as an
/// `untrusted_input` server's does — while one tagged `trusted`, or tagged
/// with its legs and not `untrusted_input`, leaves the run clean.
#[test]
fn an_untagged_servers_result_taints_its_run_and_a_vouched_ones_does_not() {
    let mock = common::spawn_mock_mcp("mock://res", false);
    let dir = common::unique_path("taint-untagged", "d");
    std::fs::create_dir_all(&dir).unwrap();
    for (server, tags) in SERVERS {
        for (i, step) in [
            format!("{{ kind: mcp.tool, server: {server}, tool: mock.ops, depends_on: [s] }}"),
            format!(
                "{{ kind: mcp.resource, server: {server}, op: read, uri: \"mock://res\", depends_on: [s] }}"
            ),
            format!("{{ kind: tool, name: {server}.mock.ops, depends_on: [s] }}"),
            format!(
                "{{ kind: wait, on: resource, server: {server}, uri: \"mock://res\", timeout: 20s, depends_on: [s] }}"
            ),
        ]
        .iter()
        .enumerate()
        {
            let dir = format!("{dir}/{server}-{i}");
            std::fs::create_dir_all(&dir).unwrap();
            let wf = format!(
                "  - name: read\n    steps:\n\
                 \x20     s: {{ kind: once }}\n\
                 \x20     q: {step}\n\
                 \x20     m: {{ kind: memory.set, key: k, value: v, depends_on: [q] }}\n\
                 \x20     f: {{ kind: finish, depends_on: [m], status: completed }}\n"
            );
            // The wait's server alone, on a mock of its own that says the
            // resource changed once the wait subscribes: its one push goes
            // down whichever stream asks first.
            let emitting = step
                .contains("kind: wait")
                .then(|| common::spawn_mock_mcp("mock://res", true));
            let cfg = match &emitting {
                Some(e) => served(&e.uri(), &[(server, tags)], "", &wf),
                None => served(&mock.uri(), &SERVERS, "", &wf),
            };
            let (_, log) = run(&dir, &cfg);
            assert!(
                log.lines()
                    .any(|l| l.contains("\"step\":\"q\"") && l.contains("\"status\":\"done\"")),
                "{server} {step}: the read itself went ahead\n{log}"
            );
            assert_eq!(
                log.contains(DENIED),
                server == "plain",
                "{server} {step}: only the untagged server's result taints\n{log}"
            );
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// A step whose `server:` is a template is judged by the server it names
/// once rendered: an untagged one's result taints the run, through
/// `mcp.tool`, `mcp.resource` and a resource `wait`, as when the definition
/// names it.
#[test]
fn a_templated_server_is_judged_by_the_server_it_renders_to() {
    let mock = common::spawn_mock_mcp("mock://res", false);
    let dir = common::unique_path("taint-templated", "d");
    std::fs::create_dir_all(&dir).unwrap();
    for (server, tags) in SERVERS {
        for (i, step) in [
            "{ kind: mcp.tool, server: \"{{ inputs.srv }}\", tool: mock.ops, depends_on: [s] }",
            "{ kind: mcp.resource, server: \"{{ inputs.srv }}\", op: read, uri: \"mock://res\", depends_on: [s] }",
            "{ kind: wait, on: resource, server: \"{{ inputs.srv }}\", uri: \"mock://res\", timeout: 20s, depends_on: [s] }",
        ]
        .iter()
        .enumerate()
        {
            let dir = format!("{dir}/{server}-{i}");
            std::fs::create_dir_all(&dir).unwrap();
            let wf = format!(
                "  - name: read\n    steps:\n\
                 \x20     s: {{ kind: once, inputs: {{ srv: {server} }} }}\n\
                 \x20     q: {step}\n\
                 \x20     m: {{ kind: memory.set, key: k, value: v, depends_on: [q] }}\n\
                 \x20     f: {{ kind: finish, depends_on: [m], status: completed }}\n"
            );
            let emitting = step
                .contains("kind: wait")
                .then(|| common::spawn_mock_mcp("mock://res", true));
            let cfg = match &emitting {
                Some(e) => served(&e.uri(), &[(server, tags)], "", &wf),
                None => served(&mock.uri(), &SERVERS, "", &wf),
            };
            let (_, log) = run(&dir, &cfg);
            assert!(
                log.lines()
                    .any(|l| l.contains("\"step\":\"q\"") && l.contains("\"status\":\"done\"")),
                "{server} {step}: the read itself went ahead\n{log}"
            );
            assert_eq!(
                log.contains(DENIED),
                server == "plain",
                "{server} {step}: only the untagged server's result taints\n{log}"
            );
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// A `memory.*` or `artifact.*` step that `tools.overrides` maps onto a
/// server is that server's call: an untagged one's result taints the run,
/// as the same call through a `tool` step does, and one tagged `trusted`
/// leaves it clean.
#[test]
fn a_contract_step_mapped_onto_an_untagged_server_taints_its_run() {
    let mock = common::spawn_mock_mcp("mock://res", false);
    let dir = common::unique_path("taint-mapped", "d");
    std::fs::create_dir_all(&dir).unwrap();
    for server in ["plain", "vouched"] {
        let extra = "tools:\n  overrides:\n\
                     \x20   memory.get: { server: SERVER, tool: mock.ops, result: \"{\\\"found\\\": true, \\\"value\\\": {{result.structuredContent}}}\" }\n\
                     \x20   artifact.get: { server: SERVER, tool: mock.ops, result: \"{\\\"id\\\": \\\"a\\\", \\\"content\\\": {{result.structuredContent}}}\" }\n"
            .replace("SERVER", server);
        for (i, step) in [
            "{ kind: memory.get, key: k, depends_on: [s] }",
            "{ kind: artifact.get, id: a, depends_on: [s] }",
        ]
        .iter()
        .enumerate()
        {
            let dir = format!("{dir}/{server}-{i}");
            std::fs::create_dir_all(&dir).unwrap();
            let wf = format!(
                "  - name: recall\n    steps:\n\
                 \x20     s: {{ kind: once }}\n\
                 \x20     q: {step}\n\
                 \x20     m: {{ kind: memory.set, key: k2, value: v, depends_on: [q] }}\n\
                 \x20     f: {{ kind: finish, depends_on: [m], status: completed }}\n"
            );
            let (_, log) = run(&dir, &served(&mock.uri(), &SERVERS, &extra, &wf));
            assert!(
                log.lines()
                    .any(|l| l.contains("\"step\":\"q\"") && l.contains("\"status\":\"done\"")),
                "{server} {step}: the mapped call went ahead\n{log}"
            );
            assert_eq!(
                log.contains(DENIED),
                server == "plain",
                "{server} {step}: only the untagged server's result taints\n{log}"
            );
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// A `subscribe` start fires its run with the resource it read, the
/// server's answer: from a server that declares no tag the run carries it
/// from its start — the read itself and, as the configuration says, the
/// server's resources — and a `tainted` rule holds the run; from a server
/// tagged `trusted`, or with its legs, the run is clean.
#[test]
fn a_subscribe_starts_run_carries_an_untagged_servers_resource() {
    let dir = common::unique_path("taint-subscribe", "d");
    std::fs::create_dir_all(&dir).unwrap();
    for (server, tags) in SERVERS {
        let dir = format!("{dir}/{server}");
        std::fs::create_dir_all(&dir).unwrap();
        let mock = common::spawn_mock_mcp("mock://res", true);
        let wf = format!(
            "  - name: watch\n    steps:\n\
             \x20     s: {{ kind: subscribe, server: {server}, uri: \"mock://res\", inputs: {{ c: \"{{{{ payload.content }}}}\" }} }}\n\
             \x20     t: {{ kind: tool, name: workflow.status, args: {{ run: \"{{{{ run.id }}}}\" }}, depends_on: [s] }}\n\
             \x20     m: {{ kind: memory.set, key: k, value: \"{{{{ inputs.c }}}}\", depends_on: [t], on_error: continue }}\n\
             \x20     f: {{ kind: finish, depends_on: [m], status: completed, output: \"{{{{ steps.t.output.runs.0.taint }}}}\" }}\n"
        );
        let cfg = served(&mock.uri(), &[(server, tags)], "", &wf).replace(
            "observability: { log_level: info }",
            "observability: { log_level: info, log_content: true }",
        );
        let (_, log) = run(&dir, &cfg);
        let done: Vec<serde_json::Value> = log
            .lines()
            .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
            .filter(|v| v["event"] == "run.done")
            .collect();
        assert_eq!(done.len(), 1, "{server}: the start fired once\n{log}");
        let facts = &done[0]["output"];
        assert_eq!(
            facts["tainted"],
            server == "plain",
            "{server}: {facts}\n{log}"
        );
        assert_eq!(
            log.contains(DENIED),
            server == "plain",
            "{server}: only the untagged server's resource taints\n{log}"
        );
        if server == "plain" {
            let taint: Vec<&str> = facts["taint"]
                .as_array()
                .unwrap()
                .iter()
                .filter_map(|t| t.as_str())
                .collect();
            for source in [
                "a result of the untagged server plain",
                "the resources of mcp server \"plain\"",
            ] {
                assert!(taint.contains(&source), "{source}: {facts}");
            }
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// A server an instruction document declares vouches for nothing: what it
/// answers taints the run whatever legs the document names, `trusted` on
/// it is refused at load, and only the operator's catalog entry it names
/// vouches for it.
#[test]
fn a_documents_server_is_untrusted_unless_the_operators_catalog_vouches() {
    let mock = common::spawn_mock_mcp("mock://res", false);
    let dir = common::unique_path("taint-document", "d");
    std::fs::create_dir_all(&dir).unwrap();
    let uri = mock.uri();
    let catalog = format!(
        "services:\n  docs: {{ kind: mcp, endpoint: \"{uri}\", tags: {{ \"*\": [trusted] }} }}\n"
    );
    for (i, (block, services, tainted)) in [
        (format!("endpoint: \"{uri}\"\n"), String::new(), true),
        (
            format!("endpoint: \"{uri}\"\ntags: {{\"*\": [sensitive]}}\n"),
            String::new(),
            true,
        ),
        ("service: docs\n".to_string(), catalog, false),
    ]
    .into_iter()
    .enumerate()
    {
        let dir = format!("{dir}/{i}");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            format!("{dir}/AGENT.md"),
            format!("Read the feed.\n\n:::!mcp{{name=docsrv}}\nns: docsrv\n{block}:::\n"),
        )
        .unwrap();
        let cfg = format!(
            "agent: {{ name: taint, instruction: {{ file: \"{dir}/AGENT.md\" }} }}\n\
             store: {{ kind: file, file: {{ path: __STATE__ }} }}\n\
             observability: {{ log_level: info }}\n\
             lifecycle: {{ run_until: idle, idle_grace: 1s }}\n{services}\
             security:\n  policies:\n\
             \x20   - match: {{ tool: \"memory.set\", tainted: true }}\n      action: deny\n\
             workflows:\n\
             \x20 - name: read\n    steps:\n\
             \x20     s: {{ kind: once }}\n\
             \x20     q: {{ kind: mcp.tool, server: docsrv, tool: mock.ops, depends_on: [s] }}\n\
             \x20     m: {{ kind: memory.set, key: k, value: v, depends_on: [q] }}\n\
             \x20     f: {{ kind: finish, depends_on: [m], status: completed }}\n"
        );
        let (_, log) = run(&dir, &cfg);
        assert!(
            log.lines()
                .any(|l| l.contains("\"step\":\"q\"") && l.contains("\"status\":\"done\"")),
            "{block}: the call went ahead\n{log}"
        );
        assert_eq!(log.contains(DENIED), tainted, "{block}\n{log}");
    }
    // A document saying `trusted` of its own server is refused.
    std::fs::write(
        format!("{dir}/AGENT.md"),
        format!("Read the feed.\n\n:::!mcp{{name=docsrv}}\nendpoint: \"{uri}\"\ntags: {{\"*\": [trusted]}}\n:::\n"),
    )
    .unwrap();
    let cfg = format!(
        "agent: {{ name: taint, instruction: {{ file: \"{dir}/AGENT.md\" }} }}\n\
         store: {{ kind: file, file: {{ path: __STATE__ }} }}\n"
    );
    let (code, log) = run(&dir, &cfg);
    assert_ne!(code, Some(0), "{log}");
    assert!(
        log.contains("mcp.servers[0].tags[")
            && log.contains("vouching for a server is the operator's"),
        "{log}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// A model's call of a tool an untagged server serves taints the work the
/// turn does, before its result is read: the turn's next call, `memory.set`,
/// meets the rule. The same call on the server tagged `trusted` does not.
#[test]
fn a_turns_call_of_an_untagged_servers_tool_taints_it() {
    let mock = common::spawn_mock_mcp("mock://res", false);
    let dir = common::unique_path("taint-turn", "d");
    std::fs::create_dir_all(&dir).unwrap();
    for (server, tainted) in [("plain", true), ("vouched", false)] {
        let dir = format!("{dir}/{server}");
        std::fs::create_dir_all(&dir).unwrap();
        let play = format!("{dir}/play.json");
        std::fs::write(
            &play,
            serde_json::json!({"turns": [
                {"tool_calls": [{"name": format!("{server}.mock.ops"), "arguments": {}}]},
                {"tool_calls": [{"name": "memory.set", "arguments": {"key": "k", "value": "v"}}]},
                {"content": "done"}
            ]})
            .to_string(),
        )
        .unwrap();
        let extra = format!("intelligence:\n  endpoints: \"mock:file:{play}\"\n  model: mock\n");
        let wf = "  - name: turn\n    steps:\n\
                  \x20     s: { kind: once }\n\
                  \x20     a: { kind: agent, instruction: \"read, then remember\", depends_on: [s] }\n\
                  \x20     f: { kind: finish, depends_on: [a], status: completed }\n";
        let (_, log) = run(&dir, &served(&mock.uri(), &SERVERS, &extra, wf));
        assert!(
            log.contains(&format!("\"tool\":\"{server}.mock.ops\"")),
            "{server}: the turn called the server's tool\n{log}"
        );
        assert_eq!(
            log.contains("\"event\":\"tool.policy.refused\""),
            tainted,
            "{server}: the turn's memory.set meets the tainted rule only after the untagged server's tool\n{log}"
        );
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// A skill an MCP server serves is the server's text whatever the server is
/// tagged — here `trusted` — so a step that loads it is tainted; a skill of
/// the operator's own folder taints nothing.
#[test]
fn a_servers_skill_taints_the_step_that_loads_it_and_a_folder_skill_does_not() {
    let mock = common::spawn_mock_mcp_at("2026-07-28", "mock://res", false);
    let dir = common::unique_path("taint-skill", "d");
    std::fs::create_dir_all(format!("{dir}/skills/notes")).unwrap();
    std::fs::write(
        format!("{dir}/skills/notes/SKILL.md"),
        "---\nname: notes\ndescription: The folder's\n---\n\nTake notes.\n",
    )
    .unwrap();
    let play = format!("{dir}/play.json");
    std::fs::write(
        &play,
        serde_json::json!({"turns": [{"content": "{\"ok\": true}"}]}).to_string(),
    )
    .unwrap();
    let extra = format!(
        "intelligence:\n  endpoints: \"mock:file:{play}\"\n  model: mock\n\
         skills:\n  dir: {dir}/skills\n  sources:\n    - {{ server: vouched }}\n"
    );
    for (i, (skill, tainted)) in [("vouched/handbook", true), ("local/notes", false)]
        .into_iter()
        .enumerate()
    {
        let run_dir = format!("{dir}/{i}");
        std::fs::create_dir_all(&run_dir).unwrap();
        let wf = format!(
            "  - name: skill\n    steps:\n\
             \x20     s: {{ kind: once }}\n\
             \x20     t: {{ kind: think, prompt: p, skills: [\"{skill}\"], depends_on: [s] }}\n\
             \x20     m: {{ kind: memory.set, key: k, value: v, depends_on: [t] }}\n\
             \x20     f: {{ kind: finish, depends_on: [m], status: completed }}\n"
        );
        let (_, log) = run(
            &run_dir,
            &served(&mock.uri(), &[("vouched", "[trusted]")], &extra, &wf),
        );
        assert!(
            log.contains("\"event\":\"skills.loaded\""),
            "{skill}: the step loaded it\n{log}"
        );
        assert_eq!(
            log.contains(DENIED),
            tainted,
            "{skill}: only the server's skill taints\n{log}"
        );
    }
    let _ = std::fs::remove_dir_all(&dir);
}
