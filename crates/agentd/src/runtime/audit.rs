// SPDX-License-Identifier: AGPL-3.0-only
//! The **audit stream**: an append-only
//! record of *who did what* — every A2A call, every principal-driven tool/command,
//! config reloads, restores, store conflicts, and kills. Each event is
//! `{ts, principal, role, action, target, outcome, request_id, trace, instance}`
//! — plus `sid` when the caller signed in with a session,
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
    /// The session the caller signed in with ([`crate::a2a::Principal::session`]).
    /// Several sessions share one principal id by design — ownership is by
    /// name — so this is the only thing in the trail that says WHICH of them
    /// acted, and it is what a revocation names.
    pub sid: Option<&'a str>,
}

impl Runtime {
    /// Emit an audit event to the configured sinks. A no-op when no sink is
    /// configured (`observability.audit.sink`). Cheap on the common path.
    pub(crate) fn audit(&self, ev: AuditEvent<'_>) {
        self.audit_mirrored(ev, true)
    }

    /// [`Runtime::audit`], saying whether the event is also mirrored onto the
    /// observation feed.
    fn audit_mirrored(&self, ev: AuditEvent<'_>, mirror_to_feed: bool) {
        // Mirror onto the observation feed as operator-visible `audit` events
        // when introspection is on — independent of the sinks, which stay the
        // durable/system record and are written either way.
        #[cfg(feature = "a2a")]
        if mirror_to_feed
            && let Some(feed) = &self.a2a_feed
            && feed.introspection()
        {
            let mut data = json!({
                "ts": now_ms(),
                "principal": ev.principal,
                "role": ev.role,
                "action": ev.action,
                "target": ev.target,
                "outcome": ev.outcome,
            });
            if let Some(sid) = ev.sid {
                data["sid"] = json!(sid);
            }
            feed.push("audit", super::a2a_server::FeedVis::Operator, data);
        }
        #[cfg(not(feature = "a2a"))]
        let _ = mirror_to_feed;
        let Some(sinks) = &self.settings.observability.audit.sink else {
            return;
        };
        if sinks.is_empty() {
            return;
        }
        let mut record = json!({
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
        if let Some(sid) = ev.sid {
            record["sid"] = json!(sid);
        }
        if sinks.iter().any(|s| matches!(s, AuditSink::Log)) {
            // A single closed-vocabulary `audit` event (never content-suppressed —
            // an audit trail is metadata, not conversation content).
            self.log.info("audit", record.clone());
        }
        if sinks.iter().any(|s| matches!(s, AuditSink::Stream))
            && let Some(stream) = &self.settings.observability.audit.stream
        {
            // Queued, not appended: `audit` runs on `&self` from every
            // authorization path, and the append needs the state owner. The
            // tick drains it, which also puts these records behind the same
            // pressure gate as every other admission.
            crate::obs::log::tap_direct(stream, "audit", record.clone());
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
        self.audit_mirrored(
            AuditEvent {
                action: &action,
                target,
                outcome,
                principal: Some(&principal.id),
                role: Some(&role),
                request_id,
                sid: principal.session.as_deref(),
            },
            mirror_to_feed(method, op, outcome),
        );
    }
}

/// Whether an A2A call's audit event is mirrored onto the observation feed.
///
/// A successful READ is not: display clients poll reads (a log tail at about
/// once a second), and echoing each poll back onto the feed they are reading
/// would fill it with its own plumbing. Everything else is — a refusal of any
/// call, and every mutation. The op is classified by the op table, not by the
/// shape of the action string, so an op a workflow declares under a name that
/// happens to end like a read is still recorded. A call is classified by the
/// bridge verb the runtime answered, which is what `method` is.
#[cfg(feature = "a2a")]
fn mirror_to_feed(method: &str, op: Option<&str>, outcome: &str) -> bool {
    use crate::runtime::a2a_server::Verb;
    let read = match op {
        Some(op) => crate::runtime::surface::is_read_op(op),
        None => Verb::of(method).is_some_and(Verb::reads),
    };
    !(outcome == "ok" && read)
}

#[cfg(test)]
mod tests {
    // The emitter is exercised end-to-end by
    // `runtime_v2_a2a_e2e::a2a_calls_are_audited_when_the_audit_log_sink_is_on`
    // (a real daemon with `observability.audit.sink: [log]`); these pin the
    // one decision that is made here rather than there.
    #[cfg(feature = "a2a")]
    use super::*;

    #[cfg(feature = "a2a")]
    #[test]
    fn reads_are_not_mirrored_but_refusals_and_mutations_are() {
        // Successful reads: a read op, a read method.
        for op in [
            "status",
            "debug.events",
            "conversation.get",
            "workflow.status",
        ] {
            assert!(!mirror_to_feed("SendMessage", Some(op), "ok"), "{op}");
        }
        // Every verb the runtime answers, by the name it is audited under: a
        // read is not mirrored, anything else is. The push-config reads are
        // the ones a list of spec names missed.
        use crate::runtime::a2a_server::Verb;
        for v in Verb::ALL {
            let name = format!("{v:?}");
            assert_eq!(Verb::of(&name), Some(*v), "{name} is dispatched");
            assert_eq!(mirror_to_feed(&name, None, "ok"), !v.reads(), "{name}");
        }
        for m in ["PushConfigGet", "PushConfigList", "GetTask"] {
            assert!(!mirror_to_feed(m, None, "ok"), "{m}");
        }
        // The same reads, refused, are mirrored.
        assert!(mirror_to_feed("SendMessage", Some("debug.events"), "error"));
        assert!(mirror_to_feed("GetTask", None, "rate_limited"));
        // Mutations are mirrored.
        for op in ["workflow.run", "admin.set", "admin.drain", "subagent.kill"] {
            assert!(mirror_to_feed("SendMessage", Some(op), "ok"), "{op}");
        }
        assert!(mirror_to_feed("SendMessage", None, "ok"));
        assert!(mirror_to_feed("CancelTask", None, "ok"));
        // An op outside the table — a workflow's own command — is work, and
        // no suffix makes it a read.
        assert!(mirror_to_feed(
            "SendMessage",
            Some("audit.debug.events"),
            "ok"
        ));
        assert!(mirror_to_feed("SendMessage", Some("review.start"), "ok"));
    }
}
