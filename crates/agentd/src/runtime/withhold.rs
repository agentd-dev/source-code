// SPDX-License-Identifier: AGPL-3.0-only
//! **Read-back withholding** (RFC 0045 §5.11.3): a run's or a child's result
//! that may carry outside input is never handed to a context that holds both
//! `sensitive` and `egress`.
//!
//! A run's result is the text it was handed, worked over: a webhook body, an
//! A2A peer's message, a tainted stream's events. A conversation or a run
//! that holds both other legs and reads it back — `workflow.status`, a sync
//! workflow tool, `subagent.await`, a plan item bound to it, the note a
//! finished run leaves — would complete the trifecta. Refusing every such
//! context at load took the read-back tools, the plan and the subagents away
//! from any root beside a webhook route, so the read is answered instead: the
//! run's status, with its output, error, step outputs and variables replaced
//! by a marker that says why, and a `readback.withheld` line in the log. A
//! context that holds a leg less is handed the text whole, as before; what it
//! reads it carries on, and the static check follows that (`config::taint`).
//!
//! Who holds what, and which runs can carry outside text, is the
//! [`Withholding`](crate::config::taint::Withholding) the registry derives
//! with every workflow set — the reach the load-time check judges by, so the
//! two answers are one. At runtime a run started by an A2A peer's own request,
//! or by a run that carries outside text, carries it too, and a child carries
//! what the run or child that spawned it carried. A result whose every origin
//! the reader holds already is not withheld: a route's own run reading back
//! the child it handed its text to reads nothing new.

use super::reactor::{Runtime, Target};
use super::tools::ToolCaller;
use crate::config::settings::Role;
use serde_json::{Value, json};
use std::collections::BTreeSet;

/// A context a result is read back into.
#[derive(Debug, Clone)]
pub(crate) enum Reader {
    /// A conversation: a turn under the root grant, the plan it reads every
    /// turn, the notes left in its transcript.
    Conversation,
    /// A workflow run, by id: what one of its steps reads, every later step
    /// of it can.
    Run(String),
    /// A flat child, by handle.
    Child(String),
}

