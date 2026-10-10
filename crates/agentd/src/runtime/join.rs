// SPDX-License-Identifier: AGPL-3.0-only
//! A configured MCP server that is down at boot joins when it comes up.
//!
//! Startup dials every server once and carries on without the ones that do
//! not answer; under compose or Kubernetes start ordering that is the normal
//! case, not the exception. Each such server is probed off the loop, with a
//! backoff (1 s, doubling, at most 60 s). When one answers, the loop dials it
//! through the reload's own staging and commit, with the running
//! configuration: what a reload does for a server that was down until it —
//! the registry rebuilt with its tools, the workflows that name it loaded,
//! its subscriptions, waits and MCP Events bindings restored — happens here,
//! and nothing else moves.

use super::events::Event;
use super::reactor::Runtime;
use crate::state::now_ms;
use serde_json::json;
use std::collections::BTreeMap;

const BACKOFF_MIN_MS: u64 = 1_000;
const BACKOFF_MAX_MS: u64 = 60_000;

/// The servers without a live connection, and where their probes are.
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

fn backoff(failures: u32) -> u64 {
    BACKOFF_MIN_MS
        .saturating_mul(1u64 << failures.min(16))
        .min(BACKOFF_MAX_MS)
}

impl Runtime {
    /// Every pass: probe, off the loop, each configured server that has no
    /// live connection and whose probe is due.
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
            let tx = self.events_tx.clone();
            let timeout = crate::obs::health::management_timeout();
            let server = name.clone();
            let spawned = std::thread::Builder::new()
                .name("agentd-join".into())
                .spawn(move || {
                    let ok = crate::mcp::from_spec(&spec, timeout)
                        .and_then(|mut c| c.initialize())
                        .is_ok();
                    let _ = tx.send(Event::McpReachable { server, ok });
                });
            if spawned.is_err()
                && let Some(p) = self.joining.probes.get_mut(&name)
            {
                p.out = false;
            }
        }
    }

    /// A probe answered. A server that answers is joined; one that does not
    /// is asked again later.
    pub(crate) fn on_mcp_reachable(&mut self, server: &str, ok: bool) {
        let now = now_ms();
        let Some(probe) = self.joining.probes.get_mut(server) else {
            return; // joined meanwhile, or no longer configured
        };
        probe.out = false;
        if !ok {
            probe.failures += 1;
            probe.due_at = now + backoff(probe.failures);
            return;
        }
        match self.join_servers() {
            Ok(changed) if self.mcp.contains_key(server) => {
                self.log
                    .info("mcp.joined", json!({"server": server, "changed": changed}));
            }
            // It answered the probe, and not the dial that followed.
            Ok(_) => {
                if let Some(p) = self.joining.probes.get_mut(server) {
                    p.failures += 1;
                    p.due_at = now + backoff(p.failures);
                }
            }
            Err(errors) => {
                self.log.warn(
                    "mcp.join.refused",
                    json!({"server": server, "errors": errors}),
                );
                if let Some(p) = self.joining.probes.get_mut(server) {
                    p.failures += 1;
                    p.due_at = now + backoff(p.failures);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_probe_backs_off_to_a_minute() {
        assert_eq!(super::backoff(0), 1_000);
        assert_eq!(super::backoff(1), 2_000);
        assert_eq!(super::backoff(5), 32_000);
        assert_eq!(super::backoff(6), 60_000);
        assert_eq!(super::backoff(40), 60_000);
    }
}
