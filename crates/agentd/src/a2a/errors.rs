// SPDX-License-Identifier: AGPL-3.0-only
//! The A2A error vocabulary agentd answers with: the codes, the machine
//! `reason`s and the `google.rpc` detail shapes that carry them.
//!
//! One table, so a refusal means the same thing whichever layer raises it —
//! the listener before a request reaches the SDK, or the runtime on its
//! single-writer loop. The table is always compiled: code outside the `a2a` feature (the manifest, the
//! reload path, a test) names a code from here instead of retyping the number.
//!
//! The spec's codes are **not** reused for anything else. `-32003` means push
//! notifications are off and `-32007` means no extended card is configured;
//! "who are you" and "you may not" are [`UNAUTHENTICATED`] and
//! [`PERMISSION_DENIED`], outside the JSON-RPC reserved range, so a client that
//! branches on a spec code is never told the wrong thing
//! (`crates/agentd-cli/tests/error_codes_guard.rs` holds the listener to it).
//!
//! Two layers. The builders below emit the runtime's error *objects*,
//! `{code, message, data}` with `data` an array of `google.rpc` `Any`s tagged
//! by `@type` — the shape the reactor answers a refused request with on its
//! reply channel, and one the table needs no feature gate for. None of them
//! is written to the wire as it is: with the `a2a` feature, [`to_a2a`] turns
//! one into the SDK's typed [`a2a_types::A2AError`], which is what a handler
//! returns and the SDK renders, and [`response`] renders a refusal the
//! listener makes before the SDK is reached with the SDK's own types.
//!
//! **What the SDK keeps.** The SDK's JSON-RPC error always ends `error.data`
//! with the protocol's `ErrorInfo` — the reason its code names, under
//! `a2a-protocol.org`, stamped with a `timestamp` — and keeps an `ErrorInfo`
//! of the application's only for its `metadata`, which it merges in. So the
//! reason agentd coined for a refusal travels as that metadata's
//! [`AGENTD_REASON`]: `{"reason": "INTERNAL_ERROR", "domain":
//! "a2a-protocol.org", "metadata": {"agentdReason": "DRAINING", …}}`. Every
//! other detail (a `BadRequest`) goes through as it is. The two identity
//! codes are agentd's and not the protocol's — the SDK has no reason for
//! them — so a `-31401` or `-31403` the listener answers carries agentd's own
//! `ErrorInfo` (`UNAUTHENTICATED`, `PERMISSION_DENIED` under `agentd.dev`).

use serde_json::{Map, Value, json};

// ---- JSON-RPC 2.0 -----------------------------------------------------------
// Taken from the MCP crate's table rather than retyped: agentd speaks one
// JSON-RPC, whichever protocol rides on it.

pub const PARSE_ERROR: i64 = ::mcp::rpc::PARSE_ERROR;
pub const INVALID_REQUEST: i64 = ::mcp::rpc::INVALID_REQUEST;
pub const METHOD_NOT_FOUND: i64 = ::mcp::rpc::METHOD_NOT_FOUND;
pub const INVALID_PARAMS: i64 = ::mcp::rpc::INVALID_PARAMS;
pub const INTERNAL_ERROR: i64 = ::mcp::rpc::INTERNAL_ERROR;

// ---- A2A 1.0 ----------------------------------------------------------------

pub const TASK_NOT_FOUND: i64 = -32001;
pub const TASK_NOT_CANCELABLE: i64 = -32002;
/// Push notifications are not enabled on this agent — and nothing else.
pub const PUSH_NOTIFICATION_NOT_SUPPORTED: i64 = -32003;
pub const UNSUPPORTED_OPERATION: i64 = -32004;
pub const CONTENT_TYPE_NOT_SUPPORTED: i64 = -32005;
pub const INVALID_AGENT_RESPONSE: i64 = -32006;
/// This agent has no extended card — and nothing else (never "anonymous").
pub const EXTENDED_AGENT_CARD_NOT_CONFIGURED: i64 = -32007;
pub const EXTENSION_SUPPORT_REQUIRED: i64 = -32008;
pub const VERSION_NOT_SUPPORTED: i64 = -32009;

// ---- agentd's own -----------------------------------------------------------
// Outside -32768..-32000, which JSON-RPC reserves, and shaped after the HTTP
// status the listener answers them with, so the number reads as what it is.

