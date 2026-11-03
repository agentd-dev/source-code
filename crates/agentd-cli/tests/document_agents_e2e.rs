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
        Daemon::boot_with(doc, run_until, "")
    }

    /// [`Daemon::boot`] with `extra` appended to the operator's configuration.
    fn boot_with(doc: &str, run_until: &str, extra: &str) -> Daemon {
        let dir = common::unique_path("doc-agents", "d");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(format!("{dir}/team.md"), doc).unwrap();
        let cfg = format!(
            "agent: {{ name: parent, instruction: {dir}/team.md, document_capabilities: [agents] }}\n\
             store: {{ kind: file, file: {{ path: {dir}/state }} }}\n\
             intelligence: {{ endpoints: \"mock:final\", model: mock }}\n\
             lifecycle: {{ run_until: {run_until}, idle_grace: 1500ms }}\n\
             observability: {{ log_level: info, log_content: true }}\n{extra}"
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
        assert_eq!(e["workflows"], 2, "{e}");
        assert_eq!(e["workflow_names"].as_array().map(Vec::len), Some(2), "{e}");
        assert!(e["reports_to"].is_null(), "{e}");
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

/// An org chart: `alpha` with `beta` nested in it, a report. Each agent's
/// own `once` workflow, no team workflow at all.
#[cfg(feature = "hot-reload")]
fn org(reports: &[&str]) -> String {
    let nested: String = reports
        .iter()
        .map(|id| {
            format!(
                "\n::::agent{{id={id} title=\"{id}\"}}\nYou are {id}.\n\n\
                 :::workflow{{id={id}-up dialect=\"instruction/workflow\"}}\n\
                 name: {id}-up\nsteps: {{ s: {{ kind: once }}, f: {{ kind: finish, depends_on: [s], status: completed }} }}\n:::\n\
                 ::::\n"
            )
        })
        .collect();
    format!(
        "---\nspec: \"1\"\n---\n# The org\n\n\
         :::::agent{{id=alpha title=\"Alpha lead\"}}\nYou lead alpha.\n\n\
         ::::workflow{{id=alpha-up dialect=\"instruction/workflow\"}}\n\
         name: alpha-up\nsteps: {{ s: {{ kind: once }}, f: {{ kind: finish, depends_on: [s], status: completed }} }}\n::::\n\
         {nested}:::::\n\n\
         :::workflow{{id=also dialect=\"instruction/workflow\"}}\n\
         name: also\nsteps: {{ s: {{ kind: once }}, run: {{ kind: subagent, depends_on: [s], template: beta }}, \
         ls: {{ kind: tool, depends_on: [run], name: subagent.list, args: {{}} }}, \
         st: {{ kind: tool, depends_on: [ls], name: status, args: {{}} }}, \
         f: {{ kind: finish, depends_on: [st], status: completed, \
         output: \"{{{{steps.run.output.handle}}}} {{{{steps.ls.output.subagents}}}} {{{{steps.st.output.subagents}}}}\" }} }}\n:::\n"
    )
}

/// Under `subagents.document_agents.start: on_compose`, a document's agents
/// start with no workflow: a manager and its report, each labelled with its
/// reporting line. A reload that adds a second report starts it alone — the
/// manager's text has the new report cut out, so its composition did not
/// change and nothing else restarts.
#[cfg(feature = "hot-reload")]
#[test]
fn on_compose_starts_an_org_chart_and_a_report_a_reload_adds() {
    let keepalive = "subagents: { document_agents: { start: on_compose } }\n\
        limits: { subagents: { instances: { breadth: 4, total: 8 } } }\n\
        workflows:\n  - name: tick\n    steps:\n      every: { kind: schedule, every: 1h }\n      \
        done: { kind: finish, depends_on: [every], status: completed }\n";
    let mut d = Daemon::boot_with(&org(&["beta"]), "drained", keepalive);
    d.wait_for("alpha and beta to start and run their own workflows", |d| {
        let (a, b) = (d.handles("alpha"), d.handles("beta"));
        a.len() == 1
            && b.len() == 1
            && d.child_ran(&a[0], "alpha-up")
            && d.child_ran(&b[0], "beta-up")
    });
    let log = d.log();
    let composed = |log: &str, t: &str| {
        events(log, "subagent.template.composed")
            .into_iter()
            .filter(|e| e["template"] == t)
            .collect::<Vec<_>>()
    };
    let beta = &composed(&log, "beta")[0];
    assert_eq!(beta["reports_to"], "alpha", "{beta}");
    assert_eq!(beta["title"], "beta", "{beta}");
    assert_eq!(beta["workflows"], 1, "{beta}");
    assert_eq!(beta["servers"], 0, "{beta}");
    assert!(composed(&log, "alpha")[0]["reports_to"].is_null(), "{log}");
    let spawn = events(&log, "instance.spawn")
        .into_iter()
        .find(|e| e["template"] == "beta")
        .unwrap();
    assert_eq!(spawn["reports_to"], "alpha", "{spawn}");
    let child_log = spawn["log"].as_str().unwrap();
    assert!(std::path::Path::new(child_log).is_absolute(), "{spawn}");
    assert!(
        std::fs::read_to_string(child_log)
            .unwrap()
            .contains("beta-up"),
        "{spawn}"
    );
    // The manager's instruction is its own: the report's text is not in it.
    let alpha = d.handles("alpha")[0].clone();
    let config: Value = serde_json::from_str(
        &std::fs::read_to_string(format!("{}/config.json", d.child_dir(&alpha))).unwrap(),
    )
    .unwrap();
    let text = config["agent"]["instruction"].as_str().unwrap();
    assert!(
        text.contains("You lead alpha.") && !text.contains("You are beta."),
        "{text}"
    );
    assert_eq!(events(&log, "subagent.template.started").len(), 2, "{log}");
    // The document's own workflow spawning `beta` too is answered with the
    // live one: said, no second child, no failed step.
    d.wait_for("the also run to end", |d| {
        events(&d.log(), "run.done")
            .iter()
            .any(|e| e["workflow"] == "also")
    });
    let log = d.log();
    let also = events(&log, "run.done")
        .into_iter()
        .find(|e| e["workflow"] == "also")
        .unwrap();
    assert_eq!(also["status"], "completed", "{also}");
    let out = also["output"].as_str().unwrap();
    assert!(
        out.starts_with(&format!("{} ", d.handles("beta")[0])),
        "{also}"
    );
    // Both listers name each child's reporting line: `subagent.list`, and
    // the status document a client adopts on connect or resync.
    assert_eq!(out.matches(r#""reports_to":"alpha""#).count(), 2, "{also}");
    assert_eq!(
        events(&log, "subagent.template.already_live").len(),
        1,
        "{log}"
    );

    std::fs::write(format!("{}/team.md", d.dir), org(&["beta", "gamma"])).unwrap();
    d.sighup();
    d.wait_for("gamma to start and run its own workflow", |d| {
        let g = d.handles("gamma");
        g.len() == 1 && d.child_ran(&g[0], "gamma-up")
    });
    let log = d.log();
    assert_eq!(composed(&log, "gamma")[0]["reports_to"], "alpha", "{log}");
    // Nothing else moved: alpha and beta were neither recomposed nor started
    // again.
    assert!(
        events(&log, "subagent.template.recomposed").is_empty(),
        "{log}"
    );
    assert_eq!(composed(&log, "alpha").len(), 1, "{log}");
    assert_eq!(d.handles("alpha").len(), 1, "{log}");
    assert_eq!(d.handles("beta").len(), 1, "{log}");
}

/// Three agents, each written under the document's `tone` parameter, so a
/// new `tone` recomposes all three. `beta` has a skill named like an
/// instruction.md id.
#[cfg(feature = "hot-reload")]
fn team_in(tone: &str, beta_flat: bool) -> String {
    let wf = |id: &str| {
        format!(
            ":::workflow{{id={id}-up dialect=\"instruction/workflow\"}}\nname: {id}-up\n\
             steps: {{ s: {{ kind: once }}, f: {{ kind: finish, depends_on: [s], status: completed }} }}\n:::\n"
        )
    };
    let beta = if beta_flat {
        "::::agent{id=beta}\nYou are beta, in a ${tone} voice.\n::::\n".to_string()
    } else {
        format!(
            "::::agent{{id=beta}}\nYou are beta, in a ${{tone}} voice.\n\n\
             :::skill{{id=web.search title=\"Web search\"}}\nSearch the web.\n:::\n\n{}::::\n",
            wf("beta")
        )
    };
    format!(
        "---\nspec: \"1\"\nparameters:\n  tone: {{ type: string, default: {tone} }}\n---\n# Team\n\n\
         ::::agent{{id=alpha}}\nYou are alpha, in a ${{tone}} voice.\n\n{}::::\n\n{beta}\n\
         ::::agent{{id=gamma}}\nYou are gamma, in a ${{tone}} voice.\n\n{}::::\n",
        wf("alpha"),
        wf("gamma")
    )
}

/// The instance ceilings left at their defaults — total 8 and rate `4/1h` —
/// with breadth raised to the three agents: two reloads that recompose all
/// three respawn six times, and every one is live afterwards. A respawn
/// replaces its predecessor; the ceilings bound new agents.
#[cfg(feature = "hot-reload")]
#[test]
fn recomposes_past_the_default_total_and_rate_keep_the_team_live() {
    let extra = "subagents: { document_agents: { start: on_compose } }\n\
        limits: { subagents: { instances: { breadth: 3 } } }\n\
        workflows:\n  - name: tick\n    steps:\n      every: { kind: schedule, every: 1h }\n      \
        done: { kind: finish, depends_on: [every], status: completed }\n";
    let mut d = Daemon::boot_with(&team_in("warm", false), "drained", extra);
    d.wait_for("the team to start", |d| {
        ["alpha", "beta", "gamma"]
            .iter()
            .all(|t| d.handles(t).len() == 1)
    });
    // `beta`'s skill reached its child under its own name.
    let beta = d.handles("beta")[0].clone();
    d.wait_for("beta's skills", |d| {
        d.child_log(&beta).contains("web.search")
    });
    for (round, tone) in [(1, "plain"), (2, "technical")] {
        std::fs::write(format!("{}/team.md", d.dir), team_in(tone, false)).unwrap();
        d.sighup();
        d.wait_for("all three to be respawned", |d| {
            events(&d.log(), "subagent.template.respawned").len() == 3 * round
        });
    }
    let log = d.log();
    assert!(
        events(&log, "subagent.template.respawn_failed").is_empty(),
        "{log}"
    );
    assert!(
        events(&log, "subagent.template.recompose_refused").is_empty(),
        "{log}"
    );
    // The last child of each is the one running, and runs its own workflow.
    for t in ["alpha", "beta", "gamma"] {
        let hs = d.handles(t);
        assert_eq!(hs.len(), 3, "{t}: {log}");
        let last = hs.last().unwrap().clone();
        d.wait_for("the last child's own workflow", |d| {
            d.child_ran(&last, &format!("{t}-up"))
        });
        let exited = events(&d.log(), "instance.exited");
        assert!(
            !exited.iter().any(|e| e["handle"] == last.as_str()),
            "{t} {last} is live: {log}"
        );
    }
}

/// A singleton whose agent becomes prose-only — a flat worker, no longer an
/// instance child — is retired and not started again (it was respawned as an
/// instance, and a debug build panicked).
#[cfg(feature = "hot-reload")]
#[test]
fn an_agent_that_turns_flat_is_retired_not_respawned() {
    let extra = "subagents: { document_agents: { start: on_compose } }\n\
        limits: { subagents: { instances: { breadth: 3 } } }\n\
        workflows:\n  - name: tick\n    steps:\n      every: { kind: schedule, every: 1h }\n      \
        done: { kind: finish, depends_on: [every], status: completed }\n";
    let mut d = Daemon::boot_with(&team_in("warm", false), "drained", extra);
    d.wait_for("the team to start", |d| {
        ["alpha", "beta", "gamma"]
            .iter()
            .all(|t| d.handles(t).len() == 1)
    });
    let beta = d.handles("beta")[0].clone();
    std::fs::write(format!("{}/team.md", d.dir), team_in("warm", true)).unwrap();
    d.sighup();
    d.wait_for("beta to retire", |d| {
        events(&d.log(), "instance.exited")
            .iter()
            .any(|e| e["handle"] == beta.as_str() && e["status"] == "retired")
    });
    std::thread::sleep(Duration::from_millis(500));
    let log = d.log();
    let retired = events(&log, "subagent.template.retired");
    assert!(
        retired.iter().any(|e| e["template"] == "beta"
            && e["why"]
                .as_str()
                .is_some_and(|w| w.contains("no longer an instance"))),
        "{log}"
    );
    assert!(
        events(&log, "subagent.template.respawned").is_empty(),
        "{log}"
    );
    assert_eq!(d.handles("beta").len(), 1, "{log}");
    assert!(
        d.child.try_wait().unwrap().is_none(),
        "the parent still runs: {log}"
    );
}

/// The keep-alive every on-compose test runs under: a workflow on a long
/// schedule, so a `drained` daemon has standing work.
#[cfg(feature = "hot-reload")]
const KEEPALIVE: &str = "workflows:\n  - name: tick\n    steps:\n      every: { kind: schedule, every: 1h }\n      \
    done: { kind: finish, depends_on: [every], status: completed }\n";

/// One agent, `alpha`, written as `prose`; `broken` gives its own workflow
/// a step that calls a tool the child does not have.
#[cfg(feature = "hot-reload")]
fn solo(prose: &str, broken: bool) -> String {
    let bad = if broken {
        "  bad: { kind: tool, depends_on: [s], name: no.such.tool, args: {} }\n"
    } else {
        ""
    };
    format!(
        "---\nspec: \"1\"\n---\n# Solo\n\n\
         ::::agent{{id=alpha}}\n{prose}\n\n\
         :::workflow{{id=alpha-up dialect=\"instruction/workflow\"}}\n\
         name: alpha-up\nsteps:\n  s: {{ kind: once }}\n  \
         f: {{ kind: finish, depends_on: [s], status: completed }}\n{bad}:::\n::::\n"
    )
}

/// A reload whose new composition of a live singleton cannot start — its
/// workflow calls a tool the child does not have — is refused before
/// the live child is retired: the child keeps running, the same handle,
/// and the refusal says why. (The child was retired first, and the
/// respawn then failed, taking the agent down.)
#[cfg(feature = "hot-reload")]
#[test]
fn a_recompose_that_cannot_start_keeps_the_running_child() {
    let extra = format!("subagents: {{ document_agents: {{ start: on_compose }} }}\n{KEEPALIVE}");
    let mut d = Daemon::boot_with(&solo("You are alpha.", false), "drained", &extra);
    d.wait_for("alpha to start and run its own workflow", |d| {
        let a = d.handles("alpha");
        a.len() == 1 && d.child_ran(&a[0], "alpha-up")
    });
    let alpha = d.handles("alpha")[0].clone();
    std::fs::write(
        format!("{}/team.md", d.dir),
        solo("You are alpha, version two.", true),
    )
    .unwrap();
    d.sighup();
    d.wait_for("the recompose to be refused", |d| {
        !events(&d.log(), "subagent.template.recompose_refused").is_empty()
            || !events(&d.log(), "subagent.template.recomposed").is_empty()
    });
    std::thread::sleep(Duration::from_millis(1500));
    let log = d.log();
    let refused = events(&log, "subagent.template.recompose_refused");
    assert_eq!(refused.len(), 1, "{log}");
    assert_eq!(refused[0]["template"], "alpha", "{log}");
    assert_eq!(refused[0]["handle"], alpha.as_str(), "{log}");
    assert!(
        refused[0]["why"]
            .as_str()
            .is_some_and(|w| w.contains("no.such.tool")),
        "{log}"
    );
    for gone in [
        "subagent.template.recomposed",
        "subagent.template.respawn_failed",
        "subagent.template.retired",
    ] {
        assert!(events(&log, gone).is_empty(), "{gone}: {log}");
    }
    assert_eq!(d.handles("alpha"), vec![alpha.clone()], "{log}");
    assert!(
        !events(&log, "instance.exited")
            .iter()
            .any(|e| e["handle"] == alpha.as_str()),
        "the running alpha was retired: {log}"
    );
}

/// Turning `start: on_compose` on by a reload starts the agents the
/// document had already composed, under the instance ceilings: with breadth
/// for two, two of three start and the third's refusal is said.
#[cfg(feature = "hot-reload")]
#[test]
fn turning_on_compose_on_by_reload_starts_the_composed_agents() {
    let extra = format!("limits: {{ subagents: {{ instances: {{ breadth: 2 }} }} }}\n{KEEPALIVE}");
    let mut d = Daemon::boot_with(&team_in("warm", false), "drained", &extra);
    d.wait_for("the team to be composed", |d| {
        events(&d.log(), "subagent.template.composed").len() == 3
    });
    std::thread::sleep(Duration::from_millis(500));
    assert!(events(&d.log(), "instance.spawn").is_empty(), "{}", d.log());
    let cfg = format!("{}/c.yaml", d.dir);
    let mut text = std::fs::read_to_string(&cfg).unwrap();
    text.push_str("subagents: { document_agents: { start: on_compose } }\n");
    std::fs::write(&cfg, text).unwrap();
    d.sighup();
    d.wait_for("two to start and one to be refused", |d| {
        let log = d.log();
        events(&log, "subagent.template.started").len() == 2
            && events(&log, "subagent.template.start_refused").len() == 1
    });
    let log = d.log();
    let refused = &events(&log, "subagent.template.start_refused")[0];
    assert!(
        refused["error"]
            .as_str()
            .is_some_and(|e| e.contains("breadth")),
        "{refused}"
    );
    let started: Vec<String> = ["alpha", "beta", "gamma"]
        .iter()
        .filter(|t| d.handles(t).len() == 1)
        .map(|t| t.to_string())
        .collect();
    assert_eq!(started.len(), 2, "{log}");
    for t in &started {
        let h = d.handles(t)[0].clone();
        d.wait_for("each started agent's own workflow", |d| {
            d.child_ran(&h, &format!("{t}-up"))
        });
    }
}
