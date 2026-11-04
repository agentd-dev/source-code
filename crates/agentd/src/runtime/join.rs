// SPDX-License-Identifier: AGPL-3.0-only
//! A configured MCP server that is down at boot joins when it comes up.
//!
//! Startup dials every server once and carries on without the ones that do
//! not answer; under compose or Kubernetes start ordering that is the normal
//! case, not the exception. Each such server is dialed again off the loop,
//! with a backoff (1 s, doubling, at most 60 s) — the same dial boot makes:
//! the handshake, then its tool list. When one answers, the loop adopts that
//! connection as it is: the server's tools join the registry beside every
//! other server's as last built (no other server is listed again — a
//! catalogue changes only at a reload the operator starts), the workflow
//! tools are registered over the installed definitions, and what lives on
//! the connection — `subscribe` starts, resource waits, MCP Events bindings —
//! is restored on it. No document is read, no start is armed again, nothing
//! else moves.
//!
//! A Company's MCP server (RFC 0047 §5.5.1) joins the same way and only this
//! way, every boot included: its dial is the Company's discovery, the
//! supervisor's Session and a Bearer token for the server, then the same
//! handshake and tool list. A Company that does not answer, or does not
//! verify, is tried again on the same backoff; one whose server refuses this
//! agent outright (`extension_required`) is not tried again.
//!
//! So do the tools of a Company's OpenAPI operations (RFC 0047 §9, P5a): the
//! server they are named under joins once the supervisor has read the API's
//! description and made a tool of each operation `api_allow` lists that it
//! declares, every boot included, on the same backoff. A fetch that later
//! finds the description changed has them listed again: tools that changed
//! replace the server's in the registry as a join's do, and the change is
//! logged (`company.api.changed`); unchanged, nothing moves.

use super::events::Event;
use super::reactor::Runtime;
use crate::registry::{Registry, ServerTools};
use crate::state::now_ms;
use serde_json::json;
use std::collections::BTreeMap;
use std::sync::Arc;

const BACKOFF_MIN_MS: u64 = 1_000;
const BACKOFF_MAX_MS: u64 = 60_000;
/// How often a step waiting for an on-use server looks again.
const CONNECT_POLL_MS: u64 = 500;

/// The servers without a live connection, and where their dials are.
#[derive(Default)]
pub(crate) struct Joining {
    probes: BTreeMap<String, Probe>,
    /// The operation tools each Company OpenAPI API offers, as adopted.
    offered: BTreeMap<String, Operations>,
    /// The APIs whose description changed since they were adopted: listed
    /// again.
    stale: std::collections::BTreeSet<String>,
    /// The servers being dialled when a description changed: an API among
    /// them, whose listing may have read it before, is listed once more.
    again: std::collections::BTreeSet<String>,
    /// The `connect: on_use` servers something has needed: dialed until they
    /// connect (RFC 0055 §4.3).
    wanted: std::collections::BTreeSet<String>,
    /// When each step waiting for an on-use server (`run/step`) began to.
    connect_waits: BTreeMap<String, u64>,
    /// When each step waiting for a delegated server's tool catalogue an
    /// enrolment is listing (`run/step`) began to (`catalogue_wait`).
    pub(crate) catalogue_waits: BTreeMap<String, u64>,
    /// The on-use servers whose sign-in failed: not dialed again until
    /// something needs one anew, so a refused sign-in does not ask again on
    /// every backoff.
    held: std::collections::BTreeSet<String>,
    /// When each server's last sign-in failed, and what a step that waited
    /// for it fails with.
    sign_in_failed: BTreeMap<String, (u64, String)>,
}

/// The tools of a Company OpenAPI API's operations (RFC 0047 §9, P5a): the
/// description's URL they were made from, each tool, named within its
/// server, and what each one's call is made of.
#[derive(Debug, Clone)]
pub(crate) struct Operations {
    pub url: String,
    pub tools: Vec<::mcp::wire::Tool>,
    #[cfg(feature = "companies")]
    pub shapes: BTreeMap<String, crate::companies::api::Shape>,
    /// The Company verified and lists no API the server stands for: none
    /// are offered, and it is listed again on the backoff until it does.
    pub withheld: bool,
}

