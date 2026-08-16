// SPDX-License-Identifier: Apache-2.0
//! The **audit stream** (plan §3.11, RFC 0025 §3.3 `audit`): an append-only
//! record of *who did what* — every A2A call, every principal-driven tool/command,
//! config reloads, restores, store conflicts, and kills. Each event is
//! `{ts, principal, role, action, target, outcome, request_id, trace, instance}`,
//! emitted to the configured sinks: `log` (a closed-vocabulary `audit` log line)
//! and/or `store` (a durable, append-only `Kind::Audit` record, ULID-keyed — never
//! CAS'd, never listed, so it cannot be rewritten). Audit is security telemetry:
//! it answers "why did the agent do that, and on whose authority?".

use crate::config::v2::AuditSink;
use crate::runtime::reactor::Runtime;
use crate::state::{Kind, now_ms, ulid};
use serde_json::{Value, json};

/// One audit event to record.
pub(crate) struct AuditEvent<'a> {
    pub action: &'a str,
    pub target: Value,
    pub outcome: &'a str,
    pub principal: Option<&'a str>,
    pub role: Option<&'a str>,
    pub request_id: Option<&'a str>,
}

impl Runtime {
    /// Emit an audit event to the configured sinks. A no-op when no sink is
    /// configured (`observability.audit.sink`). Cheap on the common path.
    pub(crate) fn audit(&self, ev: AuditEvent<'_>) {
        // Mirror onto the interface feed when debug is on (RFC 0032 §4:
        // operator-visible `audit` events) — independent of the sinks, which
        // stay the durable/system record. The taskless interface READS are
        // excluded: a display client polls them (debug.events at ~1 Hz), and
        // mirroring their own audit back onto the feed would feed-loop the
        // debug pane with its own plumbing. The durable sinks still record
        // them.
        #[cfg(feature = "a2a")]
        if let Some(feed) = &self.a2a_feed
            && feed.debug()
            && !ev.action.ends_with(":interface.info")
            && !ev.action.ends_with(":conversation.get")
            && !ev.action.ends_with(":run.get")
            && !ev.action.ends_with(":subagent.get")
            && !ev.action.ends_with(":debug.events")
            && !ev.action.ends_with(":pairing.code")
        {
            feed.push(
                "audit",
                super::a2a_server::FeedVis::Operator,
                json!({
                    "ts": now_ms(),
                    "principal": ev.principal,
                    "role": ev.role,
                    "action": ev.action,
                    "target": ev.target,
                    "outcome": ev.outcome,
                }),
            );
        }
        let Some(sinks) = &self.settings.observability.audit.sink else {
            return;
        };
        if sinks.is_empty() {
            return;
        }
        let record = json!({
            "ts": now_ms(),
            "instance": self.instance,
            "principal": ev.principal,
            "role": ev.role,
            "action": ev.action,
            "target": ev.target,
            "outcome": ev.outcome,
            "request_id": ev.request_id,
            "trace": self.trace_id,
        });
        if sinks.iter().any(|s| matches!(s, AuditSink::Log)) {
            // A single closed-vocabulary `audit` event (never content-suppressed —
            // an audit trail is metadata, not conversation content).
            self.log.info("audit", record.clone());
        }
        if sinks.iter().any(|s| matches!(s, AuditSink::Store)) {
            // Append-only: a fresh ULID id per event (Kind::Audit is not indexed,
            // so this never conflicts and is never overwritten).
            let id = ulid::new();
            if let Err(e) = self.durable.put(Kind::Audit, &id, record, None) {
                // The store sink is best-effort telemetry — a failed audit write is
                // logged but never fails the audited action.
                self.log.warn(
                    "audit.store.fail",
                    json!({"action": ev.action, "err": e.to_string()}),
                );
            }
        }
    }

    /// Audit an A2A request (the principal, the method/op, the outcome).
    #[cfg(feature = "a2a")]
    pub(crate) fn audit_a2a(
        &self,
        method: &str,
        op: Option<&str>,
        principal: &crate::a2a::Principal,
        outcome: &str,
        target: Value,
        request_id: Option<&str>,
    ) {
        let action = match op {
            Some(o) => format!("a2a.{method}:{o}"),
            None => format!("a2a.{method}"),
        };
        let role = format!("{:?}", principal.role).to_lowercase();
        self.audit(AuditEvent {
            action: &action,
            target,
            outcome,
            principal: Some(&principal.id),
            role: Some(&role),
            request_id,
        });
    }
}

#[cfg(test)]
mod tests {
    // The emitter is exercised end-to-end by `runtime_v2_audit_e2e` (a real
    // daemon with `observability.audit.sink: [log]`); a pure-unit test would only
    // restate the JSON shape. The shape is asserted there against the log line.
}
