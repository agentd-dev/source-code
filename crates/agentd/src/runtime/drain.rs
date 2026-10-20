// SPDX-License-Identifier: AGPL-3.0-only
//! The **graceful drain**: what runs on, what it stops, and what it leaves for
//! the next life.
//!
//! A drain admits no new work: no turn dispatches, no start fires, and no step
//! starts but a `lifecycle.shutdown` workflow's. The work already running runs
//! on until `lifecycle.drain_timeout` — a conversation turn, a step's turn, the
//! thinks serving them, a subagent, a call out on an executor thread — and its
//! result is recorded as usual; a run then parks at its next step,
//! checkpointed, and resumes in the next life. What has no end of its own is
//! asked to stop as the drain begins: a warm subagent is cancelled, and an
//! instance-tier child is sent `SIGTERM` and drains itself.
//!
//! When the timeout passes or a second `SIGTERM`/`SIGINT` comes, the drain is
//! forced: what still runs is stopped by the kill ladder, `lifecycle.shutdown`
//! runs included, and a shutdown run it cuts short is cancelled, since it
//! belongs to this life. An exit asked for stops what runs as well, but for
//! those runs, which exist to run now and are waited for within the timeout.
//! Nothing else the drain stopped is failed or closed. A step whose worker it
//! stopped stays `running` in the final checkpoint, so the restore replays it
//! as it replays one a crash caught running; a turn it stopped leaves its
//! inbox event pending, so the next life answers it, and a gate it had open
//! waits for the replay to ask again; a subagent it stopped records `parked`,
//! so the next life spawns it again — but for a `sync` one a stopped turn
//! waited on, which the turn's replay asks for again. A child that ends on its
//! own with a real failure during the drain still fails as before: only the
//! drain's own stopping is exempt.

use super::children::ChildKind;
use super::reactor::{ABANDON_GRACE, PendingKind, Runtime, Target, is_terminal_status};
use crate::engine::StepStatus;
use crate::engine::model::{Step, Workflow};
use crate::obs::trace::TraceContext;
use crate::state::now_ms;
use crate::supervisor::reap::{Reaped, WaitOutcome};
use crate::supervisor::tree::NodeId;
use crate::wire::intel::Usage;
use serde_json::{Value, json};
use std::sync::atomic::Ordering;
use std::time::Instant;

/// What a drain stopped, for the one `drain.interrupted` line that says what
/// the next life takes up.
#[derive(Debug, Default)]
pub(crate) struct Left {
    /// `run/step` of each step left `running` — its turn stopped, or its call
    /// still out on an executor thread. The restore replays each.
    steps: Vec<String>,
    /// Each conversation turn stopped, with the inbox event that replays it.
    turns: Vec<Value>,
    /// The purpose of each think worker stopped: `preflight`, `compaction`,
    /// `tool` or `decide`.
    thinks: Vec<String>,
    /// Each subagent parked, instance-tier children included.
    subagents: Vec<String>,
    /// The `lifecycle.shutdown` runs a forced drain cut short. These are
    /// cancelled, not left: the next life does not run this one's shutdown.
    runs: Vec<String>,
}

/// Whether `s` is an `event {on: lifecycle.shutdown}` start.
fn shutdown_start(s: &Step) -> bool {
    s.kind == "event" && s.field_str("on") == Some("lifecycle.shutdown")
}

/// Whether `wf` declares a `lifecycle.shutdown` start: a deinit workflow, which
/// exists to run during a drain.
fn shutdown_workflow(wf: &Workflow) -> bool {
    wf.start_steps().iter().any(|s| shutdown_start(s))
}

impl Left {
    fn is_empty(&self) -> bool {
        !self.leaves_work() && self.runs.is_empty()
    }

    /// Whether the drain left work for the next life: a cancelled shutdown
    /// run is not such work.
    fn leaves_work(&self) -> bool {
        !self.steps.is_empty()
            || !self.turns.is_empty()
            || !self.thinks.is_empty()
            || !self.subagents.is_empty()
    }
}