impl Joining {
    /// The step `run/step`'s wait for a delegated server's catalogue ended
    /// — it found its tool, finished or was cancelled — so the next wait it
    /// may begin begins anew rather than at this one's start.
    pub(crate) fn catalogue_wait_ended(&mut self, run: &str, step: &str) {
        self.catalogue_waits.remove(&format!("{run}/{step}"));
    }

    /// The run `run` ended: no step of it waits for anything.
    pub(crate) fn run_ended(&mut self, run: &str) {
        let prefix = format!("{run}/");
        self.catalogue_waits.retain(|k, _| !k.starts_with(&prefix));
    }

    /// The sign-in of `server` ended well: it is dialed again now, its
    /// backoff forgotten.
    pub(crate) fn dial_now(&mut self, server: &str) {
        if let Some(p) = self.probes.get_mut(server) {
            p.failures = 0;
            p.due_at = 0;
        }
    }

    /// The sign-in of `server` failed at `at`: each step that waited for it
    /// fails with `why`, and the server is not dialed again until something
    /// needs it anew.
    pub(crate) fn sign_in_failed(&mut self, server: &str, at: u64, why: String) {
        self.sign_in_failed.insert(server.to_string(), (at, why));
        self.held.insert(server.to_string());
        self.wanted.remove(server);
    }

    /// Something needs the server `server`: dialed until it connects.
    pub(crate) fn want(&mut self, server: &str) {
        self.wanted.insert(server.to_string());
    }

    /// Whether something has needed the on-use server `server`.
    pub(crate) fn wants(&self, server: &str) -> bool {
        self.wanted.contains(server)
    }

    /// Whether the Company OpenAPI API `server` offers its operation tools.
    pub(crate) fn offers(&self, server: &str) -> bool {
        self.offered.contains_key(server)
    }

    /// Each Company OpenAPI API's operation tools, as adopted.
    pub(crate) fn offered(&self) -> impl Iterator<Item = (&String, &Operations)> {
        self.offered.iter()
    }

    /// What the call of the operation tool `tool` of the Company OpenAPI API
    /// `server` is made of, as adopted.
    #[cfg(feature = "companies")]
    pub(crate) fn shape(&self, server: &str, tool: &str) -> Option<&crate::companies::api::Shape> {
        self.offered.get(server)?.shapes.get(tool)
    }

    /// The description at `url` changed: the operation tools of each API
    /// made from it are listed again — after the listing out now, for one
    /// being listed.
    #[cfg(feature = "companies")]
    pub(crate) fn description_changed(&mut self, url: &str) {
        for (server, p) in &self.probes {
            if p.out {
                self.again.insert(server.clone());
            }
        }
        for (server, o) in &self.offered {
            if o.url == url && !self.again.contains(server) {
                self.stale.insert(server.clone());
            }
        }
    }

    /// The APIs a Company lists changed: the operation tools of each of
    /// `servers` are listed again — after the listing out now, for one being
    /// listed — so they follow an API that moved, and are withdrawn from one
    /// no longer listed.
    #[cfg(feature = "companies")]
    pub(crate) fn apis_changed(&mut self, servers: &[String]) {
        for server in servers {
            if self.probes.get(server).is_some_and(|p| p.out) {
                self.again.insert(server.clone());
            } else if self.offered.contains_key(server) {
                self.stale.insert(server.clone());
            }
        }
    }
}

/// What a dial found: a connection and its tools, or a Company OpenAPI API's
/// operation tools.
enum Found {
    Mcp(Box<crate::mcp::client::McpClient>, Vec<::mcp::wire::Tool>),
    #[cfg_attr(not(feature = "companies"), allow(dead_code))]
    Operations(Operations),
}

#[derive(Default)]
struct Probe {
    failures: u32,
    due_at: u64,
    out: bool,
}

/// What a dial off the loop came back with: the initialized connection and
/// its tools, a Company OpenAPI API's operation tools, or nothing — and
/// whether the server refused this agent for good.
pub struct Reachable {
    server: String,
    found: Option<Found>,
    refused: Option<String>,
    /// The server needs a person to sign in first (RFC 0055 §4.3): with
    /// the `auth` spec to sign in by, or why agentd cannot.
    sign_in: Option<Result<crate::config::AuthSpec, String>>,
    /// The `auth` spec the connection was made with, where agentd found it
    /// itself — a server with no `auth` whose RFC 9728 metadata named its
    /// authorization server — to dial it with from now on.
    auth: Option<crate::config::AuthSpec>,
    /// A registry server's resolution, found or refused, whatever the dial
    /// that followed it came to.
    registry: Option<super::registries::Resolved>,
}

