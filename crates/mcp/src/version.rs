// SPDX-License-Identifier: Apache-2.0
//! MCP protocol version + **era** model, and version negotiation for both eras.
//!
//! MCP versions are `YYYY-MM-DD` date strings marking the last backward-incompatible
//! change; they sort chronologically as plain strings. Two eras
//! (modelcontextprotocol.io/specification/draft/basic/versioning §terminology):
//!
//! * **Legacy** — an `initialize` handshake + session (`2025-11-25` and earlier).
//!   The client advertises its latest version in `initialize`; the server echoes
//!   it if supported, else returns one it does (client adopts or disconnects).
//! * **Modern** — stateless, per-request `_meta` (`2026-07-28`+). There is *no
//!   handshake*: every request declares its version, and an unsupported version is
//!   rejected per request with an [`UnsupportedProtocolVersion`] error (`-32022`)
//!   listing the server's `supported` versions; the client retries with a mutual
//!   one. A dual-era client detects the server's era once and caches it.

use serde::Deserialize;

/// A protocol era: how version/identity/capabilities are conveyed and whether the
/// connection is session-based.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Era {
    /// `initialize` handshake + session (`2025-11-25` and earlier).
    Legacy,
    /// Stateless per-request metadata (`2026-07-28` and later).
    Modern,
}

/// Every MCP revision this library understands, **newest first** (dates sort
/// chronologically). To support a newly-released revision, add its date at the
/// front. The head is the latest overall; era-specific latests are
/// [`LATEST_MODERN_VERSION`] / [`LATEST_LEGACY_VERSION`].
pub const SUPPORTED_PROTOCOL_VERSIONS: &[&str] = &[
    "2026-07-28", // modern (stateless) — the RC (blog.modelcontextprotocol.io)
    "2025-11-25", // legacy — the current stable
    "2025-06-18",
    "2025-03-26",
    "2024-11-05",
];

/// The first **modern** (stateless) revision — the era boundary. Any well-formed
/// date `>=` this is Modern; anything earlier is Legacy.
pub const FIRST_MODERN_VERSION: &str = "2026-07-28";

/// The latest modern revision (advertised where the peer is known to be modern).
pub const LATEST_MODERN_VERSION: &str = "2026-07-28";

/// The latest legacy revision — what the `initialize` handshake advertises.
pub const LATEST_LEGACY_VERSION: &str = "2025-11-25";

/// The version advertised in a **legacy** `initialize` handshake. Kept as the
/// latest legacy revision: the handshake path speaks legacy until the modern
/// (stateless) dialect is wired into the client (a later phase). A modern client
/// declares its version per-request instead ([`LATEST_MODERN_VERSION`]).
pub const PROTOCOL_VERSION: &str = LATEST_LEGACY_VERSION;

/// The version a legacy Streamable HTTP server assumes when a request carries no
/// `MCP-Protocol-Version` header (transports §protocol-version-header).
pub const DEFAULT_NEGOTIATED_VERSION: &str = "2025-03-26";

/// The MCP-reserved JSON-RPC error code for an unsupported protocol version
/// (`-32022`, modern negotiation).
pub const UNSUPPORTED_PROTOCOL_VERSION_CODE: i64 = -32022;

/// The MCP-reserved JSON-RPC error code for a Streamable-HTTP header/body mismatch
/// or a missing/malformed required routing header (`-32020`).
pub const HEADER_MISMATCH_CODE: i64 = -32020;

/// The `_meta` key namespace carrying per-request protocol metadata in the modern
/// era (`io.modelcontextprotocol/{protocolVersion,clientInfo,clientCapabilities}`).
pub const META_NS: &str = "io.modelcontextprotocol/";

/// Is `v` a revision this library explicitly understands?
pub fn is_supported_version(v: &str) -> bool {
    SUPPORTED_PROTOCOL_VERSIONS.contains(&v)
}

/// Does `s` have the MCP `YYYY-MM-DD` version shape? (Cheap structural check, not a
/// calendar validation — enough to tell a date revision from a bogus string.)
pub fn is_date_version(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() == 10
        && b[4] == b'-'
        && b[7] == b'-'
        && b.iter()
            .enumerate()
            .all(|(i, &c)| i == 4 || i == 7 || c.is_ascii_digit())
}

/// The [`Era`] a protocol version belongs to. A well-formed date `>=`
/// [`FIRST_MODERN_VERSION`] (including unknown future dates) is [`Era::Modern`];
/// anything else is [`Era::Legacy`] (the safe default — legacy is the older,
/// wider-deployed behavior).
pub fn era_of(version: &str) -> Era {
    if is_date_version(version) && version >= FIRST_MODERN_VERSION {
        Era::Modern
    } else {
        Era::Legacy
    }
}

/// Negotiate the session version from a **legacy** server's `initialize` response
/// (lifecycle §version-negotiation). The server echoes our advertised version if
/// it supports it, else returns another it supports.
///
/// * A version we **know** → adopt it.
/// * An **unknown but newer** well-formed date → adopt it optimistically
///   (forward-compat: a future revision keeps our stable method subset, so a
///   brand-new server is reachable *before* we add its date above) — the caller
///   should log that it is speaking an unrecognized revision.
/// * Anything else (an older-unknown or malformed version) → `None`: the client
///   cannot agree on a version and SHOULD disconnect.
pub fn negotiate_version(server_version: &str) -> Option<String> {
    if is_supported_version(server_version) {
        return Some(server_version.to_string());
    }
    if is_date_version(server_version) && server_version > SUPPORTED_PROTOCOL_VERSIONS[0] {
        return Some(server_version.to_string());
    }
    None
}

