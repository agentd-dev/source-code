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
}

impl Joining {
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
            #[cfg(feature = "companies")]
            let companies = self.companies.clone();
            #[cfg(feature = "companies")]
            let api = crate::config::settings::companies::is_api_server(&self.settings, &name);
            let spawned = std::thread::Builder::new()
                .name("agentd-join".into())
                .spawn(move || {
                    // The dial boot makes, on its own thread: the loop is
                    // never held by a server that does not answer.
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
                        None => (
                            crate::mcp::from_spec(&spec, timeout)
                                .map(super::supervisor_client)
                                .and_then(|mut c| c.initialize().map(|()| c))
                                .and_then(|c| {
                                    c.list_tools().map(|tools| Found::Mcp(Box::new(c), tools))
                                })
                                .ok(),
                            None,
                        ),
                    };
                    let _ = tx.send(Event::McpReachable(Box::new(Reachable {
                        server,
                        found,
                        refused,
                    })));
                });
            if spawned.is_err()
                && let Some(p) = self.joining.probes.get_mut(&name)
            {
                p.out = false;
            }
        }
    }

    /// A dial came back. A connection is adopted; nothing is dialed again
    /// after a backoff.
    pub(crate) fn on_mcp_reachable(&mut self, r: Reachable) {
        let now = now_ms();
        let Reachable {
            server,
            found,
            refused,
        } = r;
        let Some(probe) = self.joining.probes.get_mut(&server) else {
            return; // connected meanwhile (a reload dialed it), or gone
        };
        probe.out = false;
        if self.mcp.contains_key(&server) {
            return;
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
            Some(Found::Mcp(client, tools)) => self.join_adopt(&server, *client, tools),
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
    fn registry_with(
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
    #[test]
    fn the_dial_backs_off_from_a_second_to_a_minute() {
        assert_eq!(super::backoff(1), 1_000);
        assert_eq!(super::backoff(2), 2_000);
        assert_eq!(super::backoff(6), 32_000);
        assert_eq!(super::backoff(7), 60_000);
        assert_eq!(super::backoff(40), 60_000);
    }
}
