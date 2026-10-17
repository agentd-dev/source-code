// SPDX-License-Identifier: AGPL-3.0-only
//! What the log says about the credentials agentd presents.
//!
//! A credential that cannot be produced — no login cached, a token expired
//! with nothing to renew it, an issuer or an AWS source that does not answer,
//! a token file that cannot be read — fails each request that needs it,
//! unsent, and the operator learns of it here: `auth.unavailable` (warn)
//! names the target and the cause when its requests start failing, then at
//! most once a minute while they keep failing, and `auth.recovered` (info)
//! says when one succeeds again, also at most once a minute (`obs::quiet`).
//! The supervisor keeps the one record for the process tree: a child — a
//! turn worker, a subagent — hands its outcomes up
//! ([`crate::subagent::protocol::AgentMsg::Credential`]) instead of saying
//! them, so a credential every turn worker fails on is still one line a
//! minute, not one a turn.

use std::collections::BTreeMap;
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

use serde_json::json;

use crate::obs::log::Logger;
use crate::obs::quiet::{Health, Say};

/// How a child hands one outcome to the supervisor: the target and, for a
/// failure, its cause.
type HandUp = Box<dyn Fn(&str, Option<&str>) + Send + Sync>;

/// Where this process's credential outcomes go, attached once at its start.
enum Sink {
    /// The supervisor's: the record is kept here and said on this logger.
    Log(Logger),
    /// A child's: each outcome that is news to the supervisor is handed to
    /// it.
    Up(HandUp),
}

static SINK: OnceLock<Sink> = OnceLock::new();

/// Keep this process's record and say it on `log`: the supervisor's. The
/// first call, of this or [`hand_up`], wins.
pub fn set_logger(log: &Logger) {
    let _ = SINK.set(Sink::Log(log.clone()));
}

/// Hand this child's outcomes to `up`, which sends them to the supervisor:
/// every failure, so the supervisor counts each, and a success when it is
/// news — the target's first in this process, or its first since a failure.
/// The first call, of this or [`set_logger`], wins.
pub(crate) fn hand_up(up: impl Fn(&str, Option<&str>) + Send + Sync + 'static) {
    let _ = SINK.set(Sink::Up(Box::new(up)));
}

/// The record per login target, of the attempts to produce its credential.
static TARGETS: Mutex<BTreeMap<String, Health>> = Mutex::new(BTreeMap::new());

/// What a child last handed up per target: `true` for a success.
static HANDED: Mutex<BTreeMap<String, bool>> = Mutex::new(BTreeMap::new());

/// Record one attempt to produce `target`'s credential and pass its outcome
/// on, the error naming the target. A success for a target that never failed
/// costs a lookup.
pub(crate) fn outcome<T>(target: &str, attempt: Result<T, String>) -> Result<T, String> {
    let err = attempt.as_ref().err().map(String::as_str);
    match SINK.get() {
        Some(Sink::Up(up)) => {
            let mut handed = HANDED.lock().unwrap_or_else(|e| e.into_inner());
            // Handed up under the lock, so the supervisor hears a target's
            // outcomes in the order they were decided here.
            if is_news(&mut handed, target, err.is_none()) {
                up(target, err);
            }
        }
        Some(Sink::Log(log)) => record(Some(log), target, err),
        None => record(None, target, err),
    }
    attempt.map_err(|cause| format!("{target}: {cause}"))
}

/// An outcome a child handed up ([`crate::subagent::protocol::AgentMsg::Credential`]):
/// recorded and said as this process's own.
pub(crate) fn heard(target: &str, err: Option<&str>) {
    let log = match SINK.get() {
        Some(Sink::Log(log)) => Some(log),
        _ => None,
    };
    record(log, target, err);
}

/// Whether a child's outcome for `target` is news to the supervisor, kept in
/// `handed`: a failure always is; a success when the target has none handed
/// up yet, or its last was a failure.
fn is_news(handed: &mut BTreeMap<String, bool>, target: &str, ok: bool) -> bool {
    match handed.get_mut(target) {
        Some(last) => {
            let news = !ok || !*last;
            *last = ok;
            news
        }
        None => {
            handed.insert(target.to_string(), ok);
            true
        }
    }
}