impl std::fmt::Debug for Reachable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Reachable")
            .field("server", &self.server)
            .field("found", &self.found.is_some())
            .finish()
    }
}

/// The wait before the next dial, after `failures` in a row.
fn backoff(failures: u32) -> u64 {
    BACKOFF_MIN_MS
        .saturating_mul(1u64 << failures.saturating_sub(1).min(16))
        .min(BACKOFF_MAX_MS)
}

impl Runtime {
    /// Every pass: dial, off the loop, each configured server that has no
    /// live connection and whose dial is due.
    pub(crate) fn join_tick(&mut self) {
        let now = now_ms();
        let down: Vec<String> = self
            .settings
            .mcp
            .servers
            .iter()
            .map(|s| s.name.clone())
            .filter(|n| {
                !self.mcp.contains_key(n)
                    && (!self.joining.offers(n) || self.joining.stale.contains(n))
            })
            // A delegated server is never dialled with agentd's own
            // credential: its connections are the supervisor's pool's.
            .filter(|n| !self.mcp_specs.get(n).is_some_and(|s| s.delegated))
            // An on-use server is dialed once something needs it: a step, a
            // wait, a start, or the instruction that declared it — and not
            // while a person signs in to it, nor after a sign-in that failed
            // until something needs it again.
            .filter(|n| {
                !self.settings.mcp.on_use(n)
                    || ((self.joining.wanted.contains(n)
                        || self.settings.mcp.document_matched.contains(n))
                        && !self.mcp_sign_ins.contains_key(n)
                        && !self.joining.held.contains(n))
            })
            .collect();
        self.joining.probes.retain(|n, _| down.contains(n));
        for name in down {
            let Some(spec) = self.mcp_specs.get(&name).cloned() else {
                continue;
            };
            let probe = self.joining.probes.entry(name.clone()).or_default();
            if probe.out || now < probe.due_at {
                continue;
            }
            probe.out = true;
            let timeout = self
                .settings
                .mcp
                .servers
                .iter()
                .find(|s| s.name == name)
                .and_then(|s| s.timeout.map(|d| d.0))
                .or(self.settings.mcp.default_timeout.map(|d| d.0))
                .unwrap_or(std::time::Duration::from_secs(60));
            let tx = self.events_tx.clone();
            let server = name.clone();
            // An on-use server with no `auth` that turns out to want one is
            // signed in to as this says (RFC 0055 §4.3). A registry server is
            // dialed on use too, and its endpoint is known only once its
            // record is read: discovery for it runs on the dial's thread.
            let no_auth = spec.auth.is_none() && spec.oauth.is_none();
            let on_use = self.settings.mcp.on_use(&name) || spec.registry.is_some();
            let discovered = (on_use && no_auth && spec.registry.is_none())
                .then(|| self.settings.discovered_mcp_auth(&spec.endpoint));
            let discover_with = (spec.registry.is_some() && no_auth).then(|| self.settings.clone());
            // A registry server's record, as last read, for the dial to reuse
            // while it is fresh.
            let cached = spec.registry.as_ref().and_then(|d| self.registry_cached(d));
            #[cfg(feature = "companies")]
            let companies = self.companies.clone();
            #[cfg(feature = "companies")]
            let api = crate::config::settings::companies::is_api_server(&self.settings, &name);
            let spawned = std::thread::Builder::new()
                .name("agentd-join".into())
                .spawn(move || {
                    // The dial boot makes, on its own thread: the loop is
                    // never held by a server that does not answer.
                    let (mut sign_in, mut auth, mut registry) = (None, None, None);
                    let (found, refused) = match spec.company {
                        // A Company's OpenAPI operations: its description,
                        // read and made into tools.
                        #[cfg(feature = "companies")]
                        Some(_) if api => (
                            companies
                                .as_ref()
                                .and_then(|sup| sup.operation_tools(&server).ok())
                                .map(|o| {
                                    Found::Operations(Operations {
                                        url: o.url,
                                        tools: o.tools,
                                        shapes: o.shapes,
                                        withheld: o.withheld.is_some(),
                                    })
                                }),
                            None,
                        ),
                        #[cfg(feature = "companies")]
                        Some(_) => match companies
                            .as_ref()
                            .map(|sup| crate::companies::mcp::connect(sup, &server, timeout))
                        {
                            Some(Ok((client, tools))) => {
                                (Some(Found::Mcp(Box::new(client), tools)), None)
                            }
                            Some(Err(crate::companies::mcp::Refused::Unusable(e))) => {
                                (None, Some(e))
                            }
                            _ => (None, None),
                        },
                        #[cfg(not(feature = "companies"))]
                        Some(_) => (None, None),
                        None => {
                            let dial = |s: &crate::config::McpServerSpec| {
                                crate::mcp::from_spec(s, timeout)
                                    .map(super::supervisor_client)
                                    .and_then(|mut c| c.initialize().map(|()| c))
                                    .and_then(|c| c.list_tools().map(|t| (c, t)))
                                    .ok()
                            };
                            // A registry server: its record first, then the
                            // dial it names (RFC 0055 §4.1); refused, there
                            // is nothing to dial and the resolution says why.
                            registry = spec
                                .registry
                                .as_ref()
                                .map(|_| super::registries::resolve(&spec, cached));
                            let target = match &registry {
                                Some(r) => r.spec.clone(),
                                None => Some(spec.clone()),
                            };
                            match target {
                                None => (None, None),
                                Some(t) if on_use => {
                                    let found_auth = discovered.clone().or_else(|| {
                                        discover_with
                                            .as_ref()
                                            .map(|st| st.discovered_mcp_auth(&t.endpoint))
                                    });
                                    let dialed = super::mcp_sign_in::dial_on_use(
                                        &t,
                                        found_auth.as_ref(),
                                        timeout,
                                        dial,
                                    );
                                    sign_in = dialed.sign_in;
                                    auth = dialed.auth;
                                    (dialed.found.map(|(c, t)| Found::Mcp(Box::new(c), t)), None)
                                }
                                Some(t) => (
                                    dial(&t).map(|(c, tools)| Found::Mcp(Box::new(c), tools)),
                                    None,
                                ),
                            }
                        }
                    };
                    let _ = tx.send(Event::McpReachable(Box::new(Reachable {
                        server,
                        found,
                        refused,
                        sign_in,
                        auth,
                        registry,
                    })));
                });
            if spawned.is_err()
                && let Some(p) = self.joining.probes.get_mut(&name)
            {
                p.out = false;
            }
        }
    }

