// SPDX-License-Identifier: AGPL-3.0-only
//! A server dialed on use signs in then (RFC 0055 §4.3).
//!
//! **When.** A `connect: on_use` server is dialed off the loop when
//! something first needs it (`runtime::join`). Before the dial, a server
//! whose `auth` is an interactive OAuth grant — the device grant, or the
//! browser + PKCE loopback flow (`grant: authorization_code`) — and for
//! which the login cache holds nothing usable from its authorization server,
//! nor anything to renew from, needs a person to sign in. A server with no
//! `auth` whose dial fails is asked whether it wants one: an RFC 9728
//! challenge or metadata naming its authorization server means it does, and
//! agentd signs in as the device grant there, with its Client ID Metadata
//! Document (`security.client`) as its client — or says nobody can, with
//! none configured.
//!
//! **How.** The grant is what `agentd --login mcp:<name>` runs
//! ([`crate::auth::login::sign_in`]): discovery, the device code or the
//! authorization URL, the poll, the token kept in the login cache under
//! `mcp:<name>` with the authorization server it came from, and refreshed
//! from there as any login is. It runs on a thread of its own; the server is
//! not dialed while it does, and is dialed at once when it ends well.
//!
//! **The gate.** Where to sign in is said (`mcp.sign_in.required {server,
//! url, method}`, with the code) and shown on the A2A interface as a gate
//! agentd raised itself (`securityGate`), its question carrying the URL and
//! the code and its task the same as `mcpSignIn`: on the task of each run
//! whose step waits for the server — the run's own, or one made for it when
//! that already holds a gate — or, when no step waits, on a task of its own.
//! The person signs in at the authorization server, not at the gate; the
//! one answer it takes is `cancel`. A gate's task outlives no process: a
//! restart closes it, and the step waiting for the server starts a new
//! sign-in when it looks again.
//!
//! **The wait** is the step's connect wait (`join::connect_wait`), durable
//! as that is. It lasts the step's `timeout`, else the server's, else as
//! long as the grant's code: past it the step fails `sign_in_timeout`. A
//! sign-in that fails — denied, expired, cancelled — fails each step that
//! waited with `sign_in_<reason>`, says `mcp.sign_in.failed {server,
//! reason}`, and the server is not dialed again until something needs it
//! anew. A turn is told which servers are being signed in to (the prompt's
//! `signing_in`); their tools reach it once they have joined.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

#[cfg(feature = "a2a")]
use serde_json::Value;
use serde_json::json;

use super::reactor::Runtime;
use crate::config::{AuthSpec, McpServerSpec};
use crate::state::now_ms;

/// How long a sign-in is waited for when neither the step nor the server
/// sets a timeout and the grant names no lifetime for its code.
pub(crate) const SIGN_IN_FOR_MS: u64 = 15 * 60 * 1000;

/// What a sign-in stopped by its gate's `cancel` ends with.
const CANCELLED: &str = "the sign-in was cancelled";

/// A sign-in in progress.
pub(crate) struct McpSignIn {
    /// `device` or `browser`.
    pub method: &'static str,
    /// Where its person signs in, once the grant has said.
    pub prompt: Option<Prompted>,
    /// When its code stops being good (ms).
    pub until: Option<u64>,
    /// Set to stop the poll.
    pub cancel: Arc<AtomicBool>,
    /// The gates it is shown at.
    #[cfg_attr(not(feature = "a2a"), allow(dead_code))]
    pub gates: Vec<Gate>,
}

/// Where a person signs in.
#[derive(Debug, Clone)]
pub(crate) struct Prompted {
    pub url: String,
    /// The URL with the code in it, when the grant gave one.
    pub complete: Option<String>,
    /// The code to enter or check (device grant).
    pub code: Option<String>,
}

/// A sign-in's gate: its task, the run it was opened for, and whether the
/// task exists only for the gate.
#[cfg_attr(not(feature = "a2a"), allow(dead_code))]
pub(crate) struct Gate {
    pub task: String,
    pub run: Option<String>,
    pub standalone: bool,
}

/// How a sign-in moved on.
#[derive(Debug)]
pub enum Step {
    /// Where its person signs in, and for how long (ms).
    Prompt {
        url: String,
        complete: Option<String>,
        code: Option<String>,
        method: &'static str,
        for_ms: u64,
    },
    /// It ended: the token is in the login cache, or why not.
    Done(Result<(), String>),
}

