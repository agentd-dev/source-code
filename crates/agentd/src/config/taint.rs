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
//! It is followed back as well as forward. A run's result is the text it was
//! handed, worked over, and so is the result of a child it spawned: a caller
//! that reads one back — a sync workflow tool's reply, a `workflow` step that
//! is not detached, a `join`, a `wait {on: run}` or `{on: subagent}`, a
//! read-back contract (`registry::internal::read_back`: `workflow.run
//! {wait}`, `workflow.wait`, `workflow.status`, the `subagent.status`,
//! `.await` and `.list` that read a child by handle, and the `plan.update`
//! binding whose run or child settles its outcome into the plan), a stream
//! the daemon's own telemetry is tapped onto, or the notes a finished run and
//! a child's result leave in the root transcript — has the outside text in
//! front of it, and is judged with it in reach. The root conversation is such
//! a caller too, judged with everything the root grant reaches.
//!
//! Coarse and static on purpose: per stream, not per value, and per server, as
//! the root fold is. It is a grant-level check, not data-flow tracking — it
//! follows text into the models it is handed to and out of the runs that
//! produced it, not into shared state (memory, artifacts).

use crate::config::settings::{McpServer, Settings};
use crate::engine::model::{Step, Workflow};
use crate::registry::internal::{DefaultGrant, Onward, ReadBack};
use crate::sec::scope::{TrifectaTag, TrifectaVerdict, check_trifecta};
use serde_json::{Map, Value};
use std::collections::{BTreeMap, BTreeSet};

/// The refusals for `workflows` under `s`: one per workflow whose runs read a
/// tainted stream, are started by a run that does, or read back the result of
/// a run the outside text reaches, while its model-driven steps reach
/// `sensitive` and `egress` — and one for the root conversation when it reads
/// such a result back holding both. Unless `security.allow_trifecta` is set,
/// which lifts this check with every other trifecta gate.
///
/// `workflows` is the whole set the configuration would run. Another
/// definition can only add producers, consumers and edges, and every one of
/// them only adds taint — with one exception that errs toward refusing: it
/// can hand a reader an origin, and a result whose every origin the reader
/// was handed already taints it with nothing new. So a refusal over part of
/// the set (the definitions `validate` can read without dialling) is a
/// refusal over all of it, except a read-back the rest of the set would have
/// handed the reader; the start re-judges the whole set.
pub fn refusals(s: &Settings, workflows: &[&Workflow]) -> Vec<String> {
    let ctx = Reach::new(s, workflows);
    let (streams, runs) = propagate(&ctx, workflows);
    let mut out = Vec::new();
    for w in workflows {
        let Some(run) = runs.get(&w.name).filter(|r| r.checked()) else {
            continue;
        };
        let held = ctx.workflow(w);
        let Some(legs) = refused(s, &run.tags, &held) else {
            continue;
        };
        // What the check accepts, said as it is: every model-driven step of
        // a run the outside text reaches is judged together, so moving the
        // acting step into another workflow only moves the refusal there.
        out.push(format!(
            "workflow {:?}: lethal-trifecta refused — {}, and its model-driven steps reach {legs}: \
             untrusted input + sensitive + egress in one run. Every model-driven step of a run \
             the outside text reaches — this one, any run it starts or feeds, and any caller that \
             reads its result back — is judged together, so none of them may reach both of the \
             other legs: narrow the steps' `servers` and `tools` (a `tools` list that leaves out \
             {}), act through a deterministic step (`mcp.tool`, `http`), which the check does \
             not count (docs/security.md), or set security.allow_trifecta (audited)",
            w.name,
            run.describe(&streams),
            ctx.advice(),
        ));
    }
    // The root conversation reads results back too. It is judged with
    // everything the root grant reaches; what it starts adds nothing, since
    // a run reaches no more than the root already does.
    let reads: Vec<(String, String)> = ctx
        .root_reads()
        .into_iter()
        .filter(|(target, _)| runs.get(target).is_some_and(|r| !r.tags.is_empty()))
        .collect();
    if !reads.is_empty() {
        let mut tags = Vec::new();
        for (target, _) in &reads {
            union(&mut tags, &runs[target].tags);
        }
        let mut held = ctx.root();
        let mut seen = BTreeSet::new();
        held.retain(|h| seen.insert(h.label.clone()));
        if let Some(legs) = refused(s, &tags, &held) {
            out.push(format!(
                "the root conversation: lethal-trifecta refused — {}, and the root grant reaches \
                 {legs}: untrusted input + sensitive + egress in one conversation. A result read \
                 back carries the outside text its run was handed: take the root's read-back \
                 away — list {} in `tools.disabled`, or leave them out of an \
                 `agent.tools.internal` list; give a workflow tool `grant.root: false` or `mode: \
                 async`; set `agent.on_workflow_finished: ignore` and leave `subagent_result` out \
                 of `agent.wake_on` — narrow the root's servers, or set security.allow_trifecta \
                 (audited)",
                by_edge(&reads),
                ctx.root_read_tools()
                    .iter()
                    .map(|n| format!("`{n}`"))
                    .collect::<Vec<_>>()
                    .join(", "),
            ));
        }
    }
    out
}

/// The root's read-back edges as a refusal names them: one line per edge,
/// with every tainted workflow it reads back.
fn by_edge(reads: &[(String, String)]) -> String {
    let mut edges: Vec<(&str, Vec<String>)> = Vec::new();
    for (target, via) in reads {
        let target = format!("{target:?}");
        match edges.iter_mut().find(|(v, _)| v == via) {
            Some((_, targets)) if targets.contains(&target) => {}
            Some((_, targets)) => targets.push(target),
            None => edges.push((via, vec![target])),
        }
    }
    edges
        .into_iter()
        .map(|(via, targets)| {
            let what = if targets.len() == 1 {
                "workflow"
            } else {
                "workflows"
            };
            format!(
                "it reads back the result of {what} {} ({via})",
                targets.join(", ")
            )
        })
        .collect::<Vec<_>>()
        .join("; ")
}

/// Whether `tags` and what `held` reaches complete the trifecta; if so, the
/// capabilities that brought the other two legs, as a refusal names them.
/// Only those: listing every server the steps reach would bury the two the
/// operator has to move.
fn refused(s: &Settings, tags: &[TrifectaTag], held: &[Held]) -> Option<String> {
    let all = tags
        .iter()
        .chain(held.iter().flat_map(|h| h.tags.iter()))
        .copied();
    if check_trifecta(all, s.security.allow_trifecta) != TrifectaVerdict::RefusedTrifecta {
        return None;
    }
    Some(
        held.iter()
            .filter(|h| {
                h.tags
                    .iter()
                    .any(|t| matches!(t, TrifectaTag::Sensitive | TrifectaTag::Egress))
            })
            .map(|h| format!("{} [{}]", h.label, tag_names(&h.tags)))
            .collect::<Vec<_>>()
            .join(", "),
    )
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
        // What the root conversation reads back: its tool selection, and the
        // note a finished run leaves in its transcript.
        || old.agent.tools != new.agent.tools
        || old.agent.on_workflow_finished != new.agent.on_workflow_finished
        || old.agent.wake_on != new.agent.wake_on
        // The streams the daemon's own telemetry is tapped onto, each fed by
        // every run.
        || old.observability.runtime_events != new.observability.runtime_events
        || old.observability.audit != new.observability.audit
}

/// What feeds one stream.
#[derive(Debug, Default)]
struct StreamTaint {
    tags: Vec<TrifectaTag>,
    /// Each producer, as the refusal names it.
    producers: BTreeSet<String>,
    /// Where the text its producers put on it entered ([`RunTaint::origins`]).
    origins: BTreeSet<String>,
}

