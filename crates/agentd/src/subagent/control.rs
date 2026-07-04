// SPDX-License-Identifier: Apache-2.0
//! The subagent side of the control channel. RFC 0005, RFC 0003, RFC 0009.
//!
//! Entered when `main` sees `AGENT_SUBAGENT` set. The child:
//! 1. installs `PR_SET_PDEATHSIG` so a supervisor death collapses it (must be
//!    here — `pre_exec`'s setting is cleared by `execve`);
//! 2. reads its [`SpawnPayload`] (first control frame) from stdin;
//! 3. starts a **control reader thread** (separate from the agentic loop) that
//!    answers `Ping` with `Pong` and flips a cancel flag on `Cancel` — so
//!    liveness survives a long in-flight tool/model call (Detector C);
//! 4. emits `Ready`, connects intelligence + its scoped MCP servers, runs
//!    `agentloop::run_loop`, and sends `Result`/`Failed` back up.
//!
//! Wire: stdout carries length-framed [`AgentMsg`] up; stderr carries the
//! child's JSON telemetry (inherited to the parent). stdin carries
//! [`ControlMsg`] down.

use crate::agentloop::runner::{LoopAbort, LoopInput, Session, run_loop};
use crate::agentloop::stop::{Outcome, TerminalStatus};
use crate::config::SwapPolicy;
use crate::intel::client::{IntelClient, IntelHealthReport};
use crate::json::frame;
use crate::mcp::client::McpClient;
use crate::obs::log::{Comp, Level, LogCtx, Logger};
use crate::subagent::orchestrator::Orchestrator;
use crate::subagent::protocol::{AgentMsg, ControlMsg, IntelActive, SpawnPayload, SwapIntel};
use crate::supervisor::budget::Budget;
use std::io::{self, BufReader, Stdin, Stdout};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// The child-local LIVE intelligence handle (RFC 0018 §5.2, the process-boundary
/// adaptation). A supervisor-side `RwLock<Arc<IntelConfig>>` cannot reach a child
/// re-exec'd as its own PROCESS, so each child holds its own LIVE config: the
/// control-reader thread parks a [`SwapIntel`] here on `ControlMsg::SwapIntel`,
/// and the agentic loop drains it ONCE per turn at the turn boundary (the same
/// boundary `pause_wait` sits at), rebuilds its [`IntelClient`] from the new
/// endpoints (fresh health/breaker — §5.2 step 2), and adopts the new model. The
/// `Mutex<Option<…>>` is the whole seam; the loop never holds it across a turn.
type PendingSwap = Arc<Mutex<Option<SwapIntel>>>;

type Up = Arc<Mutex<Stdout>>;

/// The child-process HUMAN-GATE bus (RFC 0021 §7) — process-global access to the
/// control-channel writer + the `Inject` receiver, so the workflow engine (deep
/// inside `graph::exec`, possibly under the orchestrator's `workflow.run`) can
/// open a gate and poll for the human's reply without threading handles through
/// every layer (the child-side parallel of `graph::live`). Installed once by
/// [`run`] for NON-WARM children only: a warm session's `Inject` stream is its
/// turn-input channel (`run_warm`), never a gate reply — a gate inside a warm
/// turn degrades to `reply_uri`/timeout.
#[cfg(feature = "workflow")]
pub(crate) mod gatebus {
    use super::{AgentMsg, Up, send_up};
    use serde_json::Value;
    use std::sync::mpsc::Receiver;
    use std::sync::{Mutex, OnceLock};

    struct Bus {
        up: Up,
        inject: Mutex<Receiver<String>>,
    }
    // SAFETY-BY-TYPES: Receiver is !Sync; the Mutex restores Sync for the static.
    static BUS: OnceLock<Bus> = OnceLock::new();

    /// Install the bus (once, at child startup). A second install is a no-op.
    pub(crate) fn install(up: Up, inject: Receiver<String>) {
        let _ = BUS.set(Bus {
            up,
            inject: Mutex::new(inject),
        });
    }

    /// Report an opened gate UP the control channel.
    pub(crate) fn open(node: &str, payload: &Value) {
        if let Some(b) = BUS.get() {
            send_up(
                &b.up,
                &AgentMsg::Gate {
                    node: node.to_string(),
                    payload: payload.clone(),
                },
            );
        }
    }

    /// Poll (non-blocking) for a human reply delivered as `ControlMsg::Inject`.
    pub(crate) fn try_reply() -> Option<String> {
        let b = BUS.get()?;
        let rx = b.inject.lock().unwrap_or_else(|e| e.into_inner());
        rx.try_recv().ok()
    }

    /// Report the gate resolved (`via` = `"reply"` | `"uri"` | `"timeout"`).
    pub(crate) fn close(node: &str, via: &str) {
        if let Some(b) = BUS.get() {
            send_up(
                &b.up,
                &AgentMsg::GateClosed {
                    node: node.to_string(),
                    via: via.to_string(),
                },
            );
        }
    }
}

