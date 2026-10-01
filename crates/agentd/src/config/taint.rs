// SPDX-License-Identifier: AGPL-3.0-only
//! **Stream taint**: which declared streams carry input from outside the trust
//! boundary, and the lethal-trifecta check over every workflow whose runs read
//! one (RFC 0045 §5.11.3).
//!
//! The root-grant fold and the `subagent.run` gate judge SERVERS. Neither sees
//! input that reaches a run through a stream: a webhook `into:` appends what an
//! outside caller posted, and the workflow consuming that stream hands it to an
//! agent whose servers the root fold judged as if no outside text would ever
//! reach them. A config whose every server is tagged passes the root fold with
//! `sensitive` + `egress` and nothing untrusted — and then feeds a webhook body
//! to the agent holding both. So every stream gets a tag set derived from every
//! producer the configuration declares, a run started from or waiting on a
//! tainted stream carries it, and each such workflow goes through the same
//! [`check_trifecta`] as the root grant — at load, where the operator is still
//! looking, not when the first event arrives.
//!
//! Coarse and static on purpose: per stream, not per value, and per server, as
//! the root fold is. It is a grant-level check, not data-flow tracking; the
//! reader/actor split (`docs/security.md`, "The injection firewall") stays the
//! way to handle untrusted content.

use crate::config::settings::{McpServer, Settings};
use crate::engine::model::{Step, Workflow};
use crate::sec::scope::{TrifectaTag, TrifectaVerdict, check_trifecta};
use serde_json::{Map, Value};
use std::collections::{BTreeMap, BTreeSet};

/// The refusals for `workflows` under `s`: one per workflow whose runs read a
/// tainted stream (or are started by a run that does) while its model-driven
/// steps reach `sensitive` and `egress` — unless `security.allow_trifecta` is
/// set, which lifts this check with every other trifecta gate.
///
/// `workflows` is the whole set the configuration would run. The analysis is
/// monotone in it — another definition can only add producers, consumers and
/// edges — so a refusal over part of the set (the inline definitions
/// `validate` can see) is also a refusal over all of it.
pub fn refusals(s: &Settings, workflows: &[&Workflow]) -> Vec<String> {
    let ctx = Reach::new(s);
    let (streams, runs) = propagate(&ctx, workflows);
    let mut out = Vec::new();
    for w in workflows {
        let Some(run) = runs.get(&w.name).filter(|r| !r.tags.is_empty()) else {
            continue;
        };
        let held = ctx.workflow(w);
        let tags = run
            .tags
            .iter()
            .chain(held.iter().flat_map(|h| h.tags.iter()))
            .copied();
        if check_trifecta(tags, s.security.allow_trifecta) != TrifectaVerdict::RefusedTrifecta {
            continue;
        }
        // Name only what brought the other two legs: listing every server the
        // steps reach would bury the two the operator has to move.
        let legs: Vec<String> = held
            .iter()
            .filter(|h| {
                h.tags
                    .iter()
                    .any(|t| matches!(t, TrifectaTag::Sensitive | TrifectaTag::Egress))
            })
            .map(|h| format!("{} [{}]", h.label, tag_names(&h.tags)))
            .collect();
        out.push(format!(
            "workflow {:?}: lethal-trifecta refused — {}, and its model-driven steps reach {}: \
             untrusted input + sensitive + egress in one run. Split reading from acting (a \
             reader step with no sensitive or egress server, docs/security.md), narrow the \
             steps' `servers` and `tools`, or set security.allow_trifecta (audited)",
            w.name,
            run.describe(&streams),
            legs.join(", ")
        ));
    }
    out
}

/// What feeds one stream.
#[derive(Debug, Default)]
struct StreamTaint {
    tags: Vec<TrifectaTag>,
    /// Each producer, as the refusal names it.
    producers: BTreeSet<String>,
}

/// Why one workflow's runs carry taint. Only the immediate edges are kept —
/// the stream it reads, the step that runs it — so a cycle (a workflow that
/// emits into the stream it consumes) settles instead of growing a chain.
#[derive(Debug, Default)]
struct RunTaint {
    tags: Vec<TrifectaTag>,
    streams: BTreeSet<String>,
    callers: BTreeSet<String>,
}