    /// Ask for the `connect: on_use` server `server` to be dialed, if it is
    /// one and not connected: `true` when it is (so the caller waits for it).
    pub(crate) fn want_server(&mut self, server: &str) -> bool {
        if !self.settings.mcp.on_use(server) || self.mcp.contains_key(server) {
            return false;
        }
        // A registry server whose record was refused is not waited for: the
        // call fails now, with the refusal.
        if self.registries.refused.contains_key(server) {
            return false;
        }
        if self.joining.wanted.insert(server.to_string()) {
            self.joining.held.remove(server);
            self.log
                .info("mcp.connect.on_use", json!({"server": server}));
        }
        true
    }

    /// Hold the step `run_id/step_id` until the on-use server `server` it
    /// names connects: `true` when it is held — suspended on a durable timer
    /// that puts it back to pending, as a rate wait does, so a restart keeps
    /// it waiting (when it began is kept with it) — or has failed here.
    /// Once the server's `timeout` (else `mcp.default_timeout`, else 60 s)
    /// has passed it is let go, and the call fails as one to a server that
    /// is not connected does, for its `retry:` to take.
    ///
    /// While a person signs in to the server (RFC 0055 §4.3) the wait is
    /// the sign-in's: it lasts the step's `timeout` (`step_timeout`), else
    /// the server's own `timeout`, else as long as the grant's code does,
    /// and fails the step `sign_in_timeout` once that has passed; the
    /// step's run is shown the sign-in gate. A sign-in that fails while
    /// the step waits fails it with the sign-in's reason.
    pub(crate) fn connect_wait(
        &mut self,
        run_id: &str,
        step_id: &str,
        server: &str,
        step_timeout: Option<u64>,
    ) -> bool {
        let key = format!("{run_id}/{step_id}");
        if let Some(&since) = self.joining.connect_waits.get(&key)
            && let Some((at, why)) = self.joining.sign_in_failed.get(server)
            && since <= *at
        {
            let why = why.clone();
            self.joining.connect_waits.remove(&key);
            self.fail_connect_wait(run_id, step_id, why);
            return true;
        }
        if !self.want_server(server) {
            self.joining.connect_waits.remove(&key);
            return false;
        }
        let now = now_ms();
        let started = *self.joining.connect_waits.entry(key.clone()).or_insert(now);
        let entry = self.settings.mcp.servers.iter().find(|s| s.name == server);
        let signing = self.mcp_sign_ins.get(server).map(|s| s.until);
        let limit = match signing {
            Some(until) => step_timeout
                .or(entry
                    .and_then(|s| s.timeout)
                    .map(|d| d.0.as_millis() as u64))
                .unwrap_or_else(|| {
                    until.map_or(super::mcp_sign_in::SIGN_IN_FOR_MS, |u| {
                        u.saturating_sub(started)
                    })
                }),
            None => entry
                .and_then(|s| s.timeout)
                .or(self.settings.mcp.default_timeout)
                .map_or(60_000, |d| d.0.as_millis() as u64),
        };
        if now.saturating_sub(started) > limit {
            self.joining.connect_waits.remove(&key);
            if signing.is_some() {
                self.run_log(run_id).info(
                    "step.sign_in_timeout",
                    json!({"run": run_id, "step": step_id, "server": server}),
                );
                self.fail_connect_wait(
                    run_id,
                    step_id,
                    format!(
                        "sign_in_timeout: nobody signed in to the MCP server {server:?} within {}s",
                        limit / 1000
                    ),
                );
                return true;
            }
            return false;
        }
        match self.timers.arm(
            &self.durable,
            now + CONNECT_POLL_MS,
            json!({"kind": "step_budget", "run": run_id, "step": step_id}),
            serde_json::Value::Null,
        ) {
            Ok(id) => {
                if started == now {
                    self.run_log(run_id).info(
                        "step.connect_wait",
                        json!({"run": run_id, "step": step_id, "server": server}),
                    );
                }
                if let Some(r) = self.runs.get_mut(run_id) {
                    r.suspend_step(
                        step_id,
                        json!({"kind": "connect_wait", "server": server, "timer": id, "since": started}),
                    );
                }
                #[cfg(feature = "a2a")]
                if signing.is_some() {
                    self.mcp_sign_in_gate_for_run(server, run_id);
                }
                self.checkpoint(false);
                true
            }
            Err(_) => false,
        }
    }