impl Runtime {
    /// Ask what has no end of its own to stop: a warm subagent, which waits
    /// for its next message for ever, and an instance-tier child, a daemon
    /// that drains itself on `SIGTERM`. Called as the drain begins, and on
    /// each tick of it for one a running turn has started since.
    pub(crate) fn stop_endless(&mut self) {
        let warm: Vec<NodeId> = self
            .subagents
            .values()
            .filter(|s| s.mode == "warm" && !is_terminal_status(&s.status))
            .filter_map(|s| s.node)
            .collect();
        for node in warm {
            self.children.stop(node, &self.drain_reason);
        }
        let daemons: Vec<(String, i32)> = self
            .subagents
            .values()
            .filter(|s| {
                s.tier.as_deref() == Some("instance")
                    && !is_terminal_status(&s.status)
                    && s.retiring_since.is_none()
                    && !self.drain_instances.contains_key(&s.handle)
            })
            .filter_map(|s| Some((s.handle.clone(), s.pid?)))
            .collect();
        for (handle, pid) in daemons {
            self.subagent_log(&handle)
                .info("instance.drain", json!({"handle": handle, "pid": pid}));
            #[cfg(unix)]
            unsafe {
                libc::killpg(pid, libc::SIGTERM);
            }
            self.drain_instances.insert(handle, false);
        }
    }

    /// Instance-tier children the drain waits for: the ones it asked to drain
    /// and the ones already retiring, while their processes run.
    fn instances_draining(&self) -> Vec<(String, i32)> {
        self.subagents
            .values()
            .filter(|s| {
                s.tier.as_deref() == Some("instance")
                    && !is_terminal_status(&s.status)
                    && (s.retiring_since.is_some() || self.drain_instances.contains_key(&s.handle))
            })
            .filter_map(|s| Some((s.handle.clone(), s.pid?)))
            .collect()
    }

    /// Whether the drain is forced: its timeout passed, or a second
    /// `SIGTERM`/`SIGINT` came. A forced drain stops everything that runs.
    pub(crate) fn drain_force(&self) -> bool {
        self.draining
            && (crate::signals::force()
                || self
                    .drain_started
                    .is_some_and(|t| t.elapsed() >= self.settings.lifecycle.drain_timeout()))
    }

    /// Whether `run` is a live run of a deinit workflow (see
    /// [`Runtime::shutdown_runs_live`] for why any of its runs counts).
    fn shutdown_run(&self, run: &str) -> bool {
        self.runs
            .get(run)
            .filter(|r| !r.status.is_terminal())
            .and_then(|r| self.definition_for_run_ref(&r.workflow, &r.workflow_hash))
            .is_some_and(shutdown_workflow)
    }

    /// Whether a child works for a `lifecycle.shutdown` run: its step's turn,
    /// a think answering that turn, or a subagent one of its steps started.
    /// An exit asked for lets these run.
    fn serves_shutdown(&self, kind: &ChildKind) -> bool {
        match kind {
            ChildKind::StepTurn { run, .. } => self.shutdown_run(run),
            ChildKind::Think {
                reply_to: Some((node, _)),
                ..
            } => self
                .children
                .get(*node)
                .is_some_and(|c| self.serves_shutdown(&c.kind)),
            ChildKind::Subagent { handle } => self
                .subagents
                .get(handle)
                .and_then(|s| s.requested_by.as_ref())
                .and_then(|by| by["run"].as_str())
                .is_some_and(|run| self.shutdown_run(run)),
            _ => false,
        }
    }

    /// Whether stopping what runs now cuts work short: a child that owes a
    /// result (one that has reported it is only on its way out), a call out
    /// on an executor thread, an `emit … forward:` push. A forced drain
    /// (`all`) cuts all of it, and the instance-tier children and
    /// `lifecycle.shutdown` runs still running too. An exit asked for lets
    /// what serves a shutdown run finish and waits for the instance-tier
    /// children, so neither counts; nor does a child an earlier stop already
    /// parked.
    fn drain_cuts(&self, all: bool) -> bool {
        let owes = self
            .children
            .iter()
            .filter(|(_, c)| all || (c.stop.is_none() && !self.serves_shutdown(&c.kind)))
            .any(|(_, c)| match &c.kind {
                ChildKind::Subagent { handle } => self
                    .subagents
                    .get(handle)
                    .is_some_and(|s| !is_terminal_status(&s.status) && s.status != "parked"),
                _ => !c.settled,
            });
        let out = self.executing.keys().any(|k| {
            all || !k
                .split_once('/')
                .is_some_and(|(run, _)| self.shutdown_run(run))
        });
        owes || out
            || self.forwards_in_flight.load(Ordering::SeqCst) > 0
            || (all && (!self.instances_draining().is_empty() || self.shutdown_runs_live() > 0))
    }

