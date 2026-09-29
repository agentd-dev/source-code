// SPDX-License-Identifier: AGPL-3.0-only
//! The extension registry: every extension agentd declares, the methods and
//! the core methods each one applies to, and the `A2A-Extensions` negotiation
//! that activates them — per request, and only by the header.

use serde_json::{Value, json};

use super::methods::{Route, SpecMethod};
use super::ops::{Floor, Reply, op_spec, static_vocabulary};
use crate::a2a::errors::{self, reason};
use crate::config::settings::Settings;

// ── A2A extensions (spec: docs/a2a-extensions.md) ───────────────────────────
//
// Anything agentd speaks beyond the core protocol is declared as an
// `AgentExtension` on the card, identified by a URI. The URIs carry no
// version: A2A 1.0.1 §4.6.3 only suggests one, and a version in an
// identifier agentd owns is a second name for the same thing. An
// incompatible change takes a NEW URI under a new name — never a `/vN`
// suffix — because §4.6.3/§5.8 say a URI's meaning MUST NOT change under a
// peer that already speaks it. Each URI is an exact identifier: a peer is
// not expected to fetch it, and neither case nor a trailing slash is forgiven.

/// Structured operations invoked as a DataPart on `SendMessage`. A profile
/// extension: it adds no method and changes no core structure, so it is never
/// `required` and a client that ignores it can still converse.
pub const COMMAND_EXTENSION: &str = "https://agentd.dev/a2a/ext/command";
/// The observation feed, as an A2A method extension.
pub const EVENTS_EXTENSION: &str = "https://agentd.dev/a2a/ext/events";
/// The method [`EVENTS_EXTENSION`] declares, namespaced so it can never collide
/// with a method the specification defines later.
pub const EVENTS_METHOD: &str = "agentd.events/SubscribeToEvents";
/// The extension that declares agentd's own annotations on a task, so a peer
/// finds them under a URI it can look up rather than in ad-hoc metadata keys.
pub const TASK_ANNOTATIONS_EXTENSION: &str = "https://agentd.dev/a2a/ext/task-annotations";
/// The protocol binding a unix-socket interface declares on the card: JSON-RPC,
/// but over a path no `https://` URL can name.
pub const UNIX_BINDING: &str = "https://agentd.dev/a2a/binding/jsonrpc-unix";

/// The most `A2A-Extensions` tokens one request has considered. agentd
/// declares three; a header naming more than this is not a client asking for
/// what it speaks, and the listener does not spend a lookup on each.
pub const MAX_EXTENSION_TOKENS: usize = 32;
/// The longest token considered. A URI is an identifier, not a payload.
pub const MAX_EXTENSION_TOKEN_BYTES: usize = 512;

/// One extension agentd can declare. The registry: every URI, every method
/// and every rule about when an extension is in force is read from here, so
/// the card, the manifest, the route table and the negotiation cannot
/// disagree about any of them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ext {
    Command,
    Events,
    TaskAnnotations,
}

impl Ext {
    /// Every extension, in registry order — the order the card declares them
    /// in and the echo lists them in.
    pub const ALL: &[Ext] = &[Ext::Command, Ext::Events, Ext::TaskAnnotations];

