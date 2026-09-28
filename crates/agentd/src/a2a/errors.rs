// SPDX-License-Identifier: AGPL-3.0-only
//! The A2A error vocabulary agentd answers with: the codes, the machine
//! `reason`s and the `google.rpc` detail shapes that carry them.
//!
//! One table, so a refusal means the same thing whichever layer raises it —
//! the listener before a request reaches the runtime, the runtime on its
//! single-writer loop, or the filter that rewrites what the SDK answered. The
//! table is always compiled: code outside the `a2a` feature (the manifest, the
//! reload path, a test) names a code from here instead of retyping the number.
//!
//! The spec's codes are **not** reused for anything else. `-32003` means push
//! notifications are off and `-32007` means no extended card is configured;
//! "who are you" and "you may not" are [`UNAUTHENTICATED`] and
//! [`PERMISSION_DENIED`], outside the JSON-RPC reserved range, so a client that
//! branches on a spec code is never told the wrong thing
//! (`crates/agentd-cli/tests/error_codes_guard.rs` holds the listener to it).
//!
//! The builders emit plain `serde_json` in the shapes the official SDKs read —
//! an `error.data` array of `google.rpc` `Any`s tagged by `@type` — rather than
//! the SDK's own types, so the listener can answer before the protocol layer is
//! involved and the table needs no feature gate.

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

/// The HTTP status a JSON-RPC error travels with. The two identity refusals
/// carry their status so a proxy, a browser and a plain HTTP client all see
/// them; every other JSON-RPC error is an answer, delivered with 200.
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

/// A whole JSON-RPC error response. `details` become `error.data`, and an
/// empty list leaves `data` out rather than sending `[]`.
pub fn rpc_error(id: Value, code: i64, message: &str, details: Vec<Value>) -> Value {
    let mut error = json!({"code": code, "message": message});
    if !details.is_empty() {
        error["data"] = Value::Array(details);
    }
    json!({"jsonrpc": "2.0", "id": id, "error": error})
}

/// A code the SDK raised on its own, made one a spec client can read.
///
/// a2a-rs answers some failures with codes of its own invention — `-32100`
/// (its store), `-32101` (a version conflict) and `-32102` (a context another
/// principal owns). They sit inside JSON-RPC's reserved range but in none of
/// its defined blocks, so no client can interpret them, and the last one would
/// reveal that a context exists. Each becomes [`INTERNAL_ERROR`]; the SDK's
/// details and message stay as they were. Every other code passes unchanged.
pub fn normalize_native(code: i64) -> i64 {
    match code {
        -32102..=-32100 => INTERNAL_ERROR,
        other => other,
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
    fn native_out_of_range_codes_normalise() {
        for code in [-32100, -32101, -32102] {
            assert_eq!(normalize_native(code), INTERNAL_ERROR, "{code}");
        }
        for &code in ALL {
            assert_eq!(normalize_native(code), code, "{code} must pass unchanged");
        }
        assert_eq!(normalize_native(TASK_NOT_FOUND), -32001);
        // The neighbours of the band are not swept up with it.
        assert_eq!(normalize_native(-32099), -32099);
        assert_eq!(normalize_native(-32103), -32103);
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
    fn rpc_error_leaves_out_empty_data() {
        let bare = rpc_error(json!(7), TASK_NOT_FOUND, "no such task", vec![]);
        assert_eq!(
            bare,
            json!({"jsonrpc": "2.0", "id": 7, "error": {"code": -32001, "message": "no such task"}})
        );
        let info = error_info(AGENTD_DOMAIN, reason::DRAINING, &[]);
        assert!(info.get("metadata").is_none(), "{info}");
        let full = rpc_error(Value::Null, INTERNAL_ERROR, "draining", vec![info.clone()]);
        assert_eq!(full["error"]["data"], json!([info]));
    }

    /// The table and the SDK agree on every code both define, and the band
    /// [`normalize_native`] folds is exactly the SDK's own invention.
    #[cfg(feature = "a2a")]
    #[test]
    fn codes_match_the_sdk() {
        use a2a_rs::domain::error as sdk;
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
                sdk::AUTHENTICATED_EXTENDED_CARD_NOT_CONFIGURED,
            ),
        ];
        for (ours, theirs) in pairs {
            assert_eq!(ours, i64::from(theirs));
        }
        for native in [
            sdk::DATABASE_ERROR,
            sdk::VERSION_CONFLICT,
            sdk::CONTEXT_ACCESS_DENIED,
        ] {
            assert_eq!(normalize_native(i64::from(native)), INTERNAL_ERROR);
        }
    }

    /// What the builders emit is what the SDK's typed details read back —
    /// the shape a Go, Python or Rust client decodes — and re-encodes to the
    /// same bytes.
    #[cfg(feature = "a2a")]
    #[test]
    fn error_info_round_trips_as_google_rpc() {
        use a2a_rs::{ErrorDetail, ErrorInfo, FieldViolation};
        let info = error_info(
            A2A_DOMAIN,
            reason::VERSION_NOT_SUPPORTED,
            &[("supported", "1.0"), ("requested", "0.3")],
        );
        let typed: ErrorDetail = serde_json::from_value(info.clone()).expect("ErrorInfo");
        let mut want = ErrorInfo::new("VERSION_NOT_SUPPORTED")
            .with_metadata("supported", "1.0")
            .with_metadata("requested", "0.3");
        want.domain = "a2a-protocol.org".into();
        assert_eq!(typed, ErrorDetail::ErrorInfo(want));
        assert_eq!(serde_json::to_value(&typed).unwrap(), info);

        let bare = error_info(AGENTD_DOMAIN, reason::DRAINING, &[]);
        let typed: ErrorDetail = serde_json::from_value(bare.clone()).expect("bare ErrorInfo");
        assert_eq!(serde_json::to_value(&typed).unwrap(), bare);

        let bad = bad_request(&[("message.parts", "empty"), ("configuration", "bad")]);
        let typed: ErrorDetail = serde_json::from_value(bad.clone()).expect("BadRequest");
        assert_eq!(
            typed,
            ErrorDetail::BadRequest {
                field_violations: vec![
                    FieldViolation::new("message.parts", "empty"),
                    FieldViolation::new("configuration", "bad"),
                ],
            }
        );
        assert_eq!(serde_json::to_value(&typed).unwrap(), bad);

        // And the whole error: `data` is the list the SDK's client reads.
        let whole = rpc_error(json!("r1"), VERSION_NOT_SUPPORTED, "no", vec![info, bad]);
        let data: Vec<ErrorDetail> =
            serde_json::from_value(whole["error"]["data"].clone()).expect("detail list");
        assert_eq!(data.len(), 2);
    }
}
