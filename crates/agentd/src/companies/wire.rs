// SPDX-License-Identifier: AGPL-3.0-only
//! Every name the Personal Agent Protocol draft puts on the wire, as the
//! draft pinned at `95fcbf2` writes it: paths, document members, grant
//! types, parameters, scopes and error codes (rfcs/0047 §5.2). agentd's own
//! surface uses neutral names (`companies`, `company`); the protocol's
//! constants carry its name, and agentd must send and read them exactly, so
//! they live here and nowhere else — an upstream rename is a change to this
//! one file.

/// The draft's version, `major.minor` (§3.1).
pub const PROTOCOL_VERSION: &str = "0.1";
/// The major version a document must carry for agentd to use it (§3.1).
pub const PROTOCOL_MAJOR: &str = "0";

/// Where a Company publishes its discovery document (§3).
pub const DISCOVERY_PATH: &str = "/.well-known/poppy.json";
/// The RFC 8414 well-known suffix, inserted before the issuer's path (§3.2).
pub const AS_METADATA_SUFFIX: &str = "oauth-authorization-server";
/// The RFC 9728 well-known suffix of an MCP server's metadata (§6).
pub const RESOURCE_METADATA_SUFFIX: &str = "oauth-protected-resource";

/// The authorization server metadata member listing every domain whose
/// discovery document names the issuer (§3.2).
pub const DOMAINS_MEMBER: &str = "poppy_domains";
/// The metadata member listing the DPoP algorithms (RFC 9449 §5.1, §3.2).
pub const DPOP_ALGS_MEMBER: &str = "dpop_signing_alg_values_supported";
/// The prefixed spelling issue #12 reports a Company publishing instead,
/// which breaks §3.2 and is refused by name.
pub const PREFIXED_DPOP_ALGS_MEMBER: &str = "poppy_dpop_signing_alg_values_supported";
/// The algorithm every Company accepts for DPoP Proofs (§3.2, §4.3).
pub const ES256: &str = "ES256";

/// The scopes the protocol defines (§4.4), and the prefix it reserves.
pub const SCOPE_READ: &str = "poppy:read";
pub const SCOPE_WRITE: &str = "poppy:write";
pub const RESERVED_SCOPE_PREFIX: &str = "poppy:";

/// The JWT `typ` of a browser assertion (§5).
pub const BROWSER_ASSERTION_TYPE: &str = "poppy-browser+jwt";
/// The conversation protocol type the draft defines (§7.1).
pub const CONVERSATION_TYPE: &str = "poppy";

/// A conversation's requests under the `poppy` entry's endpoint (§7.2–§7.6).
pub mod conversation {
    pub const MESSAGES: &str = "messages";
    pub const EVENTS: &str = "events";
    pub const HANDOFF: &str = "handoff";
    pub const CLOSE: &str = "close";
    /// The query parameters of a read, and of a `POST` that waits for the
    /// reply (§7.3, §7.5).
    pub const CURSOR: &str = "cursor";
    pub const WAIT: &str = "wait";
    /// What a stream's reconnect sends in place of `cursor` (§7.6).
    pub const LAST_EVENT_ID: &str = "Last-Event-ID";
    /// The media type a read asks for a stream with (§7.6).
    pub const EVENT_STREAM: &str = "text/event-stream";
    /// The prefix the draft suggests for a message ID (§7.2).
    pub const MESSAGE_ID_PREFIX: &str = "msg_";
    /// The request member naming the conversation a Direct Conversation
    /// follows (§7.10).
    pub const PARENT: &str = "parent_conversation_id";
}

/// The conversation event types (§7.5, §7.6).
pub mod event {
    pub const MESSAGE: &str = "message";
    pub const STATE: &str = "state";
    pub const AUTHORIZATION: &str = "authorization";
    pub const USER_REQUESTED: &str = "user_requested";
    pub const DIRECT_OPENED: &str = "direct_opened";
    pub const DIRECT_CLOSED: &str = "direct_closed";
    /// A piece of a message being written, sent only in a stream (§7.6).
    pub const TEXT_DELTA: &str = "text-delta";
}

/// Who wrote a message (§7.4, §7.8).
pub const SENDER_AGENT: &str = "agent";
pub const SENDER_HUMAN: &str = "human";
/// Who a Company message's side is, as the Company records it (§7.4).
pub const ROLE_COMPANY: &str = "company";

/// The document member listing extensions, in a discovery document and a
/// client metadata document (§3.3, §4.1).
pub const EXTENSIONS_MEMBER: &str = "extensions";