    /// A connect wait's step fails with `why`: as an attempt, so a `retry:`
    /// counts it and a failure that repeats ends.
    fn fail_connect_wait(&mut self, run_id: &str, step_id: &str, why: String) {
        if let Some(r) = self.runs.get_mut(run_id) {
            r.begin_step(step_id);
        }
        self.finish_step_pub(
            run_id,
            step_id,
            crate::engine::run::StepStatus::Failed,
            None,
            Some(why),
            0,
        );
    }

    /// A step that waited for an on-use server was restored, its timer
    /// fired: the wait keeps the instant it began, so a restart does not
    /// restart its timeout.
    pub(crate) fn connect_wait_restored(&mut self, run_id: &str, step_id: &str, since: u64) {
        self.joining
            .connect_waits
            .entry(format!("{run_id}/{step_id}"))
            .or_insert(since);
    }

    /// A dial came back. A connection is adopted; nothing is dialed again
    /// after a backoff.
    pub(crate) fn on_mcp_reachable(&mut self, r: Reachable) {
        let now = now_ms();
        let Reachable {
            server,
            found,
            refused,
            sign_in,
            auth,
            registry,
        } = r;
        // A registry server's resolution is said and kept whatever came of
        // the dial; one refused is dialed again once its record is stale.
        let registry_refusal = registry
            .as_ref()
            .and_then(|r| self.on_registry_resolved(&server, r));
        let Some(probe) = self.joining.probes.get_mut(&server) else {
            return; // connected meanwhile (a reload dialed it), or gone
        };
        probe.out = false;
        if self.mcp.contains_key(&server) {
            return;
        }
        if let Some(retry_in) = registry_refusal {
            probe.due_at = now + retry_in;
            return;
        }
        // A person signs in before the server is dialed again (RFC 0055
        // §4.3); or nobody can, and the steps that wait for it fail.
        match sign_in {
            Some(Ok(spec)) => {
                self.start_mcp_sign_in(&server, spec);
                return;
            }
            Some(Err(why)) => {
                self.mcp_sign_in_impossible(&server, &why);
                return;
            }
            None => {}
        }
        // Found by discovery: the connection's credential is the server's
        // from now on, for every client built for it.
        if let Some(a) = &auth
            && let Some(s) = self.mcp_specs.get_mut(&server)
        {
            s.auth = Some(a.clone());
        }
        // A server that refused this agent outright is not dialled again in
        // this life.
        if let Some(e) = refused {
            probe.due_at = u64::MAX;
            self.log.warn(
                "mcp.connect.fail",
                json!({"server": server, "err": e, "reason": "refused"}),
            );
            return;
        }
        let adopted = match found {
            Some(Found::Mcp(client, tools)) => {
                // A registry server runs on the spec it was resolved to: what
                // a child is handed, and what a reload compares — with the
                // credential discovery found, which it was resolved without.
                if let Some((spec, version)) = registry.and_then(|r| r.adopted(auth)) {
                    self.registry_adopted(&server, spec, version);
                }
                self.join_adopt(&server, *client, tools)
            }
            // The Company lists no API the server stands for: tools it
            // offered are withdrawn, and it is listed again on the backoff —
            // a listing reads discovery again once its copy expires — until
            // the Company lists one.
            Some(Found::Operations(o)) if o.withheld => {
                probe.failures += 1;
                probe.due_at = now + backoff(probe.failures);
                if !self.joining.offers(&server) {
                    return;
                }
                let withdrawn = self.adopt_operations(&server, o);
                self.joining.stale.insert(server.clone());
                withdrawn
            }
            Some(Found::Operations(o)) => self.adopt_operations(&server, o),
            None => {
                probe.failures += 1;
                probe.due_at = now + backoff(probe.failures);
                return;
            }
        };
        if let Err(errors) = adopted {
            self.log.warn(
                "mcp.join.refused",
                json!({"server": server, "errors": errors}),
            );
            if let Some(p) = self.joining.probes.get_mut(&server) {
                p.failures += 1;
                p.due_at = now + backoff(p.failures);
            }
        }
    }