/// What an on-use dial came back with.
pub(crate) struct Dialed {
    pub found: Option<(crate::mcp::client::McpClient, Vec<::mcp::wire::Tool>)>,
    /// A person signs in first, by this `auth` spec — or nobody can, and why.
    pub sign_in: Option<Result<AuthSpec, String>>,
    /// The `auth` spec agentd found for the server itself and connected by.
    pub auth: Option<AuthSpec>,
}

/// Whether `auth` is an interactive grant a person signs in by.
#[cfg_attr(not(feature = "oauth"), allow(dead_code))]
pub(crate) fn interactive(auth: &AuthSpec) -> bool {
    auth.kind == "oauth2"
        && matches!(
            auth.grant.as_deref(),
            None | Some("device") | Some("authorization_code")
        )
}

/// The `auth` spec of `spec` when a person must sign in before it is
/// dialed: an interactive grant for which the login cache holds no token,
/// nor anything to renew one from.
#[cfg(feature = "oauth")]
fn wants_sign_in(spec: &McpServerSpec, timeout: Duration) -> Option<AuthSpec> {
    let auth = spec.auth.as_ref().filter(|a| interactive(a))?;
    let target = format!("mcp:{}", spec.name);
    let source = crate::auth::device::token_source_for(auth, &target, timeout).ok()??;
    source.wants_login().map(|_| auth.clone())
}

/// The dial of an on-use server, off the loop: `dial` with `spec` — unless a
/// person must sign in first. A server with no `auth` (`discovered` is how
/// agentd would sign in to it) whose dial fails is asked for its RFC 9728
/// metadata: one that names an authorization server wants a token, which
/// the login cache may already hold from an earlier sign-in — then it is
/// dialed with it — or a person signs in for.
pub(crate) fn dial_on_use(
    spec: &McpServerSpec,
    discovered: Option<&AuthSpec>,
    timeout: Duration,
    dial: impl Fn(&McpServerSpec) -> Option<(crate::mcp::client::McpClient, Vec<::mcp::wire::Tool>)>,
) -> Dialed {
    let none = |found| Dialed {
        found,
        sign_in: None,
        auth: None,
    };
    #[cfg(feature = "oauth")]
    if let Some(a) = wants_sign_in(spec, timeout) {
        return Dialed {
            found: None,
            sign_in: Some(Ok(a)),
            auth: None,
        };
    }
    let found = dial(spec);
    #[cfg(feature = "oauth")]
    if found.is_none()
        && let Some(d) = discovered
    {
        let asks = |sign_in| Dialed {
            found: None,
            sign_in: Some(sign_in),
            auth: None,
        };
        match crate::auth::challenge::discover_issuer(&spec.endpoint, d.catalog.as_deref(), timeout)
        {
            // Not a server that asks for a token: down, or refusing for
            // another reason.
            Ok(None) => {}
            Err(e) => return asks(Err(e)),
            Ok(Some(_)) if d.client_id.is_none() => return asks(Err(no_client(&spec.name))),
            Ok(Some(_)) => {
                let mut with = spec.clone();
                with.auth = Some(d.clone());
                if let Some(a) = wants_sign_in(&with, timeout) {
                    return asks(Ok(a));
                }
                return Dialed {
                    found: dial(&with),
                    sign_in: None,
                    auth: Some(d.clone()),
                };
            }
        }
    }
    #[cfg(not(feature = "oauth"))]
    let _ = (discovered, timeout);
    none(found)
}

/// Why nobody can sign in to `server`, which asks for a token and has no
/// `auth`, with no `security.client` configured.
#[cfg_attr(not(feature = "oauth"), allow(dead_code))]
fn no_client(server: &str) -> String {
    format!(
        "the MCP server {server:?} asks for authorization, its mcp.servers entry has no `auth`, and agentd has no client to sign in as: set security.client (agentd's Client ID Metadata Document), or the entry's `auth`"
    )
}

/// The one word a failed sign-in is said with.
pub(crate) fn reason_of(err: &str) -> &'static str {
    if err == CANCELLED {
        "cancelled"
    } else if err.contains("access_denied") {
        "denied"
    } else if err.contains("expired") {
        "expired"
    } else {
        "failed"
    }
}

