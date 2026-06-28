// SPDX-License-Identifier: Apache-2.0
//! The inotify file-watch reload trigger (RFC 0017 §5.2).
//!
//! A `--watch-config`-armed, dependency-free (raw `libc` inotify) watch that sets
//! the SAME `RELOAD` latch SIGHUP does — so a Kubernetes ConfigMap volume update
//! reloads with no signal plumbing. Both triggers funnel into the one reload
//! routine (RFC 0017 §5.2: "there is one code path"); this module only *sets the
//! latch* (attributed `trigger:"watch"`), never re-implements the reload.
//!
//! ## Why we watch the DIRECTORY, not the file
//!
//! A Kubernetes ConfigMap volume is a tree of symlinks: `…/config.json` →
//! `..data/config.json`, and `..data` is itself a symlink to a timestamped
//! directory. On an update the kubelet writes a NEW timestamped directory and
//! **atomically renames** `..data` to point at it (a directory-symlink swap).
//! The original file *inode* is never written — so an inotify watch on the file
//! itself sees nothing. We therefore watch the file's **parent directory** for
//! the create/move/close events that the swap (and a plain in-place edit)
//! produce, and fire when an event names our file's basename.
//!
//! ## Re-arm after the swap
//!
//! When the projected directory is swapped, the kernel may deliver `IN_IGNORED`
//! and drop the watch (the watched directory's identity changed). We re-add the
//! watch on the (stable) parent path so a SECOND ConfigMap update still fires —
//! this is the subtle correctness bit for the symlink-swap projection.
//!
//! The watcher is **best-effort**: a fatal inotify error logs `config.watch.error`
//! and exits the thread (SIGHUP still works); it NEVER kills the daemon.

#![cfg(all(unix, feature = "config-watch"))]

use crate::obs::log::Logger;
use serde_json::json;
use std::path::Path;

/// The inotify event mask we arm on the config file's parent directory. Covers
/// both the ConfigMap atomic-swap case (`IN_MOVED_TO`/`IN_CREATE` of the new
/// projection, `IN_MOVED_FROM`/`IN_DELETE` of the old) and the plain in-place
/// edit case (`IN_CLOSE_WRITE` when an editor writes the file directly).
#[cfg(any(target_os = "linux", target_os = "android"))]
const WATCH_MASK: u32 = libc::IN_CLOSE_WRITE
    | libc::IN_MOVED_TO
    | libc::IN_CREATE
    | libc::IN_MOVED_FROM
    | libc::IN_DELETE;

/// One parsed inotify record: the event `mask` and the optional `name` (the file
/// within the watched directory the event is about; `None` when the record
/// carries no name, e.g. an `IN_IGNORED` on the watch itself).
type Event = (u32, Option<String>);

/// Parse a buffer returned by one `read(2)` on an inotify fd into its event
/// records. PURE — no syscalls — so it is the unit-testable core of the watcher
/// (the FFI read is the only untestable part). A read can return MULTIPLE
/// variable-length records back-to-back: each is a fixed 16-byte header
/// (`wd: i32, mask: u32, cookie: u32, len: u32`) followed by `len` bytes of NUL-
/// padded name. We read the header fields by byte offset (no pointer-cast /
/// alignment assumption — the buffer is a `read` target with no alignment
/// guarantee), then slice `len` bytes of name and trim at the first NUL.
///
/// A truncated trailing record (a short/partial read mid-record) is skipped
/// rather than panicking — defensive, though inotify guarantees whole records.
pub fn parse_events(buf: &[u8]) -> Vec<Event> {
    const HEADER: usize = 16; // i32 + u32 + u32 + u32, packed (no tail padding)
    let mut out = Vec::new();
    let mut off = 0usize;
    while off + HEADER <= buf.len() {
        // mask is bytes [4..8); len is bytes [12..16). Little/native-endian per
        // the kernel ABI — `from_ne_bytes` matches how the kernel wrote them.
        let mask = u32::from_ne_bytes([buf[off + 4], buf[off + 5], buf[off + 6], buf[off + 7]]);
        let len = u32::from_ne_bytes([buf[off + 12], buf[off + 13], buf[off + 14], buf[off + 15]])
            as usize;
        let name_start = off + HEADER;
        let name_end = name_start + len;
        if name_end > buf.len() {
            break; // truncated trailing record — stop (defensive)
        }
        let name = if len == 0 {
            None
        } else {
            let raw = &buf[name_start..name_end];
            // The name is NUL-terminated and NUL-padded to an alignment boundary.
            let nul = raw.iter().position(|&b| b == 0).unwrap_or(raw.len());
            // Lossy: a config file path on a sane volume is UTF-8; a non-UTF-8
            // name simply won't match our (UTF-8) basename, which is correct.
            Some(String::from_utf8_lossy(&raw[..nul]).into_owned())
        };
        out.push((mask, name));
        off = name_end;
    }
    out
}

