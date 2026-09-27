// SPDX-License-Identifier: AGPL-3.0-only
//! The `a2a` section of the `--capabilities` manifest.
//!
//! Beside the lists it reports rather than in `runtime::capabilities`, so a
//! change to the A2A surface and the change to how the manifest reports it
//! land in the same directory — and the rest of the manifest never has to be
//! touched for it.

use crate::config::v2::Settings;
use serde_json::{Value, json};

/// What `--capabilities` reports about the A2A listener, or `null` when no
/// `a2a.listen` is configured.
pub fn a2a_section(s: &Settings) -> Value {
    let Some(listen) = s.a2a.listen.as_ref() else {
        return Value::Null;
    };
    let principals: Vec<Value> = s
        .a2a
        .principals
        .iter()
        .map(|p| json!({"role": format!("{:?}", p.role).to_lowercase(), "match": principal_match_desc(&p.matcher), "grants": p.grants}))
        .collect();
    // Derived from the list the listener actually dispatches, plus the two
    // bootstrap calls it answers ahead of the dispatch table. This was a
    // fourth hand-maintained copy and it had drifted: it omitted
    // `GetExtendedAgentCard` and the four push-config methods, and listed
    // `GetAgentCard`, which `METHODS` does not carry.
    let mut methods: Vec<&str> = super::METHODS
        .iter()
        .copied()
        .filter(|m| *m != "SubscribeToEvents" || s.interface.enabled)
        .chain(super::LOCAL_METHODS.iter().copied())
        .collect();
    methods.sort_unstable();
    json!({
        "listen": listen,
        "tls": s.a2a.tls.cert.is_some(),
        "mtls": s.a2a.tls.client_ca.is_some(),
        "bearer": s.a2a.bearer.is_some(),
        "methods": methods,
        // The command ops come from the ONE list the agent card renders as
        // skills and the extension declares, so the manifest cannot
        // advertise a surface the card denies (they disagreed once: the
        // manifest listed ops the card never mentioned).
        "command_ops": super::command_ops_of(s),
        "extensions": super::extensions_of(s),
        "principals": principals,
        "loopback_operator": s.a2a.principals.is_empty(),
    })
}

/// A redacted description of a principal matcher (secrets never leak here).
fn principal_match_desc(m: &crate::config::v2::PrincipalMatch) -> Value {
    if m.any {
        json!({"any": true})
    } else if let Some(s) = &m.san {
        json!({"san": s})
    } else if let Some(s) = &m.sub {
        json!({"sub": s})
    } else if m.bearer_ref.is_some() {
        json!({"bearer_ref": "***"})
    } else if let Some(a) = &m.aauth_agent {
        json!({"aauth_agent": a})
    } else {
        json!({})
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The section moved out of `runtime::capabilities` without changing a
    /// byte. The expected strings are what the builder in `runtime/mod.rs`
    /// printed for these settings immediately before the move, so a later
    /// edit to the section has to change them on purpose rather than by
    /// accident — and the last assertion keeps the manifest reading its
    /// `a2a` section from here rather than from a copy.
    #[test]
    fn a2a_section_is_byte_identical() {
        let mut s = Settings {
            a2a: serde_json::from_value(json!({
                "listen": "https://0.0.0.0:8443",
                "bearer": "{{secret:A2A_BEARER}}",
                "tls": {"cert": "/tls/cert.pem", "key": "/tls/key.pem", "client_ca": "/tls/ca.pem"},
                "principals": [
                    {"match": {"san": "spiffe://corp/ops/*"}, "role": "operator"},
                    {"match": {"sub": "alice"}, "role": "user", "grants": ["knowledge.*"]},
                    {"match": {"bearer_ref": "{{secret:PEER}}"}, "role": "agent"},
                    {"match": {"aauth_agent": "agent://*"}, "role": "agent"},
                    {"match": {"any": true}, "role": "anonymous"}
                ]
            }))
            .unwrap(),
            ..Settings::default()
        };
        s.interface.enabled = true;
        s.interface.debug = true;
        s.interface.pairing.enabled = true;
        assert_eq!(
            a2a_section(&s).to_string(),
            r#"{"bearer":true,"command_ops":["status","config","workflow.run","workflow.status","workflow.cancel","workflow.signal","subagent.send","subagent.kill","subagent.status","plan.get","admin.drain","admin.lameduck","admin.pause","admin.resume","admin.cancel","interface.info","config.set","conversation.get","run.get","subagent.get","debug.events","pairing.code"],"extensions":["https://agentd.dev/a2a/ext/command/v1","https://agentd.dev/a2a/ext/interface/v1"],"listen":"https://0.0.0.0:8443","loopback_operator":false,"methods":["CancelTask","CreateTaskPushNotificationConfig","DeleteTaskPushNotificationConfig","GetAgentCard","GetExtendedAgentCard","GetTask","GetTaskPushNotificationConfig","ListTaskPushNotificationConfigs","ListTasks","Pair","SendMessage","SendStreamingMessage","SubscribeToEvents","SubscribeToTask"],"mtls":true,"principals":[{"grants":[],"match":{"san":"spiffe://corp/ops/*"},"role":"operator"},{"grants":["knowledge.*"],"match":{"sub":"alice"},"role":"user"},{"grants":[],"match":{"bearer_ref":"***"},"role":"agent"},{"grants":[],"match":{"aauth_agent":"agent://*"},"role":"agent"},{"grants":[],"match":{"any":true},"role":"anonymous"}],"tls":true}"#
        );

        let mut t = s.clone();
        t.interface = Default::default();
        t.a2a.principals.clear();
        t.a2a.bearer = None;
        t.a2a.tls = Default::default();
        t.a2a.listen = Some("unix:/run/agentd.sock".into());
        assert_eq!(
            a2a_section(&t).to_string(),
            r#"{"bearer":false,"command_ops":["status","config","workflow.run","workflow.status","workflow.cancel","workflow.signal","subagent.send","subagent.kill","subagent.status","plan.get","admin.drain","admin.lameduck","admin.pause","admin.resume","admin.cancel"],"extensions":["https://agentd.dev/a2a/ext/command/v1"],"listen":"unix:/run/agentd.sock","loopback_operator":true,"methods":["CancelTask","CreateTaskPushNotificationConfig","DeleteTaskPushNotificationConfig","GetAgentCard","GetExtendedAgentCard","GetTask","GetTaskPushNotificationConfig","ListTaskPushNotificationConfigs","ListTasks","Pair","SendMessage","SendStreamingMessage","SubscribeToTask"],"mtls":false,"principals":[],"tls":false}"#
        );

        t.a2a.listen = None;
        assert_eq!(a2a_section(&t), Value::Null, "no listener, no section");

        let loaded = crate::config::v2::Loaded {
            settings: s.clone(),
            doc: json!({}),
            file_doc: json!({}),
            files: Vec::new(),
            warnings: Vec::new(),
            trace: Default::default(),
        };
        assert_eq!(
            crate::runtime::capabilities(&loaded)["a2a"],
            a2a_section(&s),
            "the manifest's `a2a` is this section"
        );
    }
}