    /// The extension's URI. One exhaustive match, so a variant cannot be
    /// added without one.
    pub fn uri(self) -> &'static str {
        match self {
            Ext::Command => COMMAND_EXTENSION,
            Ext::Events => EVENTS_EXTENSION,
            Ext::TaskAnnotations => TASK_ANNOTATIONS_EXTENSION,
        }
    }

    /// Whether the extension means anything on `route`. An extension a
    /// request names on a method it does not apply to is not activated
    /// there, so the echo never claims a behaviour the answer did not have.
    pub fn applies_to(self, route: Route) -> bool {
        // An extension applies to every method it declares.
        if let Route::Extension { ext_method } = route
            && owner_of(ext_method) == Some(self)
        {
            return true;
        }
        match self {
            Ext::Command => matches!(
                route,
                Route::Spec(SpecMethod::SendMessage | SpecMethod::SendStreamingMessage)
            ),
            Ext::Events => false,
            // Every answer that carries a Task: the sends, the task reads and
            // the cancel, a task stream, and the feed's `task` events.
            Ext::TaskAnnotations => {
                matches!(
                    route,
                    Route::Spec(
                        SpecMethod::SendMessage
                            | SpecMethod::SendStreamingMessage
                            | SpecMethod::GetTask
                            | SpecMethod::ListTasks
                            | SpecMethod::CancelTask
                            | SpecMethod::SubscribeToTask
                    )
                ) || route
                    == Route::Extension {
                        ext_method: EVENTS_METHOD,
                    }
            }
        }
    }

    /// The extension's public declaration: what it is, and nothing that
    /// varies with this instance's switches, its workflows or its callers.
    fn declaration(self) -> Value {
        let uri = self.uri();
        match self {
            // The vocabulary agentd CAN answer, not what this instance serves
            // or this caller may run: the same list on every public card, so
            // it says nothing about whether introspection is on or who may
            // drain.
            Ext::Command => json!({
                "uri": uri,
                "description": "Structured operations invoked as a DataPart on SendMessage: \
                                {\"data\": {\"agentd\": {\"op\": \"…\", …}}}. \
                                The ops this caller may run are on the extended card.",
                "params": {
                    "dataPartKey": "agentd",
                    "schema": schema_of(uri),
                    "ops": op_entries(&static_vocabulary()),
                },
            }),
            Ext::Events => json!({
                "uri": uri,
                "description": "The instance-wide observation feed display clients render. \
                                A2A has no instance feed, so the method is declared here.",
                "params": {"method": EVENTS_METHOD, "schema": schema_of(uri)},
            }),
            Ext::TaskAnnotations => json!({
                "uri": uri,
                "description": "agentd's facts about a task — what it is linked to, who started \
                                it, when, its status history, a gate's answer schema — in the \
                                task's metadata under this URI.",
                "params": {"schema": schema_of(uri)},
            }),
        }
    }

    fn bit(self) -> u8 {
        1 << (self as u8)
    }
}

/// The methods agentd answers that A2A does not define, each paired with the
/// extension that declares it and who may call it. Nothing may be served off
/// this list: the route table reads it, negotiation refuses each method unless
/// its extension is declared AND activated, and the authorization matrix
/// admits each caller by the row's floor — a row has to say who may call its
/// method, so adding one cannot open it to every named caller by default.
pub const EXTENSION_METHODS: &[(&str, Ext, Floor)] =
    &[(EVENTS_METHOD, Ext::Events, Floor::AnyNamed)];

/// The extension that declares `method`, if one does.
pub fn owner_of(method: &str) -> Option<Ext> {
    EXTENSION_METHODS
        .iter()
        .find(|(name, ..)| *name == method)
        .map(|(_, ext, _)| *ext)
}

/// Who may call extension method `method`, if it is one.
pub fn extension_floor(method: &str) -> Option<Floor> {
    EXTENSION_METHODS
        .iter()
        .find(|(name, ..)| *name == method)
        .map(|(.., floor)| *floor)
}

/// One extension as an instance declares it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Declaration {
    pub ext: Ext,
    /// A client that does not activate a required extension is refused with
    /// `-32008` on every method it applies to. None of agentd's is: each is
    /// something a client may ignore and still converse.
    pub required: bool,
}

/// What an instance declares, from the one switch that varies it:
/// the command and task-annotations extensions always, events while the feed
/// is on.
///
/// Takes the switch rather than the settings because the listener asks with
/// what it SERVES: the feed is armed at spawn and `a2a.events` is
/// restart-only, so whether the listener holds a feed is the answer the card
/// gives too.
pub fn declared_when(events: bool) -> Vec<Declaration> {
    Ext::ALL
        .iter()
        .copied()
        .filter(|e| *e != Ext::Events || events)
        .map(|ext| Declaration {
            ext,
            required: false,
        })
        .collect()
}

/// Every extension THIS instance declares, in registry order.
///
/// The card is a promise, so an instance that serves no observation feed must
/// not advertise it — and the `--capabilities` manifest must say the same
/// thing, since a controller reads one and a peer reads the other.
pub fn declared_for(s: &Settings) -> Vec<Declaration> {
    declared_when(s.a2a.events.enabled)
}