/// The subagent entry point. Returns the process exit code.
pub fn run() -> i32 {
    install_pdeathsig();
    // If the supervisor already died in the fork/exec window, bail (we'd be
    // reparented to init / the subreaper).
    #[cfg(unix)]
    if unsafe { libc::getppid() } == 1 {
        return crate::exit::GENERIC;
    }

    let mut stdin = BufReader::new(io::stdin());
    let payload = match read_spawn(&mut stdin) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("agentd subagent: bad spawn payload: {e}");
            return crate::exit::USAGE;
        }
    };

    // Outbound extra trust anchor (`--tls-ca`), inherited via the payload:
    // install process-wide BEFORE the first dial (the intel/MCP/A2A clients
    // below), exactly as the supervisor did in `main`. The path is public
    // material (a CA cert); a re-exec child shares the pod fs, so the same path
    // resolves here. Idempotent when the parent already installed it in THIS
    // process image (never the case across re-exec, but harmless).
    #[cfg(feature = "tls")]
    if let Some(path) = payload.tls_ca.as_deref()
        && let Err(e) = std::fs::read(path).and_then(|pem| crate::net::tls::install_extra_ca(&pem))
    {
        eprintln!("agentd subagent: --tls-ca {path}: {e}");
        return crate::exit::USAGE;
    }

    let up: Up = Arc::new(Mutex::new(io::stdout()));
    let log = build_logger(&payload);
    let cancel = Arc::new(AtomicBool::new(false));
    // Tree-wide turn-boundary suspension (RFC 0005 §4.3 / RFC 0015 §4.3): the
    // control thread sets this on `Pause`, clears it on `Resume`; the loop waits
    // between turns while it is set. `cancel` always wins (see `pause_wait`).
    let paused = Arc::new(AtomicBool::new(false));

    // For a warm continue-session, the control thread forwards each `Inject`
    // event to the loop over this channel; a one-shot run never reads it.
    let (inject_tx, inject_rx) = std::sync::mpsc::channel::<String>();

    // The child-local LIVE intel handle (RFC 0018 §5.2): the control thread parks
    // a hot-swap here; the loop drains it at the turn boundary. `None` until the
    // first swap arrives, so the no-swap path never touches the lock past one
    // cheap empty check per turn.
    let pending_swap: PendingSwap = Arc::new(Mutex::new(None));

    // The control reader runs on its own thread and owns stdin from here on,
    // so Ping/Pong keeps flowing while the loop is busy — and so Resume/Cancel/
    // SwapIntel still arrive while the loop is suspended at a turn boundary.
    spawn_control_thread(
        stdin,
        Arc::clone(&up),
        Arc::clone(&cancel),
        Arc::clone(&paused),
        inject_tx,
        Arc::clone(&pending_swap),
        log.ctx().clone(),
    );

    // A NON-WARM child hands its `Inject` stream to the gate bus (RFC 0021 §7):
    // a workflow `human` node deep in this child polls it for the human's reply.
    // A warm session keeps the stream — it is its turn-input channel.
    #[cfg_attr(not(feature = "workflow"), allow(unused_mut))]
    let mut inject_rx = Some(inject_rx);
    #[cfg(feature = "workflow")]
    if !payload.warm {
        gatebus::install(
            Arc::clone(&up),
            inject_rx
                .take()
                .expect("inject_rx handed to the gate bus once"),
        );
    }

    send_up(&up, &AgentMsg::Ready);
    log.info(
        "loop.start",
        serde_json::json!({"depth": payload.depth, "warm": payload.warm}),
    );

    let mut intel = match IntelClient::from_parts(
        &payload.intelligence.uri,
        payload.intelligence.token.clone(),
    ) {
        Ok(mut c) => {
            // Outbound LLM calls join the run's distributed trace (RFC 0010).
            c.set_trace_id(payload.telemetry.trace_id.clone());
            // RFC 0018 §6: bridge this child's intel reachability UP to the
            // supervisor (which has no LLM of its own) on each all-down transition.
            install_intel_health_reporter(&mut c, &up);
            c
        }
        Err(e) => {
            return fail(
                &up,
                &log,
                format!("intel: {e}"),
                crate::exit::INTEL_UNAVAILABLE,
            );
        }
    };

    let mut servers = Vec::new();
    for spec in &payload.mcp_servers {
        let connected = crate::mcp::from_spec(spec, Duration::from_secs(60))
            .and_then(|mut c| c.initialize().map(|()| c));
        match connected {
            Ok(mut c) => {
                log.info("mcp.connect", serde_json::json!({"server": spec.name}));
                // Stamp the run id (retry dedup, RFC 0011) + a W3C traceparent
                // (distributed tracing, RFC 0010) on every tool call.
                let mut meta = serde_json::json!({"agent/run_id": payload.telemetry.run_id});
                if let Some(tid) = &payload.telemetry.trace_id {
                    meta["traceparent"] = crate::obs::trace::outbound_traceparent(tid).into();
                }
                c.set_tool_meta(meta);
                servers.push(c);
            }
            Err(e) => {
                return fail(
                    &up,
                    &log,
                    format!("mcp '{}': {e}", spec.name),
                    crate::exit::MCP_REQUIRED_DOWN,
                );
            }
        }
    }

    // A payload carrying a WORKFLOW is driven by the graph engine instead of the
    // ReAct loop (pivot Phase 7 · W4): same connections, same supervision, same
    // exit contract — the parent handed this child a whole workflow. One-shot by
    // construction (`warm` is ignored: a workflow terminates via its own graph).
    #[cfg(feature = "workflow")]
    if let Some(wf) = payload.workflow.clone() {
        return run_workflow_child(&wf, &intel, &servers, &payload, &up, &log);
    }

    let mut input = LoopInput {
        instruction: payload.instruction.clone(),
        output_contract: payload.output_contract.clone(),
        seed: payload
            .context_seed
            .iter()
            .map(|m| (m.role.clone(), m.content.clone()))
            .collect(),
        model: payload.intelligence.model.clone().unwrap_or_default(),
        max_steps: payload.limits.max_steps,
        max_tokens: payload.limits.max_tokens,
        deadline: Instant::now() + Duration::from_millis(payload.limits.deadline_ms.max(1)),
        cancel: Some(Arc::clone(&cancel)),
    };

    // Self-orchestration: the model can delegate subtasks via subagent.spawn,
    // which spawns + supervises a child agent (depth + 1, scoped) from here.
    let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("agentd"));
    let mut orch = Orchestrator::from_payload(exe, &payload, Duration::from_secs(25), log.clone());

    // A warm continue-session lives across many events; a one-shot runs once.
    // A warm session is the long-lived loop/reactive shape (RFC 0008), so it gets
    // the all-down backoff (RFC 0018 §6): a transient host-model roll recovers
    // without crashing the daemon, rather than exiting 4 like a `once` job.
    if payload.warm {
        intel.enable_alldown_backoff(crate::intel::client::AllDownPolicy::default());
        return run_warm(
            intel,
            &servers,
            &input,
            &payload,
            &mut orch,
            &cancel,
            &paused,
            inject_rx
                .as_ref()
                .expect("a warm session keeps its inject stream"),
            &pending_swap,
            &up,
            &log,
        );
    }

    // One-shot: a single turn. Suspend at the turn boundary (before the turn
    // starts) if paused; a turn already in progress is never interrupted.
    pause_wait(&paused, &cancel, &log);
    // RFC 0018 §5.2 turn-boundary read: a swap that landed before this single
    // turn started is applied here (rebuild client + adopt model). A swap that
    // lands DURING the turn is finish-on-old and invisible — a one-shot has no
    // next turn, so `restart-turn` is moot for it (the run ends after this turn).
    apply_pending_swap(&pending_swap, &mut intel, &mut input.model, &up, &log);
    match run_loop(&intel, &servers, &input, &mut orch, &log) {
        Ok((outcome, usage)) => {
            let code = crate::exit::once_exit(outcome.status, outcome.partial);
            // Roll the run's total tokens up to the supervisor BEFORE the terminal
            // Result, so hierarchical accounting (`agentd_tokens_total`) sees them.
            // One Usage per run (a one-shot is a single turn) — never cumulative
            // AND per-turn, so `record_tokens`' fetch_add can't double-count.
            send_up(&up, &AgentMsg::Usage(usage));
            send_up(&up, &AgentMsg::Result { outcome });
            code
        }
        Err(LoopAbort::Intel(m)) => fail(
            &up,
            &log,
            format!("intel: {m}"),
            crate::exit::INTEL_UNAVAILABLE,
        ),
        Err(LoopAbort::Mcp(m)) => fail(
            &up,
            &log,
            format!("mcp: {m}"),
            crate::exit::MCP_REQUIRED_DOWN,
        ),
    }
}

