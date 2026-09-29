// SPDX-License-Identifier: AGPL-3.0-only
//! The agent card: the public one anybody may read, and the extended one a
//! caller who authenticated with a declared scheme is told.
//!
//! Both are built from the settings in force NOW, on every request, and from
//! the posture the listener enforces now — never from a copy taken at spawn —
//! so a reload that adds a principal rule changes what the card declares on
//! the next read.

use super::commands::refusal;
use super::{COMMAND_EXTENSION, FEED_RING, UNSUPPORTED_OPERATION, command_ops_of};
use crate::a2a::Principal;
use crate::a2a::errors::{self, reason};
use crate::config::v2::Settings;
use crate::engine::model::Workflow;
use crate::runtime::reactor::Runtime;
use crate::runtime::surface::auth::{ListenerAuth, listener_auth_of, origin_of, security_of};
use crate::runtime::surface::{
    A2A_PROTOCOL_VERSION, EVENTS_EXTENSION, RUNTIME_SETTABLE, UNIX_BINDING, declarations,
    op_entries,
};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::sync::Arc;

/// What the card says when `agent.description` is unset.
const DEFAULT_DESCRIPTION: &str =
    "A durable agent (agentd) — conversations, workflows, and subagents over A2A.";

/// Which card to build.
#[derive(Clone, Copy)]
pub(crate) enum CardView<'a> {
    /// The card anybody may read: the same for every caller, whatever is
    /// loaded or switched on.
    Public,
    /// The card `caller` is told once it authenticated: the public one plus
    /// what this caller may run.
    Extended(&'a Principal),
}

impl Runtime {
    /// The public card, unauthenticated at `/.well-known/agent-card.json`.
    pub(super) fn a2a_agent_card(&self) -> Value {
        card_served(
            &self.settings,
            self.a2a_serving.as_ref(),
            &self.workflows,
            CardView::Public,
        )
    }

    /// `GetExtendedAgentCard`: the card for `principal`.
    pub(super) fn a2a_extended_card(&self, principal: &Principal) -> Value {
        card_served(
            &self.settings,
            self.a2a_serving.as_ref(),
            &self.workflows,
            CardView::Extended(principal),
        )
    }
}

/// The card a runtime serves: from its settings, the listener it serves on
/// (`serving`, once bound) and its loaded workflows — everything it reads,
/// and nothing else of the runtime's, so what a card could be built from is
/// all here to compare.
///
/// The posture is the live resolver's, which a reload of `a2a.principals`
/// replaces. It is read from the resolver rather than recomputed from the
/// settings, because a rebuild that fails keeps the old rules — and then the
/// card must describe the rules still enforced.
///
/// The extended view: the listener has already refused every caller that did
/// not present a declared scheme's credential (an `any` rule, the implicit
/// operator and a unix peer get 401 there), so what reaches here is a named
/// caller — or, on a listener that declares no scheme, whoever the bind
/// admits. That listener has no extended card to give: A2A 1.0 §3.3.4 makes
/// an agent that does not advertise one answer "unsupported operation".
pub(super) fn card_served(
    settings: &Settings,
    serving: Option<&super::A2aServing>,
    workflows: &BTreeMap<String, Arc<Workflow>>,
    view: CardView<'_>,
) -> Value {
    let posture = serving
        .map(|s| s.bridge.resolver().posture())
        .unwrap_or_else(|| listener_auth_of(&settings.a2a));
    let url = serving
        .map(|s| s.advertised_url.clone())
        .or_else(|| crate::runtime::surface::auth::configured_url(settings))
        .unwrap_or_default();
    if let CardView::Extended(principal) = view {
        if !posture.declares_any() {
            return refusal(
                UNSUPPORTED_OPERATION,
                reason::UNSUPPORTED_OPERATION,
                "this agent declares no authentication scheme, so it has no extended card",
                &[],
            );
        }
        // A second lock behind the listener's gate: a path into the runtime
        // that skipped it still does not tell an anonymous caller anything.
        if principal.is_anonymous() {
            return refusal(
                errors::UNAUTHENTICATED,
                reason::UNAUTHENTICATED,
                "authentication required",
                &[],
            );
        }
    }
    agent_card_of(settings, &url, posture, workflows, view)
}

