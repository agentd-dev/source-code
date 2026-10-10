// SPDX-License-Identifier: AGPL-3.0-only
//! MCP Events configuration (RFC 0045 §5.2, §5.15): the **bindings** an
//! operator declares on a server — one event type, fixed arguments, appended
//! to a declared stream — and the **grants** inside which runtime
//! subscriptions are made. Both are parsed whatever the build; without the
//! `mcp-events` feature they are refused by name.
//!
//! Neither is part of a server's dial spec: editing one never re-dials the
//! server (§5.7).

use super::{Diagnostics, Dur, Settings, StoreKind, StoreOnError};
use serde::Deserialize;
use serde_json::{Map, Value};
use std::time::Duration;

/// A delivery mode an operator may list. Push arrives with the SEP's
/// confirmation of the mode (RFC 0045 P4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Delivery {
    Poll,
    Webhook,
}

impl Delivery {
    pub fn as_str(self) -> &'static str {
        match self {
            Delivery::Poll => "poll",
            Delivery::Webhook => "webhook",
        }
    }
}

/// Where a subscription's occurrences land: a declared stream and a subject,
/// the shape every other `into:` has.
#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct EventInto {
    pub stream: String,
    pub subject: String,
}

/// How far back a subscription catches up after downtime (`maxAgeMs`), or
/// `none`: always from "now".
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Replay {
    None,
    Within(Duration),
}

impl Default for Replay {
    fn default() -> Self {
        Replay::Within(Duration::from_secs(3600))
    }
}

impl<'de> Deserialize<'de> for Replay {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Raw {
            Secs(u64),
            Text(String),
        }
        match Raw::deserialize(d)? {
            Raw::Secs(s) => Ok(Replay::Within(Duration::from_secs(s))),
            Raw::Text(t) if t == "none" => Ok(Replay::None),
            Raw::Text(t) => crate::config::parse_duration(&t)
                .map(Replay::Within)
                .map_err(serde::de::Error::custom),
        }
    }
}

/// One event type on this server, subscribed with fixed arguments, every
/// occurrence appended to `into` (RFC 0045 §5.2).
#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct EventBinding {
    /// The event type, exactly as the server's `events/list` names it.
    pub name: String,
    /// The subscription arguments: part of the binding's identity.
    #[serde(default)]
    pub arguments: Map<String, Value>,
    /// The acceptable modes, in preference order. Required: each mode has a
    /// different exposure.
    pub delivery: Vec<Delivery>,
    pub into: EventInto,
    #[serde(default)]
    pub replay: Replay,
    /// The intake ceiling, `<burst>/<per>`; required on an untagged or
    /// `untrusted_input` server.
    #[serde(default)]
    pub rate: Option<String>,
    /// Webhook only: the lease to suggest (`ttlMs`); default 1h.
    #[serde(default)]
    pub ttl: Option<Dur>,
}

/// An event type the operator lets runtime subscriptions be made for — by a
/// `wait {on: event, subscribe:}` or the `events.*` tools — with what each of
/// them gets fixed (RFC 0045 §5.15).
#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct EventGrant {
    pub name: String,
    /// Arguments no runtime subscription may set; merged under the runtime
    /// ones.
    #[serde(default)]
    pub fixed: Map<String, Value>,
    pub delivery: Vec<Delivery>,
    pub into: EventInto,
    #[serde(default)]
    pub replay: Replay,
    #[serde(default)]
    pub rate: Option<String>,
    #[serde(default)]
    pub ttl: Option<Dur>,
    /// Live runtime subscriptions under this grant.
    #[serde(default = "default_max")]
    pub max: u32,
    /// The longest a tool-made subscription lives; default 1d.
    #[serde(default)]
    pub lifetime: Option<Dur>,
    /// Offer the `events.*` tools for this event type.
    #[serde(default)]
    pub tools: bool,
}

fn default_max() -> u32 {
    8
}

/// Bindings one server may declare.
pub const MAX_BINDINGS_PER_SERVER: usize = 64;
/// Subscriptions one instance holds: static bindings and live runtime
/// subscriptions together.
pub const MAX_SUBSCRIPTIONS: usize = 256;
/// The most runtime subscriptions one grant may allow.
pub const MAX_PER_GRANT: u32 = 64;
/// The longest lifetime a grant may give a tool-made subscription.
pub const MAX_LIFETIME: Duration = Duration::from_secs(30 * 24 * 3600);