impl RunTaint {
    fn describe(&self, streams: &BTreeMap<String, StreamTaint>) -> String {
        let mut why: Vec<String> = self
            .streams
            .iter()
            .map(|name| {
                let fed = streams
                    .get(name)
                    .map(|t| t.producers.iter().cloned().collect::<Vec<_>>().join(", "))
                    .unwrap_or_default();
                format!("it consumes stream {name:?} (fed by {fed})")
            })
            .collect();
        why.extend(self.callers.iter().map(|c| format!("it is run by {c}")));
        why.join("; ")
    }
}

/// Union `from` into `into`; whether anything was new.
fn union(into: &mut Vec<TrifectaTag>, from: &[TrifectaTag]) -> bool {
    let mut grew = false;
    for t in from {
        if !into.contains(t) {
            into.push(*t);
            grew = true;
        }
    }
    grew
}

fn tag_names(tags: &[TrifectaTag]) -> String {
    tags.iter()
        .map(|t| match t {
            TrifectaTag::UntrustedInput => "untrusted_input",
            TrifectaTag::Sensitive => "sensitive",
            TrifectaTag::Egress => "egress",
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// Every step of `w`, nested bodies and branches included: an agent step
/// inside a `foreach` is handed the same run's input as one at the top.
fn all_steps(w: &Workflow) -> Vec<&Step> {
    fn walk<'a>(steps: impl Iterator<Item = &'a Step>, out: &mut Vec<&'a Step>) {
        for s in steps {
            out.push(s);
            for body in s.body.iter().chain(s.branches.values()) {
                walk(body.steps.values(), out);
            }
        }
    }
    let mut out = Vec::new();
    walk(w.steps.values(), &mut out);
    out
}

/// The stream a step reads events from: a `stream` or `correlate` start, or a
/// `wait {on: event}`.
fn consumes(st: &Step) -> Option<&str> {
    match st.kind.as_str() {
        "stream" | "correlate" => st.field_str("stream"),
        "wait" if st.field_str("on") == Some("event") => st.field_str("stream"),
        _ => None,
    }
}

/// Fold every declared producer into each stream's taint, and each tainted
/// stream into the runs that read it, until nothing moves. A fixed point,
/// because an `emit` target and a `workflow` step's target are static: a run
/// reading a tainted stream taints what it emits into, and the workflows it
/// starts.
fn propagate(
    ctx: &Reach,
    workflows: &[&Workflow],
) -> (BTreeMap<String, StreamTaint>, BTreeMap<String, RunTaint>) {
    let untrusted = [TrifectaTag::UntrustedInput];
    let mut streams: BTreeMap<String, StreamTaint> = BTreeMap::new();
    let mut feed = |stream: &str, tags: &[TrifectaTag], producer: String| {
        let t = streams.entry(stream.to_string()).or_default();
        union(&mut t.tags, tags);
        t.producers.insert(producer);
    };
    // The edges that append what an outside party sent. Both are untrusted by
    // construction: a webhook body is whatever its caller posted (a valid
    // signature says who sent it, not that its text is safe to obey), and an
    // A2A peer's message is another agent's output.
    for w in workflows {
        for st in w.start_steps() {
            let Some(stream) = st
                .field("into")
                .and_then(|i| i.get("stream"))
                .and_then(Value::as_str)
            else {
                continue;
            };
            let edge = match st.kind.as_str() {
                "webhook" => "webhook `into:`",
                "a2a" => "A2A `into:`",
                _ => continue,
            };
            feed(
                stream,
                &untrusted,
                format!("{edge} at workflow {:?} step {:?}", w.name, st.id),
            );
        }
    }
    // A mirrored stream carries whatever the child's own producers put on it.
    // Those are compiled from the template at spawn, into another process's
    // configuration — they cannot be derived here, so the mirror counts as
    // untrusted, as the RFC states for a taint that cannot be computed.
    for (name, t) in &ctx.s.subagents.templates {
        for m in t.mirror_streams.iter().flatten() {
            feed(
                m,
                &untrusted,
                format!("mirror_streams of subagent template {name:?}"),
            );
        }
    }
    // The workflow tools a model-driven step could call: a run started that
    // way takes its inputs from a model that read the caller's input.
    let tools: Vec<(&str, &str)> = workflows
        .iter()
        .filter_map(|w| {
            w.tool
                .as_ref()
                .filter(|t| t.grant.workflows)
                .map(|t| (t.name.as_str(), w.name.as_str()))
        })
        .collect();
    let mut runs: BTreeMap<String, RunTaint> = BTreeMap::new();
    loop {
        let mut grew = false;
        for w in workflows {
            for st in all_steps(w) {
                if let Some(name) = consumes(st)
                    && let Some(t) = streams.get(name).filter(|t| !t.tags.is_empty())
                {
                    let run = runs.entry(w.name.clone()).or_default();
                    grew |= union(&mut run.tags, &t.tags);
                    grew |= run.streams.insert(name.to_string());
                }
            }
        }
        for w in workflows {
            let Some(tags) = runs
                .get(&w.name)
                .filter(|r| !r.tags.is_empty())
                .map(|r| r.tags.clone())
            else {
                continue;
            };
            for st in all_steps(w) {
                let at = format!("workflow {:?} step {:?}", w.name, st.id);
                let mut started: Vec<(&str, String)> = Vec::new();
                match st.kind.as_str() {
                    "emit" => {
                        if let Some(stream) = st.field_str("stream") {
                            let t = streams.entry(stream.to_string()).or_default();
                            grew |= union(&mut t.tags, &tags);
                            grew |= t.producers.insert(format!("`emit` at {at}"));
                        }
                    }
                    // A templated name can start any of them.
                    "workflow" => match st.field_str("name") {
                        Some(n) if !n.contains("{{") => started.push((n, at.clone())),
                        _ => {
                            started.extend(workflows.iter().map(|o| (o.name.as_str(), at.clone())))
                        }
                    },
                    "agent" => {
                        let allow = allow_list(&st.spec);
                        for (tool, target) in &tools {
                            if ctx.callable(allow.as_deref(), tool) {
                                started.push((target, format!("{at} (through its tool {tool:?})")));
                            }
                        }
                    }
                    _ => {}
                }
                for (target, caller) in started {
                    let run = runs.entry(target.to_string()).or_default();
                    grew |= union(&mut run.tags, &tags);
                    grew |= run.callers.insert(caller);
                }
            }
        }
        if !grew {
            return (streams, runs);
        }
    }
}

/// A step's `tools` allow-list, when it has one.
fn allow_list(spec: &Map<String, Value>) -> Option<Vec<String>> {
    spec.get("tools").and_then(Value::as_array).map(|a| {
        a.iter()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect()
    })
}

/// The servers among `servers` that `names` lists; an unknown name reaches
/// nothing, as it dials nothing.
fn named(servers: &[(String, Held)], names: &[String]) -> Vec<Held> {
    servers
        .iter()
        .filter(|(n, _)| names.contains(n))
        .map(|(_, h)| h.clone())
        .collect()
}

/// One capability a model-driven step can reach, as a refusal names it.
#[derive(Debug, Clone, PartialEq)]
struct Held {
    label: String,
    tags: Vec<TrifectaTag>,
}

/// What the configuration lets a model-driven step reach, server by server.
struct Reach<'a> {
    s: &'a Settings,
    /// Every configured MCP server, by name, with its declared tags.
    servers: Vec<(String, Held)>,
    /// What a spawn of each subagent template reaches.
    templates: BTreeMap<String, Vec<Held>>,
    /// The `exec` tool is callable: run locally, or mapped off-box.
    exec: bool,
}

impl<'a> Reach<'a> {
    fn new(s: &'a Settings) -> Self {
        let held =
            |srv: &McpServer, label: String| srv.tag_set().ok().map(|tags| Held { label, tags });
        let servers: Vec<(String, Held)> = s
            .mcp
            .servers
            .iter()
            .filter_map(|srv| {
                held(srv, format!("mcp server {:?}", srv.name)).map(|h| (srv.name.clone(), h))
            })
            .collect();
        // A template that does not compile is refused by `validate` on its
        // own; here it is taken to reach every server, so a broken template
        // never narrows the answer.
        let compiled = crate::config::templates::compile_templates(s).unwrap_or_default();
        let mut templates = BTreeMap::new();
        for (name, t) in &s.subagents.templates {
            let reach = match compiled.get(name) {
                // An instance child dials the servers its own machinery
                // declares, resolved against the parent's catalog — the same
                // resolution `compile_templates` checks them under.
                Some(c) if c.tier == crate::config::templates::Tier::Instance => c
                    .fragment
                    .pointer("/mcp/servers")
                    .and_then(|v| serde_json::from_value::<Vec<McpServer>>(v.clone()).ok())
                    .map(|list| {
                        let mut probe = Settings {
                            services: s.services.clone(),
                            ..Default::default()
                        };
                        probe.mcp.servers = list;
                        let _ = crate::config::settings::resolve_services(&mut probe);
                        probe
                            .mcp
                            .servers
                            .iter()
                            .filter_map(|srv| {
                                held(
                                    srv,
                                    format!("mcp server {:?} of template {name:?}", srv.name),
                                )
                            })
                            .collect()
                    })
                    .unwrap_or_default(),
                Some(_) => match &t.servers {
                    Some(names) => named(&servers, names),
                    None => servers.iter().map(|(_, h)| h.clone()).collect(),
                },
                None => servers.iter().map(|(_, h)| h.clone()).collect(),
            };
            templates.insert(name.clone(), reach);
        }
        Reach {
            s,
            exec: (cfg!(feature = "exec") && s.security.exec.enabled)
                || s.tools.overrides.contains_key("exec"),
            servers,
            templates,
        }
    }

    fn all(&self) -> Vec<Held> {
        self.servers.iter().map(|(_, h)| h.clone()).collect()
    }

    /// Whether a model under `allow` can call `tool` at all.
    fn callable(&self, allow: Option<&[String]>, tool: &str) -> bool {
        !self.s.tools.disabled.iter().any(|d| d == tool)
            && allow.is_none_or(|a| a.iter().any(|p| crate::registry::pattern_matches(p, tool)))
    }

    /// Everything a `subagent.run` call can hand a child: any template, and —
    /// unless freeform spawns are off — any configured server.
    fn any_spawn(&self) -> Vec<Held> {
        let mut out = Vec::new();
        if self.s.subagents.allow_freeform != Some(false) {
            out.extend(self.all());
        }
        out.extend(self.templates.values().flatten().cloned());
        out
    }

    /// The tools under `allow` that reach past the step's own servers: the
    /// spawn tool (when the caller holds it), `exec`, and any internal tool an
    /// override maps onto a server.
    fn tools(&self, allow: Option<&[String]>, spawns: bool, out: &mut Vec<Held>) {
        if spawns && self.callable(allow, "subagent.run") {
            out.extend(self.any_spawn());
        }
        if self.exec && self.callable(allow, "exec") {
            out.push(Held {
                label: "the `exec` tool".into(),
                tags: vec![TrifectaTag::Sensitive, TrifectaTag::Egress],
            });
        }
        for (tool, ov) in &self.s.tools.overrides {
            if self.callable(allow, tool) {
                out.extend(named(&self.servers, std::slice::from_ref(&ov.server)));
            }
        }
    }

    /// A step's explicit `servers:`, or every configured server: without the
    /// list the step is planned over every tool granted to workflows, whose
    /// servers are only known once they are dialed.
    fn listed_or_all(&self, spec: &Map<String, Value>) -> Vec<Held> {
        match spec.get("servers").and_then(Value::as_array) {
            Some(a) => named(
                &self.servers,
                &a.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect::<Vec<_>>(),
            ),
            None => self.all(),
        }
    }

    /// A spawn's reach: its template's, or its own `servers` — and what the
    /// child's tools reach. A child is not granted `subagent.run` unless its
    /// allow-list names it.
    fn spawn(&self, spec: &Map<String, Value>, out: &mut Vec<Held>) {
        if let Some(t) = spec.get("template").and_then(Value::as_str) {
            match self.templates.get(t) {
                Some(reach) => out.extend(reach.iter().cloned()),
                None => out.extend(self.any_spawn()),
            }
            return;
        }
        out.extend(self.listed_or_all(spec));
        let allow = allow_list(spec);
        self.tools(allow.as_deref(), allow.is_some(), out);
    }

    /// Everything the model-driven steps of `w` can reach. `think` and the
    /// other single-call kinds hold no tools; a deterministic `mcp.tool` or
    /// `http` step is not steered by the text it carries.
    fn workflow(&self, w: &Workflow) -> Vec<Held> {
        let mut out: Vec<Held> = Vec::new();
        for st in all_steps(w) {
            match st.kind.as_str() {
                "agent" => {
                    out.extend(self.listed_or_all(&st.spec));
                    let allow = allow_list(&st.spec);
                    self.tools(allow.as_deref(), true, &mut out);
                }
                "subagent" => self.spawn(&st.spec, &mut out),
                "tool" if st.field_str("name") == Some("subagent.run") => {
                    match st.field("args").and_then(Value::as_object) {
                        Some(args) => self.spawn(args, &mut out),
                        None => out.extend(self.any_spawn()),
                    }
                }
                // A `message` hands the text to the agent's own conversation,
                // and the agent turning it holds the root grant.
                "message" => {
                    out.extend(self.all());
                    self.tools(None, true, &mut out);
                }
                _ => {}
            }
        }
        let mut seen = BTreeSet::new();
        out.retain(|h| seen.insert(h.label.clone()));
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn settings(v: Value) -> Settings {
        serde_json::from_value(v).expect("settings")
    }

    fn wf(v: Value) -> Workflow {
        crate::engine::model::parse_workflow(&v).expect("workflow")
    }

    fn check(s: &Settings, ws: &[Workflow]) -> Vec<String> {
        refusals(s, &ws.iter().collect::<Vec<_>>())
    }

    /// `mail` holds both legs the root fold allows together; `notes` is
    /// sensitive alone.
    fn servers(extra: Value) -> Settings {
        let mut v = json!({
            "streams": {"inbox": {}, "work": {}},
            "mcp": {"servers": [
                {"name": "mail", "endpoint": "https://mail.invalid/mcp",
                 "tags": {"*": ["sensitive", "egress"]}},
                {"name": "notes", "endpoint": "https://notes.invalid/mcp",
                 "tags": {"*": ["sensitive"]}}
            ]}
        });
        if let (Some(o), Some(e)) = (v.as_object_mut(), extra.as_object()) {
            for (k, x) in e {
                o.insert(k.clone(), x.clone());
            }
        }
        settings(v)
    }

    fn intake(kind: &str) -> Workflow {
        let start = if kind == "webhook" {
            json!({"kind": "webhook", "path": "/in", "into": {"stream": "inbox", "subject": "msg"}})
        } else {
            json!({"kind": "a2a", "command": "note", "into": {"stream": "inbox", "subject": "msg"}})
        };
        wf(json!({"name": "intake", "steps": {"s": start}}))
    }

    fn consumer(stream: &str, servers: &[&str]) -> Workflow {
        wf(json!({"name": "triage", "steps": {
            "s": {"kind": "stream", "stream": stream},
            "a": {"kind": "agent", "depends_on": ["s"], "instruction": "x",
                  "servers": servers, "tools": ["mail.*", "notes.*"]},
            "f": {"kind": "finish", "depends_on": ["a"], "status": "completed"}
        }}))
    }

    #[test]
    fn a_webhook_into_stream_feeding_sensitive_and_egress_is_refused_naming_all_four() {
        let errs = check(
            &servers(json!({})),
            &[intake("webhook"), consumer("inbox", &["mail"])],
        );
        assert_eq!(errs.len(), 1, "{errs:?}");
        let e = &errs[0];
        for want in [
            "workflow \"triage\"",
            "stream \"inbox\"",
            "webhook `into:` at workflow \"intake\" step \"s\"",
            "mcp server \"mail\" [sensitive, egress]",
            "security.allow_trifecta",
        ] {
            assert!(e.contains(want), "missing {want:?} in {e}");
        }
    }

    #[test]
    fn an_a2a_into_stream_is_tainted_the_same_way() {
        let errs = check(
            &servers(json!({})),
            &[intake("a2a"), consumer("inbox", &["mail"])],
        );
        assert_eq!(errs.len(), 1, "{errs:?}");
        assert!(
            errs[0].contains("A2A `into:` at workflow \"intake\""),
            "{}",
            errs[0]
        );
    }

    #[test]
    fn the_explicit_allow_lifts_it() {
        let s = servers(json!({"security": {"allow_trifecta": true}}));
        assert!(check(&s, &[intake("webhook"), consumer("inbox", &["mail"])]).is_empty());
    }

    #[test]
    fn two_legs_are_allowed_and_an_untainted_stream_is_not_checked() {
        // Untrusted input + sensitive is two legs.
        assert!(
            check(
                &servers(json!({})),
                &[intake("webhook"), consumer("inbox", &["notes"])]
            )
            .is_empty()
        );
        // Nothing outside feeds `work`: a schedule emits into it.
        let tick = wf(json!({"name": "tick", "steps": {
            "s": {"kind": "schedule", "every": "1m"},
            "e": {"kind": "emit", "depends_on": ["s"], "stream": "work", "subject": "t"},
            "f": {"kind": "finish", "depends_on": ["e"], "status": "completed"}
        }}));
        assert!(check(&servers(json!({})), &[tick, consumer("work", &["mail"])]).is_empty());
    }

    #[test]
    fn an_emit_from_a_tainted_run_taints_its_target_and_the_chain_is_named() {
        // intake → inbox → relay emits into work → triage reads work.
        let relay = wf(json!({"name": "relay", "steps": {
            "s": {"kind": "stream", "stream": "inbox"},
            "e": {"kind": "emit", "depends_on": ["s"], "stream": "work", "subject": "fwd"},
            "f": {"kind": "finish", "depends_on": ["e"], "status": "completed"}
        }}));
        let errs = check(
            &servers(json!({})),
            &[intake("webhook"), relay, consumer("work", &["mail"])],
        );
        assert_eq!(errs.len(), 1, "{errs:?}");
        assert!(
            errs[0].contains("stream \"work\" (fed by `emit` at workflow \"relay\" step \"e\")"),
            "{}",
            errs[0]
        );
    }

    #[test]
    fn a_wait_on_event_and_a_nested_agent_are_seen() {
        let w = wf(json!({"name": "triage", "steps": {
            "s": {"kind": "manual"},
            "w": {"kind": "wait", "depends_on": ["s"], "on": "event", "stream": "inbox"},
            "each": {"kind": "foreach", "depends_on": ["w"], "over": "{{steps.w.output}}",
                     "body": {"steps": {
                         "a": {"kind": "agent", "instruction": "x", "servers": ["mail"], "tools": []}
                     }}},
            "f": {"kind": "finish", "depends_on": ["each"], "status": "completed"}
        }}));
        let errs = check(&servers(json!({})), &[intake("webhook"), w]);
        assert_eq!(errs.len(), 1, "{errs:?}");
    }

    #[test]
    fn a_child_workflow_started_by_a_tainted_run_carries_the_taint() {
        let parent = wf(json!({"name": "parent", "steps": {
            "s": {"kind": "stream", "stream": "inbox"},
            "c": {"kind": "workflow", "depends_on": ["s"], "name": "child"},
            "f": {"kind": "finish", "depends_on": ["c"], "status": "completed"}
        }}));
        let child = wf(json!({"name": "child", "steps": {
            "s": {"kind": "manual"},
            "a": {"kind": "agent", "depends_on": ["s"], "instruction": "x",
                  "servers": ["mail"], "tools": []},
            "f": {"kind": "finish", "depends_on": ["a"], "status": "completed"}
        }}));
        let errs = check(&servers(json!({})), &[intake("webhook"), parent, child]);
        assert_eq!(errs.len(), 1, "{errs:?}");
        assert!(errs[0].contains("workflow \"child\""), "{}", errs[0]);
        assert!(
            errs[0].contains("run by workflow \"parent\" step \"c\""),
            "{}",
            errs[0]
        );
    }

    #[test]
    fn an_agent_that_can_spawn_reaches_every_server() {
        // The step names only `notes`, but without a `tools` list it holds
        // `subagent.run`, and a child can be handed `mail`.
        let w = wf(json!({"name": "triage", "steps": {
            "s": {"kind": "stream", "stream": "inbox"},
            "a": {"kind": "agent", "depends_on": ["s"], "instruction": "x", "servers": ["notes"]},
            "f": {"kind": "finish", "depends_on": ["a"], "status": "completed"}
        }}));
        let errs = check(&servers(json!({})), &[intake("webhook"), w.clone()]);
        assert_eq!(errs.len(), 1, "{errs:?}");
        // …and with the spawn tool disabled it reaches `notes` alone.
        let s = servers(json!({"tools": {"disabled": ["subagent.run"]}}));
        assert!(check(&s, &[intake("webhook"), w]).is_empty());
    }

    #[test]
    fn a_mirrored_stream_counts_as_untrusted() {
        let mut s = servers(json!({}));
        s.subagents.templates.insert(
            "kid".into(),
            serde_json::from_value(json!({"instruction": "x", "mirror_streams": ["inbox"]}))
                .unwrap(),
        );
        let errs = check(&s, &[consumer("inbox", &["mail"])]);
        assert_eq!(errs.len(), 1, "{errs:?}");
        assert!(
            errs[0].contains("mirror_streams of subagent template \"kid\""),
            "{}",
            errs[0]
        );
    }

    #[test]
    fn a_workflow_tool_a_tainted_agent_can_call_carries_the_taint() {
        // `triage` reads `inbox` with an agent holding only `notes` — and the
        // `send` workflow's tool, whose run hands an agent `mail`.
        let triage = wf(json!({"name": "triage", "steps": {
            "s": {"kind": "stream", "stream": "inbox"},
            "a": {"kind": "agent", "depends_on": ["s"], "instruction": "x",
                  "servers": ["notes"], "tools": ["notes.*", "mail.send"]},
            "f": {"kind": "finish", "depends_on": ["a"], "status": "completed"}
        }}));
        let send = wf(
            json!({"name": "send", "tool": {"name": "mail.send"}, "steps": {
                "s": {"kind": "manual"},
                "a": {"kind": "agent", "depends_on": ["s"], "instruction": "x",
                      "servers": ["mail"], "tools": []},
                "f": {"kind": "finish", "depends_on": ["a"], "status": "completed"}
            }}),
        );
        let errs = check(&servers(json!({})), &[intake("webhook"), triage, send]);
        assert_eq!(errs.len(), 1, "{errs:?}");
        assert!(
            errs[0].contains("workflow \"send\"")
                && errs[0].contains(
                    "run by workflow \"triage\" step \"a\" (through its tool \"mail.send\")"
                ),
            "{}",
            errs[0]
        );
    }

    #[test]
    fn a_message_hands_the_text_to_the_root_grant() {
        let w = wf(json!({"name": "triage", "steps": {
            "s": {"kind": "stream", "stream": "inbox"},
            "m": {"kind": "message", "depends_on": ["s"], "to": "ops", "text": "{{steps.s.output}}"},
            "f": {"kind": "finish", "depends_on": ["m"], "status": "completed"}
        }}));
        let errs = check(&servers(json!({})), &[intake("webhook"), w]);
        assert_eq!(errs.len(), 1, "{errs:?}");
        assert!(errs[0].contains("mcp server \"mail\""), "{}", errs[0]);
    }

    #[test]
    fn a_template_spawn_reaches_its_template_servers_and_exec_counts_as_both_legs() {
        let mut s = servers(json!({}));
        s.subagents.templates.insert(
            "reader".into(),
            serde_json::from_value(json!({"instruction": "x", "servers": ["notes"]})).unwrap(),
        );
        s.subagents.templates.insert(
            "sender".into(),
            serde_json::from_value(json!({"instruction": "x", "servers": ["mail"]})).unwrap(),
        );
        let spawn = |t: &str| {
            wf(json!({"name": "triage", "steps": {
                "s": {"kind": "stream", "stream": "inbox"},
                "c": {"kind": "subagent", "depends_on": ["s"], "template": t},
                "f": {"kind": "finish", "depends_on": ["c"], "status": "completed"}
            }}))
        };
        assert!(check(&s, &[intake("webhook"), spawn("reader")]).is_empty());
        assert_eq!(check(&s, &[intake("webhook"), spawn("sender")]).len(), 1);
        // `exec` mapped off-box is both legs on its own, whatever server it
        // is mapped onto.
        let s =
            servers(json!({"tools": {"overrides": {"exec": {"server": "notes", "tool": "run"}}}}));
        let w = wf(json!({"name": "triage", "steps": {
            "s": {"kind": "stream", "stream": "inbox"},
            "a": {"kind": "agent", "depends_on": ["s"], "instruction": "x",
                  "servers": [], "tools": ["exec"]},
            "f": {"kind": "finish", "depends_on": ["a"], "status": "completed"}
        }}));
        let errs = check(&s, &[intake("webhook"), w]);
        assert_eq!(errs.len(), 1, "{errs:?}");
        assert!(
            errs[0].contains("the `exec` tool [sensitive, egress]"),
            "{}",
            errs[0]
        );
    }
}