/// Spawn the dedicated inotify watcher thread (RFC 0017 §5.2). Returns
/// immediately; the thread lives for the process. A blocking `read` loop on its
/// own thread is the simplest correct shape — the supervisor reactor is never
/// blocked by it, and on a config change the thread sets the `RELOAD` latch +
/// wakes the reactor exactly as SIGHUP does (attributed `trigger:"watch"`).
///
/// `config_path` is the resolved config file path (`--config`/`AGENTD_CONFIG`).
/// We watch its parent directory and fire on its basename.
pub fn spawn_config_watcher(config_path: &Path, log: &Logger) {
    let path = config_path.to_path_buf();
    let log = log.clone();
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map(Path::to_path_buf)
        .unwrap_or_else(|| Path::new(".").to_path_buf());
    let basename = match path.file_name().map(|n| n.to_string_lossy().into_owned()) {
        Some(b) => b,
        None => {
            log.warn(
                "config.watch.error",
                json!({"err": "config path has no file name", "path": path.display().to_string()}),
            );
            return;
        }
    };
    log.info(
        "config.watch.armed",
        json!({"path": path.display().to_string(), "dir": parent.display().to_string()}),
    );
    let thread_log = log.clone();
    if let Err(e) = std::thread::Builder::new()
        .name("config-watch".into())
        .spawn(move || watch_loop(&parent, &basename, &thread_log))
    {
        log.warn("config.watch.error", json!({"err": e.to_string()}));
    }
}

