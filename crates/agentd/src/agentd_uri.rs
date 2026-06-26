//! The `agentd://` resource scheme — agentd exposing its own state as MCP
//! resources. RFC 0004 §custom-scheme, RFC 0009 §async (completion-as-resource).
//!
//! Only agentd (and a peer agentd) understands these URIs; to a generic MCP
//! client they are opaque readable resources. Two surfaces use them:
//!   * **within a subagent** — `agentd://subagent/<handle>` reads the status /
//!     distilled result of an async child spawned via `subagent.spawn{async}`
//!     (completion-as-self-resource);
//!   * **served** — a peer reads `agentd://status` (this agentd's run/health
//!     state) over the served self-MCP.

/// The scheme prefix.
pub const SCHEME: &str = "agentd://";

/// Whether `uri` is an `agentd://` resource — so it routes to agentd's own
/// resource backends rather than an MCP server.
pub fn is_agentd(uri: &str) -> bool {
    uri.trim().starts_with(SCHEME)
}

/// A parsed `agentd://` resource address.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentdResource {
    /// `agentd://status` — this agentd's own run/health state.
    Status,
    /// `agentd://subagent/<handle>` — an async child's status / distilled result.
    Subagent(String),
}

impl AgentdResource {
    /// Parse an `agentd://` URI. `None` for a non-agentd scheme or an
    /// unrecognized/empty path.
    pub fn parse(uri: &str) -> Option<AgentdResource> {
        let rest = uri.trim().strip_prefix(SCHEME)?.trim_end_matches('/');
        if rest == "status" {
            return Some(AgentdResource::Status);
        }
        let handle = rest.strip_prefix("subagent/")?.trim();
        if handle.is_empty() {
            return None;
        }
        Some(AgentdResource::Subagent(handle.to_string()))
    }
}

/// The `agentd://subagent/<handle>` URI for an async child's completion.
pub fn subagent_uri(handle: &str) -> String {
    format!("{SCHEME}subagent/{handle}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_the_scheme() {
        assert!(is_agentd("agentd://status"));
        assert!(is_agentd("  agentd://subagent/0.1  "));
        assert!(!is_agentd("file:///x"));
        assert!(!is_agentd("agentdx://y"));
    }

    #[test]
    fn parses_status_and_subagent() {
        assert_eq!(AgentdResource::parse("agentd://status"), Some(AgentdResource::Status));
        assert_eq!(AgentdResource::parse("agentd://status/"), Some(AgentdResource::Status));
        assert_eq!(AgentdResource::parse("agentd://subagent/0.1"), Some(AgentdResource::Subagent("0.1".into())));
        assert_eq!(subagent_uri("0.2"), "agentd://subagent/0.2");
    }

    #[test]
    fn rejects_unknown_and_foreign() {
        assert_eq!(AgentdResource::parse("agentd://nope"), None);
        assert_eq!(AgentdResource::parse("agentd://subagent/"), None);
        assert_eq!(AgentdResource::parse("file:///x"), None);
    }
}