/// Record one outcome for `target` (`err` is a failure's cause) and say what
/// it has the log say, on `log` when there is one.
fn record(log: Option<&Logger>, target: &str, err: Option<&str>) {
    let now = Instant::now();
    // The verdict is taken under the lock and said after it.
    let say = {
        let mut targets = TARGETS.lock().unwrap_or_else(|e| e.into_inner());
        match (err, targets.get_mut(target)) {
            (None, None) => Say::Nothing,
            (None, Some(h)) => h.record(true, now),
            (Some(_), Some(h)) => h.record(false, now),
            (Some(_), None) => targets
                .entry(target.to_string())
                .or_insert_with(Health::new)
                .record(false, now),
        }
    };
    if let Some(log) = log {
        match (&say, err) {
            (Say::Failed { failures }, Some(cause)) => log.warn(
                "auth.unavailable",
                json!({"target": target, "err": cause, "failures": failures}),
            ),
            (Say::Recovered { failures }, _) => log.info(
                "auth.recovered",
                json!({"target": target, "failures": failures}),
            ),
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The error names its target; the record is per target, so a target
    /// that keeps failing is not said again within the minute while another
    /// target's first failure is.
    #[test]
    fn a_failure_names_its_target_and_is_kept_per_target() {
        let target = "test:report-a";
        let other = "test:report-b";
        let failed = |t: &str| {
            outcome::<()>(t, Err("no cached credential".into()))
                .expect_err("a failure stays a failure")
        };
        assert_eq!(failed(target), "test:report-a: no cached credential");
        failed(target);
        failed(other);
        let lost = |t: &str| {
            TARGETS
                .lock()
                .unwrap()
                .get(t)
                .map(|h| h.lost)
                .unwrap_or_default()
        };
        assert_eq!(lost(target), 2, "both failures counted for the target");
        assert_eq!(lost(other), 1, "the other target has its own record");
        // A success passes through untouched.
        assert_eq!(outcome(target, Ok(7)), Ok(7));
        // The success recovered the target: the next failure is a new run.
        failed(target);
        assert_eq!(lost(target), 1);
    }

    /// A child hands up every failure, so the supervisor counts each, and a
    /// success only when it is news: the target's first, or its first since
    /// a failure. A turn worker's thousandth signed request costs no frame.
    #[test]
    fn a_child_hands_up_each_failure_and_a_success_that_is_news() {
        let mut handed = BTreeMap::new();
        let t = "mcp:s";
        assert!(is_news(&mut handed, t, true), "the target's first success");
        assert!(!is_news(&mut handed, t, true), "a second is not news");
        assert!(is_news(&mut handed, t, false), "a failure always is");
        assert!(is_news(&mut handed, t, false), "and the next one too");
        assert!(is_news(&mut handed, t, true), "the first success after it");
        assert!(!is_news(&mut handed, t, true));
        assert!(
            is_news(&mut handed, "intelligence", false),
            "a target's first failure"
        );
        assert!(is_news(&mut handed, "intelligence", true));
    }

    /// What children hand up lands in the supervisor's one record: a failure
    /// each of two workers met counts twice for the target, and one worker's
    /// success recovers it.
    #[test]
    fn a_childs_outcome_is_kept_in_the_supervisors_record() {
        let t = "test:report-heard";
        let lost = || TARGETS.lock().unwrap().get(t).map(|h| h.lost);
        heard(t, None);
        assert_eq!(lost(), None, "a success for a target that never failed");
        heard(t, Some("AWS_ACCESS_KEY_ID is not set"));
        heard(t, Some("AWS_ACCESS_KEY_ID is not set"));
        assert_eq!(lost(), Some(2), "one record for both workers' failures");
        heard(t, None);
        assert_eq!(lost(), Some(0), "a success recovered it");
    }
}
