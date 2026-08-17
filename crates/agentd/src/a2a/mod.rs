// SPDX-License-Identifier: AGPL-3.0-only
//! The **A2A v2 surface** (RFC 0029): agentd 2.0's only external channel.
//! Principals + roles + authorization ([`principals`]) and durable tasks +
//! conversations ([`tasks`]). The transport binding (the HTTPS listener + the
//! command/NL/gate routing into the runtime) is wired in the runtime at the
//! P5 cut-over.

/// Talking to another agent: the outbound half, in the spec's types.
#[cfg(feature = "a2a")]
pub mod peer;
/// agentd's answers to the A2A specification's server ports.
#[cfg(feature = "a2a")]
pub mod ports;
pub mod principals;
/// The listener: identity in, protocol out.
#[cfg(feature = "a2a")]
pub mod serve;
pub mod tasks;
/// The wire projection, built from the specification's own types.
#[cfg(feature = "a2a")]
pub mod wire;

pub use principals::{CallerIdentity, Principal, Resolver};
pub use tasks::{Link, State, Task};
