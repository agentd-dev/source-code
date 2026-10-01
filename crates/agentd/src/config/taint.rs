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
//! An edge is followed in every spelling the configuration offers for it: a
//! `workflow` step, a workflow tool and the `workflow.run` tool all start a
//! run; a `message` step and the `message.send` tool both hand text to the
//! root conversation; a webhook `into:` and a webhook start that `emit`s its
//! body both put a caller's text on a stream. A check that one spelling
//! dodges is a check only for the operator who happened to write the other.
//!
//! Coarse and static on purpose: per stream, not per value, and per server, as
//! the root fold is. It is a grant-level check, not data-flow tracking — it
//! follows text forward into the models it is handed to, not into shared state
//! (memory, artifacts) or back out of a run's result.

use crate::config::settings::{McpServer, Settings};
use crate::engine::model::{Step, Workflow};
use crate::registry::internal::{DefaultGrant, Onward};
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
    let ctx = Reach::new(s, workflows);
    let (streams, runs) = propagate(&ctx, workflows);
    let mut out = Vec::new();
    for w in workflows {
        let Some(run) = runs.get(&w.name).filter(|r| r.checked()) else {
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
        // What the check accepts, said as it is: every model-driven step of
        // a run the outside text reaches is judged together, so moving the
        // acting step into another workflow only moves the refusal there.
        out.push(format!(
            "workflow {:?}: lethal-trifecta refused — {}, and its model-driven steps reach {}: \
             untrusted input + sensitive + egress in one run. Every model-driven step of a run \
             the outside text reaches — this one, and any run it starts or feeds — is judged \
             together, so none of them may reach both of the other legs: narrow the steps' \
             `servers` and `tools` (a `tools` list that leaves out {}), act through a \
             deterministic step (`mcp.tool`, `http`), which the check does not count \
             (docs/security.md), or set security.allow_trifecta (audited)",
            w.name,
            run.describe(&streams),
            legs.join(", "),
            ctx.onward
                .iter()
                .map(|(n, _)| *n)
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    out
}

/// The taint each workflow's runs carry, by workflow — what a workflow tool's
/// caller is handed back as its result (`registry::register_workflow_tools`).
pub fn run_tags(s: &Settings, workflows: &[&Workflow]) -> BTreeMap<String, Vec<TrifectaTag>> {
    let ctx = Reach::new(s, workflows);
    propagate(&ctx, workflows)
        .1
        .into_iter()
        .filter(|(_, r)| !r.tags.is_empty())
        .map(|(name, r)| (name, r.tags))
        .collect()
}

/// Whether a reload from `old` to `new` moves anything this check reads, so
/// the definitions have to be judged again — the stored ones included, which
/// nothing else re-checks. Kept beside [`Reach`] and [`propagate`], the only
/// readers of these settings here, so a new input is added to both at once.
pub fn inputs_moved(old: &Settings, new: &Settings) -> bool {
    old.mcp != new.mcp
        || old.services != new.services
        || old.subagents != new.subagents
        || old.streams != new.streams
        || old.tools != new.tools
        || old.knowledge.server != new.knowledge.server
        || old.search.server != new.search.server
        || old.security.exec != new.security.exec
        || old.security.allow_trifecta != new.security.allow_trifecta
        || old.security.policies != new.security.policies
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
    /// What an outside caller handed the run directly: a webhook or A2A
    /// start, a `wait {on: webhook}`, a relayed signal, or a start by a run
    /// carrying only that. The operator who opened that route decided what
    /// its own run may do with it (`examples/startup/sre.yaml`: alerts from
    /// the operator's own monitoring drive the remediation), so such a run is
    /// not judged on its own reach. What it passes ON is: an `emit` onto a
    /// stream taints the stream exactly as `into:` would, and a run started
    /// by one that read a tainted stream is judged.
    outside: BTreeSet<String>,
}

impl RunTaint {
    /// Whether the run is judged: it reads a tainted stream, or a run that
    /// does started it.
    fn checked(&self) -> bool {
        !self.tags.is_empty() && !(self.streams.is_empty() && self.callers.is_empty())
    }

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

/// A field the engine renders at dispatch (`engine::model::RAW_FIELDS` holds
/// none of the ones read here): what it names is only known then, so it may
/// name anything.
fn templated(name: &str) -> bool {
    name.contains("{{")
}

/// The tainted streams a step reads events from: a `stream` or `correlate`
/// start, or a `wait {on: event}`. A templated name is any of them.
fn consumes<'t>(st: &Step, streams: &'t BTreeMap<String, StreamTaint>) -> Vec<&'t str> {
    let name = match st.kind.as_str() {
        "stream" | "correlate" => st.field_str("stream"),
        "wait" if st.field_str("on") == Some("event") => st.field_str("stream"),
        _ => None,
    };
    let Some(name) = name else {
        return Vec::new();
    };
    streams
        .iter()
        .filter(|(n, t)| !t.tags.is_empty() && (templated(name) || *n == name))
        .map(|(n, _)| n.as_str())
        .collect()
}

/// What outside caller hands a run of `w` its text directly, by step: a start
/// that fires a run on a request or a peer's message, or a wait a request
/// resumes. A start with `into:` appends instead of firing, and is a stream
/// producer, not this.
fn outside_starts(w: &Workflow) -> Vec<String> {
    all_steps(w)
        .into_iter()
        .filter(|st| st.field("into").is_none())
        .filter_map(|st| {
            let what = match st.kind.as_str() {
                "a2a" => Some("an `a2a` start"),
                k => crate::engine::model::route_opener(k, st.field_str("on")),
            };
            what.map(|what| format!("{what} {:?}", st.id))
        })
        .collect()
}

/// Fold every declared producer into each stream's taint, and each tainted
/// stream into the runs that read it, until nothing moves. A fixed point,
/// because an `emit` target and a `workflow` step's target are static (or,
/// when templated, every one there is): a run reading a tainted stream taints
/// what it emits into, and the workflows it starts.
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
    // The runs an outside caller hands its text to directly. Not judged on
    // their own reach (see [`RunTaint::outside`]); seeded so what they emit
    // and start carries it.
    let mut runs: BTreeMap<String, RunTaint> = BTreeMap::new();
    for w in workflows {
        for why in outside_starts(w) {
            let run = runs.entry(w.name.clone()).or_default();
            union(&mut run.tags, &untrusted);
            run.outside.insert(why);
        }
        // A webhook start's `signal:` relays the request body into every run
        // listening for that signal — the same text a direct start hands its
        // own run.
        for st in w.start_steps() {
            if st.kind == "webhook" && st.field("signal").is_some() {
                for r in &ctx.receivers {
                    let run = runs.entry(r.clone()).or_default();
                    union(&mut run.tags, &untrusted);
                    run.outside.insert(format!(
                        "the signal relayed by workflow {:?} step {:?}",
                        w.name, st.id
                    ));
                }
            }
        }
    }
    loop {
        let mut grew = false;
        for w in workflows {
            for st in all_steps(w) {
                for name in consumes(st, &streams) {
                    let tags = streams[name].tags.clone();
                    let run = runs.entry(w.name.clone()).or_default();
                    grew |= union(&mut run.tags, &tags);
                    grew |= run.streams.insert(name.to_string());
                }
            }
        }
        for w in workflows {
            let Some((tags, checked)) = runs
                .get(&w.name)
                .filter(|r| !r.tags.is_empty())
                .map(|r| (r.tags.clone(), r.checked()))
            else {
                continue;
            };
            for st in all_steps(w) {
                let at = format!("workflow {:?} step {:?}", w.name, st.id);
                if st.kind == "emit"
                    && let Some(stream) = st.field_str("stream")
                {
                    // A templated target is any declared stream.
                    let targets: Vec<&str> = if templated(stream) {
                        ctx.s.streams.keys().map(String::as_str).collect()
                    } else {
                        vec![stream]
                    };
                    for target in targets {
                        let t = streams.entry(target.to_string()).or_default();
                        grew |= union(&mut t.tags, &tags);
                        grew |= t.producers.insert(format!("`emit` at {at}"));
                    }
                }
                let mut started = Vec::new();
                ctx.step(st, &at, &mut Vec::new(), &mut started);
                for (target, caller) in started {
                    let run = runs.entry(target).or_default();
                    grew |= union(&mut run.tags, &tags);
                    grew |= if checked {
                        run.callers.insert(caller)
                    } else {
                        run.outside.insert(caller)
                    };
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
/// nothing, as it dials nothing. A templated name is rendered at spawn or at
/// dispatch, so it may be any of them.
fn named(servers: &[(String, Held)], names: &[String]) -> Vec<Held> {
    let any = names.iter().any(|n| templated(n));
    servers
        .iter()
        .filter(|(n, _)| any || names.contains(n))
        .map(|(_, h)| h.clone())
        .collect()
}

/// The `exec` tool, which is both legs on its own: it can touch anything
/// inside its `workdir` and talk to the network.
fn exec_held() -> Held {
    Held {
        label: "the `exec` tool".into(),
        tags: vec![TrifectaTag::Sensitive, TrifectaTag::Egress],
    }
}

/// One capability a model-driven step can reach, as a refusal names it.
#[derive(Debug, Clone, PartialEq)]
struct Held {
    label: String,
    tags: Vec<TrifectaTag>,
}

/// Who is calling a tool, under its `tools` allow-list: a workflow step, or
/// a flat child.
#[derive(Debug, Clone, Copy)]
enum Who<'l> {
    Step(Option<&'l [String]>),
    /// A flat child is offered no internal tool
    /// (`subagent/control.rs::NoSelfTools`) — except one a policy rule might
    /// touch, which it routes back up and the supervisor serves under the
    /// subagent grant (`runtime/subagents.rs`, `gated_tools`).
    Child(Option<&'l [String]>),
}

/// Every tag a tool could carry: what a workflow tool is judged by when the
/// question is whether a policy rule might touch it, since its derived tags
/// are only known once the registry is built.
const ANY_TAG: [TrifectaTag; 3] = [
    TrifectaTag::UntrustedInput,
    TrifectaTag::Sensitive,
    TrifectaTag::Egress,
];

/// A workflow started by a step, and the step as a refusal names it.
type Started = Vec<(String, String)>;

/// What the configuration lets a model-driven step reach, server by server.
struct Reach<'a> {
    s: &'a Settings,
    /// Every configured MCP server, by name, with its declared tags.
    servers: Vec<(String, Held)>,
    /// What a spawn of each subagent template reaches through its servers.
    templates: BTreeMap<String, Vec<Held>>,
    /// Each flat template's `tools` allow-list, which a child spawned from
    /// it runs under. An instance template is absent: its child is a daemon
    /// of its own, whose tools act inside it.
    flat: BTreeMap<String, Option<Vec<String>>>,
    /// The `exec` tool is callable: run locally, or mapped off-box.
    exec: bool,
    /// Each internal contract's grant, from the registry's own table.
    grants: BTreeMap<&'static str, DefaultGrant>,
    /// The internal contracts that hand text onward, and where
    /// (`registry::internal::onward`).
    onward: Vec<(&'static str, Onward)>,
    /// The internal contracts mapped onto a server — an override, or a
    /// `knowledge.*` / `search.*` profile — and that server. A call to one is
    /// served by the supervisor whatever the step's own `servers` say.
    mapped: Vec<(String, String)>,
    /// Every workflow in the set, by name.
    names: Vec<String>,
    /// Each workflow tool: its name, its workflow, and its grant.
    wf_tools: Vec<(String, String, crate::engine::model::WorkflowToolGrant)>,
    /// The workflows a signal reaches: a `signal` start, or a
    /// `wait {on: signal}` anywhere in the run.
    receivers: Vec<String>,
}

impl<'a> Reach<'a> {
    fn new(s: &'a Settings, workflows: &[&Workflow]) -> Self {
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
        let mut flat = BTreeMap::new();
        for (name, t) in &s.subagents.templates {
            if compiled
                .get(name)
                .is_none_or(|c| c.tier != crate::config::templates::Tier::Instance)
            {
                flat.insert(name.clone(), t.tools.clone());
            }
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
                // A flat child dials its template's servers; what its tools
                // reach is added at the spawn ([`Who::Child`]).
                Some(_) => match &t.servers {
                    Some(names) => named(&servers, names),
                    None => servers.iter().map(|(_, h)| h.clone()).collect(),
                },
                None => servers.iter().map(|(_, h)| h.clone()).collect(),
            };
            templates.insert(name.clone(), reach);
        }
        let contracts = crate::registry::internal::contracts();
        let profiles = crate::registry::profile_servers(s);
        let mut mapped: Vec<(String, String)> = s
            .tools
            .overrides
            .iter()
            .map(|(tool, ov)| (tool.clone(), ov.server.clone()))
            .collect();
        // The same profile mapping `registry::build` makes, on the prefix it
        // makes it on. Whether the server advertises the tool is only known
        // once it is dialed, so it is taken to.
        for c in contracts.iter().filter(|c| !c.builtin) {
            if let Some(srv) = c.name.split('.').next().and_then(|f| profiles.get(f))
                && !s.tools.overrides.contains_key(c.name)
            {
                mapped.push((c.name.to_string(), srv.to_string()));
            }
        }
        let receivers = workflows
            .iter()
            .filter(|w| {
                all_steps(w).iter().any(|st| {
                    (st.kind == "signal" && st.is_start())
                        || (st.kind == "wait" && st.field_str("on") == Some("signal"))
                })
            })
            .map(|w| w.name.clone())
            .collect();
        Reach {
            s,
            exec: (cfg!(feature = "exec") && s.security.exec.enabled)
                || s.tools.overrides.contains_key("exec"),
            servers,
            templates,
            flat,
            grants: contracts.iter().map(|c| (c.name, c.grant)).collect(),
            onward: contracts
                .iter()
                .filter_map(|c| crate::registry::internal::onward(c.name).map(|o| (c.name, o)))
                .collect(),
            mapped,
            names: workflows.iter().map(|w| w.name.clone()).collect(),
            wf_tools: workflows
                .iter()
                .filter_map(|w| {
                    w.tool
                        .as_ref()
                        .map(|t| (t.name.clone(), w.name.clone(), t.grant))
                })
                .collect(),
            receivers,
        }
    }

    fn all(&self) -> Vec<Held> {
        self.servers.iter().map(|(_, h)| h.clone()).collect()
    }

    fn disabled(&self, tool: &str) -> bool {
        self.s.tools.disabled.iter().any(|d| d == tool)
    }

    /// What a turn under the root grant reaches: every server, and what the
    /// root's own tools reach — it holds every internal contract's grant.
    /// Where it starts runs, and what its children's tools reach, add nothing:
    /// a run or a child reaches no more than the root already does.
    fn root(&self) -> Vec<Held> {
        let mut out = self.all();
        if self
            .onward
            .iter()
            .any(|(t, o)| *o == Onward::Children && !self.disabled(t))
        {
            out.extend(self.templates.values().flatten().cloned());
        }
        if self.exec && !self.disabled("exec") {
            out.push(exec_held());
        }
        for (tool, server) in &self.mapped {
            if !self.disabled(tool) {
                out.extend(named(&self.servers, std::slice::from_ref(server)));
            }
        }
        out
    }

    /// The tags the registry gives an internal contract — `exec` both legs, a
    /// mapped one its server's, plus what `tools.narrow` adds — which a policy
    /// rule matching on tags reads.
    fn tool_tags(&self, tool: &str) -> Vec<TrifectaTag> {
        let mut tags = Vec::new();
        if tool == "exec" {
            union(&mut tags, &exec_held().tags);
        }
        for (_, server) in self.mapped.iter().filter(|(t, _)| t == tool) {
            for h in named(&self.servers, std::slice::from_ref(server)) {
                union(&mut tags, &h.tags);
            }
        }
        for t in self
            .s
            .tools
            .narrow
            .get(tool)
            .into_iter()
            .flat_map(|n| &n.tags)
        {
            union(&mut tags, TrifectaTag::parse(t).as_slice());
        }
        tags
    }

    /// Whether `who` can call the internal contract (or any other tool)
    /// `tool` at all: not disabled, granted to it — the registry's grant, not
    /// a second copy — on its allow-list, and, for a child, routed back up
    /// because a policy rule might touch it (the same `could_apply` the
    /// runtime asks). A templated allow-list entry is rendered at dispatch,
    /// so it may name anything.
    fn callable(&self, who: Who, tool: &str) -> bool {
        if self.disabled(tool) {
            return false;
        }
        let (allow, granted) = match who {
            Who::Step(allow) => (allow, self.grants.get(tool).is_none_or(|g| g.workflows)),
            Who::Child(allow) => (allow, self.grants.get(tool).is_none_or(|g| g.subagents)),
        };
        granted
            && allow.is_none_or(|a| {
                a.iter()
                    .any(|p| templated(p) || crate::registry::pattern_matches(p, tool))
            })
            && match who {
                Who::Step(_) => true,
                Who::Child(_) => self.gated(tool, &self.tool_tags(tool)),
            }
    }

    /// Whether a policy rule might touch a child's call of `tool`.
    fn gated(&self, tool: &str, tags: &[TrifectaTag]) -> bool {
        crate::sec::policy::could_apply(
            &self.s.security.policies,
            tool,
            tags,
            crate::config::settings::PolicyCaller::Subagent,
        )
    }

    /// Everything a `subagent.run` call can hand a child: any template, and —
    /// unless freeform spawns are off — any configured server; and what the
    /// child's tools reach under any allow-list it may be given.
    fn any_spawn(&self, at: &str, held: &mut Vec<Held>, started: &mut Started) {
        if self.s.subagents.allow_freeform != Some(false) {
            held.extend(self.all());
            self.tools(Who::Child(None), at, held, started);
        }
        held.extend(self.templates.values().flatten().cloned());
        for tools in self.flat.values() {
            self.tools(Who::Child(tools.as_deref()), at, held, started);
        }
    }

    /// Where an internal contract that hands text onward puts it: `args` are
    /// a deterministic call's own (a `tool` step), absent when a model picks
    /// them, which can name anything.
    fn hand_on(
        &self,
        tool: &str,
        onward: Onward,
        args: Option<&Map<String, Value>>,
        via: String,
        held: &mut Vec<Held>,
        started: &mut Started,
    ) {
        match onward {
            Onward::Root => held.extend(self.root()),
            Onward::Children => match args.filter(|_| tool == "subagent.run") {
                Some(args) => self.spawn(args, &via, held, started),
                None => self.any_spawn(&via, held, started),
            },
            Onward::Workflows => match args
                .and_then(|a| a.get("name"))
                .and_then(Value::as_str)
                .filter(|n| !templated(n))
            {
                Some(n) => started.push((n.to_string(), via)),
                None => started.extend(self.names.iter().map(|n| (n.clone(), via.clone()))),
            },
            Onward::Signal => {
                started.extend(self.receivers.iter().map(|n| (n.clone(), via.clone())))
            }
        }
    }

    /// The tools `who` can call that reach past its own servers: the ones
    /// that hand text onward, `exec`, any internal tool mapped onto a server,
    /// and the workflow tools, each of which starts a run.
    fn tools(&self, who: Who, at: &str, held: &mut Vec<Held>, started: &mut Started) {
        for (tool, onward) in &self.onward {
            if self.callable(who, tool) {
                self.hand_on(
                    tool,
                    *onward,
                    None,
                    format!("{at} (through {tool})"),
                    held,
                    started,
                );
            }
        }
        if self.exec && self.callable(who, "exec") {
            held.push(exec_held());
        }
        for (tool, server) in &self.mapped {
            if self.callable(who, tool) {
                held.extend(named(&self.servers, std::slice::from_ref(server)));
            }
        }
        for (tool, target, grant) in &self.wf_tools {
            let granted = match who {
                Who::Step(_) => grant.workflows,
                Who::Child(_) => grant.subagents && self.gated(tool, &ANY_TAG),
            };
            if granted && self.callable(who, tool) {
                started.push((target.clone(), format!("{at} (through its tool {tool:?})")));
            }
        }
    }

    /// A step's explicit `servers:`, or every configured server: without the
    /// list the step is planned over every tool granted to workflows, whose
    /// servers are only known once they are dialed. With it, the plan holds
    /// the MCP tools of those servers alone (`runtime::turns::tool_plan`).
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

    /// A spawn's reach: its template's servers, or its own — and what the
    /// child's tools reach, under the template's `tools` or the spawn's own,
    /// whichever way the spawn is spelled.
    fn spawn(
        &self,
        spec: &Map<String, Value>,
        at: &str,
        held: &mut Vec<Held>,
        started: &mut Started,
    ) {
        match spec.get("template").and_then(Value::as_str) {
            Some(t) => match self.templates.get(t) {
                Some(reach) => {
                    held.extend(reach.iter().cloned());
                    if let Some(tools) = self.flat.get(t) {
                        self.tools(Who::Child(tools.as_deref()), at, held, started);
                    }
                }
                None => self.any_spawn(at, held, started),
            },
            None => {
                held.extend(self.listed_or_all(spec));
                let allow = allow_list(spec);
                self.tools(Who::Child(allow.as_deref()), at, held, started);
            }
        }
    }

    /// What one step hands its run's text to: the capabilities a model it
    /// drives can reach (`held`), and the workflows it starts (`started`).
    /// `think` and the other single-call kinds hold no tools; a deterministic
    /// `mcp.tool` or `http` step is not steered by the text it carries.
    fn step(&self, st: &Step, at: &str, held: &mut Vec<Held>, started: &mut Started) {
        match st.kind.as_str() {
            "agent" => {
                held.extend(self.listed_or_all(&st.spec));
                let allow = allow_list(&st.spec);
                self.tools(Who::Step(allow.as_deref()), at, held, started);
            }
            "subagent" => self.spawn(&st.spec, at, held, started),
            // A deterministic call of a tool that hands its arguments to a
            // model is that hand-off, spelled as a step.
            "tool" => {
                if let Some(name) = st.field_str("name")
                    && let Some(onward) = crate::registry::internal::onward(name)
                {
                    let args = st.field("args").and_then(Value::as_object);
                    self.hand_on(name, onward, args, at.to_string(), held, started);
                }
            }
            // A `message` hands the text to the agent's own conversation,
            // and the agent turning it holds the root grant.
            "message" => held.extend(self.root()),
            // A templated name can start any of them.
            "workflow" => match st.field_str("name") {
                Some(n) if !templated(n) => started.push((n.to_string(), at.to_string())),
                _ => started.extend(self.names.iter().map(|n| (n.clone(), at.to_string()))),
            },
            _ => {}
        }
    }

    /// Everything the model-driven steps of `w` can reach.
    fn workflow(&self, w: &Workflow) -> Vec<Held> {
        let mut out: Vec<Held> = Vec::new();
        for st in all_steps(w) {
            self.step(st, "", &mut out, &mut Vec::new());
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
        // …and with every tool that hands text on disabled it reaches `notes`
        // alone.
        let s = servers(json!({"tools": {"disabled": [
            "subagent.run", "subagent.send", "message.send", "workflow.run", "workflow.signal"
        ]}}));
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

    /// A reader narrowed to `notes`, holding `extra` on top of its own tools —
    /// the shape the refusal tells an operator to write.
    fn reader(extra: &[&str]) -> Workflow {
        let mut tools = vec!["notes.*"];
        tools.extend_from_slice(extra);
        wf(json!({"name": "triage", "steps": {
            "s": {"kind": "stream", "stream": "inbox"},
            "a": {"kind": "agent", "depends_on": ["s"], "instruction": "x",
                  "servers": ["notes"], "tools": tools},
            "f": {"kind": "finish", "depends_on": ["a"], "status": "completed"}
        }}))
    }

    /// A workflow anyone may start, whose agent holds `mail`.
    fn sender(start: Value) -> Workflow {
        wf(json!({"name": "send", "steps": {
            "s": start,
            "a": {"kind": "agent", "depends_on": ["s"], "instruction": "x",
                  "servers": ["mail"], "tools": []},
            "f": {"kind": "finish", "depends_on": ["a"], "status": "completed"}
        }}))
    }

    #[test]
    fn message_send_is_the_message_step_spelled_as_a_tool() {
        let s = servers(json!({}));
        assert!(check(&s, &[intake("webhook"), reader(&[])]).is_empty());
        let errs = check(&s, &[intake("webhook"), reader(&["message.send"])]);
        assert_eq!(errs.len(), 1, "{errs:?}");
        assert!(errs[0].contains("mcp server \"mail\""), "{}", errs[0]);
        // …and as a deterministic step.
        let w = wf(json!({"name": "triage", "steps": {
            "s": {"kind": "stream", "stream": "inbox"},
            "m": {"kind": "tool", "depends_on": ["s"], "name": "message.send",
                  "args": {"text": "{{steps.s.output}}"}},
            "f": {"kind": "finish", "depends_on": ["m"], "status": "completed"}
        }}));
        assert_eq!(check(&s, &[intake("webhook"), w]).len(), 1);
    }

    #[test]
    fn workflow_run_and_workflow_signal_start_what_they_can_name() {
        let s = servers(json!({}));
        let manual = json!({"kind": "manual"});
        assert!(
            check(
                &s,
                &[intake("webhook"), reader(&[]), sender(manual.clone())]
            )
            .is_empty()
        );
        let errs = check(
            &s,
            &[
                intake("webhook"),
                reader(&["workflow.run"]),
                sender(manual.clone()),
            ],
        );
        assert_eq!(errs.len(), 1, "{errs:?}");
        assert!(
            errs[0].contains("workflow \"send\"")
                && errs[0].contains("run by workflow \"triage\" step \"a\" (through workflow.run)"),
            "{}",
            errs[0]
        );
        // `workflow.signal` reaches what listens for a signal: a manual start
        // does not…
        let signal = reader(&["workflow.signal"]);
        assert!(check(&s, &[intake("webhook"), signal.clone(), sender(manual)]).is_empty());
        // …a signal start does.
        let errs = check(
            &s,
            &[
                intake("webhook"),
                signal,
                sender(json!({"kind": "signal", "name": "go"})),
            ],
        );
        assert_eq!(errs.len(), 1, "{errs:?}");
        // A deterministic `workflow.run` starts the workflow it names, or —
        // templated — any of them.
        let run = |name: &str| {
            wf(json!({"name": "triage", "steps": {
                "s": {"kind": "stream", "stream": "inbox"},
                "r": {"kind": "tool", "depends_on": ["s"], "name": "workflow.run",
                      "args": {"name": name, "inputs": {"text": "{{steps.s.output}}"}}},
                "f": {"kind": "finish", "depends_on": ["r"], "status": "completed"}
            }}))
        };
        let other = wf(json!({"name": "other", "steps": {
            "s": {"kind": "manual"},
            "f": {"kind": "finish", "depends_on": ["s"], "status": "completed"}
        }}));
        let send = sender(json!({"kind": "manual"}));
        assert!(
            check(
                &s,
                &[intake("webhook"), run("other"), other.clone(), send.clone()]
            )
            .is_empty()
        );
        assert_eq!(
            check(
                &s,
                &[intake("webhook"), run("send"), other.clone(), send.clone()]
            )
            .len(),
            1
        );
        assert_eq!(
            check(
                &s,
                &[
                    intake("webhook"),
                    run("{{steps.s.output.data.w}}"),
                    other,
                    send
                ]
            )
            .len(),
            1
        );
    }

    #[test]
    fn subagent_send_steers_a_child_that_can_hold_anything() {
        let s = servers(json!({}));
        assert_eq!(
            check(&s, &[intake("webhook"), reader(&["subagent.send"])]).len(),
            1
        );
    }

    #[test]
    fn a_templated_server_or_tool_may_name_anything() {
        let s = servers(json!({}));
        let w = |servers: Value, tools: Value| {
            wf(json!({"name": "triage", "steps": {
                "s": {"kind": "stream", "stream": "inbox"},
                "a": {"kind": "agent", "depends_on": ["s"], "instruction": "x",
                      "servers": servers, "tools": tools},
                "f": {"kind": "finish", "depends_on": ["a"], "status": "completed"}
            }}))
        };
        assert!(
            check(
                &s,
                &[intake("webhook"), w(json!(["notes"]), json!(["notes.*"]))]
            )
            .is_empty()
        );
        // The event names the server.
        assert_eq!(
            check(
                &s,
                &[
                    intake("webhook"),
                    w(json!(["{{steps.s.output.data.srv}}"]), json!(["notes.*"]))
                ]
            )
            .len(),
            1
        );
        // The event names the tool: `subagent.run`, say.
        assert_eq!(
            check(
                &s,
                &[
                    intake("webhook"),
                    w(json!(["notes"]), json!(["{{steps.s.output.data.t}}"]))
                ]
            )
            .len(),
            1
        );
    }

    #[test]
    fn a_templated_stream_is_any_stream() {
        let s = servers(json!({}));
        // A relay emitting, from inside a body, into a stream the event names
        // taints every declared stream.
        let relay = wf(json!({"name": "relay", "steps": {
            "s": {"kind": "stream", "stream": "inbox"},
            "g": {"kind": "subgraph", "depends_on": ["s"], "body": {"steps": {
                "e": {"kind": "emit", "stream": "{{steps.s.output.subject}}", "subject": "x"}
            }}},
            "f": {"kind": "finish", "depends_on": ["g"], "status": "completed"}
        }}));
        let errs = check(&s, &[intake("webhook"), relay, consumer("work", &["mail"])]);
        assert_eq!(errs.len(), 1, "{errs:?}");
        assert!(errs[0].contains("stream \"work\""), "{}", errs[0]);
        // A wait on a stream its input names reads every tainted one.
        let w = wf(json!({"name": "triage", "steps": {
            "s": {"kind": "manual"},
            "w": {"kind": "wait", "depends_on": ["s"], "on": "event",
                  "stream": "{{steps.s.output.which}}"},
            "a": {"kind": "agent", "depends_on": ["w"], "instruction": "x",
                  "servers": ["mail"], "tools": []},
            "f": {"kind": "finish", "depends_on": ["a"], "status": "completed"}
        }}));
        let errs = check(&s, &[intake("webhook"), w]);
        assert_eq!(errs.len(), 1, "{errs:?}");
        assert!(errs[0].contains("stream \"inbox\""), "{}", errs[0]);
    }

    /// A start that hands its body to the run directly — or a wait a request
    /// resumes — is the operator's route: its own run is not judged. What it
    /// emits is outside text on a stream, exactly as `into:` puts it there.
    #[test]
    fn a_direct_start_that_emits_taints_the_stream_like_into() {
        let s = servers(json!({}));
        let emit = json!({"kind": "emit", "depends_on": ["s"], "stream": "inbox",
                          "subject": "msg", "data": "{{inputs}}"});
        let fin = json!({"kind": "finish", "depends_on": ["e"], "status": "completed"});
        for start in [
            json!({"kind": "webhook", "path": "/in"}),
            json!({"kind": "a2a", "command": "note"}),
        ] {
            let intake = wf(json!({"name": "intake", "steps": {
                "s": start, "e": emit, "f": fin
            }}));
            let errs = check(&s, &[intake, consumer("inbox", &["mail"])]);
            assert_eq!(errs.len(), 1, "{errs:?}");
            assert!(
                errs[0].contains("fed by `emit` at workflow \"intake\" step \"e\""),
                "{}",
                errs[0]
            );
        }
        let wait = wf(json!({"name": "intake", "steps": {
            "s": {"kind": "manual"},
            "w": {"kind": "wait", "depends_on": ["s"], "on": "webhook"},
            "e": {"kind": "emit", "depends_on": ["w"], "stream": "inbox", "subject": "msg"},
            "f": {"kind": "finish", "depends_on": ["e"], "status": "completed"}
        }}));
        assert_eq!(check(&s, &[wait, consumer("inbox", &["mail"])]).len(), 1);
        // The route's own run, holding both legs, loads: that is the
        // operator's call about their own route.
        let own = wf(json!({"name": "incident", "steps": {
            "s": {"kind": "webhook", "path": "/alert"},
            "a": {"kind": "agent", "depends_on": ["s"], "instruction": "x", "servers": ["mail"]},
            "f": {"kind": "finish", "depends_on": ["a"], "status": "completed"}
        }}));
        assert!(check(&s, &[own]).is_empty());
    }

    #[test]
    fn a_relayed_signal_is_the_same_outside_text() {
        let s = servers(json!({}));
        let hook = wf(json!({"name": "resolved", "steps": {
            "s": {"kind": "webhook", "path": "/resolved", "signal": "done/{{body.id}}"},
            "f": {"kind": "finish", "depends_on": ["s"], "status": "completed"}
        }}));
        let parked = wf(json!({"name": "incident", "steps": {
            "s": {"kind": "manual"},
            "w": {"kind": "wait", "depends_on": ["s"], "on": "signal", "signal": "done/x"},
            "e": {"kind": "emit", "depends_on": ["w"], "stream": "inbox", "subject": "msg"},
            "f": {"kind": "finish", "depends_on": ["e"], "status": "completed"}
        }}));
        assert!(check(&s, &[parked.clone(), consumer("inbox", &["mail"])]).is_empty());
        assert_eq!(
            check(&s, &[hook, parked, consumer("inbox", &["mail"])]).len(),
            1
        );
    }

    #[test]
    fn a_profile_tool_reaches_its_profile_server() {
        let s = servers(json!({"search": {"server": "mail"}}));
        assert!(check(&s, &[intake("webhook"), reader(&[])]).is_empty());
        let errs = check(&s, &[intake("webhook"), reader(&["search.*"])]);
        assert_eq!(errs.len(), 1, "{errs:?}");
        assert!(errs[0].contains("mcp server \"mail\""), "{}", errs[0]);
    }

    #[test]
    fn a_workflow_tool_not_granted_to_workflows_starts_nothing_from_a_step() {
        let send = |grant: Value| {
            wf(
                json!({"name": "send", "tool": {"name": "mail.send", "grant": grant}, "steps": {
                    "s": {"kind": "manual"},
                    "a": {"kind": "agent", "depends_on": ["s"], "instruction": "x",
                          "servers": ["mail"], "tools": []},
                    "f": {"kind": "finish", "depends_on": ["a"], "status": "completed"}
                }}),
            )
        };
        let s = servers(json!({}));
        assert_eq!(
            check(
                &s,
                &[intake("webhook"), reader(&["mail.send"]), send(json!({}))]
            )
            .len(),
            1
        );
        assert!(
            check(
                &s,
                &[
                    intake("webhook"),
                    reader(&["mail.send"]),
                    send(json!({"workflows": false}))
                ]
            )
            .is_empty()
        );
    }

    #[test]
    fn a_flat_child_reaches_its_servers_alone() {
        // `exec` mapped off-box is both legs for whoever can call it — but a
        // flat child is offered no internal tool, whichever way it is
        // spawned, while no policy rule routes one back up.
        let mut s =
            servers(json!({"tools": {"overrides": {"exec": {"server": "notes", "tool": "run"}}}}));
        s.subagents.templates.insert(
            "reader".into(),
            serde_json::from_value(json!({"instruction": "x", "servers": ["notes"]})).unwrap(),
        );
        let spawn = |spec: Value| {
            let mut c = json!({"kind": "subagent", "depends_on": ["s"]});
            for (k, v) in spec.as_object().unwrap() {
                c[k] = v.clone();
            }
            wf(json!({"name": "triage", "steps": {
                "s": {"kind": "stream", "stream": "inbox"},
                "c": c,
                "f": {"kind": "finish", "depends_on": ["c"], "status": "completed"}
            }}))
        };
        assert!(
            check(
                &s,
                &[intake("webhook"), spawn(json!({"template": "reader"}))]
            )
            .is_empty()
        );
        assert!(
            check(
                &s,
                &[
                    intake("webhook"),
                    spawn(json!({"instruction": "x", "servers": ["notes"], "tools": ["exec"]}))
                ]
            )
            .is_empty()
        );
    }

    /// …unless a policy rule might touch one of the internal tools a child
    /// is granted: the child routes that call back up and the supervisor
    /// serves it, so the tool's reach is the child's.
    #[test]
    fn a_child_reaches_the_tools_a_policy_routes_back_up() {
        let policy =
            |m: Value| json!({"security": {"policies": [{"match": m, "action": "allow"}]}});
        let reader = |s: &mut Settings| {
            s.subagents.templates.insert(
                "reader".into(),
                serde_json::from_value(json!({"instruction": "x", "servers": ["notes"]})).unwrap(),
            );
        };
        let spawn = wf(json!({"name": "triage", "steps": {
            "s": {"kind": "stream", "stream": "inbox"},
            "c": {"kind": "subagent", "depends_on": ["s"], "template": "reader"},
            "f": {"kind": "finish", "depends_on": ["c"], "status": "completed"}
        }}));
        // `message.send` is granted to subagents: under a rule naming it, the
        // child can hand the text to the root grant.
        let mut s = servers(policy(json!({"tool": "message.send"})));
        reader(&mut s);
        let errs = check(&s, &[intake("webhook"), spawn.clone()]);
        assert_eq!(errs.len(), 1, "{errs:?}");
        assert!(errs[0].contains("mcp server \"mail\""), "{}", errs[0]);
        // A rule only for workflow callers routes nothing of the child's.
        let mut s = servers(policy(
            json!({"tool": "message.send", "caller": ["workflow"]}),
        ));
        reader(&mut s);
        assert!(check(&s, &[intake("webhook"), spawn.clone()]).is_empty());
        // A rule on tags reads the tags the registry gives the tool: `exec`
        // carries egress.
        let mut s = servers(json!({
            "tools": {"overrides": {"exec": {"server": "notes", "tool": "run"}}},
            "security": {"policies": [{"match": {"tags": ["egress"]}, "action": "allow"}]}
        }));
        reader(&mut s);
        let errs = check(&s, &[intake("webhook"), spawn]);
        assert_eq!(errs.len(), 1, "{errs:?}");
        assert!(errs[0].contains("the `exec` tool"), "{}", errs[0]);
    }

    #[test]
    fn a_run_started_by_a_routes_own_run_is_its_too_but_one_started_by_a_reader_is_judged() {
        // The route's run can start `send` (its agent holds workflow.run):
        // still the route's own work, not judged.
        let s = servers(json!({}));
        let route = wf(json!({"name": "incident", "steps": {
            "s": {"kind": "webhook", "path": "/alert"},
            "a": {"kind": "agent", "depends_on": ["s"], "instruction": "x",
                  "servers": ["notes"], "tools": ["notes.*", "workflow.run"]},
            "f": {"kind": "finish", "depends_on": ["a"], "status": "completed"}
        }}));
        let send = sender(json!({"kind": "manual"}));
        assert!(check(&s, &[route, send.clone()]).is_empty());
        // The same edge from a run that read a tainted stream is judged.
        assert_eq!(
            check(&s, &[intake("webhook"), reader(&["workflow.run"]), send]).len(),
            1
        );
    }

    #[test]
    fn run_tags_carry_what_a_run_reads() {
        let s = servers(json!({}));
        let tick = wf(json!({"name": "tick", "steps": {
            "s": {"kind": "schedule", "every": "1m"},
            "f": {"kind": "finish", "depends_on": ["s"], "status": "completed"}
        }}));
        let (i, c) = (intake("webhook"), consumer("inbox", &["notes"]));
        let tags = run_tags(&s, &[&i, &c, &tick]);
        assert_eq!(tags.get("triage"), Some(&vec![TrifectaTag::UntrustedInput]));
        assert_eq!(tags.get("tick"), None);
    }

    #[test]
    fn every_input_the_check_reads_moves_it() {
        let base = servers(json!({}));
        assert!(!inputs_moved(&base, &base.clone()));
        for change in [
            json!({"mcp": {"servers": []}}),
            json!({"services": {"m": {"kind": "mcp", "endpoint": "https://m.invalid/mcp"}}}),
            json!({"subagents": {"allow_freeform": false}}),
            json!({"streams": {"inbox": {}}}),
            json!({"tools": {"disabled": ["exec"]}}),
            json!({"knowledge": {"server": "notes"}}),
            json!({"search": {"server": "notes"}}),
            json!({"security": {"exec": {"enabled": true}}}),
            json!({"security": {"allow_trifecta": true}}),
            json!({"security": {"policies": [{"match": {"tool": "x"}, "action": "allow"}]}}),
        ] {
            assert!(inputs_moved(&base, &servers(change.clone())), "{change}");
        }
    }
}
