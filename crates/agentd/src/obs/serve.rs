//! Opt-in HTTP probe/scrape surface: `/metrics` + `/healthz` + `/readyz`.
//! RFC 0010 §health + §metrics; RFC 0016 §10 (fleet liveness). [feature: metrics]
//!
//! One blocking-accept thread, one thread-per-connection — no async, matching
//! the supervisor's processes-plus-threads model. GET-only, unauthenticated,
//! read-only; bound only when `--metrics-addr` is set (default off).
//!
//! `/healthz` (the k8s `livenessProbe` target, RFC 0016 §10) reuses the
//! **supervisor reactor heartbeat** (`obs::health::tick_age_ms`): a wedged
//! reactor reads unhealthy and k8s restarts the pod, while a healthy tree with
//! one *stuck subagent* keeps reading healthy (the reactor is the one detecting
//! and killing that child — RFC 0003 — so it is still ticking). `/readyz` (the
//! `readinessProbe` target) is the orthogonal lame-duck/drain gate and is left
//! intact below.

use crate::obs::log::Logger;
use serde_json::json;
use std::borrow::Cow;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::thread;
use std::time::Duration;

/// Liveness window for `/healthz`: a supervisor tick older than this — i.e. the
/// reactor loop has not made progress recently — reads unhealthy.
const STALE_AFTER_MS: u64 = 5_000;

/// Bind `addr` and serve probes on a background thread. Returns the bind error
/// (so the caller decides whether a failed bind is fatal). Accepts the bare
/// `:port` form (all interfaces) as well as an explicit `host:port`. RFC 0010.
pub fn spawn(addr: &str, log: Logger) -> std::io::Result<()> {
    let listener = TcpListener::bind(normalize_bind_addr(addr).as_ref())?;
    let local = listener.local_addr().ok();
    let bound = local
        .map(|a| a.to_string())
        .unwrap_or_else(|| addr.to_string());
    log.info(
        "metrics.serving",
        json!({"addr": bound, "endpoints": ["/metrics", "/healthz", "/readyz"]}),
    );
    // Make all-interfaces exposure visible in the logs, not just inferable from
    // the address. The surface is read-only + secret-free, but an operator who
    // typed `:port` should see that it is reachable off-host.
    if local.is_some_and(|a| a.ip().is_unspecified()) {
        log.warn(
            "metrics.bound_all_interfaces",
            json!({"addr": bound, "note": "read-only probe surface reachable on all interfaces; restrict via firewall/NetworkPolicy or bind 127.0.0.1:PORT"}),
        );
    }
    thread::Builder::new()
        .name("metrics-http".into())
        .spawn(move || {
            // `flatten` drops accept errors; one bad client never wedges the loop.
            for s in listener.incoming().flatten() {
                let _ = handle(s);
            }
        })?;
    Ok(())
}

/// Expand a bare `:port` to `0.0.0.0:port` — the "all interfaces, this port"
/// convention most servers accept, which `TcpListener::bind` otherwise rejects
/// (an empty host fails to resolve). `0.0.0.0` is all *IPv4* interfaces (the
/// conservative pick for an unauthenticated surface; it does not silently widen
/// to IPv6 — pass `[::]:port` for that). An explicit `host:port` (incl. a
/// bracketed IPv6 `[::]:port`) is passed through untouched, so it only ever
/// *adds* a host where one is missing.
fn normalize_bind_addr(addr: &str) -> Cow<'_, str> {
    if addr.starts_with(':') {
        Cow::Owned(format!("0.0.0.0{addr}"))
    } else {
        Cow::Borrowed(addr)
    }
}

fn handle(mut stream: TcpStream) -> std::io::Result<()> {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    let path = read_request_target(&mut stream)?;
    let (status, ctype, body) = route(&path);
    let head = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes())?;
    stream.write_all(body.as_bytes())?;
    stream.flush()
}