/// The card, from settings, the advertised URL, the posture and the loaded
/// workflows — no `Runtime`, so it can be tested as a function.
///
/// The public view reads `workflows` not at all: what is loaded, like what is
/// switched on and who is asking, is the extended card's business. The card
/// is built as JSON and emitted as the SDK's typed `AgentCard` serialises it,
/// both views by the one path.
fn agent_card_of(
    settings: &Settings,
    advertised_url: &str,
    posture: ListenerAuth,
    workflows: &BTreeMap<String, Arc<Workflow>>,
    view: CardView<'_>,
) -> Value {
    // One skill, the same on every public card: A2A's `skills` is REQUIRED and
    // a peer that knows nothing of agentd can always talk. Workflows and
    // commands are what a caller may RUN, so they belong to the card that
    // knows who the caller is.
    let mut skills = vec![json!({
        "id": "conversation",
        "name": "Conversation",
        "description": "Talk to this agent in natural language. Authenticate and read the \
                        extended card for its workflows and commands.",
        "tags": ["conversation"],
    })];
    let mut extensions = declarations(settings);
    if let CardView::Extended(caller) = view {
        skills.extend(
            workflows
                .values()
                .filter(|w| Runtime::may_run(caller, w))
                .map(|w| workflow_skill(w)),
        );
        for ext in &mut extensions {
            narrow(ext, settings, workflows, caller);
        }
    }
    // Exactly the four fields `AgentCapabilities` carries in the A2A protobuf.
    // `pushNotifications` tracks whether this instance will accept a webhook,
    // not whether the code exists; `extendedAgentCard` whether a caller could
    // authenticate to read one.
    let capabilities = json!({
        "streaming": true,
        "pushNotifications": settings.a2a.push.enabled,
        "extendedAgentCard": posture.declares_any(),
        "extensions": extensions,
    });
    // A unix socket is JSON-RPC over a path no `https://` URL can name, so it
    // declares agentd's own binding: a stock client selects an interface by
    // binding and version before it sends anything, and must not select one
    // it cannot dial.
    let binding = if advertised_url.starts_with("unix:") {
        UNIX_BINDING
    } else {
        a2a_rs::domain::PROTOCOL_BINDING_JSONRPC
    };
    let name = settings
        .agent
        .name
        .as_deref()
        .filter(|n| !n.trim().is_empty())
        .unwrap_or("agentd");
    let description = settings
        .agent
        .description
        .as_deref()
        .filter(|d| !d.trim().is_empty())
        .unwrap_or(DEFAULT_DESCRIPTION);
    let mut card = json!({
        // `agent.name` as the operator wrote it, else the product. Never the
        // instance name: its fallbacks are the pod and the host, and those
        // are not for an unauthenticated reader.
        "name": name,
        "description": description,
        "version": crate::VERSION,
        "supportedInterfaces": [
            {"url": advertised_url, "protocolBinding": binding,
             "protocolVersion": A2A_PROTOCOL_VERSION}
        ],
        "capabilities": capabilities,
        // What a message may carry is what `a2a_send` accepts: one list.
        "defaultInputModes": crate::a2a::wire::INPUT_MODES,
        "defaultOutputModes": ["text/plain", "application/json"],
        "skills": skills,
    });
    if let Some(sec) = security_of(
        &posture,
        origin_of(advertised_url).as_deref(),
        &settings.a2a.device_grant.scopes,
    ) {
        card["securitySchemes"] = serde_json::to_value(&sec.schemes).unwrap_or(Value::Null);
        card["securityRequirements"] =
            serde_json::to_value(&sec.requirements).unwrap_or(Value::Null);
    }
    // Emit what the SDK emits. The card is a protocol document, and the only
    // authority on its wire form is the type generated from the A2A protobuf:
    // proto3 JSON omits a field at its default, so `required: false` on an
    // extension is ABSENT on a conformant wire and a peer reads absence as
    // false.
    //
    // A conversion failure would mean the card is not an `AgentCard`, which the
    // unit tests make impossible; were it ever to happen, sending the
    // hand-built form beats sending nothing.
    match serde_json::from_value::<a2a_rs::domain::AgentCard>(card.clone())
        .ok()
        .and_then(|c| serde_json::to_value(c).ok())
    {
        Some(canonical) => canonical,
        None => card,
    }
}

/// Narrow an extension's public declaration to what `caller` may use.
fn narrow(
    ext: &mut Value,
    settings: &Settings,
    workflows: &BTreeMap<String, Arc<Workflow>>,
    caller: &Principal,
) {
    let uri = ext["uri"].as_str().unwrap_or_default();
    if uri == COMMAND_EXTENSION {
        // What this instance serves AND this caller may run: an operator sees
        // the admin family, a `user` does not, and neither sees an
        // introspection op while introspection is off.
        let ops: Vec<&str> = command_ops_of(settings)
            .into_iter()
            .filter(|op| caller.may_command(op))
            .collect();
        ext["params"]["ops"] = json!(op_entries(&ops));
        let commands = declared_commands(workflows, caller);
        if !commands.is_empty() {
            ext["params"]["commands"] = json!(commands);
        }
        if caller.may_command("admin.set") {
            ext["params"]["settable"] = json!(RUNTIME_SETTABLE);
        }
    } else if uri == EVENTS_EXTENSION {
        // The replay window: how far behind a reconnecting subscriber may be
        // and still resume rather than re-bootstrap.
        ext["params"]["ring"] = json!(FEED_RING);
        // The kinds this caller's subscription can carry, from the one table
        // the feed's push is checked against.
        ext["params"]["kinds"] = json!(crate::runtime::surface::events::kinds_for(
            caller.is_operator(),
            settings.a2a.introspection.enabled,
        ));
    }
}

