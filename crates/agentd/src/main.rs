//! `agentd` binary entry point.
//!
//! One binary, one entry point, no subcommand dispatch. Behaviour
//! is inferred from the loaded workflow (HTTP routes → server mode;
//! else one-shot). Overrides flow through CLI flags or `AGENTD_*`
//! environment variables. See `src/runtime.rs` for the resolver.

use std::process::ExitCode;

fn main() -> ExitCode {
    agentd::runtime::run(std::env::args().collect())
}
