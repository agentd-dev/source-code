//! Trigger adapters (RFC §7).
//!
//! Three first-class trigger modes. Two are already covered by
//! earlier phases:
//!
//! - **Manual** — explicit start-node invocation from the CLI
//!   (`agent run --start NAME`). The `[`crate::engine::Engine`]`
//!   is the whole API surface; no adapter required.
//! - **HTTP** — this module, behind the `trigger-http` Cargo
//!   feature. A hand-rolled HTTP/1.1 server maps routes to
//!   workflow start nodes, reads the JSON body into the trigger
//!   payload, and writes the `[`crate::engine::ExecutionOutcome`]`
//!   back as a JSON response.
//! - **MCP subscription** — not in Phase 6. Needs
//!   `resources/subscribe` support on the MCP client. Tracked as a
//!   follow-up once Phase 5's MCP surface grows subscription.

#[cfg(feature = "trigger-http")]
pub mod http;

#[cfg(all(feature = "trigger-http", feature = "server-tls"))]
pub mod http_tls;
