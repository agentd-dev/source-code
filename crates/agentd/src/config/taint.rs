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
//! A result read back is not refused on: it is withheld. A run's result is
//! the text it was handed, worked over, and so is the result of a child it
//! spawned. Every way the code reads one back — a sync workflow tool's reply,
//! a `workflow` step that is not detached, a `join`, a `wait {on: run}` or
//! `{on: subagent}`, a read-back contract (`registry::internal::read_back`),
//! and the notes a finished run or a child's result leave in a transcript —
//! hands a context that holds both `sensitive` and `egress` the result's
//! status with its text replaced by a marker (`runtime::withhold`), from the
//! [`Withholding`] worked out here, with the same reach the check judges by.
//! So no read-back completes the trifecta, and the check does not refuse a
//! context for holding one. It still follows each read: a reader that holds a
//! leg less is handed the text whole, so its own result carries the taint on
//! to whoever reads it next.
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

/// The refusals for `workflows` under `s`: one per workflow whose runs read
/// a tainted stream, or are started by a run that does, while its
/// model-driven steps reach `sensitive` and `egress`. Unless
/// `security.allow_trifecta` is set, which lifts this check with every
/// other trifecta gate.
///
/// `workflows` is the whole set the configuration would run. Another
/// definition can only add producers, consumers and edges, and every one of
/// them only adds taint to what is judged — a read-back adds taint only to a
/// reader that holds a leg less, which nothing downstream of it can complete
/// — so a refusal over part of the set (the definitions `validate` can read
/// without dialling) is a refusal over all of it; the start re-judges the
/// whole set.
pub fn refusals(s: &Settings, workflows: &[&Workflow]) -> Vec<String> {
    let ctx = Reach::new(s, workflows);
    let p = propagate(&ctx, workflows);
    let mut out = Vec::new();
    for w in workflows {
        let Some(run) = p.runs.get(&w.name).filter(|r| r.checked()) else {
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
             the outside text reaches — this one, and any run it starts or feeds — is judged \
             together, so none of them may reach both of the other legs: narrow the steps' \
             `servers` and `tools` (a `tools` list that leaves out {}), act through a \
             deterministic step (`mcp.tool`, `http`), which the check does not count \
             (docs/security.md), or set security.allow_trifecta (audited)",
            w.name,
            run.describe(&p.streams),
            ctx.advice(),
        ));
    }
    out
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

/// Whether a context reaching `held` is one outside text must not be handed
/// to: it holds `sensitive` and `egress` both, so the text would complete the
/// trifecta. The same verdict [`refused`] gives, so `security.allow_trifecta`
/// lifts it as it lifts every other trifecta gate.
fn holds_both(s: &Settings, held: &[Held]) -> bool {
    refused(s, &[TrifectaTag::UntrustedInput], held).is_some()
}

/// Whether a reload from `old` to `new` moves anything this check reads, so
/// the definitions have to be judged again — the stored ones included, which
/// nothing else re-checks — and the [`Withholding`] worked out again. Kept
/// beside [`Reach`] and [`propagate`], the only readers of these settings
/// here, so a new input is added to both at once.
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
        // The streams the daemon's own telemetry is tapped onto, each fed by
        // every run.
        || old.observability.runtime_events != new.observability.runtime_events
        || old.observability.audit != new.observability.audit
}

/// What the runtime withholds, and from whom (`runtime::withhold`): which
/// runs and children can carry outside text, where it entered, and which
/// contexts hold both `sensitive` and `egress` — worked out once per
/// workflow set (`registry::Registry::register_workflow_tools`), with the
/// reach the check judges by, so the static and the runtime answers cannot
/// drift apart.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Withholding {
    /// Each workflow whose runs can carry outside text.
    runs: BTreeMap<String, Tainted>,
    /// Whether a turn under the root grant holds both legs — itself, or
    /// through what it hands its text on to.
    root: bool,
    /// The workflows whose text can reach both legs: their own model-driven
    /// steps, or what they hand the text on to.
    workflows: BTreeSet<String>,
    /// Every workflow the set holds.
    names: BTreeSet<String>,
    /// Each kind of flat child — by template, `None` for a freeform spawn —
    /// whose text can reach both legs.
    children: BTreeSet<Option<String>>,
    /// Every flat template the settings hold.
    templates: BTreeSet<String>,
    /// Each kind of flat child that holds a leg less and can read a run's or
    /// a child's result back: its own result may carry any of them.
    child_reads: BTreeSet<Option<String>>,
    /// Where the outside text of every tainted run entered, together.
    any: BTreeSet<String>,
}

