// SPDX-License-Identifier: AGPL-3.0-only
//! **A document's own agents (RFC 0056), end to end.** The operator grants
//! `agents`; the instruction document — a file — defines two agents, each
//! with a `once` workflow of its own, and a workflow of its own whose `once`
//! start spawns them. The real binary composes the team at load, spawns both
//! as instance children, reaches one by its name over A2A, and each child
//! runs its own workflow in its own daemon. A reload that edits one agent and
//! removes the other retires the removed one, and retires and starts again
//! the edited one from its new composition. Served over `mcp:`, the same
//! team runs only from a pinned publisher whose signature attests `agents`.
//!
//! The arbiter of what a child ran is the CHILD's log: each instance child
//! writes its own, in its directory under the parent's durable state.
#![cfg(all(unix, feature = "a2a"))]

mod common;

use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::Value;

const WAIT: Duration = Duration::from_secs(40);

fn events(log: &str, name: &str) -> Vec<Value> {
    log.lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter(|v| v["event"] == name)
        .collect()
}

/// One agent: prose, a `once` workflow that says it is up, and a typed
/// command its parent may send it.
fn agent(id: &str, prose: &str) -> String {
    format!(
        "::::agent{{id={id} title=\"{id}\"}}\n{prose}\n\n\
         :::workflow{{id={id}-up dialect=\"instruction/workflow\"}}\n\
         name: {id}-up\nsteps:\n  s: {{kind: once}}\n  \
         f: {{kind: finish, depends_on: [s], status: completed, output: \"{id} up\"}}\n:::\n\n\
         :::workflow{{id={id}-ping dialect=\"instruction/workflow\"}}\n\
         name: {id}-ping\nsteps:\n  cmd: {{kind: a2a, command: {id}.ping, roles: [agent, operator]}}\n  \
         f: {{kind: finish, depends_on: [cmd], status: completed, output: \"pong from {id}\"}}\n:::\n\
         ::::\n\n"
    )
}

/// The document: its agents, and the workflow that opens the team — spawns
/// both, then asks `alpha` by its name.
fn document(agents: &[String]) -> String {
    format!(
        "---\nspec: \"1\"\n---\n# The team\n\nRun the team.\n\n{}\
         :::workflow{{id=open dialect=\"instruction/workflow\"}}\n\
         name: open\nsteps:\n  s: {{kind: once}}\n  \
         team:\n    kind: parallel\n    depends_on: [s]\n    branches:\n      \
         alpha: {{steps: {{run: {{kind: subagent, template: alpha}}}}}}\n      \
         beta: {{steps: {{run: {{kind: subagent, template: beta}}}}}}\n  \
         ask: {{kind: a2a.delegate, depends_on: [team], peer: alpha, command: alpha.ping, args: {{}}, timeout: 30s, retry: {{max: 8, backoff: 1s}}}}\n  \
         nap: {{kind: sleep, depends_on: [ask], duration: 3s}}\n  \
         f: {{kind: finish, depends_on: [nap], status: completed, output: \"{{{{steps.ask.output}}}}\"}}\n:::\n",
        agents.concat()
    )
}

struct Daemon {
    child: Child,
    dir: String,
    err_path: String,
}

impl Daemon {
    fn boot(doc: &str, run_until: &str) -> Daemon {
        let dir = common::unique_path("doc-agents", "d");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(format!("{dir}/team.md"), doc).unwrap();
        let cfg = format!(
            "agent: {{ name: parent, instruction: {dir}/team.md, document_capabilities: [agents] }}\n\
             store: {{ kind: file, file: {{ path: {dir}/state }} }}\n\
             intelligence: {{ endpoints: \"mock:final\", model: mock }}\n\
             lifecycle: {{ run_until: {run_until}, idle_grace: 1500ms }}\n\
             observability: {{ log_level: info, log_content: true }}\n"
        );
        std::fs::write(format!("{dir}/c.yaml"), cfg).unwrap();
        let err_path = format!("{dir}/parent.log");
        let child = Command::new(env!("CARGO_BIN_EXE_agentd"))
            .args(["--config", &format!("{dir}/c.yaml")])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(std::fs::File::create(&err_path).unwrap())
            .spawn()
            .unwrap();
        Daemon {
            child,
            dir,
            err_path,
        }
    }

