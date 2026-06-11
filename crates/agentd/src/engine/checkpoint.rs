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

/// The persisted state of a paused run.
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
    /// Node outputs accumulated up to the pause (includes the reserved
    /// `trigger` entry).
    pub node_outputs: HashMap<String, Value>,
    /// The `pause_for_approval` node that suspended the run.
    pub paused_at: String,
    /// The node to execute on resume (the pause node's successor).
    /// `None` means the pause node had no out-edge — resuming completes.
    pub resume_node: Option<String>,
    /// The operator-facing approval prompt.
    pub reason: Option<String>,
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