/// The commands workflows declare (`a2a` starts with a `command`) that
/// `caller` may fire: the grants admit the op, and the start's `roles:` the
/// caller — the two filters a send meets, at the listener and in the reactor.
fn declared_commands(
    workflows: &BTreeMap<String, Arc<Workflow>>,
    caller: &Principal,
) -> Vec<Value> {
    let role = serde_json::to_value(caller.role).unwrap_or(Value::Null);
    let role = role.as_str().unwrap_or_default();
    let mut out = Vec::new();
    for w in workflows.values() {
        for s in w.start_steps() {
            let Some(op) = s.spec.get("command").and_then(Value::as_str) else {
                continue;
            };
            let admits = s
                .spec
                .get("roles")
                .and_then(Value::as_array)
                .is_none_or(|roles| {
                    roles.is_empty() || roles.iter().any(|r| r.as_str() == Some(role))
                });
            if s.kind != "a2a" || !admits || !caller.may_command(op) {
                continue;
            }
            let mut c = json!({"op": op, "workflow": w.name});
            if let Some(schema) = s.spec.get("schema") {
                c["schema"] = schema.clone();
            }
            out.push(c);
        }
    }
    out
}

/// A workflow as a skill. The id is namespaced so a workflow can never share
/// one with the conversation skill, and the description is never empty: the
/// proto REQUIRES one, and the SDK drops an empty string.
fn workflow_skill(w: &Workflow) -> Value {
    let description = w
        .description
        .clone()
        .filter(|d| !d.trim().is_empty())
        .unwrap_or_else(|| format!("Run the {} workflow", w.name));
    json!({
        "id": format!("workflow:{}", w.name),
        "name": w.name,
        "description": description,
        "tags": ["workflow"],
    })
}

#[cfg(test)]
mod tests {
    use super::super::listener::advertised_url as advertised;
    use super::*;
    use crate::config::v2::{A2a, Role};
    use crate::runtime::surface::{op_spec, static_vocabulary};

    fn settings(a2a: serde_json::Value) -> Settings {
        Settings {
            a2a: serde_json::from_value(a2a).unwrap(),
            ..Default::default()
        }
    }

    /// The authority a listener on `listen` binds: the address a kernel would
    /// report for it, `:0` resolved to a port it could have given.
    fn bound_of(listen: &str) -> String {
        if let Some(path) = listen.strip_prefix("unix://") {
            return format!("unix:{path}");
        }
        let authority = listen.split_once("://").map_or(listen, |(_, r)| r);
        let authority = authority.trim_end_matches('/');
        let (host, port) = authority.rsplit_once(':').unwrap();
        let host = if host == "localhost" {
            "127.0.0.1"
        } else {
            host
        };
        let port = if port == "0" { "41234" } else { port };
        format!("{host}:{port}")
    }

    fn url_of(a2a: &A2a) -> String {
        advertised(a2a, &bound_of(a2a.listen.as_deref().unwrap()))
    }

    fn no_workflows() -> BTreeMap<String, Arc<Workflow>> {
        BTreeMap::new()
    }

    fn workflows(docs: &[serde_json::Value]) -> BTreeMap<String, Arc<Workflow>> {
        docs.iter()
            .map(|d| {
                // Each document names its start `s`; the finish that makes it
                // a whole workflow is the same every time.
                let mut d = d.clone();
                d["steps"]["f"] =
                    json!({"kind": "finish", "depends_on": ["s"], "status": "completed"});
                let w = crate::engine::model::parse_workflow(&d)
                    .unwrap_or_else(|e| panic!("an invalid workflow {d}: {e:?}"));
                (w.name.clone(), Arc::new(w))
            })
            .collect()
    }

    fn caller(id: &str, role: Role, grants: &[&str]) -> Principal {
        Principal {
            id: id.into(),
            role,
            grants: grants.iter().map(|g| (*g).to_string()).collect(),
            ..Principal::anonymous()
        }
    }

    fn card(s: &Settings, wfs: &BTreeMap<String, Arc<Workflow>>, view: CardView<'_>) -> Value {
        agent_card_of(s, &url_of(&s.a2a), listener_auth_of(&s.a2a), wfs, view)
    }

    /// A card is a fixpoint of the SDK's typed `AgentCard`: nothing it sets is
    /// outside the message, so nothing is dropped by a typed reader.
    fn assert_fixpoint(card: &Value, what: &str) -> a2a_rs::domain::AgentCard {
        let typed: a2a_rs::domain::AgentCard = serde_json::from_value(card.clone())
            .unwrap_or_else(|e| panic!("{what}: not an AgentCard ({e}): {card}"));
        let back = serde_json::to_value(&typed).expect("and serializes again");
        assert_eq!(card, &back, "{what}: not a fixpoint of the SDK's type");
        typed
    }