/// Route a request target to (status line, content-type, body).
fn route(path: &str) -> (&'static str, &'static str, String) {
    // Strip any `?query` — probes are path-only.
    let path = path.split('?').next().unwrap_or(path);
    match path {
        "/metrics" => (
            "200 OK",
            "text/plain; version=0.0.4",
            super::metrics::render_prometheus(),
        ),
        "/healthz" => health_response(),
        "/readyz" => readiness_response(),
        _ => ("404 Not Found", "text/plain", "not found\n".into()),
    }
}

/// Readiness (RFC 0010 §3.7 / RFC 0015 §4.2). The surface is bound only after
/// the daemon has initialized, so reaching it means the process is up. Readiness
/// is then overridden NotReady when the operator has lame-ducked the instance
/// (`lame-duck{ready:false}`) or a drain is in progress — both advertise "don't
/// route new work here" without the process necessarily exiting. The override is
/// toward NotReady only: clearing lame-duck restores Ready iff nothing else
/// (drain) holds it down.
fn readiness_response() -> (&'static str, &'static str, String) {
    let lame_duck = crate::signals::lame_duck();
    let draining = crate::signals::draining();
    if lame_duck || draining {
        (
            "503 Service Unavailable",
            "text/plain",
            format!("not ready lame_duck={lame_duck} draining={draining}\n"),
        )
    } else {
        ("200 OK", "text/plain", "ready\n".into())
    }
}

/// Liveness (RFC 0010 §3.7 / RFC 0016 §10). The verdict is a function of the
/// **supervisor reactor heartbeat** alone (`obs::health::tick_age_ms`) — a fresh
/// tick means the reactor loop is making progress, so the pod is live. It does
/// **not** consult any per-subagent stuck state: a wedged child is detected and
/// killed by the reactor itself (RFC 0003), which keeps ticking while it does,
/// so a stuck subagent never flips liveness and never costs a healthy tree its
/// pod. Only the *reactor* wedging (ticks stop, age grows past `STALE_AFTER_MS`)
/// — or a drain in progress — reads unhealthy so k8s restarts the pod.
fn health_response() -> (&'static str, &'static str, String) {
    let draining = crate::signals::draining();
    let age = crate::obs::health::tick_age_ms();
    if !draining && age < STALE_AFTER_MS {
        ("200 OK", "text/plain", format!("ok tick_age_ms={age}\n"))
    } else {
        (
            "503 Service Unavailable",
            "text/plain",
            format!("unhealthy draining={draining} tick_age_ms={age}\n"),
        )
    }
}

