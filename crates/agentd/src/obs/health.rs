//! Health / liveness. RFC 0010 §health; RFC 0016 §10 (the fleet-liveness
//! contract reading).
//!
//! Mode-aware (RFC 0010): one-shot uses the exit code; the long-lived daemon
//! modes (loop / reactive / schedule) expose **supervisor-heartbeat liveness**.
//! The key decision (RFC 0016 §10): *a live PID is not a live agent* — liveness
//! tracks whether the **supervisor reactor loop** is making progress, not
//! whether the process exists. **Idle is healthy** (the reactor wakes on every
//! `recv_timeout` expiry and [`tick`]s, so a daemon idling for hours stays
//! live), and a **stuck *subagent* must NOT fail the daemon's liveness**: the
//! reactor is the thing that *detects and kills* a wedged child (RFC 0003's
//! 3-detector model, `supervisor::liveness` + the kill ladder), so while it does
//! so it is by definition still ticking — failing pod liveness there would
//! destroy a whole healthy tree for one wedged leaf.
//!
//! Concretely: every reactor hot loop (the daemon driver, the per-run reactor,
//! the interval sleep) calls [`tick`], bumping a process-global timestamp. The
//! per-child stuck-detector (`supervisor::liveness`) is a **separate** clock
//! that never touches this one — it drives the kill ladder, not pod liveness.
//! A background writer thread renders `{alive, supervisor_tick_age_ms, …}` to
//! the `--health-file` once a second; a K8s `exec` probe reads it. If the
//! reactor itself wedges, ticks stop, the age grows past the threshold, and
//! `alive` flips to false — even though the writer keeps writing — so k8s
//! restarts the pod (RFC 0016 §10).
//!
//! Default surface = exit code + `--health-file`. The opt-in `/healthz`+`/readyz`
//! HTTP surface (feature `metrics`, `obs::serve`) reuses this same heartbeat
//! liveness over the hand-rolled HTTP server.

use crate::obs::log::rfc3339_millis;
use serde_json::json;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Last time a supervisor loop proved progress (ms since epoch). Bumped from
/// every hot loop; read by the writer thread. Cheap (one relaxed store).
static LAST_TICK_MS: AtomicU64 = AtomicU64::new(0);

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Record supervisor progress. Call from each supervisor hot loop.
pub fn tick() {
    LAST_TICK_MS.store(now_ms(), Ordering::Relaxed);
}

/// Age of the last tick (ms). Large ⇒ the supervisor reactor loop is wedged
/// (RFC 0016 §10). This is the sole input to the `/healthz` liveness verdict;
/// no per-subagent state feeds it.
pub fn tick_age_ms() -> u64 {
    now_ms().saturating_sub(LAST_TICK_MS.load(Ordering::Relaxed))
}

/// Test-only seam: stamp the heartbeat at `ms_ago` in the past, so a test can
/// exercise the *wedged-reactor* path (`tick_age_ms() ≈ ms_ago`) deterministically
/// without sleeping. Saturates at the epoch. Not compiled into release builds.
#[cfg(test)]
pub(crate) fn set_tick_age_for_test(ms_ago: u64) {
    LAST_TICK_MS.store(now_ms().saturating_sub(ms_ago), Ordering::Relaxed);
}

/// Test-only serialization for the process-global heartbeat. Any test that
/// *mutates* `LAST_TICK_MS` (via [`tick`] or [`set_tick_age_for_test`]) and then
/// asserts on the age must hold this lock, so a concurrent test's `tick()` can't
/// race a wedged-path assertion. Mirrors `obs::log`'s `STDERR_LOCK` pattern.
#[cfg(test)]
pub(crate) static HEARTBEAT_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Spawn the health-file writer thread for a daemon. It writes once a second
/// until the process exits or a drain is requested (after which it writes a
/// final `draining` record and stops). `stale_after` is the liveness window.
pub fn spawn_writer(
    path: PathBuf,
    run_id: String,
    mode: String,
    stale_after: Duration,
) -> JoinHandle<()> {
    tick(); // seed so the first write looks alive
    std::thread::Builder::new()
        .name("health-writer".into())
        .spawn(move || {
            let stale = stale_after.as_millis() as u64;
            loop {
                let draining = crate::signals::draining();
                let age = tick_age_ms();
                let body = json!({
                    "ts": rfc3339_millis(SystemTime::now()),
                    "run_id": run_id,
                    "mode": mode,
                    "supervisor_tick_age_ms": age,
                    "alive": !draining && age < stale,
                    "draining": draining,
                });
                let _ = write_atomic(&path, body.to_string().as_bytes());
                if draining {
                    return; // last record written; daemon is winding down
                }
                std::thread::sleep(Duration::from_secs(1));
            }
        })
        .expect("spawn health writer")
}

/// Write `bytes` to `path` atomically (temp + rename) so a probe never reads a
/// torn file.
fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let tmp = PathBuf::from(format!("{}.tmp", path.display()));
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tick_freshens_the_age() {
        let _g = HEARTBEAT_TEST_LOCK.lock().unwrap(); // serialize heartbeat reads/writes
        tick();
        assert!(tick_age_ms() < 1000, "a fresh tick should be young");
    }

    #[test]
    fn set_tick_age_for_test_ages_the_heartbeat() {
        let _g = HEARTBEAT_TEST_LOCK.lock().unwrap();
        set_tick_age_for_test(60_000);
        let age = tick_age_ms();
        assert!(
            (59_000..=61_000).contains(&age),
            "seam should age the tick: {age}"
        );
        tick(); // restore a fresh heartbeat for any sibling reader
    }

    #[test]
    fn write_atomic_renders_valid_json() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("health.json");
        write_atomic(&path, br#"{"alive":true}"#).unwrap();
        let read = std::fs::read_to_string(&path).unwrap();
        let v: serde_json::Value = serde_json::from_str(&read).unwrap();
        assert_eq!(v["alive"], true);
        // no leftover temp file
        assert!(!path.with_file_name("health.json.tmp").exists());
    }
}
