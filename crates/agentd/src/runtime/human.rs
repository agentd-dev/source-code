// SPDX-License-Identifier: AGPL-3.0-only
//! **Human-in-the-loop**: the `ask_human` internal tool and the workflow
//! `human` node, wired to the interface.
//!
//! The flow: an ask flips (or creates) an A2A task to `input-required` with
//! the question as its status message — every attached display client renders
//! an answerable gate — and the asking unit suspends as a
//! [`PendingKind::Human`]. A `SendMessage` carrying that `taskId` resolves the
//! pending with the reply text: a turn's tool call returns it to the model, a
//! workflow `human` step completes with it as output. Tasks are durable, so a
//! run's gate survives a restart (rebuilt from the suspended step). A turn's
//! gate degrades to conversation continuation instead: the asking child does
//! not outlive the process, so there is no tool call left to return into, and
//! the answer starts a fresh turn carrying it.
//!
//! **Fallback** (`agent.ask_human_fallback`) when NO human channel exists
//! (`interface.enabled` off): `fail` (default — error immediately), `wait`
//! (park until the ask timeout), or `auto` — an LLM judge answers on the
//! operator's behalf (also fired when an interface-served gate times out
//! unanswered). Auto answers are marked as auto in the task, the log and the
//! audit stream — never mistakable for a human decision.

use super::reactor::{PendingKind, Runtime, Target};
use crate::config::v2::AskHumanFallback;
use crate::intel::client::IntelClient;
use crate::state::now_ms;
use crate::wire::intel::{Message, Request};
use serde_json::{Value, json};
use std::time::Duration;

/// The default patience for a human answer.
const ASK_TIMEOUT: Duration = Duration::from_secs(24 * 3600);
/// The same default in milliseconds, for callers that already work in ms
/// (a `security.policies` gate with no explicit `timeout`).
#[cfg(feature = "a2a")]
pub(crate) const ASK_TIMEOUT_MS: u64 = 24 * 3600 * 1000;
/// How long the auto judge gets once fired.
const AUTO_GRACE_MS: u64 = 10 * 60 * 1000;
/// The judge's "cannot decide" sentinel.
const UNDECIDED: &str = "UNDECIDED";