/// Why one workflow's runs carry outside text: its tags, where it entered
/// ([`RunTaint::origins`]), and the first reason, as a marker names it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Tainted {
    pub tags: Vec<TrifectaTag>,
    pub origins: BTreeSet<String>,
    pub why: String,
}

impl Withholding {
    /// The taint each workflow's runs carry, by workflow — what a workflow
    /// tool's caller is handed back as its result
    /// (`registry::register_workflow_tools`).
    pub fn run_tags(&self) -> BTreeMap<String, Vec<TrifectaTag>> {
        self.runs
            .iter()
            .map(|(n, t)| (n.clone(), t.tags.clone()))
            .collect()
    }

    /// Why a run of `workflow` can carry outside text, when it can.
    pub fn run(&self, workflow: &str) -> Option<&Tainted> {
        self.runs.get(workflow)
    }

    /// Whether a turn under the root grant holds both legs.
    pub fn root_holds_both(&self) -> bool {
        self.root
    }

    /// Whether a run of `workflow` holds both legs. A workflow this set does
    /// not hold is taken to, so a reader the derivation missed errs toward
    /// being withheld from.
    pub fn workflow_holds_both(&self, workflow: &str) -> bool {
        !self.names.contains(workflow) || self.workflows.contains(workflow)
    }

    /// Whether a flat child of `template` (`None`: a freeform spawn) holds
    /// both legs. A template the settings no longer hold — a warm child
    /// outliving a reload that removed it — is taken to, as an unknown
    /// workflow is.
    pub fn child_holds_both(&self, template: Option<&str>) -> bool {
        template.is_some_and(|t| !self.templates.contains(t))
            || self.children.contains(&template.map(str::to_string))
    }

    /// The origins a child of `template` may have read back on its own:
    /// every tainted run's, when it holds a leg less and can read one — or
    /// when its template is no longer held, and what it could read is not
    /// known.
    pub fn child_reads(&self, template: Option<&str>) -> &BTreeSet<String> {
        static NONE: BTreeSet<String> = BTreeSet::new();
        if template.is_some_and(|t| !self.templates.contains(t))
            || self.child_reads.contains(&template.map(str::to_string))
        {
            &self.any
        } else {
            &NONE
        }
    }
}

