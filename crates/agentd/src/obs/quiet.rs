// SPDX-License-Identifier: AGPL-3.0-only
//! What the log says about something that keeps failing: the first failure
//! at once, then at most one line a minute while it goes on failing, and one
//! line when it works again, also at most once a minute — so a thing that
//! fails every other attempt gets one line of each a minute, not one every
//! attempt. The OTLP exporter keeps one of these per signal
//! (`otel.export.failed`, `otel.export.recovered`), and the credentials one
//! per login target (`auth.unavailable`, `auth.recovered`).

use std::time::{Duration, Instant};

/// How long the log goes without repeating a line about one thing: a second
/// failure line while it keeps failing, or a second recovery line after it
/// fails again.
pub(crate) const FAILURE_QUIET: Duration = Duration::from_secs(60);

/// What the log has been told about one thing's attempts.
pub(crate) struct Health {
    /// Attempts lost since the last recovery line.
    pub(crate) lost: u64,
    /// When the last failure line was written; `None` while the log's last
    /// word on the thing is that it works.
    warned_at: Option<Instant>,
    /// When the last recovery line was written.
    recovered_at: Option<Instant>,
}

/// What one attempt's outcome has the log say. `failures` counts the attempts
/// lost since the last `Recovered`, this one included for `Failed`.
#[derive(Debug, PartialEq)]
pub(crate) enum Say {
    Nothing,
    Failed { failures: u64 },
    Recovered { failures: u64 },
}

impl Health {
    pub(crate) const fn new() -> Health {
        Health {
            lost: 0,
            warned_at: None,
            recovered_at: None,
        }
    }

    /// Record one attempt's outcome at `now`. A failure is said at once when
    /// the log last said the thing works, and otherwise once
    /// [`FAILURE_QUIET`] has passed since the last failure line. A success
    /// after a failure line is said once, but not within [`FAILURE_QUIET`] of
    /// the last recovery line: a thing that fails every other attempt gets
    /// one line of each a minute, and the log keeps saying it fails until a
    /// success comes after that minute.
    pub(crate) fn record(&mut self, ok: bool, now: Instant) -> Say {
        let quiet =
            |at: Option<Instant>| at.is_some_and(|at| now.duration_since(at) < FAILURE_QUIET);
        if ok {
            if self.warned_at.is_none() || quiet(self.recovered_at) {
                return Say::Nothing;
            }
            self.warned_at = None;
            self.recovered_at = Some(now);
            return Say::Recovered {
                failures: std::mem::take(&mut self.lost),
            };
        }
        self.lost += 1;
        if quiet(self.warned_at) {
            return Say::Nothing;
        }
        self.warned_at = Some(now);
        Say::Failed {
            failures: self.lost,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_failing_thing_is_named_once_a_minute_and_its_recovery_once() {
        let t0 = Instant::now();
        let mut h = Health::new();
        assert_eq!(h.record(true, t0), Say::Nothing, "a success says nothing");
        assert_eq!(h.record(false, t0), Say::Failed { failures: 1 });
        // Every attempt inside the quiet minute fails silently…
        for s in 1..60 {
            let at = t0 + Duration::from_secs(s);
            assert_eq!(h.record(false, at), Say::Nothing, "{s}s");
        }
        // …and the minute after the last line, the run is named again.
        assert_eq!(
            h.record(false, t0 + FAILURE_QUIET),
            Say::Failed { failures: 61 }
        );
        assert_eq!(
            h.record(false, t0 + FAILURE_QUIET + Duration::from_secs(1)),
            Say::Nothing
        );
        let back = t0 + FAILURE_QUIET + Duration::from_secs(2);
        assert_eq!(h.record(true, back), Say::Recovered { failures: 62 });
        assert_eq!(h.record(true, back), Say::Nothing);
        // A new run of failures is named at once, counted from one.
        assert_eq!(h.record(false, back), Say::Failed { failures: 1 });
    }

    #[test]
    fn a_thing_that_fails_every_other_attempt_is_named_once_a_minute() {
        let t0 = Instant::now();
        let at = |s: u64| t0 + Duration::from_secs(s);
        let mut h = Health::new();
        assert_eq!(h.record(false, at(0)), Say::Failed { failures: 1 });
        assert_eq!(h.record(true, at(5)), Say::Recovered { failures: 1 });
        // Failing again after the log said it works is said at once…
        assert_eq!(h.record(false, at(10)), Say::Failed { failures: 1 });
        // …and then, within the minute, neither line repeats.
        for s in (15..65).step_by(10) {
            assert_eq!(h.record(true, at(s)), Say::Nothing, "{s}s");
            assert_eq!(h.record(false, at(s + 5)), Say::Nothing, "{}s", s + 5);
        }
        // The first success a minute after the last recovery line says it,
        // counting every attempt lost since then.
        assert_eq!(h.record(true, at(65)), Say::Recovered { failures: 6 });

        // Over an hour of it: one line of each a minute, give or take one.
        let mut lines = 0;
        for s in (70..3670).step_by(5) {
            if h.record(s % 10 == 5, at(s)) != Say::Nothing {
                lines += 1;
            }
        }
        assert!((119..=121).contains(&lines), "{lines} lines in an hour");
    }
}