impl Runtime {
    /// The `ask_human` internal tool.
    pub(crate) fn ask_human_tool(
        &mut self,
        caller: &super::tools::ToolCaller,
        args: Value,
    ) -> super::tools::ToolOutcome {
        use super::tools::ToolOutcome;
        let question = {
            let q = args["question"].as_str().unwrap_or("").trim().to_string();
            let mut q = if q.is_empty() {
                "The agent needs your input.".to_string()
            } else {
                q
            };
            if q.len() > 2000 {
                let mut cut = 2000;
                while cut > 0 && !q.is_char_boundary(cut) {
                    cut -= 1;
                }
                q.truncate(cut);
                q.push('…');
            }
            q
        };
        let timeout = args
            .get("timeout")
            .and_then(Value::as_str)
            .and_then(|t| crate::config::parse_duration(t).ok())
            .unwrap_or(ASK_TIMEOUT);
        let deadline_ms = now_ms() + timeout.as_millis() as u64;
        // The declared answer shape, carried so the reply can be checked
        // against it rather than merely advertised to clients.
        let schema = args.get("schema").cloned().filter(|v| !v.is_null());
        // Who must answer. A malformed `to` is refused rather than dropped: a
        // gate that looks routed and is not is worse than one that never
        // claimed to be.
        let addressee = match args.get("to").filter(|v| !v.is_null()) {
            None => None,
            Some(v) => match crate::a2a::principals::Addressee::parse(v) {
                Ok(a) => Some(a),
                Err(e) => {
                    return ToolOutcome::Ready(Value::String(format!("ask_human: {e}")), true);
                }
            },
        };

        // The approval policy decides whether to ask AT ALL. It is checked
        // before availability, because "do not interrupt me" is a decision the
        // operator made and should hold whether or not a channel happens to
        // exist.
        match self.settings.agent.approval {
            crate::config::v2::Approval::Ask => {}
            // An ADDRESSED gate is never auto-answered, whatever the approval
            // policy says. The point of naming a decider is that the record is
            // true; a model judge standing in for the finance lead makes it a
            // lie, and the operator who set `approval: auto` was making a
            // statement about the agent's own asks, not about a gate that
            // names someone.
            _ if addressee.is_some() => {}
            crate::config::v2::Approval::Accept => {
                // Accept what the ask RECOMMENDS. With nothing recommended
                // there is nothing to accept, and inventing an answer to a
                // question a person wanted asked is worse than asking it — so
                // fall through to the judge instead of guessing.
                let recommended = args
                    .get("recommend")
                    .cloned()
                    .filter(|v| !v.is_null())
                    .or_else(|| schema.as_ref().and_then(|s| s.get("default").cloned()));
                if let Some(v) = recommended {
                    let text = match &v {
                        Value::String(s) => s.clone(),
                        other => other.to_string(),
                    };
                    self.log.info(
                        "human.auto_accepted",
                        json!({"question": question, "answer": text, "policy": "accept"}),
                    );
                    self.audit(super::audit::AuditEvent {
                        action: "ask_human.accepted",
                        target: json!({"question": question}),
                        outcome: "accept",
                        principal: Some("policy"),
                        role: None,
                        request_id: None,
                    });
                    return ToolOutcome::Ready(
                        json!({"reply": text, "timed_out": false, "via": "accept"}),
                        false,
                    );
                }
                let ask = self.next_id("ask");
                self.spawn_human_judge(&ask, &question);
                return ToolOutcome::Deferred(PendingKind::Human {
                    task: ask,
                    question,
                    deadline_ms: now_ms() + AUTO_GRACE_MS,
                    standalone: false,
                    auto_fired: true,
                    schema,
                    addressee: None,
                });
            }
            crate::config::v2::Approval::Auto => {
                let ask = self.next_id("ask");
                self.spawn_human_judge(&ask, &question);
                return ToolOutcome::Deferred(PendingKind::Human {
                    task: ask,
                    question,
                    deadline_ms: now_ms() + AUTO_GRACE_MS,
                    standalone: false,
                    auto_fired: true,
                    schema,
                    addressee: None,
                });
            }
        }

        // A human can answer only through the interface surface.
        #[cfg(feature = "a2a")]
        let available = self.settings.interface.enabled && self.a2a_sink.is_some();
        #[cfg(not(feature = "a2a"))]
        let available = false;

        if available {
            #[cfg(feature = "a2a")]
            return self.human_gate(caller, question, deadline_ms, schema, addressee);
        }
        let _ = caller;
        // No channel to ask on: take the configured fallback.
        match self.settings.agent.ask_human_fallback {
            AskHumanFallback::Fail => ToolOutcome::Ready(
                Value::String(
                    "ask_human: no human channel (interface.enabled is off) and \
                     agent.ask_human_fallback = fail"
                        .into(),
                ),
                true,
            ),
            AskHumanFallback::Wait => {
                let ask = self.next_id("ask");
                self.log.info(
                    "human.ask.parked",
                    // The `fail` branch above names the cause; this one used to
                    // say only "no human channel", and it is the branch people
                    // actually configure. An integrator lost an hour to the
                    // asymmetry — the condition is one config key, and the log
                    // that fires is the one place they look.
                    json!({
                        "ask": ask,
                        "deadline_ms": deadline_ms,
                        "note": "no human channel (interface.enabled is off); \
                                 ask_human_fallback = wait — this gate will park until its timeout"
                    }),
                );
                ToolOutcome::Deferred(PendingKind::Human {
                    task: ask,
                    question,
                    deadline_ms,
                    standalone: false,
                    auto_fired: false,
                    schema: schema.clone(),
                    addressee: addressee.clone(),
                })
            }
            AskHumanFallback::Auto => {
                let ask = self.next_id("ask");
                self.spawn_human_judge(&ask, &question);
                ToolOutcome::Deferred(PendingKind::Human {
                    task: ask,
                    question,
                    deadline_ms: now_ms() + AUTO_GRACE_MS,
                    standalone: false,
                    auto_fired: true,
                    schema: schema.clone(),
                    addressee: None,
                })
            }
        }
    }

