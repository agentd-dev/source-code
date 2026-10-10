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

/// An event type the instruction's own server offers whose occurrences say
/// the instruction may have changed (`agent.instruction.mcp.events`): each
/// is polled for, and an occurrence re-reads the instruction — beside the
/// resource subscription and the `refresh` timer, never instead of them.
/// The type's name alone, or `{name, arguments}`.
#[derive(Debug, Clone, PartialEq)]
pub struct InstructionEvent {
    /// The event type, exactly as the server's `events/list` names it.
    pub name: String,
    /// The subscription arguments: part of the binding's identity.
    pub arguments: Map<String, Value>,
}

impl<'de> Deserialize<'de> for InstructionEvent {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Full {
            name: String,
            #[serde(default)]
            arguments: Map<String, Value>,
        }
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Raw {
            Name(String),
            Full(Full),
        }
        Ok(match Raw::deserialize(d)? {
            Raw::Name(name) => InstructionEvent {
                name,
                arguments: Map::new(),
            },
            Raw::Full(f) => InstructionEvent {
                name: f.name,
                arguments: f.arguments,
            },
        })
    }
}

/// Said at load wherever MCP Events is configured.
const DRAFT_WARNING: &str = "MCP Events tracks SEP-3415, a draft: its methods, codes and capability may change between releases";

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
            "{at}.delivery: list the acceptable modes, in preference order (poll, webhook)"
        ));
    }
    for (i, m) in delivery.iter().enumerate() {
        if delivery[..i].contains(m) {
            d.errors
                .push(format!("{at}.delivery: `{}` is listed twice", m.as_str()));
        }
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

/// What `webhook` delivery needs (RFC 0045 §5.2), for each binding or grant
/// that lists it: the listener it is delivered to (the `a2a` feature,
/// `webhooks.listen`, no client certificate asked — no server presents one),
/// a `webhooks.url` the server can deliver to (`https://` [SEP "TLS
/// requirement"]), and a credential on the server, because a webhook
/// subscription is keyed by an authenticated principal [SEP "Subscription
/// identity"].
fn check_webhook(s: &Settings, modes: &[(String, &super::McpServer)], d: &mut Diagnostics) {
    if modes.is_empty() {
        return;
    }
    let first = &modes[0].0;
    if !cfg!(feature = "a2a") {
        d.errors.push(format!(
            "{first}.delivery: `webhook` needs the webhook listener, which requires building with --features a2a"
        ));
    }
    if s.webhooks.listen.is_none() {
        d.errors.push(format!(
            "{first}.delivery: `webhook` needs the webhook listener — set webhooks.listen"
        ));
    }
    if s.webhooks.tls.client_ca.is_some() {
        d.errors.push(format!(
            "{first}.delivery: `webhook` cannot be delivered to a listener that asks for a client certificate (webhooks.tls.client_ca): no MCP server presents one"
        ));
    }
    match s.webhooks.url.as_deref() {
        None => d.errors.push(format!(
            "{first}.delivery: `webhook` needs webhooks.url — the https:// URL the server delivers to"
        )),
        Some(u) => match super::validate_webhooks_url(u) {
            Ok(o) if o.scheme != "https" => d.errors.push(format!(
                "{first}.delivery: `webhook` needs an https:// webhooks.url — callback URLs must use https [SEP-3415]"
            )),
            Ok(o) if !globally_routable(&o.host) => d.warnings.push(format!(
                "webhooks.url {u:?} is not a globally routable address: a server delivering webhooks may refuse it unless it is configured to allow it"
            )),
            _ => {} // a bad URL is reported with webhooks.url itself
        },
    }
    for (at, srv) in modes {
        let has = |auth: bool, headers: &std::collections::BTreeMap<String, String>| {
            auth || headers
                .keys()
                .any(|k| k.eq_ignore_ascii_case("authorization"))
        };
        let own = has(
            srv.auth.is_some() || srv.oauth.is_some() || srv.aauth == Some(true),
            &srv.headers,
        );
        let catalogued = srv
            .service
            .as_ref()
            .and_then(|n| s.services.get(n))
            .is_some_and(|svc| has(svc.auth.is_some(), &svc.headers));
        if !own && !catalogued {
            d.errors.push(format!(
                "{at}.delivery: `webhook` needs a credential on mcp server {:?} (auth, oauth, aauth, an Authorization header, or a catalogued service with one) — a webhook subscription is keyed by the authenticated principal",
                srv.name
            ));
        }
    }
}

/// Whether `host` may be a globally routable address: a name (DNS decides),
/// or a literal outside loopback, private, link-local and the unspecified
/// ranges.
fn globally_routable(host: &str) -> bool {
    use std::net::IpAddr;
    if crate::net::http::is_loopback_host(host) {
        return false;
    }
    match host.trim_matches(['[', ']']).parse::<IpAddr>() {
        Ok(IpAddr::V4(v4)) => {
            !(v4.is_private() || v4.is_link_local() || v4.is_unspecified() || v4.is_loopback())
        }
        Ok(IpAddr::V6(v6)) => {
            let seg = v6.segments()[0];
            !(v6.is_loopback()
                || v6.is_unspecified()
                || (seg & 0xfe00) == 0xfc00
                || (seg & 0xffc0) == 0xfe80)
        }
        Err(_) => true,
    }
}

/// `replay` and `ttl`: a catch-up bound under a second asks the server for
/// nothing between polls — `none` says that plainly — and `ttl` is the
/// webhook lease, which a binding without `webhook` has none of.
fn check_leases(
    at: &str,
    delivery: &[Delivery],
    replay: Replay,
    ttl: Option<&Dur>,
    d: &mut Diagnostics,
) {
    if let Replay::Within(w) = replay
        && w < Duration::from_secs(1)
    {
        d.errors.push(format!(
            "{at}.replay: under a second replays nothing — write `none` to always start from now"
        ));
    }
    if ttl.is_some() && !delivery.contains(&Delivery::Webhook) {
        d.errors.push(format!(
            "{at}.ttl: the webhook lease, and this binding lists no `webhook` delivery"
        ));
    }
}

/// The waits of `w` that subscribe under no grant: each names a server and an
/// event type with no `event_grants` entry. Checked wherever a definition is
/// — load, reload, a stored one, `workflow.create`/`update` — so a document
/// or the agent can use only what the operator granted (RFC 0045 §5.15).
pub(crate) fn subscribe_refusals(s: &Settings, w: &crate::engine::model::Workflow) -> Vec<String> {
    use crate::engine::model::Step;
    // Every step, nested bodies included: a wait inside a loop or a branch
    // subscribes like one at the top.
    fn walk<'a>(steps: impl Iterator<Item = &'a Step>, out: &mut Vec<&'a Step>) {
        for st in steps {
            out.push(st);
            for body in st.body.iter().chain(st.branches.values()) {
                walk(body.steps.values(), out);
            }
        }
    }
    let mut steps = Vec::new();
    walk(w.steps.values(), &mut steps);
    let mut out = Vec::new();
    for st in steps {
        if st.kind != "wait" {
            continue;
        }
        let Some(sub) = st.spec.get("subscribe") else {
            continue;
        };
        let (Some(server), Some(event)) = (
            sub.get("server").and_then(Value::as_str),
            sub.get("event").and_then(Value::as_str),
        ) else {
            continue; // the model check names a missing one
        };
        let granted = s
            .mcp
            .servers
            .iter()
            .find(|x| x.name == server)
            .is_some_and(|x| x.event_grants.iter().any(|g| g.name == event));
        if !granted {
            out.push(format!(
                "workflow {:?} step {:?}: subscribes to {event:?} on mcp server {server:?}, which no `event_grants` entry grants — runtime subscriptions are made only inside an operator grant",
                w.name, st.id
            ));
        } else if !cfg!(feature = "mcp-events") {
            out.push(format!(
                "workflow {:?} step {:?}: a wait that subscribes requires building with --features mcp-events",
                w.name, st.id
            ));
        }
    }
    out
}

