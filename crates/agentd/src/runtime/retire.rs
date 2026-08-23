// SPDX-License-Identifier: AGPL-3.0-only
//! **Graceful workflow retirement** — the one exit path for every way a
//! definition leaves the daemon: removed on a config reload, replaced by a
//! new version, or `workflow.delete`d.
//!
//! Before this, the three exits behaved three different ways: reload pinned
//! live runs but leaked the definition forever and never unsubscribed its
//! MCP resources; delete unsubscribed nothing AND pinned nothing, so a live
//! run of a deleted workflow lost its definition mid-flight and was refused.
//! Now retirement always: disarms starts and unsubscribes resources no other
//! armed workflow still wants; pins the definition for live runs; stops
//! admitting new ones; then applies the workflow's own `unload:` policy —
//! `drain` (default; bounded by `timeout`, then cancel), `cancel`, or
//! `detach`. The pin is garbage-collected when its last run reaches a
//! terminal status, which also closes the old reload leak.

use crate::engine::model::{UnloadPolicy, Workflow};
use crate::state::{Kind, now_ms};
use serde_json::json;

/// Durable definition pins, keyed by content hash (`Kind::Memory`). Written
/// once per definition version when its first run starts; read back at
/// restore for any non-terminal run whose definition is no longer in the
/// registry — which is what lets a run **survive a restart that also changed
/// or removed its workflow**, instead of being refused. Deleted when the last
/// run of that hash reaches a terminal status.
pub(crate) const PIN_PREFIX: &str = "_pins/";

/// A retired definition still owning live runs. The policy was applied at
/// retirement; what remains to track is only the drain bound.
#[derive(Debug, Clone)]
pub(crate) struct Retiring {
    pub hash: String,
    /// `drain` past this instant cancels what remains. `None` = unbounded.
    pub deadline_ms: Option<u64>,
}

impl super::reactor::Runtime {
    /// Persist the definition a starting run will need if this process — and
    /// possibly this configuration — is gone before the run is. One write per
    /// definition version per process life.
    pub(crate) fn ensure_pin(&mut self, wf: &Workflow) {
        if !self.pin_written.insert(wf.hash.clone()) {
            return;
        }
        if let Err(e) = self.durable.put(
            Kind::Memory,
            &format!("{PIN_PREFIX}{}", wf.hash),
            json!({"name": wf.name, "definition": wf.definition}),
            None,
        ) {
            // Non-fatal: the run still executes; only the cross-restart
            // guarantee narrows to "definition unchanged" for this version.
            self.log.warn(
                "workflow.pin_fail",
                json!({"workflow": wf.name, "hash": &wf.hash[..12.min(wf.hash.len())], "err": e.to_string()}),
            );
            self.pin_written.remove(&wf.hash);
        }
    }

    /// Restore-side: for every non-terminal run whose definition is neither
    /// current nor already pinned in memory, load the durable pin — the other
    /// half of [`Self::ensure_pin`].
    pub(crate) fn restore_pins(&mut self) {
        let missing: Vec<(String, String)> = self
            .runs
            .values()
            .filter(|r| !r.status.is_terminal())
            .filter(|r| {
                let current = self
                    .workflows
                    .get(&r.workflow)
                    .is_some_and(|w| w.hash == r.workflow_hash);
                !current && !self.pinned.contains_key(&r.workflow_hash)
            })
            .map(|r| (r.workflow.clone(), r.workflow_hash.clone()))
            .collect();
        for (name, hash) in missing {
            let doc = self
                .durable
                .get(Kind::Memory, &format!("{PIN_PREFIX}{hash}"))
                .ok()
                .flatten()
                .and_then(|env| env.state.get("definition").cloned());
            match doc.map(|d| crate::engine::model::parse_workflow(&d)) {
                Some(Ok(wf)) if wf.hash == hash => {
                    self.log.info(
                        "workflow.pin_restored",
                        json!({"workflow": name, "hash": &hash[..12.min(hash.len())]}),
                    );
                    self.pin_written.insert(hash.clone());
                    self.pinned.insert(hash, std::sync::Arc::new(wf));
                }
                other => {
                    // No pin (a pre-pin store) or a corrupt one: the run meets
                    // the resume policy exactly as before this feature.
                    self.log.warn(
                        "workflow.pin_missing",
                        json!({"workflow": name, "hash": &hash[..12.min(hash.len())],
                               "err": match other { Some(Err(e)) => e.join("; "), _ => "no durable pin".into() }}),
                    );
                }
            }
        }
    }