    /// The interface-served gate: flip (or create) the owning A2A task to
    /// `input-required` and suspend the asker.
    #[cfg(feature = "a2a")]
    pub(crate) fn human_gate(
        &mut self,
        caller: &super::tools::ToolCaller,
        question: String,
        deadline_ms: u64,
        schema: Option<Value>,
        addressee: Option<crate::a2a::principals::Addressee>,
    ) -> super::tools::ToolOutcome {
        use super::children::ChildKind;
        use super::tools::ToolOutcome;
        use crate::a2a::tasks::{Link, State};

        // The task this ask belongs to: the A2A task behind the asking turn,
        // or the task tracking the asking run.
        let linked: Option<String> = if let Some(node) = caller.node {
            match self.children.get(node).map(|c| c.kind.clone()) {
                Some(ChildKind::RootTurn {
                    event: Some(ev), ..
                }) => self.event_to_task.get(&ev).cloned(),
                Some(ChildKind::StepTurn { run, .. }) => {
                    self.runs.get(&run).and_then(|r| r.task.clone())
                }
                _ => None,
            }
        } else if let Some(run) = &caller.run {
            self.runs.get(run).and_then(|r| r.task.clone())
        } else {
            None
        };
        let linked = linked.filter(|t| self.tasks.get(t).is_some_and(|t| !t.state.is_terminal()));

        // One live gate per task (asks within one unit are sequential anyway).
        if let Some(t) = &linked
            && self
                .pending
                .iter()
                .any(|p| matches!(&p.kind, PendingKind::Human { task, .. } if task == t))
        {
            return ToolOutcome::Ready(
                Value::String("ask_human: an ask is already pending on this task".into()),
                true,
            );
        }

        let (task_id, standalone) = match linked {
            Some(t) => (t, false),
            None => {
                // No A2A caller owns this unit (a scheduled turn, a subagent,
                // a run started by a timer): create the gate task so attached
                // operators see and answer it.
                let principal_id = caller
                    .principal
                    .clone()
                    .unwrap_or_else(|| "operator".to_string());
                let principal = crate::a2a::Principal {
                    id: principal_id,
                    role: crate::config::v2::Role::Operator,
                    grants: Vec::new(),
                    rate: None,
                    budget: None,
                    labels: Default::default(),
                };
                if let Some(run) = &caller.run {
                    let run = run.clone();
                    let ctx = format!("run-{run}");
                    let tid = self.task_create(&ctx, &principal, Link::Run { id: run.clone() });
                    // The run's completion drives this task terminal.
                    if let Some(r) = self.runs.get_mut(&run) {
                        r.task = Some(tid.clone());
                        r.touch();
                    }
                    (tid, false)
                } else {
                    let ctx = caller.context_id();
                    let tid = self.task_create(&ctx, &principal, Link::Turn { ctx: ctx.clone() });
                    (tid, true)
                }
            }
        };

        if let Some(t) = self.tasks.get_mut(&task_id) {
            // The schema travels WITH the gate: the question says what is being
            // asked, the schema says how to ask it. Without it a client can
            // only offer a text box and hope the person types one of the words
            // the schema would have listed.
            t.ask_schema = schema.clone();
            t.transition(State::InputRequired, Some(question.clone()));
        }
        self.task_persist(&task_id);
        self.task_sync(&task_id);
        self.log.info(
            "human.ask",
            json!({"task": task_id, "question": question, "deadline_ms": deadline_ms}),
        );
        self.audit(super::audit::AuditEvent {
            action: "ask_human",
            target: json!({"task": task_id}),
            outcome: "asked",
            principal: caller.principal.as_deref(),
            role: None,
            request_id: None,
        });
        ToolOutcome::Deferred(PendingKind::Human {
            task: task_id,
            question,
            deadline_ms,
            standalone,
            auto_fired: false,
            schema,
            addressee,
        })
    }

    /// Put a rejected gate back, with the reason appended to the question.
    ///
    /// The alternative is failing the step, which throws away a human who is
    /// still sitting there — and the usual cause is a typo, not a refusal.
    fn reask_human(&mut self, mut p: super::reactor::PendingTool, question: &str, why: &str) {
        let amended = format!("{question}\n\n(previous answer rejected: {why})");
        if let PendingKind::Human { question: q, .. } = &mut p.kind {
            *q = amended.clone();
        }
        #[cfg(feature = "a2a")]
        if let PendingKind::Human { task, .. } = &p.kind {
            use crate::a2a::tasks::State;
            let task = task.clone();
            if let Some(t) = self.tasks.get_mut(&task) {
                t.transition(State::InputRequired, Some(amended));
            }
            self.task_persist(&task);
            self.task_sync(&task);
        }
        self.pending.push(p);
    }