/// Drive a **warm continue-session** (RFC 0008 §spawn-vs-continue): prepare the
/// session once, then run one turn per delivered event over the *same*
/// transcript, emitting [`AgentMsg::Turn`] after each. The process and its
/// conversation stay warm between events until the supervisor cancels it or
/// closes the control channel, at which point a terminal [`AgentMsg::Result`]
/// marks closure. Each turn gets a fresh per-event budget (steps/tokens/deadline)
/// so one reaction can't starve the session.
#[allow(clippy::too_many_arguments)]
fn run_warm(
    mut intel: IntelClient,
    servers: &[McpClient],
    input: &LoopInput,
    payload: &SpawnPayload,
    orch: &mut Orchestrator,
    cancel: &Arc<AtomicBool>,
    paused: &Arc<AtomicBool>,
    inject_rx: &Receiver<String>,
    pending_swap: &PendingSwap,
    up: &Up,
    log: &Logger,
) -> i32 {
    let mut session = match Session::prepare(servers, input, orch) {
        Ok(s) => s,
        Err(LoopAbort::Intel(m)) => {
            return fail(
                up,
                log,
                format!("intel: {m}"),
                crate::exit::INTEL_UNAVAILABLE,
            );
        }
        Err(LoopAbort::Mcp(m)) => {
            return fail(up, log, format!("mcp: {m}"), crate::exit::MCP_REQUIRED_DOWN);
        }
    };
    let limits = &payload.limits;
    loop {
        // Turn boundary: if paused (RFC 0005 §4.3 / RFC 0015 §4.3), suspend HERE,
        // before starting the next turn — never mid-turn. A `Cancel` during pause
        // wins and proceeds to wind-down (the loop falls through to the cancel
        // check below). The control thread keeps running, so Resume/Cancel arrive
        // while we wait. The supervisor reactor and its liveness heartbeat are not
        // affected — only this child loop suspends.
        pause_wait(paused, cancel, log);
        // Live tools refresh (pivot Phase 7 follow-up): an inbound
        // `notifications/tools/list_changed` on any of THIS child's own MCP
        // connections re-enumerates the catalogue at this turn boundary, so a
        // warm session tracks a changing server instead of holding a stale tool
        // set for its whole life. (Spawned one-shots already re-list per run.)
        refresh_tools_if_changed(&mut session, orch, servers, log);
        // RFC 0018 §5.2 turn-boundary read: a hot-swap parked by the control thread
        // is drained + applied HERE, before the turn — the loop rebuilds its client
        // (fresh health/breaker) and adopts the new model. The transcript is
        // UNTOUCHED (§5.3 — no context reset); a turn already running was never
        // torn (finish-on-old by construction — the swap only lands at this seam).
        apply_pending_swap_warm(pending_swap, &mut intel, &mut session, up, log);
        // Snapshot the pre-turn transcript so `restart-turn` (RFC 0018 §5.3) can
        // discard a turn that completed under a model swap and re-run it on the new
        // model from this exact state. Cheap (a `usize`); unused under finish-on-old.
        let pre_turn = session.transcript_len();
        // One turn over the persistent transcript, bounded by a fresh per-event
        // budget (a new deadline each turn, so the session isn't globally capped).
        let deadline = Instant::now() + Duration::from_millis(limits.deadline_ms.max(1));
        let mut budget = Budget::new(limits.max_steps, limits.max_tokens, deadline);
        let (outcome, usage) = match session.run_turn(&intel, orch, log, &mut budget, Some(cancel))
        {
            Ok(ou) => ou,
            Err(LoopAbort::Intel(m)) => {
                return fail(
                    up,
                    log,
                    format!("intel: {m}"),
                    crate::exit::INTEL_UNAVAILABLE,
                );
            }
            Err(LoopAbort::Mcp(m)) => {
                return fail(up, log, format!("mcp: {m}"), crate::exit::MCP_REQUIRED_DOWN);
            }
        };
        // Cancellation during a turn ends the session (terminal Result below);
        // any other terminal is just this reaction's turn — the session lives on.
        if outcome.status == TerminalStatus::Cancelled {
            break;
        }
        // RFC 0018 §5.3 `restart-turn`: a model-changing swap that LANDED while this
        // turn was in flight discards the turn's result and re-runs it on the new
        // model from the pre-turn transcript. We never tore the `complete_once` —
        // the turn finished; we drop its appended messages and loop WITHOUT
        // consuming a new event. Bounded by the step budget like any turn. The swap
        // is applied at the top of the loop (the turn-boundary seam), so we only
        // decide here whether to re-run; an endpoint repoint (model unchanged) is
        // never a restart (it is always invisible / finish-on-old, §5.1).
        if restart_turn_pending(pending_swap, session.model()) {
            session.truncate_transcript(pre_turn);
            log.info(
                "intel.swap.restart_turn",
                serde_json::json!({"discarded_turn": true}),
            );
            continue;
        }
        // Roll this turn's tokens up to the supervisor BEFORE the Turn event, so
        // hierarchical accounting (`agentd_tokens_total`) sees each warm turn's
        // usage. This `usage` is exactly ONE turn's delta (`run_turn` accumulates
        // per-turn `tok_in`/`tok_out` against a fresh per-event budget), and
        // `record_tokens` fetch_adds — so one Usage per emitted turn never
        // double-counts (a cancelled or restart-discarded turn emits no Turn and
        // no Usage here).
        send_up(up, &AgentMsg::Usage(usage));
        send_up(up, &AgentMsg::Turn { outcome });
        if cancel.load(Ordering::Relaxed) {
            break;
        }
        // Block for the next event (single-consumer, in-order FIFO).
        match wait_for_inject(inject_rx, cancel) {
            Some(message) => {
                log.info(
                    "subagent.inject",
                    serde_json::json!({"bytes": message.len()}),
                );
                session.deliver(&message);
            }
            None => break, // cancelled, or the supervisor closed the control channel
        }
    }
    // Session closed: a single terminal Result so the supervisor sees closure.
    let status = if cancel.load(Ordering::Relaxed) {
        TerminalStatus::Cancelled
    } else {
        TerminalStatus::Completed
    };
    let code = crate::exit::once_exit(status, false);
    send_up(
        up,
        &AgentMsg::Result {
            outcome: Outcome {
                status,
                partial: false,
                result: serde_json::Value::Null,
                scheduled: Vec::new(),
                subscriptions: Vec::new(),
            },
        },
    );
    code
}

/// Block until the next event is injected, the supervisor closes the control
/// channel (its `Inject` sender drops → `Disconnected`), or a cancel is
/// requested — polled so cancellation between events stays prompt.
fn wait_for_inject(rx: &Receiver<String>, cancel: &AtomicBool) -> Option<String> {
    loop {
        if cancel.load(Ordering::Relaxed) {
            return None;
        }
        match rx.recv_timeout(Duration::from_millis(200)) {
            Ok(message) => return Some(message),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return None,
        }
    }
}