/// No credential, or one that does not resolve to a principal (HTTP 401).
pub const UNAUTHENTICATED: i64 = -31401;
/// A known principal whose role does not grant the call (HTTP 403).
pub const PERMISSION_DENIED: i64 = -31403;

/// The `ErrorInfo.reason` values agentd emits. A separate namespace because
/// several share a name with the code they explain.
pub mod reason {
    pub const RATE_LIMITED: &str = "RATE_LIMITED";
    pub const DRAINING: &str = "DRAINING";
    pub const EXTENSION_NOT_ACTIVATED: &str = "EXTENSION_NOT_ACTIVATED";
    pub const EXTENSION_NOT_DECLARED: &str = "EXTENSION_NOT_DECLARED";
    pub const EXTENSION_NOT_MARKED: &str = "EXTENSION_NOT_MARKED";
    pub const COMMAND_ENVELOPE_AMBIGUOUS: &str = "COMMAND_ENVELOPE_AMBIGUOUS";
    pub const COMMAND_TASK_ID: &str = "COMMAND_TASK_ID";
    pub const UNKNOWN_OP: &str = "UNKNOWN_OP";
    pub const INVALID_COMMAND_ARGS: &str = "INVALID_COMMAND_ARGS";
    pub const INTROSPECTION_DISABLED: &str = "INTROSPECTION_DISABLED";
    pub const UNAUTHENTICATED: &str = "UNAUTHENTICATED";
    pub const PERMISSION_DENIED: &str = "PERMISSION_DENIED";
    pub const VERSION_NOT_SUPPORTED: &str = "VERSION_NOT_SUPPORTED";
    pub const EXTENSION_SUPPORT_REQUIRED: &str = "EXTENSION_SUPPORT_REQUIRED";
    pub const CONTENT_TYPE_NOT_SUPPORTED: &str = "CONTENT_TYPE_NOT_SUPPORTED";
    pub const UNSUPPORTED_OPERATION: &str = "UNSUPPORTED_OPERATION";
}

/// The domain of a reason the A2A specification itself defines.
pub const A2A_DOMAIN: &str = "a2a-protocol.org";
/// The domain of every reason agentd coined.
pub const AGENTD_DOMAIN: &str = "agentd.dev";

/// The reasons that belong to the specification. Everything else is agentd's,
/// so a client can tell a protocol refusal from a product one by the domain.
const SPEC_REASONS: &[&str] = &[
    reason::VERSION_NOT_SUPPORTED,
    reason::EXTENSION_SUPPORT_REQUIRED,
    reason::CONTENT_TYPE_NOT_SUPPORTED,
    reason::UNSUPPORTED_OPERATION,
];

/// The `ErrorInfo.domain` a reason is published under.
pub fn domain_of(reason: &str) -> &'static str {
    if SPEC_REASONS.contains(&reason) {
        A2A_DOMAIN
    } else {
        AGENTD_DOMAIN
    }
}

/// The HTTP status a refusal the listener makes before the SDK travels with.
/// The two identity refusals carry their status so a proxy, a browser and a
/// plain HTTP client all see them; every other JSON-RPC error is an answer,
/// delivered with 200 — as the SDK delivers every error it renders.
pub fn http_status_of(code: i64) -> u16 {
    match code {
        UNAUTHENTICATED => 401,
        PERMISSION_DENIED => 403,
        _ => 200,
    }
}

/// A `google.rpc.ErrorInfo` detail. Empty metadata is omitted, as the SDKs
/// omit it.
pub fn error_info(domain: &str, reason: &str, metadata: &[(&str, &str)]) -> Value {
    let mut info = json!({
        "@type": "type.googleapis.com/google.rpc.ErrorInfo",
        "reason": reason,
        "domain": domain,
    });
    if !metadata.is_empty() {
        let map: Map<String, Value> = metadata
            .iter()
            .map(|(k, v)| ((*k).to_string(), Value::from(*v)))
            .collect();
        info["metadata"] = Value::Object(map);
    }
    info
}

/// A `google.rpc.BadRequest` detail: one violation per `(field, description)`.
pub fn bad_request(violations: &[(&str, &str)]) -> Value {
    let violations: Vec<Value> = violations
        .iter()
        .map(|(field, description)| json!({"field": field, "description": description}))
        .collect();
    json!({
        "@type": "type.googleapis.com/google.rpc.BadRequest",
        "fieldViolations": violations,
    })
}

