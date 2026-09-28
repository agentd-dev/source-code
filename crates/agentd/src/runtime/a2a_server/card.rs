// SPDX-License-Identifier: AGPL-3.0-only
//! The agent card: public, extended, and the command ops rendered as skills.

use super::{COMMAND_EXTENSION, INTERFACE_EXTENSION, command_ops_of, err_obj, extensions_of};
use crate::a2a::Principal;
use crate::runtime::reactor::Runtime;
use serde_json::{Value, json};

/// One line per command op, for the card, from the op table. A skill without
/// a description is a skill a caller has to guess at.
fn command_description(op: &str) -> &'static str {
    crate::runtime::surface::op_spec(op).map_or("", |s| s.description)
}

/// The command ops as A2A skills, from settings alone.
fn command_skills_of(settings: &crate::config::v2::Settings) -> Vec<Value> {
    command_ops_of(settings)
        .into_iter()
        .map(|op| {
            let tag = if crate::runtime::surface::is_admin_op(op) {
                "admin"
            } else {
                "command"
            };
            json!({
                "id": op,
                "name": op,
                "description": command_description(op),
                "tags": ["command", tag],
                "inputModes": ["application/json"],
                "outputModes": ["application/json"],
            })
        })
        .collect()
}

impl Runtime {
    /// The command ops as A2A skills.
    fn command_skills(&self) -> Vec<Value> {
        command_skills_of(&self.settings)
    }

    /// The A2A agent card: served over `GetAgentCard`, and unauthenticated on
    /// GET at `/.well-known/agent-card.json` and `/.well-known/agent.json` —
    /// discovery is public by both of its conventional paths.
    pub(super) fn a2a_agent_card(&self) -> Value {
        let skills: Vec<Value> = self
            .workflows
            .values()
            .map(|w| json!({"id": w.name, "name": w.name, "description": w.description.clone().unwrap_or_default(), "tags": ["workflow"]}))
            .collect();
        // The commands are skills too. A skill is A2A's own answer to "what can
        // I ask this agent to do", so a stock client discovers `workflow.run`
        // and `admin.drain` the same way it discovers a workflow — and the
        // extended card below narrows the list to what the CALLER may run.
        agent_card_of(&self.settings, skills, self.command_skills())
    }

    /// The **authenticated** card: the public one, plus what only a named
    /// caller may be told.
    ///
    /// The public card lists every workflow as a skill because discovery has to
    /// work before anyone is authenticated. This one lists the workflows *this
    /// principal may actually run*, which is the useful answer — a caller that
    /// reads a skill here can call it.
    pub(super) fn a2a_extended_card(&self, principal: &Principal) -> Value {
        if principal.is_anonymous() {
            return err_obj(-32007, "the extended card requires an authenticated caller");
        }
        let mut card = self.a2a_agent_card();
        let mut skills: Vec<Value> = self
            .workflows
            .values()
            .filter(|w| Runtime::may_run(principal, w))
            .map(|w| json!({"id": w.name, "name": w.name, "description": w.description.clone().unwrap_or_default(), "tags": ["workflow"]}))
            .collect();
        // …and the ops THIS caller may run. An operator sees the admin family;
        // a `user` does not, and that is the answer to "which tools may I
        // call" without a second protocol.
        skills.extend(self.command_skills().into_iter().filter(|sk| {
            sk["id"]
                .as_str()
                .is_some_and(|op| principal.may_command(op))
        }));
        card["skills"] = json!(skills);
        card
    }
}