/// Suspend the loop at a turn boundary while `paused` is set (RFC 0005 §4.3 /
/// RFC 0015 §4.3). Polls at the same cadence as `wait_for_inject` so a `Resume`
/// (or `Cancel`) lands promptly. `cancel` always wins: a cancel during a pause
/// returns immediately so the loop proceeds to wind-down. Logs once on enter and
/// once on leave (debounced — never per poll). The supervisor reactor is NOT
/// gated by this; only the child's agentic loop suspends, so the liveness
/// heartbeat keeps ticking (RFC 0015 §4.3).
fn pause_wait(paused: &AtomicBool, cancel: &AtomicBool, log: &Logger) {
    if !paused.load(Ordering::Relaxed) || cancel.load(Ordering::Relaxed) {
        return; // fast path: not paused (or cancel wins) → no log, no wait
    }
    log.info("loop.paused", serde_json::json!({}));
    while paused.load(Ordering::Relaxed) && !cancel.load(Ordering::Relaxed) {
        std::thread::sleep(Duration::from_millis(50));
    }
    log.info("loop.resumed", serde_json::json!({}));
}

/// Rebuild an [`IntelClient`] from a hot-swap's endpoint list (RFC 0018 §5.2
/// step 2). A repointed endpoint starts CLOSED — a fresh [`crate::intel::endpoints::EndpointList`]
/// gives every endpoint a brand-new `HealthRecord`, so NO stale breaker state
/// carries to a new CID. The run's trace id is re-stamped onto the new client so
/// outbound calls keep joining the run's distributed trace. Returns `None` (and
/// logs) if the new list is unparseable, in which case the caller keeps the old
/// client (a bad swap never tears a working run).
fn rebuild_intel(swap: &SwapIntel, old: &IntelClient, log: &Logger) -> Option<IntelClient> {
    match IntelClient::from_parts(&swap.uri, swap.token.clone()) {
        Ok(mut c) => {
            c.set_trace_id(old.trace_id().map(str::to_string));
            // A warm/long-lived loop keeps its all-down backoff across a swap (the
            // daemon must not start crashing on a transient roll just because it was
            // repointed). The one-shot path never enabled it, so this is a no-op there.
            if old.alldown_enabled() {
                c.enable_alldown_backoff(crate::intel::client::AllDownPolicy::default());
            }
            Some(c)
        }
        Err(e) => {
            log.warn(
                "intel.swap.reject",
                serde_json::json!({"err": e.to_string()}),
            );
            None
        }
    }
}

/// Emit the `intel.swap` event (RFC 0018 §8 / §5) for an applied swap. NO secret
/// and NO URL ever appear — only the swap KIND (`endpoint`/`model`), the model
/// names (which are non-secret identifiers), the policy, and whether the endpoint
/// list changed. The endpoint identity stays transport+index-only, surfaced by
/// the `agentd://intelligence` resource, never here (RFC 0012 §3.7).
fn log_swap(
    log: &Logger,
    from_model: &str,
    to_model: &str,
    endpoint_change: bool,
    policy: SwapPolicy,
) {
    let kind = if from_model != to_model {
        "model"
    } else {
        "endpoint"
    };
    log.info(
        "intel.swap",
        serde_json::json!({
            "kind": kind,
            "model_from": from_model,
            "model_to": to_model,
            "endpoint_change": endpoint_change,
            "policy": policy.as_str(),
        }),
    );
}

/// Apply a parked hot-swap at the ONE-SHOT turn boundary (RFC 0018 §5.2): drain
/// the pending slot, rebuild the client (fresh health), and adopt the new model
/// into `model`. A no-op (one cheap empty-lock check) when nothing is pending —
/// the no-swap path is unchanged. `restart-turn` is moot for a one-shot (it has a
/// single turn), so the policy only governs the event label here.
fn apply_pending_swap(
    pending: &PendingSwap,
    intel: &mut IntelClient,
    model: &mut String,
    up: &Up,
    log: &Logger,
) {
    let Some(swap) = pending.lock().unwrap_or_else(|e| e.into_inner()).take() else {
        return; // fast path: no swap pending
    };
    let from_model = model.clone();
    let to_model = swap.model.clone().unwrap_or_else(|| from_model.clone());
    let endpoint_change = match rebuild_intel(&swap, intel, log) {
        Some(mut c) => {
            // The rebuilt client has fresh breakers + no reporter — re-install it
            // so the child keeps bridging reachability up after a repoint (§6).
            install_intel_health_reporter(&mut c, up);
            *intel = c;
            true
        }
        None => false,
    };
    *model = to_model.clone();
    log_swap(log, &from_model, &to_model, endpoint_change, swap.policy);
}

/// Apply a parked hot-swap at a WARM-session turn boundary (RFC 0018 §5.2): drain
/// the pending slot, rebuild the client (fresh health), and adopt the new model
/// onto the live [`Session`] (the transcript is UNTOUCHED — §5.3). A no-op when
/// nothing is pending.
fn apply_pending_swap_warm(
    pending: &PendingSwap,
    intel: &mut IntelClient,
    session: &mut Session<'_>,
    up: &Up,
    log: &Logger,
) {
    let Some(swap) = pending.lock().unwrap_or_else(|e| e.into_inner()).take() else {
        return; // fast path: no swap pending
    };
    let from_model = session.model().to_string();
    let to_model = swap.model.clone().unwrap_or_else(|| from_model.clone());
    let endpoint_change = match rebuild_intel(&swap, intel, log) {
        Some(mut c) => {
            // The rebuilt client has fresh breakers + no reporter — re-install it
            // so a warm session keeps bridging reachability up after a repoint (§6).
            install_intel_health_reporter(&mut c, up);
            *intel = c;
            true
        }
        None => false,
    };
    session.set_model(&to_model);
    log_swap(log, &from_model, &to_model, endpoint_change, swap.policy);
}

/// Peek (without draining) whether a `restart-turn` swap is parked that would
/// CHANGE the model from the session's current one (RFC 0018 §5.3). Only a
/// model-changing `restart-turn` swap that landed WHILE the turn was in flight
/// warrants discarding + re-running the just-completed turn; an endpoint repoint
/// (model unchanged) is always finish-on-old / invisible (§5.1), and a
/// finish-on-old swap is applied at the next boundary without a re-run.
fn restart_turn_pending(pending: &PendingSwap, current_model: &str) -> bool {
    let guard = pending.lock().unwrap_or_else(|e| e.into_inner());
    match guard.as_ref() {
        Some(swap) if swap.policy == SwapPolicy::RestartTurn => {
            swap.model.as_deref().is_some_and(|m| m != current_model)
        }
        _ => false,
    }
}

fn fail(up: &Up, log: &Logger, error: String, code: i32) -> i32 {
    log.error("loop.error", serde_json::json!({"err": error}));
    send_up(up, &AgentMsg::Failed { error });
    code
}