/// `agent.instruction.mcp.events`: each a named type, listed once. Polled
/// only — a webhook needs a credential and a route checked against a server,
/// and an instruction that names none is served by whichever answers; one
/// re-read a second is all an occurrence can bring sooner anyway.
/// How many it holds, toward the instance's cap.
fn check_instruction(s: &Settings, d: &mut Diagnostics) -> usize {
    let events = s
        .agent
        .instruction_spec
        .mcp
        .as_ref()
        .map_or(&[][..], |m| m.events());
    if events.is_empty() {
        return 0;
    }
    if !cfg!(feature = "mcp-events") {
        d.errors.push(
            "agent.instruction.mcp.events: following the instruction by MCP Events requires building with --features mcp-events"
                .into(),
        );
        return 0;
    }
    if events.len() > MAX_BINDINGS_PER_SERVER {
        d.errors.push(format!(
            "agent.instruction.mcp.events: {} event types, at most {MAX_BINDINGS_PER_SERVER}",
            events.len()
        ));
    }
    for (i, e) in events.iter().enumerate() {
        if e.name.trim().is_empty() {
            d.errors.push(format!(
                "agent.instruction.mcp.events[{i}]: an event type needs a name"
            ));
        } else if events[..i].contains(e) {
            d.errors.push(format!(
                "agent.instruction.mcp.events[{}]: listed twice with the same arguments",
                e.name
            ));
        }
    }
    events.len()
}

/// Every MCP Events check, collected into `d` (RFC 0045 §5.2, §5.15).
pub(crate) fn validate(s: &Settings, d: &mut Diagnostics) {
    let followed = check_instruction(s, d);
    let declared: Vec<&super::McpServer> = s
        .mcp
        .servers
        .iter()
        .filter(|srv| !srv.events.is_empty() || !srv.event_grants.is_empty())
        .collect();
    if declared.is_empty() {
        if followed > 0 {
            d.warnings.push(DRAFT_WARNING.into());
        }
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
    let mut total = followed;
    let mut webhook_modes: Vec<(String, &super::McpServer)> = Vec::new();
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
            check_leases(&at, &b.delivery, b.replay, b.ttl.as_ref(), d);
            if b.delivery.contains(&Delivery::Webhook) {
                webhook_modes.push((at.clone(), srv));
            }
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
            check_leases(&at, &g.delivery, g.replay, g.ttl.as_ref(), d);
            if g.delivery.contains(&Delivery::Webhook) {
                webhook_modes.push((at.clone(), srv));
            }
        }
    }
    check_webhook(s, &webhook_modes, d);
    if total > MAX_SUBSCRIPTIONS {
        d.errors.push(format!(
            "mcp.servers[].events (with agent.instruction.mcp.events): {total} bindings, at most {MAX_SUBSCRIPTIONS} per instance"
        ));
    }
    d.warnings.push(DRAFT_WARNING.into());
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
