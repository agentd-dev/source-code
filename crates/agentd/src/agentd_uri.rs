//! The `agentd://` resource scheme — agentd exposing its own state as MCP
//! resources. RFC 0004 §custom-scheme, RFC 0009 §async (completion-as-resource).
//!
//! Only agentd (and a peer agentd) understands these URIs; to a generic MCP
//! client they are opaque readable resources. Two surfaces use them:
//!   * **within a subagent** — `agentd://subagent/<handle>` reads the status /
//!     distilled result of an async child spawned via `subagent.spawn{async}`
//!     (completion-as-self-resource);
//!   * **served** — a peer reads `agentd://status` (this agentd's run/health
//!     state), `agentd://run/<run_id>` (the run aggregate), and
//!     `agentd://session/<handle>` (a warm session's turn state) over the served
//!     self-MCP — the run/session resources are also subscribable and fire
//!     `notifications/resources/updated` repeatedly (RFC 0005 §3.3/§3.4).

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
    /// `agentd://run/<run_id>` — the served run's aggregate (mode, spawn counts,
    /// uptime). Subscribable; fires on each spawn / terminal-run transition.
    Run(String),
    /// `agentd://session/<handle>` — a served warm session's turn state.
    /// Subscribable; fires on each warm-turn boundary.
    Session(String),
}

impl AgentdResource {
    /// Parse an `agentd://` URI. `None` for a non-agentd scheme or an
    /// unrecognized/empty path.
    pub fn parse(uri: &str) -> Option<AgentdResource> {
        let rest = uri.trim().strip_prefix(SCHEME)?.trim_end_matches('/');
        if rest == "status" {
            return Some(AgentdResource::Status);
        }
        if let Some(handle) = rest.strip_prefix("subagent/") {
            let handle = handle.trim();
            if handle.is_empty() {
                return None;
            }
            return Some(AgentdResource::Subagent(handle.to_string()));
        }
        if let Some(id) = rest.strip_prefix("run/") {
            let id = id.trim();
            if id.is_empty() {
                return None;
            }
            return Some(AgentdResource::Run(id.to_string()));
        }
        if let Some(handle) = rest.strip_prefix("session/") {
            let handle = handle.trim();
            if handle.is_empty() {
                return None;
            }
            return Some(AgentdResource::Session(handle.to_string()));
        }
        None
    }
}

/// The `agentd://subagent/<handle>` URI for an async child's completion.
pub fn subagent_uri(handle: &str) -> String {
    format!("{SCHEME}subagent/{handle}")
}

/// The `agentd://run/<run_id>` URI for the served run's aggregate.
pub fn run_uri(run_id: &str) -> String {
    format!("{SCHEME}run/{run_id}")
}

/// The `agentd://session/<handle>` URI for a served warm session's turn state.
pub fn session_uri(handle: &str) -> String {
    format!("{SCHEME}session/{handle}")
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
        assert_eq!(
            AgentdResource::parse("agentd://status"),
            Some(AgentdResource::Status)
        );
        assert_eq!(
            AgentdResource::parse("agentd://status/"),
            Some(AgentdResource::Status)
        );
        assert_eq!(
            AgentdResource::parse("agentd://subagent/0.1"),
            Some(AgentdResource::Subagent("0.1".into()))
        );
        assert_eq!(subagent_uri("0.2"), "agentd://subagent/0.2");
    }

    #[test]
    fn parses_run_and_session() {
        assert_eq!(
            AgentdResource::parse("agentd://run/r-7"),
            Some(AgentdResource::Run("r-7".into()))
        );
        // a trailing slash is trimmed like the other arms
        assert_eq!(
            AgentdResource::parse("agentd://run/r-7/"),
            Some(AgentdResource::Run("r-7".into()))
        );
        assert_eq!(
            AgentdResource::parse("agentd://session/served.3"),
            Some(AgentdResource::Session("served.3".into()))
        );
        // builders round-trip back through parse
        assert_eq!(run_uri("r-7"), "agentd://run/r-7");
        assert_eq!(session_uri("served.3"), "agentd://session/served.3");
        assert_eq!(
            AgentdResource::parse(&run_uri("r-7")),
            Some(AgentdResource::Run("r-7".into()))
        );
        assert_eq!(
            AgentdResource::parse(&session_uri("served.3")),
            Some(AgentdResource::Session("served.3".into()))
        );
    }

    #[test]
    fn rejects_unknown_and_foreign() {
        assert_eq!(AgentdResource::parse("agentd://nope"), None);
        assert_eq!(AgentdResource::parse("agentd://subagent/"), None);
        assert_eq!(AgentdResource::parse("agentd://run/"), None);
        assert_eq!(AgentdResource::parse("agentd://session/"), None);
        assert_eq!(AgentdResource::parse("file:///x"), None);
    }
}