    /// Resolve pending ask `i` with an answer. `via` marks who decided
    /// (`"human"` / `"auto"`) in the task, the log and the audit stream.
    /// `answered_by` is the principal who actually replied. The audit line
    /// used to carry `via` — "human" or "auto" — in the principal field, which
    /// says HOW a gate was answered and not by WHOM. An addressed gate makes
    /// that load-bearing: "the finance lead approved" is only a record if the
    /// record names them.
    pub(crate) fn human_answer(
        &mut self,
        i: usize,
        text: &str,
        via: &str,
        answered_by: Option<&str>,
    ) {
        let p = self.pending.remove(i);
        let PendingKind::Human {
            task,
            standalone,
            schema,
            question,
            ..
        } = &p.kind
        else {
            return;
        };
        let (task, standalone) = (task.clone(), *standalone);
        self.fire_event_starts(
            "human.answered",
            &serde_json::json!({"task": task, "via": via}),
        );
        // The declared answer shape was advertised to clients and never applied
        // to what came back, so a gate could ask for
        // `{decision: "file"|"hold"}` and the run would proceed on "maybe
        // later". Check it here — and re-ask rather than fail, because the
        // person is still there and a second try is cheaper than a dead run.
        if let Some(schema) = schema.clone() {
            let value = match crate::mcp::elicit::shape_reply(&json!(text), &schema) {
                ::mcp::inbound::Answer::Accept(v) => v,
                // Cancel/Decline: the person declined to answer in the declared
                // shape, which is a real answer, not a validation failure.
                _ => json!(text),
            };
            if let Err(errs) = crate::jsonschema::validate(&schema, &value) {
                let q = question.clone();
                self.log.info(
                    "human.answer.rejected",
                    json!({"task": task, "errors": errs, "via": via}),
                );
                // An `auto` judge that cannot produce the shape must not spin.
                if via == "auto" {
                    self.human_task_fail(
                        &task,
                        &format!(
                            "auto-answer does not match the declared schema: {}",
                            errs.join("; ")
                        ),
                    );
                    return;
                }
                self.reask_human(p, &q, &errs.join("; "));
                return;
            }
        }
        // The asker has to still BE there to receive it. A child that died with
        // its gate open took its `ToolResult` slot with it, so `reply` would
        // write into a closed pipe and a human's decision would vanish leaving
        // nothing but a debug line — while the task read as answered.
        // `poll_pending_human` reaps orphaned gates every tick; this is the race
        // where the answer lands in the same tick the child is reaped, and the
        // honest outcome is a failed gate, not a silent one.
        if let Target::Child(node, _) = &p.target
            && self.children.get(*node).is_none()
        {
            const LATE: &str = "ask_human: the asking turn ended before the answer arrived";
            self.human_task_fail(&task, LATE);
            self.log.warn(
                "human.answer.undelivered",
                json!({"task": task, "via": via}),
            );
            self.audit(super::audit::AuditEvent {
                action: "ask_human.answered",
                target: json!({"task": task}),
                outcome: "undelivered",
                principal: answered_by.or(Some(via)),
                role: None,
                request_id: None,
            });
            return;
        }
        #[cfg(feature = "a2a")]
        if self.tasks.contains_key(&task) {
            use crate::a2a::tasks::State;
            let note = if via == "auto" {
                "auto-answered (no human reply)"
            } else {
                "answered"
            };
            if let Some(t) = self.tasks.get_mut(&task) {
                if standalone {
                    // The Q&A was this task's whole purpose.
                    t.transition(State::Completed, Some(note.to_string()));
                } else {
                    t.transition(State::Working, Some(note.to_string()));
                }
            }
            self.task_persist(&task);
            self.task_sync(&task);
        }
        let _ = standalone;
        self.log.info(
            "human.answered",
            json!({"task": task, "via": via, "by": answered_by}),
        );
        // The record names WHO, not just how: an addressed gate is only worth
        // declaring if the audit line can be read back as "this person decided
        // this".
        self.audit(super::audit::AuditEvent {
            action: "ask_human.answered",
            target: json!({"task": task}),
            outcome: via,
            principal: answered_by.or(Some(via)),
            role: None,
            request_id: None,
        });
        // A tool result must match `ask_human`'s DECLARED output shape,
        // `{reply, timed_out}` (see `registry/internal.rs`). Consumers read
        // that contract literally: the MCP elicitation bridge pulls `reply` out
        // of the tool result to build the `accept` content, and a bare string
        // would leave it with nothing, turning every `elicitation/create` into
        // a `cancel`. `via` rides along so the asker can tell an auto judge's
        // guess from a human decision.
        //
        // A workflow `human` step is NOT a tool result: the answer itself is
        // the step's output, and later steps template on
        // `steps.<gate>.output`, so a step keeps the bare reply.
        let result = match &p.target {
            Target::Child(..) => json!({"reply": text, "timed_out": false, "via": via}),
            Target::Step(..) => Value::String(text.to_string()),
        };
        self.reply(&p.target, result, false);
    }