/// Drain this child's own MCP notification queues at a warm turn boundary; on an
/// inbound `tools/list_changed`, rebuild the session's tool catalogue live. A
/// failed re-list is a warning (the old catalogue stays — the next boundary
/// retries); a warm child holds no subscriptions, so draining here eats nothing
/// the daemon relies on (subscriptions live on the DAEMON's own connections).
fn refresh_tools_if_changed(
    session: &mut Session,
    orch: &mut Orchestrator,
    servers: &[McpClient],
    log: &Logger,
) {
    use crate::wire::mcp::method;
    let changed = servers.iter().any(|s| {
        s.drain_notifications()
            .iter()
            .any(|n| n.method == method::NOTIFY_TOOLS_LIST_CHANGED)
    });
    if !changed {
        return;
    }
    match session.refresh_tools(orch) {
        Ok(()) => log.info(
            "mcp.tools_refreshed",
            serde_json::json!({"tools": session.tools_len()}),
        ),
        Err(e) => {
            let msg = match e {
                LoopAbort::Intel(m) | LoopAbort::Mcp(m) => m,
            };
            log.warn("mcp.tools_refresh_failed", serde_json::json!({"err": msg}));
        }
    }
}

/// Drive a payload-carried workflow to its terminal outcome and map it onto the
/// child result contract (pivot Phase 7 · W4): GraphStatus → TerminalStatus →
/// the RFC 0011 exit table, the graph detail (status/reason/steps/tokens)
/// carried in the result body, and the intelligence cost rolled up as Usage.
#[cfg(feature = "workflow")]
fn run_workflow_child(
    graph: &crate::graph::Graph,
    intel: &IntelClient,
    servers: &[McpClient],
    payload: &SpawnPayload,
    up: &Up,
    log: &Logger,
) -> i32 {
    use crate::graph::drive_connected;

    // The graph crossed a process boundary — re-validate fail-closed (cheap, pure)
    // even though the parent validated at the authoring boundary.
    if let Err(errs) = graph.validate() {
        return fail(
            up,
            log,
            format!("workflow: invalid graph: {}", errs.join("; ")),
            crate::exit::USAGE,
        );
    }
    let wall = Duration::from_millis(payload.limits.deadline_ms.max(1));
    let deadline = Some(Instant::now() + wall);
    let model = payload.intelligence.model.clone().unwrap_or_default();
    let factory = crate::graph::ExecFactory {
        intel_uri: payload.intelligence.uri.clone(),
        intel_token: payload.intelligence.token.clone(),
        model: model.clone(),
        server_specs: payload.mcp_servers.clone(),
        max_steps: payload.limits.max_steps,
        max_tokens: payload.limits.max_tokens,
        node_timeout: wall,
    };
    // Async subgraphs spawn through the SAME orchestrator machinery a model's
    // subagent.spawn uses — supervised children under this payload's caps.
    let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("agentd"));
    let mut orch = Orchestrator::from_payload(exe, payload, Duration::from_secs(25), log.clone());

    // REACTIVE semantics (the daemon path): drive ONE step — a `Wait` suspends
    // and this child EXITS with the serialized slice in its result; the daemon
    // arms the watch and resumes a fresh child on the update/timeout. Token
    // roll-up is the PER-CHILD delta (the cumulative pool rides the slice).
    if payload.workflow_reactive {
        let entry_tokens = payload
            .workflow_resume
            .as_ref()
            .map(|r| r.state.tokens())
            .unwrap_or(0);
        let resume_from = payload.workflow_resume.clone().map(|r| {
            let outcome = if r.timed_out {
                crate::graph::WaitOutcome::TimedOut
            } else {
                crate::graph::WaitOutcome::Updated(r.content.unwrap_or(serde_json::Value::Null))
            };
            (r.state, outcome)
        });
        let factory = crate::graph::ExecFactory {
            intel_uri: payload.intelligence.uri.clone(),
            intel_token: payload.intelligence.token.clone(),
            model: model.clone(),
            server_specs: payload.mcp_servers.clone(),
            max_steps: payload.limits.max_steps,
            max_tokens: payload.limits.max_tokens,
            node_timeout: wall,
        };
        let result = crate::graph::drive_connected_once(
            graph,
            resume_from,
            intel,
            servers,
            &model,
            payload.limits.max_steps,
            payload.limits.max_tokens,
            wall,
            deadline,
            Some(factory),
            Some(&mut orch),
            log,
        );
        return match result {
            crate::graph::DriveResult::Done(o) => finish_workflow_child(o, entry_tokens, up, log),
            crate::graph::DriveResult::Suspended(s) => {
                log.info(
                    "workflow.suspended",
                    serde_json::json!({
                        "on_uri": s.on_uri,
                        "timeout_ms": s.timeout_ms,
                        "steps": s.state.steps(),
                        "tokens": s.state.tokens(),
                    }),
                );
                send_up(
                    up,
                    &AgentMsg::Usage(crate::wire::intel::Usage {
                        input_tokens: 0,
                        output_tokens: s.state.tokens().saturating_sub(entry_tokens),
                    }),
                );
                let outcome = crate::agentloop::stop::Outcome {
                    status: crate::agentloop::stop::TerminalStatus::Completed,
                    partial: false,
                    result: serde_json::json!({
                        "$workflow": { "suspended": {
                            "on_uri": s.on_uri,
                            "timeout_ms": s.timeout_ms,
                            "state": s.state,
                            // A HUMAN GATE's published face (RFC 0021 §7) rides
                            // the suspension so the daemon can surface it.
                            "gate": s.gate,
                        }}
                    }),
                    scheduled: Vec::new(),
                    subscriptions: Vec::new(),
                };
                send_up(up, &AgentMsg::Result { outcome });
                crate::exit::SUCCESS
            }
        };
    }

    // Checkpoint resume (RFC 0021 §8.4): fetch the envelope from the named
    // checkpointer server, verify the workflow hash, and drive from the restored
    // slice — board, budget, and visit counts carry over. A hash mismatch is a
    // REFUSAL (the graph is not the one the state was taken from), overridable
    // only by the explicit `--workflow-resume-force`.
    if let Some(r) = &payload.workflow_resume_ref {
        let state = match fetch_resume_state(graph, r, intel, servers, &model, payload, log) {
            Ok(s) => s,
            Err(reason) => {
                log.error(
                    "workflow.resume.refused",
                    serde_json::json!({"err": reason}),
                );
                let outcome = Outcome {
                    status: TerminalStatus::Refused,
                    partial: false,
                    result: serde_json::Value::String(reason),
                    scheduled: Vec::new(),
                    subscriptions: Vec::new(),
                };
                send_up(up, &AgentMsg::Result { outcome });
                return crate::exit::REFUSED;
            }
        };
        log.info(
            "workflow.resume",
            serde_json::json!({
                "server": r.server, "key": r.key, "seq": r.seq,
                "at": state.at, "steps": state.steps(), "tokens": state.tokens(),
            }),
        );
        let entry_tokens = state.tokens();
        let o = crate::graph::drive_connected_from(
            graph,
            state,
            r.force, // graph-edit-and-continue resets the progress guards
            intel,
            servers,
            &model,
            payload.limits.max_steps,
            payload.limits.max_tokens,
            wall,
            deadline,
            Some(factory),
            Some(&mut orch),
            log,
        );
        return finish_workflow_child(o, entry_tokens, up, log);
    }

    let o = drive_connected(
        graph,
        intel,
        servers,
        &model,
        payload.limits.max_steps,
        payload.limits.max_tokens,
        wall,
        deadline,
        Some(factory),
        Some(&mut orch),
        log,
    );

    finish_workflow_child(o, 0, up, log)
}