/// An error object, `{code, message, data?}`. `details` become `data`, and
/// an empty list leaves `data` out rather than sending `[]`.
pub fn error_object(code: i64, message: &str, details: Vec<Value>) -> Value {
    let mut error = json!({"code": code, "message": message});
    if !details.is_empty() {
        error["data"] = Value::Array(details);
    }
    error
}

/// The `ErrorInfo.metadata` key the reason agentd coined for a refusal is
/// carried under, in the protocol's `ErrorInfo` the SDK writes.
pub const AGENTD_REASON: &str = "agentdReason";

#[cfg(feature = "a2a")]
pub use typed::{response, to_a2a, to_wire};

#[cfg(feature = "a2a")]
mod typed {
    use super::{AGENTD_DOMAIN, AGENTD_REASON, INTERNAL_ERROR, PERMISSION_DENIED, UNAUTHENTICATED};
    use a2a_types::{
        A2AError, ERROR_INFO_TYPE, JsonRpcError, JsonRpcId, JsonRpcResponse, TypedDetail,
    };
    use serde_json::{Value, json};

    /// An error object's details, as the SDK's typed details.
    fn details_of(e: &Value) -> Vec<TypedDetail> {
        e.get("data")
            .cloned()
            .and_then(|d| serde_json::from_value::<Vec<TypedDetail>>(d).ok())
            .unwrap_or_default()
    }

    /// An `ErrorInfo` of agentd's, with its reason named in its metadata —
    /// the part of it the SDK keeps.
    fn reason_in_metadata(mut d: TypedDetail) -> TypedDetail {
        let ours = d.type_url == ERROR_INFO_TYPE
            && d.value.get("domain").and_then(Value::as_str) == Some(AGENTD_DOMAIN);
        if let (true, Some(reason)) = (ours, d.value.get("reason").cloned()) {
            let meta = d
                .value
                .entry("metadata".to_string())
                .or_insert_with(|| json!({}));
            if let Some(m) = meta.as_object_mut() {
                m.insert(AGENTD_REASON.to_string(), reason);
            }
        }
        d
    }

    /// The runtime's error object as the spec's typed error: its code — the
    /// spec's, or agentd's own outside the reserved range — and message as
    /// they are, and its `data` as the typed details the SDK carries, each of
    /// agentd's reasons named where the SDK keeps it.
    pub fn to_a2a(e: &Value) -> A2AError {
        let code = e
            .get("code")
            .and_then(Value::as_i64)
            .and_then(|n| i32::try_from(n).ok())
            .unwrap_or(INTERNAL_ERROR as i32);
        let message = e
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("internal error");
        let details: Vec<TypedDetail> = details_of(e).into_iter().map(reason_in_metadata).collect();
        let error = A2AError::new(code, message);
        if details.is_empty() {
            error
        } else {
            error.with_details(details)
        }
    }

    /// The error object as the JSON-RPC error that goes on the wire: the
    /// SDK's rendering of [`to_a2a`], except for agentd's two identity codes,
    /// which carry agentd's own `ErrorInfo` (the SDK names no reason for a
    /// code outside the protocol).
    pub fn to_wire(e: &Value) -> JsonRpcError {
        let a = to_a2a(e);
        if matches!(i64::from(a.code), UNAUTHENTICATED | PERMISSION_DENIED) {
            let details = details_of(e);
            return JsonRpcError {
                code: a.code,
                message: a.message,
                data: (!details.is_empty())
                    .then(|| serde_json::to_value(details).unwrap_or_default()),
            };
        }
        a.to_jsonrpc_error()
    }