/// The payload of an [`UNSUPPORTED_PROTOCOL_VERSION_CODE`] error's `data` — the
/// modern era's version-negotiation signal (versioning §protocol-version-negotiation).
#[derive(Debug, Clone, Default, Deserialize)]
pub struct UnsupportedProtocolVersion {
    /// The versions the server supports.
    #[serde(default)]
    pub supported: Vec<String>,
    /// The version the client requested (echoed back).
    #[serde(default)]
    pub requested: Option<String>,
}

/// Given a modern server's advertised `supported` versions (from a `-32022`
/// error), pick the best mutually-supported one to retry with — our newest that
/// the server also supports. `None` ⇒ no common version (surface to the user).
pub fn best_mutual_version(server_supported: &[String]) -> Option<String> {
    SUPPORTED_PROTOCOL_VERSIONS
        .iter()
        .find(|&&ours| server_supported.iter().any(|s| s == ours))
        .map(|v| v.to_string())
}

/// Is `code` a JSON-RPC error code only a **modern** server emits? Used for era
/// detection (versioning §backward-compatibility): a `-32022`
/// (UnsupportedProtocolVersion) or `-32020` (HeaderMismatch) in the body of a
/// failed modern probe identifies a modern server, so the client retries rather
/// than falling back to `initialize`. Generic codes (e.g. `-32601` method-not-
/// found) are ambiguous across eras and are NOT modern-defining.
pub fn is_modern_error_code(code: i64) -> bool {
    code == UNSUPPORTED_PROTOCOL_VERSION_CODE || code == HEADER_MISMATCH_CODE
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn eras_split_at_the_2026_boundary() {
        assert_eq!(era_of("2024-11-05"), Era::Legacy);
        assert_eq!(era_of("2025-11-25"), Era::Legacy);
        assert_eq!(era_of("2026-07-28"), Era::Modern);
        // An unknown FUTURE date is modern (forward-compat); a malformed string is
        // treated as legacy (the safe, wider-deployed default).
        assert_eq!(era_of("2099-01-01"), Era::Modern);
        assert_eq!(era_of("1.0.0"), Era::Legacy);
    }

    #[test]
    fn era_latests_are_consistent() {
        assert_eq!(era_of(LATEST_LEGACY_VERSION), Era::Legacy);
        assert_eq!(era_of(LATEST_MODERN_VERSION), Era::Modern);
        assert_eq!(FIRST_MODERN_VERSION, LATEST_MODERN_VERSION);
        assert!(is_supported_version(LATEST_LEGACY_VERSION));
        assert!(is_supported_version(LATEST_MODERN_VERSION));
        // The list is newest-first.
        let mut sorted = SUPPORTED_PROTOCOL_VERSIONS.to_vec();
        sorted.sort_unstable();
        sorted.reverse();
        assert_eq!(sorted.as_slice(), SUPPORTED_PROTOCOL_VERSIONS);
    }

    #[test]
    fn is_date_version_recognizes_the_shape() {
        assert!(is_date_version("2025-11-25"));
        assert!(is_date_version("2026-07-28"));
        assert!(!is_date_version("2025-11-5"));
        assert!(!is_date_version("2025/11/25"));
        assert!(!is_date_version("1.0.0"));
    }

    #[test]
    fn legacy_negotiate_adopts_known_and_newer_but_refuses_old_unknown() {
        for v in SUPPORTED_PROTOCOL_VERSIONS {
            assert_eq!(negotiate_version(v).as_deref(), Some(*v));
        }
        assert_eq!(
            negotiate_version("2099-01-01").as_deref(),
            Some("2099-01-01")
        );
        assert_eq!(negotiate_version("2020-01-01"), None);
        assert_eq!(negotiate_version("1.0.0"), None);
    }

    #[test]
    fn modern_best_mutual_picks_our_newest_common() {
        // Server supports two versions; we pick our newest that overlaps.
        let supported = vec!["2025-11-25".to_string(), "2026-07-28".to_string()];
        assert_eq!(
            best_mutual_version(&supported).as_deref(),
            Some("2026-07-28")
        );
        // Only an older overlap.
        let supported = vec!["2025-06-18".to_string()];
        assert_eq!(
            best_mutual_version(&supported).as_deref(),
            Some("2025-06-18")
        );
        // No overlap at all.
        let supported = vec!["1900-01-01".to_string()];
        assert_eq!(best_mutual_version(&supported), None);
    }

    #[test]
    fn modern_error_codes_are_recognized() {
        assert!(is_modern_error_code(UNSUPPORTED_PROTOCOL_VERSION_CODE));
        assert!(is_modern_error_code(HEADER_MISMATCH_CODE));
        // Generic JSON-RPC codes are ambiguous across eras, not modern-defining.
        assert!(!is_modern_error_code(-32601)); // method not found
        assert!(!is_modern_error_code(-32602)); // invalid params
        assert!(!is_modern_error_code(-32000));
    }

    #[test]
    fn unsupported_error_payload_parses() {
        let data = serde_json::json!({
            "supported": ["2026-07-28", "2025-11-25"],
            "requested": "1900-01-01"
        });
        let p: UnsupportedProtocolVersion = serde_json::from_value(data).unwrap();
        assert_eq!(p.supported, ["2026-07-28", "2025-11-25"]);
        assert_eq!(p.requested.as_deref(), Some("1900-01-01"));
    }
}
