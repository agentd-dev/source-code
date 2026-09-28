// SPDX-License-Identifier: AGPL-3.0-only
//! The extension URIs agentd declares, and the methods they carry.

// ── A2A extensions (spec: docs/a2a-extensions.md) ───────────────────────────
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
/// handshake. Activation is not declaration: the echo is intersected with this
/// build-wide list and never consults `Settings`, so an instance that will not
/// serve the interface feed still recognises — and echoes back — the header
/// naming it. Whether the instance DECLARES the extension on its card is
/// `extensions_of`'s decision, and that one does read `Settings`.
pub const EXTENSIONS: &[&str] = &[
    COMMAND_EXTENSION,
    INTERFACE_EXTENSION,
    TASK_ANNOTATIONS_EXTENSION,
];

/// Every extension THIS instance declares on its card, in card order.
///
/// The card is a promise, so an instance that serves no observation feed must
/// not advertise it — and the `--capabilities` manifest must say the same
/// thing, since a controller reads one and a peer reads the other. They had
/// already drifted once over the command ops; this is the second list.
pub fn extensions_of(s: &crate::config::v2::Settings) -> Vec<&'static str> {
    let mut v = vec![COMMAND_EXTENSION];
    if s.a2a.events.enabled {
        v.push(INTERFACE_EXTENSION);
    }
    // Every task this instance projects carries its annotations under this
    // URI, whatever the switches: a display client reads a gate's askSchema
    // and a task's link only when the card names the extension, so leaving it
    // undeclared hid them from every client that checks before it reads.
    v.push(TASK_ANNOTATIONS_EXTENSION);
    v
}

/// The observation feed, as an A2A method extension.
pub const EVENTS_EXTENSION: &str = "https://agentd.dev/a2a/ext/events/v1";
/// The method [`EVENTS_EXTENSION`] declares, namespaced so it can never collide
/// with a method the specification defines later.
pub const EVENTS_METHOD: &str = "agentd.events/SubscribeToEvents";
/// The extension that declares agentd's own annotations on a task, so a peer
/// finds them under a URI it can look up rather than in ad-hoc metadata keys.
pub const TASK_ANNOTATIONS_EXTENSION: &str = "https://agentd.dev/a2a/ext/task-annotations/v1";
/// The protocol binding a unix-socket interface declares on the card: JSON-RPC,
/// but over a path no `https://` URL can name.
pub const UNIX_BINDING: &str = "https://agentd.dev/a2a/binding/jsonrpc-unix/v1";
