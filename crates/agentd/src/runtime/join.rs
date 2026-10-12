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
}

#[derive(Default)]
struct Probe {
    failures: u32,
    due_at: u64,
    out: bool,
}

/// What a dial off the loop came back with: the initialized connection and
/// its tools, or nothing.
pub struct Reachable {
    server: String,
    found: Option<(crate::mcp::client::McpClient, Vec<::mcp::wire::Tool>)>,
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
            .filter(|n| !self.mcp.contains_key(n))
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
            let spawned = std::thread::Builder::new()
                .name("agentd-join".into())
                .spawn(move || {
                    // The dial boot makes, on its own thread: the loop is
                    // never held by a server that does not answer.
                    let found = crate::mcp::from_spec(&spec, timeout)
                        .map(super::supervisor_client)
                        .and_then(|mut c| c.initialize().map(|()| c))
                        .and_then(|c| c.list_tools().map(|tools| (c, tools)))
                        .ok();
                    let _ = tx.send(Event::McpReachable(Box::new(Reachable { server, found })));
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
        let Reachable { server, found } = r;
        let Some(probe) = self.joining.probes.get_mut(&server) else {
            return; // connected meanwhile (a reload dialed it), or gone
        };
        probe.out = false;
        if self.mcp.contains_key(&server) {
            return;
        }
        let Some((client, tools)) = found else {
            probe.failures += 1;
            probe.due_at = now + backoff(probe.failures);
            return;
        };
        if let Err(errors) = self.join_adopt(&server, client, tools) {
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

    /// Adopt `server`'s connection: its tools beside the others' as last
    /// built, the installed workflows' tools registered again over them, and
    /// what lives on the connection restored.
    fn join_adopt(
        &mut self,
        server: &str,
        mut client: crate::mcp::client::McpClient,
        tools: Vec<::mcp::wire::Tool>,
    ) -> Result<(), Vec<String>> {
        let Some(s) = self.settings.mcp.servers.iter().find(|s| s.name == server) else {
            return Ok(()); // no longer configured
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
        client.set_tool_meta(crate::mcp::tool_meta(&self.run_id, &self.instance));
        super::report_dropped_tools(&self.log, server, &client);
        let count = built.last().map_or(0, |t| t.tools.len());
        let protocol = client.protocol_version();
        for w in &registry.warnings {
            self.log.warn("registry.warning", json!({"warning": w}));
        }
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
