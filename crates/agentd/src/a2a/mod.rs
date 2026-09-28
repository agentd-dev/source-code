// SPDX-License-Identifier: AGPL-3.0-only
//! The **A2A surface**: agentd's only external channel.
//! Principals + roles + authorization ([`principals`]) and durable tasks +
//! conversations ([`tasks`]). The transport binding — the HTTPS listener and
//! the command/NL/gate routing into the runtime — lives in the runtime, so
//! this module stays a pure model of who may call what and what a task is.

/// The A2A error vocabulary. Always compiled, so code outside the `a2a`
/// feature can name a code without a copy of it.
pub mod errors;
/// The authorization server the listener hosts.
#[cfg(feature = "a2a")]
pub mod oauth;
/// Talking to another agent: the outbound half, in the spec's types.
#[cfg(feature = "a2a")]
pub mod peer;
/// agentd's answers to the A2A specification's server ports.
#[cfg(feature = "a2a")]
pub mod ports;
pub mod principals;
/// Push notifications: telling a caller instead of making it watch.
#[cfg(feature = "a2a")]
pub mod push;
/// Message replies to the read ops.
#[cfg(feature = "a2a")]
pub mod reply;
/// The listener: identity in, protocol out.
#[cfg(feature = "a2a")]
pub mod serve;
pub mod tasks;
/// The wire projection, built from the specification's own types.
#[cfg(feature = "a2a")]
pub mod wire;

pub use principals::{Evidence, Principal, Resolution, Resolver, Via};
pub use tasks::{Link, State, Task};