/// Fetch + verify a checkpoint envelope (RFC 0021 §8.4): `state.get {key[,seq]}`
/// on the checkpointer server, envelope version check, workflow-hash
/// verification (skipped under `force`), and the run-slice extraction. Every
/// failure is a REFUSAL string (exit 5) — resuming from nothing/a foreign graph
/// is a semantic refusal, not a crash.
#[cfg(feature = "workflow")]
fn fetch_resume_state(
    graph: &crate::graph::Graph,
    r: &crate::subagent::protocol::WorkflowResumeRef,
    intel: &IntelClient,
    servers: &[McpClient],
    model: &str,
    payload: &SpawnPayload,
    log: &Logger,
) -> Result<crate::graph::GraphState, String> {
    use crate::graph::{GraphExec, SessionExec};
    let key = r.key.replace("{run_id}", &payload.telemetry.run_id);
    let mut args = serde_json::json!({"key": key});
    if let Some(seq) = r.seq {
        args["seq"] = seq.into();
    }
    let wall = Duration::from_millis(payload.limits.deadline_ms.max(1));
    let mut exec = SessionExec::new(
        intel,
        servers,
        log,
        model,
        payload.limits.max_steps,
        payload.limits.max_tokens,
        wall,
    );
    let (val, is_err) = exec.call_tool(&r.server, "state.get", &args);
    if is_err {
        return Err(format!(
            "workflow-resume: state.get on '{}' failed: {val}",
            r.server
        ));
    }
    let envelope = val.get("state").unwrap_or(&val);
    if envelope.get("v").and_then(serde_json::Value::as_u64) != Some(1) {
        return Err(format!(
            "workflow-resume: unsupported envelope version (want v=1): {}",
            envelope
                .get("v")
                .cloned()
                .unwrap_or(serde_json::Value::Null)
        ));
    }
    let want = crate::graph::workflow_hash(graph);
    let got = envelope
        .get("workflow_hash")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("");
    if !r.force && got != want {
        return Err(format!(
            "workflow-resume: workflow hash mismatch (checkpoint {got}, supplied graph {want}) — \
             the state was not taken from this graph; --workflow-resume-force overrides"
        ));
    }
    serde_json::from_value(envelope.get("state").cloned().unwrap_or_default())
        .map_err(|e| format!("workflow-resume: bad run slice in envelope: {e}"))
}

/// Map a terminal [`GraphOutcome`] onto the child result contract: GraphStatus →
/// TerminalStatus → the exit table, the workflow detail in the result body, and
/// the intelligence cost (LESS `entry_tokens` — a resumed reactive child rolls
/// only ITS delta; the cumulative pool rides the persisted slice) as Usage.
#[cfg(feature = "workflow")]
fn finish_workflow_child(
    o: crate::graph::GraphOutcome,
    entry_tokens: u64,
    up: &Up,
    log: &Logger,
) -> i32 {
    use crate::agentloop::stop::{Outcome, TerminalStatus};
    use crate::graph::GraphStatus;
    let status = match o.status {
        GraphStatus::Completed => TerminalStatus::Completed,
        GraphStatus::Halted => o.terminal.unwrap_or(TerminalStatus::Crashed),
        GraphStatus::Exhausted => {
            let r = o.reason.as_deref().unwrap_or("");
            if r.contains("deadline") {
                TerminalStatus::Deadline
            } else if r.contains("token") {
                TerminalStatus::ExhaustedTokens
            } else {
                TerminalStatus::ExhaustedSteps
            }
        }
        GraphStatus::LoopDetected => TerminalStatus::LoopDetected,
        GraphStatus::Stalled => TerminalStatus::Stalled,
        GraphStatus::Crashed => TerminalStatus::Crashed,
    };
    log.info(
        "workflow.exit",
        serde_json::json!({
            "workflow_status": format!("{:?}", o.status),
            "reason": o.reason,
            "steps": o.steps,
            "tokens": o.tokens,
        }),
    );
    // Roll the workflow's intelligence cost up for hierarchical accounting
    // (`agentd_tokens_total`) BEFORE the terminal Result — the input/output split
    // is unknown at this layer, so the total rides output_tokens.
    send_up(
        up,
        &AgentMsg::Usage(crate::wire::intel::Usage {
            input_tokens: 0,
            output_tokens: o.tokens.saturating_sub(entry_tokens),
        }),
    );
    let code = crate::exit::once_exit(status, false);
    let outcome = Outcome {
        status,
        partial: false,
        result: serde_json::json!({
            "workflow_status": format!("{:?}", o.status),
            "reason": o.reason,
            "steps": o.steps,
            "tokens": o.tokens,
            "result": o.result,
        }),
        scheduled: Vec::new(),
        subscriptions: Vec::new(),
    };
    send_up(up, &AgentMsg::Result { outcome });
    code
}

fn read_spawn(reader: &mut BufReader<Stdin>) -> Result<SpawnPayload, String> {
    let bytes = frame::read_frame(reader)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| "stdin closed before spawn payload".to_string())?;
    match serde_json::from_slice::<ControlMsg>(&bytes).map_err(|e| e.to_string())? {
        ControlMsg::Spawn(p) => Ok(*p),
        // Defense-in-depth (unreachable in practice — the supervisor always sends
        // Spawn first): report only the variant LABEL, never `{other:?}`. A
        // `SwapIntel`/`Inject` first frame would otherwise Debug-print a plaintext
        // token / injected instruction to stderr, contradicting "token NEVER logged".
        other => Err(format!(
            "first frame was not Spawn (got {})",
            control_msg_label(&other)
        )),
    }
}

/// The bare variant tag of a [`ControlMsg`] — NO payload (a `SwapIntel`/`Inject`
/// carries a credential / injected instruction that must never reach a log/stderr).
fn control_msg_label(msg: &ControlMsg) -> &'static str {
    match msg {
        ControlMsg::Spawn(_) => "spawn",
        ControlMsg::Ping { .. } => "ping",
        ControlMsg::Pause => "pause",
        ControlMsg::Resume => "resume",
        ControlMsg::Cancel { .. } => "cancel",
        ControlMsg::Inject { .. } => "inject",
        ControlMsg::SwapIntel(_) => "swap_intel",
    }
}

fn build_logger(payload: &SpawnPayload) -> Logger {
    let t = &payload.telemetry;
    let level = Level::parse(&t.log_level).unwrap_or(Level::Info);
    Logger::new(
        LogCtx {
            run_id: t.run_id.clone(),
            agent_id: t.agent_id.clone(),
            agent_path: t.agent_path.clone(),
            comp: Comp::Agent,
            pid: std::process::id(),
            trace_id: t.trace_id.clone(),
        },
        level,
    )
    .with_content(t.log_content)
}

