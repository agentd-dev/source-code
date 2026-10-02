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
//! two answers are one. What a run or a child carries is recorded on its own
//! record ([`Carries`]) when text reaches it: its definition's taint when it
//! starts, the A2A peer whose request started it, and whatever the run,
//! child or conversation that started, spawned, signalled or steered it
//! carried then. Read back, a result is judged by that record — together
//! with what its definition's entry in the table says now, which can only
//! add — never by what happens to be installed or on record at the time:
//! a definition deleted, a spawner's run evicted, a restart, all leave it in
//! place. A run or a child with no record at all is taken to carry text from
//! an origin no reader holds. A result whose every origin the reader holds
//! already is not withheld: a route's own run reading back the child it
//! handed its text to reads nothing new.

use super::reactor::{Runtime, Target};
use super::tools::ToolCaller;
use crate::config::settings::Role;
pub(crate) use crate::engine::run::Carries;
use serde_json::{Value, json};

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

/// The origin of a run or a child no longer on record: no reader holds it,
/// so a reader holding both legs is withheld from.
const GONE: &str = "an origin no longer on record";

/// What a run or a child that is not on record carries: [`GONE`].
fn gone(what: String) -> Carries {
    let mut c = Carries::default();
    c.add([GONE.to_string()], || {
        format!("{what} is no longer on record")
    });
    c
}

/// What an A2A peer's own request hands a run or a child: a peer agent's
/// message is another agent's output, as an `a2a` start's is.
pub(crate) fn peer_carries(peer: &str, how: &str) -> Carries {
    let mut c = Carries::default();
    c.add([format!("the A2A peer {peer}")], || {
        format!("it was {how} by the A2A peer {peer}")
    });
    c
}

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
                Carries::default(),
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
        let (carried, what) = match source {
            Source::Run(id) => (self.run_carries(id), "this run"),
            Source::Child(h) => (self.child_carries(h), "this subagent"),
        };
        if carried.origins.is_subset(&handed.origins) {
            return None;
        }
        if !both {
            // Handed whole, the reader carries it on: its own result is
            // judged with it, whatever the table says the reader can read.
            if matches!(reader, Reader::Run(_) | Reader::Child(_)) {
                self.read_flows
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push((reader.clone(), carried));
            }
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
    /// `runs` list (`workflow.status`, `status`), each child of a
    /// `subagents` list (`subagent.list`, `status`), or the one run or child
    /// the reply is about (`workflow.run` with `wait`, `workflow.wait`, a
    /// sync workflow tool, `subagent.status`, `subagent.await`). `None`
    /// reads it all.
    pub(crate) fn withhold_reply(&self, reader: Option<&Reader>, reply: &mut Value, via: &str) {
        let Some(reader) = reader else { return };
        let mut listed = false;
        for (list, key) in [("runs", "id"), ("subagents", "handle")] {
            if let Some(items) = reply.get_mut(list).and_then(Value::as_array_mut) {
                listed = true;
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
            }
        }
        if listed {
            return;
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

    /// What run `id` carries: its record ([`Carries`]), and what its
    /// definition's entry in the table says now — which only adds, so a
    /// definition that changed taint since the run started errs toward
    /// withholding. A run no longer on record carries [`GONE`].
    pub(crate) fn run_carries(&self, id: &str) -> Carries {
        let Some(r) = self.runs.get(id) else {
            return gone(format!("run {id}"));
        };
        let mut c = r.carries.clone();
        if let Some(t) = self.registry.withholding().run(&r.workflow) {
            c.add(t.origins.iter().cloned(), || t.why.clone());
        }
        self.unsettled(|r| matches!(r, Reader::Run(x) if x == id), &mut c);
        c
    }

    /// What child `handle` carries: its record — what its spawner and every
    /// run or child that steered it carried then — and what a child of its
    /// kind can read back on its own. A child no longer on record carries
    /// [`GONE`].
    pub(crate) fn child_carries(&self, handle: &str) -> Carries {
        let Some(s) = self.subagents.get(handle) else {
            return gone(format!("subagent {handle}"));
        };
        let mut c = s.carries.clone();
        if let Some(t) = self.child_template(handle) {
            let reads = self.registry.withholding().child_reads(t);
            c.add(reads.iter().cloned(), || {
                "it can read back another run's result".to_string()
            });
        }
        self.unsettled(|r| matches!(r, Reader::Child(x) if x == handle), &mut c);
        c
    }

    /// Add to `c` what a reader `is` was handed whole and not yet settled.
    fn unsettled(&self, is: impl Fn(&Reader) -> bool, c: &mut Carries) {
        for (r, from) in self
            .read_flows
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
        {
            if is(r) {
                c.union(from);
            }
        }
    }

    /// Write what each reader was handed whole onto its record
    /// ([`Runtime::read_flows`]).
    pub(crate) fn settle_read_flows(&mut self) {
        let flows = std::mem::take(&mut *self.read_flows.lock().unwrap_or_else(|e| e.into_inner()));
        for (reader, carries) in flows {
            match reader {
                Reader::Run(id) => self.hand_run(&id, &carries),
                Reader::Child(h) => self.steer_carries(&h, &carries),
                Reader::Conversation => {}
            }
        }
    }

    /// What a tool call's caller carries, and so hands whatever it starts,
    /// spawns, signals or steers: a run's or a child's record and, for a call
    /// an A2A peer (an `agent` principal) made or a turn of its own
    /// conversation, the peer's text — decided now, while who it is is
    /// known, and recorded on what it reaches.
    ///
    /// A conversation carries nothing else. A root holding both legs is
    /// withheld every tainted result; one holding a leg less is judged by
    /// everything it can hand its text on to (`config::taint::Withholding`),
    /// so what it starts or spawns holds a leg less too, and no reader
    /// downstream of it holds both.
    pub(crate) fn caller_carries(&self, caller: &ToolCaller) -> Carries {
        if let Some(h) = &caller.subagent {
            return self.child_carries(h);
        }
        if let Some(r) = &caller.run {
            return self.run_carries(r);
        }
        let mut c = Carries::default();
        if let Some(p) = caller.principal.as_deref()
            && self
                .principal_index
                .get(p)
                .is_some_and(|p| p.role == Role::Agent)
        {
            c.union(&peer_carries(p, "handed text"));
        }
        c
    }

    /// Add `carries` to child `handle`'s record — text a run, a child, a
    /// conversation or a peer steered into it (`subagent.send`).
    pub(crate) fn steer_carries(&mut self, handle: &str, carries: &Carries) {
        if let Some(s) = self.subagents.get_mut(handle)
            && s.carries.union(carries)
        {
            s.dirty = true;
        }
    }

    /// Add `carries` to run `id`'s record — text a signal handed it.
    pub(crate) fn hand_run(&mut self, id: &str, carries: &Carries) {
        if let Some(r) = self.runs.get_mut(id)
            && r.carries.union(carries)
        {
            r.dirty = true;
        }
    }

    /// The kind of flat child `handle` is — `Some(template)`, `Some(None)`
    /// for a freeform spawn — or `None` for an instance child, which is a
    /// daemon of its own: the table does not know its reach, so as a reader
    /// it is taken to hold both legs, and on its own it reads nothing back
    /// here.
    fn child_template(&self, handle: &str) -> Option<Option<&str>> {
        let s = self.subagents.get(handle)?;
        match s.tier.as_deref() {
            Some("instance") => None,
            _ => Some(s.template.as_deref()),
        }
    }
}
