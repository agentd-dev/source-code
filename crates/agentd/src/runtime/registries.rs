// SPDX-License-Identifier: AGPL-3.0-only
//! Registry servers at run time (RFC 0055 §4).
//!
//! A registry server is an `mcp.servers` entry with `registry` + `server`:
//! the operator's, one a document's `:::mcp{registry server}` folded into, or
//! one made here when a workflow step names it (`{kind: mcp.tool, registry,
//! server, tool}`) or the model connects one (`mcp.registry.connect`). It is
//! dialed by the join (`runtime::join`) like any on-use server; its dial
//! reads its record first ([`resolve`], off the loop): the registry's answer,
//! reused while it is fresh (`refresh`, kept in the store), held to what
//! agentd dials (`mcp::registry::validate_record`), then the endpoint it
//! names.
//!
//! A run pins the version it first used: a step of a run that used version
//! `v` of a server that has since been resolved to another one calls a
//! connection to `v` itself (`<registry>/<server>@<v>`), made and dialed as
//! any registry server, so a publish under a running run does not change the
//! server it talks to.

use super::reactor::Runtime;
use crate::config::settings::registries::{self, StepServer};
use crate::config::{AuthSpec, McpServerSpec, RegistryDial};
use crate::mcp::registry::{Api, Resolution};
use crate::state::{Kind, now_ms};
use serde_json::{Map, Value, json};
use std::collections::BTreeMap;

/// Registry servers' run-time state.
#[derive(Default)]
pub(crate) struct State {
    /// Each connected registry server: what it was resolved from and to.
    pub live: BTreeMap<String, Live>,
    /// The servers made at run time, kept across a reload.
    pub made: BTreeMap<String, crate::config::settings::McpServer>,
    /// The servers whose record was refused, and why: a step naming one
    /// fails with the reason instead of waiting for it.
    pub refused: BTreeMap<String, String>,
    /// The operator's servers whose record disagrees with the `src` an
    /// instruction's block named beside it, and why: the document's steps
    /// naming one fail with the reason; everything else calls it.
    pub document_refused: BTreeMap<String, String>,
}

/// What [`Runtime::keep_made_registry_servers`] carried into a reload's
/// settings, to apply when it commits.
#[derive(Default)]
pub(crate) struct KeptMade {
    pub made: BTreeMap<String, crate::config::settings::McpServer>,
    /// Each made server the new settings' registries no longer approve.
    pub dropped: Vec<Value>,
}

/// A connected registry server.
#[derive(Debug, Clone)]
pub(crate) struct Live {
    pub unresolved: McpServerSpec,
    pub resolved: McpServerSpec,
    pub version: String,
}

/// What a registry server's dial found of its record.
#[derive(Debug)]
pub(crate) struct Resolved {
    pub registry: String,
    pub server: String,
    /// The version asked for (`latest` or exact).
    pub asked: String,
    /// The record as read, with when it was fetched, and whether this dial
    /// fetched it (so the loop keeps it).
    pub record: Option<(Value, u64)>,
    pub fetched: bool,
    pub resolution: Result<Resolution, String>,
    /// The spec to dial: the record's endpoint and headers.
    pub spec: Option<McpServerSpec>,
    pub refresh_ms: u64,
}

impl Resolved {
    /// The spec a connection made on this resolution runs on, and the
    /// version it is: the record's endpoint and headers, and — where the
    /// entry has no `auth` and discovery found one (`found`) — that
    /// credential, which the resolution was made without.
    pub(crate) fn adopted(self, found: Option<AuthSpec>) -> Option<(McpServerSpec, String)> {
        let (mut spec, res) = (self.spec?, self.resolution.ok()?);
        if spec.auth.is_none() {
            spec.auth = found;
        }
        Some((spec, res.version))
    }
}

/// The store key a registry's record of `server` at `version` is kept
/// under.
fn cache_key(d: &RegistryDial) -> String {
    format!(
        "_mcpregistry/{}",
        crate::sha::sha256_hex(format!("{}\n{}\n{}", d.base, d.server, d.version).as_bytes())
    )
}

