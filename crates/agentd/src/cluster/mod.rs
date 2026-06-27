//! Horizontal scaling — sharding + autoscaling signals + the capacity surface
//! (RFC 0019). [feature = "cluster"]
//!
//! This is the dependency-free, deterministic core of RFC 0019 Phase A:
//!   * [`shard`] — the `--shard K/N` predicate (a hand-rolled FNV-1a partition),
//!     applied at reactive routing intake before any spawn (RFC 0019 §4);
//!   * the autoscaling signal set lives in [`crate::obs::metrics`] (RFC 0019 §5);
//!   * the `agentd://capacity` read surface lives in [`crate::mcp::server`]
//!     (RFC 0019 §9), gated behind serve-mcp like the other served resources.
//!
//! The work-claim / lease convention (§3) and standby mode (§7) are **deferred**
//! (RFC 0019 §12, the `work.*` tool contract is not yet frozen) — this module
//! advertises neither.

pub mod shard;

pub use shard::{Shard, TimerShard};
