// SPDX-License-Identifier: AGPL-3.0-only
//! The JSON-RPC methods the listener answers: the specification's eleven, and
//! the extension methods [`super::EXTENSION_METHODS`] declares.
//!
//! Lives here rather than beside the dispatch because the `--capabilities`
//! manifest is always compiled and the listener is `a2a`-gated: a manifest that
//! could not read the table grew a hand-typed copy of it, and that copy drifted
//! by five methods.
//!
//! [`route_of`] is the whole vocabulary. It matches names exactly — no `a2a.`
//! prefix strip, no 0.3 alias, no convenience method — so a name that is not in
//! the table is `-32601` for every caller, before any authorization runs and
//! before anything reaches the runtime. Each of the old spellings once worked
//! for some methods and not others, and none of them went through the version
//! gate a spec client would have been held to.

use super::EXTENSION_METHODS;

/// The A2A protocol version, as the `A2A-Version` header and a card
/// interface's `protocolVersion` spell it.
pub const A2A_PROTOCOL_VERSION: &str = "1.0";

/// The core A2A 1.0 JSON-RPC methods, and nothing else.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpecMethod {
    SendMessage,
    SendStreamingMessage,
    GetTask,
    ListTasks,
    CancelTask,
    SubscribeToTask,
    CreateTaskPushNotificationConfig,
    GetTaskPushNotificationConfig,
    ListTaskPushNotificationConfigs,
    DeleteTaskPushNotificationConfig,
    GetExtendedAgentCard,
}

impl SpecMethod {
    /// Every core method, in the order the specification lists them.
    pub const ALL: &[SpecMethod] = &[
        SpecMethod::SendMessage,
        SpecMethod::SendStreamingMessage,
        SpecMethod::GetTask,
        SpecMethod::ListTasks,
        SpecMethod::CancelTask,
        SpecMethod::SubscribeToTask,
        SpecMethod::CreateTaskPushNotificationConfig,
        SpecMethod::GetTaskPushNotificationConfig,
        SpecMethod::ListTaskPushNotificationConfigs,
        SpecMethod::DeleteTaskPushNotificationConfig,
        SpecMethod::GetExtendedAgentCard,
    ];

    /// The method's wire name. One exhaustive match, so a variant cannot be
    /// added without a spelling.
    pub fn name(self) -> &'static str {
        match self {
            SpecMethod::SendMessage => "SendMessage",
            SpecMethod::SendStreamingMessage => "SendStreamingMessage",
            SpecMethod::GetTask => "GetTask",
            SpecMethod::ListTasks => "ListTasks",
            SpecMethod::CancelTask => "CancelTask",
            SpecMethod::SubscribeToTask => "SubscribeToTask",
            SpecMethod::CreateTaskPushNotificationConfig => "CreateTaskPushNotificationConfig",
            SpecMethod::GetTaskPushNotificationConfig => "GetTaskPushNotificationConfig",
            SpecMethod::ListTaskPushNotificationConfigs => "ListTaskPushNotificationConfigs",
            SpecMethod::DeleteTaskPushNotificationConfig => "DeleteTaskPushNotificationConfig",
            SpecMethod::GetExtendedAgentCard => "GetExtendedAgentCard",
        }
    }
}

/// Where a method name leads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Route {
    /// One of the specification's methods.
    Spec(SpecMethod),
    /// A method an extension declares. Whether this instance declares — and
    /// the request activates — that extension is decided later in the
    /// pipeline, never here: the table does not depend on the settings.
    Extension { ext_method: &'static str },
}

impl Route {
    /// The wire name the route answers to.
    pub fn name(self) -> &'static str {
        match self {
            Route::Spec(m) => m.name(),
            Route::Extension { ext_method } => ext_method,
        }
    }
}

/// The route for `method`, matched exactly, or `None` — which is `-32601`.
pub fn route_of(method: &str) -> Option<Route> {
    if let Some(m) = SpecMethod::ALL.iter().find(|m| m.name() == method) {
        return Some(Route::Spec(*m));
    }
    EXTENSION_METHODS
        .iter()
        .find(|(name, _)| *name == method)
        .map(|(name, _)| Route::Extension { ext_method: name })
}