/// The [`Withholding`] for `workflows` under `s`.
pub fn withholding(s: &Settings, workflows: &[&Workflow]) -> Withholding {
    let ctx = Reach::new(s, workflows);
    let p = propagate(&ctx, workflows);
    let runs: BTreeMap<String, Tainted> = p
        .runs
        .iter()
        .filter(|(_, r)| !r.tags.is_empty())
        .map(|(name, r)| {
            (
                name.clone(),
                Tainted {
                    tags: r.tags.clone(),
                    origins: r.origins.clone(),
                    why: r.why(&p.streams),
                },
            )
        })
        .collect();
    let any = runs
        .values()
        .flat_map(|t| t.origins.iter().cloned())
        .collect();
    // Each kind of flat child: what its text can reach, from its own servers
    // and tools on through the runs it starts.
    let mut children = BTreeSet::new();
    let mut child_reads = BTreeSet::new();
    let kinds = ctx
        .flat
        .keys()
        .map(|t| Some(t.clone()))
        .chain(std::iter::once(None));
    for kind in kinds {
        let mut spec = Map::new();
        if let Some(t) = &kind {
            spec.insert("template".into(), Value::String(t.clone()));
        }
        let mut e = Edges::default();
        ctx.spawn(&spec, "", &mut e);
        if holds_both(s, &p.onward(e.held, &e.started, &e.emits)) {
            children.insert(kind);
        } else if !e.read.is_empty() {
            child_reads.insert(kind);
        }
    }
    // The root by its onward reach, as a run is: what it is handed whole it
    // can pass to any run it starts, and any child — a run's `subagent` step
    // spawns one whatever `tools.disabled` says of the root's own
    // `subagent.run`.
    let root = {
        let mut e = Edges::default();
        ctx.any_spawn("", &mut e);
        let mut held = ctx.root();
        held.extend(e.held);
        let mut started: Started = ctx
            .names
            .iter()
            .map(|n| (n.clone(), String::new()))
            .collect();
        started.extend(e.started);
        holds_both(s, &p.onward(held, &started, &e.emits))
    };
    Withholding {
        runs,
        root,
        workflows: p.both.clone(),
        names: workflows.iter().map(|w| w.name.clone()).collect(),
        children,
        templates: ctx.flat.keys().cloned().collect(),
        child_reads,
        any,
    }
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
    /// The results of tainted runs it is handed whole — it holds a leg less,
    /// so nothing withholds them — each edge, as a marker names it, and the
    /// workflow it reads.
    reads: BTreeSet<(String, String)>,
    /// Where the outside text it carries entered: an outside caller's route
    /// (a direct start, a relayed signal, or an `into:` onto a stream it
    /// reads), or a mirrored stream. A result is withheld from a reader only
    /// for an origin the reader does not hold already — a route's own run
    /// reading back what it handed a child, or what a consumer made of the
    /// text its own route put on a stream, reads back nothing it was not
    /// given (`runtime::withhold`).
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
    /// started it, or it is handed a tainted run's result whole.
    fn checked(&self) -> bool {
        !self.tags.is_empty()
            && !(self.streams.is_empty() && self.callers.is_empty() && self.reads.is_empty())
    }

    /// Why it is judged, as a refusal says it. A run a read alone taints
    /// holds a leg less and is never refused, so its reads are not named.
    fn describe(&self, streams: &BTreeMap<String, StreamTaint>) -> String {
        let mut why: Vec<String> = self
            .streams
            .iter()
            .map(|name| consumed(name, streams))
            .collect();
        why.extend(self.callers.iter().map(|c| format!("it is run by {c}")));
        why.join("; ")
    }

    /// The first reason its runs carry outside text, as the marker that
    /// withholds one names it: short, since it replaces a result a model
    /// reads.
    fn why(&self, streams: &BTreeMap<String, StreamTaint>) -> String {
        self.outside
            .iter()
            .map(|o| format!("it is handed outside input by {o}"))
            .chain(self.streams.iter().map(|name| consumed(name, streams)))
            .chain(self.callers.iter().map(|c| format!("it is run by {c}")))
            .chain(
                self.reads
                    .iter()
                    .map(|(via, target)| format!("it reads back workflow {target:?} at {via}")),
            )
            .next()
            .unwrap_or_else(|| "it may carry outside input".to_string())
    }
}

/// "It consumes stream `name`", with what feeds it.
fn consumed(name: &str, streams: &BTreeMap<String, StreamTaint>) -> String {
    let fed = streams
        .get(name)
        .map(|t| t.producers.iter().cloned().collect::<Vec<_>>().join(", "))
        .unwrap_or_default();
    format!("it consumes stream {name:?} (fed by {fed})")
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

/// The stream a step reads events from, as written: a `stream` or
/// `correlate` start, or a `wait {on: event}`.
fn read_stream(st: &Step) -> Option<&str> {
    match st.kind.as_str() {
        "stream" | "correlate" => st.field_str("stream"),
        "wait" if st.field_str("on") == Some("event") => st.field_str("stream"),
        _ => None,
    }
}

/// The tainted streams a step reads events from. A templated name is any of
/// them.
fn consumes<'t>(st: &Step, streams: &'t BTreeMap<String, StreamTaint>) -> Vec<&'t str> {
    let Some(name) = read_stream(st) else {
        return Vec::new();
    };
    streams
        .iter()
        .filter(|(n, t)| !t.tags.is_empty() && (templated(name) || *n == name))
        .map(|(n, _)| n.as_str())
        .collect()
}

/// The streams an `emit` step puts its run's text on: the one it names, or
/// — templated — any declared stream.
fn emits<'s>(st: &'s Step, s: &'s Settings) -> Vec<&'s str> {
    match st.field_str("stream").filter(|_| st.kind == "emit") {
        Some(stream) if templated(stream) => s.streams.keys().map(String::as_str).collect(),
        Some(stream) => vec![stream],
        None => Vec::new(),
    }
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

/// What [`propagate`] settles.
struct Propagation<'w> {
    streams: BTreeMap<String, StreamTaint>,
    runs: BTreeMap<String, RunTaint>,
    /// Every workflow's steps, each with its [`Edges`].
    edges: Vec<(&'w Workflow, Vec<StepEdges<'w>>)>,
    /// The workflows whose text can reach both legs ([`Propagation::onward`]).
    both: BTreeSet<String>,
}