/// Read the request line and return its target path (GET assumed). Headers and
/// body are ignored — these are unauthenticated read-only probes.
fn read_request_target(stream: &mut TcpStream) -> std::io::Result<String> {
    let mut buf = [0u8; 1024];
    let n = stream.read(&mut buf)?;
    let head = String::from_utf8_lossy(&buf[..n]);
    let line = head.lines().next().unwrap_or("");
    // "GET /path HTTP/1.1" → the middle token.
    Ok(line.split_whitespace().nth(1).unwrap_or("/").to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn routes_known_and_unknown_paths() {
        let (s, ct, body) = route("/metrics");
        assert!(s.starts_with("200"));
        assert!(ct.contains("version=0.0.4"));
        assert!(body.contains("agentd_runs_started_total"));

        let (s, _, _) = route("/nope");
        assert!(s.starts_with("404"));
    }

    #[test]
    fn readyz_flips_503_under_lame_duck_then_clears() {
        let _g = crate::signals::test_guard();
        // Clean baseline (other tests share these process-global latches).
        crate::signals::set_lame_duck(false);
        // Pre-condition: not draining here (no SIGTERM in unit tests) → ready.
        if !crate::signals::draining() {
            let (s, _, body) = route("/readyz");
            assert_eq!(s, "200 OK", "baseline /readyz is ready");
            assert_eq!(body, "ready\n");
        }
        // Lame-duck → /readyz reports 503 while the process keeps running.
        crate::signals::set_lame_duck(true);
        let (s, _, body) = route("/readyz");
        assert_eq!(s, "503 Service Unavailable", "lame-duck → NotReady");
        assert!(body.contains("lame_duck=true"), "body: {body}");
        // Clearing the override restores readiness (nothing else holds it down).
        crate::signals::set_lame_duck(false);
        if !crate::signals::draining() {
            let (s, _, _) = route("/readyz");
            assert_eq!(s, "200 OK", "clearing lame-duck restores Ready");
        }
    }

    #[test]
    fn normalize_bind_addr_expands_bare_port_only() {
        // Bare :port → all interfaces.
        assert_eq!(normalize_bind_addr(":9090"), "0.0.0.0:9090");
        // Explicit hosts pass through untouched.
        assert_eq!(normalize_bind_addr("0.0.0.0:9090"), "0.0.0.0:9090");
        assert_eq!(normalize_bind_addr("127.0.0.1:9090"), "127.0.0.1:9090");
        assert_eq!(normalize_bind_addr("localhost:9090"), "localhost:9090");
        // IPv6 all-interfaces keeps its bracketed host (does not start with ':').
        assert_eq!(normalize_bind_addr("[::]:9090"), "[::]:9090");
    }

    #[test]
    fn bare_port_actually_binds() {
        // The regression: TcpListener::bind(":0") fails to resolve, but the
        // normalized form binds. Port 0 → an ephemeral port, so the test is
        // self-contained and never collides.
        let l = TcpListener::bind(normalize_bind_addr(":0").as_ref());
        assert!(l.is_ok(), "bare :port must bind after normalisation: {l:?}");
    }

    #[test]
    fn metrics_path_ignores_query_string() {
        let (s, _, _) = route("/metrics?foo=bar");
        assert!(s.starts_with("200"));
    }

    // The `/healthz` reactor-heartbeat contract (RFC 0016 §10). One test drives
    // the whole fresh→wedged→fresh transition sequentially: `LAST_TICK_MS` is a
    // single process-global the supervisor heartbeat shares, so the stale and
    // fresh phases must not be split across two `#[test]`s that the harness may
    // interleave in the same process (one's stale-stamp would race the other's
    // fresh-tick). The verdict consults *only* the reactor heartbeat — never any
    // per-subagent stuck state — so a healthy tree with one wedged leaf stays
    // live and keeps its pod; only the reactor itself wedging flips liveness.
    #[test]
    fn healthz_reflects_only_the_reactor_heartbeat() {
        let _sig = crate::signals::test_guard();
        // Hold the heartbeat lock so a concurrent test's `tick()` cannot race the
        // wedged-path assertion (the shared `LAST_TICK_MS` is process-global).
        let _g = crate::obs::health::HEARTBEAT_TEST_LOCK.lock().unwrap();
        crate::signals::set_lame_duck(false); // unrelated to liveness, but a shared latch
        if crate::signals::draining() {
            return; // a SIGTERM in the test process would dominate the verdict
        }
        // Fresh reactor tick → live.
        crate::obs::health::tick();
        let (s, _, body) = route("/healthz");
        assert_eq!(s, "200 OK", "fresh reactor tick → live");
        assert!(body.starts_with("ok tick_age_ms="), "body: {body}");

        // Wedged reactor (heartbeat aged past the window) → unhealthy so k8s
        // restarts the pod. Driven deterministically via the test seam.
        crate::obs::health::set_tick_age_for_test(STALE_AFTER_MS + 1_000);
        let (s, _, body) = route("/healthz");
        assert_eq!(s, "503 Service Unavailable", "wedged reactor → unhealthy");
        assert!(body.starts_with("unhealthy"), "body: {body}");

        // A renewed reactor tick restores liveness (and unpoisons the shared
        // heartbeat for any sibling test that reads it after this one).
        crate::obs::health::tick();
        let (s, _, _) = route("/healthz");
        assert_eq!(s, "200 OK", "a renewed reactor tick restores liveness");
    }
}
