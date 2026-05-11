//! Filesystem-watch trigger.
//!
//! Fires the configured start node on filesystem events under
//! a watched path. Uses the `notify` crate (inotify on Linux,
//! FSEvents on macOS, ReadDirectoryChangesW on Windows).
//!
//! ## Debouncing
//!
//! Filesystem events arrive in bursts — `mv src dst` typically
//! emits 3 notify events on Linux. A per-path timer coalesces
//! rapid events into one trigger fire after `debounce_ms` of
//! quiet. This avoids flooding the engine on `rsync` / atomic-
//! rename patterns.
//!
//! ## Event filter
//!
//! The workflow's `events` list accepts any of:
//!
//! - `"create"` — matches `notify::EventKind::Create(_)`.
//! - `"modify"` — matches `Modify(Data(..))`.
//! - `"remove"` — matches `Remove(_)`.
//! - `"rename"` — matches `Modify(Name(..))`.
//!
//! Empty list = all four. Events outside this set are silently
//! dropped (no audit noise; typical cases are metadata touches
//! and chmod).
//!
//! ## Payload
//!
//! ```json
//! {
//!   "kind": "fs_watch",
//!   "path": "/var/in/batch-123.json",
//!   "event": "create",
//!   "fired_at_unix_ms": 1_745_000_000_000,
//!   "tick": 17
//! }
//! ```
//!
//! If debouncing coalesced multiple kinds for the same path, the
//! payload reports the **last** observed event (typical: `create`
//! followed by `modify` coalesce into `modify`).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use notify::{EventKind, RecursiveMode, Watcher};
use serde_json::json;

use crate::engine::{Engine, RunOptions, TriggerMeta};
use crate::error::{Error, Result};
use crate::workflow::WorkflowDoc;
use crate::workflow::model::Trigger;

/// Pre-validated filesystem-watch trigger — ready to spawn.
#[derive(Debug)]
pub struct FsWatchTrigger {
    path: PathBuf,
    start_node: String,
    recursive: bool,
    kinds: KindFilter,
    debounce: Duration,
}

impl FsWatchTrigger {
    pub fn from_trigger(trig: &Trigger) -> Result<Option<Self>> {
        match trig {
            Trigger::FsWatch {
                path,
                start_node,
                recursive,
                events,
                debounce_ms,
            } => {
                if !path.exists() {
                    return Err(Error::Config(format!(
                        "trigger.fs_watch path `{}` does not exist",
                        path.display()
                    )));
                }
                let kinds = KindFilter::parse(events)?;
                Ok(Some(Self {
                    path: path.clone(),
                    start_node: start_node.clone(),
                    recursive: *recursive,
                    kinds,
                    debounce: Duration::from_millis(*debounce_ms),
                }))
            }
            _ => Ok(None),
        }
    }

    pub fn start_node(&self) -> &str {
        &self.start_node
    }