impl Propagation<'_> {
    /// Everything text handed to a context reaching `held` can reach: that,
    /// and — to a fixed point — what every run it starts or signals reaches,
    /// and every run consuming a stream it emits onto. A reader that holds
    /// both legs anywhere in it is one a tainted result is withheld from:
    /// handed the text whole, it could pass it to the run that completes the
    /// trifecta.
    fn onward(
        &self,
        mut held: Vec<Held>,
        started: &Started,
        emitted: &BTreeSet<String>,
    ) -> Vec<Held> {
        let mut seen: BTreeSet<&str> = BTreeSet::new();
        let mut next: Vec<&str> = started.iter().map(|(t, _)| t.as_str()).collect();
        next.extend(self.consumers(emitted));
        while let Some(name) = next.pop() {
            if !seen.insert(name) {
                continue;
            }
            let Some((_, steps)) = self.edges.iter().find(|(w, _)| w.name == name) else {
                continue;
            };
            for (_, _, e) in steps {
                held.extend(e.held.iter().cloned());
                next.extend(e.started.iter().map(|(t, _)| t.as_str()));
                next.extend(self.consumers(&e.emits));
            }
        }
        let mut labels = BTreeSet::new();
        held.retain(|h| labels.insert(h.label.clone()));
        held
    }

    /// The workflows whose runs read events off any of `streams`.
    fn consumers<'a>(&'a self, streams: &BTreeSet<String>) -> Vec<&'a str> {
        if streams.is_empty() {
            return Vec::new();
        }
        self.edges
            .iter()
            .filter(|(_, steps)| {
                steps.iter().any(|(st, _, _)| {
                    read_stream(st).is_some_and(|n| {
                        templated(n) || streams.iter().any(|s| s == n || templated(s))
                    })
                })
            })
            .map(|(w, _)| w.name.as_str())
            .collect()
    }
}

