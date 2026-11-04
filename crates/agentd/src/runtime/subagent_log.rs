// SPDX-License-Identifier: AGPL-3.0-only
//! `subagent.log {handle, lines?, after?}`: the last lines of one child's
//! log, and a cursor to tail it with. Read-only and the operator's alone.
//!
//! Where the lines come from is the record's business, never the caller's:
//!
//! * an **instance child** is a daemon of its own and writes its own log, in
//!   the directory its parent composed it in (`<dir>/log`, the path
//!   `instance.spawn` names). The path is derived from the parent's record;
//!   at most [`FILE_TAIL_CAP`] bytes from its end are read, and the cursor is
//!   the byte offset just past the last whole line returned.
//! * a **flat worker** is a process of this one's tree whose own lines go to
//!   this instance's stderr. What this instance holds in memory is its log
//!   ring (`a2a.introspection.enabled`): the lines it logged about the worker
//!   — those naming its `handle`, or carrying its `agent_path` — and the
//!   cursor is the ring's sequence number. Without a ring there is nothing in
//!   memory to read, and the answer says so.
//!
//! The lines are agentd's own JSON lines, as written; agentd keeps secrets
//! out of them, and nothing is added here.

use super::reactor::Runtime;
use super::tools::{ToolCaller, ToolOutcome};
use serde_json::{Value, json};
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

/// The lines a read returns when it names no `lines`.
pub(crate) const DEFAULT_LINES: usize = 200;
/// The most lines one read returns.
pub(crate) const MAX_LINES: usize = 1000;
/// The most bytes read from the end of a child's log file.
pub(crate) const FILE_TAIL_CAP: u64 = 256 * 1024;
/// The longest one returned line is, in bytes; a longer one is cut, marked.
pub(crate) const LINE_CAP: usize = 8 * 1024;

/// What one read of a log file found.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct FileTail {
    pub lines: Vec<String>,
    /// The byte offset just past the last whole line read: the next read's
    /// `after`. A line still being written is left for that read.
    pub cursor: u64,
    /// Lines between `after` (or the start) and the first one returned were
    /// left out — the byte cap, or the line count.
    pub skipped: bool,
}

/// The newest `max` whole lines of the file at `path` after the byte offset
/// `after`, reading at most `cap` bytes from its end. An `after` past the
/// end (the file was replaced) reads as no cursor at all.
pub(crate) fn tail_file(
    path: &Path,
    after: Option<u64>,
    max: usize,
    cap: u64,
) -> std::io::Result<FileTail> {
    let mut f = std::fs::File::open(path)?;
    let len = f.metadata()?.len();
    let from = after.filter(|a| *a <= len).unwrap_or(0);
    let start = from.max(len.saturating_sub(cap));
    f.seek(SeekFrom::Start(start))?;
    let mut buf = Vec::new();
    f.take(len - start).read_to_end(&mut buf)?;
    let mut skipped = start > from;
    // A window that starts inside a line begins after it.
    let mut first = 0;
    if start > from {
        match buf.iter().position(|b| *b == b'\n') {
            Some(i) => first = i + 1,
            None => first = buf.len(),
        }
    }
    let (body, cursor) = match buf[first..].iter().rposition(|b| *b == b'\n') {
        Some(i) => (&buf[first..first + i + 1], start + (first + i + 1) as u64),
        // No whole line: one longer than the whole window is passed over
        // rather than waited on for ever; anything shorter is still coming.
        None if buf.len() as u64 >= cap => {
            skipped = true;
            (&buf[0..0], len)
        }
        None => (&buf[0..0], start + first as u64),
    };
    let text = String::from_utf8_lossy(body);
    let mut lines: Vec<String> = text
        .split('\n')
        .filter(|l| !l.trim().is_empty())
        .map(cap_line)
        .collect();
    if lines.len() > max {
        lines.drain(..lines.len() - max);
        skipped = true;
    }
    Ok(FileTail {
        lines,
        cursor,
        skipped,
    })
}

/// `line`, cut to [`LINE_CAP`] bytes at a character boundary and marked.
fn cap_line(line: &str) -> String {
    if line.len() <= LINE_CAP {
        return line.to_string();
    }
    let mut cut = LINE_CAP;
    while !line.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}…(+{} bytes)", &line[..cut], line.len() - cut)
}

/// Is the ring line `v` one about the flat worker `handle`: naming it, or
/// written in its `agent_path` (`sub/<handle>`, or a path under it)?
pub(crate) fn is_childs_line(v: &Value, handle: &str) -> bool {
    if v["handle"].as_str() == Some(handle) {
        return true;
    }
    let own = format!("sub/{handle}");
    v["agent_path"]
        .as_str()
        .is_some_and(|p| p == own || p.strip_prefix(&own).is_some_and(|r| r.starts_with('/')))
}

