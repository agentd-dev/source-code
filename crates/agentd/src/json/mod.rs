//! Shared JSON-RPC 2.0 codec.
//!
//! One set of wire types serves three surfaces: the MCP client (to external
//! servers), the self-MCP server, and the private supervisor↔subagent
//! control channel. They differ only in *framing* (see [`frame`]): MCP stdio
//! is newline-delimited; the control channel is length-prefixed. RFC 0004,
//! RFC 0005.
//!
//! Keeping every wire type behind `serde` in this one module is deliberate: it
//! is the single isolation point from which the codec could be swapped to a
//! lighter encoder (e.g. miniserde) without touching call sites, should the
//! proc-macro compile weight ever need to go (rfcs/0002 §dependency-budget).

pub mod frame;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// JSON-RPC request/response id. Spec allows string or number (and, in
/// responses to a parse error, null). We never *send* a null id.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Id {
    Num(i64),
    Str(String),
}

impl From<i64> for Id {
    fn from(n: i64) -> Self {
        Id::Num(n)
    }
}
impl From<String> for Id {
    fn from(s: String) -> Self {
        Id::Str(s)
    }
}

/// A JSON-RPC 2.0 request (has an `id`; expects a response).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Request {
    pub jsonrpc: Version,
    pub id: Id,
    pub method: String,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub params: Option<Value>,
}

/// A JSON-RPC 2.0 notification (no `id`; no response).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Notification {
    pub jsonrpc: Version,
    pub method: String,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub params: Option<Value>,
}

/// A JSON-RPC 2.0 response (exactly one of `result` / `error`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Response {
    pub jsonrpc: Version,
    pub id: Id,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub error: Option<RpcError>,
}

/// A JSON-RPC 2.0 error object. Distinct from a *successful* result that
/// carries `isError: true` — the latter is a tool-domain failure fed back to
/// the model as an observation, the former is a protocol/transport failure.
/// That distinction is load-bearing in the loop (RFC 0004 §isError).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RpcError {
    pub code: i64,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub data: Option<Value>,
}

/// Any inbound JSON-RPC frame, before we know which kind it is. A reader
/// thread parses one of these per frame and dispatches: responses resolve a
/// pending request by id; notifications fan out to handlers; requests (only
/// on the server side / sampling-style server→client) are answered.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum Incoming {
    // Order matters for untagged. `Request` first: its `method` is a *required*
    // field, so it only matches frames that actually have one — and a Response's
    // optional `result`/`error` would otherwise let `Response` swallow a Request.
    // A Response (id, no method) then falls through to `Response`; a Notification
    // (method, no id) to `Notification`.
    Request(Request),
    Response(Response),
    Notification(Notification),
}

/// The literal `"2.0"`. A newtype so a malformed `jsonrpc` field is a parse
/// error, not a silent mismatch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Version;

impl Serialize for Version {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str("2.0")
    }
}
impl<'de> Deserialize<'de> for Version {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        if s == "2.0" {
            Ok(Version)
        } else {
            Err(serde::de::Error::custom("jsonrpc version must be \"2.0\""))
        }
    }
}

impl Request {
    pub fn new(id: impl Into<Id>, method: impl Into<String>, params: Option<Value>) -> Self {
        Request {
            jsonrpc: Version,
            id: id.into(),
            method: method.into(),
            params,
        }
    }
}

impl Notification {
    pub fn new(method: impl Into<String>, params: Option<Value>) -> Self {
        Notification {
            jsonrpc: Version,
            method: method.into(),
            params,
        }
    }
}

impl Response {
    pub fn ok(id: Id, result: Value) -> Self {
        Response {
            jsonrpc: Version,
            id,
            result: Some(result),
            error: None,
        }
    }
    pub fn err(id: Id, code: i64, message: impl Into<String>) -> Self {
        Response {
            jsonrpc: Version,
            id,
            result: None,
            error: Some(RpcError {
                code,
                message: message.into(),
                data: None,
            }),
        }
    }
}

// Standard JSON-RPC error codes (subset we use).
pub const PARSE_ERROR: i64 = -32700;
pub const INVALID_REQUEST: i64 = -32600;
pub const METHOD_NOT_FOUND: i64 = -32601;
pub const INVALID_PARAMS: i64 = -32602;
pub const INTERNAL_ERROR: i64 = -32603;
/// MCP server-defined: a `resources/read` for a URI the server doesn't have.
pub const RESOURCE_NOT_FOUND: i64 = -32002;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_roundtrips() {
        let r = Request::new(1, "tools/call", Some(serde_json::json!({"name": "x"})));
        let s = serde_json::to_string(&r).unwrap();
        assert!(s.contains("\"jsonrpc\":\"2.0\""));
        assert!(s.contains("\"id\":1"));
        let back: Request = serde_json::from_str(&s).unwrap();
        assert_eq!(back.method, "tools/call");
    }

    #[test]
    fn incoming_discriminates_response_vs_notification() {
        let resp = r#"{"jsonrpc":"2.0","id":7,"result":{"ok":true}}"#;
        match serde_json::from_str::<Incoming>(resp).unwrap() {
            Incoming::Response(r) => assert_eq!(r.id, Id::Num(7)),
            other => panic!("expected response, got {other:?}"),
        }
        let note = r#"{"jsonrpc":"2.0","method":"notifications/resources/updated","params":{"uri":"file://a"}}"#;
        match serde_json::from_str::<Incoming>(note).unwrap() {
            Incoming::Notification(n) => assert_eq!(n.method, "notifications/resources/updated"),
            other => panic!("expected notification, got {other:?}"),
        }
    }

    #[test]
    fn incoming_parses_request_not_response() {
        // Regression: a server→client request (has `method`) must parse as
        // Request, not be swallowed by Response (whose fields are all optional).
        let req = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#;
        match serde_json::from_str::<Incoming>(req).unwrap() {
            Incoming::Request(r) => assert_eq!(r.method, "initialize"),
            other => panic!("expected request, got {other:?}"),
        }
    }

    #[test]
    fn bad_version_is_a_parse_error() {
        let bad = r#"{"jsonrpc":"1.0","id":1,"method":"x"}"#;
        assert!(serde_json::from_str::<Request>(bad).is_err());
    }

    #[test]
    fn string_id_supported() {
        let resp = r#"{"jsonrpc":"2.0","id":"abc","result":1}"#;
        let r: Response = serde_json::from_str(resp).unwrap();
        assert_eq!(r.id, Id::Str("abc".into()));
    }
}
