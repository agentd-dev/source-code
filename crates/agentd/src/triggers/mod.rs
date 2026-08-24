// SPDX-License-Identifier: AGPL-3.0-only
//! Trigger primitives: the cron schedule parser behind the `schedule` start
//! node.

// The 5-field cron parser, feature-gated (no deps). The `schedule` start node
// (`runtime::starts`) uses `timer::CronExpr`.
#[cfg(feature = "cron")]
pub mod timer;