/// Whose result is read back.
#[derive(Debug, Clone, Copy)]
pub(crate) enum Source<'a> {
    Run(&'a str),
    Child(&'a str),
}

/// Where a run's or a child's outside text entered, and the first reason.
#[derive(Debug, Default)]
struct Carried {
    origins: BTreeSet<String>,
    why: Option<String>,
}

impl Carried {
    fn add(&mut self, origins: impl IntoIterator<Item = String>, why: impl FnOnce() -> String) {
        let before = self.origins.len();
        self.origins.extend(origins);
        if self.origins.len() > before && self.why.is_none() {
            self.why = Some(why());
        }
    }
}

/// How far a run's parents, or a child's spawners, are followed: far past
/// any real nesting (`limits.subagents.depth` defaults to 3), and a bound,
/// so a record that names itself cannot spin.
const MAX_HOPS: usize = 32;

impl Reader {
    fn label(&self) -> String {
        match self {
            Reader::Conversation => "conversation".into(),
            Reader::Run(id) => format!("run:{id}"),
            Reader::Child(h) => format!("subagent:{h}"),
        }
    }
}

impl Runtime {
    /// The context a tool call's reply is read into. `None` for a call no
    /// model makes — an A2A op run through a tool's implementation for an
    /// operator or a principal, who is outside the agent and reads what they
    /// may read.
    pub(crate) fn reader_of(&self, caller: &ToolCaller) -> Option<Reader> {
        if let Some(h) = &caller.subagent {
            return Some(Reader::Child(h.clone()));
        }
        if let Some(r) = &caller.run {
            return Some(Reader::Run(r.clone()));
        }
        (caller.node.is_some() || caller.ctx.is_some()).then_some(Reader::Conversation)
    }

    /// The context a deferred reply to `target` is read into. A child that
    /// is gone is taken as a conversation, which errs toward withholding —
    /// the reply has nobody to reach anyway.
    pub(crate) fn reader_at(&self, target: &Target) -> Reader {
        use super::children::ChildKind;
        match target {
            Target::Step(run, _) => Reader::Run(run.clone()),
            Target::Child(node, _) => match self.children.get(*node).map(|c| &c.kind) {
                Some(ChildKind::StepTurn { run, .. }) => Reader::Run(run.clone()),
                Some(ChildKind::Subagent { handle }) => Reader::Child(handle.clone()),
                _ => Reader::Conversation,
            },
        }
    }

    /// The marker that replaces `source`'s result for `reader`, when it is
    /// withheld: `source` can carry outside text the reader does not hold
    /// already, and the reader holds both `sensitive` and `egress`.
    pub(crate) fn withheld(&self, reader: &Reader, source: Source) -> Option<String> {
        let table = self.registry.withholding();
        let (both, handed, who) = match reader {
            Reader::Conversation => (
                table.root_holds_both(),
                Carried::default(),
                "this conversation".to_string(),
            ),
            Reader::Run(id) => {
                let wf = self.runs.get(id).map(|r| r.workflow.as_str());
                (
                    wf.is_none_or(|w| table.workflow_holds_both(w)),
                    self.run_carries(id),
                    match wf {
                        Some(w) => format!("workflow {w:?}"),
                        None => format!("run {id}"),
                    },
                )
            }
            Reader::Child(h) => (
                self.child_template(h)
                    .is_none_or(|t| table.child_holds_both(t)),
                self.child_carries(h),
                format!("subagent {h}"),
            ),
        };
        if !both {
            return None;
        }
        let (carried, what) = match source {
            Source::Run(id) => (self.run_carries(id), "this run"),
            Source::Child(h) => (self.child_carries(h), "this subagent"),
        };
        if carried.origins.is_subset(&handed.origins) {
            return None;
        }
        Some(format!(
            "output withheld: {what} carries outside input ({}); {who} holds sensitive and egress tools",
            carried
                .why
                .unwrap_or_else(|| "it may carry outside input".into())
        ))
    }

    /// Withhold from `reader` the text of one run's or child's record — its
    /// `output`, `result`, `error`, the opening of a child's `instruction`,
    /// and a run's step outputs and variables — leaving its status. The
    /// marker, when it was; logged as `readback.withheld`, with `via` naming
    /// the path.
    pub(crate) fn withhold_record(
        &self,
        reader: &Reader,
        source: Source,
        record: &mut Value,
        via: &str,
    ) -> Option<String> {
        let marker = self.withheld(reader, source)?;
        let o = record.as_object_mut()?;
        let mut any = false;
        for k in ["output", "result", "error", "instruction"] {
            if let Some(x) = o.get_mut(k).filter(|x| !x.is_null()) {
                *x = Value::String(marker.clone());
                any = true;
            }
        }
        for k in ["step_states", "vars"] {
            any |= o.remove(k).is_some();
        }
        if !any {
            return None;
        }
        self.log_withheld(reader, source, &marker, via);
        Some(marker)
    }

    /// Withhold from `reader` what a read-back reply carries: each run of a
    /// `runs` list (`workflow.status`), each child of a `subagents` list
    /// (`subagent.list`), or the one run or child the reply is about
    /// (`workflow.run` with `wait`, `workflow.wait`, a sync workflow tool,
    /// `subagent.status`, `subagent.await`). `None` reads it all.
    pub(crate) fn withhold_reply(&self, reader: Option<&Reader>, reply: &mut Value, via: &str) {
        let Some(reader) = reader else { return };
        for (list, key) in [("runs", "id"), ("subagents", "handle")] {
            if let Some(items) = reply.get_mut(list).and_then(Value::as_array_mut) {
                for item in items {
                    let Some(id) = item.get(key).and_then(Value::as_str).map(str::to_string) else {
                        continue;
                    };
                    let source = if list == "runs" {
                        Source::Run(&id)
                    } else {
                        Source::Child(&id)
                    };
                    self.withhold_record(reader, source, item, via);
                }
                return;
            }
        }
        if let Some(id) = reply.get("run").and_then(Value::as_str).map(str::to_string) {
            self.withhold_record(reader, Source::Run(&id), reply, via);
        } else if let Some(h) = reply
            .get("handle")
            .and_then(Value::as_str)
            .map(str::to_string)
        {
            self.withhold_record(reader, Source::Child(&h), reply, via);
        }
    }

    /// `text` — a note or a plan item's note about `source` — or, when it is
    /// withheld from `reader`, the marker in its place.
    pub(crate) fn withhold_text(
        &self,
        reader: &Reader,
        source: Source,
        text: String,
        via: &str,
    ) -> String {
        match self.withheld(reader, source) {
            Some(marker) if !text.is_empty() => {
                self.log_withheld(reader, source, &marker, via);
                marker
            }
            _ => text,
        }
    }

    /// Answer a deferred wait on a run or a child — `workflow.wait`,
    /// `workflow.run` with `wait`, a sync workflow tool, `subagent.await` —
    /// withholding from whoever it answers what it may not read. A sync
    /// `subagent.run` is answered here too: its child carries what its
    /// spawner did, which the spawner holds already.
    pub(crate) fn reply_read_back(&mut self, target: &Target, mut result: Value, via: &str) {
        let reader = self.reader_at(target);
        self.withhold_reply(Some(&reader), &mut result, via);
        self.reply(target, result, false);
    }

    pub(crate) fn log_withheld(&self, reader: &Reader, source: Source, marker: &str, via: &str) {
        let (key, id) = match source {
            Source::Run(id) => ("run", id),
            Source::Child(h) => ("subagent", h),
        };
        self.log.info(
            "readback.withheld",
            json!({key: id, "reader": reader.label(), "via": via, "reason": marker}),
        );
    }

    /// Where the outside text a run carries entered: what its definition can
    /// be handed (the registry's table), an A2A peer's request that started
    /// it, and the run that started it.
    fn run_carries(&self, id: &str) -> Carried {
        let mut c = Carried::default();
        let mut next = Some(id.to_string());
        for _ in 0..MAX_HOPS {
            let Some(r) = next.take().and_then(|id| self.runs.get(&id)) else {
                break;
            };
            if let Some(t) = self.registry.withholding().run(&r.workflow) {
                c.add(t.origins.iter().cloned(), || t.why.clone());
            }
            // A peer agent's message is another agent's output, as an `a2a`
            // start's is: a run it asked for directly carries it.
            if let Some(p) = r.principal.as_deref()
                && r.start.payload.get("requested_by").and_then(Value::as_str) == Some(p)
                && self
                    .principal_index
                    .get(p)
                    .is_some_and(|p| p.role == Role::Agent)
            {
                c.add([format!("the A2A peer {p}")], || {
                    format!("it was started by the A2A peer {p}")
                });
            }
            next = r
                .parent
                .as_ref()
                .and_then(|p| p.get("run"))
                .and_then(Value::as_str)
                .map(str::to_string);
        }
        c
    }

    /// Where the outside text a child carries entered: what the run or child
    /// that spawned it carried, and what a child of its kind can read back
    /// on its own. A child the root spawned carries nothing from it: a root
    /// that holds both legs is never handed outside text, and one that holds
    /// a leg less leaves nobody to withhold from.
    fn child_carries(&self, handle: &str) -> Carried {
        let mut c = Carried::default();
        let mut next = Some(handle.to_string());
        for _ in 0..MAX_HOPS {
            let Some(h) = next.take() else { break };
            let Some(s) = self.subagents.get(&h) else {
                break;
            };
            if let Some(t) = self.child_template(&h) {
                let reads = self.registry.withholding().child_reads(t);
                c.add(reads.iter().cloned(), || {
                    "it can read back another run's result".to_string()
                });
            }
            let by = s.requested_by.as_ref();
            if let Some(run) = by.and_then(|b| b.get("run")).and_then(Value::as_str) {
                let from = self.run_carries(run);
                let why = from.why.clone();
                c.add(from.origins, || match why {
                    Some(w) => format!(
                        "it was spawned by run {run}, which {}",
                        w.trim_start_matches("it ")
                    ),
                    None => format!("it was spawned by run {run}"),
                });
            }
            next = by
                .and_then(|b| b.get("subagent"))
                .and_then(Value::as_str)
                .map(str::to_string);
        }
        c
    }

    /// The kind of flat child `handle` is — `Some(template)`, `Some(None)`
    /// for a freeform spawn — or `None` for an instance child, which is a
    /// daemon of its own and holds no tool here.
    fn child_template(&self, handle: &str) -> Option<Option<&str>> {
        let s = self.subagents.get(handle)?;
        match s.tier.as_deref() {
            Some("instance") => None,
            _ => Some(s.template.as_deref()),
        }
    }
}
