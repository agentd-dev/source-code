//! Process-local counters → Prometheus text. RFC 0010 §metrics. [feature: metrics]
//!
//! Off the default path: the public `record_*` fns are **no-ops unless built
//! with `--features metrics`**, so call sites stay clean and the default build
//! pays nothing (metrics are otherwise derivable from the JSON-lines event
//! stream — that is the default story). With the feature, a tiny dependency-free
//! atomic registry backs an opt-in HTTP `/metrics` scrape surface (`obs::serve`).
//!
//! Counters are **per supervisor process**. The long-lived root daemon's surface
//! reflects the runs it supervises — every one-shot, reaction, and scheduled fire
//! flows through `supervise_once` — plus the tokens its *direct* children report
//! up the control channel. Nested subagents keep their own (process-local)
//! counters, still visible in their logs; cross-process metric rollup is a
//! deliberate non-goal for v1 (same boundary as the tree token ceiling).

/// Terminal disposition of one supervised run.
#[derive(Debug, Clone, Copy)]
pub enum RunOutcome {
    Completed,
    Failed,
    Killed,
}

/// A supervised run began (`supervise_once` entry).
pub fn record_run_started() {
    #[cfg(feature = "metrics")]
    imp::REGISTRY.runs_started.fetch_add(1, Ordering::Relaxed);
}

/// A supervised run reached a terminal disposition.
pub fn record_run(outcome: RunOutcome) {
    #[cfg(feature = "metrics")]
    imp::REGISTRY.record_run(outcome);
    #[cfg(not(feature = "metrics"))]
    let _ = outcome;
}

/// A reactive trigger fired (one reaction).
pub fn record_reaction() {
    #[cfg(feature = "metrics")]
    imp::REGISTRY.reactions.fetch_add(1, Ordering::Relaxed);
}

/// Tokens reported up by a direct child (`AgentMsg::Usage`).
pub fn record_tokens(input: u64, output: u64) {
    #[cfg(feature = "metrics")]
    imp::REGISTRY.record_tokens(input, output);
    #[cfg(not(feature = "metrics"))]
    let _ = (input, output);
}

/// The restart governor's circuit breaker tripped.
pub fn record_restart_tripped() {
    #[cfg(feature = "metrics")]
    imp::REGISTRY.restarts_tripped.fetch_add(1, Ordering::Relaxed);
}

/// Render the current counters as Prometheus text exposition format.
#[cfg(feature = "metrics")]
pub fn render_prometheus() -> String {
    imp::REGISTRY.render()
}

#[cfg(feature = "metrics")]
use std::sync::atomic::Ordering;

#[cfg(feature = "metrics")]
mod imp {
    use super::RunOutcome;
    use std::fmt::Write;
    use std::sync::atomic::{AtomicU64, Ordering};

    pub(super) static REGISTRY: Registry = Registry::new();

    pub(super) struct Registry {
        pub(super) runs_started: AtomicU64,
        runs_completed: AtomicU64,
        runs_failed: AtomicU64,
        runs_killed: AtomicU64,
        pub(super) reactions: AtomicU64,
        tokens_input: AtomicU64,
        tokens_output: AtomicU64,
        pub(super) restarts_tripped: AtomicU64,
    }

    impl Registry {
        const fn new() -> Registry {
            Registry {
                runs_started: AtomicU64::new(0),
                runs_completed: AtomicU64::new(0),
                runs_failed: AtomicU64::new(0),
                runs_killed: AtomicU64::new(0),
                reactions: AtomicU64::new(0),
                tokens_input: AtomicU64::new(0),
                tokens_output: AtomicU64::new(0),
                restarts_tripped: AtomicU64::new(0),
            }
        }

        pub(super) fn record_run(&self, outcome: RunOutcome) {
            let c = match outcome {
                RunOutcome::Completed => &self.runs_completed,
                RunOutcome::Failed => &self.runs_failed,
                RunOutcome::Killed => &self.runs_killed,
            };
            c.fetch_add(1, Ordering::Relaxed);
        }

        pub(super) fn record_tokens(&self, input: u64, output: u64) {
            self.tokens_input.fetch_add(input, Ordering::Relaxed);
            self.tokens_output.fetch_add(output, Ordering::Relaxed);
        }

        pub(super) fn render(&self) -> String {
            let mut s = String::new();
            let g = |a: &AtomicU64| a.load(Ordering::Relaxed);
            counter(&mut s, "agentd_runs_started_total", "Supervised runs started", g(&self.runs_started));
            counter(&mut s, "agentd_runs_completed_total", "Supervised runs that completed", g(&self.runs_completed));
            counter(&mut s, "agentd_runs_failed_total", "Supervised runs that failed on infra", g(&self.runs_failed));
            counter(&mut s, "agentd_runs_killed_total", "Supervised runs torn down by the supervisor", g(&self.runs_killed));
            counter(&mut s, "agentd_reactions_total", "Reactive triggers fired", g(&self.reactions));
            counter(&mut s, "agentd_tokens_input_total", "Input tokens reported by direct children", g(&self.tokens_input));
            counter(&mut s, "agentd_tokens_output_total", "Output tokens reported by direct children", g(&self.tokens_output));
            counter(&mut s, "agentd_restarts_tripped_total", "Restart-governor breaker trips", g(&self.restarts_tripped));
            s
        }
    }

    /// One counter family in Prometheus text exposition format.
    fn counter(s: &mut String, name: &str, help: &str, value: u64) {
        let _ = writeln!(s, "# HELP {name} {help}");
        let _ = writeln!(s, "# TYPE {name} counter");
        let _ = writeln!(s, "{name} {value}");
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn render_is_valid_prometheus_text() {
            let r = Registry::new();
            r.runs_started.fetch_add(3, Ordering::Relaxed);
            r.record_run(RunOutcome::Completed);
            r.record_run(RunOutcome::Failed);
            r.record_tokens(100, 50);
            let out = r.render();
            assert!(out.contains("# TYPE agentd_runs_started_total counter"));
            assert!(out.contains("agentd_runs_started_total 3"));
            assert!(out.contains("agentd_runs_completed_total 1"));
            assert!(out.contains("agentd_runs_failed_total 1"));
            assert!(out.contains("agentd_tokens_input_total 100"));
            assert!(out.contains("agentd_tokens_output_total 50"));
            // Every metric carries a HELP + TYPE header (8 counter families).
            assert_eq!(out.matches("# TYPE ").count(), 8);
            assert_eq!(out.matches(" counter\n").count(), 8);
        }
    }
}