/// Sleep `d`, in slices, unless `cancel` is set first.
#[cfg_attr(not(feature = "oauth"), allow(dead_code))]
fn sleep_unless(cancel: &AtomicBool, d: Duration) -> Result<(), String> {
    let until = std::time::Instant::now() + d;
    loop {
        if cancel.load(Ordering::SeqCst) {
            return Err(CANCELLED.into());
        }
        let left = until.saturating_duration_since(std::time::Instant::now());
        if left.is_zero() {
            return Ok(());
        }
        std::thread::sleep(left.min(Duration::from_millis(100)));
    }
}

/// The question a sign-in's gate asks.
#[cfg_attr(not(feature = "a2a"), allow(dead_code))]
fn question(server: &str, p: &Prompted) -> String {
    match (&p.complete, &p.code) {
        (Some(full), Some(code)) => format!(
            "Sign in to the MCP server {server:?}: open {full} and check that the page shows the code {code}. This closes by itself once you have signed in there; answer cancel to stop."
        ),
        (None, Some(code)) => format!(
            "Sign in to the MCP server {server:?}: open {} and enter the code {code}. This closes by itself once you have signed in there; answer cancel to stop.",
            p.url
        ),
        (_, None) => format!(
            "Sign in to the MCP server {server:?}: open {} in a browser on the machine agentd runs on. This closes by itself once you have signed in there; answer cancel to stop.",
            p.url
        ),
    }
}

/// The shape a sign-in gate's answer takes: the one answer is `cancel`.
#[cfg(feature = "a2a")]
fn answer_schema() -> Value {
    json!({
        "type": "object",
        "properties": {"decision": {"type": "string", "enum": ["cancel"]}},
        "required": ["decision"],
    })
}

impl Runtime {
    /// The servers being signed in to, for a turn to be told.
    pub(crate) fn mcp_signing_in(&self) -> Vec<String> {
        self.mcp_sign_ins.keys().cloned().collect()
    }

    /// Start signing in to `server` by `spec`, unless that is under way: the
    /// grant on a thread of its own, its prompt and its end sent back as
    /// [`Step`]s. The server is dialed with `spec` from now on.
    pub(crate) fn start_mcp_sign_in(&mut self, server: &str, spec: AuthSpec) {
        if self.mcp_sign_ins.contains_key(server) {
            return;
        }
        if let Some(s) = self.mcp_specs.get_mut(server) {
            s.auth = Some(spec.clone());
        }
        let method = if spec.grant.as_deref() == Some("authorization_code") {
            "browser"
        } else {
            "device"
        };
        let cancel = Arc::new(AtomicBool::new(false));
        self.mcp_sign_ins.insert(
            server.to_string(),
            McpSignIn {
                method,
                prompt: None,
                until: None,
                cancel: Arc::clone(&cancel),
                gates: Vec::new(),
            },
        );
        #[cfg(feature = "oauth")]
        {
            use crate::auth::login::Prompt;
            let timeout = self
                .settings
                .mcp
                .servers
                .iter()
                .find(|s| s.name == server)
                .and_then(|s| s.timeout)
                .or(self.settings.mcp.default_timeout)
                .map_or(Duration::from_secs(30), |d| d.0);
            let redirect = self
                .settings
                .security
                .client
                .as_ref()
                .and_then(|c| c.redirect_uris.first().cloned());
            let tx = self.events_tx.clone();
            let srv = server.to_string();
            let spawned = std::thread::Builder::new()
                .name(format!("mcp-sign-in:{server}"))
                .spawn(move || {
                    let send = |step: Step| {
                        let _ = tx.send(super::events::Event::McpSignIn {
                            server: srv.clone(),
                            step,
                        });
                    };
                    let done = crate::auth::login::sign_in(
                        &spec,
                        &format!("mcp:{srv}"),
                        redirect,
                        timeout,
                        Duration::from_millis(SIGN_IN_FOR_MS),
                        |p| match p {
                            Prompt::Device(p) => send(Step::Prompt {
                                url: p.verification_uri.to_string(),
                                complete: p.verification_uri_complete.map(str::to_string),
                                code: Some(p.user_code.to_string()),
                                method: "device",
                                for_ms: p
                                    .expires_in
                                    .map_or(SIGN_IN_FOR_MS, |s| s.saturating_mul(1000)),
                            }),
                            Prompt::Browser(url) => send(Step::Prompt {
                                url: url.to_string(),
                                complete: None,
                                code: None,
                                method: "browser",
                                for_ms: SIGN_IN_FOR_MS,
                            }),
                        },
                        |d| sleep_unless(&cancel, d),
                    );
                    send(Step::Done(done));
                });
            if let Err(e) = spawned {
                self.end_mcp_sign_in(server, Err(format!("its thread did not start: {e}")));
            }
        }
        #[cfg(not(feature = "oauth"))]
        {
            let _ = (spec, cancel, method);
            self.end_mcp_sign_in(
                server,
                Err("this build has no `oauth` feature, and signs in to no server".into()),
            );
        }
    }

