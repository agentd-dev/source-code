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
/// The pre-1.14 spelling of the interface extension, still declared so a
/// display client pinned to it keeps working for one minor.
pub const INTERFACE_EXTENSION_LEGACY: &str = "urn:agentd:interface";
/// The DEPRECATED `a2a.*` admin methods. Declared because agentd still answers
/// them — a card that hid them would be lying about the surface — with the
/// description pointing at where they went.
pub const ADMIN_METHODS_EXTENSION: &str = "https://agentd.dev/a2a/ext/admin-methods/v1";

/// The methods agentd answers that A2A does not define, each paired with the
/// extension that declares it. Nothing may be served off this list:
/// `every_non_spec_method_is_declared_as_an_extension` is the check.
pub const EXTENSION_METHODS: &[(&str, &str)] = &[
    ("SubscribeToEvents", INTERFACE_EXTENSION),
    ("a2a.drain", ADMIN_METHODS_EXTENSION),
    ("a2a.lameduck", ADMIN_METHODS_EXTENSION),
    ("a2a.pause", ADMIN_METHODS_EXTENSION),
    ("a2a.resume", ADMIN_METHODS_EXTENSION),
    ("a2a.cancel", ADMIN_METHODS_EXTENSION),
];

/// Every extension URI this build can activate, for the `A2A-Extensions`
/// handshake.
pub const EXTENSIONS: &[&str] = &[
    COMMAND_EXTENSION,
    INTERFACE_EXTENSION,
    INTERFACE_EXTENSION_LEGACY,
    ADMIN_METHODS_EXTENSION,
];

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