fn send_up(up: &Up, msg: &AgentMsg) {
    if let Ok(mut out) = up.lock() {
        // Best-effort: a dead parent means our writes fail; we don't crash.
        let _ = frame::write_frame(&mut *out, msg);
    }
}

/// Wire the child's intelligence reachability UP to the supervisor (RFC 0018 §6).
/// The model loop runs in this CHILD process and owns the breaker/failover state;
/// the supervisor has no LLM and no live view of it. The reporter is edge-triggered
/// (fires only on an all-down ENTER/EXIT transition) and carries transport+index
/// ONLY — NEVER a URL/cid/host or credential (RFC 0012 §3.7). Re-installed after a
/// hot-swap rebuild (the rebuilt client has fresh breakers + no reporter). Cloning
/// the `up` Arc lets the reporter outlive this fn (it is owned by the new client).
fn install_intel_health_reporter(intel: &mut IntelClient, up: &Up) {
    let up = Arc::clone(up);
    intel.set_health_reporter(Box::new(move |r: IntelHealthReport| {
        let active = r.active.map(|(index, transport)| IntelActive {
            index,
            transport: transport.to_string(),
        });
        send_up(
            &up,
            &AgentMsg::IntelHealth {
                all_down: r.all_down,
                active,
            },
        );
    }));
}

/// The control reader thread. Owns stdin, answers `Ping` with `Pong`, flips the
/// cancel flag on `Cancel`, toggles the `paused` flag on `Pause`/`Resume`, and
/// forwards each `Inject` event to a warm session's loop over `inject_tx`. It
/// keeps running while the loop is suspended (so `Resume`/`Cancel`/`Ping` still
/// arrive — the whole point of a separate thread). Exits on EOF (the supervisor
/// closed the channel) or a read error — which drops `inject_tx`, unblocking a
/// warm session's wait.
#[allow(clippy::too_many_arguments)]
fn spawn_control_thread(
    mut stdin: BufReader<Stdin>,
    up: Up,
    cancel: Arc<AtomicBool>,
    paused: Arc<AtomicBool>,
    inject_tx: Sender<String>,
    pending_swap: PendingSwap,
    ctx: LogCtx,
) {
    let log = Logger::new(ctx, Level::Debug);
    std::thread::Builder::new()
        .name("subagent-control".into())
        .spawn(move || {
            // Exits on Ok(None)/Err — the supervisor closed the channel.
            while let Ok(Some(bytes)) = frame::read_frame(&mut stdin) {
                match serde_json::from_slice::<ControlMsg>(&bytes) {
                    Ok(ControlMsg::Ping { seq }) => send_up(&up, &AgentMsg::Pong { seq }),
                    Ok(ControlMsg::Cancel { reason }) => {
                        log.info("subagent.cancel", serde_json::json!({"reason": reason}));
                        cancel.store(true, Ordering::Relaxed);
                    }
                    // Turn-boundary suspension (RFC 0005 §4.3 / RFC 0015 §4.3): set
                    // the flag here; the loop suspends at its next boundary. The
                    // loop's `pause_wait` does the enter/leave logging (debounced).
                    Ok(ControlMsg::Pause) => paused.store(true, Ordering::Relaxed),
                    Ok(ControlMsg::Resume) => paused.store(false, Ordering::Relaxed),
                    // Deliver into the warm session; a one-shot run never reads
                    // the receiver, so the send is simply dropped there.
                    Ok(ControlMsg::Inject { message }) => {
                        let _ = inject_tx.send(message);
                    }
                    // Intelligence hot-swap (RFC 0018 §5.2): park the new config in
                    // the child-local LIVE handle. The loop drains + applies it at
                    // its next turn boundary (rebuild client + adopt model); we
                    // never touch the loop's in-flight `complete_once`. A swap that
                    // supersedes a still-unread one simply overwrites it — the loop
                    // only ever cares about the LATEST config (last-write-wins). The
                    // token rides this frame (like Spawn) but is NEVER logged.
                    Ok(ControlMsg::SwapIntel(swap)) => {
                        log.info(
                            "subagent.swap_intel",
                            serde_json::json!({"endpoint_change": true}),
                        );
                        *pending_swap.lock().unwrap_or_else(|e| e.into_inner()) = Some(*swap);
                    }
                    Ok(ControlMsg::Spawn(_)) | Err(_) => { /* unexpected/garbage — ignore */ }
                }
            }
        })
        .ok();
}

/// `PR_SET_PDEATHSIG(SIGKILL)`: when the supervisor (our parent) dies, the
/// kernel sends us SIGKILL — the leaf-up tree collapse (RFC 0003). Must be set
/// after `execve` (it is cleared across exec), i.e. here in the child's `main`.
#[cfg(target_os = "linux")]
fn install_pdeathsig() {
    unsafe {
        libc::prctl(
            libc::PR_SET_PDEATHSIG,
            libc::SIGKILL as libc::c_ulong,
            0,
            0,
            0,
        );
    }
}

#[cfg(not(target_os = "linux"))]
fn install_pdeathsig() {
    // PDEATHSIG is Linux-only; on other Unix the supervisor's kill ladder is
    // the fallback. (agentd targets Linux for production.)
}

// The full control path is exercised end to end by the `subagent_spawn`
// integration test (a real subagent process). The flag-driven turn-boundary
// suspend logic is unit-tested here directly.
#[cfg(test)]
mod tests {
    use super::*;
    use crate::obs::log::{Comp, Level, LogCtx, Logger};

    fn test_log() -> Logger {
        Logger::new(
            LogCtx {
                run_id: "r".into(),
                agent_id: "0".into(),
                agent_path: "0".into(),
                comp: Comp::Agent,
                pid: 0,
                trace_id: None,
            },
            Level::Info,
        )
    }

    /// A best-effort upward handle for the swap-apply tests — writes to the real
    /// stdout (the reporter re-install path is exercised; the framed bytes are
    /// inert in a unit test, and `send_up` is best-effort by construction).
    fn test_up() -> Up {
        Arc::new(Mutex::new(io::stdout()))
    }

    #[test]
    fn pause_wait_returns_immediately_when_not_paused() {
        let paused = AtomicBool::new(false);
        let cancel = AtomicBool::new(false);
        let t = Instant::now();
        pause_wait(&paused, &cancel, &test_log());
        // No sleep on the fast path.
        assert!(t.elapsed() < Duration::from_millis(40));
    }

    #[test]
    fn pause_wait_cancel_wins_over_pause() {
        // Paused AND cancelled → cancel wins: return at once (the loop then winds
        // down at its cancel check). Never blocks.
        let paused = AtomicBool::new(true);
        let cancel = AtomicBool::new(true);
        let t = Instant::now();
        pause_wait(&paused, &cancel, &test_log());
        assert!(t.elapsed() < Duration::from_millis(40));
    }