    /// Nobody can sign in to `server`, which wants it, for `why`: said, and
    /// each step that waits for it fails.
    pub(crate) fn mcp_sign_in_impossible(&mut self, server: &str, why: &str) {
        self.log.warn(
            "mcp.sign_in.failed",
            json!({"server": server, "reason": "unavailable", "error": why}),
        );
        self.joining.sign_in_failed(
            server,
            now_ms(),
            format!("sign_in_unavailable: nobody can sign in to the MCP server {server:?}: {why}"),
        );
    }

    /// A sign-in moved on: where to sign in is said and shown, or it ended.
    pub(crate) fn on_mcp_sign_in(&mut self, server: &str, step: Step) {
        match step {
            Step::Done(result) => self.end_mcp_sign_in(server, result),
            Step::Prompt {
                url,
                complete,
                code,
                method,
                for_ms,
            } => {
                let Some(t) = self.mcp_sign_ins.get_mut(server) else {
                    return; // ended meanwhile
                };
                t.method = method;
                t.until = Some(now_ms().saturating_add(for_ms));
                t.prompt = Some(Prompted {
                    url: url.clone(),
                    complete,
                    code: code.clone(),
                });
                // The code is a sign-in anyone holding it can complete as
                // themselves: it goes to the gate, where operators see it, and
                // into the log only where there is no gate to show it on (a
                // build or a configuration without the A2A listener).
                #[cfg(feature = "a2a")]
                let gated = self.settings.a2a.listen.is_some();
                #[cfg(not(feature = "a2a"))]
                let gated = false;
                let mut line = json!({"server": server, "url": url, "method": method,
                                      "expires_in_s": for_ms / 1000});
                if !gated {
                    line["code"] = json!(code);
                }
                self.log.info("mcp.sign_in.required", line);
                #[cfg(feature = "a2a")]
                {
                    let waiting = self.runs_waiting_for(server);
                    for run in &waiting {
                        self.mcp_sign_in_gate_for_run(server, run);
                    }
                    if waiting.is_empty() {
                        self.open_mcp_sign_in_gate(server, None);
                    }
                }
            }
        }
    }

    /// The runs with a step waiting for the on-use server `server`.
    #[cfg(feature = "a2a")]
    fn runs_waiting_for(&self, server: &str) -> Vec<String> {
        self.runs
            .iter()
            .filter(|(_, r)| {
                r.steps.values().any(|s| {
                    s.status == crate::engine::run::StepStatus::Suspended
                        && s.wait
                            .as_ref()
                            .is_some_and(|w| w["kind"] == "connect_wait" && w["server"] == server)
                })
            })
            .map(|(id, _)| id.clone())
            .collect()
    }

    /// The sign-in to `server` ended with `result`: said, its gates closed,
    /// and the server dialed now — or each step that waited for it failed,
    /// and the server left alone until something needs it again.
    pub(crate) fn end_mcp_sign_in(&mut self, server: &str, result: Result<(), String>) {
        let Some(track) = self.mcp_sign_ins.remove(server) else {
            return;
        };
        track.cancel.store(true, Ordering::SeqCst);
        let said = match &result {
            Ok(()) => {
                self.log.info(
                    "mcp.sign_in.done",
                    json!({"server": server, "method": track.method}),
                );
                self.joining.dial_now(server);
                format!("Signed in to the MCP server {server:?}; what waited for it goes on.")
            }
            Err(e) => {
                let reason = reason_of(e);
                self.log.warn(
                    "mcp.sign_in.failed",
                    json!({"server": server, "method": track.method, "reason": reason, "error": e}),
                );
                self.joining.sign_in_failed(
                    server,
                    now_ms(),
                    format!(
                        "sign_in_{reason}: the sign-in to the MCP server {server:?} ended {reason}: {e}"
                    ),
                );
                format!("The sign-in to the MCP server {server:?} ended {reason}: {e}")
            }
        };
        #[cfg(feature = "a2a")]
        {
            use crate::a2a::tasks::State;
            let end = match &result {
                Ok(()) => State::Completed,
                Err(e) if e == CANCELLED => State::Canceled,
                Err(_) => State::Failed,
            };
            for g in &track.gates {
                self.close_mcp_sign_in_gate(g, &said, end);
            }
        }
        #[cfg(not(feature = "a2a"))]
        let _ = said;
    }

