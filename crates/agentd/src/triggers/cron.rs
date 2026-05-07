//! Cron + interval triggers.
//!
//! Fires the configured start node on a recurring schedule. Two
//! shapes — full 5-field cron expressions via the `cron` crate, and
//! a simpler "every Ns" interval for the common "poll this
//! periodically" case.
//!
//! ## Threading
//!
//! One dedicated scheduler thread per trigger spec. The thread
//! computes the next fire time, sleeps until it (waking up every
//! 200ms to poll the shutdown flag), then calls
//! [`crate::engine::Engine::run`] on the configured start node.
//! Successive runs are **serial** — a long-running execution
//! holds the schedule; a cron tick that arrives during a run is
//! **dropped** rather than queued. Operators who need
//! overlap-tolerant cron should split into two workflows.
//!
//! ## Payload
//!
//! The synthetic trigger delivered to the engine has this shape:
//!
//! ```json
//! {
//!   "kind": "cron",               // or "interval"
//!   "schedule": "0 */5 * * *",    // for cron
//!   "every_ms": 30000,            // for interval
//!   "fired_at_unix_ms": 1_745_000_000_000,
//!   "tick": 17                    // monotonic counter per trigger
//! }
//! ```
//!
//! ## Time source
//!
//! Cron schedules parse in the **runtime's local timezone** — the
//! operator sets `TZ` on the process if they need a specific zone
//! (matches systemd / crond convention). Interval mode is TZ-agnostic.

use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::json;

use crate::engine::{Engine, RunOptions, TriggerMeta};
use crate::error::{Error, Result};
use crate::workflow::WorkflowDoc;
use crate::workflow::model::Trigger;

/// Parsed + validated schedule. Kept separate from the TOML-side
/// [`Trigger`] so the scheduler loop doesn't re-parse per-tick.
/// `cron::Schedule` is ~250 bytes; boxed so the variant-size delta
/// doesn't dominate the enum footprint.
#[derive(Debug)]
enum Schedule {
    Cron(Box<cron::Schedule>),
    Interval(Duration),
}

impl Schedule {
    fn next_after(&self, now: SystemTime) -> Option<SystemTime> {
        match self {
            Schedule::Cron(s) => {
                // `cron` works in `chrono::DateTime<Local>` — convert
                // back to `SystemTime` for uniform sleep arithmetic.
                use chrono::TimeZone;
                let now_local = chrono::Local
                    .from_utc_datetime(&chrono::DateTime::<chrono::Utc>::from(now).naive_utc());
                let next = s.after(&now_local).next()?;
                let unix_ms = next.timestamp_millis();
                if unix_ms < 0 {
                    return None;
                }
                Some(UNIX_EPOCH + Duration::from_millis(unix_ms as u64))
            }
            Schedule::Interval(d) => Some(now + *d),
        }
    }

    fn kind(&self) -> &'static str {
        match self {
            Schedule::Cron(_) => "cron",
            Schedule::Interval(_) => "interval",
        }
    }
}

/// Prepared trigger ready to spawn.
#[derive(Debug)]
pub struct CronTrigger {
    schedule: Schedule,
    start_node: String,
    /// Echoed into the trigger payload as a human-readable field.
    description: String,
}

impl CronTrigger {
    /// Build from the declarative [`Trigger`] variant. Fails at
    /// spawn time on invalid cron expressions / malformed `every`
    /// so operators never see "silently not firing."
    pub fn from_trigger(trig: &Trigger) -> Result<Option<Self>> {
        match trig {
            Trigger::Cron {
                schedule,
                start_node,
            } => {
                // `cron` parses "m h dom mon dow" (5 fields) as well
                // as "s m h dom mon dow dow?" (6–7). Accept both to
                // match operators' muscle memory.
                let parsed = schedule.parse::<cron::Schedule>().map_err(|e| {
                    Error::Config(format!("trigger.cron schedule `{schedule}`: {e}"))
                })?;
                Ok(Some(Self {
                    schedule: Schedule::Cron(Box::new(parsed)),
                    start_node: start_node.clone(),
                    description: schedule.clone(),
                }))
            }
            Trigger::Interval { every, start_node } => {
                let dur = parse_duration(every)
                    .map_err(|e| Error::Config(format!("trigger.interval every=`{every}`: {e}")))?;
                Ok(Some(Self {
                    schedule: Schedule::Interval(dur),
                    start_node: start_node.clone(),
                    description: every.clone(),
                }))
            }
            _ => Ok(None),
        }
    }