    /// Stop the children still let run: with `all` every one, else every one
    /// but those that serve a `lifecycle.shutdown` run.
    fn stop_running(&mut self, all: bool) {
        let nodes: Vec<NodeId> = self
            .children
            .iter()
            .filter(|(_, c)| c.stop.is_none() && (all || !self.serves_shutdown(&c.kind)))
            .map(|(n, _)| *n)
            .collect();
        for node in nodes {
            self.children.stop(node, &self.drain_reason);
        }
    }

    /// Whether the drain holds back the next steps of a run of `wf`: every
    /// run's but a `lifecycle.shutdown` workflow's, which exists to run during
    /// the drain — until the drain is forced and stops what runs.
    pub(crate) fn drain_holds(&self, wf: &Workflow) -> bool {
        self.draining && (self.drain_force() || !shutdown_workflow(wf))
    }

    /// One tick of a drain. Returns the exit code once it is over.
    pub(crate) fn drain_step(&mut self) -> Option<i32> {
        let timeout = self.settings.lifecycle.drain_timeout();
        let started = self.drain_started.unwrap_or_else(Instant::now);
        let force = self.drain_force();
        // An exit asked for — a `finish` with `exit: true`, a policy, a store
        // that cannot write — stops what runs rather than waiting on it, but
        // for the `lifecycle.shutdown` runs, which exist to run now.
        let exiting = self.exit.is_some();
        self.stop_endless();
        if force || exiting {
            if !self.drain_forced && self.drain_cuts(force) {
                self.drain_forced = true;
            }
            self.stop_running(force);
        }
        if force {
            let unkilled: Vec<(String, i32)> = self
                .instances_draining()
                .into_iter()
                .filter(|(h, _)| self.drain_instances.get(h) != Some(&true))
                .collect();
            for (handle, pid) in unkilled {
                #[cfg(unix)]
                unsafe {
                    libc::killpg(pid, libc::SIGKILL);
                }
                self.drain_instances.insert(handle, true);
            }
        }
        let signalled = self.children.drive_stops(force);
        self.log_kill_escalations(&signalled);
        let gone = self.children.is_empty() && self.instances_draining().is_empty();
        // Unless forced, the drain waits for its `lifecycle.shutdown` runs,
        // and, letting work run, for what has no process of its own: a call
        // out on an executor thread, an `emit … forward:` push.
        let done = gone
            && (force
                || (self.shutdown_runs_live() == 0
                    && (exiting
                        || (self.executing.is_empty()
                            && self.forwards_in_flight.load(Ordering::SeqCst) == 0))));
        if !done && started.elapsed() < timeout + ABANDON_GRACE {
            return None;
        }
        if !done {
            self.log
                .warn("drain.abandon", json!({"children": self.children.len()}));
            for (node, kind, trace, settled, parked) in self.children.abandon() {
                if parked && !settled {
                    self.park_child(node, &kind, trace.as_ref(), None);
                }
            }
            self.drain_forced = true;
        }
        // A deinit run the force cut short is cancelled: it belongs to this
        // life's shutdown, and the next life must not run it at its boot.
        let cut: Vec<String> = self
            .runs
            .values()
            .filter(|r| !r.status.is_terminal())
            .filter(|r| {
                self.definition_for_run_ref(&r.workflow, &r.workflow_hash)
                    .and_then(|w| w.step(&r.start.node))
                    .is_some_and(shutdown_start)
            })
            .map(|r| r.id.clone())
            .collect();
        for run in &cut {
            self.cancel_run(
                run,
                "the drain was forced before this lifecycle.shutdown run finished",
            );
        }
        self.drain_left.steps.retain(|s| {
            s.split_once('/')
                .is_none_or(|(run, _)| !cut.iter().any(|c| c == run))
        });
        self.drain_left.runs.extend(cut);
        // What runs without a process of its own is left as it stands: a call
        // still out keeps its step `running`.
        let out: Vec<String> = self
            .executing
            .keys()
            .filter(|k| {
                k.split_once('/')
                    .and_then(|(run, _)| self.runs.get(run))
                    .is_some_and(|r| !r.status.is_terminal())
            })
            .cloned()
            .collect();
        self.drain_left.steps.extend(out);
        crate::obs::metrics::record_drain(if self.drain_forced {
            "forced"
        } else {
            "completed"
        });
        if !self.drain_left.is_empty() {
            let l = &self.drain_left;
            self.log.info(
                "drain.interrupted",
                json!({"steps": l.steps, "turns": l.turns, "thinks": l.thinks,
                       "subagents": l.subagents, "runs": l.runs}),
            );
        }
        self.checkpoint(true);
        self.log
            .info("drain.done", json!({"reason": self.drain_reason}));
        crate::state::hold_point("drain.done");
        Some(self.drain_exit_code())
    }