    /// Each tick: a sign-in past its code's life ends `expired`, and one
    /// whose own gate task was cancelled, or every one of whose gates' tasks
    /// ended — the runs it was for cancelled, say — ends `cancelled`.
    pub(crate) fn poll_mcp_sign_ins(&mut self) {
        if self.mcp_sign_ins.is_empty() {
            return;
        }
        let now = now_ms();
        let ends: Vec<(String, String)> = self
            .mcp_sign_ins
            .iter()
            .filter_map(|(server, t)| {
                #[cfg(feature = "a2a")]
                {
                    let ended = |g: &Gate| {
                        self.tasks
                            .get(&g.task)
                            .is_none_or(|x| x.state.is_terminal())
                    };
                    if t.gates.iter().any(|g| g.standalone && ended(g))
                        || (!t.gates.is_empty() && t.gates.iter().all(ended))
                    {
                        return Some((server.clone(), CANCELLED.to_string()));
                    }
                }
                // A little past the code's life: the grant says it first.
                if t.until.is_some_and(|u| now >= u.saturating_add(5_000)) {
                    return Some((server.clone(), "the device code expired".to_string()));
                }
                None
            })
            .collect();
        for (server, why) in ends {
            self.end_mcp_sign_in(&server, Err(why));
        }
    }

    /// The server whose sign-in gate is open on `task`.
    #[cfg_attr(not(any(feature = "a2a", feature = "companies")), allow(dead_code))]
    pub(crate) fn mcp_sign_in_on(&self, task: &str) -> Option<String> {
        self.mcp_sign_ins
            .iter()
            .find(|(_, t)| t.gates.iter().any(|g| g.task == task))
            .map(|(s, _)| s.clone())
    }

    /// Show the sign-in to `server` on `run`'s task, once its person has
    /// somewhere to sign in and the run is not shown it yet.
    #[cfg(feature = "a2a")]
    pub(crate) fn mcp_sign_in_gate_for_run(&mut self, server: &str, run: &str) {
        let Some(t) = self.mcp_sign_ins.get(server) else {
            return;
        };
        if t.prompt.is_none() || t.gates.iter().any(|g| g.run.as_deref() == Some(run)) {
            return;
        }
        self.open_mcp_sign_in_gate(server, Some(run));
    }

    /// Whether a gate is open on `task`.
    #[cfg(feature = "a2a")]
    fn task_gated(&self, task: &str) -> bool {
        #[cfg(feature = "companies")]
        if self.gate_open_on(task) {
            return true;
        }
        self.mcp_sign_in_on(task).is_some()
            || self.pending.iter().any(|p| {
                matches!(&p.kind, super::reactor::PendingKind::Human { task: t, .. } if t == task)
            })
    }