    /// Spawn the watch thread. The thread owns a `notify::Watcher`
    /// plus an mpsc channel; the event loop debounces events and
    /// hands coalesced triggers to the engine synchronously (serial
    /// per trigger, matching the cron convention).
    pub fn spawn(
        self,
        workflow: Arc<WorkflowDoc>,
        engine: Arc<Engine>,
        options: RunOptions,
        shutdown: Arc<AtomicBool>,
    ) -> thread::JoinHandle<()> {
        thread::spawn(move || {
            let (tx, rx) = mpsc::channel::<notify::Result<notify::Event>>();
            let mut watcher = match notify::recommended_watcher(move |res| {
                let _ = tx.send(res);
            }) {
                Ok(w) => w,
                Err(e) => {
                    tracing::error!(
                        target: "agentd::audit",
                        event = "fs_watch.watcher_init_failed",
                        reason = %format!("{e}"),
                    );
                    return;
                }
            };
            let mode = if self.recursive {
                RecursiveMode::Recursive
            } else {
                RecursiveMode::NonRecursive
            };
            if let Err(e) = watcher.watch(&self.path, mode) {
                tracing::error!(
                    target: "agentd::audit",
                    event = "fs_watch.watch_failed",
                    path = %self.path.display(),
                    reason = %format!("{e}"),
                );
                return;
            }
            tracing::info!(
                target: "agentd::audit",
                event = "fs_watch.started",
                path = %self.path.display(),
                recursive = self.recursive,
                debounce_ms = self.debounce.as_millis() as u64,
            );

            // Debounce buckets: keyed by absolute path. Each entry
            // holds the last observed kind + a fire deadline. A
            // background tick drains expired buckets into the
            // engine.
            let mut pending: HashMap<PathBuf, (&'static str, Instant)> = HashMap::new();
            let mut tick_counter: u64 = 0;

            loop {
                if shutdown.load(Ordering::Relaxed) {
                    return;
                }
                // Poll at the debounce tick so expired buckets fire
                // promptly. Raising this over 200ms delays shutdown
                // observation; lowering it spins the CPU.
                let poll = self.debounce.min(Duration::from_millis(200));
                match rx.recv_timeout(poll) {
                    Ok(Ok(ev)) => {
                        let Some(label) = self.kinds.label_for(&ev.kind) else {
                            continue;
                        };
                        let deadline = Instant::now() + self.debounce;
                        for path in ev.paths {
                            pending.insert(path, (label, deadline));
                        }
                    }
                    Ok(Err(e)) => {
                        tracing::warn!(
                            target: "agentd::audit",
                            event = "fs_watch.event_error",
                            reason = %format!("{e}"),
                        );
                    }
                    Err(mpsc::RecvTimeoutError::Timeout) => {}
                    Err(mpsc::RecvTimeoutError::Disconnected) => return,
                }

                // Drain expired debounce buckets.
                let now = Instant::now();
                let ready: Vec<(PathBuf, &'static str)> = pending
                    .iter()
                    .filter(|(_, (_, deadline))| *deadline <= now)
                    .map(|(p, (l, _))| (p.clone(), *l))
                    .collect();
                for (path, label) in ready {
                    pending.remove(&path);
                    tick_counter = tick_counter.saturating_add(1);
                    self.fire(&workflow, &engine, &options, &path, label, tick_counter);
                    if shutdown.load(Ordering::Relaxed) {
                        return;
                    }
                }
            }
        })
    }

    fn fire(
        &self,
        workflow: &WorkflowDoc,
        engine: &Engine,
        options: &RunOptions,
        path: &Path,
        event: &str,
        tick: u64,
    ) {
        let payload = json!({
            "kind": "fs_watch",
            "path": path.display().to_string(),
            "event": event,
            "fired_at_unix_ms": now_unix_ms(),
            "tick": tick,
        });
        tracing::info!(
            target: "agentd::audit",
            event = "fs_watch.fire",
            path = %path.display(),
            fs_event = event,
            tick = tick,
            start_node = %self.start_node,
        );
        let started = Instant::now();
        match engine.run(
            workflow,
            &self.start_node,
            TriggerMeta::manual(payload),
            options.clone(),
        ) {
            Ok(outcome) => {
                tracing::info!(
                    target: "agentd::audit",
                    event = "fs_watch.completed",
                    path = %path.display(),
                    tick = tick,
                    status = %outcome.status_label(),
                    elapsed_ms = started.elapsed().as_millis() as u64,
                );
            }
            Err(e) => {
                tracing::error!(
                    target: "agentd::audit",
                    event = "fs_watch.error",
                    path = %path.display(),
                    tick = tick,
                    reason = %format!("{e}"),
                );
            }
        }
    }
}

/// Mask of which notify event kinds trigger the workflow.
#[derive(Debug, Clone, Copy)]
struct KindFilter {
    create: bool,
    modify: bool,
    remove: bool,
    rename: bool,
}

impl KindFilter {
    fn parse(list: &[String]) -> Result<Self> {
        if list.is_empty() {
            return Ok(Self::all());
        }
        let mut filter = Self::none();
        for raw in list {
            match raw.to_ascii_lowercase().as_str() {
                "create" => filter.create = true,
                "modify" => filter.modify = true,
                "remove" | "delete" => filter.remove = true,
                "rename" => filter.rename = true,
                other => {
                    return Err(Error::Config(format!(
                        "trigger.fs_watch events: unknown `{other}` \
                         (expected create / modify / remove / rename)"
                    )));
                }
            }
        }
        Ok(filter)
    }

    fn all() -> Self {
        Self {
            create: true,
            modify: true,
            remove: true,
            rename: true,
        }
    }
    fn none() -> Self {
        Self {
            create: false,
            modify: false,
            remove: false,
            rename: false,
        }
    }

    /// Convert a notify::EventKind to our 4-way label if the caller
    /// is subscribed. Returns `None` for unsubscribed or ignored
    /// kinds (metadata, access, etc.).
    fn label_for(&self, kind: &EventKind) -> Option<&'static str> {
        match kind {
            EventKind::Create(_) if self.create => Some("create"),
            EventKind::Modify(notify::event::ModifyKind::Data(_)) if self.modify => Some("modify"),
            EventKind::Modify(notify::event::ModifyKind::Name(_)) if self.rename => Some("rename"),
            EventKind::Remove(_) if self.remove => Some("remove"),
            _ => None,
        }
    }
}

fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn kind_filter_empty_means_all() {
        let f = KindFilter::parse(&[]).unwrap();
        assert!(f.create && f.modify && f.remove && f.rename);
    }

    #[test]
    fn kind_filter_normalizes_case() {
        let f = KindFilter::parse(&["CREATE".into(), "Modify".into()]).unwrap();
        assert!(f.create && f.modify);
        assert!(!f.remove && !f.rename);
    }

    #[test]
    fn kind_filter_rejects_unknown() {
        let err = KindFilter::parse(&["touch".into()]).unwrap_err();
        assert!(format!("{err}").contains("touch"));
    }

    #[test]
    fn from_trigger_requires_existing_path() {
        let trig = Trigger::FsWatch {
            path: PathBuf::from("/definitely/not/real"),
            start_node: "n".into(),
            recursive: false,
            events: vec![],
            debounce_ms: 100,
        };
        let err = FsWatchTrigger::from_trigger(&trig).unwrap_err();
        assert!(format!("{err}").contains("does not exist"));
    }

    #[test]
    fn from_trigger_accepts_real_path() {
        let dir = tempfile::TempDir::new().unwrap();
        let trig = Trigger::FsWatch {
            path: dir.path().to_path_buf(),
            start_node: "on_file".into(),
            recursive: true,
            events: vec!["create".into(), "modify".into()],
            debounce_ms: 100,
        };
        let prepped = FsWatchTrigger::from_trigger(&trig).unwrap().unwrap();
        assert_eq!(prepped.start_node(), "on_file");
        assert!(prepped.recursive);
    }

    #[test]
    fn from_trigger_returns_none_for_unrelated_variant() {
        let trig = Trigger::InternalEvent {
            name: "boot".into(),
            start_node: "n".into(),
        };
        assert!(FsWatchTrigger::from_trigger(&trig).unwrap().is_none());
    }
}