/// The settings documents may not write: a served document (the agent's
/// instruction, a subagent template) declaring a binding or a grant would
/// choose an upstream subscription, open an inbound route and decide how
/// often runs start (RFC 0045 §5.11.6). `mcp` is a document's to write and
/// the classifier does not descend into arrays, so this is its own check,
/// made wherever a document's configuration folds. Each named as
/// `mcp.servers[<name>].<key>`.
pub(crate) fn document_declared(fragment: &Map<String, Value>) -> Vec<String> {
    fragment
        .get("mcp")
        .and_then(|m| m.get("servers"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .flat_map(|srv| {
            let name = srv.get("name").and_then(Value::as_str).unwrap_or("?");
            ["events", "event_grants"]
                .into_iter()
                .filter(|k| srv.get(*k).is_some())
                .map(move |k| format!("mcp.servers[{name}].{k}"))
        })
        .collect()
}

/// What a binding and a grant share, checked the same way: the modes, where
/// occurrences land, the intake ceiling.
fn check_common(
    s: &Settings,
    at: &str,
    delivery: &[Delivery],
    into: &EventInto,
    rate: Option<&str>,
    untrusted: bool,
    d: &mut Diagnostics,
) {
    if delivery.is_empty() {
        d.errors.push(format!(
            "{at}.delivery: list the acceptable modes, in preference order (poll)"
        ));
    }
    for (i, m) in delivery.iter().enumerate() {
        if delivery[..i].contains(m) {
            d.errors
                .push(format!("{at}.delivery: `{}` is listed twice", m.as_str()));
        }
    }
    if delivery.contains(&Delivery::Webhook) {
        d.errors.push(format!(
            "{at}.delivery: `webhook` is not built yet (RFC 0045 P3); list `poll`"
        ));
    }
    if into.stream.trim().is_empty() || !s.streams.contains_key(&into.stream) {
        d.errors.push(format!(
            "{at}.into.stream: '{}' is not declared under `streams:`",
            into.stream
        ));
    }
    if into.subject.trim().is_empty() {
        d.errors.push(format!(
            "{at}.into.subject: a subject is required — a stream event without one has nowhere to land"
        ));
    }
    match rate {
        Some(r) => {
            if let Err(e) = crate::supervisor::tree::parse_rate(r) {
                d.errors.push(format!("{at}.rate: {e}"));
            }
        }
        None if untrusted => d.errors.push(format!(
            "{at}.rate: required on an untagged or `untrusted_input` server — whoever can write into the watched upstream otherwise decides how often runs start"
        )),
        None => {}
    }
}

/// Every MCP Events check, collected into `d` (RFC 0045 §5.2, §5.15).
pub(crate) fn validate(s: &Settings, d: &mut Diagnostics) {
    let declared: Vec<&super::McpServer> = s
        .mcp
        .servers
        .iter()
        .filter(|srv| !srv.events.is_empty() || !srv.event_grants.is_empty())
        .collect();
    if declared.is_empty() {
        return;
    }
    if !cfg!(feature = "mcp-events") {
        for srv in &declared {
            d.errors.push(format!(
                "mcp.servers[{}]: MCP Events bindings and grants require building with --features mcp-events",
                srv.name
            ));
        }
        return;
    }
    let mut total = 0usize;
    for srv in &declared {
        let untrusted = srv.tag_set().is_ok_and(|tags| {
            tags.is_empty() || tags.contains(&crate::sec::scope::TrifectaTag::UntrustedInput)
        });
        if srv.events.len() > MAX_BINDINGS_PER_SERVER {
            d.errors.push(format!(
                "mcp.servers[{}].events: {} bindings, at most {MAX_BINDINGS_PER_SERVER} per server",
                srv.name,
                srv.events.len()
            ));
        }
        total += srv.events.len();
        let mut seen = std::collections::BTreeSet::new();
        for b in &srv.events {
            let at = format!("mcp.servers[{}].events[{}]", srv.name, b.name);
            if b.name.trim().is_empty() {
                d.errors.push(format!(
                    "mcp.servers[{}].events[]: a binding has an empty name",
                    srv.name
                ));
            }
            let identity = (
                b.name.clone(),
                crate::engine::model::canonical(&Value::Object(b.arguments.clone())),
            );
            if !seen.insert(identity) {
                d.errors
                    .push(format!("{at}: declared twice with the same arguments"));
            }
            check_common(
                s,
                &at,
                &b.delivery,
                &b.into,
                b.rate.as_deref(),
                untrusted,
                d,
            );
        }
        let mut granted = std::collections::BTreeSet::new();
        for g in &srv.event_grants {
            let at = format!("mcp.servers[{}].event_grants[{}]", srv.name, g.name);
            if g.name.trim().is_empty() {
                d.errors.push(format!(
                    "mcp.servers[{}].event_grants[]: a grant has an empty name",
                    srv.name
                ));
            }
            if !granted.insert(g.name.clone()) {
                d.errors.push(format!("{at}: one grant per event type"));
            }
            if g.max == 0 || g.max > MAX_PER_GRANT {
                d.errors.push(format!(
                    "{at}.max: {} — between 1 and {MAX_PER_GRANT}",
                    g.max
                ));
            }
            if g.lifetime.as_ref().is_some_and(|l| l.0 > MAX_LIFETIME) {
                d.errors.push(format!("{at}.lifetime: at most 30d"));
            }
            check_common(
                s,
                &at,
                &g.delivery,
                &g.into,
                g.rate.as_deref(),
                untrusted,
                d,
            );
        }
    }
    if total > MAX_SUBSCRIPTIONS {
        d.errors.push(format!(
            "mcp.servers[].events: {total} bindings, at most {MAX_SUBSCRIPTIONS} per instance"
        ));
    }
    d.warnings.push(
        "MCP Events tracks SEP-3415, a draft: its methods, codes and capability may change between releases"
            .into(),
    );
    let weak = match (&s.store.kind, &s.store.on_error) {
        (StoreKind::Memory, _) => {
            Some("the store is the memory store, so a binding's cursor does not survive a restart")
        }
        (_, StoreOnError::Degrade) => Some(
            "store.on_error is degrade, so a write that failed reads as kept and a binding's cursor can move past an occurrence that was never stored",
        ),
        _ => None,
    };
    if let Some(why) = weak {
        d.warnings.push(format!(
            "mcp.servers[].events: {why} (RFC 0045 §5.1); use a persistent store with on_error: halt"
        ));
    }
}