/// The URIs [`declared_for`] names, as the manifest lists them.
pub fn extensions_of(s: &Settings) -> Vec<&'static str> {
    declared_for(s).iter().map(|d| d.ext.uri()).collect()
}

/// The card's `capabilities.extensions`: each declared extension's public
/// declaration. The extended card narrows these to its caller.
pub fn declarations(s: &Settings) -> Vec<Value> {
    declared_for(s)
        .iter()
        .map(|d| {
            let mut v = d.ext.declaration();
            // proto3 JSON leaves a `false` out, and the card is canonicalised
            // through the SDK's type, so only a required one says so.
            if d.required {
                v["required"] = json!(true);
            }
            v
        })
        .collect()
}

/// The published schema of an extension: served next to its URI.
pub fn schema_of(uri: &str) -> String {
    format!("{uri}/schema.json")
}

/// `[{op, reply}]` for `ops`, as the command extension's `params.ops` spells
/// them.
pub fn op_entries(ops: &[&str]) -> Vec<Value> {
    ops.iter()
        .map(|op| {
            let reply = match op_spec(op).map(|s| s.reply) {
                Some(Reply::Message) => "message",
                _ => "task",
            };
            json!({"op": op, "reply": reply})
        })
        .collect()
}

/// The extensions activated for one request: requested ∩ declared ∩ applies
/// to the method. Travels with the request to the runtime, which projects a
/// task's annotations only while task-annotations is in it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Active(u8);

impl Active {
    /// Nothing activated — every answer made outside a request.
    pub const NONE: Active = Active(0);

    pub fn contains(self, ext: Ext) -> bool {
        self.0 & ext.bit() != 0
    }

    fn with(self, ext: Ext) -> Active {
        Active(self.0 | ext.bit())
    }

    /// Exactly `exts`, for a test that stands in for the negotiation.
    #[cfg(all(test, feature = "a2a"))]
    pub(crate) fn of(exts: &[Ext]) -> Active {
        exts.iter().copied().fold(Active::NONE, Active::with)
    }

    /// The activated extensions, in registry order.
    pub fn exts(self) -> impl Iterator<Item = Ext> {
        Ext::ALL.iter().copied().filter(move |e| self.contains(*e))
    }

    /// The `A2A-Extensions` response value: the activated URIs, in registry
    /// order, `, `-joined — or `None`, which is no header at all rather than
    /// an empty one.
    pub fn echo(self) -> Option<String> {
        let uris: Vec<&str> = self.exts().map(Ext::uri).collect();
        (!uris.is_empty()).then(|| uris.join(", "))
    }
}

/// The URIs a request's `A2A-Extensions` field lines name.
///
/// Every line is read — RFC 9110 lets a sender split a list across repeated
/// lines, and a proxy may — each split on `,`, trimmed, empties dropped and
/// duplicates removed in first-seen order. A line that is not UTF-8, and a
/// token over [`MAX_EXTENSION_TOKEN_BYTES`], name nothing; at most
/// [`MAX_EXTENSION_TOKENS`] tokens are considered. None of this is refused:
/// the spec lets an agent ignore what it does not support.
pub fn parse_extension_header<'a>(lines: impl IntoIterator<Item = &'a [u8]>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut considered = 0;
    for line in lines {
        let Ok(line) = std::str::from_utf8(line) else {
            continue;
        };
        for token in line.split(',').map(str::trim).filter(|t| !t.is_empty()) {
            if considered == MAX_EXTENSION_TOKENS {
                return out;
            }
            considered += 1;
            if token.len() <= MAX_EXTENSION_TOKEN_BYTES && !out.iter().any(|u| u == token) {
                out.push(token.to_string());
            }
        }
    }
    out
}