/// The newest `max` ring lines about the flat worker `handle` after the ring
/// sequence `after`, as text, and the cursor — `None` without a ring.
pub(crate) fn ring_tail(handle: &str, after: u64, max: usize) -> Option<(Vec<String>, u64)> {
    let (lines, newest) =
        crate::obs::log::read_ring_tail(after, max, |v| is_childs_line(v, handle))?;
    let lines = lines
        .into_iter()
        .map(|mut v| {
            // The cursor is the answer's; a line is as it was written.
            if let Some(o) = v.as_object_mut() {
                o.remove("seq");
            }
            cap_line(&serde_json::to_string(&v).unwrap_or_default())
        })
        .collect();
    Some((lines, newest))
}

/// The `lines` argument, defaulted and bounded.
pub(crate) fn lines_arg(args: &Value) -> usize {
    args["lines"]
        .as_u64()
        .map(|n| (n as usize).clamp(1, MAX_LINES))
        .unwrap_or(DEFAULT_LINES)
}

impl Runtime {
    /// The `subagent.log` tool, and the op that runs it verbatim. The
    /// caller's ownership of the handle is already settled
    /// ([`Runtime::subagent_tool`]); the operator's alone on top of that, as
    /// the op is: a child's log is the instance's, not its owner's.
    pub(crate) fn subagent_log_tool(&self, caller: &ToolCaller, args: &Value) -> ToolOutcome {
        let err = |e: String| ToolOutcome::Ready(Value::String(e), true);
        if let Some(p) = self.acting_principal(caller.principal.as_deref())
            && !p.is_operator()
        {
            return err(format!(
                "subagent.log: the operator's alone, and this work acts for {}",
                p.id
            ));
        }
        let handle = args["handle"].as_str().unwrap_or("");
        let Some(s) = self.subagents.get(handle) else {
            return err(format!("no such subagent {handle:?}"));
        };
        let max = lines_arg(args);
        let after = args["after"].as_u64();
        if s.tier.as_deref() == Some("instance") {
            let Some(dir) = s.config_path.as_deref().and_then(|c| Path::new(c).parent()) else {
                return ToolOutcome::Ready(
                    json!({"handle": handle, "source": "file", "lines": [], "cursor": after.unwrap_or(0),
                           "note": "this instance child has no directory on record, so no log"}),
                    false,
                );
            };
            return match tail_file(&dir.join("log"), after, max, FILE_TAIL_CAP) {
                Ok(t) => ToolOutcome::Ready(
                    json!({"handle": handle, "source": "file", "lines": t.lines,
                           "cursor": t.cursor, "skipped": t.skipped}),
                    false,
                ),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => ToolOutcome::Ready(
                    json!({"handle": handle, "source": "file", "lines": [], "cursor": 0,
                           "note": "the child's log is gone: its directory went when it exited"}),
                    false,
                ),
                Err(e) => err(format!("subagent.log: the child's log: {e}")),
            };
        }
        match ring_tail(handle, after.unwrap_or(0), max) {
            Some((lines, cursor)) => ToolOutcome::Ready(
                json!({"handle": handle, "source": "ring", "lines": lines, "cursor": cursor}),
                false,
            ),
            None => ToolOutcome::Ready(
                json!({"handle": handle, "source": "ring", "lines": [], "cursor": after.unwrap_or(0),
                       "note": "a flat worker logs into this instance's log; no log ring is kept in memory to read it from (a2a.introspection.enabled keeps one)"}),
                false,
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn file(lines: &[&str]) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("log");
        let mut f = std::fs::File::create(&p).unwrap();
        for l in lines {
            writeln!(f, "{l}").unwrap();
        }
        (dir, p)
    }

    /// The tail is the newest lines, the cursor the end of the last whole
    /// one, and a read from the cursor sees only what was written since.
    #[test]
    fn an_instance_log_is_tailed_and_followed_by_its_cursor() {
        let (_d, p) = file(&[r#"{"event":"a"}"#, r#"{"event":"b"}"#, r#"{"event":"c"}"#]);
        let t = tail_file(&p, None, 2, FILE_TAIL_CAP).unwrap();
        assert_eq!(t.lines, vec![r#"{"event":"b"}"#, r#"{"event":"c"}"#]);
        assert!(t.skipped, "a line was left out");
        assert_eq!(t.cursor, std::fs::metadata(&p).unwrap().len());

        let again = tail_file(&p, Some(t.cursor), 200, FILE_TAIL_CAP).unwrap();
        assert!(again.lines.is_empty());
        assert_eq!(again.cursor, t.cursor);

        // A line half written is left for the next read.
        let mut f = std::fs::OpenOptions::new().append(true).open(&p).unwrap();
        f.write_all(b"{\"event\":\"d\"}\n{\"event\":").unwrap();
        let next = tail_file(&p, Some(t.cursor), 200, FILE_TAIL_CAP).unwrap();
        assert_eq!(next.lines, vec![r#"{"event":"d"}"#]);
        assert!(!next.skipped);
        writeln!(f, r#""e"}}"#).unwrap();
        let last = tail_file(&p, Some(next.cursor), 200, FILE_TAIL_CAP).unwrap();
        assert_eq!(last.lines, vec![r#"{"event":"e"}"#]);
    }

    /// No more than the cap is read from the end, and the window's cut first
    /// line is not returned as if it were whole.
    #[test]
    fn an_instance_log_is_read_no_further_back_than_the_byte_cap() {
        let long: Vec<String> = (0..100).map(|i| format!(r#"{{"n":{i:04}}}"#)).collect();
        let refs: Vec<&str> = long.iter().map(String::as_str).collect();
        let (_d, p) = file(&refs);
        // Each line is 11 bytes with its newline; 50 bytes hold 4 whole ones.
        let t = tail_file(&p, None, 1000, 50).unwrap();
        assert_eq!(t.lines, long[96..].to_vec(), "{t:?}");
        assert!(t.skipped);
        // A cursor far behind is held to the cap too.
        let t = tail_file(&p, Some(0), 1000, 50).unwrap();
        assert_eq!(t.lines.len(), 4);
        assert!(t.skipped);
        // A cursor past the end (the log was replaced) reads the tail.
        let t = tail_file(&p, Some(1 << 40), 1000, 50).unwrap();
        assert_eq!(t.lines.len(), 4);
        // One line longer than the whole window is passed over, not waited on.
        let (_d2, big) = file(&[&"x".repeat(200)]);
        let t = tail_file(&big, None, 10, 50).unwrap();
        assert!(t.lines.is_empty());
        assert_eq!(t.cursor, 201);
    }

    #[test]
    fn a_long_line_is_cut_and_marked() {
        let line = format!("{{\"m\":\"{}\"}}", "é".repeat(LINE_CAP));
        let cut = cap_line(&line);
        assert!(cut.len() < line.len());
        assert!(cut.contains("…(+"), "{}", &cut[cut.len() - 20..]);
    }

    #[test]
    fn the_line_count_is_bounded() {
        assert_eq!(lines_arg(&json!({})), DEFAULT_LINES);
        assert_eq!(lines_arg(&json!({"lines": 0})), 1);
        assert_eq!(lines_arg(&json!({"lines": 5})), 5);
        assert_eq!(lines_arg(&json!({"lines": 50_000})), MAX_LINES);
    }

    /// A flat worker's lines are the ring's lines about it — those naming its
    /// handle or written in its path — and no other child's.
    #[test]
    fn a_flat_workers_lines_come_from_the_ring() {
        use crate::obs::log::{Comp, Level, LogCtx, Logger};
        // The ring is process-global; the capacity its own test installs.
        crate::obs::log::install_event_ring(64);
        let ctx = |path: &str| LogCtx {
            run_id: "r".into(),
            agent_id: "a".into(),
            agent_path: path.into(),
            comp: Comp::Supervisor,
            pid: 1,
            trace_id: None,
            span_id: None,
            parent_span_id: None,
            trace_flags: None,
        };
        let parent = Logger::new(ctx("0"), Level::Trace);
        let (_, base) = ring_tail("sub-ring-x", 0, 1).unwrap();
        parent.info("subagent.spawn", json!({"handle": "sub-ring-x"}));
        parent.info("subagent.spawn", json!({"handle": "sub-ring-y"}));
        Logger::new(ctx("sub/sub-ring-x"), Level::Trace).info("loop.step", json!({"step": 1}));
        Logger::new(ctx("sub/sub-ring-xy"), Level::Trace).info("loop.step", json!({"step": 2}));
        parent.info(
            "subagent.turn",
            json!({"handle": "sub-ring-x", "status": "completed"}),
        );

        let (lines, cursor) = ring_tail("sub-ring-x", base, 200).unwrap();
        let events: Vec<Value> = lines
            .iter()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(
            events
                .iter()
                .map(|e| e["event"].clone())
                .collect::<Vec<_>>(),
            vec![
                json!("subagent.spawn"),
                json!("loop.step"),
                json!("subagent.turn")
            ],
            "{lines:?}"
        );
        assert!(events.iter().all(|e| e.get("seq").is_none()));
        assert!(cursor > base);
        // The newest only, and nothing again from the cursor.
        let (last, _) = ring_tail("sub-ring-x", base, 1).unwrap();
        assert!(last[0].contains("subagent.turn"), "{last:?}");
        let (none, again) = ring_tail("sub-ring-x", cursor, 200).unwrap();
        assert!(none.is_empty(), "{none:?}");
        assert!(again >= cursor);
    }

    #[test]
    fn a_line_is_a_childs_by_its_handle_or_its_path() {
        assert!(is_childs_line(&json!({"handle": "h1"}), "h1"));
        assert!(is_childs_line(&json!({"agent_path": "sub/h1"}), "h1"));
        assert!(is_childs_line(&json!({"agent_path": "sub/h1/x"}), "h1"));
        assert!(!is_childs_line(&json!({"agent_path": "sub/h10"}), "h1"));
        assert!(!is_childs_line(
            &json!({"handle": "h2", "agent_path": "0"}),
            "h1"
        ));
    }
}