    /// Retire `wf` (already removed — or about to be — from the live
    /// registry). `reason` names the exit for the log: "reload" / "replaced" /
    /// "deleted".
    pub(crate) fn retire_workflow(&mut self, wf: &Workflow, reason: &str) {
        // 1. Unsubscribe the MCP resources this definition armed — unless
        //    another still-armed workflow subscribes the same (server, uri),
        //    in which case the subscription is theirs now.
        for s in wf.start_steps() {
            if s.kind != "subscribe" {
                continue;
            }
            let (Some(server), Some(uri)) = (s.field_str("server"), s.field_str("uri")) else {
                continue;
            };
            let still_wanted = self
                .workflows
                .values()
                .filter(|w| w.name != wf.name && w.armed)
                .flat_map(|w| w.start_steps())
                .any(|o| {
                    o.kind == "subscribe"
                        && o.field_str("server") == Some(server)
                        && o.field_str("uri") == Some(uri)
                });
            if !still_wanted && let Some(c) = self.mcp.get(server) {
                match c.unsubscribe(uri) {
                    Ok(()) => self.log.info(
                        "workflow.unsubscribed",
                        json!({"workflow": wf.name, "server": server, "uri": uri}),
                    ),
                    Err(e) => self.log.warn(
                        "workflow.unsubscribe_fail",
                        json!({"workflow": wf.name, "server": server, "uri": uri, "err": e.to_string()}),
                    ),
                }
            }
        }

        // 2. Live runs of THIS definition version.
        let live: Vec<String> = self
            .runs
            .values()
            .filter(|r| {
                !r.status.is_terminal() && r.workflow == wf.name && r.workflow_hash == wf.hash
            })
            .map(|r| r.id.clone())
            .collect();
        if live.is_empty() {
            self.log.info(
                "workflow.unloaded",
                json!({"workflow": wf.name, "hash": &wf.hash[..12.min(wf.hash.len())], "reason": reason, "live_runs": 0}),
            );
            return;
        }

        // 3. Pin, so the runs keep a definition to execute against.
        self.pinned
            .insert(wf.hash.clone(), std::sync::Arc::new(wf.clone()));
        let policy = wf.unload.policy;
        self.log.info(
            "workflow.retiring",
            json!({"workflow": wf.name, "hash": &wf.hash[..12.min(wf.hash.len())],
                   "reason": reason, "policy": policy.as_str(), "live_runs": live.len(),
                   "timeout_ms": wf.unload.timeout_ms}),
        );

        // 4. The policy.
        match policy {
            UnloadPolicy::Cancel => {
                for id in &live {
                    self.cancel_run(id, "workflow retired (unload: cancel)");
                }
                self.retiring.insert(
                    wf.hash.clone(),
                    Retiring {
                        hash: wf.hash.clone(),
                        deadline_ms: None,
                    },
                );
            }
            UnloadPolicy::Drain | UnloadPolicy::Detach => {
                let deadline_ms = match policy {
                    UnloadPolicy::Drain => wf.unload.timeout_ms.map(|t| now_ms() + t),
                    _ => None,
                };
                self.retiring.insert(
                    wf.hash.clone(),
                    Retiring {
                        hash: wf.hash.clone(),
                        deadline_ms,
                    },
                );
            }
        }
    }

    /// Tick half: a `drain` whose deadline passed cancels what remains. Cheap —
    /// the retiring map is empty except in the minutes around a retirement.
    pub(crate) fn retire_tick(&mut self) {
        if self.retiring.is_empty() {
            return;
        }
        let now = now_ms();
        let overdue: Vec<String> = self
            .retiring
            .values()
            .filter(|r| r.deadline_ms.is_some_and(|d| now >= d))
            .map(|r| r.hash.clone())
            .collect();
        for hash in overdue {
            let victims: Vec<String> = self
                .runs
                .values()
                .filter(|r| !r.status.is_terminal() && r.workflow_hash == hash)
                .map(|r| r.id.clone())
                .collect();
            for id in &victims {
                self.cancel_run(id, "workflow retired (unload drain timeout)");
            }
            if let Some(r) = self.retiring.get_mut(&hash) {
                r.deadline_ms = None; // fired once; the sweep finishes the story
            }
        }
    }

    /// Run-terminal half: when the last live run of a pinned hash finishes,
    /// the pin and the retiring record are released — the definition is gone
    /// for real, and the reload-era leak with it.
    pub(crate) fn retire_sweep(&mut self) {
        if self.pinned.is_empty() && self.retiring.is_empty() {
            return;
        }
        let referenced: std::collections::BTreeSet<String> = self
            .runs
            .values()
            .filter(|r| !r.status.is_terminal())
            .map(|r| r.workflow_hash.clone())
            .collect();
        let dropped: Vec<(String, String)> = self
            .pinned
            .iter()
            .filter(|(hash, _)| !referenced.contains(*hash))
            .map(|(hash, wf)| (hash.clone(), wf.name.clone()))
            .collect();
        for (hash, name) in &dropped {
            self.log.info(
                "workflow.unloaded",
                json!({"workflow": name, "hash": &hash[..12.min(hash.len())], "live_runs": 0}),
            );
            let _ = self
                .durable
                .delete(Kind::Memory, &format!("{PIN_PREFIX}{hash}"));
            self.pinned.remove(hash);
            self.pin_written.remove(hash);
        }
        self.retiring.retain(|hash, _| referenced.contains(hash));
    }
}
