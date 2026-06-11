//! Checkpoints — the durable state of a paused run.
//!
//! When a run reaches a `pause_for_approval` node the engine writes a
//! checkpoint: enough to rebuild the [`ExecutionContext`] and continue
//! at the node after the pause. `--resume RUN_ID` reads it back. The
//! ephemeral parts of a run (the absolute deadline, the inbound trace
//! context) are *not* persisted — a resumed run gets a fresh deadline.
//!
//! Checkpoints are plain JSON under a state directory, one file per
//! run id. They contain node outputs verbatim, so a state directory
//! deserves the same care as the data the workflow processes.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::engine::context::TriggerKind;

/// The persisted state of a suspended run — either a `pause_for_approval`
/// pause (awaiting a human) or a per-node progress snapshot for
/// crash-recovery. Resume treats both identically; only auto-resume
/// distinguishes them via [`Checkpoint::awaiting_approval`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Checkpoint {
    /// The run's execution id — also the checkpoint file's stem.
    pub run_id: String,
    /// The workflow the checkpoint belongs to (resume verifies a match).
    pub workflow: String,
    /// The original start node.
    pub start_node: String,
    pub trigger_kind: TriggerKind,
    pub trigger_input: Value,
    /// Node outputs accumulated up to this point (includes the reserved
    /// `trigger` entry).
    pub node_outputs: HashMap<String, Value>,
    /// The node the run last reached (a pause node, or the last node
    /// that completed before a progress snapshot).
    pub paused_at: String,
    /// The node to execute on resume. `None` means there is no successor
    /// — resuming completes.
    pub resume_node: Option<String>,
    /// The operator-facing approval prompt (pause checkpoints only).
    pub reason: Option<String>,
    /// `true` for a `pause_for_approval` checkpoint (a human must resume
    /// it); `false` for an automatic per-node progress snapshot (safe to
    /// auto-resume after a crash). Defaults to `true` so checkpoints
    /// written before this field existed are treated as deliberate.
    #[serde(default = "default_true")]
    pub awaiting_approval: bool,
}

fn default_true() -> bool {
    true
}

impl Checkpoint {
    /// The checkpoint file path for a run under `dir`.
    pub fn path(dir: &Path, run_id: &str) -> PathBuf {
        dir.join(format!("{run_id}.json"))
    }

    /// Write the checkpoint under `dir` (creating it if needed).
    pub fn save(&self, dir: &Path) -> Result<PathBuf, String> {
        std::fs::create_dir_all(dir)
            .map_err(|e| format!("create state dir {}: {e}", dir.display()))?;
        let path = Self::path(dir, &self.run_id);
        let json = serde_json::to_string_pretty(self).map_err(|e| format!("serialize: {e}"))?;
        std::fs::write(&path, json).map_err(|e| format!("write {}: {e}", path.display()))?;
        Ok(path)
    }

    /// Read a checkpoint for `run_id` from `dir`.
    pub fn load(dir: &Path, run_id: &str) -> Result<Self, String> {
        let path = Self::path(dir, run_id);
        let raw = std::fs::read_to_string(&path)
            .map_err(|e| format!("read checkpoint {}: {e}", path.display()))?;
        serde_json::from_str(&raw).map_err(|e| format!("parse checkpoint {}: {e}", path.display()))
    }

    /// Remove the checkpoint file once a resumed run finishes (so a run
    /// id can't be resumed twice). Best-effort.
    pub fn discard(dir: &Path, run_id: &str) {
        let _ = std::fs::remove_file(Self::path(dir, run_id));
    }

    /// Every parseable checkpoint under `dir`, sorted by run id.
    /// Unreadable / non-checkpoint `.json` files are skipped.
    pub fn list(dir: &Path) -> Vec<Checkpoint> {
        let mut out = Vec::new();
        let Ok(entries) = std::fs::read_dir(dir) else {
            return out;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            if let Ok(raw) = std::fs::read_to_string(&path) {
                if let Ok(cp) = serde_json::from_str::<Checkpoint>(&raw) {
                    out.push(cp);
                }
            }
        }
        out.sort_by(|a, b| a.run_id.cmp(&b.run_id));
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn round_trips_through_disk() {
        let dir = tempfile::TempDir::new().unwrap();
        let mut outputs = HashMap::new();
        outputs.insert("a".to_string(), json!({"x": 1}));
        let cp = Checkpoint {
            run_id: "exec-7".into(),
            workflow: "wf".into(),
            start_node: "main".into(),
            trigger_kind: TriggerKind::Manual,
            trigger_input: json!({"k": "v"}),
            node_outputs: outputs,
            paused_at: "gate".into(),
            resume_node: Some("after".into()),
            reason: Some("approve?".into()),
            awaiting_approval: true,
        };
        cp.save(dir.path()).unwrap();
        let back = Checkpoint::load(dir.path(), "exec-7").unwrap();
        assert_eq!(back.run_id, "exec-7");
        assert_eq!(back.resume_node.as_deref(), Some("after"));
        assert_eq!(back.node_outputs["a"], json!({"x": 1}));

        Checkpoint::discard(dir.path(), "exec-7");
        assert!(Checkpoint::load(dir.path(), "exec-7").is_err());
    }
}