    /// The whole JSON-RPC response refusing request `id` with `e`.
    pub fn response(id: Value, e: &Value) -> Value {
        let id = serde_json::from_value::<JsonRpcId>(id).unwrap_or(JsonRpcId::Null);
        serde_json::to_value(JsonRpcResponse::error(id, to_wire(e))).unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every code in the table, so the status test covers additions too.
    const ALL: &[i64] = &[
        PARSE_ERROR,
        INVALID_REQUEST,
        METHOD_NOT_FOUND,
        INVALID_PARAMS,
        INTERNAL_ERROR,
        TASK_NOT_FOUND,
        TASK_NOT_CANCELABLE,
        PUSH_NOTIFICATION_NOT_SUPPORTED,
        UNSUPPORTED_OPERATION,
        CONTENT_TYPE_NOT_SUPPORTED,
        INVALID_AGENT_RESPONSE,
        EXTENDED_AGENT_CARD_NOT_CONFIGURED,
        EXTENSION_SUPPORT_REQUIRED,
        VERSION_NOT_SUPPORTED,
        UNAUTHENTICATED,
        PERMISSION_DENIED,
    ];

    #[test]
    fn http_status_mapping() {
        assert_eq!(http_status_of(UNAUTHENTICATED), 401);
        assert_eq!(http_status_of(PERMISSION_DENIED), 403);
        for &code in ALL {
            if code != UNAUTHENTICATED && code != PERMISSION_DENIED {
                assert_eq!(http_status_of(code), 200, "code {code}");
            }
        }
        // The identity codes live outside JSON-RPC's reserved block, where no
        // spec code can collide with them.
        for code in [UNAUTHENTICATED, PERMISSION_DENIED] {
            assert!(!(-32768..=-32000).contains(&code), "{code} is reserved");
        }
    }

    #[test]
    fn reasons_are_published_under_their_owners_domain() {
        // Spelled out rather than read from SPEC_REASONS, so dropping one from
        // the table fails here.
        for r in [
            "VERSION_NOT_SUPPORTED",
            "EXTENSION_SUPPORT_REQUIRED",
            "CONTENT_TYPE_NOT_SUPPORTED",
            "UNSUPPORTED_OPERATION",
        ] {
            assert_eq!(domain_of(r), "a2a-protocol.org", "{r}");
        }
        for r in [
            reason::RATE_LIMITED,
            reason::DRAINING,
            reason::UNAUTHENTICATED,
            reason::PERMISSION_DENIED,
            reason::UNKNOWN_OP,
        ] {
            assert_eq!(domain_of(r), "agentd.dev", "{r}");
        }
    }

    #[test]
    fn error_object_leaves_out_empty_data() {
        let bare = error_object(TASK_NOT_FOUND, "no such task", vec![]);
        assert_eq!(bare, json!({"code": -32001, "message": "no such task"}));
        let info = error_info(AGENTD_DOMAIN, reason::DRAINING, &[]);
        assert!(info.get("metadata").is_none(), "{info}");
        let full = error_object(INTERNAL_ERROR, "draining", vec![info.clone()]);
        assert_eq!(full["data"], json!([info]));
    }

    /// A refusal on the wire is the SDK's: the protocol's `ErrorInfo` last,
    /// agentd's reason in its metadata, every other detail as it was — and
    /// the identity codes with agentd's own `ErrorInfo`.
    #[cfg(feature = "a2a")]
    #[test]
    fn the_wire_error_is_the_sdks() {
        let bad = bad_request(&[("message.parts[0].data.agentd.op", "unknown")]);
        let e = error_object(
            INVALID_PARAMS,
            "unknown command",
            vec![
                bad.clone(),
                error_info(AGENTD_DOMAIN, reason::UNKNOWN_OP, &[("op", "x")]),
            ],
        );
        let v = response(json!("r1"), &e);
        assert_eq!(v["id"], "r1");
        assert_eq!(v["error"]["code"], -32602);
        assert_eq!(v["error"]["message"], "unknown command");
        let data = v["error"]["data"].as_array().unwrap();
        assert_eq!(data.len(), 2, "{v}");
        assert_eq!(data[0], bad);
        assert_eq!(data[1]["reason"], "INVALID_PARAMS", "{v}");
        assert_eq!(data[1]["domain"], A2A_DOMAIN);
        assert_eq!(data[1]["metadata"][AGENTD_REASON], "UNKNOWN_OP");
        assert_eq!(data[1]["metadata"]["op"], "x");
        assert!(data[1]["metadata"]["timestamp"].is_string(), "{v}");

        let denied = error_object(
            PERMISSION_DENIED,
            "no",
            vec![error_info(AGENTD_DOMAIN, reason::PERMISSION_DENIED, &[])],
        );
        let v = response(json!(3), &denied);
        assert_eq!(
            v["error"],
            json!({"code": -31403, "message": "no", "data": [
                {"@type": "type.googleapis.com/google.rpc.ErrorInfo",
                 "reason": "PERMISSION_DENIED", "domain": "agentd.dev"}]})
        );
        // An id the request did not carry usably is null.
        assert_eq!(response(json!([1]), &denied)["id"], Value::Null);
    }

    /// The table and the A2A project's SDK agree on every code both define.
    #[cfg(feature = "a2a")]
    #[test]
    fn codes_match_the_sdk() {
        use a2a_types::error_code as sdk;
        let pairs = [
            (PARSE_ERROR, sdk::PARSE_ERROR),
            (INVALID_REQUEST, sdk::INVALID_REQUEST),
            (METHOD_NOT_FOUND, sdk::METHOD_NOT_FOUND),
            (INVALID_PARAMS, sdk::INVALID_PARAMS),
            (INTERNAL_ERROR, sdk::INTERNAL_ERROR),
            (TASK_NOT_FOUND, sdk::TASK_NOT_FOUND),
            (TASK_NOT_CANCELABLE, sdk::TASK_NOT_CANCELABLE),
            (
                PUSH_NOTIFICATION_NOT_SUPPORTED,
                sdk::PUSH_NOTIFICATION_NOT_SUPPORTED,
            ),
            (UNSUPPORTED_OPERATION, sdk::UNSUPPORTED_OPERATION),
            (CONTENT_TYPE_NOT_SUPPORTED, sdk::CONTENT_TYPE_NOT_SUPPORTED),
            (INVALID_AGENT_RESPONSE, sdk::INVALID_AGENT_RESPONSE),
            (
                EXTENDED_AGENT_CARD_NOT_CONFIGURED,
                sdk::EXTENDED_CARD_NOT_CONFIGURED,
            ),
            (EXTENSION_SUPPORT_REQUIRED, sdk::EXTENSION_SUPPORT_REQUIRED),
            (VERSION_NOT_SUPPORTED, sdk::VERSION_NOT_SUPPORTED),
        ];
        for (ours, theirs) in pairs {
            assert_eq!(ours, i64::from(theirs));
        }
        assert_eq!(A2A_DOMAIN, a2a_types::PROTOCOL_DOMAIN);
    }

    /// What the builders emit is what the SDK's typed details read back —
    /// the shape a Go, Python or Rust client decodes — and re-encodes to the
    /// same object.
    #[cfg(feature = "a2a")]
    #[test]
    fn error_info_round_trips_as_google_rpc() {
        use a2a_types::{FieldViolation, TypedDetail};
        use std::collections::HashMap;
        let info = error_info(
            A2A_DOMAIN,
            reason::VERSION_NOT_SUPPORTED,
            &[("supported", "1.0"), ("requested", "0.3")],
        );
        let typed: TypedDetail = serde_json::from_value(info.clone()).expect("ErrorInfo");
        let want = TypedDetail::error_info(
            "VERSION_NOT_SUPPORTED",
            a2a_types::PROTOCOL_DOMAIN,
            Some(HashMap::from([
                ("supported".to_string(), "1.0".to_string()),
                ("requested".to_string(), "0.3".to_string()),
            ])),
        );
        assert_eq!(typed, want);
        assert_eq!(serde_json::to_value(&typed).unwrap(), info);

        let bare = error_info(AGENTD_DOMAIN, reason::DRAINING, &[]);
        let typed: TypedDetail = serde_json::from_value(bare.clone()).expect("bare ErrorInfo");
        assert_eq!(serde_json::to_value(&typed).unwrap(), bare);

        let bad = bad_request(&[("message.parts", "empty"), ("configuration", "bad")]);
        let typed: TypedDetail = serde_json::from_value(bad.clone()).expect("BadRequest");
        assert_eq!(
            typed,
            TypedDetail::bad_request(vec![
                FieldViolation {
                    field: "message.parts".into(),
                    description: "empty".into(),
                },
                FieldViolation {
                    field: "configuration".into(),
                    description: "bad".into(),
                },
            ])
        );
        assert_eq!(serde_json::to_value(&typed).unwrap(), bad);

        // And the whole error: `data` is the list the SDK's client reads.
        let whole = error_object(VERSION_NOT_SUPPORTED, "no", vec![info, bad]);
        let data: Vec<TypedDetail> =
            serde_json::from_value(whole["data"].clone()).expect("detail list");
        assert_eq!(data.len(), 2);
    }
}
