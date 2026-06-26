//! Health / liveness. RFC 0010 §health.
//!
//! Mode-aware (RFC 0010): one-shot uses the exit code; the long-lived daemon
//! modes (loop / reactive / schedule) expose **supervisor-heartbeat liveness**.
//! The key decision: liveness tracks whether the *supervisor* loop is making
//! progress — **idle is healthy**, and a stuck *subagent* must NOT fail the
//! daemon's liveness. So every hot loop (the daemon driver, the per-run reactor,
//! the interval sleep) calls [`tick`], bumping a process-global timestamp. A
//! background writer thread renders `{alive, supervisor_tick_age_ms, …}` to the
//! `--health-file` once a second; a K8s `exec` probe reads it. If the
//! supervisor wedges, ticks stop, the age grows past the threshold, and `alive`
//! flips to false — even though the writer keeps writing.
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
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

/// Record supervisor progress. Call from each supervisor hot loop.
pub fn tick() {
    LAST_TICK_MS.store(now_ms(), Ordering::Relaxed);
}

/// Age of the last tick (ms). Large ⇒ the supervisor loop is wedged.
pub fn tick_age_ms() -> u64 {
    now_ms().saturating_sub(LAST_TICK_MS.load(Ordering::Relaxed))
}

/// Spawn the health-file writer thread for a daemon. It writes once a second
/// until the process exits or a drain is requested (after which it writes a
/// final `draining` record and stops). `stale_after` is the liveness window.
pub fn spawn_writer(path: PathBuf, run_id: String, mode: String, stale_after: Duration) -> JoinHandle<()> {
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
        tick();
        assert!(tick_age_ms() < 1000, "a fresh tick should be young");
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