    /// The registry with `server`'s tools as `tools`, beside the others' as
    /// last built, the installed workflows' tools registered again over them.
    /// `None` when `server` is no longer configured.
    pub(crate) fn registry_with(
        &self,
        server: &str,
        tools: Vec<::mcp::wire::Tool>,
    ) -> Result<Option<Registry>, Vec<String>> {
        let Some(s) = self.settings.mcp.servers.iter().find(|s| s.name == server) else {
            return Ok(None);
        };
        let mut built: Vec<ServerTools> = self
            .registry
            .built_from()
            .iter()
            .filter(|t| t.name != server)
            .cloned()
            .collect();
        built.push(ServerTools {
            name: server.to_string(),
            ns: s.ns.clone(),
            tags: self
                .mcp_specs
                .get(server)
                .map(|sp| sp.tags.clone())
                .unwrap_or_default(),
            tools,
        });
        let mut registry = Registry::build(&self.settings, &built)?;
        let defs: Vec<&crate::engine::Workflow> = self.workflows.values().map(|w| &**w).collect();
        let errs = registry.register_workflow_tools(&self.settings, &defs);
        if !errs.is_empty() {
            return Err(errs);
        }
        for w in &registry.warnings {
            self.log.warn("registry.warning", json!({"warning": w}));
        }
        Ok(Some(registry))
    }

