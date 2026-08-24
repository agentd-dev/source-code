// SPDX-License-Identifier: AGPL-3.0-only
//! The subagent side of the control channel.
//!
//! Entered when `main` sees `AGENT_SUBAGENT` set. The child, in order:
//! 1. installs `PR_SET_PDEATHSIG` so a supervisor death collapses it. This must
//!    happen here rather than in `pre_exec`, because `execve` clears the setting;
//! 2. reads its [`SpawnPayload`] (first control frame) from stdin;
//! 3. starts a **control reader thread**, separate from the agentic loop, that
//!    answers `Ping` with `Pong` and flips a cancel flag on `Cancel`, so
//!    liveness survives a long in-flight tool or model call;
//! 4. emits `Ready`, connects intelligence + its scoped MCP servers, runs
//!    `agentloop::run_loop`, and sends `Result`/`Failed` back up.
//!
//! Wire: stdout carries length-framed [`AgentMsg`] up; stderr carries the
//! child's JSON telemetry (inherited to the parent). stdin carries
//! [`ControlMsg`] down.

use crate::agentloop::action::SelfHandler;
use crate::agentloop::runner::{LoopAbort, LoopInput, Session, run_loop};
use crate::agentloop::stop::{Outcome, TerminalStatus};
use crate::config::SwapPolicy;
use crate::intel::client::{IntelClient, IntelHealthReport};
use crate::json::frame;
use crate::mcp::client::McpClient;
use crate::obs::log::{Comp, Level, LogCtx, Logger};
use crate::subagent::protocol::{AgentMsg, ControlMsg, IntelActive, SpawnPayload, SwapIntel};
use crate::supervisor::budget::Budget;
use std::io::{self, BufReader, Stdin, Stdout};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// The child-local LIVE intelligence handle. A supervisor-side shared config
/// cannot reach a child that was re-exec'd as its own PROCESS, so each child
/// holds its own live copy: the control-reader thread parks a [`SwapIntel`] here
/// on `ControlMsg::SwapIntel`, and the agentic loop drains it ONCE per turn at
/// the turn boundary — the same boundary `pause_wait` sits at — rebuilds its
/// [`IntelClient`] from the new endpoints with fresh health and a closed
/// breaker, and adopts the new model. The `Mutex<Option<…>>` is the whole seam;
/// the loop never holds the lock across a turn, so a swap arriving mid-turn
/// parks without blocking the control thread.
type PendingSwap = Arc<Mutex<Option<SwapIntel>>>;

pub(crate) type Up = Arc<Mutex<Stdout>>;

/// How long an MCP server's `elicitation/create` waits for a human before the
/// server is told `cancel`. The gate itself may outlive this — the operator can
/// still answer it in the TUI — but a server should not hold a request open
/// indefinitely waiting for someone to walk back to their desk.
const ELICITATION_TIMEOUT: Duration = Duration::from_secs(300);

/// The in-child self-handler for a subagent. A subagent is a **flat child of the
/// reactor**: it runs a ReAct loop over its granted MCP + code tools and reports
/// its result, and it has **no** in-child orchestration self-tools — no nested
/// `subagent.spawn`, `schedule`, `subscribe`, `workflow.*` or `a2a.delegate`.
/// Keeping delegation with the reactor is what makes the tree flat and its depth
/// and concurrency caps enforceable from one place. `finish` is handled by the
/// loop itself rather than the self-handler, so completion still works here.
struct NoSelfTools;
impl SelfHandler for NoSelfTools {
    fn tools(&self) -> Vec<crate::wire::intel::ToolDef> {
        Vec::new()
    }
    fn handle(&mut self, _name: &str, _args: &serde_json::Value) -> Option<(String, bool)> {
        None
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

    // Install the SAME agent identity the root has, so this child signs its MCP
    // requests under one tree-wide identity. The key file is a shared-fs path
    // resolved here, because the re-exec crossed the process boundary; a bad
    // key or secret is a startup failure (exit 2) rather than an unsigned run.
    #[cfg(feature = "aauth")]
    if let Some(settings) = &payload.aauth
        && let Err(e) = crate::aauth::setup(settings, std::time::Duration::from_secs(30))
    {
        eprintln!("agentd subagent: aauth: {e}");
        return crate::exit::USAGE;
    }

    let up: Up = Arc::new(Mutex::new(io::stdout()));
    let log = build_logger(&payload);
    let cancel = Arc::new(AtomicBool::new(false));
    // Turn-boundary suspension: the control thread sets this on `Pause` and
    // clears it on `Resume`; the loop waits between turns while it is set.
    // `cancel` always wins over it, so a paused child can still be drained
    // (see `pause_wait`).
    let paused = Arc::new(AtomicBool::new(false));

    // For a warm continue-session, the control thread forwards each `Inject`
    // event to the loop over this channel; a one-shot run never reads it.
    let (inject_tx, inject_rx) = std::sync::mpsc::channel::<String>();

    // The child-local LIVE intel handle: the control thread parks a hot-swap
    // here; the loop drains it at the turn boundary. `None` until the first swap
    // arrives, so the common no-swap path costs one cheap empty check per turn
    // and never contends on the lock.
    let pending_swap: PendingSwap = Arc::new(Mutex::new(None));

    // The reply slots for ToolRequest/BudgetRequest round-trips.
    let replies = Arc::new(crate::subagent::replies::Replies::new());

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
        Arc::clone(&replies),
        log.ctx().clone(),
    );