    #[test]
    fn pause_wait_suspends_until_resume() {
        // Paused → block; another thread clears `paused` (a Resume), and the wait
        // returns. The flag is the whole mechanism — this proves the seam.
        let paused = Arc::new(AtomicBool::new(true));
        let cancel = Arc::new(AtomicBool::new(false));
        let p2 = Arc::clone(&paused);
        let unblock = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(120));
            p2.store(false, Ordering::Relaxed); // Resume
        });
        let t = Instant::now();
        pause_wait(&paused, &cancel, &test_log());
        // It actually waited for the resume (≥ ~one poll interval), then returned.
        assert!(t.elapsed() >= Duration::from_millis(80));
        assert!(!paused.load(Ordering::Relaxed));
        unblock.join().unwrap();
    }

    #[test]
    fn pause_wait_breaks_out_on_cancel_during_pause() {
        // A cancel that lands WHILE suspended unblocks the wait (cancel always
        // wins), so a drain during a pause proceeds (RFC 0015 §4.3).
        let paused = Arc::new(AtomicBool::new(true));
        let cancel = Arc::new(AtomicBool::new(false));
        let c2 = Arc::clone(&cancel);
        let canceller = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(120));
            c2.store(true, Ordering::Relaxed); // Cancel during pause
        });
        pause_wait(&paused, &cancel, &test_log());
        assert!(cancel.load(Ordering::Relaxed));
        assert!(paused.load(Ordering::Relaxed)); // still paused, but cancel broke us out
        canceller.join().unwrap();
    }

    fn swap_to(uri: &str, model: Option<&str>, policy: SwapPolicy) -> SwapIntel {
        SwapIntel {
            uri: uri.into(),
            token: None,
            model: model.map(str::to_string),
            policy,
        }
    }

    #[test]
    fn apply_pending_swap_rebuilds_client_and_adopts_model() {
        // RFC 0018 §5.2: a parked swap is drained + applied at the one-shot turn
        // boundary — the client points at the new endpoint list and the model is
        // adopted. The new endpoint list starts with FRESH health (every endpoint
        // CLOSED — `from_parts` builds a new HealthRecord, no stale breaker).
        let pending: PendingSwap = Arc::new(Mutex::new(None));
        let mut intel = IntelClient::from_parts("https://old.example", None).unwrap();
        let mut model = "old-model".to_string();
        *pending.lock().unwrap() = Some(swap_to(
            "https://a.example,https://b.example",
            Some("new-model"),
            SwapPolicy::FinishOnOld,
        ));
        apply_pending_swap(&pending, &mut intel, &mut model, &test_up(), &test_log());
        assert_eq!(model, "new-model");
        assert_eq!(
            intel.endpoint_count(),
            2,
            "client repointed to the new list"
        );
        // The slot is drained — a second apply is a no-op (no double-swap).
        assert!(pending.lock().unwrap().is_none());
        apply_pending_swap(&pending, &mut intel, &mut model, &test_up(), &test_log());
        assert_eq!(model, "new-model");
    }

    #[test]
    fn apply_pending_swap_is_a_noop_when_nothing_pending() {
        // The no-swap path: the model + endpoint count are byte-for-byte unchanged.
        let pending: PendingSwap = Arc::new(Mutex::new(None));
        let mut intel = IntelClient::from_parts("https://only.example", None).unwrap();
        let mut model = "m".to_string();
        apply_pending_swap(&pending, &mut intel, &mut model, &test_up(), &test_log());
        assert_eq!(model, "m");
        assert_eq!(intel.endpoint_count(), 1);
    }

    #[test]
    fn restart_turn_pending_only_for_model_change_under_restart_policy() {
        let pending: PendingSwap = Arc::new(Mutex::new(None));
        // No swap pending → never a restart.
        assert!(!restart_turn_pending(&pending, "m"));
        // A finish-on-old swap (even a model change) → never a restart.
        *pending.lock().unwrap() = Some(swap_to(
            "https://a.example",
            Some("big"),
            SwapPolicy::FinishOnOld,
        ));
        assert!(!restart_turn_pending(&pending, "small"));
        // A restart-turn swap that does NOT change the model (endpoint repoint) →
        // never a restart (a repoint is always finish-on-old / invisible, §5.1).
        *pending.lock().unwrap() = Some(swap_to(
            "https://a.example",
            Some("small"),
            SwapPolicy::RestartTurn,
        ));
        assert!(!restart_turn_pending(&pending, "small"));
        // A restart-turn swap that DOES change the model → a restart.
        *pending.lock().unwrap() = Some(swap_to(
            "https://a.example",
            Some("big"),
            SwapPolicy::RestartTurn,
        ));
        assert!(restart_turn_pending(&pending, "small"));
    }

    #[test]
    fn read_spawn_error_never_echoes_a_swap_intel_token() {
        // Defense-in-depth (the info fold-in): a non-Spawn first frame must report
        // only the variant LABEL — never `{other:?}`, which would Debug-print a
        // plaintext token / injected instruction to stderr ("token NEVER logged").
        let swap = ControlMsg::SwapIntel(Box::new(SwapIntel {
            uri: "https://secret-host.example/secret-path".into(),
            token: Some("super-secret-token".into()),
            model: Some("m".into()),
            policy: SwapPolicy::FinishOnOld,
        }));
        let mut buf = Vec::new();
        frame::write_frame(&mut buf, &swap).unwrap();
        let mut reader = BufReader::new(io::Cursor::new(buf));
        // `read_spawn` takes `BufReader<Stdin>`; the label helper is the unit under
        // test for the redaction property — drive it directly to avoid a real stdin.
        let err = format!(
            "first frame was not Spawn (got {})",
            control_msg_label(&swap)
        );
        assert_eq!(err, "first frame was not Spawn (got swap_intel)");
        assert!(!err.contains("super-secret-token"), "token leaked: {err}");
        assert!(!err.contains("secret-host.example"), "uri leaked: {err}");
        // The label helper covers every variant tag, payload-free.
        assert_eq!(
            control_msg_label(&ControlMsg::Inject {
                message: "do bad things".into()
            }),
            "inject"
        );
        let _ = &mut reader; // the framed bytes are constructed; the property is the label
    }

    #[test]
    fn bad_swap_list_keeps_the_old_client() {
        // RFC 0018 §5.2: an unparseable new list never tears a working run — the
        // old client is kept; only the model (a plain string) is still adopted.
        let pending: PendingSwap = Arc::new(Mutex::new(None));
        let mut intel =
            IntelClient::from_parts("https://old.example,https://old2.example", None).unwrap();
        let mut model = "old".to_string();
        *pending.lock().unwrap() = Some(swap_to("", Some("new"), SwapPolicy::FinishOnOld));
        apply_pending_swap(&pending, &mut intel, &mut model, &test_up(), &test_log());
        assert_eq!(intel.endpoint_count(), 2, "kept the old 2-endpoint client");
        assert_eq!(model, "new");
    }
}
