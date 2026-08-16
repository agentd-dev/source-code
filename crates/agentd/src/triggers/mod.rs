// SPDX-License-Identifier: Apache-2.0
//! Trigger primitives. agentd 2.0 removed the v1 supervisor mode drivers
//! (`mode`, `warm`, `router`); `timer` (cron) backs the v2 `schedule` start node.

// The 5-field cron parser, feature-gated (no deps). The v2 `schedule` start node
// (`runtime::starts`) uses `timer::CronExpr`.
#[cfg(feature = "cron")]
pub mod timer;