/// Resolve the registry server `spec` (off the loop): its record from
/// `cached` while fresh, else from the registry; held to what agentd dials;
/// the remote's address through the SSRF guard.
pub(crate) fn resolve(spec: &McpServerSpec, cached: Option<(Value, u64)>) -> Resolved {
    let d = spec.registry.clone().unwrap_or_default();
    let refresh_ms = d.refresh.as_millis() as u64;
    let mut out = Resolved {
        registry: d.registry.clone(),
        server: d.server.clone(),
        asked: d.version.clone(),
        record: None,
        fetched: false,
        resolution: Err(String::new()),
        spec: None,
        refresh_ms,
    };
    let fresh = cached.filter(|(_, at)| now_ms().saturating_sub(*at) < refresh_ms);
    let record = match fresh {
        Some(c) => c,
        None => match Api::of(&d).version(&d.server, &d.version) {
            Ok(v) => {
                out.fetched = true;
                (v, now_ms())
            }
            Err(e) => {
                out.resolution = Err(e);
                return out;
            }
        },
    };
    out.resolution = crate::mcp::registry::validate_record(&d.server, &record.0, &d, &spec.headers)
        .and_then(|r| crate::mcp::registry::guard_url(&r.url, d.allow_private).map(|()| r));
    if let Ok(r) = &out.resolution {
        out.spec = Some(spec.resolved(&r.url, r.headers.clone()));
    }
    out.record = Some(record);
    out
}

/// What `mcp.registry.search` answers of `records`: each one whose name
/// `reg` approves, once — its name, title, description, version, status and
/// the kinds of its remotes.
pub(crate) fn approved_summaries(
    reg: &crate::config::settings::registries::McpRegistry,
    records: &[Value],
) -> Vec<Value> {
    let mut seen = std::collections::BTreeSet::new();
    records
        .iter()
        .map(crate::mcp::registry::summary)
        .filter(|s| {
            s["name"].as_str().is_some_and(|n| reg.approves(n))
                && seen.insert(s["name"].to_string())
        })
        .map(|mut s| {
            if let Some(o) = s.as_object_mut() {
                o.remove("packages");
                let kinds: Vec<Value> = o
                    .get("remotes")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(|r| r.get("type").cloned())
                    .collect();
                o.insert("remotes".into(), Value::Array(kinds));
            }
            s
        })
        .collect()
}

impl Runtime {
    /// The record of `d` the store keeps, with when it was fetched.
    pub(crate) fn registry_cached(&self, d: &RegistryDial) -> Option<(Value, u64)> {
        let env = self.durable.get(Kind::Memory, &cache_key(d)).ok()??;
        let at = env.state.get("fetched_at")?.as_u64()?;
        Some((env.state.get("record")?.clone(), at))
    }

    /// A registry server's resolution came back: kept, said, and — refused —
    /// the wait before its record is read again (`Some`).
    pub(crate) fn on_registry_resolved(&mut self, server: &str, r: &Resolved) -> Option<u64> {
        if r.fetched
            && let Some((record, at)) = &r.record
            && let Some(spec) = self.mcp_specs.get(server)
            && let Some(d) = &spec.registry
        {
            let _ = self.durable.put(
                Kind::Memory,
                &cache_key(d),
                json!({"base": d.base, "server": d.server, "version": d.version,
                       "record": record, "fetched_at": at}),
                None,
            );
        }
        match &r.resolution {
            Ok(res) => {
                self.registries.refused.remove(server);
                match &res.document_refused {
                    Some(e) => {
                        self.log.warn(
                            "mcp.registry.refused",
                            json!({"registry": r.registry, "server": r.server, "name": server,
                                   "err": e, "document": true}),
                        );
                        self.registries
                            .document_refused
                            .insert(server.to_string(), e.clone());
                    }
                    None => {
                        self.registries.document_refused.remove(server);
                    }
                }
                self.log.info(
                    "mcp.registry.resolved",
                    json!({"registry": r.registry, "server": r.server, "name": server,
                           "version": res.version, "url": res.url, "asked": r.asked,
                           "cached": !r.fetched, "skipped": res.skipped}),
                );
                if res.deprecated() {
                    self.log.warn(
                        "mcp.registry.deprecated",
                        json!({"registry": r.registry, "server": r.server, "name": server,
                               "version": res.version}),
                    );
                }
                None
            }
            // A registry that did not answer is asked again on the join's
            // backoff; a record agentd will not dial, once it is stale.
            Err(e) if r.record.is_none() => {
                self.log.warn(
                    "mcp.registry.unreachable",
                    json!({"registry": r.registry, "server": r.server, "name": server, "err": e}),
                );
                None
            }
            Err(e) => {
                self.log.warn(
                    "mcp.registry.refused",
                    json!({"registry": r.registry, "server": r.server, "name": server, "err": e}),
                );
                self.registries
                    .refused
                    .insert(server.to_string(), e.clone());
                Some(r.refresh_ms.max(1_000))
            }
        }
    }

