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
//! this feature (RFC 0015 §5.6 froze the `work.*` contract): see [`claim`] — both
//! spawn-claim and continue-claim are wired into the reactor. Standby (§7) is also
//! built (a claim-pull route over an assignment channel, in `config`/`triggers`).
//! The only deferred bits are the `claim.style=resource` CAS path (a documented
//! stub — the CAS wire contract isn't frozen) and a standby warm-child pool; the
//! mock work-server used to conformance-test the convention lives in the
//! `agentd-conformance` crate, not here.

pub mod claim;
pub mod shard;

pub use claim::{
    ClaimOutcome, ClaimSpec, advertises_work_tools, claim, claim_styled, derive_claim_key,
};
pub use shard::{Shard, TimerShard};
