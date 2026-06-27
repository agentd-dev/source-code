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
    /// `agentd://capabilities` — this agentd's self-description manifest
    /// (identity, declared capability surface, live counters). RFC 0015 §3.4.
    Capabilities,
    /// `agentd://inventory` — the live subagent-tree projection (lifecycle flags,
    /// totals, per-node status/usage). Management-only, subscribable. RFC 0015 §5.3.
    Inventory,
    /// `agentd://subagent/<handle>` — an async child's status / distilled result.
    Subagent(String),
    /// `agentd://run/<run_id>` — the served run's aggregate (mode, spawn counts,
    /// uptime). Subscribable; fires on each spawn / terminal-run transition.
    Run(String),
    /// `agentd://session/<handle>` — a served warm session's turn state.
    /// Subscribable; fires on each warm-turn boundary.
    Session(String),
    /// `agentd://events[?after=<seq>&level=<lvl>&event=<prefixes>]` — the
    /// bounded live-event ring (RFC 0016 §7). Subscribable; fires on each new
    /// event (notify-then-read). The cursor + filters ride the query string.
    Events(EventsQuery),
}

/// The parsed query of an `agentd://events?…` read (RFC 0016 §7.2/§7.3). All
/// fields are optional; an empty query is "the whole window from seq 0".
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct EventsQuery {
    /// Return only entries with `seq > after` — the cursor the subscriber
    /// advances to the last `seq` it saw. `0` (default) ⇒ the whole window.
    pub after: u64,
    /// Server-side level filter (`?level=warn`): exact match on the line `level`.
    pub level: Option<String>,
    /// Server-side event-prefix filter (`?event=subagent.,limit.`): a comma-list
    /// of dotted prefixes — an entry matches if its `event` starts with any.
    pub event_prefixes: Vec<String>,
}

impl EventsQuery {
    /// Parse the `key=value&key=value` query after the `?`. Unknown keys are
    /// ignored (forward-compatible); a malformed `after` falls back to `0` (the
    /// safe full-window default — a cursor read never errors on a bad number).
    fn parse(query: &str) -> EventsQuery {
        let mut q = EventsQuery::default();
        for pair in query.split('&') {
            let Some((k, v)) = pair.split_once('=') else {
                continue;
            };
            match k {
                "after" => q.after = v.trim().parse().unwrap_or(0),
                "level" => {
                    let v = v.trim();
                    if !v.is_empty() {
                        q.level = Some(v.to_string());
                    }
                }
                "event" => {
                    q.event_prefixes = v
                        .split(',')
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                        .map(str::to_string)
                        .collect();
                }
                _ => {} // unknown key — ignore (forward-compatible)
            }
        }
        q
    }
}

impl AgentdResource {
    /// Parse an `agentd://` URI. `None` for a non-agentd scheme or an
    /// unrecognized/empty path.
    pub fn parse(uri: &str) -> Option<AgentdResource> {
        let rest = uri.trim().strip_prefix(SCHEME)?;
        // Split the optional `?query` off the path (only `agentd://events` uses
        // one today — the cursor/filters, RFC 0016 §7). Path-only resources see
        // identical behaviour: their `rest` has no `?`, so `path == rest`.
        let (path, query) = match rest.split_once('?') {
            Some((p, q)) => (p, Some(q)),
            None => (rest, None),
        };
        let path = path.trim_end_matches('/');
        if path == "events" {
            return Some(AgentdResource::Events(
                query.map(EventsQuery::parse).unwrap_or_default(),
            ));
        }
        let rest = path;
        if rest == "status" {
            return Some(AgentdResource::Status);
        }
        if rest == "capabilities" {
            return Some(AgentdResource::Capabilities);
        }
        if rest == "inventory" {
            return Some(AgentdResource::Inventory);
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

/// The `agentd://inventory` URI — the live subagent-tree projection (RFC 0015 §5.3).
pub const INVENTORY_URI: &str = "agentd://inventory";

/// The `agentd://events` URI — the bounded live-event ring (RFC 0016 §7). The
/// bare base URI (subscribe/list/notify use it); a read appends `?after=<seq>`
/// and the optional `?level=`/`?event=` filters.
pub const EVENTS_URI: &str = "agentd://events";

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
    fn parses_capabilities() {
        assert_eq!(
            AgentdResource::parse("agentd://capabilities"),
            Some(AgentdResource::Capabilities)
        );
        assert_eq!(
            AgentdResource::parse("agentd://capabilities/"),
            Some(AgentdResource::Capabilities)
        );
    }

    #[test]
    fn parses_inventory() {
        assert_eq!(
            AgentdResource::parse(INVENTORY_URI),
            Some(AgentdResource::Inventory)
        );
        assert_eq!(
            AgentdResource::parse("agentd://inventory/"),
            Some(AgentdResource::Inventory)
        );
    }

    #[test]
    fn parses_events_base_and_query() {
        // Bare base URI → an empty (default) query: whole window from seq 0.
        assert_eq!(
            AgentdResource::parse(EVENTS_URI),
            Some(AgentdResource::Events(EventsQuery::default()))
        );
        assert_eq!(
            AgentdResource::parse("agentd://events/"),
            Some(AgentdResource::Events(EventsQuery::default()))
        );
        // The cursor + filters ride the query string (RFC 0016 §7.2/§7.3).
        let parsed =
            AgentdResource::parse("agentd://events?after=4821&level=warn&event=subagent.,limit.");
        assert_eq!(
            parsed,
            Some(AgentdResource::Events(EventsQuery {
                after: 4821,
                level: Some("warn".into()),
                event_prefixes: vec!["subagent.".into(), "limit.".into()],
            }))
        );
        // A malformed cursor falls back to the safe full-window default (0).
        assert_eq!(
            AgentdResource::parse("agentd://events?after=notanumber"),
            Some(AgentdResource::Events(EventsQuery::default()))
        );
        // An unknown query key is ignored (forward-compatible).
        assert_eq!(
            AgentdResource::parse("agentd://events?nonsense=1&after=7"),
            Some(AgentdResource::Events(EventsQuery {
                after: 7,
                ..EventsQuery::default()
            }))
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