    /// Fail pending ask `i` (timeout / cancel / judge failure).
    pub(crate) fn human_fail(&mut self, i: usize, msg: &str) {
        let p = self.pending.remove(i);
        let PendingKind::Human { task, .. } = &p.kind else {
            return;
        };
        let task = task.clone();
        self.human_task_fail(&task, msg);
        self.log
            .warn("human.ask.failed", json!({"task": task, "err": msg}));
        self.reply(&p.target, Value::String(msg.to_string()), true);
    }

    /// Fail the gate task itself (a no-op when no A2A task backs the ask).
    fn human_task_fail(&mut self, task: &str, msg: &str) {
        #[cfg(feature = "a2a")]
        if self.tasks.contains_key(task) {
            use crate::a2a::tasks::State;
            if let Some(t) = self.tasks.get_mut(task) {
                t.transition(State::Failed, Some(msg.to_string()));
            }
            self.task_persist(task);
            self.task_sync(task);
        }
        #[cfg(not(feature = "a2a"))]
        let _ = (task, msg);
    }

    /// The per-tick pass over pending asks: prune gates whose step already
    /// resolved (the durable wait-record timeout owns step timeouts), fire the
    /// `auto` judge on an unanswered deadline, and fail what remains.
    pub(crate) fn poll_pending_human(&mut self) {
        let now = now_ms();
        let auto = self.settings.agent.ask_human_fallback == AskHumanFallback::Auto;
        enum End {
            /// The asker is gone: drop the gate and fail its task with `why`.
            Prune(String, &'static str),
            Timeout,
        }
        // Addressed by TARGET, never by index — the reentrancy that panicked
        // `poll_pending`: ending one gate calls `reply`, which re-enters the
        // reactor (a step outcome cascades through `finish_step` into
        // `cancel_scoped_children`, which prunes `pending` itself), so an index
        // remembered across an end addresses a different entry by the time we
        // use it — or one past the end, panicking the reactor thread and taking
        // the daemon with it.
        let mut fire_auto: Vec<Target> = Vec::new();
        let mut ends: Vec<(Target, End)> = Vec::new();
        for p in self.pending.iter() {
            let PendingKind::Human {
                task,
                deadline_ms,
                auto_fired,
                ..
            } = &p.kind
            else {
                continue;
            };
            match &p.target {
                // The step resolved some other way (its wait-record timed out,
                // the run was cancelled): drop the dangling gate.
                Target::Step(run, step) => {
                    let suspended = self
                        .runs
                        .get(run)
                        .and_then(|r| r.steps.get(step))
                        .is_some_and(|s| s.status == crate::engine::run::StepStatus::Suspended);
                    if !suspended {
                        ends.push((
                            p.target.clone(),
                            End::Prune(task.clone(), "the asking step resolved without an answer"),
                        ));
                        continue;
                    }
                }
                // The asking CHILD is gone (it crashed, was killed, its turn was
                // torn down). Nothing can receive the answer any more — the
                // `ToolResult` slot died with the process — so leaving the gate
                // open would park an operator in front of an answerable question
                // whose answer goes nowhere, for the rest of the 24 h ask
                // timeout. Fail it explicitly: the task leaves `input-required`,
                // and a later reply on it continues the conversation as a fresh
                // turn — the documented degrade for a turn's gate.
                Target::Child(node, _) if self.children.get(*node).is_none() => {
                    ends.push((
                        p.target.clone(),
                        End::Prune(
                            task.clone(),
                            "the asking turn ended before the gate was answered",
                        ),
                    ));
                    continue;
                }
                Target::Child(..) => {}
            }
            if now >= *deadline_ms {
                if auto && !auto_fired {
                    fire_auto.push(p.target.clone());
                } else {
                    ends.push((p.target.clone(), End::Timeout));
                }
            }
        }
        for target in fire_auto {
            let Some(p) = self.pending.iter_mut().find(|p| p.target == target) else {
                continue;
            };
            let PendingKind::Human {
                task,
                question,
                deadline_ms,
                auto_fired,
                ..
            } = &mut p.kind
            else {
                continue;
            };
            *auto_fired = true;
            *deadline_ms = now + AUTO_GRACE_MS;
            let (task, question) = (task.clone(), question.clone());
            #[cfg(feature = "a2a")]
            {
                use crate::a2a::tasks::State;
                if let Some(t) = self.tasks.get_mut(&task) {
                    t.transition(
                        State::InputRequired,
                        Some("auto-answering (no human reply in time)…".to_string()),
                    );
                }
                self.task_sync(&task);
            }
            self.spawn_human_judge(&task, &question);
        }
        for (target, end) in ends {
            // Re-find the entry by target on every iteration: ending an earlier
            // gate can reenter and remove entries, so any index captured before
            // the loop would be stale. A missing entry means that gate is
            // already settled, so skip it.
            let Some(i) = self
                .pending
                .iter()
                .position(|p| p.target == target && matches!(&p.kind, PendingKind::Human { .. }))
            else {
                continue;
            };
            match end {
                End::Prune(task, why) => {
                    self.pending.remove(i);
                    self.log
                        .warn("human.ask.pruned", json!({"task": task, "err": why}));
                    self.human_task_fail(&task, why);
                }
                End::Timeout => self.human_fail(i, "ask_human: no answer within the timeout"),
            }
        }
    }

    /// Spawn the `auto` judge on a background thread — same shape as the goal
    /// judge: an intel dial folded back through [`super::events::Event::Background`].
    pub(crate) fn spawn_human_judge(&mut self, ask: &str, question: &str) {
        let uri = self.intel_uri.clone();
        let token = self.current_intel_bearer();
        let headers = self.intel_headers.clone();
        let aws_auth = self.intel_aws_auth();
        let dialect = self.intel_dialect();
        let model = self.model.clone();
        let tx = self.events_tx.clone();
        let (ask, question) = (ask.to_string(), question.to_string());
        self.log.info(
            "human.judge.start",
            json!({"ask": ask, "question": question}),
        );
        std::thread::Builder::new()
            .name("human-judge".into())
            .spawn(move || {
                let result =
                    human_judge_call(&uri, token, &headers, aws_auth, dialect, &model, &question);
                let _ = tx.send(super::events::Event::Background {
                    id: format!("human.judge:{ask}"),
                    result,
                });
            })
            .ok();
    }

    /// The judge came back: answer the ask on the operator's behalf, or fail it.
    pub(crate) fn on_human_judge(&mut self, ask: &str, result: &Value) {
        let Some(i) = self
            .pending
            .iter()
            .position(|p| matches!(&p.kind, PendingKind::Human { task, .. } if task == ask))
        else {
            return; // answered by a human / cancelled while the judge ran
        };
        match result["answer"].as_str() {
            Some(a) if !a.trim().is_empty() && a.trim() != UNDECIDED => {
                let answer = a.trim().to_string();
                self.human_answer(i, &answer, "auto", None);
            }
            Some(_) => self.human_fail(i, "ask_human: the auto judge could not decide (UNDECIDED)"),
            None => {
                let err = result["error"].as_str().unwrap_or("no answer").to_string();
                self.human_fail(i, &format!("ask_human: auto judge failed: {err}"));
            }
        }
    }

    /// Rebuild run-linked gates after a restore: a durable task in
    /// `input-required` whose run has a suspended `human` step re-arms the
    /// pending ask, so the answer path works across restarts. Turn-linked
    /// gates are NOT re-armed — no child survives the restart to receive the
    /// answer, so an answer simply continues the conversation as a fresh turn.
    #[cfg(feature = "a2a")]
    pub(crate) fn rebuild_human_asks(&mut self) {
        use crate::a2a::tasks::{Link, State};
        // (task, run, step, question, deadline, schema, addressee)
        type RestoredGate = (
            String,
            String,
            String,
            String,
            u64,
            Option<Value>,
            Option<crate::a2a::principals::Addressee>,
        );
        let gates: Vec<RestoredGate> = self
            .tasks
            .values()
            .filter(|t| t.state == State::InputRequired)
            .filter_map(|t| match &t.link {
                Link::Run { id } => {
                    let r = self.runs.get(id)?;
                    let (step_id, wait) = r.steps.iter().find_map(|(sid, s)| {
                        (s.status == crate::engine::run::StepStatus::Suspended
                            && s.wait.as_ref()?.get("kind")?.as_str()? == "human")
                            .then(|| (sid.clone(), s.wait.clone().unwrap_or(Value::Null)))
                    })?;
                    let question = t.message.clone().unwrap_or_default();
                    let deadline_ms = wait
                        .get("deadline_ms")
                        .and_then(Value::as_u64)
                        .unwrap_or_else(|| now_ms() + ASK_TIMEOUT.as_millis() as u64);
                    // The gate's enforcement, read back from the durable wait
                    // record: a restart must not weaken a gate.
                    let schema = wait.get("schema").cloned().filter(|v| !v.is_null());
                    let addressee = wait
                        .get("to")
                        .filter(|v| !v.is_null())
                        .and_then(|v| crate::a2a::principals::Addressee::parse(v).ok());
                    Some((
                        t.id.clone(),
                        id.clone(),
                        step_id,
                        question,
                        deadline_ms,
                        schema,
                        addressee,
                    ))
                }
                _ => None,
            })
            .collect();
        for (task, run, step, question, deadline_ms, schema, addressee) in gates {
            self.log.info(
                "human.ask.restored",
                json!({"task": task, "run": run, "step": step}),
            );
            self.push_pending(super::reactor::PendingTool {
                target: Target::Step(run, step),
                name: "human".into(),
                kind: PendingKind::Human {
                    task,
                    question,
                    deadline_ms,
                    standalone: false,
                    auto_fired: false,
                    schema,
                    addressee,
                },
                started_ms: now_ms(),
            });
        }
    }
}

/// The auto-judge intel dial: answer `question` on the operator's behalf.
fn human_judge_call(
    uri: &str,
    token: Option<String>,
    headers: &[(String, String)],
    aws_auth: Option<crate::config::AuthSpec>,
    dialect: Option<String>,
    model: &str,
    question: &str,
) -> Value {
    let client = match IntelClient::from_parts(uri, token) {
        Ok(c) => {
            #[allow(unused_mut)]
            let mut c = c
                .with_headers(headers.to_vec())
                .with_dialect(dialect.as_deref());
            #[cfg(feature = "oauth")]
            if let Some(aws) = &aws_auth
                && let Ok(s) = crate::auth::aws::SigV4Signer::from_spec(aws, "intelligence")
            {
                c = c.with_signer(Some(s as std::sync::Arc<dyn ::mcp::http::RequestSigner>));
            }
            #[cfg(not(feature = "oauth"))]
            let _ = &aws_auth;
            c
        }
        Err(e) => return json!({"error": format!("intel: {e}")}),
    };
    let system = "You are answering ON BEHALF OF the unavailable human operator of an \
autonomous agent. The agent asked the operator a question. Decide pragmatically and \
conservatively: prefer the safe, reversible choice; never approve destructive or \
irreversible actions on the operator's behalf. Reply with ONLY the answer text the \
operator would give — no preamble. If you genuinely cannot decide, reply exactly \
UNDECIDED.";
    let req = Request {
        model: model.to_string(),
        messages: vec![
            Message::System(system.to_string()),
            Message::User(format!("QUESTION FOR THE OPERATOR:\n{question}")),
        ],
        tools: vec![],
        max_tokens: 400,
        temperature: Some(0.0),
    };
    match client.complete(&req) {
        Ok(resp) => json!({"answer": resp.text.unwrap_or_default()}),
        Err(e) => json!({"error": format!("intel: {e}")}),
    }
}