    /// The kind of `node` when the drain parked it, else `None`.
    pub(crate) fn parked_kind(&self, node: NodeId) -> Option<ChildKind> {
        self.children
            .get(node)
            .filter(|c| c.parked)
            .map(|c| c.kind.clone())
    }

    /// Leave what a child the drain stopped owed for the next life. `trace`
    /// is the span it was spawned under — the child may be reaped already —
    /// and `usage` what its turn spent, when it reported one before it
    /// stopped.
    pub(crate) fn park_child(
        &mut self,
        node: NodeId,
        kind: &ChildKind,
        trace: Option<&TraceContext>,
        usage: Option<Usage>,
    ) {
        self.activity_end(node);
        self.park_gates(node, trace);
        match kind {
            ChildKind::StepTurn {
                run,
                step,
                reservation,
            } => {
                // Left `running`, owned by this worker: the restore resets it
                // to pending and runs it again.
                let worker = node.0.to_string();
                let owned = self
                    .runs
                    .get(run)
                    .filter(|r| !r.status.is_terminal())
                    .and_then(|r| r.step(step))
                    .is_some_and(|s| {
                        s.status == StepStatus::Running && s.worker.as_deref() == Some(&worker)
                    });
                if owned {
                    self.settle_parked(*reservation, usage, &[]);
                    self.drain_left.steps.push(format!("{run}/{step}"));
                }
            }
            ChildKind::RootTurn {
                ctx,
                event,
                reservation,
                ..
            } => {
                // The inbox event stays pending: the next life answers it.
                self.settle_parked(*reservation, usage, &[]);
                self.drain_left
                    .turns
                    .push(json!({"ctx": ctx, "inbox_event": event}));
            }
            ChildKind::Think {
                purpose,
                ctx,
                reservation,
                ..
            } => {
                // Its requester is stopped with it, and a preflight's turn
                // waits for the next life with its inbox event.
                let scopes = match (reservation, &usage, ctx) {
                    (None, Some(_), Some(c)) => self.conversation_scopes(c),
                    _ => Vec::new(),
                };
                self.settle_parked(*reservation, usage, &scopes);
                self.drain_left.thinks.push(purpose.clone());
            }
            ChildKind::Subagent { handle } => {
                let handle = handle.clone();
                self.park_subagent(&handle);
            }
        }
        self.children.mark_settled(node);
    }

    /// Settle what a stopped turn reserved or spent: its usage, when it said
    /// what that was, else the reservation is released.
    fn settle_parked(&mut self, reservation: Option<u64>, usage: Option<Usage>, scopes: &[String]) {
        match (reservation, usage) {
            (Some(r), Some(u)) => self.governor.settle(r, u),
            (Some(r), None) => self.governor.release(r),
            (None, Some(u)) => self.governor.charge(u, scopes),
            (None, None) => {}
        }
    }

    /// Close the gates a stopped child had open without failing them. A gate
    /// on a task its caller owns goes back to `working`: the replay of the
    /// turn or step in the next life asks on that task again, and the answer
    /// is taken then. A gate task opened for this ask alone is cancelled,
    /// since the replay opens its own. Its line is in the asking child's
    /// `trace`.
    fn park_gates(&mut self, node: NodeId, trace: Option<&TraceContext>) {
        let asks = |p: &super::reactor::PendingTool| {
            matches!(p.target, Target::Child(n, _) if n == node)
                && matches!(p.kind, PendingKind::Human { .. })
        };
        let gates: Vec<(String, bool)> = self
            .pending
            .iter()
            .filter(|p| asks(p))
            .filter_map(|p| match &p.kind {
                PendingKind::Human {
                    task, standalone, ..
                } => Some((task.clone(), *standalone)),
                _ => None,
            })
            .collect();
        self.pending.retain(|p| !asks(p));
        #[cfg(feature = "a2a")]
        for (task, standalone) in gates {
            use crate::a2a::tasks::State;
            let Some(t) = self.tasks.get_mut(&task).filter(|t| !t.state.is_terminal()) else {
                continue;
            };
            let (state, why) = if standalone {
                (
                    State::Canceled,
                    "the agent stopped before this was answered; it asks again when it resumes",
                )
            } else {
                (
                    State::Working,
                    "the agent stopped before this was answered; it asks again here when it resumes",
                )
            };
            t.transition(state, Some(why.to_string()));
            self.task_persist(&task);
            self.task_sync(&task);
            self.log.in_trace(trace).info(
                "human.ask.drained",
                json!({"task": task, "state": state.wire()}),
            );
        }
        #[cfg(not(feature = "a2a"))]
        let _ = (gates, trace);
    }

