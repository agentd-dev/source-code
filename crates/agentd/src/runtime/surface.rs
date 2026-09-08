// SPDX-License-Identifier: AGPL-3.0-only
//! **What this instance offers over A2A** — the command ops and the extension
//! URIs that declare them.
//!
//! Always compiled, even in a build without the `a2a` feature, because
//! `--capabilities` reports this surface and that report is not feature-gated.
//! It lived inside the gated listener module until the feature matrix caught
//! it: the manifest referenced a module that a no-`a2a` build does not have.

use crate::config::v2::Settings;

// ── A2A extensions (spec: docs/topics/extensions.md) ────────────────────────
//
// Anything agentd speaks beyond the core protocol is declared as an
// `AgentExtension` on the card, identified by a URI. The spec's guidance on
// those URIs is followed here: they carry a VERSION (a breaking change takes a
// new URI rather than redefining this one), and they are identifiers — a peer
// is not expected to fetch them.

/// Structured operations invoked as a DataPart on `SendMessage`. Data-only in
/// the spec's taxonomy: it adds no method and changes no core structure, so it
/// is never `required` and a client that ignores it can still converse.
pub const COMMAND_EXTENSION: &str = "https://agentd.dev/a2a/ext/command/v1";
/// The instance-wide observation feed display clients render. A2A has no such
/// concept, so `SubscribeToEvents` is declared here as a method extension.
pub const INTERFACE_EXTENSION: &str = "https://agentd.dev/a2a/ext/interface/v1";
/// The methods agentd answers that A2A does not define, each paired with the
/// extension that declares it. Nothing may be served off this list:
/// `every_non_spec_method_is_declared_as_an_extension` is the check.
pub const EXTENSION_METHODS: &[(&str, &str)] = &[("SubscribeToEvents", INTERFACE_EXTENSION)];

/// Every extension URI this build can activate, for the `A2A-Extensions`
/// handshake. Activation is not declaration: an instance that will not serve
/// the interface feed still recognises the header naming it, and simply does
/// not echo it back.
pub const EXTENSIONS: &[&str] = &[COMMAND_EXTENSION, INTERFACE_EXTENSION];

/// Every extension THIS instance declares on its card, in card order.
///
/// The card is a promise, so an instance with the interface surface off must
/// not advertise it — and the `--capabilities` manifest must say the same
/// thing, since a controller reads one and a peer reads the other. They had
/// already drifted once over the command ops; this is the second list.
pub fn extensions_of(s: &crate::config::v2::Settings) -> Vec<&'static str> {
    let mut v = vec![COMMAND_EXTENSION];
    if s.interface.enabled {
        v.push(INTERFACE_EXTENSION);
    }
    v
}

/// Is `op` one of the built-in command ops?
///
/// The built-in surface is RESERVED: a workflow's `a2a` start node may not
/// declare a command that collides with one, and the listener dispatches a
/// built-in to its own handler even if one somehow does. Both halves matter —
/// the ops carry their own authorization (`admin.*` is operator-only), and a
/// declared command takes the inbox path where that check does not run. A
/// workflow able to claim `admin.drain` could quietly shadow an operator's
/// drain control, and where the model may create workflows, that author is
/// the model.
pub fn is_builtin_op(op: &str) -> bool {
    // Every op any build can serve, not just this instance's enabled subset:
    // reserving a name only while a feature is on would make the collision
    // appear the day `interface.enabled` flipped.
    const ALL: &[&str] = &[
        "status",
        "config",
        "config.set",
        "workflow.run",
        "workflow.status",
        "workflow.cancel",
        "workflow.signal",
        "subagent.send",
        "subagent.kill",
        "subagent.status",
        "subagent.get",
        "plan.get",
        "ask_human",
        "admin.drain",
        "admin.lameduck",
        "admin.pause",
        "admin.resume",
        "admin.cancel",
        "interface.info",
        "conversation.get",
        "run.get",
        "debug.events",
        "pairing.code",
    ];
    ALL.contains(&op)
}

/// The command ops an instance serves, in ONE place: the agent card renders
/// them as skills, the command extension declares them, and `--capabilities`
/// reports them. Three views, one list — they cannot disagree (they did: the
/// manifest listed ops the card never mentioned).
pub fn command_ops_of(s: &Settings) -> Vec<&'static str> {
    let mut ops = vec![
        "status",
        "config",
        "workflow.run",
        "workflow.status",
        "workflow.cancel",
        "workflow.signal",
        "subagent.send",
        "subagent.kill",
        "subagent.status",
        "plan.get",
        "admin.drain",
        "admin.lameduck",
        "admin.pause",
        "admin.resume",
        "admin.cancel",
    ];
    if s.interface.enabled {
        ops.push("interface.info");
        if s.interface.debug {
            ops.extend(["conversation.get", "run.get", "debug.events"]);
        }
    }
    ops
}