    /// A registry server's connection was made with `spec`: it runs on it.
    pub(crate) fn registry_adopted(&mut self, server: &str, spec: McpServerSpec, version: String) {
        let Some(unresolved) = self.mcp_specs.get(server).cloned() else {
            return;
        };
        self.mcp_specs.insert(server.to_string(), spec.clone());
        self.registries.live.insert(
            server.to_string(),
            Live {
                unresolved,
                resolved: spec,
                version,
            },
        );
    }

    /// Whether the connected registry server `server`'s record is stale —
    /// older than its registry's `refresh`, or no longer kept: a re-dial of
    /// it reads the record again rather than dial the endpoint it named.
    pub(crate) fn registry_record_stale(&self, server: &str) -> bool {
        let Some(d) = self
            .registries
            .live
            .get(server)
            .and_then(|l| l.unresolved.registry.as_ref())
        else {
            return false;
        };
        match self.registry_cached(d) {
            Some((_, at)) => now_ms().saturating_sub(at) >= d.refresh.as_millis() as u64,
            None => true,
        }
    }

    /// The connection of the registry server `server`, whose record is
    /// stale, is to be dialed again: it is let go, and the join dials the
    /// server through its registry (RFC 0055 §4.1) — a `deleted` record is
    /// refused there and the server stays down, a moved remote is followed.
    /// It is dialed on the spec it was resolved from, without a credential
    /// found for the old endpoint where its entry names none: the new one's
    /// is found for it.
    pub(crate) fn registry_redial(&mut self, server: &str, reason: &str) {
        let Some(live) = self.registries.live.remove(server) else {
            return;
        };
        let mut spec = live.unresolved;
        let own_auth = self
            .settings
            .mcp
            .servers
            .iter()
            .find(|s| s.name == server)
            .is_some_and(|s| s.auth.is_some());
        if !own_auth {
            spec.auth = None;
        }
        self.mcp.remove(server);
        self.mcp_specs.insert(server.to_string(), spec);
        self.joining.want(server);
        self.log.info(
            "mcp.disconnect",
            json!({"server": server, "reason": reason, "registry": "stale"}),
        );
    }

    /// Make the registry server a step or the model names, when it is not
    /// configured yet: approved by its registry, as the operator's own
    /// entry for it when there is one.
    pub(crate) fn ensure_registry_server(&mut self, named: &StepServer) -> Result<(), String> {
        if self
            .settings
            .mcp
            .servers
            .iter()
            .any(|s| s.name == named.name)
        {
            return Ok(());
        }
        let srv = registries::implicit_server(
            &self.settings,
            &named.name,
            &named.registry,
            &named.server,
            named.version.as_deref(),
        )?;
        let spec = super::reload::dial_spec(&self.settings, &srv)?;
        self.log.info(
            "mcp.registry.server",
            json!({"name": srv.name, "registry": named.registry, "server": named.server,
                   "version": named.version.as_deref().unwrap_or(registries::LATEST)}),
        );
        self.mcp_specs.insert(srv.name.clone(), spec);
        self.registries.made.insert(srv.name.clone(), srv.clone());
        self.settings.mcp.servers.push(srv);
        Ok(())
    }

