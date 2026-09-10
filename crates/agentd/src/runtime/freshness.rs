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
    /// `agent.instruction.trust` pins a `freshness` and the instruction is
    /// re-fetchable (a resource URI). Idempotent — a restart re-arms.
    pub(crate) fn arm_freshness(&mut self) {
        // Idempotent: a reload calls this again so a changed interval takes
        // effect, and arming twice would double the poll rate rather than
        // change it. Any existing freshness timer is disarmed first.
        for id in self.timers.armed_of_kind("freshness") {
            let _ = self.timers.disarm(&self.durable, &id);
        }
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

    /// The instruction source has been unreachable past its deadline. What
    /// that MEANS is the operator's call (`agent.instruction.unavailable`),
    /// because the right answer differs by deployment: a support agent should
    /// keep answering on the instruction it has; a deploy agent whose
    /// authorization may have been withdrawn should not.
    fn apply_unavailable_policy(&mut self, uri: &str, err: &str) {
        use crate::config::v2::InstructionUnavailable as P;
        // `auto`: a trust-pinned source FREEZES (§7.7 — a stale authorization
        // is a security matter); an unpinned one KEEPS (a failed poll on an
        // unsigned artifact is usually a blip, and the agent holds a good copy).
        let pinned = self.settings.agent.instruction_spec.trust.iter().any(|s| {
            if s.publisher.is_empty() {
                return false;
            }
            // A REGISTRY read names the document by the same uri the pin does,
            // and one config can pin several — so the pin has to match this
            // one. Every other transport carries the signature inside the
            // document, where there is nothing to match on: config load
            // verified those bytes against these pins, so the source is
            // pinned. Requiring the uri match there read an `oci://` source as
            // unpinned and quietly downgraded `auto` from freeze to keep.
            let registry = uri.starts_with("instruction://") || uri.starts_with("mcp://");
            !registry || uri.starts_with(s.uri.split('@').next().unwrap_or(&s.uri))
        });
        let policy = match self.settings.agent.instruction_spec.unavailable {
            P::Auto if pinned => P::Freeze,
            P::Auto => P::Keep,
            other => other,
        };
        self.log.warn(
            "instruction.unavailable",
            json!({"uri": uri, "err": err,
                   "policy": format!("{policy:?}").to_lowercase(),
                   "trust_pinned": pinned}),
        );
        match policy {
            // Deliberately nothing beyond the line above: the agent keeps
            // running on the last good instruction, and the log says so.
            P::Keep | P::Auto => {}
            P::Freeze => {
                self.freshness_frozen = true;
                self.note_root(
                    "instruction.unavailable: the instruction source could not be re-read \
                     before its deadline; new work is refused until it is reachable again."
                        .into(),
                );
            }
            P::Drain => {
                self.note_root(
                    "instruction.unavailable: the instruction source is unreachable; \
                     finishing live work and then exiting."
                        .into(),
                );
                self.begin_drain("instruction source unavailable");
            }
            P::Exit => {
                self.log.error(
                    "proc.exit",
                    json!({"code": crate::exit::MCP_REQUIRED_DOWN,
                           "err": format!("instruction source unavailable: {uri}")}),
                );
                self.exit = Some(crate::exit::MCP_REQUIRED_DOWN);
            }
        }
    }

    /// How often to re-read, in ms — `None` = never.
    ///
    /// Two inputs, and they mean different things. `agent.instruction.refresh`
    /// is a POLL interval: how current the operator wants the text.
    /// `agent.instruction.trust[].freshness` is the §7.7 revocation deadline: how
    /// long an authorization may go unconfirmed before the agent stops acting
    /// on it. When both are set the tighter one wins, because a poll that is
    /// slower than the deadline would let authorization expire between checks.
    fn min_freshness_ms(&self) -> Option<u64> {
        let revocation = min_freshness_ms(&self.settings.agent.instruction_spec.trust);
        let poll = match self.settings.agent.instruction_spec.refresh.as_deref() {
            None | Some("auto") => self.auto_refresh_ms(),
            Some("off") | Some("never") => None,
            Some(d) => crate::config::parse_duration(d)
                .ok()
                .map(|d| d.as_millis() as u64),
        };
        match (poll, revocation) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        }
    }

    /// The `auto` interval for THIS instruction's kind. Polling is only ever
    /// the right mechanism for a mutable remote source; everything else has a
    /// better one or nothing to do.
    fn auto_refresh_ms(&self) -> Option<u64> {
        /// A mutable remote reference, checked at this cadence by default.
        const DEFAULT_POLL_MS: u64 = 5 * 60 * 1000;
        let uri = self.instruction.uri.as_deref()?;
        // A digest-pinned artifact cannot change: re-reading it can only
        // return the bytes it already returned.
        if uri.contains("@sha256:") {
            return None;
        }
        match self.instruction.source {
            // A file is watched by inotify when `watch_config` is on; polling
            // it would be strictly worse (slower AND more work).
            "file" | "static" => None,
            _ => Some(DEFAULT_POLL_MS),
        }
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
                        self.apply_unavailable_policy(&uri, &e);
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
    use crate::config::v2::{InstructionSource, InstructionUnavailable as P};

    /// The `auto` policy resolves by whether the source is TRUST-pinned, which
    /// is the distinction that matters: a stale authorization is a security
    /// question (§7.7), an unreachable unsigned artifact is an availability
    /// one. Mirrors `apply_unavailable_policy`'s resolution.
    fn resolve(configured: P, pinned: bool) -> P {
        match configured {
            P::Auto if pinned => P::Freeze,
            P::Auto => P::Keep,
            other => other,
        }
    }

    #[test]
    fn auto_freezes_a_pinned_source_and_keeps_an_unpinned_one() {
        assert_eq!(resolve(P::Auto, true), P::Freeze, "pinned: §7.7 applies");
        assert_eq!(
            resolve(P::Auto, false),
            P::Keep,
            "unpinned: a blip, not a revocation"
        );
        // An explicit choice is never overridden by the pinning state — an
        // operator who says `keep` on a signed source means it.
        for p in [P::Keep, P::Freeze, P::Drain, P::Exit] {
            assert_eq!(resolve(p, true), p);
            assert_eq!(resolve(p, false), p);
        }
    }

    fn src(freshness: Option<&str>) -> InstructionSource {
        InstructionSource {
            uri: "instruction://x".into(),
            publisher: "p".into(),
            author_keys: vec![],
            delivery_keys: vec![],
            max_capabilities: vec![],
            reader: None,
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
