// SPDX-License-Identifier: AGPL-3.0-only
//! The `a2a` section of the `--capabilities` manifest.
//!
//! Beside the lists it reports rather than in `runtime::capabilities`, so a
//! change to the A2A surface and the change to how the manifest reports it
//! land in the same directory — and the rest of the manifest never has to be
//! touched for it.

use crate::config::v2::Settings;
use serde_json::{Value, json};

use super::auth::{configured_url, listener_auth_of};
use super::{EXTENSION_METHODS, SpecMethod, extensions_of};

/// Every JSON-RPC method this instance answers: the specification's eleven,
/// then the methods of the extensions it declares.
///
/// Derived from the route table and the declaration list, which are the same
/// two lists the listener routes by and the card declares from. The manifest
/// used to keep its own copy, and it had drifted: it listed `GetAgentCard`,
/// which was never a method of the specification, and missed five that were.
pub fn methods_of(s: &Settings) -> Vec<&'static str> {
    let declared = extensions_of(s);
    SpecMethod::ALL
        .iter()
        .map(|m| m.name())
        .chain(
            EXTENSION_METHODS
                .iter()
                .filter(|(_, uri)| declared.contains(uri))
                .map(|(name, _)| *name),
        )
        .collect()
}

/// What `--capabilities` reports about the A2A listener, or `null` when no
/// `a2a.listen` is configured.
///
/// The auth fields are the listener's posture ([`listener_auth_of`]), the same
/// value the resolver admits callers by and the card publishes its schemes
/// from, so a controller reading this cannot be told a different story than a
/// peer reading the card. `url` is what the settings alone can say
/// ([`configured_url`]): `null` for a `:0` port or a unix socket, whose URL is
/// not known until the bind. `cors_origins` is a count, not the list — the
/// origins are configuration an operator already has, and a launched UI's
/// origin is not configuration at all.
pub fn a2a_section(s: &Settings) -> Value {
    if s.a2a.listen.is_none() {
        return Value::Null;
    }
    let posture = listener_auth_of(&s.a2a);
    json!({
        "methods": methods_of(s),
        // The command ops come from the ONE list the agent card renders as
        // skills and the extension declares, so the manifest cannot
        // advertise a surface the card denies (they disagreed once: the
        // manifest listed ops the card never mentioned).
        "command_ops": super::command_ops_of(s),
        "extensions": extensions_of(s),
        "url": configured_url(s),
        "auth": {
            "bearer": posture.bearer,
            "mtls": posture.mtls,
            "device": posture.device,
            "required": posture.required,
            "implicit_operator": posture.implicit_operator,
        },
        "events": s.a2a.events.enabled,
        "introspection": s.a2a.introspection.enabled,
        "cors_origins": s.a2a.cors.origins.len(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::surface::{Route, route_of};

    fn settings(a2a: Value) -> Settings {
        Settings {
            a2a: serde_json::from_value(a2a).unwrap(),
            ..Settings::default()
        }
    }

    /// The manifest's methods are the route table: every name routes, every
    /// core method is there, and an extension's method is there exactly when
    /// the instance declares the extension. Nothing the table refuses — the
    /// card read as a method, pairing — can be reported as served.
    #[test]
    fn manifest_methods_are_the_route_table() {
        let with_feed =
            settings(json!({"listen": "http://127.0.0.1:8080", "events": {"enabled": true}}));
        let without = settings(json!({"listen": "http://127.0.0.1:8080"}));
        for s in [&with_feed, &without] {
            let reported = a2a_section(s)["methods"].clone();
            let reported: Vec<&str> = reported
                .as_array()
                .unwrap()
                .iter()
                .map(|m| m.as_str().unwrap())
                .collect();
            assert_eq!(reported, methods_of(s), "the section reports methods_of");
            for name in &reported {
                assert!(route_of(name).is_some(), "{name} does not route");
            }
            for m in SpecMethod::ALL {
                assert!(reported.contains(&m.name()), "{m:?} is not reported");
            }
            for gone in ["GetAgentCard", "Pair", "interface.pair"] {
                assert!(!reported.contains(&gone), "{gone} is reported");
            }
            let declared = extensions_of(s);
            for (name, uri) in EXTENSION_METHODS {
                assert_eq!(
                    reported.contains(name),
                    declared.contains(uri),
                    "{name} is reported iff {uri} is declared"
                );
                assert!(matches!(route_of(name), Some(Route::Extension { .. })));
            }
        }
        assert!(
            methods_of(&with_feed).len() > methods_of(&without).len(),
            "declaring the feed's extension adds its method"
        );
    }

    /// The section's exact shape for two instances, so a later edit changes
    /// it on purpose rather than by accident — and the last assertion keeps
    /// the manifest reading its `a2a` section from here rather than from a
    /// copy.
    #[test]
    fn a2a_section_shape() {
        let s = settings(json!({
            "listen": "https://0.0.0.0:8443",
            "url": "https://agent.example.com",
            "bearer": "{{secret:A2A_BEARER}}",
            "tls": {"cert": "/tls/cert.pem", "key": "/tls/key.pem", "client_ca": "/tls/ca.pem"},
            "principals": [
                {"match": {"san": "spiffe://corp/ops/*"}, "role": "operator"},
                {"id": "peer", "match": {"bearer_ref": "{{secret:PEER}}"}, "role": "agent"}
            ],
            "cors": {"origins": ["https://ui.example.com", "https://ops.example.com"]},
            "events": {"enabled": true},
            "introspection": {"enabled": true}
        }));
        assert_eq!(
            a2a_section(&s).to_string(),
            r#"{"auth":{"bearer":true,"device":false,"implicit_operator":false,"mtls":true,"required":true},"command_ops":["status","config","workflow.run","workflow.status","workflow.cancel","workflow.signal","subagent.send","subagent.kill","subagent.status","plan.get","conversation.get","run.get","subagent.get","debug.events","admin.drain","admin.pause","admin.resume","admin.cancel","admin.set"],"cors_origins":2,"events":true,"extensions":["https://agentd.dev/a2a/ext/command/v1","https://agentd.dev/a2a/ext/interface/v1","https://agentd.dev/a2a/ext/task-annotations/v1"],"introspection":true,"methods":["SendMessage","SendStreamingMessage","GetTask","ListTasks","CancelTask","SubscribeToTask","CreateTaskPushNotificationConfig","GetTaskPushNotificationConfig","ListTaskPushNotificationConfigs","DeleteTaskPushNotificationConfig","GetExtendedAgentCard","SubscribeToEvents"],"url":"https://agent.example.com"}"#
        );

        // A unix socket: the kernel authenticates, the caller is the operator,
        // and there is no URL a remote caller could use.
        let t = settings(json!({"listen": "unix:/run/agentd.sock"}));
        assert_eq!(
            a2a_section(&t).to_string(),
            r#"{"auth":{"bearer":false,"device":false,"implicit_operator":true,"mtls":false,"required":false},"command_ops":["status","config","workflow.run","workflow.status","workflow.cancel","workflow.signal","subagent.send","subagent.kill","subagent.status","plan.get","admin.drain","admin.pause","admin.resume","admin.cancel","admin.set"],"cors_origins":0,"events":false,"extensions":["https://agentd.dev/a2a/ext/command/v1","https://agentd.dev/a2a/ext/task-annotations/v1"],"introspection":false,"methods":["SendMessage","SendStreamingMessage","GetTask","ListTasks","CancelTask","SubscribeToTask","CreateTaskPushNotificationConfig","GetTaskPushNotificationConfig","ListTaskPushNotificationConfigs","DeleteTaskPushNotificationConfig","GetExtendedAgentCard"],"url":null}"#
        );

        assert_eq!(
            a2a_section(&Settings::default()),
            Value::Null,
            "no listener, no section"
        );

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
