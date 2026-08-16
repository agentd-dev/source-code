// SPDX-License-Identifier: Apache-2.0
//! The **A2A v2 surface** (RFC 0029): agentd 2.0's only external channel.
//! Principals + roles + authorization ([`principals`]) and durable tasks +
//! conversations ([`tasks`]). The transport binding (the HTTPS listener + the
//! command/NL/gate routing into the runtime) is wired in the runtime at the
//! P5 cut-over.

pub mod principals;
pub mod tasks;

pub use principals::{CallerIdentity, Principal, Resolver};
pub use tasks::{Link, State, Task};