    /// Open a gate for the sign-in to `server`: on `run`'s task — made for
    /// it when the run has none, and a task of its own in the run's
    /// conversation when the run's holds a gate already — or, for no run, on
    /// a task of its own.
    #[cfg(feature = "a2a")]
    fn open_mcp_sign_in_gate(&mut self, server: &str, run: Option<&str>) {
        use crate::a2a::tasks::{Link, McpSignInGate, State};
        let Some(t) = self.mcp_sign_ins.get(server) else {
            return;
        };
        let Some(p) = t.prompt.clone() else {
            return;
        };
        let method = t.method;
        let principal = crate::a2a::Principal {
            id: "operator".to_string(),
            role: crate::config::settings::Role::Operator,
            ..crate::a2a::Principal::anonymous()
        };
        // A freshly minted id, never the listener's reservation: that
        // belongs to the task the request being served creates.
        let reserved = self.reserved_task_id.take();
        let of_run = run.and_then(|r| {
            self.runs
                .get(r)
                .map(|x| (r.to_string(), x.task.clone(), x.conversation.clone()))
        });
        let (task, standalone) = match of_run {
            Some((_, Some(tid), _))
                if self.tasks.get(&tid).is_some_and(|t| !t.state.is_terminal())
                    && !self.task_gated(&tid) =>
            {
                (tid, false)
            }
            Some((run, None, _)) => {
                let ctx = format!("run-{run}");
                let tid = self.task_create(&ctx, &principal, Link::Run { id: run.clone() }, None);
                // The run's completion drives this task terminal.
                if let Some(r) = self.runs.get_mut(&run) {
                    r.task = Some(tid.clone());
                    r.touch();
                }
                (tid, false)
            }
            Some((run, Some(_), conversation)) => {
                let ctx = conversation.unwrap_or_else(|| format!("run-{run}"));
                let tid = self.task_create(&ctx, &principal, Link::Turn { ctx: ctx.clone() }, None);
                (tid, true)
            }
            None => {
                let ctx = format!("mcp-sign-in-{server}");
                let tid = self.task_create(&ctx, &principal, Link::Turn { ctx: ctx.clone() }, None);
                (tid, true)
            }
        };
        self.reserved_task_id = reserved;
        if let Some(t) = self.tasks.get_mut(&task) {
            t.ask_schema = Some(answer_schema());
            t.security_gate = true;
            t.ask_channel = None;
            t.mcp_sign_in = Some(McpSignInGate {
                server: server.to_string(),
                standalone,
                method: method.to_string(),
                url: p.complete.clone().unwrap_or_else(|| p.url.clone()),
                code: p.code.clone(),
            });
            t.transition(State::InputRequired, Some(question(server, &p)));
        }
        self.task_persist(&task);
        self.task_sync(&task);
        if let Some(t) = self.mcp_sign_ins.get_mut(server) {
            t.gates.push(Gate {
                task,
                run: run.map(str::to_string),
                standalone,
            });
        }
    }

    /// Close the sign-in gate `g`, saying `said`: a task of its own ends
    /// `end`, a run's goes back to working.
    #[cfg(feature = "a2a")]
    fn close_mcp_sign_in_gate(&mut self, g: &Gate, said: &str, end: crate::a2a::tasks::State) {
        use crate::a2a::tasks::State;
        let Some(t) = self.tasks.get_mut(&g.task) else {
            return;
        };
        if t.mcp_sign_in.is_none() {
            return;
        }
        t.mcp_sign_in = None;
        t.ask_schema = None;
        t.security_gate = false;
        if t.state.is_terminal() {
            // Its work ended first: nothing to move on.
        } else if g.standalone {
            t.transition(end, Some(said.to_string()));
        } else {
            t.transition(State::Working, Some(said.to_string()));
        }
        self.task_persist(&g.task);
        self.task_sync(&g.task);
    }

    /// An answer `text` on the sign-in gate `task` (`server`'s): `cancel`
    /// ends the sign-in, and anything else is refused with the gate open.
    #[cfg(feature = "a2a")]
    pub(crate) fn mcp_sign_in_answer(
        &mut self,
        server: &str,
        task: &str,
        text: &str,
        message: &Value,
    ) -> Value {
        let shaped = super::human::shape_answer(text, &answer_schema());
        if shaped["decision"] != "cancel" {
            return super::a2a_server::err_obj(
                ::mcp::rpc::INVALID_PARAMS,
                &format!(
                    "you sign in to the MCP server {server:?} at its authorization server, and this closes by itself once you have; the one answer it takes is cancel"
                ),
            );
        }
        if let Some(t) = self.tasks.get_mut(task) {
            t.record_inbound(message);
        }
        self.end_mcp_sign_in(server, Err(CANCELLED.into()));
        self.task_reply(task)
    }