/// Pipeline step 9: what `requested` activates on `route`, given what the
/// instance `declared` — or the JSON-RPC `error` object refusing the request.
///
/// * An extension method is `-32601` unless its extension is declared
///   (`EXTENSION_NOT_DECLARED`) and activated (`EXTENSION_NOT_ACTIVATED`):
///   a method is part of the extension, so using it without the header is
///   using the extension without saying so.
/// * A required extension that applies to the method and was not activated
///   is `-32008`.
/// * Everything else is answered, with requested ∩ declared ∩ applies.
pub fn negotiate(
    requested: &[String],
    declared: &[Declaration],
    route: Route,
) -> Result<Active, Value> {
    let is_declared = |e: Ext| declared.iter().any(|d| d.ext == e);
    let is_requested = |e: Ext| requested.iter().any(|u| u == e.uri());
    if let Route::Extension { ext_method } = route
        && let Some(owner) = owner_of(ext_method)
    {
        let uri = owner.uri();
        let refused = if !is_declared(owner) {
            Some((
                reason::EXTENSION_NOT_DECLARED,
                format!("{uri} is not offered by this instance"),
            ))
        } else if !is_requested(owner) {
            Some((
                reason::EXTENSION_NOT_ACTIVATED,
                format!("method not available: activate {uri} with the A2A-Extensions header"),
            ))
        } else {
            None
        };
        if let Some((why, message)) = refused {
            return Err(error_object(
                errors::METHOD_NOT_FOUND,
                &message,
                errors::error_info(
                    errors::domain_of(why),
                    why,
                    &[("extension", uri), ("method", ext_method)],
                ),
            ));
        }
    }
    let missing: Vec<&str> = declared
        .iter()
        .filter(|d| d.required && d.ext.applies_to(route) && !is_requested(d.ext))
        .map(|d| d.ext.uri())
        .collect();
    if !missing.is_empty() {
        let list = missing.join(", ");
        return Err(error_object(
            errors::EXTENSION_SUPPORT_REQUIRED,
            &format!("this agent requires {list}: activate it with the A2A-Extensions header"),
            errors::error_info(
                errors::domain_of(reason::EXTENSION_SUPPORT_REQUIRED),
                reason::EXTENSION_SUPPORT_REQUIRED,
                &[("extensions", &list)],
            ),
        ));
    }
    Ok(declared
        .iter()
        .map(|d| d.ext)
        .filter(|e| is_requested(*e) && e.applies_to(route))
        .fold(Active::NONE, Active::with))
}