    /// Record a subagent the drain stopped: `parked`, as a rule —
    /// not finished, so the next life spawns it again, a `detached` one too,
    /// which a restart after a crash does not revive. A `sync` child a turn
    /// waited on is `cancelled` instead: that turn is stopped too and asks for
    /// it again when it replays, so a parked one would run twice. Whatever
    /// waits on it in memory is stopped with it, since its answer does not
    /// come in this life.
    fn park_subagent(&mut self, handle: &str) {
        let Some(s) = self
            .subagents
            .get_mut(handle)
            .filter(|s| !is_terminal_status(&s.status) && s.status != "parked")
        else {
            return;
        };
        let asked_again = s.mode == "sync"
            && s.requested_by
                .as_ref()
                .is_some_and(|by| !by["caller"].is_null());
        let status = if asked_again { "cancelled" } else { "parked" };
        s.status = status.into();
        if asked_again {
            s.error = Some(
                "stopped by the drain; the turn that waited on it asks for it again when it resumes"
                    .into(),
            );
        }
        s.node = None;
        s.pid = None;
        s.updated = now_ms();
        s.dirty = true;
        let (tokens, error) = (s.tokens, s.error.clone());
        if asked_again {
            self.subagent_log(handle).info(
                "subagent.result",
                json!({"handle": handle, "status": status, "tokens": tokens, "err": error}),
            );
        } else {
            self.drain_left.subagents.push(handle.to_string());
        }
        let waiters: Vec<NodeId> = self
            .pending
            .iter()
            .filter(|p| matches!(&p.kind, PendingKind::Subagent { handle: h } if h == handle))
            .filter_map(|p| match p.target {
                Target::Child(n, _) => Some(n),
                Target::Step(..) | Target::McpTask(..) => None,
            })
            .collect();
        for n in waiters {
            self.children.stop(n, &self.drain_reason);
        }
    }

    /// An instance-tier child exited while the drain was waiting for it.
    /// Parked when it drained as asked (exit 0) or by the drain's own signal;
    /// `false` when it ended some other way, which is closed as before.
    pub(crate) fn park_drained_instance(&mut self, handle: &str, r: &Reaped) -> bool {
        let Some(&killed) = self.drain_instances.get(handle) else {
            return false;
        };
        if self
            .subagents
            .get(handle)
            .is_none_or(|s| s.retiring_since.is_some())
        {
            return false;
        }
        let ours = match r.outcome {
            WaitOutcome::Exited(0) => true,
            WaitOutcome::Signaled(sig) => sig == libc::SIGTERM || (killed && sig == libc::SIGKILL),
            WaitOutcome::Exited(_) => false,
        };
        if !ours {
            return false;
        }
        self.park_subagent(handle);
        let status = self.subagents.get(handle).map(|s| s.status.clone());
        self.subagent_log(handle).info(
            "instance.exited",
            json!({"handle": handle, "pid": r.pid, "status": status}),
        );
        true
    }

    /// The code a drain exits with. An exit asked for keeps its own code. A
    /// daemon drains to 0: its runs' outcomes are in their records, and what
    /// the drain parked resumes in the next life. A job's code is its work's,
    /// as at an idle exit, so a run the drain left unfinished reads 3 — a job
    /// that ended with a run unfinished — and so does a turn or a subagent it
    /// stopped, or work it never let start. A Job controller is never told a
    /// job succeeded that the drain cut short.
    fn drain_exit_code(&self) -> i32 {
        if let Some(code) = self.exit {
            return code;
        }
        if !self.job_now() {
            return crate::exit::SUCCESS;
        }
        let code = self.job_exit_code();
        let unfinished = self.drain_left.leaves_work()
            || !self.turn_queue.is_empty()
            || !self.staged_turns.is_empty()
            || !self.inbox_queue.is_empty();
        if code == crate::exit::SUCCESS && unfinished {
            return crate::exit::apply_budget_remap(
                crate::exit::PARTIAL,
                self.settings.lifecycle.exit_code_map.get("3").copied(),
            );
        }
        code
    }
}