/// Why one workflow's runs carry taint. Only the immediate edges are kept —
/// the stream it reads, the step that runs it — so a cycle (a workflow that
/// emits into the stream it consumes) settles instead of growing a chain.
#[derive(Debug, Default)]
struct RunTaint {
    tags: Vec<TrifectaTag>,
    streams: BTreeSet<String>,
    callers: BTreeSet<String>,
    /// The results of tainted runs it reads back: each edge, as a refusal
    /// names it, and the workflow it reads.
    reads: BTreeSet<(String, String)>,
    /// Where the outside text it carries entered: an outside caller's route
    /// (a direct start, a relayed signal, or an `into:` onto a stream it
    /// reads), or a mirrored stream. A result read back taints its reader
    /// only with an origin the reader does not hold already — a run that
    /// reads back what it handed a child, or what a consumer made of the
    /// text its own route put on a stream, reads back nothing it was not
    /// given.
    origins: BTreeSet<String>,
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
    /// Whether the run is judged: it reads a tainted stream, a run that does
    /// started it, or it reads back a tainted run's result.
    fn checked(&self) -> bool {
        !self.tags.is_empty()
            && !(self.streams.is_empty() && self.callers.is_empty() && self.reads.is_empty())
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
        let reads: Vec<(String, String)> = self
            .reads
            .iter()
            .map(|(via, target)| (target.clone(), format!("at {via}")))
            .collect();
        if !reads.is_empty() {
            why.push(by_edge(&reads));
        }
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

/// The origin an outside caller's text has when it reaches runs through the
/// route of workflow `name` ([`RunTaint::origins`]).
fn route_origin(name: &str) -> String {
    format!("the route of workflow {name:?}")
}

/// Fold every declared producer into each stream's taint, and each tainted
/// stream into the runs that read it, until nothing moves. A fixed point,
/// because an `emit` target and a `workflow` step's target are static (or,
/// when templated, every one there is): a run reading a tainted stream taints
/// what it emits into, and the workflows it starts.
///
/// Twice. A read-back is judged against the origins the reader was HANDED —
/// by its route, the streams it reads and the runs that start it — and those
/// have to be settled before any read is decided: judged mid-pass, a read
/// would see whatever part of them the pass had reached, so the verdict would
/// hang on the order the workflows are listed in (`--validate-config` lists
/// them as declared, a start by name). So the first pass follows the forward
/// edges alone, and the second decides every read against what the first
/// settled. That keeps the second monotone: what a reader was handed is fixed
/// in it, and a target's taint only grows, so a read once new stays new.
fn propagate(
    ctx: &Reach,
    workflows: &[&Workflow],
) -> (BTreeMap<String, StreamTaint>, BTreeMap<String, RunTaint>) {
    let handed = settle(ctx, workflows, None);
    let handed: BTreeMap<String, BTreeSet<String>> = handed
        .1
        .into_iter()
        .map(|(name, r)| (name, r.origins))
        .collect();
    settle(ctx, workflows, Some(&handed))
}

/// One pass of [`propagate`]: the forward edges, and — given what each run
/// was handed — the read-back edges.
fn settle(
    ctx: &Reach,
    workflows: &[&Workflow],
    handed: Option<&BTreeMap<String, BTreeSet<String>>>,
) -> (BTreeMap<String, StreamTaint>, BTreeMap<String, RunTaint>) {
    let untrusted = [TrifectaTag::UntrustedInput];
    let mut streams: BTreeMap<String, StreamTaint> = BTreeMap::new();
    let mut feed = |stream: &str, tags: &[TrifectaTag], producer: String, origin: String| {
        let t = streams.entry(stream.to_string()).or_default();
        union(&mut t.tags, tags);
        t.producers.insert(producer);
        t.origins.insert(origin);
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
                route_origin(&w.name),
            );
        }
    }
    // A mirrored stream carries whatever the child's own producers put on it.
    // Those are compiled from the template at spawn, into another process's
    // configuration — they cannot be derived here, so the mirror counts as
    // untrusted, as the RFC states for a taint that cannot be computed.
    for (name, t) in &ctx.s.subagents.templates {
        for m in t.mirror_streams.iter().flatten() {
            let producer = format!("mirror_streams of subagent template {name:?}");
            feed(m, &untrusted, producer.clone(), producer);
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
            run.origins.insert(route_origin(&w.name));
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
                    run.origins.insert(route_origin(&w.name));
                }
            }
        }
    }
    // What each step hands on and reads back is fixed by the definitions
    // and the settings, so it is worked out once; only the taint moves.
    let edges: Vec<(&Workflow, Vec<StepEdges>)> = workflows
        .iter()
        .map(|w| {
            let steps = all_steps(w)
                .into_iter()
                .map(|st| {
                    let at = format!("workflow {:?} step {:?}", w.name, st.id);
                    let mut e = Edges::default();
                    ctx.step(st, &at, &mut e);
                    (st, at, e)
                })
                .collect();
            (*w, steps)
        })
        .collect();
    let none = BTreeSet::new();
    loop {
        let mut grew = false;
        // The daemon's own telemetry, tapped onto a stream: `run.done`
        // carries every run's error (and its output under
        // `observability.log_content`), a step's or tool's event its text,
        // an audit record the question an `ask_human` put. Which run's text a
        // given event carries is only known when it is logged, so the tap is
        // fed by every run, as a read-back of each.
        for (stream, producer) in &ctx.taps {
            let (mut tags, mut origins) = (Vec::new(), BTreeSet::new());
            for r in runs.values() {
                union(&mut tags, &r.tags);
                origins.extend(r.origins.iter().cloned());
            }
            if tags.is_empty() {
                continue;
            }
            let t = streams.entry(stream.clone()).or_default();
            grew |= union(&mut t.tags, &tags);
            grew |= t.producers.insert(producer.clone());
            for o in origins {
                grew |= t.origins.insert(o);
            }
        }
        for (w, steps) in &edges {
            for (st, _, e) in steps {
                for name in consumes(st, &streams) {
                    let (tags, origins) = (streams[name].tags.clone(), &streams[name].origins);
                    let run = runs.entry(w.name.clone()).or_default();
                    grew |= union(&mut run.tags, &tags);
                    grew |= run.streams.insert(name.to_string());
                    for o in origins {
                        grew |= run.origins.insert(o.clone());
                    }
                }
                let Some(handed) = handed else {
                    continue;
                };
                // A result read back puts its run's text in front of the
                // reader — unless every origin of that text is one the reader
                // was handed already ([`RunTaint::origins`]).
                let mine = handed.get(&w.name).unwrap_or(&none);
                for (target, via) in &e.read {
                    // Another run of its own workflow carries nothing the
                    // reader's own edges do not judge it for already.
                    if *target == w.name {
                        continue;
                    }
                    let Some((tags, origins)) = runs
                        .get(target)
                        .filter(|r| !r.tags.is_empty())
                        .map(|r| (r.tags.clone(), r.origins.clone()))
                    else {
                        continue;
                    };
                    // A tainted run always has an origin; one without would
                    // be a path that lost it, and is read as new, not as
                    // nothing.
                    if !origins.is_empty() && origins.is_subset(mine) {
                        continue;
                    }
                    let run = runs.entry(w.name.clone()).or_default();
                    for o in origins {
                        grew |= run.origins.insert(o);
                    }
                    grew |= union(&mut run.tags, &tags);
                    grew |= run.reads.insert((via.clone(), target.clone()));
                }
            }
        }
        for (w, steps) in &edges {
            let Some((tags, checked, origins)) = runs
                .get(&w.name)
                .filter(|r| !r.tags.is_empty())
                .map(|r| (r.tags.clone(), r.checked(), r.origins.clone()))
            else {
                continue;
            };
            for (st, at, e) in steps {
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
                        for o in &origins {
                            grew |= t.origins.insert(o.clone());
                        }
                    }
                }
                for (target, caller) in &e.started {
                    let run = runs.entry(target.clone()).or_default();
                    grew |= union(&mut run.tags, &tags);
                    for o in &origins {
                        grew |= run.origins.insert(o.clone());
                    }
                    grew |= if checked {
                        run.callers.insert(caller.clone())
                    } else {
                        run.outside.insert(caller.clone())
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

/// A workflow, and the edge that reaches it as a refusal names it.
type Started = Vec<(String, String)>;

/// A step, as a refusal names it, and its [`Edges`].
type StepEdges<'w> = (&'w Step, String, Edges);

/// What one step does with its run's text: the capabilities a model it
/// drives can reach, the workflows it starts, and the workflows whose result
/// it reads back — and whether it hands the text to a child, and may leave
/// that child warm.
#[derive(Debug, Default)]
struct Edges {
    held: Vec<Held>,
    started: Started,
    read: Started,
    spawns: bool,
    warm: bool,
}

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
    /// Each internal contract's grant and family, from the registry's own
    /// table.
    grants: BTreeMap<&'static str, (DefaultGrant, &'static str)>,
    /// The internal contracts that hand text onward, and where
    /// (`registry::internal::onward`).
    onward: Vec<(&'static str, Onward)>,
    /// The internal contracts whose reply carries a run's result, and which
    /// (`registry::internal::read_back`).
    read_back: Vec<(&'static str, ReadBack)>,
    /// The internal contracts mapped onto a server — an override, or a
    /// `knowledge.*` / `search.*` profile — and that server. A call to one is
    /// served by the supervisor whatever the step's own `servers` say.
    mapped: Vec<(String, String)>,
    /// Every workflow in the set, by name.
    names: Vec<String>,
    /// Each workflow tool: its name, its workflow, its grant, and whether
    /// its reply is the run's output (`mode: sync`).
    wf_tools: Vec<(
        String,
        String,
        crate::engine::model::WorkflowToolGrant,
        bool,
    )>,
    /// The workflows a signal reaches: a `signal` start, or a
    /// `wait {on: signal}` anywhere in the run.
    receivers: Vec<String>,
    /// The workflows whose runs hand their text to a child — spawn one or
    /// steer one — so a child's result read back by handle may be theirs;
    /// each with whether the child may be warm, whose every turn leaves a
    /// note in the root transcript.
    spawners: Vec<(String, bool)>,
    /// The streams the daemon's own telemetry is tapped onto
    /// (`observability.runtime_events`, an audit `sink: [stream]`), each as
    /// a refusal names its producer.
    taps: Vec<(String, String)>,
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
        let mut taps = Vec::new();
        if let Some(stream) = s
            .observability
            .runtime_events
            .as_ref()
            .and_then(|r| r.stream.as_ref())
        {
            taps.push((
                stream.clone(),
                "the runtime-events tap (`observability.runtime_events`), which carries \
                 every run's events"
                    .to_string(),
            ));
        }
        if let Some(stream) = &s.observability.audit.stream
            && s.observability
                .audit
                .sink
                .iter()
                .flatten()
                .any(|k| *k == crate::config::settings::AuditSink::Stream)
        {
            taps.push((
                stream.clone(),
                "the audit stream (`observability.audit`), which carries what every run's \
                 tool calls and questions name"
                    .to_string(),
            ));
        }
        let mut reach = Reach {
            s,
            exec: (cfg!(feature = "exec") && s.security.exec.enabled)
                || s.tools.overrides.contains_key("exec"),
            servers,
            templates,
            flat,
            grants: contracts
                .iter()
                .map(|c| (c.name, (c.grant, c.family)))
                .collect(),
            onward: contracts
                .iter()
                .filter_map(|c| crate::registry::internal::onward(c.name).map(|o| (c.name, o)))
                .collect(),
            read_back: contracts
                .iter()
                .filter_map(|c| crate::registry::internal::read_back(c.name).map(|r| (c.name, r)))
                .collect(),
            mapped,
            names: workflows.iter().map(|w| w.name.clone()).collect(),
            wf_tools: workflows
                .iter()
                .filter_map(|w| {
                    w.tool.as_ref().map(|t| {
                        (
                            t.name.clone(),
                            w.name.clone(),
                            t.grant,
                            t.mode == crate::engine::model::WorkflowToolMode::Sync,
                        )
                    })
                })
                .collect(),
            receivers,
            spawners: Vec::new(),
            taps,
        };
        // Whether a run spawns is read off its own edges, which the
        // spawners themselves do not feed.
        reach.spawners = workflows
            .iter()
            .filter_map(|w| {
                let mut e = Edges::default();
                for st in all_steps(w) {
                    reach.step(st, "", &mut e);
                }
                e.spawns.then(|| (w.name.clone(), e.warm))
            })
            .collect();
        reach
    }

    fn all(&self) -> Vec<Held> {
        self.servers.iter().map(|(_, h)| h.clone()).collect()
    }

    fn disabled(&self, tool: &str) -> bool {
        self.s.tools.disabled.iter().any(|d| d == tool)
    }

    /// The contracts through which the root reads a result back, as its
    /// refusal names them: the read-back contracts, and the ones that reach
    /// a child, which can read one back for it.
    fn root_read_tools(&self) -> Vec<&'static str> {
        let mut names: Vec<&'static str> = self.read_back.iter().map(|(n, _)| *n).collect();
        for (n, o) in &self.onward {
            if *o == Onward::Children && !names.contains(n) {
                names.push(n);
            }
        }
        names
    }

    /// The tools a refusal advises leaving out of a `tools` list: the ones
    /// that hand text on, and the ones that read a result back.
    fn advice(&self) -> String {
        let mut names: Vec<&str> = self.onward.iter().map(|(n, _)| *n).collect();
        for (n, _) in &self.read_back {
            if !names.contains(n) {
                names.push(n);
            }
        }
        names.join(", ")
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

    /// Whether the root conversation holds the internal contract `tool`:
    /// granted to the root, not disabled, and in `agent.tools.internal` —
    /// the root's plan filters by it, by name or family.
    fn root_holds(&self, tool: &str) -> bool {
        let select = &self.s.agent.tools.internal;
        self.grants.get(tool).is_some_and(|(grant, family)| {
            grant.root && !self.disabled(tool) && (select.allows(tool) || select.allows(family))
        })
    }

    /// The workflows whose result the root conversation reads back, each
    /// with the edge: a read-back contract the root holds, a child it spawns
    /// or steers that can read one back (its result is the root's to read),
    /// a sync workflow tool granted to the root, the note
    /// `agent.on_workflow_finished` leaves in its transcript, which carries
    /// the run's output or error, and the note a child's result or warm turn
    /// leaves there.
    fn root_reads(&self) -> Started {
        let mut e = Edges::default();
        for (tool, read) in &self.read_back {
            if self.root_holds(tool) {
                self.read_back_of(*read, None, format!("through `{tool}`"), &mut e);
            }
        }
        if let Some((tool, _)) = self
            .onward
            .iter()
            .find(|(t, o)| *o == Onward::Children && self.root_holds(t))
        {
            let mut child = Edges::default();
            self.any_spawn(
                &format!("by a child it reaches through `{tool}`"),
                &mut child,
            );
            e.read.extend(child.read);
        }
        let mut out = e.read;
        for (tool, target, grant, sync) in &self.wf_tools {
            if *sync && grant.root && !self.disabled(tool) {
                out.push((target.clone(), format!("through its tool {tool:?}")));
            }
        }
        use crate::config::settings::{OnWorkflowFinished, WakeEvent};
        let wake = self.s.agent.wake_on();
        if self.s.agent.on_workflow_finished != OnWorkflowFinished::Ignore
            && (wake.contains(&WakeEvent::WorkflowFinished)
                || wake.contains(&WakeEvent::WorkflowFailed))
        {
            out.extend(self.names.iter().map(|n| {
                (
                    n.clone(),
                    "the note `agent.on_workflow_finished` writes into its transcript".to_string(),
                )
            }));
        }
        // A child's result is noted only under `subagent_result`; a warm
        // child's every turn is noted whatever the wake policy says.
        let results = wake.contains(&WakeEvent::SubagentResult);
        out.extend(
            self.spawners
                .iter()
                .filter(|(_, warm)| results || *warm)
                .map(|(n, _)| {
                    (
                        n.clone(),
                        "the note a child's result leaves in its transcript".to_string(),
                    )
                }),
        );
        out
    }

    /// The tags the registry gives an internal contract — `exec` both legs, a
    /// mapped one its server's, plus what `tools.narrow` adds — which a policy
    /// rule matching on tags reads.
    fn tool_tags(&self, tool: &str) -> Vec<TrifectaTag> {
        let mut tags = Vec::new();
        // A read-back contract carries the taint of every run whose result
        // it may return (`registry::register_workflow_tools`). That is only
        // known once this check has run; taken as untrusted here, it is the
        // same answer whenever it matters — a read-back adds a refusal only
        // when some run is tainted, and then the registry tags it so.
        if crate::registry::internal::read_back(tool).is_some() {
            union(&mut tags, &[TrifectaTag::UntrustedInput]);
        }
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
        let grant = self.grants.get(tool).map(|(g, _)| g);
        let (allow, granted) = match who {
            Who::Step(allow) => (allow, grant.is_none_or(|g| g.workflows)),
            Who::Child(allow) => (allow, grant.is_none_or(|g| g.subagents)),
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
    fn any_spawn(&self, at: &str, e: &mut Edges) {
        // A model picks the mode, and a steered child is warm already.
        e.spawns = true;
        e.warm = true;
        if self.s.subagents.allow_freeform != Some(false) {
            e.held.extend(self.all());
            self.tools(Who::Child(None), at, e);
        }
        e.held.extend(self.templates.values().flatten().cloned());
        for tools in self.flat.values() {
            self.tools(Who::Child(tools.as_deref()), at, e);
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
        e: &mut Edges,
    ) {
        match onward {
            Onward::Root => e.held.extend(self.root()),
            Onward::Children => match args.filter(|_| tool == "subagent.run") {
                Some(args) => self.spawn(args, &via, e),
                None => self.any_spawn(&via, e),
            },
            Onward::Workflows => e.started.extend(self.workflows_named(args, "name", &via)),
            Onward::Signal => e
                .started
                .extend(self.receivers.iter().map(|n| (n.clone(), via.clone()))),
        }
    }

    /// Whose result a contract that reads one back hands its caller: `args`
    /// are a deterministic call's own, absent when a model picks them — and a
    /// model can pick `wait: true` and any run.
    fn read_back_of(
        &self,
        read: ReadBack,
        args: Option<&Map<String, Value>>,
        via: String,
        e: &mut Edges,
    ) {
        match read {
            // Only a call that waits is handed the output; one that does
            // not gets the run id, and a later read is its own edge.
            ReadBack::Started => {
                let waits = args.is_none_or(|a| match a.get("wait") {
                    Some(Value::Bool(b)) => *b,
                    Some(Value::String(t)) => templated(t),
                    _ => false,
                });
                if waits {
                    e.read.extend(self.workflows_named(args, "name", &via));
                }
            }
            // A run id names no workflow until the run exists.
            ReadBack::Run => e
                .read
                .extend(self.names.iter().map(|n| (n.clone(), via.clone()))),
            ReadBack::Runs => match args.and_then(|a| a.get("run")) {
                Some(_) => e
                    .read
                    .extend(self.names.iter().map(|n| (n.clone(), via.clone()))),
                None => e.read.extend(self.workflows_named(args, "name", &via)),
            },
            // A handle names no workflow either: it may be any spawner's
            // child.
            ReadBack::Child => e
                .read
                .extend(self.spawners.iter().map(|(n, _)| (n.clone(), via.clone()))),
            // What the binding names: a model may bind either; a
            // deterministic call binds what its `bind` says, and a templated
            // one may say either.
            ReadBack::Bound => {
                let bind = match args {
                    None => None,
                    Some(a) => match a.get("bind") {
                        None => return,
                        Some(b) => b.as_object(),
                    },
                };
                if bind.is_none_or(|b| b.contains_key("run")) {
                    self.read_back_of(ReadBack::Run, None, via.clone(), e);
                }
                if bind.is_none_or(|b| b.contains_key("subagent")) {
                    self.read_back_of(ReadBack::Child, None, via, e);
                }
            }
        }
    }

    /// The workflow a deterministic call's `field` names, or every workflow
    /// when a model picks it or the name is templated.
    fn workflows_named(
        &self,
        args: Option<&Map<String, Value>>,
        field: &str,
        via: &str,
    ) -> Started {
        match args
            .and_then(|a| a.get(field))
            .and_then(Value::as_str)
            .filter(|n| !templated(n))
        {
            Some(n) => vec![(n.to_string(), via.to_string())],
            None => self
                .names
                .iter()
                .map(|n| (n.clone(), via.to_string()))
                .collect(),
        }
    }

    /// The tools `who` can call that reach past its own servers: the ones
    /// that hand text onward, the ones that read a run's result back, `exec`,
    /// any internal tool mapped onto a server, and the workflow tools, each
    /// of which starts a run (and, in `mode: sync`, hands back its output).
    fn tools(&self, who: Who, at: &str, e: &mut Edges) {
        for (tool, onward) in &self.onward {
            if self.callable(who, tool) {
                self.hand_on(tool, *onward, None, format!("{at} (through {tool})"), e);
            }
        }
        for (tool, read) in &self.read_back {
            if self.callable(who, tool) {
                self.read_back_of(*read, None, format!("{at} (through {tool})"), e);
            }
        }
        if self.exec && self.callable(who, "exec") {
            e.held.push(exec_held());
        }
        for (tool, server) in &self.mapped {
            if self.callable(who, tool) {
                e.held
                    .extend(named(&self.servers, std::slice::from_ref(server)));
            }
        }
        for (tool, target, grant, sync) in &self.wf_tools {
            let granted = match who {
                Who::Step(_) => grant.workflows,
                Who::Child(_) => grant.subagents && self.gated(tool, &ANY_TAG),
            };
            if granted && self.callable(who, tool) {
                let via = format!("{at} (through its tool {tool:?})");
                if *sync {
                    e.read.push((target.clone(), via.clone()));
                }
                e.started.push((target.clone(), via));
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
    fn spawn(&self, spec: &Map<String, Value>, at: &str, e: &mut Edges) {
        e.spawns = true;
        e.warm |= spec
            .get("mode")
            .and_then(Value::as_str)
            .is_some_and(|m| m == "warm" || templated(m));
        match spec.get("template").and_then(Value::as_str) {
            Some(t) => match self.templates.get(t) {
                Some(reach) => {
                    e.held.extend(reach.iter().cloned());
                    if let Some(tools) = self.flat.get(t) {
                        self.tools(Who::Child(tools.as_deref()), at, e);
                    }
                }
                None => self.any_spawn(at, e),
            },
            None => {
                e.held.extend(self.listed_or_all(spec));
                let allow = allow_list(spec);
                self.tools(Who::Child(allow.as_deref()), at, e);
            }
        }
    }

    /// What one step hands its run's text to — the capabilities a model it
    /// drives can reach, and the workflows it starts — and the workflows
    /// whose result it reads back. `think` and the other single-call kinds
    /// hold no tools; a deterministic `mcp.tool` or `http` step is not
    /// steered by the text it carries.
    fn step(&self, st: &Step, at: &str, e: &mut Edges) {
        match st.kind.as_str() {
            "agent" => {
                e.held.extend(self.listed_or_all(&st.spec));
                let allow = allow_list(&st.spec);
                self.tools(Who::Step(allow.as_deref()), at, e);
            }
            "subagent" => self.spawn(&st.spec, at, e),
            // A deterministic call of a tool that hands its arguments to a
            // model is that hand-off, spelled as a step; one whose reply is a
            // run's result is that read, spelled as a step.
            "tool" => {
                if let Some(name) = st.field_str("name") {
                    let args = st.field("args").and_then(Value::as_object);
                    if let Some(onward) = crate::registry::internal::onward(name) {
                        self.hand_on(name, onward, args, at.to_string(), e);
                    }
                    if let Some(read) = crate::registry::internal::read_back(name) {
                        self.read_back_of(read, args, at.to_string(), e);
                    }
                }
            }
            // A `message` hands the text to the agent's own conversation,
            // and the agent turning it holds the root grant.
            "message" => e.held.extend(self.root()),
            // A templated name can start any of them. Unless it is detached,
            // the step's output is the child run's: `sync` waits for it, and
            // `async` hands a handle a `join` reads it through.
            "workflow" => {
                let started = match st.field_str("name") {
                    Some(n) if !templated(n) => vec![(n.to_string(), at.to_string())],
                    _ => self
                        .names
                        .iter()
                        .map(|n| (n.clone(), at.to_string()))
                        .collect(),
                };
                if st.field_str("mode") != Some("detached") {
                    e.read.extend(started.iter().cloned());
                }
                e.started.extend(started);
            }
            // A run waited on by id hands the step its output, and so does
            // the deterministic `workflow.wait` — whichever run the id names.
            "wait" if st.field_str("on") == Some("run") => {
                self.read_back_of(ReadBack::Run, None, at.to_string(), e)
            }
            "workflow.wait" => self.read_back_of(ReadBack::Run, None, at.to_string(), e),
            // A child waited on by handle hands the step its result.
            "wait" if st.field_str("on") == Some("subagent") => {
                self.read_back_of(ReadBack::Child, None, at.to_string(), e)
            }
            // A `join` hands back each handle's output: a run's (an async
            // `workflow` step's, or one `workflow.run` started) or a child's.
            "join" => {
                self.read_back_of(ReadBack::Run, None, at.to_string(), e);
                self.read_back_of(ReadBack::Child, None, at.to_string(), e);
            }
            _ => {}
        }
    }

    /// Everything the model-driven steps of `w` can reach.
    fn workflow(&self, w: &Workflow) -> Vec<Held> {
        let mut e = Edges::default();
        for st in all_steps(w) {
            self.step(st, "", &mut e);
        }
        let mut seen = BTreeSet::new();
        e.held.retain(|h| seen.insert(h.label.clone()));
        e.held
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

    /// [`check`]'s refusals of workflows alone. A step that can spawn may
    /// leave a child warm, whose every turn is noted to the root — so beside
    /// such a step the root, holding `mail`, is refused as well; the tests
    /// that judge the workflow count only its own.
    fn wf_check(s: &Settings, ws: &[Workflow]) -> Vec<String> {
        check(s, ws)
            .into_iter()
            .filter(|e| e.starts_with("workflow "))
            .collect()
    }

    /// `mail` holds both legs the root fold allows together; `notes` is
    /// sensitive alone. The root reads no run's or child's result back —
    /// with `mail` in its grant it would otherwise be refused beside every
    /// workflow these tests judge; [`root`] gives it the read-back to test
    /// that.
    fn servers(extra: Value) -> Settings {
        let mut v = json!({
            "agent": {"on_workflow_finished": "ignore", "tools": {"internal": "none"},
                      "wake_on": ["human_reply"]},
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
        let errs = wf_check(&servers(json!({})), &[intake("webhook"), w.clone()]);
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
        // `send` workflow's tool, whose run hands an agent `mail`. Async, so
        // nobody reads its run back: the edge judged here is the start.
        let triage = wf(json!({"name": "triage", "steps": {
            "s": {"kind": "stream", "stream": "inbox"},
            "a": {"kind": "agent", "depends_on": ["s"], "instruction": "x",
                  "servers": ["notes"], "tools": ["notes.*", "mail.send"]},
            "f": {"kind": "finish", "depends_on": ["a"], "status": "completed"}
        }}));
        let send = wf(
            json!({"name": "send", "tool": {"name": "mail.send", "mode": "async"}, "steps": {
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
            wf_check(&s, &[intake("webhook"), reader(&["subagent.send"])]).len(),
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
            wf_check(
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
            "a": {"kind": "agent", "depends_on": ["s"], "instruction": "x", "servers": ["mail"],
                  "tools": ["mail.*"]},
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
                json!({"name": "send", "tool": {"name": "mail.send", "mode": "async", "grant": grant}, "steps": {
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
            // What the root reads back.
            json!({"agent": {"on_workflow_finished": "note", "tools": {"internal": "none"}}}),
            json!({"agent": {"on_workflow_finished": "ignore", "tools": {"internal": "all"}}}),
            json!({"agent": {"on_workflow_finished": "ignore", "tools": {"internal": "none"},
                             "wake_on": ["workflow_finished"]}}),
            // The streams the daemon's telemetry is tapped onto.
            json!({"observability": {"runtime_events": {"stream": "work", "include": ["run"]}}}),
            json!({"observability": {"audit": {"sink": ["stream"], "stream": "work"}}}),
        ] {
            assert!(inputs_moved(&base, &servers(change.clone())), "{change}");
        }
    }

    // ---- results read back (RFC 0045 §5.11.3, "follow it") ----------------

    /// `fetch` reads the tainted `inbox` with an agent holding only `notes`
    /// — two legs, allowed. What it returns is that text, worked over.
    fn fetch(tool: Value) -> Workflow {
        let mut v = json!({"name": "fetch", "steps": {
            "s": {"kind": "stream", "stream": "inbox"},
            "a": {"kind": "agent", "depends_on": ["s"], "instruction": "x",
                  "servers": ["notes"], "tools": ["notes.*"]},
            "f": {"kind": "finish", "depends_on": ["a"], "status": "completed"}
        }});
        if !tool.is_null() {
            v["tool"] = tool;
        }
        wf(v)
    }

    /// A manually started workflow whose `read` step (one or more steps,
    /// given whole) runs before an agent holding `mail` — both other legs.
    fn caller(read: Value) -> Workflow {
        let mut steps = json!({
            "s": {"kind": "manual"},
            "a": {"kind": "agent", "depends_on": ["r"], "instruction": "x",
                  "servers": ["mail"], "tools": ["mail.*"]},
            "f": {"kind": "finish", "depends_on": ["a"], "status": "completed"}
        });
        let mut read = read;
        read["depends_on"] = json!(["s"]);
        steps["r"] = read;
        wf(json!({"name": "caller", "steps": steps}))
    }

    /// The refusals for `caller` alone, each checked to name the read-back.
    fn caller_refused(s: &Settings, ws: &[Workflow], edge: &str) -> bool {
        let errs = check(s, ws);
        let mine: Vec<&String> = errs
            .iter()
            .filter(|e| e.starts_with("workflow \"caller\""))
            .collect();
        for e in &mine {
            // One edge may read several workflows back, named together.
            let read = e
                .split("it reads back the result of workflow")
                .skip(1)
                .any(|r| {
                    r.split(" (")
                        .next()
                        .is_some_and(|t| t.contains("\"fetch\""))
                });
            assert!(read, "missing the read of \"fetch\" in {e}");
            for want in [edge, "mcp server \"mail\" [sensitive, egress]"] {
                assert!(e.contains(want), "missing {want:?} in {e}");
            }
        }
        !mine.is_empty()
    }

    #[test]
    fn a_sync_workflow_tools_reply_taints_its_caller_and_an_async_one_does_not() {
        let s = servers(json!({}));
        let agent = |tool: &str| {
            caller(json!({"kind": "agent", "instruction": "x", "servers": [], "tools": [tool]}))
        };
        assert!(caller_refused(
            &s,
            &[
                intake("webhook"),
                fetch(json!({"name": "inbox.next"})),
                agent("inbox.next")
            ],
            "(through its tool \"inbox.next\")"
        ));
        assert!(!caller_refused(
            &s,
            &[
                intake("webhook"),
                fetch(json!({"name": "inbox.next", "mode": "async"})),
                agent("inbox.next")
            ],
            ""
        ));
    }

    #[test]
    fn workflow_run_reads_back_only_when_it_waits() {
        let s = servers(json!({}));
        let run =
            |args: Value| caller(json!({"kind": "tool", "name": "workflow.run", "args": args}));
        let refused = |args: Value| {
            caller_refused(
                &s,
                &[intake("webhook"), fetch(Value::Null), run(args)],
                "at workflow \"caller\" step \"r\"",
            )
        };
        assert!(refused(json!({"name": "fetch", "wait": true})));
        assert!(refused(json!({"name": "{{inputs.which}}", "wait": true})));
        assert!(refused(json!({"name": "fetch", "wait": "{{inputs.wait}}"})));
        assert!(!refused(json!({"name": "fetch"})));
        assert!(!refused(json!({"name": "fetch", "wait": false})));
        // A model holding `workflow.run` can ask to wait for any workflow.
        assert!(caller_refused(
            &s,
            &[
                intake("webhook"),
                fetch(Value::Null),
                caller(json!({"kind": "agent", "instruction": "x", "servers": [],
                              "tools": ["workflow.run"]}))
            ],
            "(through workflow.run)"
        ));
    }

    #[test]
    fn workflow_wait_and_status_read_back_any_run() {
        let s = servers(json!({}));
        let refused = |read: Value, edge: &str| {
            caller_refused(
                &s,
                &[intake("webhook"), fetch(Value::Null), caller(read)],
                edge,
            )
        };
        let model = |tool: &str| json!({"kind": "agent", "instruction": "x", "servers": [], "tools": [tool]});
        assert!(refused(model("workflow.wait"), "(through workflow.wait)"));
        assert!(refused(
            model("workflow.status"),
            "(through workflow.status)"
        ));
        // The deterministic spellings: a run id names any workflow's run…
        let step = "at workflow \"caller\" step \"r\"";
        assert!(refused(
            json!({"kind": "tool", "name": "workflow.wait", "args": {"run": "{{inputs.run}}"}}),
            step
        ));
        assert!(refused(
            json!({"kind": "workflow.wait", "run": "{{inputs.run}}"}),
            step
        ));
        assert!(refused(
            json!({"kind": "wait", "on": "run", "run": "{{inputs.run}}"}),
            step
        ));
        assert!(refused(
            json!({"kind": "tool", "name": "workflow.status", "args": {"run": "fetch-1"}}),
            step
        ));
        // …and `workflow.status` by name reads only that workflow's runs.
        assert!(refused(
            json!({"kind": "tool", "name": "workflow.status", "args": {"name": "fetch"}}),
            step
        ));
        assert!(!refused(
            json!({"kind": "tool", "name": "workflow.status", "args": {"name": "caller"}}),
            step
        ));
    }

    #[test]
    fn a_workflow_step_reads_its_child_back_unless_detached() {
        let s = servers(json!({}));
        let step = |mode: &str| caller(json!({"kind": "workflow", "name": "fetch", "mode": mode}));
        let edge = "at workflow \"caller\" step \"r\"";
        for mode in ["sync", "async"] {
            assert!(
                caller_refused(
                    &s,
                    &[intake("webhook"), fetch(Value::Null), step(mode)],
                    edge
                ),
                "{mode}"
            );
        }
        assert!(!caller_refused(
            &s,
            &[intake("webhook"), fetch(Value::Null), step("detached")],
            edge
        ));
    }

    /// The read-back chain the check refuses loads under the explicit
    /// allow, which lifts it with every other trifecta gate.
    #[test]
    fn the_explicit_allow_lifts_the_read_back_chain() {
        let ws = [
            intake("webhook"),
            fetch(json!({"name": "inbox.next"})),
            caller(
                json!({"kind": "agent", "instruction": "x", "servers": [], "tools": ["inbox.next"]}),
            ),
        ];
        assert!(!check(&servers(json!({})), &ws).is_empty());
        assert!(check(&servers(json!({"security": {"allow_trifecta": true}})), &ws).is_empty());
    }

    /// What a route's own run hands a child and reads back is the text it
    /// was handed already — the operator's call, as the route's own run is.
    /// What another route's text became is new to it, and judged.
    #[test]
    fn a_read_back_taints_only_with_text_the_reader_was_not_handed() {
        let s = servers(json!({}));
        let route = |read: Value| {
            wf(json!({"name": "hook", "steps": {
                "s": {"kind": "webhook", "path": "/hook"},
                "r": read,
                "a": {"kind": "agent", "depends_on": ["r"], "instruction": "x",
                      "servers": ["mail"], "tools": ["mail.*"]},
                "f": {"kind": "finish", "depends_on": ["a"], "status": "completed"}
            }}))
        };
        let child = wf(json!({"name": "child", "steps": {
            "s": {"kind": "manual"},
            "a": {"kind": "agent", "depends_on": ["s"], "instruction": "x",
                  "servers": ["notes"], "tools": ["notes.*"]},
            "f": {"kind": "finish", "depends_on": ["a"], "status": "completed"}
        }}));
        let own = route(json!({"kind": "workflow", "depends_on": ["s"], "name": "child"}));
        assert!(check(&s, &[own, child]).is_empty());
        // `fetch` reads what ANOTHER route put on `inbox`.
        let other = route(json!({"kind": "workflow", "depends_on": ["s"], "name": "fetch"}));
        let errs = check(&s, &[intake("webhook"), other, fetch(Value::Null)]);
        assert_eq!(errs.len(), 1, "{errs:?}");
        assert!(
            errs[0].starts_with("workflow \"hook\"")
                && errs[0].contains("it reads back the result of workflow \"fetch\""),
            "{}",
            errs[0]
        );
        // …while what a consumer made of the text the route ITSELF put on
        // the stream is not. Read by status, so the stream is the only path
        // the route's text takes to `fetch`.
        let into = wf(json!({"name": "hook", "steps": {
            "s": {"kind": "webhook", "path": "/hook"},
            "e": {"kind": "emit", "depends_on": ["s"], "stream": "inbox", "subject": "m"},
            "r": {"kind": "tool", "depends_on": ["e"], "name": "workflow.status",
                  "args": {"name": "fetch"}},
            "a": {"kind": "agent", "depends_on": ["r"], "instruction": "x",
                  "servers": ["mail"], "tools": ["mail.*"]},
            "f": {"kind": "finish", "depends_on": ["a"], "status": "completed"}
        }}));
        assert!(check(&s, &[into, fetch(Value::Null)]).is_empty());
        // The same holds for a workflow's `into:` start beside its own
        // direct one: both are the route of that workflow.
        let both = wf(json!({"name": "hook", "steps": {
            "in": {"kind": "webhook", "path": "/hook/in", "into": {"stream": "inbox", "subject": "m"}},
            "s": {"kind": "webhook", "path": "/hook"},
            "r": {"kind": "tool", "depends_on": ["s"], "name": "workflow.status",
                  "args": {"name": "fetch"}},
            "a": {"kind": "agent", "depends_on": ["r"], "instruction": "x",
                  "servers": ["mail"], "tools": ["mail.*"]},
            "f": {"kind": "finish", "depends_on": ["a"], "status": "completed"}
        }}));
        assert!(check(&s, &[both, fetch(Value::Null)]).is_empty());
    }

    /// The root conversation is a caller too, judged with everything the
    /// root grant reaches — here `mail`.
    fn root(agent: Value) -> Settings {
        let mut s = servers(json!({}));
        s.agent = serde_json::from_value(agent).expect("agent");
        s
    }

    fn root_refused(s: &Settings, ws: &[Workflow], edge: &str) -> bool {
        let errs = check(s, ws);
        let mine: Vec<&String> = errs
            .iter()
            .filter(|e| e.starts_with("the root conversation"))
            .collect();
        for e in &mine {
            for want in [
                "it reads back the result of workflow \"fetch\"",
                edge,
                "mcp server \"mail\" [sensitive, egress]",
            ] {
                assert!(e.contains(want), "missing {want:?} in {e}");
            }
        }
        !mine.is_empty()
    }

    #[test]
    fn the_root_reading_a_tainted_result_back_is_judged_through_each_edge() {
        let ws = [intake("webhook"), fetch(Value::Null)];
        let quiet = json!({"on_workflow_finished": "ignore", "tools": {"internal": "none"}});
        assert!(!root_refused(&root(quiet.clone()), &ws, ""));
        // The note a finished — or, by default, failed — run leaves.
        assert!(root_refused(
            &root(json!({"tools": {"internal": "none"}})),
            &ws,
            "(the note `agent.on_workflow_finished` writes into its transcript)"
        ));
        assert!(!root_refused(
            &root(json!({"tools": {"internal": "none"}, "wake_on": ["a2a_message"]})),
            &ws,
            ""
        ));
        // Each read-back contract the root's plan holds.
        for tool in ["workflow.run", "workflow.wait", "workflow.status"] {
            let agent = json!({"on_workflow_finished": "ignore", "tools": {"internal": [tool]}});
            assert!(
                root_refused(&root(agent.clone()), &ws, &format!("(through `{tool}`)")),
                "{tool}"
            );
            // Selected by its family as well…
            let family =
                json!({"on_workflow_finished": "ignore", "tools": {"internal": ["workflow"]}});
            assert!(root_refused(
                &root(family),
                &ws,
                &format!("(through `{tool}`)")
            ));
            // …and gone once disabled.
            let mut s = root(agent);
            s.tools.disabled = vec![tool.to_string()];
            assert!(!root_refused(&s, &ws, ""), "{tool}");
        }
        // A sync workflow tool granted to the root.
        let tool = |t: Value| [intake("webhook"), fetch(t)];
        assert!(root_refused(
            &root(quiet.clone()),
            &tool(json!({"name": "inbox.next"})),
            "(through its tool \"inbox.next\")"
        ));
        assert!(!root_refused(
            &root(quiet.clone()),
            &tool(json!({"name": "inbox.next", "grant": {"root": false}})),
            ""
        ));
        assert!(!root_refused(
            &root(quiet),
            &tool(json!({"name": "inbox.next", "mode": "async"})),
            ""
        ));
    }

    #[test]
    fn the_root_holding_one_leg_less_is_not_refused() {
        let mut s = root(json!({}));
        s.mcp.servers.retain(|m| m.name == "notes");
        assert!(check(&s, &[intake("webhook"), fetch(Value::Null)]).is_empty());
    }

    // ---- children, plans, joins and taps (the read-backs by handle) -------

    /// `fetch`, handing what it read to a child: what the child returns is
    /// that text, so a child read back by handle may be `fetch`'s result.
    fn spawner(mode: &str) -> Workflow {
        wf(json!({"name": "fetch", "steps": {
            "s": {"kind": "stream", "stream": "inbox"},
            "c": {"kind": "subagent", "depends_on": ["s"], "instruction": "x",
                  "servers": ["notes"], "tools": ["notes.*"], "mode": mode},
            "f": {"kind": "finish", "depends_on": ["c"], "status": "completed"}
        }}))
    }

    fn model(tool: &str) -> Value {
        json!({"kind": "agent", "instruction": "x", "servers": [], "tools": [tool]})
    }

    #[test]
    fn a_join_reads_back_any_run_or_child() {
        let s = servers(json!({}));
        // `workflow.run` without `wait` hands back a run id, which is no
        // read; the `join` on that id is.
        let joined = |join: bool| {
            let mut steps = json!({
                "s": {"kind": "manual"},
                "r0": {"kind": "tool", "depends_on": ["s"], "name": "workflow.run",
                       "args": {"name": "fetch"}},
                "r": {"kind": "join", "depends_on": ["r0"],
                      "handles": ["{{steps.r0.output.run}}"]},
                "a": {"kind": "agent", "depends_on": ["r"], "instruction": "x",
                      "servers": ["mail"], "tools": ["mail.*"]},
                "f": {"kind": "finish", "depends_on": ["a"], "status": "completed"}
            });
            if !join {
                steps["r"] = json!({"kind": "sleep", "depends_on": ["r0"], "duration": "1s"});
            }
            wf(json!({"name": "caller", "steps": steps}))
        };
        let edge = "at workflow \"caller\" step \"r\"";
        assert!(caller_refused(
            &s,
            &[intake("webhook"), fetch(Value::Null), joined(true)],
            edge
        ));
        assert!(!caller_refused(
            &s,
            &[intake("webhook"), fetch(Value::Null), joined(false)],
            ""
        ));
        // A handle may be a child's as well.
        assert!(caller_refused(
            &s,
            &[intake("webhook"), spawner("async"), joined(true)],
            edge
        ));
    }

    #[test]
    fn a_child_read_back_by_handle_is_its_spawners_result() {
        let s = servers(json!({}));
        let refused = |ws: &[Workflow], edge: &str| caller_refused(&s, ws, edge);
        let with = |read: Value| [intake("webhook"), spawner("async"), caller(read)];
        for tool in ["subagent.status", "subagent.await", "subagent.list"] {
            assert!(
                refused(&with(model(tool)), &format!("(through {tool})")),
                "{tool}"
            );
            // A run that spawns nothing has no child to read.
            assert!(
                !refused(
                    &[intake("webhook"), fetch(Value::Null), caller(model(tool))],
                    ""
                ),
                "{tool}"
            );
        }
        let step = "at workflow \"caller\" step \"r\"";
        assert!(refused(
            &with(json!({"kind": "tool", "name": "subagent.await",
                         "args": {"handle": "{{inputs.h}}"}})),
            step
        ));
        assert!(refused(
            &with(json!({"kind": "wait", "on": "subagent", "subagent": "{{inputs.h}}"})),
            step
        ));
    }

    #[test]
    fn a_plan_binding_reads_back_what_it_binds() {
        let s = servers(json!({}));
        let refused = |ws: &[Workflow], edge: &str| caller_refused(&s, ws, edge);
        let bind = |bind: Value| {
            caller(json!({"kind": "tool", "name": "plan.update",
                          "args": {"item": 1, "bind": bind}}))
        };
        // A model may bind any run, or any child.
        let plan = "(through plan.update)";
        assert!(refused(
            &[
                intake("webhook"),
                fetch(Value::Null),
                caller(model("plan.update"))
            ],
            plan
        ));
        assert!(refused(
            &[
                intake("webhook"),
                spawner("async"),
                caller(model("plan.update"))
            ],
            plan
        ));
        // A deterministic call binds what it says.
        let step = "at workflow \"caller\" step \"r\"";
        assert!(refused(
            &[
                intake("webhook"),
                fetch(Value::Null),
                bind(json!({"run": "{{inputs.run}}"}))
            ],
            step
        ));
        assert!(!refused(
            &[
                intake("webhook"),
                fetch(Value::Null),
                bind(json!({"subagent": "{{inputs.h}}"}))
            ],
            ""
        ));
        assert!(refused(
            &[
                intake("webhook"),
                spawner("async"),
                bind(json!({"subagent": "{{inputs.h}}"}))
            ],
            step
        ));
        assert!(!refused(
            &[
                intake("webhook"),
                fetch(Value::Null),
                caller(json!({"kind": "tool", "name": "plan.update",
                              "args": {"item": 1, "status": "done"}}))
            ],
            ""
        ));
    }

    /// A flat child is offered the internal tools a policy rule might touch,
    /// and a read-back contract carries the run taint in the registry — so a
    /// rule on `untrusted_input` hands the child `workflow.status`, and what
    /// it reads back is its spawner's to act on.
    #[test]
    fn a_policy_on_the_run_taint_hands_a_child_the_read_back() {
        let spawn = wf(json!({"name": "caller", "steps": {
            "s": {"kind": "manual"},
            "r": {"kind": "subagent", "depends_on": ["s"], "template": "reader"},
            "a": {"kind": "agent", "depends_on": ["r"], "instruction": "x",
                  "servers": ["mail"], "tools": ["mail.*"]},
            "f": {"kind": "finish", "depends_on": ["a"], "status": "completed"}
        }}));
        let with = |policy: Value| {
            let mut s = servers(json!({"security": {"policies": [
                {"match": policy, "action": "allow"}
            ]}}));
            s.subagents.templates.insert(
                "reader".into(),
                serde_json::from_value(json!({"instruction": "x", "servers": ["notes"]})).unwrap(),
            );
            s
        };
        let ws = [intake("webhook"), fetch(Value::Null), spawn];
        assert!(caller_refused(
            &with(json!({"tags": ["untrusted_input"]})),
            &ws,
            "(through workflow.status)"
        ));
        assert!(!caller_refused(&with(json!({"tags": ["egress"]})), &ws, ""));
    }

    #[test]
    fn the_root_reads_back_through_plans_children_and_their_notes() {
        let ws = [intake("webhook"), fetch(Value::Null)];
        let spawning = [intake("webhook"), spawner("async")];
        let quiet = |internal: Value| {
            root(
                json!({"on_workflow_finished": "ignore", "wake_on": ["human_reply"],
                        "tools": {"internal": internal}}),
            )
        };
        // The plan binding, by name or by family.
        for select in [json!(["plan.update"]), json!(["plan"])] {
            assert!(root_refused(&quiet(select), &ws, "(through `plan.update`)"));
        }
        let mut s = quiet(json!(["plan"]));
        s.tools.disabled = vec!["plan.update".into()];
        assert!(!root_refused(&s, &ws, ""));
        // A child read back by handle.
        assert!(root_refused(
            &quiet(json!(["subagent.status"])),
            &spawning,
            "(through `subagent.status`)"
        ));
        // A child the root spawns reads back what a rule hands it.
        let mut s = quiet(json!(["subagent.run"]));
        assert!(!root_refused(&s, &ws, ""));
        s.security.policies = serde_json::from_value(
            json!([{"match": {"tool": "workflow.status"}, "action": "allow"}]),
        )
        .unwrap();
        assert!(root_refused(
            &s,
            &ws,
            "(by a child it reaches through `subagent.run` (through workflow.status))"
        ));
        // The note a child's result leaves: under `subagent_result` for any
        // child, and for a warm one whatever the wake policy says.
        let note = "(the note a child's result leaves in its transcript)";
        let wake = |on: Value| {
            root(json!({"on_workflow_finished": "ignore", "wake_on": on,
                        "tools": {"internal": "none"}}))
        };
        assert!(root_refused(
            &wake(json!(["subagent_result"])),
            &spawning,
            note
        ));
        assert!(!root_refused(&wake(json!(["human_reply"])), &spawning, ""));
        assert!(root_refused(
            &wake(json!(["human_reply"])),
            &[intake("webhook"), spawner("warm")],
            note
        ));
    }

    /// The verdict is the same whatever order the workflows are listed in:
    /// each read-back is decided against what the reader was handed once
    /// that has settled. A route's own descendant reading back another of
    /// its descendants — or the route's own run — reads nothing new.
    #[test]
    fn a_read_back_verdict_does_not_hang_on_the_order() {
        let s = servers(json!({}));
        let start =
            |name: &str, mode: &str| json!({"kind": "workflow", "name": name, "mode": mode});
        let mk = |name: &str, steps: Value| wf(json!({"name": name, "steps": steps}));
        let acting = json!({"kind": "agent", "instruction": "x", "servers": ["mail"],
                            "tools": ["mail.*"]});
        let fin =
            |after: &str| json!({"kind": "finish", "depends_on": [after], "status": "completed"});
        let z = mk(
            "z",
            json!({
                "s": {"kind": "webhook", "path": "/z"},
                "b": {"depends_on": ["s"], "kind": "workflow", "name": "b", "mode": "detached"},
                "t": {"depends_on": ["s"], "kind": "workflow", "name": "t", "mode": "detached"},
                "f": {"kind": "finish", "depends_on": ["b", "t"], "status": "completed"}
            }),
        );
        let b = mk(
            "b",
            json!({
                "s": {"kind": "manual"},
                "c": {"depends_on": ["s"], "kind": "workflow", "name": "c", "mode": "detached"},
                "f": fin("c")
            }),
        );
        let mut c_read = start("t", "sync");
        c_read["depends_on"] = json!(["s"]);
        let mut c_act = acting.clone();
        c_act["depends_on"] = json!(["r"]);
        let c = mk(
            "c",
            json!({"s": {"kind": "manual"}, "r": c_read, "a": c_act, "f": fin("a")}),
        );
        let t = mk("t", json!({"s": {"kind": "manual"}, "f": fin("s")}));
        let set = [z, b, c, t];
        for order in [[0, 1, 2, 3], [1, 2, 3, 0], [3, 2, 1, 0], [2, 0, 3, 1]] {
            let ws: Vec<Workflow> = order.iter().map(|i| set[*i].clone()).collect();
            assert!(check(&s, &ws).is_empty(), "{order:?}: {:?}", check(&s, &ws));
        }
        // The route's child reading back the route's own run, and a sibling
        // the route started: both were handed what they read.
        let hook = mk(
            "hook",
            json!({
                "s": {"kind": "webhook", "path": "/hook"},
                "c": {"depends_on": ["s"], "kind": "workflow", "name": "child"},
                "d": {"depends_on": ["s"], "kind": "workflow", "name": "sib", "mode": "detached"},
                "f": {"kind": "finish", "depends_on": ["c", "d"], "status": "completed"}
            }),
        );
        let sib = mk("sib", json!({"s": {"kind": "manual"}, "f": fin("s")}));
        for target in ["hook", "sib"] {
            let mut a = acting.clone();
            a["depends_on"] = json!(["r"]);
            let child = mk(
                "child",
                json!({
                    "s": {"kind": "manual"},
                    "r": {"kind": "tool", "depends_on": ["s"], "name": "workflow.status",
                          "args": {"name": target}},
                    "a": a, "f": fin("a")
                }),
            );
            for ws in [
                [hook.clone(), child.clone(), sib.clone()],
                [child.clone(), sib.clone(), hook.clone()],
            ] {
                assert!(check(&s, &ws).is_empty(), "{target}: {:?}", check(&s, &ws));
            }
        }
    }

    #[test]
    fn a_stream_the_daemons_telemetry_is_tapped_onto_carries_every_runs_taint() {
        let tapped = |obs: Value| servers(json!({"observability": obs}));
        let runtime = json!({"runtime_events": {"stream": "work", "include": ["run"]}});
        let audit = json!({"audit": {"sink": ["stream"], "stream": "work"}});
        for (obs, producer) in [
            (runtime, "the runtime-events tap"),
            (audit, "the audit stream"),
        ] {
            let s = tapped(obs);
            let errs = check(
                &s,
                &[
                    intake("webhook"),
                    fetch(Value::Null),
                    consumer("work", &["mail"]),
                ],
            );
            assert_eq!(errs.len(), 1, "{errs:?}");
            assert!(
                errs[0].starts_with("workflow \"triage\"") && errs[0].contains(producer),
                "{}",
                errs[0]
            );
            // No run carries outside text: the tap carries none either.
            assert!(check(&s, &[consumer("work", &["mail"])]).is_empty());
        }
        // Untapped, `work` is fed by nothing.
        assert!(
            check(
                &servers(json!({})),
                &[
                    intake("webhook"),
                    fetch(Value::Null),
                    consumer("work", &["mail"])
                ]
            )
            .is_empty()
        );
    }
}