/// Fold every declared producer into each stream's taint, and each tainted
/// stream into the runs that read it, until nothing moves. A fixed point,
/// because an `emit` target and a `workflow` step's target are static (or,
/// when templated, every one there is): a run reading a tainted stream taints
/// what it emits into, and the workflows it starts.
///
/// A result read back taints its reader only when the reader holds a leg
/// less (`both` below): one that holds both has it withheld at runtime, so it
/// is never handed the text. That makes every read decided by the reader's
/// reach alone — fixed before the pass starts — so the pass is monotone and
/// its answer does not hang on the order the workflows are listed in. And a
/// read never adds a refusal: everything the reader can hand the text on to
/// is inside its onward reach, which holds a leg less.
fn propagate<'w>(ctx: &Reach, workflows: &[&'w Workflow]) -> Propagation<'w> {
    let untrusted = [TrifectaTag::UntrustedInput];
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
    let mut p = Propagation {
        streams: BTreeMap::new(),
        runs: BTreeMap::new(),
        edges,
        both: BTreeSet::new(),
    };
    p.both = p
        .edges
        .iter()
        .filter(|(_, steps)| {
            let (mut held, mut started, mut emitted) = (Vec::new(), Vec::new(), BTreeSet::new());
            for (_, _, e) in steps {
                held.extend(e.held.iter().cloned());
                started.extend(e.started.iter().cloned());
                emitted.extend(e.emits.iter().cloned());
            }
            holds_both(ctx.s, &p.onward(held, &started, &emitted))
        })
        .map(|(w, _)| w.name.clone())
        .collect();
    let Propagation {
        streams,
        runs,
        edges,
        both,
    } = &mut p;
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
    loop {
        let mut grew = false;
        // The daemon's own telemetry, tapped onto a stream: `run.done`
        // carries every run's error (and its output under
        // `observability.log_content`), a step's or tool's event its text,
        // an audit record the question an `ask_human` put. Which run's text a
        // given event carries is only known when it is logged, so the tap is
        // fed by every run. It feeds streams, not a context, so what reads
        // it is judged as any consumer is.
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
        for (w, steps) in edges.iter() {
            for (st, _, e) in steps {
                for name in consumes(st, streams) {
                    let (tags, origins) = (streams[name].tags.clone(), &streams[name].origins);
                    let run = runs.entry(w.name.clone()).or_default();
                    grew |= union(&mut run.tags, &tags);
                    grew |= run.streams.insert(name.to_string());
                    for o in origins {
                        grew |= run.origins.insert(o.clone());
                    }
                }
                // A result read back is handed whole only to a reader that
                // holds a leg less; its result carries the text on.
                if both.contains(&w.name) {
                    continue;
                }
                for (target, via) in &e.read {
                    // Another run of its own workflow carries nothing the
                    // reader's own edges do not account for already.
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
                    let run = runs.entry(w.name.clone()).or_default();
                    for o in origins {
                        grew |= run.origins.insert(o);
                    }
                    grew |= union(&mut run.tags, &tags);
                    grew |= run.reads.insert((via.clone(), target.clone()));
                }
            }
        }
        for (w, steps) in edges.iter() {
            let Some((tags, checked, origins)) = runs
                .get(&w.name)
                .filter(|r| !r.tags.is_empty())
                .map(|r| (r.tags.clone(), r.checked(), r.origins.clone()))
            else {
                continue;
            };
            for (_, at, e) in steps {
                for target in &e.emits {
                    let t = streams.entry(target.clone()).or_default();
                    grew |= union(&mut t.tags, &tags);
                    grew |= t.producers.insert(format!("`emit` at {at}"));
                    for o in &origins {
                        grew |= t.origins.insert(o.clone());
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
            return p;
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
/// drives can reach, the workflows it starts, the streams it emits onto, and
/// the workflows whose result it reads back — and whether it hands the text
/// to a child.
#[derive(Debug, Default)]
struct Edges {
    held: Vec<Held>,
    started: Started,
    emits: BTreeSet<String>,
    read: Started,
    spawns: bool,
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
    /// Each internal contract's grant, from the registry's own table.
    grants: BTreeMap<&'static str, DefaultGrant>,
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
    /// steer one — so a child's result read back by handle may be theirs.
    spawners: Vec<String>,
    /// Whether a flat child can read a result back itself (a policy rule
    /// routes a read-back contract up for it): then a child read back by
    /// handle may carry any run's result.
    child_reads: bool,
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
            grants: contracts.iter().map(|c| (c.name, c.grant)).collect(),
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
            child_reads: false,
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
                e.spawns.then(|| w.name.clone())
            })
            .collect();
        // Whether any flat child — of a template, or freeform — can call a
        // read-back contract itself.
        reach.child_reads = reach
            .flat
            .values()
            .map(|t| t.as_deref())
            .chain((reach.s.subagents.allow_freeform != Some(false)).then_some(None))
            .any(|allow| {
                reach
                    .read_back
                    .iter()
                    .any(|(tool, _)| reach.callable(Who::Child(allow), tool))
            });
        reach
    }

    fn all(&self) -> Vec<Held> {
        self.servers.iter().map(|(_, h)| h.clone()).collect()
    }

    fn disabled(&self, tool: &str) -> bool {
        self.s.tools.disabled.iter().any(|d| d == tool)
    }

    /// The tools a refusal advises leaving out of a `tools` list: the ones
    /// that hand text on.
    fn advice(&self) -> String {
        self.onward
            .iter()
            .map(|(n, _)| *n)
            .collect::<Vec<_>>()
            .join(", ")
    }

    /// What a turn under the root grant reaches: every server, and what the
    /// root's own tools reach — it holds every internal contract's grant.
    /// The runs it starts and the children they spawn are not included:
    /// [`withholding`] adds them to judge it as a reader.
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
        // A read-back contract carries the taint of every run whose result
        // it may return (`registry::register_workflow_tools`). That is only
        // known once this check has run; taken as untrusted here, it is the
        // same answer whenever it matters — what a child reaches through one
        // matters only when some run is tainted, and then the registry tags
        // it so. (`workflow.run` hands text on as well as reading back.)
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
        let grant = self.grants.get(tool);
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
        e.spawns = true;
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
            // A run id names no workflow until the run exists; `status`
            // lists every run.
            ReadBack::Run | ReadBack::Every => e
                .read
                .extend(self.names.iter().map(|n| (n.clone(), via.clone()))),
            ReadBack::Runs => match args.and_then(|a| a.get("run")) {
                Some(_) => e
                    .read
                    .extend(self.names.iter().map(|n| (n.clone(), via.clone()))),
                None => e.read.extend(self.workflows_named(args, "name", &via)),
            },
            // A handle names no workflow either: it may be any spawner's
            // child — and any run's result, when a child can read one back
            // itself.
            ReadBack::Child if self.child_reads => e
                .read
                .extend(self.names.iter().map(|n| (n.clone(), via.clone()))),
            ReadBack::Child => e
                .read
                .extend(self.spawners.iter().map(|n| (n.clone(), via.clone()))),
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
    /// drives can reach, the workflows it starts, the streams it emits onto
    /// — and the workflows
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
            // What it puts on a stream, a consumer reads.
            "emit" => e
                .emits
                .extend(emits(st, self.s).into_iter().map(str::to_string)),
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
        let tags = withholding(&s, &[&i, &c, &tick]).run_tags();
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
            // The streams the daemon's telemetry is tapped onto.
            json!({"observability": {"runtime_events": {"stream": "work", "include": ["run"]}}}),
            json!({"observability": {"audit": {"sink": ["stream"], "stream": "work"}}}),
        ] {
            assert!(inputs_moved(&base, &servers(change.clone())), "{change}");
        }
        // What the root is noted of is the runtime's to withhold, not this
        // check's to judge.
        let notes = json!({"agent": {"on_workflow_finished": "note",
                                     "wake_on": ["subagent_result"]}});
        assert!(!inputs_moved(&base, &servers(notes)));
    }

    // ---- results read back (RFC 0045 §5.11.3): withheld, not refused ------

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

    /// A workflow named `name` whose `read` step (given whole) runs before
    /// `act`, both after a manual start.
    fn run_reading(name: &str, read: Value, act: Value) -> Workflow {
        let mut read = read;
        read["depends_on"] = json!(["s"]);
        let mut act = act;
        act["depends_on"] = json!(["r"]);
        wf(json!({"name": name, "steps": {
            "s": {"kind": "manual"},
            "r": read,
            "a": act,
            "f": {"kind": "finish", "depends_on": ["a"], "status": "completed"}
        }}))
    }

    /// An agent holding `server`.
    fn acting(server: &str) -> Value {
        json!({"kind": "agent", "instruction": "x", "servers": [server],
               "tools": [format!("{server}.*")]})
    }

    fn model(tool: &str) -> Value {
        json!({"kind": "agent", "instruction": "x", "servers": [], "tools": [tool]})
    }

    fn withheld(s: &Settings, ws: &[Workflow]) -> Withholding {
        withholding(s, &ws.iter().collect::<Vec<_>>())
    }

    /// Every spelling of a result read back, by a caller whose agent holds
    /// `mail` — both other legs. None is refused: the runtime withholds the
    /// result from that caller, which the table says, so the caller carries
    /// nothing it read — while what it reads can carry outside input.
    #[test]
    fn a_reader_holding_both_legs_is_withheld_from_not_refused() {
        let s = servers(json!({}));
        let tool = |name: &str, args: Value| json!({"kind": "tool", "name": name, "args": args});
        let by_run = [
            model("workflow.run"),
            model("workflow.wait"),
            model("workflow.status"),
            model("plan.update"),
            tool("workflow.run", json!({"name": "fetch", "wait": true})),
            tool("workflow.wait", json!({"run": "{{inputs.run}}"})),
            tool("workflow.status", json!({"name": "fetch"})),
            tool(
                "plan.update",
                json!({"item": 1, "bind": {"run": "{{inputs.run}}"}}),
            ),
            json!({"kind": "workflow", "name": "fetch"}),
            json!({"kind": "workflow", "name": "fetch", "mode": "async"}),
            json!({"kind": "workflow.wait", "run": "{{inputs.run}}"}),
            json!({"kind": "wait", "on": "run", "run": "{{inputs.run}}"}),
            json!({"kind": "join", "handles": ["{{inputs.run}}"]}),
        ];
        let by_handle = [
            model("subagent.status"),
            model("subagent.await"),
            model("subagent.list"),
            tool("subagent.await", json!({"handle": "{{inputs.h}}"})),
            json!({"kind": "wait", "on": "subagent", "subagent": "{{inputs.h}}"}),
            json!({"kind": "join", "handles": ["{{inputs.h}}"]}),
        ];
        let cases = by_run
            .into_iter()
            .map(|r| (r, fetch(Value::Null)))
            .chain(by_handle.into_iter().map(|r| (r, spawner("async"))))
            .chain(std::iter::once((
                model("inbox.next"),
                fetch(json!({"name": "inbox.next"})),
            )));
        for (read, target) in cases {
            let ws = [
                intake("webhook"),
                target,
                run_reading("caller", read.clone(), acting("mail")),
            ];
            assert!(check(&s, &ws).is_empty(), "{read}: {:?}", check(&s, &ws));
            let w = withheld(&s, &ws);
            assert!(w.workflow_holds_both("caller"), "{read}");
            assert!(w.run("fetch").is_some(), "{read}");
            assert!(w.run("caller").is_none(), "{read}: {:?}", w.run("caller"));
        }
    }

    /// A reader that holds a leg less is handed the result whole, so its own
    /// result carries the taint on — a reader of IT holding both has it
    /// withheld in turn — and so does everything it hands the text to. That
    /// is why its reach counts what it hands on: one that starts a run
    /// holding `mail`, or emits onto a stream a run holding `mail` reads,
    /// reaches both legs through it, and is withheld from instead.
    #[test]
    fn a_reader_holding_a_leg_less_carries_what_it_reads_back() {
        let s = servers(json!({}));
        let status = json!({"kind": "tool", "name": "workflow.status", "args": {"name": "fetch"}});
        let relay = run_reading("relay", status.clone(), acting("notes"));
        let ws = [intake("webhook"), fetch(Value::Null), relay];
        assert!(check(&s, &ws).is_empty());
        let w = withheld(&s, &ws);
        assert!(!w.workflow_holds_both("relay"));
        let t = w.run("relay").expect("relay carries what it read");
        assert!(t.why.contains("reads back workflow \"fetch\""), "{}", t.why);
        assert_eq!(t.origins, w.run("fetch").unwrap().origins);

        let send = run_reading(
            "send",
            json!({"kind": "sleep", "duration": "1s"}),
            acting("mail"),
        );
        let onward = [
            json!({"kind": "workflow", "name": "send", "mode": "detached"}),
            json!({"kind": "emit", "stream": "work", "subject": "m"}),
        ];
        for hand_on in onward {
            let relay = run_reading("relay", status.clone(), hand_on.clone());
            let ws = [
                intake("webhook"),
                fetch(Value::Null),
                relay,
                send.clone(),
                consumer("work", &["mail"]),
            ];
            assert!(check(&s, &ws).is_empty(), "{hand_on}: {:?}", check(&s, &ws));
            let w = withheld(&s, &ws);
            assert!(w.workflow_holds_both("relay"), "{hand_on}");
            assert!(w.run("relay").is_none(), "{hand_on}");
        }
    }

    /// A child whose template the settings no longer hold — a warm child
    /// outliving the reload that removed it — is read as holding both legs
    /// and as having read back any tainted run, as an unknown workflow is:
    /// what it could reach is not known, so the answer errs toward
    /// withholding.
    #[test]
    fn a_child_of_a_template_no_longer_held_errs_toward_withholding() {
        let s = servers(json!({"subagents": {"templates": {
            "desk": {"instruction": "x", "servers": ["notes"]}}}}));
        let ws = [intake("webhook"), fetch(Value::Null)];
        let w = withheld(&s, &ws);
        assert!(!w.child_holds_both(Some("desk")));
        assert!(w.child_reads(Some("desk")).is_empty());
        assert!(w.child_holds_both(Some("gone")));
        assert_eq!(
            w.child_reads(Some("gone")),
            &w.run("fetch").expect("fetch is tainted").origins
        );
    }

    /// Nothing an untainted run hands back is withheld: the table names no
    /// run until outside text reaches one.
    #[test]
    fn a_run_no_outside_text_reaches_carries_nothing() {
        let s = servers(json!({}));
        let read = run_reading("caller", model("workflow.status"), acting("mail"));
        let w = withheld(&s, &[fetch(Value::Null), read.clone()]);
        assert_eq!(w.run("fetch"), None);
        assert!(w.run_tags().is_empty());
        let w = withheld(&s, &[intake("webhook"), fetch(Value::Null), read]);
        assert!(w.run("fetch").is_some());
    }

    /// The root conversation holds every read-back — the tools, the plan,
    /// the children, the notes of a finished run and of a child's result —
    /// and is not refused for any of them beside a tainted run, holding
    /// both legs: the table says it holds both, so the runtime withholds
    /// from it. A root a leg less is handed results whole.
    #[test]
    fn the_root_holding_both_legs_is_withheld_from_not_refused() {
        let s = servers(json!({"agent": {
            "on_workflow_finished": "note", "tools": {"internal": "all"},
            "wake_on": ["subagent_result", "workflow_finished", "workflow_failed"]}}));
        for ws in [
            vec![intake("webhook"), fetch(json!({"name": "inbox.next"}))],
            vec![intake("webhook"), spawner("warm")],
        ] {
            assert!(check(&s, &ws).is_empty(), "{:?}", check(&s, &ws));
            assert!(withheld(&s, &ws).root_holds_both());
        }
        let mut less = s.clone();
        less.mcp.servers.retain(|m| m.name == "notes");
        assert!(!withheld(&less, &[intake("webhook"), fetch(Value::Null)]).root_holds_both());
        // The explicit allow lifts it with every other trifecta gate.
        let mut allowed = s;
        allowed.security.allow_trifecta = true;
        assert!(!withheld(&allowed, &[intake("webhook"), fetch(Value::Null)]).root_holds_both());
    }

    /// Each kind of flat child is judged by its own reach: a template over
    /// `mail` holds both legs and is withheld from; one over `notes` holds a
    /// leg less — and, when a policy rule routes `workflow.status` up to it,
    /// reads back any run, so its own result may carry any run's outside
    /// text. A freeform child may be handed any server.
    #[test]
    fn a_childs_kind_decides_what_it_is_withheld_and_carries() {
        let with = |policies: Value| {
            let mut s = servers(json!({"security": {"policies": policies}}));
            for (name, server) in [("wide", "mail"), ("narrow", "notes")] {
                s.subagents.templates.insert(
                    name.into(),
                    serde_json::from_value(json!({"instruction": "x", "servers": [server]}))
                        .unwrap(),
                );
            }
            s
        };
        let ws = [intake("webhook"), fetch(Value::Null)];
        let w = withheld(
            &with(json!([{"match": {"tool": "workflow.status"}, "action": "allow"}])),
            &ws,
        );
        assert!(w.child_holds_both(Some("wide")));
        assert!(!w.child_holds_both(Some("narrow")));
        assert!(w.child_holds_both(None));
        assert_eq!(
            w.child_reads(Some("narrow")),
            &w.run("fetch").unwrap().origins
        );
        assert!(
            w.child_reads(Some("wide")).is_empty(),
            "withheld from, it reads nothing"
        );
        // Without the rule a child holds no read-back to carry.
        let w = withheld(&with(json!([])), &ws);
        assert!(w.child_reads(Some("narrow")).is_empty());
    }

    /// A result is withheld only for text the reader was not handed
    /// (`runtime::withhold`): what a run carries names the route it entered
    /// by. A route's own run hands its child its own route's text, and
    /// reading the child back reads nothing new; what another route put on
    /// `inbox` is.
    #[test]
    fn origins_name_the_route_the_text_entered_by() {
        let s = servers(json!({}));
        let hook = wf(json!({"name": "hook", "steps": {
            "s": {"kind": "webhook", "path": "/hook"},
            "c": {"kind": "workflow", "depends_on": ["s"], "name": "child"},
            "a": {"kind": "agent", "depends_on": ["c"], "instruction": "x",
                  "servers": ["mail"], "tools": ["mail.*"]},
            "f": {"kind": "finish", "depends_on": ["a"], "status": "completed"}
        }}));
        let child = run_reading(
            "child",
            json!({"kind": "sleep", "duration": "1s"}),
            acting("notes"),
        );
        let ws = [intake("webhook"), fetch(Value::Null), hook, child];
        assert!(check(&s, &ws).is_empty());
        let w = withheld(&s, &ws);
        let route = |n: &str| BTreeSet::from([route_origin(n)]);
        assert_eq!(w.run("hook").unwrap().origins, route("hook"));
        assert_eq!(w.run("child").unwrap().origins, route("hook"));
        assert_eq!(w.run("fetch").unwrap().origins, route("intake"));
        assert!(w.workflow_holds_both("hook"));
        // A workflow the set does not hold errs toward being withheld from.
        assert!(w.workflow_holds_both("gone"));
    }

    /// The verdict and the table are the same whatever order the workflows
    /// are listed in: a read is decided by the reader's own reach, which no
    /// other read moves.
    #[test]
    fn neither_verdict_nor_table_hangs_on_the_order() {
        let s = servers(json!({}));
        let status = json!({"kind": "tool", "name": "workflow.status", "args": {"name": "relay"}});
        let set = [
            intake("webhook"),
            fetch(Value::Null),
            run_reading(
                "relay",
                json!({"kind": "tool", "name": "workflow.status", "args": {"name": "fetch"}}),
                acting("notes"),
            ),
            run_reading("relay2", status, acting("notes")),
        ];
        let first = withheld(&s, &set);
        assert!(first.run("relay2").is_some(), "the chain is followed");
        for order in [[3, 2, 1, 0], [2, 0, 3, 1], [1, 3, 0, 2]] {
            let ws: Vec<Workflow> = order.iter().map(|i| set[*i].clone()).collect();
            assert!(check(&s, &ws).is_empty(), "{order:?}");
            assert_eq!(withheld(&s, &ws), first, "{order:?}");
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