/// A JSON-RPC `error` object carrying one detail.
fn error_object(code: i64, message: &str, detail: Value) -> Value {
    json!({"code": code, "message": message, "data": [detail]})
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::surface::route_of;

    fn route(method: &str) -> Route {
        route_of(method).unwrap_or_else(|| panic!("{method} does not route"))
    }

    fn uris(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| (*s).to_string()).collect()
    }

    fn reason_of(e: &Value) -> (&str, &str) {
        (
            e["data"][0]["reason"].as_str().unwrap_or_default(),
            e["data"][0]["domain"].as_str().unwrap_or_default(),
        )
    }

    /// Activated is exactly requested ∩ declared ∩ applies-to-the-method,
    /// for every method the table routes, every subset of the registry a
    /// request may name and both instances (feed on, feed off) — and an
    /// unknown URI, which activates nothing.
    #[test]
    fn negotiation_intersects_requested_declared_and_applicable() {
        let methods: Vec<&str> = SpecMethod::ALL
            .iter()
            .map(|m| m.name())
            .chain(EXTENSION_METHODS.iter().map(|(m, ..)| *m))
            .collect();
        for events in [false, true] {
            let declared = declared_when(events);
            for mask in 0u8..(1 << Ext::ALL.len()) {
                let mut requested: Vec<String> = Ext::ALL
                    .iter()
                    .filter(|e| mask & e.bit() != 0)
                    .map(|e| e.uri().to_string())
                    .collect();
                requested.push("https://example.invalid/nope/v1".into());
                for method in &methods {
                    let r = route(method);
                    let Ok(active) = negotiate(&requested, &declared, r) else {
                        // Only an extension method is ever refused here.
                        assert!(matches!(r, Route::Extension { .. }), "{method}");
                        continue;
                    };
                    for e in Ext::ALL {
                        let want = mask & e.bit() != 0
                            && declared.iter().any(|d| d.ext == *e)
                            && e.applies_to(r);
                        assert_eq!(
                            active.contains(*e),
                            want,
                            "events={events} mask={mask:03b} {method}: {e:?}"
                        );
                    }
                }
            }
        }
        // The rows that matter by name.
        let all = uris(&[
            COMMAND_EXTENSION,
            EVENTS_EXTENSION,
            TASK_ANNOTATIONS_EXTENSION,
        ]);
        let on = declared_when(true);
        let off = declared_when(false);
        let get = negotiate(&all, &on, route("GetTask")).unwrap();
        assert_eq!(get.echo().as_deref(), Some(TASK_ANNOTATIONS_EXTENSION));
        let send = negotiate(&all, &on, route("SendMessage")).unwrap();
        assert_eq!(
            send.echo().unwrap(),
            format!("{COMMAND_EXTENSION}, {TASK_ANNOTATIONS_EXTENSION}")
        );
        let feed = negotiate(&all, &on, route(EVENTS_METHOD)).unwrap();
        assert_eq!(
            feed.echo().unwrap(),
            format!("{EVENTS_EXTENSION}, {TASK_ANNOTATIONS_EXTENSION}")
        );
        let card = negotiate(&all, &on, route("GetExtendedAgentCard")).unwrap();
        assert_eq!(card, Active::NONE, "nothing applies to the card");
        assert_eq!(card.echo(), None, "and nothing is echoed");
        // Declared matters: the feed's URI on a feed-off instance activates
        // nothing on a method it would otherwise apply to.
        let only_events = uris(&[EVENTS_EXTENSION]);
        assert!(
            negotiate(&only_events, &on, route(EVENTS_METHOD))
                .unwrap()
                .contains(Ext::Events)
        );
        assert!(negotiate(&only_events, &off, route(EVENTS_METHOD)).is_err());
    }

    /// Every line is read, split, trimmed, deduplicated in order; empties,
    /// non-UTF-8 lines and oversized tokens name nothing; and no more than
    /// the cap is considered, however the tokens are spread over lines.
    #[test]
    fn header_parsing_merges_lines_trims_dedupes_and_caps() {
        let parse = |lines: &[&[u8]]| parse_extension_header(lines.iter().copied());
        assert_eq!(
            parse(&[b" urn:a ,urn:b,, urn:a", b"urn:c", b"", b" , ", b"urn:b"]),
            uris(&["urn:a", "urn:b", "urn:c"])
        );
        assert_eq!(parse(&[b"\xff\xfe urn:bad", b"urn:ok"]), uris(&["urn:ok"]));
        let long = format!("urn:{}", "x".repeat(MAX_EXTENSION_TOKEN_BYTES));
        let edge = format!("urn:{}", "y".repeat(MAX_EXTENSION_TOKEN_BYTES - 4));
        let line = format!("{long}, {edge}");
        assert_eq!(parse(&[line.as_bytes()]), vec![edge.clone()]);
        // The cap counts tokens considered, across lines: the 33rd is never
        // looked at, even when it is one agentd declares.
        let many: Vec<String> = (0..MAX_EXTENSION_TOKENS)
            .map(|i| format!("urn:{i}"))
            .collect();
        let first = many[..10].join(",");
        let rest = format!("{}, {COMMAND_EXTENSION}", many[10..].join(","));
        let got = parse(&[first.as_bytes(), rest.as_bytes()]);
        assert_eq!(got, many, "the cap");
        // Duplicates count toward it: a header cannot buy lookups by
        // repeating itself.
        let repeated = vec!["urn:a"; MAX_EXTENSION_TOKENS].join(",");
        let line = format!("{repeated},urn:b");
        assert_eq!(parse(&[line.as_bytes()]), uris(&["urn:a"]));
        // Matching is exact: case and a trailing slash are other URIs.
        let odd = format!("{}, {COMMAND_EXTENSION}/", COMMAND_EXTENSION.to_uppercase());
        let got = parse(&[odd.as_bytes()]);
        let active = negotiate(&got, &declared_when(true), route("SendMessage")).unwrap();
        assert_eq!(active, Active::NONE, "{got:?}");
    }

    /// A required extension the request did not activate is `-32008` with
    /// the spec's reason, on the methods it applies to — and only those.
    /// None of agentd's is required, so the declaration is synthetic.
    #[test]
    fn a_required_extension_not_activated_is_refused() {
        let declared = vec![
            Declaration {
                ext: Ext::Command,
                required: false,
            },
            Declaration {
                ext: Ext::TaskAnnotations,
                required: true,
            },
        ];
        let e = negotiate(&[], &declared, route("GetTask")).unwrap_err();
        assert_eq!(e["code"], errors::EXTENSION_SUPPORT_REQUIRED, "{e}");
        assert_eq!(
            reason_of(&e),
            (reason::EXTENSION_SUPPORT_REQUIRED, errors::A2A_DOMAIN),
            "{e}"
        );
        assert_eq!(
            e["data"][0]["metadata"]["extensions"], TASK_ANNOTATIONS_EXTENSION,
            "{e}"
        );
        // Activated, it is answered.
        let active = negotiate(
            &uris(&[TASK_ANNOTATIONS_EXTENSION]),
            &declared,
            route("GetTask"),
        )
        .unwrap();
        assert!(active.contains(Ext::TaskAnnotations));
        // It does not apply to the card, so the card is not held to it.
        assert!(negotiate(&[], &declared, route("GetExtendedAgentCard")).is_ok());
        // agentd's own declarations require nothing.
        for events in [false, true] {
            for d in declared_when(events) {
                assert!(!d.required, "{d:?}");
            }
            for m in SpecMethod::ALL {
                assert!(negotiate(&[], &declared_when(events), Route::Spec(*m)).is_ok());
            }
        }
    }

    /// An extension method is `-32601` unless its extension is declared and
    /// the request activates it, with a reason saying which — and the
    /// declared check comes first, so a feed-off instance says so whatever
    /// the request named.
    #[test]
    fn an_extension_method_needs_its_extension() {
        for (method, owner, _) in EXTENSION_METHODS {
            let r = route(method);
            let uri = owner.uri();
            let on = declared_when(true);
            let off = declared_when(false);
            let asked = uris(&[uri]);
            for (declared, requested, want) in [
                (&on, &[][..], reason::EXTENSION_NOT_ACTIVATED),
                (&off, &[][..], reason::EXTENSION_NOT_DECLARED),
                (&off, &asked[..], reason::EXTENSION_NOT_DECLARED),
            ] {
                let e = negotiate(requested, declared, r).unwrap_err();
                assert_eq!(e["code"], errors::METHOD_NOT_FOUND, "{e}");
                assert_eq!(reason_of(&e), (want, errors::AGENTD_DOMAIN), "{e}");
                assert_eq!(e["data"][0]["metadata"]["extension"], uri, "{e}");
                assert_eq!(e["data"][0]["metadata"]["method"], *method, "{e}");
            }
            let e = negotiate(&[], &on, r).unwrap_err();
            assert_eq!(
                e["message"],
                format!("method not available: activate {uri} with the A2A-Extensions header")
            );
            let e = negotiate(&[], &off, r).unwrap_err();
            assert_eq!(
                e["message"],
                format!("{uri} is not offered by this instance")
            );
            let active = negotiate(&asked, &on, r).unwrap();
            assert!(active.contains(*owner), "{method}");
            assert!(owner.applies_to(r), "{method} is its extension's");
        }
    }

    /// Every URI in the registry is agentd's (`https://agentd.dev/a2a/…`),
    /// carries no version segment, and is its own: an incompatible change
    /// takes a new name, so a `/v<N>` here would be the old habit coming
    /// back. Each extension method is namespaced under its extension's
    /// name, read off the URI's last segment.
    #[test]
    fn every_extension_uri_is_agentds_unversioned_and_unique() {
        let mut seen = Vec::new();
        for uri in Ext::ALL
            .iter()
            .map(|e| e.uri())
            .chain(std::iter::once(UNIX_BINDING))
        {
            assert!(uri.starts_with("https://agentd.dev/a2a/"), "{uri}");
            for seg in uri.split('/') {
                assert!(
                    !seg.strip_prefix('v')
                        .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit())),
                    "{uri} carries a version segment {seg:?}"
                );
            }
            assert!(!seen.contains(&uri), "{uri} twice");
            seen.push(uri);
        }
        for (method, owner, _) in EXTENSION_METHODS {
            let (ns, _) = method.split_once('/').expect("a namespaced method");
            let name = owner.uri().rsplit('/').next().unwrap();
            assert_eq!(ns, format!("agentd.{name}"), "{method}");
        }
    }
}
