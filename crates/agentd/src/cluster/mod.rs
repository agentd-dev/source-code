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
//! The work-claim / lease convention (§3) is the cross-instance-ownership half of
//! this feature (RFC 0015 §5.6 froze the `work.*` contract): see [`claim`].
//! Standby mode (§7) and the mock work-server are a separate follow-up — this
//! module does not build them.

pub mod claim;
pub mod shard;

pub use claim::{ClaimOutcome, ClaimSpec, advertises_work_tools, claim, derive_claim_key};
pub use shard::{Shard, TimerShard};
