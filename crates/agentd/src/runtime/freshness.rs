// SPDX-License-Identifier: AGPL-3.0-only
//! **§7.7 revocation — the freshness watch.** A signed instruction source's
//! authorization is CURRENT membership in the effective set, re-read on an
//! interval bounded by its `freshness`. This is the runtime hook the signing
//! logic (`config::attest`) needs: a durable periodic timer that RE-FETCHES the
//! instruction and, when the source goes unreachable past its deadline
//! (staleness, §7.7 rule 2), refuses NEW autonomous work while live work drains
//! (§5.5). A successful re-read clears the freeze (§7.7 rule 5 — only an
//! affirmative signal moves state).
//!
//! The verification and revocation DECISION logic is in `config::attest`
//! (`is_stale`, `must_recheck`, `families_retracted`); this module is the
//! reactor scheduling + the re-fetch + the new-work freeze.

use serde_json::{Value, json};

use crate::state::now_ms;

impl super::reactor::Runtime {
    /// Arm the freshness watch at startup/restore. A no-op unless
    /// `instruction_sources` pins a `freshness` and the instruction is
    /// re-fetchable (a resource URI). Idempotent — a restart re-arms.
    pub(crate) fn arm_freshness(&mut self) {
        let Some(every) = self.min_freshness_ms() else {
            return;
        };
        if self.instruction.uri.is_none() {
            return; // a static instruction has no source to re-read
        }
        let deadline = now_ms() + every;
        self.freshness_deadline_ms = Some(deadline);
        let _ = self.timers.arm(
            &self.durable,
            deadline,
            json!({"kind": "freshness"}),
            json!({}),
        );
        self.log.info("freshness.armed", json!({"every_ms": every}));
    }

    /// The shortest `freshness` interval across pinned sources, in ms.
    fn min_freshness_ms(&self) -> Option<u64> {
        min_freshness_ms(&self.settings.instruction_sources)
    }

    /// A freshness timer fired: re-read the instruction source. A successful
    /// re-read resets the deadline and clears any freeze; a source unreachable
    /// past the deadline freezes new work. Always re-arms the next check — a
    /// daemon keeps watching.
    pub(crate) fn on_freshness_check(&mut self, _payload: &Value) {
        let Some(every) = self.min_freshness_ms() else {
            return;
        };
        let now = now_ms();
        if let Some(uri) = self.instruction.uri.clone() {
            match self.subscribe_instruction(&uri) {
                Ok(()) => {
                    self.freshness_deadline_ms = Some(now + every);
                    if self.freshness_frozen {
                        self.freshness_frozen = false;
                        self.log.info("freshness.recovered", json!({"uri": uri}));
                        self.note_root(
                            "freshness.recovered: the signed instruction source is reachable \
                             again; new work resumes."
                                .into(),
                        );
                    }
                }
                Err(e) => {
                    let past = self.freshness_deadline_ms.is_some_and(|d| now >= d);
                    if past && !self.freshness_frozen {
                        self.freshness_frozen = true;
                        self.log.warn(
                            "freshness.stale",
                            json!({
                                "uri": uri,
                                "err": e,
                                "detail": "signed instruction source unreachable past its \
                                           freshness deadline — new work refused, live work \
                                           drains (§7.7)"
                            }),
                        );
                        self.note_root(
                            "freshness.stale: the signed instruction source could not be \
                             re-read before its freshness deadline; new work is refused until \
                             it is reachable again."
                                .into(),
                        );
                    }
                }
            }
        }
        let _ = self.timers.arm(
            &self.durable,
            now + every,
            json!({"kind": "freshness"}),
            json!({}),
        );
    }
}

/// The shortest `freshness` interval across pinned sources, in ms — the cadence
/// the watch re-checks at (§7.7). Sources without a `freshness` are ignored.
fn min_freshness_ms(sources: &[crate::config::v2::InstructionSource]) -> Option<u64> {
    sources
        .iter()
        .filter_map(|s| s.freshness.as_deref())
        .filter_map(|f| crate::config::parse_duration(f).ok())
        .map(|d| d.as_millis() as u64)
        .min()
}

#[cfg(test)]
mod tests {
    use super::min_freshness_ms;
    use crate::config::v2::InstructionSource;

    fn src(freshness: Option<&str>) -> InstructionSource {
        InstructionSource {
            uri: "instruction://x".into(),
            publisher: "p".into(),
            author_keys: vec![],
            delivery_keys: vec![],
            max_capabilities: vec![],
            freshness: freshness.map(str::to_string),
        }
    }

    #[test]
    fn the_watch_cadence_is_the_shortest_freshness() {
        // No sources, or none with a freshness → no watch.
        assert_eq!(min_freshness_ms(&[]), None);
        assert_eq!(min_freshness_ms(&[src(None)]), None);
        // The shortest interval wins (15m vs 1h → 15m).
        assert_eq!(
            min_freshness_ms(&[src(Some("1h")), src(Some("15m")), src(None)]),
            Some(900_000)
        );
    }
}