/// The blocking inotify watch loop (own thread). Best-effort: any fatal inotify
/// error logs `config.watch.error` and returns (ends the thread) — SIGHUP still
/// reloads. Re-arms the directory watch after an `IN_IGNORED` so a second swap
/// still fires.
#[cfg(any(target_os = "linux", target_os = "android"))]
fn watch_loop(dir: &Path, basename: &str, log: &Logger) {
    use std::os::unix::ffi::OsStrExt;

    // CLOEXEC so the fd never leaks into a re-exec'd subagent.
    let ifd = unsafe { libc::inotify_init1(libc::IN_CLOEXEC) };
    if ifd < 0 {
        log.warn(
            "config.watch.error",
            json!({"err": "inotify_init1 failed", "errno": errno()}),
        );
        return;
    }
    // RAII close of the inotify fd on any return path.
    struct Fd(libc::c_int);
    impl Drop for Fd {
        fn drop(&mut self) {
            unsafe { libc::close(self.0) };
        }
    }
    let _guard = Fd(ifd);

    let cdir = std::ffi::CString::new(dir.as_os_str().as_bytes()).ok();
    let add_watch = || -> libc::c_int {
        match &cdir {
            Some(c) => unsafe { libc::inotify_add_watch(ifd, c.as_ptr(), WATCH_MASK) },
            None => -1,
        }
    };
    if cdir.is_none() {
        log.warn(
            "config.watch.error",
            json!({"err": "watched dir path has an interior NUL"}),
        );
        return;
    }
    if add_watch() < 0 {
        log.warn(
            "config.watch.error",
            json!({"err": "inotify_add_watch failed", "dir": dir.display().to_string(), "errno": errno()}),
        );
        return;
    }

    // A generous buffer: many small records fit in one read; the kernel never
    // returns a partial record, so this only bounds events-per-read.
    let mut buf = [0u8; 4096];
    loop {
        let n = unsafe { libc::read(ifd, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
        if n < 0 {
            let e = errno();
            // EINTR: a signal interrupted the blocking read — just retry.
            if e == libc::EINTR {
                continue;
            }
            log.warn(
                "config.watch.error",
                json!({"err": "inotify read failed", "errno": e}),
            );
            return; // best-effort: end the thread, SIGHUP still works
        }
        if n == 0 {
            continue;
        }
        let mut fire = false;
        let mut rearm = false;
        for (mask, name) in parse_events(&buf[..n as usize]) {
            // The watch was dropped (the projected dir was swapped out from under
            // us, or removed) — re-arm on the stable parent path so a SECOND
            // ConfigMap update still fires (RFC 0017 §5.2 symlink-swap bit).
            if mask & libc::IN_IGNORED != 0 {
                rearm = true;
            }
            // Fire on any event naming our config file's basename.
            if name.as_deref() == Some(basename) {
                fire = true;
            }
        }
        if rearm {
            // Re-`add_watch` the same parent path (idempotent: if the old watch
            // is still valid the kernel returns the same wd). A re-arm failure is
            // logged but not fatal — the existing watch may still be live.
            if add_watch() < 0 {
                log.warn(
                    "config.watch.error",
                    json!({"err": "inotify re-arm failed", "errno": errno()}),
                );
            }
            // A swap that re-armed is itself a config change for our file — the new
            // projection IS the new file — so treat a re-arm as a fire too. (The
            // basename match above may also have set it; `fire` is idempotent.)
            fire = true;
        }
        if fire {
            // Coalesce a burst (a ConfigMap swap fires several events): if a
            // reload is already pending, don't re-request — one per burst is
            // enough, and the reload routine re-reads current state anyway.
            if crate::signals::reload_requested() {
                continue;
            }
            log.info("config.watch.fired", json!({"file": basename}));
            crate::signals::request_reload_from_watch();
        }
    }
}

/// Non-Linux Unix fallback: inotify is Linux-only. The `config-watch` feature is
/// documented as a Linux/ConfigMap surface; on other Unices we log once and the
/// SIGHUP trigger remains the reload path. (Keeps the crate compiling under
/// `--all-features` on a non-Linux Unix without a hard build break.)
#[cfg(not(any(target_os = "linux", target_os = "android")))]
fn watch_loop(_dir: &Path, _basename: &str, log: &Logger) {
    log.warn(
        "config.watch.error",
        json!({"err": "inotify file-watch is Linux-only; use SIGHUP on this platform"}),
    );
}

/// The current `errno`, for diagnostic logging. Read immediately after a failed
/// syscall.
#[cfg(any(target_os = "linux", target_os = "android"))]
fn errno() -> i32 {
    std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build one inotify record (header + NUL-padded name) into `out`, mirroring
    /// the kernel's wire layout, so the parser can be tested without a real FS.
    fn push_record(out: &mut Vec<u8>, wd: i32, mask: u32, cookie: u32, name: Option<&str>) {
        out.extend_from_slice(&wd.to_ne_bytes());
        out.extend_from_slice(&mask.to_ne_bytes());
        out.extend_from_slice(&cookie.to_ne_bytes());
        match name {
            None => out.extend_from_slice(&0u32.to_ne_bytes()), // len = 0
            Some(n) => {
                // The kernel NUL-terminates and pads `len` up to an alignment
                // boundary; emulate a NUL terminator + pad to a multiple of 4.
                let mut bytes = n.as_bytes().to_vec();
                bytes.push(0);
                while bytes.len() % 4 != 0 {
                    bytes.push(0);
                }
                out.extend_from_slice(&(bytes.len() as u32).to_ne_bytes());
                out.extend_from_slice(&bytes);
            }
        }
    }

    #[test]
    fn parses_a_single_named_record() {
        let mut buf = Vec::new();
        push_record(
            &mut buf,
            1,
            0x0000_0080, /*IN_MOVED_TO*/
            0,
            Some("config.json"),
        );
        let ev = parse_events(&buf);
        assert_eq!(ev.len(), 1);
        assert_eq!(ev[0].0, 0x0000_0080);
        assert_eq!(ev[0].1.as_deref(), Some("config.json"));
    }

    #[test]
    fn parses_multiple_records_in_one_read() {
        // A ConfigMap swap produces several records back-to-back in one read.
        let mut buf = Vec::new();
        push_record(
            &mut buf,
            1,
            0x0000_0100, /*IN_CREATE*/
            0,
            Some("..data_tmp"),
        );
        push_record(
            &mut buf,
            1,
            0x0000_0080, /*IN_MOVED_TO*/
            7,
            Some("..data"),
        );
        push_record(
            &mut buf,
            1,
            0x0000_0008, /*IN_CLOSE_WRITE*/
            0,
            Some("config.json"),
        );
        let ev = parse_events(&buf);
        assert_eq!(ev.len(), 3);
        assert_eq!(ev[0].1.as_deref(), Some("..data_tmp"));
        assert_eq!(ev[1].1.as_deref(), Some("..data"));
        assert_eq!(ev[2].1.as_deref(), Some("config.json"));
        assert_eq!(ev[2].0, 0x0000_0008);
    }

    #[test]
    fn parses_a_nameless_record() {
        // IN_IGNORED (0x8000) on the watch itself carries len = 0, no name.
        let mut buf = Vec::new();
        push_record(&mut buf, 1, 0x0000_8000 /*IN_IGNORED*/, 0, None);
        let ev = parse_events(&buf);
        assert_eq!(ev.len(), 1);
        assert_eq!(ev[0].0, 0x0000_8000);
        assert!(ev[0].1.is_none());
    }

    #[test]
    fn trims_at_the_first_nul_in_a_padded_name() {
        // The name field is NUL-padded; the parser must trim at the first NUL and
        // not surface trailing pad bytes as part of the name.
        let mut buf = Vec::new();
        push_record(&mut buf, 1, 0x0000_0080, 0, Some("a")); // "a" + pad to 4
        let ev = parse_events(&buf);
        assert_eq!(ev.len(), 1);
        assert_eq!(ev[0].1.as_deref(), Some("a"));
    }

    #[test]
    fn skips_a_truncated_trailing_record() {
        // A whole record followed by a partial header is parsed as just the one
        // complete record — never a panic / OOB slice.
        let mut buf = Vec::new();
        push_record(&mut buf, 1, 0x0000_0080, 0, Some("config.json"));
        buf.extend_from_slice(&[0u8, 1, 2]); // 3 stray bytes < a 16-byte header
        let ev = parse_events(&buf);
        assert_eq!(ev.len(), 1);
        assert_eq!(ev[0].1.as_deref(), Some("config.json"));
    }

    #[test]
    fn empty_buffer_yields_no_events() {
        assert!(parse_events(&[]).is_empty());
    }

    #[test]
    fn skips_a_record_whose_len_overruns_the_buffer() {
        // A header claiming a longer name than the buffer holds is a truncated
        // record — stop, never slice out of bounds.
        let mut buf = Vec::new();
        buf.extend_from_slice(&1i32.to_ne_bytes()); // wd
        buf.extend_from_slice(&0x80u32.to_ne_bytes()); // mask
        buf.extend_from_slice(&0u32.to_ne_bytes()); // cookie
        buf.extend_from_slice(&64u32.to_ne_bytes()); // len = 64, but no name follows
        let ev = parse_events(&buf);
        assert!(ev.is_empty());
    }

    /// End-to-end: arm a real watcher on a temp dir, then rewrite + atomically
    /// rename the config file (the ConfigMap-swap shape) and assert the RELOAD
    /// latch flips within a bounded poll. Takes `test_guard()` because it touches
    /// the process-global `signals` state (the watcher calls
    /// `request_reload_from_watch`); the guard serializes + resets it. Linux-only
    /// (inotify). Designed to be non-flaky: it RE-triggers the write/rename across
    /// the whole poll window, so a single missed event never fails the test.
    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[test]
    fn e2e_rename_fires_a_reload() {
        use crate::obs::log::{Comp, Level, LogCtx, Logger};
        use std::io::Write as _;
        use std::time::{Duration, Instant};

        let _g = crate::signals::test_guard();
        assert!(!crate::signals::reload_requested(), "clean slate");

        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join("config.json");
        std::fs::write(&cfg, b"{}\n").unwrap();

        let log = Logger::new(
            LogCtx {
                run_id: "r".into(),
                agent_id: "0".into(),
                agent_path: "0".into(),
                comp: Comp::Supervisor,
                pid: std::process::id(),
                trace_id: None,
            },
            // Quiet: only emit errors during the test (info/fired lines are noise).
            Level::Error,
        );
        spawn_config_watcher(&cfg, &log);

        // Give the watcher thread a beat to arm its inotify watch before we mutate.
        std::thread::sleep(Duration::from_millis(50));

        // Drive the ConfigMap-swap shape: write a sibling temp file then atomically
        // rename it over the config path (rename → IN_MOVED_TO of our basename).
        // Re-do it across the whole poll window so a single missed event is not a
        // flake — the latch only needs to flip ONCE.
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut fired = false;
        let mut i = 0u32;
        while Instant::now() < deadline {
            let tmp = dir.path().join(format!(".tmp-{i}"));
            let mut f = std::fs::File::create(&tmp).unwrap();
            f.write_all(format!("{{\"max_tokens\": {}}}\n", 1000 + i).as_bytes())
                .unwrap();
            f.flush().unwrap();
            std::fs::rename(&tmp, &cfg).unwrap();
            i += 1;
            // Poll a few times before re-triggering.
            for _ in 0..10 {
                if crate::signals::reload_requested() {
                    fired = true;
                    break;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            if fired {
                break;
            }
        }
        assert!(
            fired,
            "the file-watch trigger should set the RELOAD latch within 5s of a rename"
        );
        // The trigger is attributed to the watch (RFC 0017 §5.6): the apply step
        // would read this as `trigger:"watch"`.
        assert!(
            crate::signals::take_reload_was_watch(),
            "the reload should be attributed to the watch trigger"
        );
    }
}
