// SPDX-License-Identifier: AGPL-3.0-only
//! The runtime's **event vocabulary**: everything the loop reacts to arrives
//! here — child frames, reaped children, executor results, timers, signals,
//! and the durable inbox events (A2A messages, start-node firings, signals).
//!
//! Inbox events are written to the store *before* they are handed to the loop,
//! so an event that has been accepted from the outside world survives a crash
//! and is replayed on restart rather than being lost between accept and act.

use crate::state::InboxEvent;
use crate::subagent::protocol::AgentMsg;
use crate::supervisor::reap::Reaped;
use crate::supervisor::tree::NodeId;
use serde_json::Value;

/// One loop event.
#[derive(Debug)]
pub enum Event {
    /// A frame from a child (turn worker / subagent).
    Child(NodeId, AgentMsg),
    /// A child exited (from the reaper).
    Reaped(Reaped),
    /// An executor thread finished a workflow step (`mcp.tool`, mapped tools…).
    StepDone {
        run: String,
        step: String,
        output: Value,
        is_error: bool,
        error: Option<String>,
        tokens: u64,
    },
    /// An executor thread finished a mapped/MCP call made on behalf of a child's
    /// tool request.
    ToolDone {
        node: NodeId,
        req: u64,
        result: Value,
        is_error: bool,
    },
    /// Knowledge auto-context retrieval finished for a staged turn.
    KnowledgeDone { job: u64, block: Option<String> },
    /// A background intelligence call finished. `id` names the caller
    /// (`goal.judge`, `human.judge:<gate>`) so the loop can route the result
    /// back to whoever asked without blocking on the call itself.
    Background { id: String, result: Value },
    /// A durable timer fired.
    TimerFired {
        id: String,
        owner: Value,
        payload: Value,
    },
    /// A durable event was accepted (already in the inbox).
    Inbox(InboxEvent),
    /// An A2A transport request awaiting a loop-computed reply. The transport
    /// thread parks on a channel until the loop answers, so every reply is
    /// computed against a single consistent view of runtime state.
    #[cfg(feature = "a2a")]
    A2a(Box<super::a2a_server::A2aRequest>),
    /// An inbound webhook awaiting a loop-computed reply.
    #[cfg(feature = "a2a")]
    Webhook(Box<super::webhooks::WebhookRequest>),
    /// A `subscribe` start node's notify-then-read finished off-loop.
    SubscribeRead {
        server: String,
        uri: String,
        content: Option<Value>,
    },
    /// An MCP Events request finished on the events worker pool.
    #[cfg(feature = "mcp-events")]
    McpEvents(Box<super::mcp_events::pool::Done>),
    /// A verified MCP Events webhook delivery, for the loop to keep.
    #[cfg(feature = "mcp-events")]
    McpEventHook(Box<super::mcp_events::hook::Arrival>),
    /// A probe of a server down since boot answered (`runtime::join`).
    McpReachable(Box<super::join::Reachable>),
    /// A Company call's result is about to reach the context `into`: its
    /// record gains `origin`, `company:<issuer>` (RFC 0047 §6.2). Sent by
    /// the call's executor thread just before the result itself, so the
    /// record is written before the result is read.
    CompanyRead {
        into: super::companies::Reads,
        origin: String,
        why: String,
    },
    /// A Company sign-in moved on (RFC 0047 §5.7): its gate is ready to open,
    /// or it ended. Sent by the sign-in's own thread.
    #[cfg(feature = "companies")]
    CompanySignIn {
        id: String,
        step: crate::companies::signin::Step,
    },
    /// A Direct Sign-In callback reached the A2A listener: its query, for
    /// the supervisor to check.
    #[cfg(feature = "companies")]
    CompanyCallback { query: String },
    /// A Company conversation's reader has events for the stream
    /// `company.<name>` (RFC 0047 §5.5.7), and waits to be told they are
    /// appended.
    #[cfg(feature = "companies")]
    CompanyDelivery(Box<crate::companies::conversation::Delivery>),
    /// A Company sign-out finished: who is owed its outcome, and the
    /// outcome.
    #[cfg(feature = "companies")]
    CompanySignedOut {
        owed: super::company_sign_in::Owed,
        company: String,
        by: Option<String>,
        outcome: Result<Value, String>,
    },
    /// The 200 ms tick.
    Tick,
}

/// The inbox event kinds the runtime understands.
pub mod kinds {
    /// A start node fired: `{workflow, node, payload, inputs}`.
    pub const START_FIRED: &str = "start_fired";
    /// An A2A message: `{context_id, task_id?, principal, role, parts, message_id}`.
    pub const A2A_MESSAGE: &str = "a2a_message";
    /// A named signal: `{name, payload, from}`.
    pub const SIGNAL: &str = "signal";
    /// A tool-driven request to run a workflow (`workflow.run`): `{workflow, inputs, start, requested_by}`.
    pub const WORKFLOW_RUN: &str = "workflow_run";
}