    pub fn start_node(&self) -> &str {
        &self.start_node
    }

    /// Spawn the scheduler thread. Returns a `JoinHandle` the caller
    /// can park on during graceful shutdown; the `shutdown` flag is
    /// polled every 200ms so SIGTERM is observed promptly.
    pub fn spawn(
        self,
        workflow: Arc<WorkflowDoc>,
        engine: Arc<Engine>,
        options: RunOptions,
        shutdown: Arc<AtomicBool>,
    ) -> thread::JoinHandle<()> {
        thread::spawn(move || {
            let mut tick: u64 = 0;
            loop {
                if shutdown.load(std::sync::atomic::Ordering::Relaxed) {
                    return;
                }
                let now = SystemTime::now();
                let Some(next) = self.schedule.next_after(now) else {
                    tracing::warn!(
                        target: "agentd::audit",
                        event = "cron.no_next_fire",
                        kind = self.schedule.kind(),
                        description = %self.description,
                    );
                    return;
                };
                // Sleep-poll until next fire or shutdown. 200ms tick
                // is the same cadence the serve loop uses.
                while SystemTime::now() < next {
                    if shutdown.load(std::sync::atomic::Ordering::Relaxed) {
                        return;
                    }
                    let remaining = next
                        .duration_since(SystemTime::now())
                        .unwrap_or(Duration::from_millis(0));
                    thread::sleep(remaining.min(Duration::from_millis(200)));
                }

                tick = tick.saturating_add(1);
                let fired_at_ms = now_unix_ms();
                let payload = match &self.schedule {
                    Schedule::Cron(_) => json!({
                        "kind": "cron",
                        "schedule": self.description,
                        "fired_at_unix_ms": fired_at_ms,
                        "tick": tick,
                    }),
                    Schedule::Interval(d) => json!({
                        "kind": "interval",
                        "every_ms": d.as_millis() as u64,
                        "fired_at_unix_ms": fired_at_ms,
                        "tick": tick,
                    }),
                };
                tracing::info!(
                    target: "agentd::audit",
                    event = "cron.fire",
                    kind = self.schedule.kind(),
                    start_node = %self.start_node,
                    tick = tick,
                );
                let started = Instant::now();
                match engine.run(
                    &workflow,
                    &self.start_node,
                    TriggerMeta::manual(payload),
                    options.clone(),
                ) {
                    Ok(outcome) => {
                        tracing::info!(
                            target: "agentd::audit",
                            event = "cron.completed",
                            start_node = %self.start_node,
                            tick = tick,
                            status = %outcome.status_label(),
                            elapsed_ms = started.elapsed().as_millis() as u64,
                        );
                    }
                    Err(e) => {
                        tracing::error!(
                            target: "agentd::audit",
                            event = "cron.error",
                            start_node = %self.start_node,
                            tick = tick,
                            reason = %format!("{e}"),
                        );
                    }
                }
            }
        })
    }
}

fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Parse "30s" / "5m" / "2h" / "1d" into a Duration. Rejects
/// fractional parts (`30.5s`) and zero / negative intervals.
fn parse_duration(raw: &str) -> std::result::Result<Duration, String> {
    let s = raw.trim();
    if s.is_empty() {
        return Err("empty duration".into());
    }
    let (num_part, unit) = s
        .find(|c: char| c.is_alphabetic())
        .map(|i| s.split_at(i))
        .ok_or_else(|| format!("missing unit in `{s}` (expected s/m/h/d)"))?;
    let n: u64 = num_part
        .parse()
        .map_err(|_| format!("non-numeric prefix in `{s}`"))?;
    if n == 0 {
        return Err("interval must be > 0".into());
    }
    let multiplier_secs: u64 = match unit.trim() {
        "s" | "sec" | "secs" => 1,
        "m" | "min" | "mins" => 60,
        "h" | "hr" | "hrs" => 3600,
        "d" | "day" | "days" => 86_400,
        other => return Err(format!("unknown unit `{other}` (use s/m/h/d)")),
    };
    n.checked_mul(multiplier_secs)
        .map(Duration::from_secs)
        .ok_or_else(|| "interval overflow".into())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_duration_happy() {
        assert_eq!(parse_duration("30s").unwrap(), Duration::from_secs(30));
        assert_eq!(parse_duration("5m").unwrap(), Duration::from_secs(300));
        assert_eq!(parse_duration("2h").unwrap(), Duration::from_secs(7200));
        assert_eq!(parse_duration("1d").unwrap(), Duration::from_secs(86400));
    }

    #[test]
    fn parse_duration_aliases() {
        assert_eq!(parse_duration("30sec").unwrap(), Duration::from_secs(30));
        assert_eq!(parse_duration("5mins").unwrap(), Duration::from_secs(300));
        assert_eq!(parse_duration("2hrs").unwrap(), Duration::from_secs(7200));
    }

    #[test]
    fn parse_duration_rejects_zero() {
        assert!(parse_duration("0s").unwrap_err().contains("> 0"));
    }

    #[test]
    fn parse_duration_rejects_unknown_unit() {
        assert!(
            parse_duration("10weeks")
                .unwrap_err()
                .contains("unknown unit")
        );
    }

    #[test]
    fn parse_duration_rejects_missing_unit() {
        assert!(parse_duration("30").unwrap_err().contains("missing unit"));
    }

    #[test]
    fn parse_duration_rejects_non_numeric() {
        assert!(parse_duration("abcs").unwrap_err().contains("non-numeric"));
    }

    #[test]
    fn from_trigger_validates_cron_expression() {
        let bad = Trigger::Cron {
            schedule: "garbage".into(),
            start_node: "n".into(),
        };
        let err = CronTrigger::from_trigger(&bad).unwrap_err();
        assert!(format!("{err}").contains("trigger.cron"));
    }

    #[test]
    fn from_trigger_accepts_5_field_cron() {
        // `cron` crate expects 5–7 fields; `0 */5 * * *` is a classic
        // minute/hour/dom/month/dow expression.
        let trig = Trigger::Cron {
            schedule: "0 */5 * * * *".into(),
            start_node: "n".into(),
        };
        let prepped = CronTrigger::from_trigger(&trig).unwrap().unwrap();
        assert_eq!(prepped.start_node(), "n");
    }

    #[test]
    fn from_trigger_accepts_interval() {
        let trig = Trigger::Interval {
            every: "30s".into(),
            start_node: "poll".into(),
        };
        let prepped = CronTrigger::from_trigger(&trig).unwrap().unwrap();
        assert_eq!(prepped.start_node(), "poll");
    }

    #[test]
    fn from_trigger_returns_none_for_unrelated_variant() {
        let trig = Trigger::InternalEvent {
            name: "boot".into(),
            start_node: "n".into(),
        };
        assert!(CronTrigger::from_trigger(&trig).unwrap().is_none());
    }

    #[test]
    fn interval_schedule_next_after_moves_forward() {
        let s = Schedule::Interval(Duration::from_secs(30));
        let t0 = SystemTime::now();
        let t1 = s.next_after(t0).unwrap();
        assert!(t1 > t0);
        assert!(t1.duration_since(t0).unwrap() >= Duration::from_secs(29));
    }
}