    /// After a restore: every sign-in gate a previous life left open is
    /// closed, saying so — the grant went with that life. A step still
    /// waiting for the server starts a new sign-in when it looks again.
    #[cfg(feature = "a2a")]
    pub(crate) fn interrupt_mcp_sign_ins(&mut self) {
        use crate::a2a::tasks::State;
        let open: Vec<(String, String, bool)> = self
            .tasks
            .values()
            .filter(|t| !t.state.is_terminal())
            .filter_map(|t| {
                t.mcp_sign_in
                    .as_ref()
                    .map(|g| (t.id.clone(), g.server.clone(), g.standalone))
            })
            .collect();
        for (task, server, standalone) in open {
            let said = format!(
                "The sign-in to the MCP server {server:?} was interrupted: agentd restarted. A step still waiting for the server starts a new one."
            );
            self.close_mcp_sign_in_gate(
                &Gate {
                    task: task.clone(),
                    run: None,
                    standalone,
                },
                &said,
                State::Failed,
            );
            self.log.info(
                "mcp.sign_in.failed",
                json!({"server": server, "reason": "interrupted", "task": task}),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_failed_sign_in_is_said_in_one_word() {
        assert_eq!(reason_of(CANCELLED), "cancelled");
        assert_eq!(
            reason_of("oauth: device token failed: access_denied (HTTP 400)"),
            "denied"
        );
        assert_eq!(
            reason_of("device code expired before authorization"),
            "expired"
        );
        assert_eq!(
            reason_of("oauth: device token failed: expired_token (HTTP 400)"),
            "expired"
        );
        assert_eq!(reason_of("oauth: discovery failed: HTTP 500"), "failed");
    }

    #[test]
    fn only_an_interactive_oauth_grant_is_signed_in_to() {
        let a = |kind: &str, grant: Option<&str>| AuthSpec {
            kind: kind.into(),
            grant: grant.map(str::to_string),
            ..Default::default()
        };
        assert!(interactive(&a("oauth2", None)));
        assert!(interactive(&a("oauth2", Some("device"))));
        assert!(interactive(&a("oauth2", Some("authorization_code"))));
        assert!(!interactive(&a("oauth2", Some("client_credentials"))));
        assert!(!interactive(&a("oauth2", Some("id_jag"))));
        assert!(!interactive(&a("static", None)));
    }

    #[test]
    fn the_gate_says_where_to_sign_in_and_the_code() {
        let p = Prompted {
            url: "https://idp.example/device".into(),
            complete: None,
            code: Some("WDJB-MJHT".into()),
        };
        let q = question("vault", &p);
        assert!(
            q.contains("https://idp.example/device") && q.contains("WDJB-MJHT"),
            "{q}"
        );
        let p = Prompted {
            complete: Some("https://idp.example/device?user_code=WDJB-MJHT".into()),
            ..p
        };
        let q = question("vault", &p);
        assert!(
            q.contains("?user_code=WDJB-MJHT") && q.contains("check that"),
            "{q}"
        );
    }

    /// A resource server and its authorization server on one loopback
    /// origin: an MCP request draws `401` and the RFC 9728 challenge when
    /// `protected`, else `503`; the metadata names `issuer` (the origin
    /// itself when `None`), and the issuer's metadata takes a Client ID
    /// Metadata Document.
    #[cfg(feature = "oauth")]
    fn spawn_protected(protected: bool, issuer: Option<&str>) -> String {
        use std::io::{BufRead, BufReader, Write};
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://127.0.0.1:{}", l.local_addr().unwrap().port());
        let named = issuer.map_or_else(|| base.clone(), str::to_string);
        let at = base.clone();
        std::thread::spawn(move || {
            for c in l.incoming().flatten() {
                let mut s = c;
                let mut r = BufReader::new(s.try_clone().unwrap());
                let mut line = String::new();
                r.read_line(&mut line).ok();
                loop {
                    let mut h = String::new();
                    if r.read_line(&mut h).unwrap_or(0) == 0 || h.trim().is_empty() {
                        break;
                    }
                }
                let path = line.split_whitespace().nth(1).unwrap_or("").to_string();
                let (status, extra, body) = if path
                    .starts_with("/.well-known/oauth-protected-resource")
                    && protected
                {
                    let b =
                        json!({"resource": format!("{at}/mcp"), "authorization_servers": [named]});
                    (200, String::new(), b.to_string())
                } else if path.starts_with("/.well-known/oauth-authorization-server") {
                    let b = json!({"issuer": at, "token_endpoint": format!("{at}/token"),
                                   "device_authorization_endpoint": format!("{at}/device"),
                                   "client_id_metadata_document_supported": true});
                    (200, String::new(), b.to_string())
                } else if path == "/mcp" && protected {
                    let ch = format!(
                        "WWW-Authenticate: Bearer resource_metadata=\"{at}/.well-known/oauth-protected-resource/mcp\"\r\n"
                    );
                    (401, ch, String::new())
                } else {
                    (
                        if path == "/mcp" { 503 } else { 404 },
                        String::new(),
                        String::new(),
                    )
                };
                let _ = write!(
                    s,
                    "HTTP/1.1 {status} X\r\n{extra}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
            }
        });
        base
    }

    #[cfg(feature = "oauth")]
    fn unauthenticated(endpoint: &str) -> McpServerSpec {
        McpServerSpec {
            name: format!("srv-{}", endpoint.rsplit(':').next().unwrap_or_default()),
            endpoint: endpoint.to_string(),
            ..Default::default()
        }
    }

    #[cfg(feature = "oauth")]
    fn as_document(endpoint: &str, client: bool) -> AuthSpec {
        AuthSpec {
            kind: "oauth2".into(),
            grant: Some("device".into()),
            client_id: client.then(|| "https://agent.example/oauth/client.json".to_string()),
            client_id_metadata_document: client,
            protected_resource: Some(endpoint.to_string()),
            ..Default::default()
        }
    }

    /// With no `auth`, a dial the server refuses with its RFC 9728 challenge
    /// starts discovery: the server's metadata, its authorization server's,
    /// and a sign-in by the device grant as agentd's Client ID Metadata
    /// Document.
    #[cfg(feature = "oauth")]
    #[test]
    fn a_server_without_auth_that_asks_for_a_token_is_signed_in_to() {
        let endpoint = format!("{}/mcp", spawn_protected(true, None));
        let d = as_document(&endpoint, true);
        let out = dial_on_use(
            &unauthenticated(&endpoint),
            Some(&d),
            Duration::from_secs(5),
            |_| None,
        );
        assert!(out.found.is_none());
        match out.sign_in {
            Some(Ok(a)) => assert_eq!(a, d),
            other => panic!("{:?}", other.map(|r| r.map(|a| a.kind))),
        }
    }

    /// With no `security.client`, nobody can sign in, and that is said.
    #[cfg(feature = "oauth")]
    #[test]
    fn a_server_that_asks_for_a_token_with_no_client_is_not_signed_in_to() {
        let endpoint = format!("{}/mcp", spawn_protected(true, None));
        let out = dial_on_use(
            &unauthenticated(&endpoint),
            Some(&as_document(&endpoint, false)),
            Duration::from_secs(5),
            |_| None,
        );
        match out.sign_in {
            Some(Err(e)) => assert!(e.contains("security.client"), "{e}"),
            other => panic!("{:?}", other.map(|r| r.map(|a| a.kind))),
        }
    }

    /// An authorization server the server names off its own origin over
    /// plain HTTP is refused, and so is the sign-in.
    #[cfg(feature = "oauth")]
    #[test]
    fn a_named_authorization_server_agentd_may_not_reach_refuses_the_sign_in() {
        let endpoint = format!(
            "{}/mcp",
            spawn_protected(true, Some("http://127.0.0.2:9/issuer"))
        );
        let out = dial_on_use(
            &unauthenticated(&endpoint),
            Some(&as_document(&endpoint, true)),
            Duration::from_secs(5),
            |_| None,
        );
        match out.sign_in {
            Some(Err(e)) => assert!(e.contains("not an https URL"), "{e}"),
            other => panic!("{:?}", other.map(|r| r.map(|a| a.kind))),
        }
    }

    /// A server that is down, not asking for a token, is not signed in to:
    /// its dial is tried again on the backoff.
    #[cfg(feature = "oauth")]
    #[test]
    fn a_server_that_does_not_ask_for_a_token_is_not_signed_in_to() {
        let endpoint = format!("{}/mcp", spawn_protected(false, None));
        let out = dial_on_use(
            &unauthenticated(&endpoint),
            Some(&as_document(&endpoint, true)),
            Duration::from_secs(5),
            |_| None,
        );
        assert!(out.sign_in.is_none() && out.auth.is_none() && out.found.is_none());
    }

    #[test]
    fn a_cancelled_sleep_stops() {
        let cancel = AtomicBool::new(true);
        assert_eq!(
            sleep_unless(&cancel, Duration::from_secs(60)),
            Err(CANCELLED.to_string())
        );
        let cancel = AtomicBool::new(false);
        assert_eq!(sleep_unless(&cancel, Duration::from_millis(5)), Ok(()));
    }
}
