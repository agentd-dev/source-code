// SPDX-License-Identifier: AGPL-3.0-only
//! **Server→client requests**: the host's answering seam.
//!
//! MCP is bidirectional. A server that declared the matching client capability
//! may send the client a *request* of its own — `elicitation/create`, asking the
//! human operator a question mid-call. The SDK receives it and answers the
//! protocol half (including `ping`, which both sides MUST answer); what it
//! cannot do is reach the human. That is this seam.
//!
//! The rules it encodes:
//!
//! * **The host owns the human.** `elicitation/create` is delegated to a
//!   [`Handler`] the embedder supplies (agentd routes it to `ask_human`, whose
//!   gates already render in every attached client and survive a restart). The
//!   crate never invents an answer.
//! * **Decline is not an error.** The elicitation schema has three outcomes —
//!   `accept`, `decline`, `cancel` — and a user who says no is a successful
//!   response carrying `"decline"`, not a JSON-RPC error.

use serde_json::Value;

/// A server→client request the host may be asked to answer.
///
/// Non-exhaustive because MCP grows server→client requests across revisions: a
/// host handles the ones it knows and answers `None` (declined) for the rest.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum Inbound {
    /// `elicitation/create` — the server needs input from the human operator.
    /// Carries the server's message and the requested-schema, verbatim.
    Elicit {
        message: String,
        requested_schema: Value,
    },
}

/// What the host decided. Mirrors the spec's elicitation outcomes so a refusal
/// is expressible without inventing content.
#[derive(Debug, Clone)]
pub enum Answer {
    /// The user answered; `content` matches the requested schema.
    Accept(Value),
    /// The user actively refused. Not an error.
    Decline,
    /// The user dismissed it without deciding (or nothing could ask).
    Cancel,
}

/// The host's answering surface. Implemented by the embedder.
pub trait Handler: Send + Sync {
    /// Answer a server→client request. `None` means nothing could ask; the
    /// SDK side reports that as `cancel`.
    fn handle(&self, req: Inbound) -> Option<Answer>;
}