    /// The server a step's `spec` calls: the one it names, or — for a
    /// registry server — the connection to the version its run pinned. The
    /// server is made when it is not yet, and the version pinned when the
    /// run first finds it connected. `Err` refuses the step.
    pub(crate) fn registry_route(
        &mut self,
        run_id: &str,
        spec: &Map<String, Value>,
    ) -> Result<Option<String>, String> {
        // A step naming a registry server by registry and name, or naming a
        // configured registry server (the operator's, a document's) by its
        // name.
        let named = match StepServer::of(spec) {
            Some(n) => n,
            None => {
                let Some(srv) = spec
                    .get("server")
                    .and_then(Value::as_str)
                    .and_then(|n| self.settings.mcp.servers.iter().find(|s| s.name == n))
                else {
                    return Ok(None);
                };
                let (Some(registry), Some(server)) = (&srv.registry, &srv.server) else {
                    return Ok(None);
                };
                StepServer {
                    name: srv.name.clone(),
                    registry: registry.clone(),
                    server: server.clone(),
                    version: srv.version.clone(),
                }
            }
        };
        self.ensure_registry_server(&named)?;
        // The document's block for the operator's server named a `src` its
        // record does not hold: the document's steps are refused, not the
        // operator's server (RFC 0055 §4.2).
        if let Some(why) = self.registries.document_refused.get(&named.name)
            && self
                .runs
                .get(run_id)
                .and_then(|r| self.workflows.get(&r.workflow))
                .is_some_and(|w| w.document)
        {
            return Err(why.clone());
        }
        // An exact version is its own pin.
        let exact = named
            .version
            .as_deref()
            .is_some_and(|v| v != registries::LATEST);
        if exact {
            return Ok(Some(named.name));
        }
        let live = self
            .registries
            .live
            .get(&named.name)
            .map(|l| l.version.clone())
            .filter(|v| !v.is_empty());
        let Some(run) = self.runs.get_mut(run_id) else {
            return Ok(Some(named.name));
        };
        match (run.registry_pins.get(&named.name).cloned(), live) {
            (Some(pin), Some(now)) if pin != now => {
                let pinned = StepServer {
                    name: registries::implicit_name(&named.registry, &named.server, Some(&pin)),
                    version: Some(pin.clone()),
                    ..named
                };
                self.ensure_registry_server(&pinned)?;
                self.run_log(run_id).info(
                    "mcp.registry.pinned",
                    json!({"run": run_id, "server": pinned.server, "version": pin,
                           "resolved": now, "name": pinned.name}),
                );
                Ok(Some(pinned.name))
            }
            (None, Some(now)) => {
                run.registry_pins.insert(named.name.clone(), now);
                run.dirty = true;
                Ok(Some(named.name))
            }
            _ => Ok(Some(named.name)),
        }
    }

    /// Carry the registry servers made at run time into `new` settings a
    /// reload loads, each one its registry there still approves. Nothing of
    /// the running state moves: what comes back is applied by
    /// [`Self::commit_made_registry_servers`] once the reload commits, so a
    /// refused reload keeps every server it would have dropped.
    pub(crate) fn keep_made_registry_servers(
        &self,
        new: &mut crate::config::settings::Settings,
    ) -> KeptMade {
        let mut kept = KeptMade::default();
        for (name, srv) in &self.registries.made {
            if new.mcp.servers.iter().any(|s| &s.name == name) {
                continue;
            }
            let (Some(r), Some(server)) = (srv.registry.clone(), srv.server.clone()) else {
                continue;
            };
            match registries::implicit_server(new, name, &r, &server, srv.version.as_deref()) {
                Ok(s) => {
                    new.mcp.servers.push(s.clone());
                    kept.made.insert(name.clone(), s);
                }
                Err(e) => kept
                    .dropped
                    .push(json!({"name": name, "registry": r, "server": server, "reason": e})),
            }
        }
        kept
    }