/// The card, from settings and a skill list alone — no `Runtime`.
///
/// Free-standing so it can be round-tripped through the SDK's typed
/// `AgentCard` in a unit test, which is the precondition for serving the card
/// THROUGH `a2a_rs` rather than beside it.
fn agent_card_of(
    settings: &crate::config::v2::Settings,
    mut skills: Vec<Value>,
    command_skills: Vec<Value>,
) -> Value {
    skills.extend(command_skills);
    // The card is a promise, so `pushNotifications` tracks whether this
    // instance will actually accept a webhook rather than whether the code
    // exists (conformance checks both directions of that).
    // Exactly the four fields `AgentCapabilities` carries in the A2A
    // protobuf: streaming, pushNotifications, extensions, extendedAgentCard.
    // `stateTransitionHistory` used to sit here and is not a field of the
    // current message — it was dropped on every typed round trip, silently.
    let mut capabilities = json!({
        "streaming": true,
        "pushNotifications": settings.a2a.push.enabled,
        "extendedAgentCard": true,
    });
    // What agentd speaks beyond the A2A core, declared the way the protocol
    // provides for (`AgentExtension`): a URI a peer can recognise, and
    // params it can act on. Everything callable is reachable through
    // `SendMessage` with a command DataPart, so a client that ignores the
    // extension entirely can still converse — `required` is false.
    // One list decides WHICH extensions this instance declares
    // (`extensions_of`, shared with `--capabilities`); this only decides
    // how each is described.
    let mut extensions = vec![json!({
        "uri": COMMAND_EXTENSION,
        "description": "Structured operations invoked as a DataPart on SendMessage: \
                        {\"data\": {\"agentd\": {\"op\": \"…\", …}}}. \
                        The ops this caller may run are its skills on the extended card.",
        "required": false,
        "params": {"ops": command_ops_of(settings), "dataPartKey": "agentd"},
    })];
    // Advertise the interface surface so a display client can discover it
    // before authenticating. The card is public, so only the on/off bit
    // rides here.
    if extensions_of(settings).contains(&INTERFACE_EXTENSION) {
        extensions.push(json!({
            "uri": INTERFACE_EXTENSION,
            "description": "The instance-wide observation feed display clients render. \
                            A2A has no instance feed, so the method is declared here.",
            "required": false,
            "params": {"enabled": true, "methods": ["SubscribeToEvents"]},
        }));
    }
    capabilities["extensions"] = json!(extensions);
    let url = settings.a2a.listen.clone().unwrap_or_default();
    let card = json!({
        "name": "agentd",
        "description": "A durable agent (agentd) — conversations, workflows, and subagents over A2A.",
        "version": crate::VERSION,
        // How a peer reaches this instance. `supportedInterfaces` is the
        // field `AgentCard` carries; the flat `url`/`preferredTransport`
        // trio that used to sit beside it was the pre-interfaces spelling,
        // is in neither the message nor anything that reads a card in this
        // repository, and was dropped by every typed round trip anyway.
        "supportedInterfaces": [
            {"url": url, "protocolBinding": "JSONRPC", "protocolVersion": "0.3.0"}
        ],
        "capabilities": capabilities,
        "defaultInputModes": ["text/plain", "application/json"],
        "defaultOutputModes": ["text/plain", "application/json"],
        "skills": skills,
    });
    // Emit what the SDK emits. The card is a protocol document, and the only
    // authority on its wire form is the type generated from the A2A protobuf:
    // proto3 JSON omits a field at its default, so `required: false` on an
    // extension is ABSENT on a conformant wire and a peer reads absence as
    // false. Hand-written JSON was right in content and non-canonical in shape.
    //
    // A conversion failure would mean the card is not an `AgentCard`, which the
    // unit test makes impossible; were it ever to happen, sending the
    // hand-built form beats sending nothing.
    match serde_json::from_value::<a2a_rs::domain::AgentCard>(card.clone())
        .ok()
        .and_then(|c| serde_json::to_value(c).ok())
    {
        Some(canonical) => canonical,
        None => card,
    }
}

#[cfg(test)]
mod tests {
    use super::super::EXTENSIONS;
    use super::*;

    /// The card survives the SDK's typed `AgentCard` without losing a field.
    ///
    /// This is the precondition for serving the card THROUGH `a2a_rs` rather
    /// than beside it. It used not to hold: the card carried
    /// `protocolVersion`/`url`/`preferredTransport` and a
    /// `stateTransitionHistory` capability, none of which are fields of the
    /// A2A protobuf, so a typed round trip silently dropped them — and that
    /// loss was the stated reason for hand-serving the card in the first
    /// place. Emitting what the message actually has removes the reason.
    #[test]
    fn the_card_round_trips_through_the_sdks_typed_agent_card() {
        let mut s = crate::config::v2::Settings::default();
        s.a2a.listen = Some("https://agent.example:8443".into());
        s.a2a.events.enabled = true;
        let ours = agent_card_of(&s, Vec::new(), command_skills_of(&s));

        // The card IS the SDK's serialization now, so it must parse back into
        // the SDK's type unchanged — the fixpoint that proves nothing we set is
        // outside the message.
        let typed: a2a_rs::domain::AgentCard =
            serde_json::from_value(ours.clone()).expect("the card is an AgentCard");
        let back = serde_json::to_value(&typed).expect("and serializes again");
        assert_eq!(ours, back, "the card is not a fixpoint of the SDK's type");

        // Every field the A2A message REQUIRES is present and populated.
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
            assert!(
                !ours[key].is_null(),
                "the card omits the required field `{key}`: {ours}"
            );
        }
        // …and nothing the message does NOT have. These four were emitted for
        // years and dropped by every typed reader: three are the pre-interfaces
        // card spelling, and `stateTransitionHistory` left `AgentCapabilities`.
        for gone in ["protocolVersion", "url", "preferredTransport"] {
            assert!(
                ours.get(gone).is_none(),
                "`{gone}` is not an AgentCard field"
            );
        }
        assert!(
            ours["capabilities"].get("stateTransitionHistory").is_none(),
            "`stateTransitionHistory` is not an AgentCapabilities field"
        );
        // And the extensions we declare are still declared after the trip.
        let exts = back["capabilities"]["extensions"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        for uri in extensions_of(&s) {
            assert!(
                exts.iter().any(|e| e["uri"] == uri),
                "{uri} lost in the round trip: {back}"
            );
        }
    }

    /// The card and `--capabilities` declare the same extensions.
    ///
    /// A peer reads the card; a controller reads the manifest. They came from
    /// two lists, and disagreed the moment the feed was off: the card
    /// correctly withheld the interface extension while the manifest
    /// advertised it unconditionally. Both now read `extensions_of`.
    #[test]
    fn the_card_and_the_manifest_declare_the_same_extensions() {
        for enabled in [false, true] {
            let mut s = crate::config::v2::Settings::default();
            s.a2a.events.enabled = enabled;
            let declared = extensions_of(&s);
            assert_eq!(
                declared.contains(&INTERFACE_EXTENSION),
                enabled,
                "the interface extension follows a2a.events.enabled"
            );
            assert!(
                declared.contains(&COMMAND_EXTENSION),
                "the command extension is unconditional"
            );
            // Anything declarable must be activatable, or the handshake would
            // drop a URI the card just advertised.
            for uri in &declared {
                assert!(EXTENSIONS.contains(uri), "{uri} is not activatable");
            }
        }
    }
}