    fn log(&self) -> String {
        std::fs::read_to_string(&self.err_path).unwrap_or_default()
    }

    fn wait_for(&mut self, what: &str, cond: impl Fn(&Daemon) -> bool) {
        let end = Instant::now() + WAIT;
        while !cond(self) {
            if Instant::now() > end {
                panic!("timed out waiting for {what}:\n{}", self.log());
            }
            if let Ok(Some(status)) = self.child.try_wait() {
                panic!(
                    "the daemon exited ({status}) before {what}:\n{}",
                    self.log()
                );
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    /// The handles of the instance children spawned from `template`, in
    /// spawn order.
    fn handles(&self, template: &str) -> Vec<String> {
        events(&self.log(), "instance.spawn")
            .iter()
            .filter(|e| e["template"] == template)
            .filter_map(|e| e["handle"].as_str().map(str::to_string))
            .collect()
    }

    fn child_dir(&self, handle: &str) -> String {
        format!("{}/state/subagents/file/agentd/parent/{handle}", self.dir)
    }

    /// The child's own log.
    fn child_log(&self, handle: &str) -> String {
        std::fs::read_to_string(format!("{}/log", self.child_dir(handle))).unwrap_or_default()
    }

    /// Whether the child `handle` finished its own workflow `workflow`.
    fn child_ran(&self, handle: &str, workflow: &str) -> bool {
        events(&self.child_log(handle), "run.done")
            .iter()
            .any(|e| e["workflow"] == workflow && e["status"] == "completed")
    }

    #[cfg(feature = "hot-reload")]
    fn sighup(&self) {
        unsafe { libc::kill(self.child.id() as i32, libc::SIGHUP) };
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// At load the team is composed, said once per agent; the document's own
/// `once` workflow spawns both agents as instance children and reaches
/// `alpha` by its name; and each child runs its own `once` workflow in its
/// own daemon.
#[test]
fn a_documents_agents_are_composed_spawned_and_run_their_own_workflows() {
    let doc = document(&[
        agent("alpha", "You are alpha."),
        agent("beta", "You are beta."),
    ]);
    let mut d = Daemon::boot(&doc, "idle");
    d.wait_for("the parent to exit", |d| {
        !events(&d.log(), "proc.exit").is_empty()
    });
    let log = d.log();
    let composed = events(&log, "subagent.template.composed");
    let mut names: Vec<&str> = composed
        .iter()
        .filter_map(|e| e["template"].as_str())
        .collect();
    names.sort_unstable();
    assert_eq!(names, ["alpha", "beta"], "{log}");
    for e in &composed {
        assert_eq!(e["origin"], "document", "{e}");
        assert_eq!(e["tier"], "instance", "{e}");
        assert_eq!(e["singleton"], true, "{e}");
        assert_eq!(e["workflows"].as_array().map(Vec::len), Some(2), "{e}");
    }
    let open: Vec<Value> = events(&log, "run.done")
        .into_iter()
        .filter(|e| e["workflow"] == "open")
        .collect();
    assert_eq!(open.len(), 1, "{log}");
    assert_eq!(open[0]["status"], "completed", "{log}");
    assert!(
        open[0]["output"]
            .as_str()
            .is_some_and(|o| o.contains("pong from alpha")),
        "the parent reached alpha by its name: {}\n{log}",
        open[0]
    );
    for (template, workflow) in [("alpha", "alpha-up"), ("beta", "beta-up")] {
        let handles = d.handles(template);
        assert_eq!(handles.len(), 1, "{template}:\n{log}");
        assert!(
            d.child_ran(&handles[0], workflow),
            "{template} ran its own {workflow}:\n{}",
            d.child_log(&handles[0])
        );
    }
    // Nothing inside an agent became the parent's.
    assert!(
        !events(&log, "workflow.loaded")
            .iter()
            .any(|e| e["name"] == "alpha-up" || e["name"] == "beta-up"),
        "{log}"
    );
}

/// A reload that edits `alpha` and removes `beta`: the removed agent's live
/// child is retired; the edited one's is retired and started again from the
/// new composition, and runs its own workflow there. The document's `open`
/// workflow did not change, so it does not run again.
#[cfg(feature = "hot-reload")]
#[test]
fn a_reload_recomposes_a_changed_agent_and_retires_a_removed_one() {
    let mut d = Daemon::boot(
        &document(&[
            agent("alpha", "You are alpha."),
            agent("beta", "You are beta."),
        ]),
        "drained",
    );
    d.wait_for("both children to run their own workflows", |d| {
        let (a, b) = (d.handles("alpha"), d.handles("beta"));
        a.len() == 1
            && b.len() == 1
            && d.child_ran(&a[0], "alpha-up")
            && d.child_ran(&b[0], "beta-up")
    });
    let (alpha, beta) = (d.handles("alpha")[0].clone(), d.handles("beta")[0].clone());
    std::fs::write(
        format!("{}/team.md", d.dir),
        document(&[agent("alpha", "You are alpha, version two.")]),
    )
    .unwrap();
    d.sighup();
    d.wait_for("the new alpha to run its own workflow", |d| {
        let a = d.handles("alpha");
        a.len() == 2 && d.child_ran(&a[1], "alpha-up")
    });
    let log = d.log();
    let said = |name: &str, template: &str| {
        events(&log, name)
            .into_iter()
            .find(|e| e["template"] == template)
            .unwrap_or_else(|| panic!("no {name} for {template}:\n{log}"))
    };
    assert_eq!(
        said("subagent.template.retired", "beta")["handle"],
        beta.as_str()
    );
    assert_eq!(
        said("subagent.template.recomposed", "alpha")["handle"],
        alpha.as_str()
    );
    let respawned = said("subagent.template.respawned", "alpha");
    assert_eq!(respawned["was"], alpha.as_str());
    let new_alpha = d.handles("alpha")[1].clone();
    assert_eq!(respawned["handle"], new_alpha.as_str());
    // The new composition is the edited agent's.
    let config: Value = serde_json::from_str(
        &std::fs::read_to_string(format!("{}/config.json", d.child_dir(&new_alpha))).unwrap(),
    )
    .unwrap();
    assert!(
        config["agent"]["instruction"]
            .as_str()
            .is_some_and(|i| i.contains("version two")),
        "{config}"
    );
    // Both old children drained and exited as retired.
    d.wait_for("the old children to exit", |d| {
        let exited = events(&d.log(), "instance.exited");
        [&alpha, &beta].iter().all(|h| {
            exited
                .iter()
                .any(|e| e["handle"] == h.as_str() && e["status"] == "retired")
        })
    });
    // `alpha` composed again, `beta` gone; `open` ran once.
    let composed: Vec<Value> = events(&d.log(), "subagent.template.composed");
    assert_eq!(
        composed.iter().filter(|e| e["template"] == "alpha").count(),
        2,
        "{}",
        d.log()
    );
    assert_eq!(
        events(&d.log(), "run.start")
            .iter()
            .filter(|e| e["workflow"] == "open")
            .count(),
        1,
        "{}",
        d.log()
    );
    assert!(d.handles("beta").len() == 1, "{}", d.log());
}

/// Over the wire (`mcp:`), a document's agents run only when the publisher
/// the operator pinned signed it attesting `agents`. The mock registry serves
/// the same team three ways: with no pin — the agents are held, said, and
/// nothing spawns; pinned and signed attesting another family — held too;
/// and pinned and signed attesting `agents` — the team is composed when the
/// first read folds, and the document's `open` workflow spawns it.
#[cfg(feature = "sign")]
#[test]
fn a_served_documents_agents_need_a_pinned_publisher_that_attests_them() {
    let key = agentd::aauth::AgentKey::from_seed(&agentd::mcp::mock_http::MOCK_SIGN_SEED)
        .expect("test seed");
    let key_file = common::unique_path("doc-agents-pub", "key");
    std::fs::write(&key_file, key.public_bytes()).unwrap();
    let doc = document(&[
        agent("alpha", "You are alpha."),
        agent("beta", "You are beta."),
    ]);
    for (pinned, attests, held) in [
        (false, None, Some("not verified")),
        (true, Some("compute"), Some("attests [\"compute\"]")),
        (true, Some("agents"), None),
    ] {
        let registry = common::spawn_mock_mcp("mock://unused", false);
        let mut args = serde_json::json!({"text": doc});
        if let Some(c) = attests {
            args["capabilities"] = serde_json::json!([c]);
        }
        registry.control("mock.serve_document", args);
        let dir = common::unique_path("doc-agents-mcp", "d");
        std::fs::create_dir_all(&dir).unwrap();
        let uri = "instruction://ins_live@stable";
        let mut instruction = serde_json::json!({"mcp": {"server": "registry", "resource": uri}});
        if pinned {
            instruction["trust"] = serde_json::json!([{"uri": uri,
                "publisher": "https://instruction.md/pub/mock", "author_keys": [key_file]}]);
        }
        let cfg = serde_json::json!({
            "agent": {"name": "parent", "preflight": "never",
                      "instruction": instruction,
                      "document_capabilities": ["agents"]},
            "mcp": {"servers": [{"name": "registry", "endpoint": format!("{}/mcp", registry.uri())}]},
            "intelligence": {"endpoints": "mock:final", "model": "mock"},
            "store": {"kind": "file", "file": {"path": format!("{dir}/state")}},
            "lifecycle": {"run_until": "drained"},
            "observability": {"log_level": "info", "log_content": true},
            // Keeps the daemon up before the document's machinery joins.
            "workflows": [{"name": "tick", "steps": {
                "every": {"kind": "schedule", "every": "1h"},
                "done": {"kind": "finish", "depends_on": ["every"], "status": "completed"}
            }}],
        });
        std::fs::write(format!("{dir}/c.json"), cfg.to_string()).unwrap();
        let err_path = format!("{dir}/parent.log");
        let child = Command::new(env!("CARGO_BIN_EXE_agentd"))
            .args(["-c", &format!("{dir}/c.json")])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(std::fs::File::create(&err_path).unwrap())
            .spawn()
            .unwrap();
        let mut d = Daemon {
            child,
            dir,
            err_path,
        };
        d.wait_for("the first read to fold", |d| {
            !events(&d.log(), "instruction.machinery.applied").is_empty()
        });
        if held.is_none() {
            d.wait_for("the team to spawn", |d| {
                d.handles("alpha").len() == 1 && d.handles("beta").len() == 1
            });
            let log = d.log();
            let verified = events(&log, "instruction.verified");
            assert_eq!(
                verified.first().map(|v| v["capabilities"].clone()),
                Some(serde_json::json!(["agents"])),
                "{log}"
            );
            assert_eq!(events(&log, "subagent.template.composed").len(), 2, "{log}");
            let applied = &events(&log, "instruction.machinery.applied")[0];
            assert_eq!(
                applied["agents_added"],
                serde_json::json!(["alpha", "beta"]),
                "{applied}"
            );
        } else {
            // The `open` workflow runs, and its spawns find no template.
            d.wait_for("the open run to end", |d| {
                events(&d.log(), "run.done")
                    .iter()
                    .any(|e| e["workflow"] == "open")
            });
            let log = d.log();
            assert!(events(&log, "instance.spawn").is_empty(), "{log}");
            assert!(
                events(&log, "subagent.template.composed").is_empty(),
                "{log}"
            );
            let why = held.unwrap_or_default();
            assert!(
                events(&log, "config.warning").iter().any(|w| {
                    let w = w["warning"].as_str().unwrap_or_default();
                    w.contains("\"alpha\"") && w.contains(why)
                }),
                "{why}: {log}"
            );
        }
    }
    let _ = std::fs::remove_file(&key_file);
}