    /// The reload `kept` was made for committed: the made servers are those
    /// it kept, and each one it dropped is said.
    pub(crate) fn commit_made_registry_servers(&mut self, kept: KeptMade) {
        self.registries.made = kept.made;
        for d in kept.dropped {
            self.log.info("mcp.registry.server.dropped", d);
        }
    }

    // ---- discover (RFC 0055 §8.2) ----------------------------------------

    /// `mcp.registry.search {registry, query}` and `mcp.registry.connect
    /// {registry, server}`: only on a registry that sets `discover`, and only
    /// names its `servers` approve.
    pub(crate) fn registry_tool(
        &mut self,
        caller: &super::tools::ToolCaller,
        name: &str,
        args: Value,
    ) -> super::tools::ToolOutcome {
        use super::tools::ToolOutcome;
        let err = |e: String| ToolOutcome::Ready(Value::String(e), true);
        let reg_name = args
            .get("registry")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let Some(reg) = registries::registry(&self.settings, &reg_name)
            .filter(|r| r.discover)
            .cloned()
        else {
            let offered: Vec<&str> = self
                .settings
                .mcp
                .registries
                .iter()
                .filter(|r| r.discover)
                .map(|r| r.name.as_str())
                .collect();
            return err(format!(
                "{name}: registry {reg_name:?} is not one the operator lets you search; these are: {offered:?}"
            ));
        };
        match name {
            "mcp.registry.search" => {
                let query = args
                    .get("query")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                let api = Api {
                    base: reg.base().to_string(),
                    headers: reg
                        .headers
                        .iter()
                        .map(|(k, v)| (k.clone(), v.clone()))
                        .collect(),
                    auth: reg
                        .auth
                        .as_ref()
                        .map(|a| self.settings.auth_spec_at(a, &reg.url, false)),
                    target: format!("registry:{}", reg.name),
                    timeout: reg.timeout(),
                };
                let tx = self.events_tx.clone();
                let target = super::tools::ExecTarget::from(caller);
                self.log.info(
                    "mcp.registry.search",
                    json!({"registry": reg.name, "query": query}),
                );
                std::thread::Builder::new()
                    .name("tool:registry".into())
                    .spawn(move || {
                        let (result, is_error) = match api.search(&query, 50) {
                            Ok(records) => (
                                json!({"registry": reg.name, "servers": approved_summaries(&reg, &records)}),
                                false,
                            ),
                            Err(e) => (Value::String(format!("mcp.registry.search: {e}")), true),
                        };
                        target.send(&tx, result, is_error);
                    })
                    .ok();
                ToolOutcome::Executing
            }
            "mcp.registry.connect" => {
                let server = args
                    .get("server")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                if !reg.approves(&server) {
                    return err(format!(
                        "mcp.registry.connect: {}",
                        registries::not_approved(&reg, &server)
                    ));
                }
                let named = StepServer {
                    name: registries::implicit_name(&reg.name, &server, None),
                    registry: reg.name.clone(),
                    server: server.clone(),
                    version: None,
                };
                if let Err(e) = self.ensure_registry_server(&named) {
                    return err(format!("mcp.registry.connect: {e}"));
                }
                if let Some(why) = self.registries.refused.get(&named.name) {
                    return err(format!("mcp.registry.connect: {why}"));
                }
                let connected = self.mcp.contains_key(&named.name);
                if !connected {
                    self.want_server(&named.name);
                }
                ToolOutcome::Ready(
                    json!({"server": named.name, "registry": reg.name, "name": server,
                           "connected": connected,
                           "note": if connected { "connected: its tools are offered" } else { "connecting: its tools are offered once it has connected, from your next turn" }}),
                    false,
                )
            }
            other => err(format!(
                "internal tool {other:?} has no built-in implementation"
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings(discover: bool) -> crate::config::settings::Settings {
        crate::config::settings::Settings::from_document(
            json!({"store": {"kind": "memory"}, "mcp": {"registries": [
                {"name": "official", "url": "https://registry.example", "servers": ["io.acme/*"],
                 "discover": discover, "tags": {"*": ["untrusted_input"]}}]}}),
            "t",
        )
        .unwrap()
    }

    /// The model's discovery tools exist only where a registry entry sets
    /// `discover: true`, and carry the registry's tags.
    #[test]
    fn discover_offers_the_tools_only_where_the_operator_opts_in() {
        use crate::registry::{Caller, Registry};
        for discover in [false, true] {
            let s = settings(discover);
            let reg = Registry::build(&s, &[]).unwrap();
            for tool in ["mcp.registry.search", "mcp.registry.connect"] {
                assert_eq!(
                    reg.allowed(&Caller::Root, tool),
                    discover,
                    "{tool} with discover: {discover}"
                );
            }
            if discover {
                assert_eq!(
                    reg.get("mcp.registry.connect").unwrap().tags,
                    [crate::sec::scope::TrifectaTag::UntrustedInput]
                );
            }
        }
    }

    /// A connection made on a resolution runs on the record's endpoint with
    /// the credential discovery found for it, where the entry has none of
    /// its own — so a child handed the spec, and a reload comparing it, see
    /// the credential the connection was made with.
    #[test]
    fn a_registry_server_runs_on_the_credential_discovery_found() {
        let dial = RegistryDial {
            registry: "official".into(),
            server: "io.acme/kb".into(),
            ..Default::default()
        };
        let unresolved = McpServerSpec {
            name: "kb".into(),
            registry: Some(dial),
            ..Default::default()
        };
        let resolved = |own: Option<AuthSpec>| Resolved {
            registry: "official".into(),
            server: "io.acme/kb".into(),
            asked: "latest".into(),
            record: None,
            fetched: false,
            resolution: Ok(Resolution {
                name: "io.acme/kb".into(),
                version: "1.0.0".into(),
                url: "https://kb.example/mcp".into(),
                headers: Vec::new(),
                status: "active".into(),
                skipped: Vec::new(),
                document_refused: None,
            }),
            spec: Some(
                McpServerSpec {
                    auth: own,
                    ..unresolved.clone()
                }
                .resolved("https://kb.example/mcp", Vec::new()),
            ),
            refresh_ms: 1000,
        };
        let auth = |grant: &str| AuthSpec {
            kind: "oauth2".into(),
            grant: Some(grant.into()),
            ..Default::default()
        };
        let (spec, version) = resolved(None).adopted(Some(auth("device"))).unwrap();
        assert_eq!(
            (spec.endpoint.as_str(), version.as_str()),
            ("https://kb.example/mcp", "1.0.0")
        );
        assert_eq!(spec.auth, Some(auth("device")));
        let (spec, _) = resolved(Some(auth("client_credentials")))
            .adopted(Some(auth("device")))
            .unwrap();
        assert_eq!(spec.auth, Some(auth("client_credentials")));
        let refused = Resolved {
            resolution: Err("refused".into()),
            spec: None,
            ..resolved(None)
        };
        assert!(refused.adopted(None).is_none());
    }

    /// A search lists only the names the registry's globs approve, once.
    #[test]
    fn a_search_lists_only_approved_names() {
        let s = settings(true);
        let reg = &s.mcp.registries[0];
        let rec = |name: &str| {
            json!({"server": {"name": name, "version": "1", "title": name,
                              "remotes": [{"type": "streamable-http", "url": "https://x.example/mcp"}],
                              "packages": [{"registryType": "npm"}]}})
        };
        let out = approved_summaries(
            reg,
            &[rec("io.acme/kb"), rec("io.other/kb"), rec("io.acme/kb")],
        );
        assert_eq!(out.len(), 1, "{out:?}");
        assert_eq!(out[0]["name"], "io.acme/kb");
        assert_eq!(out[0]["remotes"], json!(["streamable-http"]));
        assert!(out[0].get("packages").is_none());
    }
}