    // A warm session keeps its `Inject` stream as its turn-input channel; a
    // one-shot subagent never reads it.
    let inject_rx = Some(inject_rx);

    send_up(&up, &AgentMsg::Ready);
    log.info(
        "loop.start",
        serde_json::json!({"depth": payload.depth, "warm": payload.warm}),
    );

    let mut intel = match IntelClient::from_parts(
        &payload.intelligence.uri,
        payload.intelligence.token.clone(),
    ) {
        Ok(c) => {
            // The resolved `intelligence.headers` ride every dial.
            #[allow(unused_mut)]
            let mut c = c
                .with_headers(payload.intelligence.headers.clone())
                // Select the wire dialect: `bedrock` speaks Converse.
                .with_dialect(payload.intelligence.dialect.as_deref());
            // An `intelligence.auth: {kind: aws}` SigV4-signs the LLM dial.
            #[cfg(feature = "oauth")]
            if let Some(aws) = &payload.intelligence.aws_auth
                && let Ok(s) = crate::auth::aws::SigV4Signer::from_spec(aws, "intelligence")
            {
                c = c.with_signer(Some(s as std::sync::Arc<dyn ::mcp::http::RequestSigner>));
            }
            // Outbound LLM calls join the run's distributed trace.
            c.set_trace_id(payload.telemetry.trace_id.clone());
            // Bridge this child's intel reachability UP to the supervisor, which
            // has no LLM of its own, on each all-down transition.
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
        // Let this server ask the operator questions: `elicitation/create`
        // round-trips to the supervisor's `ask_human`, which renders a gate in
        // every attached client. Declared per-connection so a server only asks
        // when we can actually deliver the question to a human.
        let elicit: Arc<dyn ::mcp::inbound::Handler> =
            Arc::new(crate::mcp::elicit::ElicitationBridge::new(
                Arc::clone(&up),
                Arc::clone(&replies),
                Arc::clone(&cancel),
                ELICITATION_TIMEOUT,
            ));
        let connected = crate::mcp::from_spec(spec, Duration::from_secs(60))
            .map(|c| c.with_elicitation(elicit))
            .and_then(|mut c| c.initialize().map(|()| c));
        match connected {
            Ok(mut c) => {
                log.info("mcp.connect", serde_json::json!({"server": spec.name}));
                // Stamp the run id, so a server can deduplicate a retried call,
                // and a W3C traceparent, so the call joins the run's trace, on
                // every tool call.
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

    // A TURN WORKER runs one turn over the supplied context slice, with internal
    // tools round-tripping to the supervisor. It reuses the connections and
    // supervision set up above; only the loop it runs differs.
    if payload.role == crate::subagent::protocol::Role::Turn {
        return crate::runtime::worker::run_turn_child(
            &payload, &intel, &servers, &up, &cancel, &replies, &log,
        );
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

    // A subagent has no in-child orchestration self-tools: the tree is flat.
    let mut orch = NoSelfTools;

    // A warm continue-session lives across many events; a one-shot runs once.
    // Only the warm shape gets the all-down backoff, because it is expected to
    // outlive a transient outage: a host-model roll should let it wait and
    // recover, where a one-shot job has nothing to wait for and exits 4.
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
    // Turn-boundary read: a swap that landed before this single turn started is
    // applied here, rebuilding the client and adopting the model. A swap that
    // lands DURING the turn finishes on the old model and is invisible — a
    // one-shot has no next turn, so `restart-turn` cannot apply to it.
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

/// Drive a **warm continue-session**: prepare the session once, then run one
/// turn per delivered event over the *same*
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
    orch: &mut NoSelfTools,
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
        // Turn boundary: if paused, suspend HERE, before starting the next turn
        // — never mid-turn, so no turn is ever left half-finished. A `Cancel`
        // during a pause wins and proceeds to wind-down, falling through to the
        // cancel check below. The control thread keeps running, so Resume and
        // Cancel still arrive while we wait. Only this child loop suspends: the
        // supervisor reactor and its liveness heartbeat keep ticking.
        pause_wait(paused, cancel, log);
        // Live tools refresh: an inbound `notifications/tools/list_changed` on
        // any of THIS child's own MCP connections re-enumerates the catalogue at
        // this turn boundary, so a warm session tracks a changing server instead
        // of holding a stale tool set for its whole life. A spawned one-shot
        // needs no equivalent — it re-lists once per run anyway.
        refresh_tools_if_changed(&mut session, orch, servers, log);
        // Turn-boundary read: a hot-swap parked by the control thread is drained
        // and applied HERE, before the turn — the loop rebuilds its client with
        // fresh health and breaker state and adopts the new model. The
        // transcript is UNTOUCHED, so a swap costs no context. A turn already
        // running is never torn, because the swap can only land at this seam.
        apply_pending_swap_warm(pending_swap, &mut intel, &mut session, up, log);
        // Snapshot the pre-turn transcript so `restart-turn` can discard a turn
        // that completed under a model swap and re-run it on the new model from
        // this exact state. Cheap (a `usize`); unused under finish-on-old.
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
        // `restart-turn`: a model-changing swap that LANDED while this turn was
        // in flight discards the turn's result and re-runs it on the new model
        // from the pre-turn transcript. The `complete_once` is never torn — the
        // turn ran to completion, and only its appended messages are dropped
        // before we loop WITHOUT consuming a new event, so no event is lost. The
        // re-run is bounded by the step budget like any turn. The swap itself is
        // applied at the top of the loop, so all we decide here is whether to
        // re-run. An endpoint repoint that leaves the model unchanged is never a
        // restart: it is invisible to the transcript.
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

/// Suspend the loop at a turn boundary while `paused` is set. Polls at the same
/// cadence as `wait_for_inject` so a `Resume` or `Cancel` lands promptly.
/// `cancel` always wins: a cancel during a pause returns immediately so the loop
/// proceeds to wind-down, which is what keeps a paused child drainable. Logs
/// once on enter and once on leave, never per poll, so a long pause does not
/// flood the log. The supervisor reactor is NOT gated by this — only the child's
/// agentic loop suspends, so the liveness heartbeat keeps ticking.
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

/// Rebuild an [`IntelClient`] from a hot-swap's endpoint list. A repointed
/// endpoint starts with its breaker CLOSED: a fresh
/// [`crate::intel::endpoints::EndpointList`] gives every endpoint a brand-new
/// `HealthRecord`, so failures recorded against the endpoint that was just
/// replaced cannot condemn the one that replaced it. The run's trace id is
/// re-stamped onto the new client so outbound calls keep joining the run's
/// distributed trace. Returns `None`, after logging, when the new list is
/// unparseable; the caller then keeps the existing client, so a malformed swap
/// never tears down a working run.
fn rebuild_intel(swap: &SwapIntel, old: &IntelClient, log: &Logger) -> Option<IntelClient> {
    match IntelClient::from_parts(&swap.uri, swap.token.clone()) {
        Ok(mut c) => {
            c.set_trace_id(old.trace_id().map(str::to_string));
            // A warm, long-lived loop keeps its all-down backoff across a swap:
            // a daemon must not start crashing on a transient outage merely
            // because it was repointed. The one-shot path never enables the
            // backoff, so this carries nothing over there.
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

/// Emit the `intel.swap` event for an applied swap. NO secret and NO URL ever
/// appear — only the swap KIND (`endpoint` or `model`), the model names, which
/// are non-secret identifiers, the policy, and whether the endpoint list
/// changed. Endpoint identity stays transport-and-index only and is surfaced by
/// the `agentd://intelligence` resource, never by this event, so an operator
/// reading logs can see that a swap happened without learning where it points.
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

/// Apply a parked hot-swap at the ONE-SHOT turn boundary: drain the pending
/// slot, rebuild the client with fresh health, and adopt the new model into
/// `model`. Costs one cheap empty-lock check when nothing is pending, so the
/// common path is not penalised. `restart-turn` cannot apply to a one-shot,
/// which has a single turn, so the policy only labels the event here.
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
            // The rebuilt client has fresh breakers and no reporter, so
            // re-install one or the child stops bridging reachability up to the
            // supervisor after a repoint.
            install_intel_health_reporter(&mut c, up);
            *intel = c;
            true
        }
        None => false,
    };
    *model = to_model.clone();
    log_swap(log, &from_model, &to_model, endpoint_change, swap.policy);
}

/// Apply a parked hot-swap at a WARM-session turn boundary: drain the pending
/// slot, rebuild the client with fresh health, and adopt the new model onto the
/// live [`Session`]. The transcript is UNTOUCHED, so the session keeps its whole
/// conversation across the swap. A no-op when nothing is pending.
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
            // The rebuilt client has fresh breakers and no reporter, so
            // re-install one or the warm session stops bridging reachability up
            // to the supervisor after a repoint.
            install_intel_health_reporter(&mut c, up);
            *intel = c;
            true
        }
        None => false,
    };
    session.set_model(&to_model);
    log_swap(log, &from_model, &to_model, endpoint_change, swap.policy);
}

/// Peek, without draining, whether a `restart-turn` swap is parked that would
/// CHANGE the model from the session's current one. Only a model-changing
/// `restart-turn` swap that landed WHILE the turn was in flight justifies
/// discarding and re-running the just-completed turn. An endpoint repoint that
/// leaves the model unchanged produces identical output and so never warrants a
/// re-run, and a finish-on-old swap is applied at the next boundary without one.
/// Peeking rather than draining leaves the swap for the boundary code to apply,
/// so the two paths cannot both consume it.
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
/// failed re-list is only a warning: the existing catalogue stays in place and
/// the next boundary retries, so a momentarily unreachable server cannot strip a
/// running session of its tools. Draining here is safe because a warm child
/// holds no subscriptions of its own — those live on the DAEMON's connections —
/// so nothing the daemon relies on is consumed.
fn refresh_tools_if_changed(
    session: &mut Session,
    orch: &mut NoSelfTools,
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
        ControlMsg::ToolResult { .. } => "tool_result",
        ControlMsg::BudgetGrant { .. } => "budget_grant",
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

pub(crate) fn send_up(up: &Up, msg: &AgentMsg) {
    if let Ok(mut out) = up.lock() {
        // Best-effort: a dead parent means our writes fail; we don't crash.
        let _ = frame::write_frame(&mut *out, msg);
    }
}

/// Wire the child's intelligence reachability UP to the supervisor. The model
/// loop runs in this CHILD process and owns the breaker and failover state,
/// while the supervisor has no LLM and no live view of it, so this bridge is the
/// only way that state reaches the readiness probe and metrics. The reporter is
/// edge-triggered — it fires only on an all-down ENTER or EXIT transition, which
/// keeps a wedged endpoint from flooding the control channel — and carries
/// transport and index ONLY, never a URL, cid, host or credential. Must be
/// re-installed after every hot-swap rebuild, since the rebuilt client has fresh
/// breakers and no reporter. Cloning the `up` Arc lets the reporter outlive this
/// call; it is owned by the new client.
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
/// keeps running while the loop is suspended, which is the whole point of a
/// separate thread: `Resume`, `Cancel` and `Ping` must still arrive at a paused
/// or busy child. Exits on EOF, meaning the supervisor closed the channel, or on
/// a read error. Either exit drops `inject_tx`, which unblocks a warm session's
/// wait so it cannot hang forever on a supervisor that is gone.
#[allow(clippy::too_many_arguments)]
fn spawn_control_thread(
    mut stdin: BufReader<Stdin>,
    up: Up,
    cancel: Arc<AtomicBool>,
    paused: Arc<AtomicBool>,
    inject_tx: Sender<String>,
    pending_swap: PendingSwap,
    replies: Arc<crate::subagent::replies::Replies>,
    ctx: LogCtx,
) {
    let log = Logger::new(ctx, Level::Debug);
    std::thread::Builder::new()
        .name("subagent-control".into())
        .spawn(move || {
            // Exits on Ok(None)/Err — the supervisor closed the channel.
            while let Ok(Some(bytes)) = frame::read_frame(&mut stdin) {
                match serde_json::from_slice::<ControlMsg>(&bytes) {
                    // Round-trip answers: park them in the reply slots the turn
                    // worker blocks on.
                    Ok(ControlMsg::ToolResult {
                        id,
                        result,
                        is_error,
                    }) => {
                        replies.deliver(
                            id,
                            crate::subagent::replies::Reply::Tool { result, is_error },
                        );
                    }
                    Ok(ControlMsg::BudgetGrant {
                        id,
                        ok,
                        wait_ms,
                        model,
                        reason,
                    }) => {
                        replies.deliver(
                            id,
                            crate::subagent::replies::Reply::Budget {
                                ok,
                                wait_ms,
                                model,
                                reason,
                            },
                        );
                    }
                    Ok(ControlMsg::Ping { seq }) => send_up(&up, &AgentMsg::Pong { seq }),
                    Ok(ControlMsg::Cancel { reason }) => {
                        log.info("subagent.cancel", serde_json::json!({"reason": reason}));
                        cancel.store(true, Ordering::Relaxed);
                    }
                    // Turn-boundary suspension: set the flag here and let the
                    // loop suspend at its next boundary. The loop's `pause_wait`
                    // does the enter/leave logging, debounced, so no log line is
                    // written here.
                    Ok(ControlMsg::Pause) => paused.store(true, Ordering::Relaxed),
                    Ok(ControlMsg::Resume) => paused.store(false, Ordering::Relaxed),
                    // Deliver into the warm session; a one-shot run never reads
                    // the receiver, so the send is simply dropped there.
                    Ok(ControlMsg::Inject { message }) => {
                        let _ = inject_tx.send(message);
                    }
                    // Intelligence hot-swap: park the new config in the
                    // child-local LIVE handle. The loop drains and applies it at
                    // its next turn boundary, rebuilding the client and adopting
                    // the model; the loop's in-flight `complete_once` is never
                    // touched from here. A swap that supersedes a still-unread
                    // one simply overwrites it — last write wins, because only
                    // the LATEST config is ever correct to dial. The token rides
                    // this frame, as Spawn's does, and is NEVER logged.
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
            // The channel closed: wake any turn worker blocked on a reply.
            replies.close();
        })
        .ok();
}

/// `PR_SET_PDEATHSIG(SIGKILL)`: when the supervisor (our parent) dies, the
/// kernel sends us SIGKILL, so a tree collapses leaf-up and leaves no orphaned
/// agent processes behind. Must be set AFTER `execve`, which clears it — hence
/// here in the child's `main` rather than in the parent's `pre_exec`.
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
    // PDEATHSIG is Linux-only; on other Unix the supervisor's kill ladder is the
    // fallback that reaps orphans. Production targets Linux.
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
        // A cancel that lands WHILE suspended unblocks the wait — cancel always
        // wins — so a drain during a pause still proceeds.
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
        // A parked swap is drained and applied at the one-shot turn boundary:
        // the client points at the new endpoint list and the model is adopted.
        // The new endpoint list starts with FRESH health — every endpoint
        // CLOSED, because `from_parts` builds a new HealthRecord and carries no
        // breaker state over.
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
        // A restart-turn swap that does NOT change the model (an endpoint
        // repoint) is never a restart: re-running would produce the same output.
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
        // Defence in depth: a non-Spawn first frame must report only the variant
        // LABEL, never `{other:?}`, which would Debug-print a plaintext token or
        // an injected instruction to stderr.
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
        // An unparseable new list never tears down a working run: the existing
        // client stays, and only the model, a plain string, is still adopted.
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
