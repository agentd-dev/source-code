// SPDX-License-Identifier: Apache-2.0
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

/// The neutral (canonical) scheme prefix the agent EMITS (`agent://`). The whole
/// runtime is de-branded to the neutral spelling; the legacy `agentd://` form is
/// still ACCEPTED on input ([`LEGACY_SCHEME`]) but never emitted.
pub const SCHEME: &str = "agent://";

/// The legacy branded scheme prefix (`agentd://`) ACCEPTED as an input alias but
/// no longer emitted (ACC SPEC L4 — neutral is canonical). A consumer addressing
/// the old `agentd://…` spelling is still honoured on reads.
pub const LEGACY_SCHEME: &str = "agentd://";

/// Whether `uri` is one of the agent's own resources — so it routes to the agent's
/// own resource backends rather than an MCP server. Accepts EITHER the neutral
/// `agent://` (emitted) or the legacy `agentd://` prefix (ACC SPEC L4; the two
/// prefixes are mutually exclusive — `agentd://x` does not start with `agent://`).
pub fn is_agentd(uri: &str) -> bool {
    let uri = uri.trim();
    uri.starts_with(SCHEME) || uri.starts_with(LEGACY_SCHEME)
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
    /// `agentd://intelligence` — the live intelligence-endpoint health view (RFC
    /// 0018 §4.4): the endpoint list (transport + index, NEVER the URL/creds),
    /// which is active, and each one's health (up/broken/last-latency).
    /// Management-only, subscribable; fires on breaker/active/all-down transitions.
    Intelligence,
    /// `agentd://capacity` — the live capacity/placement view (RFC 0019 §7.2/§9):
    /// instance identity, shard `K/N`, free slots, active subagents, intelligence
    /// warmth/health, and saturation. Management-only. The read surface agentctl
    /// uses to place work. Present only in `cluster` builds.
    Capacity,
    /// `agentd://config/effective` — the live, redacted view of the running
    /// daemon's RELOADABLE config subset (RFC 0017 §4.2 / §5.6): model, limits,
    /// log level, subscribe set, structural MCP-server names, and intelligence
    /// header NAMES — NEVER a token / URL / secret value. Management-only,
    /// subscribable; fires `resources/updated` on each APPLIED hot reload so a
    /// subscriber re-reads the post-reload view. Served only with `serve-mcp`.
    ConfigEffective,
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
        // Accept EITHER the branded `agentd://` or the neutral `agent://` prefix
        // (ACC SPEC L4 de-branding); the rest of the parse is scheme-agnostic.
        // Branded is tried first (it is the emitted alias); the prefixes are
        // mutually exclusive so the order is immaterial for correctness.
        let trimmed = uri.trim();
        let rest = trimmed
            .strip_prefix(SCHEME)
            .or_else(|| trimmed.strip_prefix(LEGACY_SCHEME))?;
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
        if rest == "intelligence" {
            return Some(AgentdResource::Intelligence);
        }
        if rest == "capacity" {
            return Some(AgentdResource::Capacity);
        }
        if rest == "config/effective" {
            return Some(AgentdResource::ConfigEffective);
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
pub const INVENTORY_URI: &str = "agent://inventory";

/// The `agentd://intelligence` URI — the live intelligence-endpoint health view
/// (RFC 0018 §4.4). Management-only; subscribable.
pub const INTELLIGENCE_URI: &str = "agent://intelligence";

/// The `agentd://capacity` URI — the live capacity/placement view (RFC 0019
/// §7.2/§9). Management-only; present only in `cluster` builds.
pub const CAPACITY_URI: &str = "agent://capacity";

/// The `agentd://config/effective` URI — the live, redacted reloadable-config
/// view (RFC 0017 §4.2 / §5.6). Management-only; subscribable; fires
/// `resources/updated` on each applied hot reload. Served only with `serve-mcp`.
pub const CONFIG_EFFECTIVE_URI: &str = "agent://config/effective";

/// The `agentd://events` URI — the bounded live-event ring (RFC 0016 §7). The
/// bare base URI (subscribe/list/notify use it); a read appends `?after=<seq>`
/// and the optional `?level=`/`?event=` filters.
pub const EVENTS_URI: &str = "agent://events";

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
        assert_eq!(subagent_uri("0.2"), "agent://subagent/0.2");
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
        assert_eq!(run_uri("r-7"), "agent://run/r-7");
        assert_eq!(session_uri("served.3"), "agent://session/served.3");
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
    fn parses_intelligence() {
        assert_eq!(
            AgentdResource::parse(INTELLIGENCE_URI),
            Some(AgentdResource::Intelligence)
        );
        assert_eq!(
            AgentdResource::parse("agentd://intelligence/"),
            Some(AgentdResource::Intelligence)
        );
    }

    #[test]
    fn parses_capacity() {
        assert_eq!(
            AgentdResource::parse(CAPACITY_URI),
            Some(AgentdResource::Capacity)
        );
        assert_eq!(
            AgentdResource::parse("agentd://capacity/"),
            Some(AgentdResource::Capacity)
        );
    }

    #[test]
    fn parses_config_effective() {
        assert_eq!(
            AgentdResource::parse(CONFIG_EFFECTIVE_URI),
            Some(AgentdResource::ConfigEffective)
        );
        // a trailing slash is trimmed like the other arms
        assert_eq!(
            AgentdResource::parse("agentd://config/effective/"),
            Some(AgentdResource::ConfigEffective)
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

    // --- ACC SPEC L4 de-branding: the neutral `agent://` prefix is also accepted
    // on input (branded `agentd://` stays accepted + emitted). -----------------

    #[test]
    fn detects_the_neutral_scheme_too() {
        // Neutral `agent://` is accepted alongside the branded `agentd://`.
        assert!(is_agentd("agent://status"));
        assert!(is_agentd("  agent://events  "));
        assert!(is_agentd("agentd://status"));
        // A foreign scheme that merely shares a prefix is not ours.
        assert!(!is_agentd("agentx://y"));
        assert!(!is_agentd("agent:/status"));
    }

    #[test]
    fn parses_the_neutral_scheme_same_as_branded() {
        // The neutral spelling resolves to the SAME resource as the branded one.
        assert_eq!(
            AgentdResource::parse("agent://status"),
            Some(AgentdResource::Status)
        );
        assert_eq!(
            AgentdResource::parse("agent://capabilities"),
            Some(AgentdResource::Capabilities)
        );
        assert_eq!(
            AgentdResource::parse("agent://capabilities/"),
            Some(AgentdResource::Capabilities)
        );
        // events: the bare base and the cursor/filter query both parse.
        assert_eq!(
            AgentdResource::parse("agent://events"),
            Some(AgentdResource::Events(EventsQuery::default()))
        );
        assert_eq!(
            AgentdResource::parse("agent://events?after=9&level=warn"),
            Some(AgentdResource::Events(EventsQuery {
                after: 9,
                level: Some("warn".into()),
                event_prefixes: vec![],
            }))
        );
        // path-bearing arms strip the neutral prefix identically.
        assert_eq!(
            AgentdResource::parse("agent://run/r-7"),
            Some(AgentdResource::Run("r-7".into()))
        );
        assert_eq!(
            AgentdResource::parse("agent://subagent/0.1"),
            Some(AgentdResource::Subagent("0.1".into()))
        );
        // and the branded spelling still parses (never dropped).
        assert_eq!(
            AgentdResource::parse("agentd://status"),
            Some(AgentdResource::Status)
        );
    }
}