/// The Operations extension, version 1 (Operations §2–§9).
pub mod operations {
    /// Its name under `extensions`, and the version agentd speaks.
    pub const EXTENSION: &str = "operations";
    pub const VERSION: &str = "1";
    /// The member an operation travels in: an MCP result's
    /// `structuredContent`, an OpenAPI `202` body, a message (§4).
    pub const MEMBER: &str = "operation";
    /// The requests under the operations endpoint (§5.2, §7).
    pub const CONFIRM: &str = "confirm";
    pub const CANCEL: &str = "cancel";
    /// How the User's approval was given (§5.1).
    pub const APPROVED_BY_USER: &str = "user";
    pub const APPROVED_BY_STANDING: &str = "standing_permission";
    /// An operation's states (§3); the last four are final.
    pub const PROPOSED: &str = "proposed";
    pub const IN_PROGRESS: &str = "in_progress";
    pub const SUCCEEDED: &str = "succeeded";
    pub const FAILED: &str = "failed";
    pub const CANCELLED: &str = "cancelled";
    pub const EXPIRED: &str = "expired";
    pub const FINAL: [&str; 4] = [SUCCEEDED, FAILED, CANCELLED, EXPIRED];
}
/// The status of a conversation that takes no more messages (§7.7).
pub const STATUS_CLOSED: &str = "closed";

/// The header an API write's idempotency key travels in (Guides
/// #both-retries).
pub const IDEMPOTENCY_KEY: &str = "Idempotency-Key";

/// The API types of `apis[].type` (§6).
pub const API_MCP: &str = "mcp";
pub const API_OPENAPI: &str = "openapi";
/// The oldest MCP revision a listed MCP server may speak (§6).
pub const MCP_MIN_VERSION: &str = "2025-06-18";

/// The DPoP request and response headers, and a proof's `typ` (§4.3).
pub const DPOP_HEADER: &str = "DPoP";
pub const DPOP_NONCE_HEADER: &str = "DPoP-Nonce";
pub const DPOP_TYP: &str = "dpop+jwt";

/// The grant types of the token endpoint (§4.2, §4.5, §4.6, §4.8).
pub mod grant {
    pub const JWT_BEARER: &str = "urn:ietf:params:oauth:grant-type:jwt-bearer";
    pub const AUTHORIZATION_CODE: &str = "authorization_code";
    pub const DEVICE_CODE: &str = "urn:ietf:params:oauth:grant-type:device_code";
    pub const REFRESH_TOKEN: &str = "refresh_token";
}

/// Request parameters the protocol adds to OAuth, or names (§4.2–§4.9).
pub mod param {
    /// Ties a token request to a Session (§4.2).
    pub const SESSION_ID: &str = "session_id";
    /// RFC 8707's resource indicator: the MCP server a Bearer token is for
    /// (§4.3).
    pub const RESOURCE: &str = "resource";
}

/// Token response members the protocol adds (§4.2).
pub mod member {
    pub const SESSION_ID: &str = "session_id";
    pub const SIGNED_IN: &str = "signed_in";
}

/// The token type of a Session Token: DPoP, or Bearer for MCP (§4.2, §4.3).
pub const TOKEN_TYPE_DPOP: &str = "DPoP";
pub const TOKEN_TYPE_BEARER: &str = "Bearer";

/// The error codes of the token endpoint (§4.2, §4.4, §4.6) and of APIs and
/// MCP servers (§6), with the standard OAuth ones the draft uses.
pub mod error {
    pub const INVALID_CLIENT: &str = "invalid_client";
    pub const INVALID_GRANT: &str = "invalid_grant";
    pub const INVALID_SESSION: &str = "invalid_session";
    pub const ACCOUNT_MISMATCH: &str = "account_mismatch";
    pub const INVALID_DPOP_PROOF: &str = "invalid_dpop_proof";
    pub const USE_DPOP_NONCE: &str = "use_dpop_nonce";
    pub const EXTENSION_REQUIRED: &str = "extension_required";
    pub const RATE_LIMITED: &str = "rate_limited";
    pub const INVALID_SCOPE: &str = "invalid_scope";
    pub const INVALID_REQUEST: &str = "invalid_request";
    pub const INVALID_TARGET: &str = "invalid_target";
    pub const UNSUPPORTED_GRANT_TYPE: &str = "unsupported_grant_type";
    pub const AUTHORIZATION_PENDING: &str = "authorization_pending";
    pub const SLOW_DOWN: &str = "slow_down";
    pub const ACCESS_DENIED: &str = "access_denied";
    pub const EXPIRED_TOKEN: &str = "expired_token";
    pub const INVALID_TOKEN: &str = "invalid_token";
    pub const SIGN_IN_REQUIRED: &str = "sign_in_required";
    pub const INSUFFICIENT_SCOPE: &str = "insufficient_scope";
    /// The conversation errors (§7.13).
    pub const CONVERSATION_NOT_FOUND: &str = "conversation_not_found";
    pub const CONVERSATION_CLOSED: &str = "conversation_closed";
    pub const DIRECT_CONVERSATION_OPEN: &str = "direct_conversation_open";
    pub const MESSAGE_ID_CONFLICT: &str = "message_id_conflict";
    pub const INVALID_CURSOR: &str = "invalid_cursor";
    pub const CURSOR_EXPIRED: &str = "cursor_expired";
    /// The operations endpoint's errors (Operations §9).
    pub const OPERATION_NOT_FOUND: &str = "operation_not_found";
    pub const TERMS_CHANGED: &str = "terms_changed";
    pub const USER_APPROVAL_REQUIRED: &str = "user_approval_required";
}