    /// Adopt the operation tools `offered` of the Company OpenAPI API
    /// `server`: the first time, and whenever a listing after a changed
    /// description finds them changed, they replace the server's tools in
    /// the registry, said `company.api.joined` or `company.api.changed`;
    /// unchanged, nothing moves.
    fn adopt_operations(&mut self, server: &str, offered: Operations) -> Result<(), Vec<String>> {
        self.joining.stale.remove(server);
        if self.joining.again.remove(server) {
            self.joining.stale.insert(server.to_string());
        }
        let shape = |tools: &[::mcp::wire::Tool]| -> BTreeMap<String, serde_json::Value> {
            tools
                .iter()
                .map(|t| (t.name.clone(), serde_json::to_value(t).unwrap_or_default()))
                .collect()
        };
        let now = shape(&offered.tools);
        let was = self.joining.offered.get(server).map(|o| shape(&o.tools));
        if was.as_ref() == Some(&now) {
            self.joining.offered.insert(server.to_string(), offered);
            return Ok(());
        }
        let Some(registry) = self.registry_with(server, offered.tools.clone())? else {
            return Ok(()); // no longer configured
        };
        self.registry = registry;
        let company = self
            .settings
            .mcp
            .servers
            .iter()
            .find(|s| s.name == server)
            .and_then(|s| s.company.as_ref())
            .map(|c| c.entry.clone());
        match was {
            None => self.log.info(
                "company.api.joined",
                json!({"company": company, "server": server, "api": offered.url,
                       "tools": now.keys().collect::<Vec<_>>()}),
            ),
            Some(was) => self.log.info(
                "company.api.changed",
                json!({"company": company, "server": server, "api": offered.url,
                       "added": now.keys().filter(|k| !was.contains_key(*k)).collect::<Vec<_>>(),
                       "removed": was.keys().filter(|k| !now.contains_key(*k)).collect::<Vec<_>>(),
                       "changed": now.iter().filter(|(k, v)| was.get(*k).is_some_and(|w| w != *v)).map(|(k, _)| k).collect::<Vec<_>>()}),
            ),
        }
        self.joining.offered.insert(server.to_string(), offered);
        self.log_workflow_tools();
        Ok(())
    }

    /// Adopt `server`'s connection: its tools beside the others' as last
    /// built, the installed workflows' tools registered again over them, and
    /// what lives on the connection restored.
    fn join_adopt(
        &mut self,
        server: &str,
        mut client: crate::mcp::client::McpClient,
        tools: Vec<::mcp::wire::Tool>,
    ) -> Result<(), Vec<String>> {
        self.joining.again.remove(server);
        let count = tools.len();
        let Some(registry) = self.registry_with(server, tools)? else {
            return Ok(()); // no longer configured
        };
        // A Company under the strict privacy profile is sent no run or
        // instance (RFC 0047 §5.10).
        #[cfg(feature = "companies")]
        let strict = self
            .companies
            .as_ref()
            .and_then(|c| c.company_of_server(server))
            .is_some_and(|c| c.privacy == crate::config::settings::companies::Privacy::Strict);
        #[cfg(not(feature = "companies"))]
        let strict = false;
        if !strict {
            client.set_tool_meta(crate::mcp::tool_meta(&self.run_id, &self.instance));
        }
        super::report_dropped_tools(&self.log, server, &client);
        let protocol = client.protocol_version();
        self.registry = registry;
        self.mcp.insert(server.to_string(), Arc::new(client));
        self.log.info(
            "mcp.connect",
            json!({"server": server, "reason": "joined", "tools": count, "protocol": protocol}),
        );
        self.log_workflow_tools();
        // What the connection was never given: the `subscribe` starts and
        // resource waits on it, and its MCP Events bindings.
        self.resubscribe_on(&[server.to_string()], &[]);
        #[cfg(feature = "mcp-events")]
        self.events_sync("joined", &[server.to_string()]);
        self.log.info("mcp.joined", json!({"server": server}));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    /// A catalogue wait's start is forgotten when the wait ends — its step
    /// found its tool or finished — and when its run ends, so a later wait
    /// of the same step starts anew and no entry outlives its run; another
    /// run's, even one whose id this one's begins, stays.
    #[test]
    fn a_catalogue_wait_is_forgotten_when_it_ends() {
        let mut j = super::Joining::default();
        j.catalogue_waits.insert("r1/s".into(), 5);
        j.catalogue_waits.insert("r1/t".into(), 6);
        j.catalogue_waits.insert("r10/s".into(), 7);
        j.catalogue_wait_ended("r1", "s");
        assert!(!j.catalogue_waits.contains_key("r1/s"));
        j.run_ended("r1");
        assert_eq!(
            j.catalogue_waits.keys().collect::<Vec<_>>(),
            vec!["r10/s"],
            "another run's wait stays"
        );
    }

    #[test]
    fn the_dial_backs_off_from_a_second_to_a_minute() {
        assert_eq!(super::backoff(1), 1_000);
        assert_eq!(super::backoff(2), 2_000);
        assert_eq!(super::backoff(6), 32_000);
        assert_eq!(super::backoff(7), 60_000);
        assert_eq!(super::backoff(40), 60_000);
    }
}
