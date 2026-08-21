// SPDX-License-Identifier: Apache-2.0
//! The runtime's **event vocabulary** (RFC 0026 §3): everything the loop reacts
//! to arrives here — child frames, reaped children, executor results, timers,
//! signals, and the durable inbox events (A2A messages, start-node firings,
//! signals) that are written ahead before being acted on.

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
    /// A background think/summarize finished (unused in P3; reserved).
    #[allow(dead_code)]
    Background { id: String, result: Value },
    /// A durable timer fired.
    TimerFired {
        id: String,
        owner: Value,
        payload: Value,
    },
    /// A durable event was accepted (already in the inbox).
    Inbox(InboxEvent),
    /// An A2A transport request awaiting a loop-computed reply (RFC 0029).
    #[cfg(feature = "a2a")]
    A2a(Box<super::a2a_server::A2aRequest>),
    /// An inbound webhook awaiting a loop-computed reply (RFC 0027).
    #[cfg(feature = "a2a")]
    Webhook(Box<super::webhooks::WebhookRequest>),
    /// A `subscribe` start node's notify-then-read finished off-loop.
    SubscribeRead {
        server: String,
        uri: String,
        content: Option<Value>,
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
    /// A2A control: `{op, args}`.
    pub const A2A_CONTROL: &str = "a2a_control";
    /// A tool-driven request to run a workflow (`workflow.run`): `{workflow, inputs, start, requested_by}`.
    pub const WORKFLOW_RUN: &str = "workflow_run";
}