    fn ops_of(card: &Value) -> Vec<String> {
        command_params(card)["ops"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|o| o["op"].as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default()
    }

    fn command_params(card: &Value) -> Value {
        card["capabilities"]["extensions"]
            .as_array()
            .and_then(|a| a.iter().find(|e| e["uri"] == COMMAND_EXTENSION))
            .map(|e| e["params"].clone())
            .unwrap_or(Value::Null)
    }

    fn skill_ids(card: &Value) -> Vec<String> {
        card["skills"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|s| s["id"].as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Both cards survive the SDK's typed `AgentCard` without losing a field,
    /// declare the A2A version the listener serves, and carry every field
    /// the message REQUIRES — every skill's included.
    ///
    /// The version was `"0.3.0"` for an interface that speaks the 1.0 wire, so
    /// a 1.0 client either passed our interface over or spoke 0.3 at it; and
    /// a skill with no description (an op without an arm, a workflow without
    /// one) lost the REQUIRED field in the round trip on one card and carried
    /// `""` on the other, because the extended card was patched after it.
    #[test]
    fn the_card_round_trips_through_the_sdks_typed_agent_card() {
        let s = settings(json!({
            "listen": "https://127.0.0.1:8443", "bearer": "{{secret:B}}",
            "tls": {"cert": "c", "key": "k"},
            "events": {"enabled": true}, "introspection": {"enabled": true},
        }));
        let wfs = workflows(&[
            json!({"name": "undescribed", "steps": {"s": {"kind": "manual"}}}),
            json!({"name": "blank", "description": "  ", "steps": {"s": {"kind": "manual"}}}),
            json!({"name": "said", "description": "Says so", "steps": {"s": {"kind": "manual"}}}),
        ]);
        let operator = caller("operator:o", Role::Operator, &[]);
        let user = caller("user:u", Role::User, &[]);
        for (what, view) in [
            ("public", CardView::Public),
            ("extended (operator)", CardView::Extended(&operator)),
            ("extended (user)", CardView::Extended(&user)),
        ] {
            let ours = card(&s, &wfs, view);
            let typed = assert_fixpoint(&ours, what);

            for key in [
                "name",
                "description",
                "version",
                "supportedInterfaces",
                "capabilities",
                "defaultInputModes",
                "defaultOutputModes",
                "skills",
            ] {
                assert!(!ours[key].is_null(), "{what}: omits `{key}`: {ours}");
            }
            // The pre-interfaces spelling and a capability the message does
            // not carry stay gone.
            for gone in ["protocolVersion", "url", "preferredTransport"] {
                assert!(
                    ours.get(gone).is_none(),
                    "{what}: `{gone}` is not a card field"
                );
            }
            assert!(ours["capabilities"].get("stateTransitionHistory").is_none());

            // The version the listener serves, Major.Minor, major 1 — and the
            // SDK read it as such.
            assert!(!typed.supported_interfaces.is_empty(), "{what}");
            for i in &typed.supported_interfaces {
                assert_eq!(i.protocol_version, A2A_PROTOCOL_VERSION, "{what}");
                let parts: Vec<&str> = i.protocol_version.split('.').collect();
                assert!(
                    parts.len() == 2 && parts.iter().all(|p| p.parse::<u32>().is_ok()),
                    "{what}: {} is not Major.Minor",
                    i.protocol_version
                );
                assert_eq!(parts[0], "1", "{what}");
            }

            // Every skill has what the proto REQUIRES, on the wire, and no tag
            // twice.
            for sk in ours["skills"].as_array().unwrap() {
                for key in ["id", "name", "description"] {
                    assert!(
                        sk[key].as_str().is_some_and(|v| !v.trim().is_empty()),
                        "{what}: a skill without `{key}`: {sk}"
                    );
                }
                let tags: Vec<&str> = sk["tags"]
                    .as_array()
                    .map(|a| a.iter().filter_map(Value::as_str).collect())
                    .unwrap_or_default();
                assert!(!tags.is_empty(), "{what}: a skill without tags: {sk}");
                let mut unique = tags.clone();
                unique.dedup();
                assert_eq!(unique, tags, "{what}: a tag twice: {sk}");
            }
            // The extensions we declare survive the trip.
            let exts = ours["capabilities"]["extensions"].as_array().unwrap();
            for uri in crate::runtime::surface::extensions_of(&s) {
                assert!(exts.iter().any(|e| e["uri"] == uri), "{what}: {uri} lost");
            }
        }
        // The undescribed workflows are described on the card that lists them.
        let ext = card(&s, &wfs, CardView::Extended(&operator));
        let described = |id: &str| {
            ext["skills"]
                .as_array()
                .unwrap()
                .iter()
                .find(|sk| sk["id"] == id)
                .map(|sk| sk["description"].clone())
        };
        assert_eq!(
            described("workflow:undescribed"),
            Some(json!("Run the undescribed workflow"))
        );
        assert_eq!(
            described("workflow:blank"),
            Some(json!("Run the blank workflow"))
        );
        assert_eq!(described("workflow:said"), Some(json!("Says so")));
    }

    /// A stock a2a-rs client, handed our card, selects our interface.
    ///
    /// a2a-rs's `TransportNegotiator::select` offers a transport factory an
    /// interface only when its `protocolBinding` is the factory's protocol and
    /// its `protocolVersion` passes `version_compatible` — empty, or major
    /// `1`. With `"0.3.0"` on the card nothing passed, and a stock client
    /// answered "no compatible transport". The negotiator itself cannot be
    /// linked here: a2a-rs 0.10's `client` feature does not compile without
    /// the reqwest transports this crate keeps out of its tree. So its rule is
    /// applied, as written there, to the card the SDK's own type parsed.
    #[test]
    fn a2a_rs_negotiates_a_transport_from_our_card() {
        // a2a-rs 0.10 `adapter/transport/negotiation.rs`, `version_compatible`.
        fn version_compatible(version: &str) -> bool {
            version.is_empty() || version.split('.').next() == Some("1")
        }
        // The copy is only as good as the version it was copied from: a
        // dependency bump fails here until the rule above is re-read against
        // the new release's negotiator and this pin moved with it.
        let lock =
            std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/../../Cargo.lock"))
                .expect("the workspace lockfile");
        let locked = lock
            .split("[[package]]")
            .find(|p| p.contains("\nname = \"a2a-rs\"\n"))
            .and_then(|p| p.lines().find_map(|l| l.strip_prefix("version = ")))
            .expect("a2a-rs is locked");
        assert_eq!(
            locked, "\"0.10.0\"",
            "a2a-rs moved: re-check version_compatible against its negotiation.rs"
        );
        let s = settings(
            json!({"listen": "https://agent.example:8443", "bearer": "{{secret:B}}",
            "tls": {"cert": "c", "key": "k"}, "url": "https://agent.example:8443"}),
        );
        let operator = caller("operator:o", Role::Operator, &[]);
        for (what, view) in [
            ("public", CardView::Public),
            ("extended", CardView::Extended(&operator)),
        ] {
            let typed: a2a_rs::domain::AgentCard =
                serde_json::from_value(card(&s, &no_workflows(), view)).unwrap();
            let offered: Vec<&str> = typed
                .supported_interfaces
                .iter()
                .filter(|i| {
                    i.protocol_binding == a2a_rs::domain::PROTOCOL_BINDING_JSONRPC
                        && version_compatible(&i.protocol_version)
                })
                .map(|i| i.url.as_str())
                .collect();
            assert_eq!(
                offered,
                vec!["https://agent.example:8443"],
                "{what}: a JSON-RPC client would find no interface to dial"
            );
        }
    }

    /// No card names an address nobody can dial: not a wildcard host, not the
    /// `:0` a bind asked for.
    #[test]
    fn advertised_url() {
        for (a2a, want) in [
            (
                json!({"listen": "https://0.0.0.0:8443", "url": "https://agent.example",
                    "tls": {"cert": "c", "key": "k"}, "bearer": "{{secret:B}}"}),
                "https://agent.example",
            ),
            (
                json!({"listen": "https://[::]:8443", "url": "https://agent.example:9443",
                    "tls": {"cert": "c", "key": "k"}, "bearer": "{{secret:B}}"}),
                "https://agent.example:9443",
            ),
            (
                json!({"listen": "http://127.0.0.1:0"}),
                "http://127.0.0.1:41234/",
            ),
            (
                json!({"listen": "https://[::1]:0",
                "tls": {"cert": "c", "key": "k"}}),
                "https://[::1]:41234/",
            ),
            (
                json!({"listen": "http://localhost:8080"}),
                "http://localhost:8080/",
            ),
        ] {
            let s = settings(a2a.clone());
            let operator = caller("operator:o", Role::Operator, &[]);
            for view in [CardView::Public, CardView::Extended(&operator)] {
                let c = card(&s, &no_workflows(), view);
                assert_eq!(c["supportedInterfaces"][0]["url"], want, "{a2a}");
                let text = c.to_string();
                for never in ["0.0.0.0", "[::]", "//::", ":0/", ":0\""] {
                    assert!(!text.contains(never), "{a2a}: the card names {never}: {c}");
                }
            }
        }
    }

    /// A unix socket is never labelled JSON-RPC, and declares no scheme — the
    /// kernel names the peer — so it has no extended card to offer either.
    #[test]
    fn unix_cards_never_claim_jsonrpc() {
        let s = settings(json!({"listen": "unix:///run/agentd.sock", "bearer": "{{secret:B}}"}));
        let c = card(&s, &no_workflows(), CardView::Public);
        assert_fixpoint(&c, "unix");
        let iface = &c["supportedInterfaces"][0];
        assert_eq!(iface["protocolBinding"], UNIX_BINDING, "{c}");
        assert_eq!(iface["url"], "unix:///run/agentd.sock", "{c}");
        assert_eq!(iface["protocolVersion"], A2A_PROTOCOL_VERSION, "{c}");
        assert!(c.get("securitySchemes").is_none(), "{c}");
        assert!(c.get("securityRequirements").is_none(), "{c}");
        assert_ne!(c["capabilities"]["extendedAgentCard"], true, "{c}");
    }

    /// The card's name is what the operator declared, else the product —
    /// never the instance name, whose fallbacks are the pod and the host.
    #[test]
    fn card_name_is_declared_not_derived() {
        let mut s = settings(json!({"listen": "http://127.0.0.1:8080"}));
        let c = card(&s, &no_workflows(), CardView::Public);
        assert_eq!(c["name"], "agentd", "{c}");
        assert_eq!(c["description"], DEFAULT_DESCRIPTION, "{c}");
        // Whatever this host is called, the card does not say it.
        let derived = s.instance_name();
        if derived != "agentd" {
            assert!(!c.to_string().contains(&derived), "{derived} leaked: {c}");
        }

        s.agent.name = Some("billing-agent".into());
        s.agent.description = Some("Answers billing questions.".into());
        let c = card(&s, &no_workflows(), CardView::Public);
        assert_eq!(c["name"], "billing-agent", "{c}");
        assert_eq!(c["description"], "Answers billing questions.", "{c}");
    }

    /// The public card says nothing about what is switched on, what is loaded
    /// or who is asking.
    ///
    /// It used to list every workflow and every op this instance served, so an
    /// anonymous reader learned whether transcript-reading introspection was
    /// on and what the operator could do — more than an authenticated user's
    /// own extended card told it. Across every combination here the cards
    /// differ only in the fields that are about reaching the agent at all.
    #[test]
    fn the_public_card_is_independent_of_introspection_workflows_and_caller() {
        let loaded = workflows(&[json!({"name": "secret-sauce",
            "steps": {"s": {"kind": "a2a", "command": "sauce.make", "roles": ["operator"]}}})]);
        let empty = no_workflows();
        let reachability = [
            "/securitySchemes",
            "/securityRequirements",
            "/capabilities/extendedAgentCard",
            "/capabilities/pushNotifications",
        ];
        let strip = |mut c: Value| {
            for p in reachability {
                let (parent, key) = p.rsplit_once('/').unwrap();
                if let Some(o) = c.pointer_mut(parent).and_then(Value::as_object_mut) {
                    o.remove(key);
                }
            }
            c
        };
        let mut baseline: Option<Value> = None;
        for introspection in [false, true] {
            for device in [false, true] {
                for push in [false, true] {
                    for wfs in [&empty, &loaded] {
                        let mut a2a = json!({"listen": "http://127.0.0.1:8080",
                            "introspection": {"enabled": introspection},
                            "push": {"enabled": push}});
                        if device {
                            a2a["bearer"] = json!("{{secret:B}}");
                            a2a["device_grant"] = json!({"enabled": true});
                        }
                        let s = settings(a2a);
                        let c = card(&s, wfs, CardView::Public);
                        let row = format!(
                            "introspection={introspection} device={device} push={push} workflows={}",
                            wfs.len()
                        );
                        assert_eq!(skill_ids(&c), vec!["conversation"], "{row}: {c}");
                        let ops = ops_of(&c);
                        assert_eq!(ops, static_vocabulary(), "{row}");
                        assert!(!c.to_string().contains("sauce"), "{row}: {c}");
                        let c = strip(c);
                        match &baseline {
                            None => baseline = Some(c),
                            Some(b) => assert_eq!(b, &c, "{row}: the public card moved"),
                        }
                    }
                }
            }
        }
    }

    /// The extended card is the public one plus what THIS caller may use:
    /// the workflows it may run, the ops this instance serves and it may call,
    /// the commands workflows declare that it may fire, and the settable paths
    /// only for a caller that may set them. Commands are never skills.
    #[test]
    fn the_extended_card_is_narrowed_to_the_caller() {
        let s = settings(
            json!({"listen": "http://127.0.0.1:8080", "bearer": "{{secret:B}}",
            "events": {"enabled": true}}),
        );
        let wfs = workflows(&[
            json!({"name": "greet", "steps": {"s": {"kind": "manual"}}}),
            json!({"name": "ops-only", "steps": {"s": {"kind": "a2a", "command": "ops.go",
                "roles": ["operator"]}}}),
            json!({"name": "report", "steps": {"s": {"kind": "a2a", "command": "report.make",
                "schema": {"type": "object"}}}}),
        ]);
        let operator = caller("operator:o", Role::Operator, &[]);
        // Granted both commands; the `roles:` of `ops-only`'s start still keep
        // `ops.go` from it.
        let user = caller("user:u", Role::User, &["report.make", "ops.go"]);

        let op = card(&s, &wfs, CardView::Extended(&operator));
        assert_eq!(
            skill_ids(&op),
            vec![
                "conversation",
                "workflow:greet",
                "workflow:ops-only",
                "workflow:report"
            ]
        );
        assert_eq!(
            ops_of(&op),
            command_ops_of(&s),
            "the operator may run every served op"
        );
        let params = command_params(&op);
        assert_eq!(params["settable"], json!(RUNTIME_SETTABLE));
        assert_eq!(
            params["commands"],
            json!([{"op": "ops.go", "workflow": "ops-only"},
                   {"op": "report.make", "workflow": "report", "schema": {"type": "object"}}])
        );
        let feed = op["capabilities"]["extensions"]
            .as_array()
            .unwrap()
            .iter()
            .find(|e| e["uri"] == EVENTS_EXTENSION)
            .unwrap();
        // Extension params are a protobuf `Struct`, whose numbers are doubles.
        assert_eq!(feed["params"]["ring"].as_f64(), Some(FEED_RING as f64));
        // Introspection is off: the operator is told of every kind but audit.
        let kinds = |c: &Value| -> Vec<String> {
            let feed = c["capabilities"]["extensions"]
                .as_array()
                .unwrap()
                .iter()
                .find(|e| e["uri"] == EVENTS_EXTENSION)
                .unwrap();
            serde_json::from_value(feed["params"]["kinds"].clone()).unwrap()
        };
        assert_eq!(
            kinds(&op),
            crate::runtime::surface::events::kinds_for(true, false)
        );
        assert!(kinds(&op).iter().any(|k| k == "auth"));
        assert!(!kinds(&op).iter().any(|k| k == "audit"));

        let us = card(&s, &wfs, CardView::Extended(&user));
        // A user is told only of what can reach it.
        assert_eq!(
            kinds(&us),
            crate::runtime::surface::events::kinds_for(false, false)
        );
        for never in ["auth", "audit", "status", "step"] {
            assert!(!kinds(&us).iter().any(|k| k == never), "{never}");
        }
        assert_eq!(
            skill_ids(&us),
            vec!["conversation", "workflow:greet", "workflow:report"]
        );
        let ops = ops_of(&us);
        assert!(ops.contains(&"workflow.run".to_string()), "{ops:?}");
        for never in ["admin.drain", "admin.set", "config", "debug.events"] {
            assert!(!ops.iter().any(|o| o == never), "{never} offered to a user");
        }
        // Introspection is off, so no caller is offered an introspection op.
        assert!(!ops_of(&op).iter().any(|o| o == "run.get"));
        let params = command_params(&us);
        assert!(params.get("settable").is_none(), "{params}");
        assert_eq!(
            params["commands"],
            json!([{"op": "report.make", "workflow": "report", "schema": {"type": "object"}}])
        );
        // Every op on either card is an op, and none is a skill.
        for c in [&op, &us] {
            for id in skill_ids(c) {
                assert!(op_spec(&id).is_none(), "the op {id} is a skill");
            }
        }
    }

    /// The security fields say exactly what the listener enforces, over the
    /// same posture table the resolver is held to — plus the rows only a
    /// card has an opinion on.
    ///
    /// The card used to declare no scheme at all while the listener demanded
    /// a bearer, a client certificate or a session token, and advertised an
    /// extended card on a listener where nobody could authenticate to read one.
    #[test]
    fn card_security_matrix() {
        let mut rows = crate::runtime::surface::auth::tests::postures();
        rows.push((
            "https + client_ca + an any rule",
            serde_json::from_value(json!({"listen": "https://0.0.0.0:8443",
                "url": "https://agent.example", "tls": {"cert": "c", "key": "k", "client_ca": "ca"},
                "principals": [{"id": "pub", "match": {"any": true}, "role": "user"}]}))
            .unwrap(),
            false,
        ));
        rows.push((
            "bearer + an any rule",
            serde_json::from_value(json!({"listen": "http://127.0.0.1:8080",
                "bearer": "{{secret:B}}",
                "principals": [{"id": "pub", "match": {"any": true}, "role": "user"}]}))
            .unwrap(),
            false,
        ));
        rows.push((
            "https device_grant + client_ca",
            serde_json::from_value(json!({"listen": "https://0.0.0.0:8443",
                "url": "https://agent.example/", "tls": {"cert": "c", "key": "k", "client_ca": "ca"},
                "device_grant": {"enabled": true, "scopes": ["user", "operator"]}}))
            .unwrap(),
            true,
        ));
        for (name, a2a, _) in rows {
            let posture = listener_auth_of(&a2a);
            let s = Settings {
                a2a,
                ..Default::default()
            };
            let c = card(&s, &no_workflows(), CardView::Public);
            let typed = assert_fixpoint(&c, name);

            let declared: Vec<&str> = c["securitySchemes"]
                .as_object()
                .map(|o| o.keys().map(String::as_str).collect())
                .unwrap_or_default();
            let reqs: Vec<Vec<&str>> = c["securityRequirements"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .map(|r| {
                            r["schemes"]
                                .as_object()
                                .map(|o| o.keys().map(String::as_str).collect())
                                .unwrap_or_default()
                        })
                        .collect()
                })
                .unwrap_or_default();

            assert_eq!(
                c["capabilities"]["extendedAgentCard"] == json!(true),
                posture.declares_any(),
                "{name}: extendedAgentCard is whether a scheme is declared: {c}"
            );
            assert_eq!(!declared.is_empty(), posture.declares_any(), "{name}: {c}");
            assert_eq!(
                c.get("securityRequirements").is_some(),
                posture.declares_any(),
                "{name}: requirements without schemes, or schemes without requirements: {c}"
            );
            assert_eq!(
                typed.security_requirements.is_empty(),
                !posture.declares_any(),
                "{name}"
            );
            for alt in &reqs {
                for key in alt {
                    assert!(
                        declared.contains(key),
                        "{name}: `{key}` is not declared: {c}"
                    );
                }
            }
            // The anonymous alternative exists exactly when an `any` rule
            // names the caller who presents nothing — and never under mTLS,
            // whose handshake demands a certificate first.
            assert_eq!(
                reqs.iter().any(Vec::is_empty),
                posture.any_rule && posture.declares_any() && !posture.mtls,
                "{name}: {c}"
            );
            if posture.mtls {
                assert!(
                    reqs.iter().all(|alt| alt.contains(&"mtls")),
                    "{name}: an alternative without the certificate: {c}"
                );
            }
            // Every mechanism the listener accepts is declared.
            assert_eq!(
                declared.contains(&"bearer"),
                posture.bearer || posture.device,
                "{name}"
            );
            assert_eq!(declared.contains(&"mtls"), posture.mtls, "{name}");
            assert_eq!(declared.contains(&"device_code"), posture.device, "{name}");
            if posture.device {
                let origin = origin_of(&url_of(&s.a2a)).unwrap();
                let oauth = &c["securitySchemes"]["device_code"]["oauth2SecurityScheme"];
                let flow = &oauth["flows"]["deviceCode"];
                assert_eq!(
                    flow["deviceAuthorizationUrl"],
                    crate::runtime::surface::auth::join(&origin, "/oauth2/device_authorization"),
                    "{name}"
                );
                assert_eq!(
                    flow["tokenUrl"],
                    crate::runtime::surface::auth::join(&origin, "/oauth2/token"),
                    "{name}"
                );
                for url in [&flow["deviceAuthorizationUrl"], &flow["tokenUrl"]] {
                    let rest = url.as_str().unwrap().split_once("://").unwrap().1;
                    assert!(!rest.contains("//"), "{name}: {url}");
                }
                for scope in &s.a2a.device_grant.scopes {
                    assert!(
                        flow["scopes"][scope.as_str()]
                            .as_str()
                            .is_some_and(|d| !d.is_empty()),
                        "{name}: scope {} undescribed: {c}",
                        scope.as_str()
                    );
                }
                assert_eq!(
                    oauth.get("oauth2MetadataUrl").is_some(),
                    origin.starts_with("https://"),
                    "{name}: metadata is pointed at only over TLS: {c}"
                );
            }
        }
    }

    /// The card and `--capabilities` declare the same extensions, and the
    /// listener negotiates against the same set.
    ///
    /// A peer reads the card; a controller reads the manifest; the listener
    /// activates. They came from separate lists, and disagreed the moment the
    /// feed was off: the card withheld the feed's extension while the
    /// manifest advertised it and the handshake echoed it back. All three now
    /// read the registry's declared set.
    #[test]
    fn the_card_and_the_manifest_declare_the_same_extensions() {
        use crate::runtime::surface::{Ext, TASK_ANNOTATIONS_EXTENSION, declared_when};
        for enabled in [false, true] {
            let s = settings(json!({"listen": "http://127.0.0.1:8080",
                "events": {"enabled": enabled}}));
            let on_card: Vec<String> = card(&s, &no_workflows(), CardView::Public)["capabilities"]
                ["extensions"]
                .as_array()
                .unwrap()
                .iter()
                .map(|e| e["uri"].as_str().unwrap().to_string())
                .collect();
            let in_manifest: Vec<String> =
                crate::runtime::surface::manifest::a2a_section(&s)["extensions"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|u| u.as_str().unwrap().to_string())
                    .collect();
            assert_eq!(on_card, in_manifest, "events={enabled}");
            let negotiated: Vec<&str> =
                declared_when(enabled).iter().map(|d| d.ext.uri()).collect();
            assert_eq!(on_card, negotiated, "events={enabled}");
            assert_eq!(
                on_card.iter().any(|u| u == EVENTS_EXTENSION),
                enabled,
                "the events extension follows a2a.events.enabled"
            );
            // The command and task-annotations extensions are unconditional:
            // whether a task carries its annotations is the caller's choice,
            // made by activating the extension, so the card always offers it.
            for always in [COMMAND_EXTENSION, TASK_ANNOTATIONS_EXTENSION] {
                assert!(on_card.iter().any(|u| u == always), "{always}");
            }
            // Registry order, which is the echo's order too.
            let order: Vec<&str> = Ext::ALL
                .iter()
                .map(|e| e.uri())
                .filter(|u| on_card.iter().any(|c| c == u))
                .collect();
            assert_eq!(on_card, order);
        }
    }
}