/// Whether an `A2A-Version` header value names a version this listener
/// speaks: the same major and minor as [`A2A_PROTOCOL_VERSION`], any patch.
///
/// The spec's rule is that the version is `major.minor` and a patch does not
/// change the protocol, so `1.0.3` is `1.0`. A bare `1` names no minor and is
/// not accepted: a server that guessed would be choosing the client's
/// protocol for it.
pub fn accepts_version(header: &str) -> bool {
    let (want_major, want_minor) = A2A_PROTOCOL_VERSION
        .split_once('.')
        .expect("A2A_PROTOCOL_VERSION is major.minor");
    let mut parts = header.trim().split('.');
    let number = |p: Option<&str>| p.and_then(|p| p.parse::<u32>().ok());
    let (major, minor) = (number(parts.next()), number(parts.next()));
    major.is_some() && major == want_major.parse().ok() && minor == want_minor.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The core table is the SDK's, name for name, in both directions — so a
    /// method the SDK defines cannot be missing from the table, and a method
    /// we invented or misspelled cannot pose as a core one. The SDK's
    /// constants are listed by name rather than iterated, because the crate
    /// offers no list to iterate: a twelfth constant must be added here on
    /// purpose.
    #[cfg(feature = "a2a")]
    #[test]
    fn spec_methods_are_exactly_the_sdks() {
        use a2a_rs::adapter::transport::jsonrpc_wire::methods as m;
        let sdk = [
            m::SEND_MESSAGE,
            m::SEND_STREAMING_MESSAGE,
            m::GET_TASK,
            m::LIST_TASKS,
            m::CANCEL_TASK,
            m::SUBSCRIBE_TO_TASK,
            m::CREATE_PUSH_CONFIG,
            m::GET_PUSH_CONFIG,
            m::LIST_PUSH_CONFIGS,
            m::DELETE_PUSH_CONFIG,
            m::GET_EXTENDED_AGENT_CARD,
        ];
        let ours: Vec<&str> = SpecMethod::ALL.iter().map(|m| m.name()).collect();
        for name in &ours {
            assert!(sdk.contains(name), "{name:?} is not an A2A method: {sdk:?}");
        }
        for name in sdk {
            assert!(
                ours.contains(&name),
                "the SDK defines {name:?} and the table does not route it"
            );
        }
        assert_eq!(ours.len(), sdk.len(), "no method is listed twice");
    }

    /// Every name the table routes, and nothing else: the specification's,
    /// then the declared extension methods, each to its own route. Every
    /// spelling agentd once answered besides those is refused, which is what
    /// makes it `-32601` on the wire.
    #[test]
    fn route_table_is_exact_and_refuses_every_legacy_spelling() {
        for m in SpecMethod::ALL {
            assert_eq!(route_of(m.name()), Some(Route::Spec(*m)), "{m:?}");
            assert_eq!(Route::Spec(*m).name(), m.name());
        }
        for (name, uri) in EXTENSION_METHODS {
            assert_eq!(
                route_of(name),
                Some(Route::Extension { ext_method: name }),
                "{name}"
            );
            // A declaration must name an extension this build can activate,
            // or a client asking for it by URI could never be granted it.
            assert!(
                super::super::EXTENSIONS.contains(uri),
                "{name:?} is declared under {uri:?}, which is not in EXTENSIONS"
            );
            assert!(
                route_of(name).is_some_and(|r| !matches!(r, Route::Spec(_))),
                "an extension method is never a core one: {name}"
            );
        }
        for legacy in [
            // The card read as a method, and its aliases.
            "GetAgentCard",
            "agent/card",
            "agent/getAuthenticatedExtendedCard",
            // The `a2a.` prefix, on a core method and on an extension one.
            "a2a.SendMessage",
            "a2a.GetTask",
            "a2a.GetAgentCard",
            "a2a.SubscribeToEvents",
            // Pairing, deleted with its handler.
            "Pair",
            "interface.pair",
            // The 0.3 names.
            "message/send",
            "message/stream",
            "tasks/get",
            "tasks/cancel",
            "tasks/resubscribe",
            "tasks/pushNotificationConfig/set",
            "SetTaskPushNotificationConfig",
            // The bridge's own verbs are not wire methods.
            "PublicCard",
            "NewTaskId",
            "PushConfigSet",
            // Case and whitespace are not forgiven.
            "sendmessage",
            " SendMessage",
            "SendMessage ",
            "",
        ] {
            assert_eq!(route_of(legacy), None, "{legacy:?} must not route");
        }
    }

    #[test]
    fn the_version_gate_accepts_one_zero_and_any_patch() {
        for ok in ["1.0", " 1.0 ", "1.0.0", "1.0.7", "1.0.x"] {
            assert!(accepts_version(ok), "{ok:?}");
        }
        for no in [
            "", " ", "1", "1.", "0.3", "0.3.0", "1.1", "2.0", "v1.0", "1.0beta", "one", ".0",
        ] {
            assert!(!accepts_version(no), "{no:?}");
        }
    }
}
