// SPDX-License-Identifier: AGPL-3.0-only
//! The JSON-RPC methods the listener answers.

/// Calls answered by the listener BEFORE the dispatch table, so they never
/// appear in [`METHODS`]: the public card read, and the interface pairing
/// handshake. Both are served — `--capabilities` reports them — and both are
/// deliberately outside the spec-method assertions, which is why they need a
/// name of their own rather than an exception buried in three test files.
pub const LOCAL_METHODS: &[&str] = &["GetAgentCard", "Pair"];

/// Every JSON-RPC method the A2A listener dispatches.
///
/// Lives here rather than beside the dispatch because the `--capabilities`
/// manifest is always compiled and the listener is `a2a`-gated: a manifest that
/// could not read this list grew a hand-typed copy of it, and that copy drifted
/// by five methods.
pub const METHODS: &[&str] = &[
    "SendMessage",
    "SendStreamingMessage",
    "GetTask",
    "CancelTask",
    "ListTasks",
    "SubscribeToTask",
    "SubscribeToEvents",
    "CreateTaskPushNotificationConfig",
    "GetTaskPushNotificationConfig",
    "ListTaskPushNotificationConfigs",
    "DeleteTaskPushNotificationConfig",
    "GetExtendedAgentCard",
];

/// The A2A protocol version, as the `A2A-Version` header and a card
/// interface's `protocolVersion` spell it.
pub const A2A_PROTOCOL_VERSION: &str = "1.0";
