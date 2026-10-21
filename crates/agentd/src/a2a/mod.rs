// SPDX-License-Identifier: AGPL-3.0-only
//! The **A2A surface**: agentd's only external channel.
//! Principals + roles + authorization ([`principals`]) and durable tasks +
//! conversations ([`tasks`]). The transport binding — the HTTPS listener and
//! the command/NL/gate routing into the runtime — lives in the runtime, so
//! this module stays a pure model of who may call what and what a task is.

/// An instance child's own channel to its parent, handed over at spawn.
/// Always compiled, so the instance tier makes one with or without a
/// listener of its own.
pub mod channel;
/// The A2A error vocabulary. Always compiled, so code outside the `a2a`
/// feature can name a code without a copy of it.
pub mod errors;
/// Signing in at the IdP on the listener origin (RFC 0044 §5.4).
#[cfg(feature = "a2a")]
pub mod login;
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
/// Where a unix-socket listener binds. Always compiled, so the instance tier
/// can choose a socket its child's listener will bind.
pub mod socket;
pub mod tasks;
/// The wire projection, built from the specification's own types.
#[cfg(feature = "a2a")]
pub mod wire;

pub use principals::{Evidence, Principal, Resolution, Resolver, Via};
pub use tasks::{Link, State, Task};
