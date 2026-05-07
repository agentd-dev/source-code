//! Filesystem tool family (RFC §10.2).
//!
//! Six handlers covering read / write / mkdir / list / stat / delete.
//! Each consults the shared [`Policy`] before touching the disk.
//! Dry-run mode skips the side effect and emits a `"dry_run": true`
//! field in the structured output.
//!
//! Error surfacing: every failure is an `Error::Tool { tool, reason }`
//! so the engine's error propagation carries the handler name.

use std::fs;
use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use crate::engine::{ExecutionContext, HandlerRegistry, NodeHandler, NodeOutcome};
use crate::error::{Error, Result};
use crate::tools::policy::{Decision, PolicyRef};
use crate::tools::{resolve_string, resolve_value, value_type_name};
use crate::workflow::{Node, NodeKind};

// ---------------------------------------------------------------------------
// Registration
// ---------------------------------------------------------------------------

pub(crate) fn register(
    registry: &mut HandlerRegistry,
    policy: PolicyRef,
    budget: crate::budget::BudgetRef,
) {
    registry.register(
        "read_file",
        Box::new(ReadFileHandler {
            policy: policy.clone(),
        }),
    );
    registry.register(
        "write_file",
        Box::new(WriteFileHandler {
            policy: policy.clone(),
            budget: budget.clone(),
        }),
    );
    registry.register("create_dir", Box::new(CreateDirHandler { policy }));
    let _ = budget;
    // list_dir / stat / delete_path land once their NodeKind variants
    // are declared (out of Phase 3 scope per RFC §10.2 — same family,
    // follow-up ticket).
}

// ---------------------------------------------------------------------------
// read_file
// ---------------------------------------------------------------------------

pub struct ReadFileHandler {
    policy: PolicyRef,
}

impl NodeHandler for ReadFileHandler {
    fn handle(&self, node: &Node, ctx: &mut ExecutionContext) -> Result<NodeOutcome> {
        let NodeKind::ReadFile { path_from } = &node.kind else {
            return Err(kind_mismatch(node, "read_file"));
        };
        let path = resolve_path("read_file", ctx, path_from)?;
        check(&self.policy.check_fs_read(&path), "read_file", &path)?;

        if ctx.dry_run {
            return Ok(NodeOutcome::Continue {
                value: json!({ "path": path.display().to_string(), "dry_run": true }),
                branch: None,
            });
        }

        let content = fs::read_to_string(&path).map_err(|e| Error::Tool {
            tool: "read_file".into(),
            reason: format!("read {}: {e}", path.display()),
        })?;
        Ok(NodeOutcome::Continue {
            value: json!({
                "path": path.display().to_string(),
                "content": content,
                "bytes": path_size(&path).unwrap_or(0),
            }),
            branch: None,
        })
    }
}

// ---------------------------------------------------------------------------
// write_file
// ---------------------------------------------------------------------------

pub struct WriteFileHandler {
    policy: PolicyRef,
    budget: crate::budget::BudgetRef,
}

impl NodeHandler for WriteFileHandler {
    fn handle(&self, node: &Node, ctx: &mut ExecutionContext) -> Result<NodeOutcome> {
        let NodeKind::WriteFile {
            path_from,
            content_from,
        } = &node.kind
        else {
            return Err(kind_mismatch(node, "write_file"));
        };
        let path = resolve_path("write_file", ctx, path_from)?;
        check(&self.policy.check_fs_write(&path), "write_file", &path)?;

        // Content is a string *or* a non-string JSON value we serialise.
        let content_val = resolve_value("write_file", ctx, content_from)?;
        let content = match content_val {
            Value::String(s) => s,
            other => serde_json::to_string(&other).map_err(Error::Json)?,
        };

        if ctx.dry_run {
            return Ok(NodeOutcome::Continue {
                value: json!({
                    "path": path.display().to_string(),
                    "bytes": content.len(),
                    "dry_run": true,
                }),
                branch: None,
            });
        }

        // Budget: reserve bytes BEFORE writing so a deny short-circuits
        // without leaving partial state on disk.
        if let Err(reason) = self.budget.check_fs_write(content.len() as u64) {
            tracing::warn!(
                target: "agentd::audit",
                event = "budget.fs_write_denied",
                path = %path.display(),
                bytes = content.len() as u64,
                reason = %reason,
            );
            return Err(Error::Tool {
                tool: "write_file".into(),
                reason,
            });
        }

        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|e| Error::Tool {
                tool: "write_file".into(),
                reason: format!("mkdir_p {}: {e}", parent.display()),
            })?;
        }
        fs::write(&path, content.as_bytes()).map_err(|e| Error::Tool {
            tool: "write_file".into(),
            reason: format!("write {}: {e}", path.display()),
        })?;
        Ok(NodeOutcome::Continue {
            value: json!({
                "path": path.display().to_string(),
                "bytes": content.len(),
                "written": true,
            }),
            branch: None,
        })
    }
}

// ---------------------------------------------------------------------------
// create_dir
// ---------------------------------------------------------------------------

pub struct CreateDirHandler {
    policy: PolicyRef,
}

impl NodeHandler for CreateDirHandler {
    fn handle(&self, node: &Node, ctx: &mut ExecutionContext) -> Result<NodeOutcome> {
        let NodeKind::CreateDir { path_from } = &node.kind else {
            return Err(kind_mismatch(node, "create_dir"));
        };
        let path = resolve_path("create_dir", ctx, path_from)?;
        check(&self.policy.check_fs_write(&path), "create_dir", &path)?;

        if ctx.dry_run {
            return Ok(NodeOutcome::Continue {
                value: json!({ "path": path.display().to_string(), "dry_run": true }),
                branch: None,
            });
        }

        fs::create_dir_all(&path).map_err(|e| Error::Tool {
            tool: "create_dir".into(),
            reason: format!("mkdir_p {}: {e}", path.display()),
        })?;
        Ok(NodeOutcome::Continue {
            value: json!({
                "path": path.display().to_string(),
                "created": true,
            }),
            branch: None,
        })
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn kind_mismatch(node: &Node, expected: &str) -> Error {
    Error::Tool {
        tool: expected.into(),
        reason: format!(
            "handler for `{expected}` received node `{}` of kind `{}`",
            node.id,
            node.kind.name()
        ),
    }
}

fn resolve_path(tool: &'static str, ctx: &ExecutionContext, dotted: &str) -> Result<PathBuf> {
    let s = resolve_string(tool, ctx, dotted)?;
    Ok(PathBuf::from(s))
}

fn check(dec: &Decision, tool: &str, path: &Path) -> Result<()> {
    match dec {
        Decision::Allow => Ok(()),
        Decision::Deny(reason) => Err(Error::Policy(format!(
            "{tool} denied on {}: {reason}",
            path.display()
        ))),
    }
}

fn path_size(path: &Path) -> Option<u64> {
    fs::metadata(path).ok().map(|m| m.len())
}

// We keep `value_type_name` in scope so future debug messages can use
// it; silence the "unused import" warning when no branch references it.
#[allow(dead_code)]
fn _value_type_name_ref(v: &Value) -> &'static str {
    value_type_name(v)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::context::{RunOptions, TriggerMeta};
    use crate::tools::policy::allow_all;
    use crate::workflow::model::Node;
    use tempfile::TempDir;

    fn ctx_with(input: Value) -> ExecutionContext {
        ExecutionContext::new(
            "e",
            "w",
            "s",
            TriggerMeta::manual(input),
            &RunOptions::default(),
        )
    }

    fn node(id: &str, kind: NodeKind) -> Node {
        Node {
            id: id.into(),
            retry: None,
            kind,
        }
    }

    #[test]
    fn read_file_returns_content() {
        let dir = TempDir::new().unwrap();
        let p = dir.path().join("hello.txt");
        std::fs::write(&p, "hi there").unwrap();

        let mut ctx = ctx_with(json!({ "path": p.display().to_string() }));
        let h = ReadFileHandler {
            policy: allow_all(),
        };
        let out = h
            .handle(
                &node(
                    "r",
                    NodeKind::ReadFile {
                        path_from: "trigger.path".into(),
                    },
                ),
                &mut ctx,
            )
            .unwrap();
        match out {
            NodeOutcome::Continue { value, .. } => {
                assert_eq!(value["content"], "hi there");
                assert_eq!(value["bytes"], 8);
            }
            other => panic!("unexpected outcome: {other:?}"),
        }
    }

    #[test]
    fn read_file_missing_path_errors() {
        let mut ctx = ctx_with(json!({}));
        let h = ReadFileHandler {
            policy: allow_all(),
        };
        let err = h
            .handle(
                &node(
                    "r",
                    NodeKind::ReadFile {
                        path_from: "trigger.nope".into(),
                    },
                ),
                &mut ctx,
            )
            .unwrap_err();
        assert!(format!("{err}").contains("not set in the execution context"));
    }

    #[test]
    fn write_file_creates_parents_and_writes() {
        let dir = TempDir::new().unwrap();
        let nested = dir.path().join("a/b/c/out.txt");
        let mut ctx = ctx_with(json!({
            "path": nested.display().to_string(),
            "content": "payload",
        }));
        let h = WriteFileHandler {
            policy: allow_all(),
            budget: crate::budget::unbounded(),
        };
        h.handle(
            &node(
                "w",
                NodeKind::WriteFile {
                    path_from: "trigger.path".into(),
                    content_from: "trigger.content".into(),
                },
            ),
            &mut ctx,
        )
        .unwrap();
        assert_eq!(std::fs::read_to_string(&nested).unwrap(), "payload");
    }

    #[test]
    fn write_file_dry_run_does_not_touch_disk() {
        let dir = TempDir::new().unwrap();
        let p = dir.path().join("out.txt");
        let mut ctx = ctx_with(json!({
            "path": p.display().to_string(),
            "content": "hi",
        }));
        ctx.dry_run = true;

        let h = WriteFileHandler {
            policy: allow_all(),
            budget: crate::budget::unbounded(),
        };
        let out = h
            .handle(
                &node(
                    "w",
                    NodeKind::WriteFile {
                        path_from: "trigger.path".into(),
                        content_from: "trigger.content".into(),
                    },
                ),
                &mut ctx,
            )
            .unwrap();
        assert!(!p.exists(), "file must not be created in dry-run mode");
        match out {
            NodeOutcome::Continue { value, .. } => {
                assert_eq!(value["dry_run"], true);
            }
            _ => panic!(),
        }
    }

    #[test]
    fn write_file_serialises_non_string_values() {
        let dir = TempDir::new().unwrap();
        let p = dir.path().join("o.json");
        let mut ctx = ctx_with(json!({
            "path": p.display().to_string(),
            "body": { "n": 42 },
        }));
        let h = WriteFileHandler {
            policy: allow_all(),
            budget: crate::budget::unbounded(),
        };
        h.handle(
            &node(
                "w",
                NodeKind::WriteFile {
                    path_from: "trigger.path".into(),
                    content_from: "trigger.body".into(),
                },
            ),
            &mut ctx,
        )
        .unwrap();
        let written = std::fs::read_to_string(&p).unwrap();
        assert_eq!(written, "{\"n\":42}");
    }

    #[test]
    fn create_dir_is_idempotent() {
        let dir = TempDir::new().unwrap();
        let nested = dir.path().join("x/y/z");
        let mut ctx = ctx_with(json!({ "path": nested.display().to_string() }));
        let h = CreateDirHandler {
            policy: allow_all(),
        };
        for _ in 0..2 {
            h.handle(
                &node(
                    "d",
                    NodeKind::CreateDir {
                        path_from: "trigger.path".into(),
                    },
                ),
                &mut ctx,
            )
            .unwrap();
        }
        assert!(nested.exists());
    }

    #[test]
    fn policy_deny_propagates() {
        use crate::tools::policy::{Decision, Policy};
        use std::sync::Arc;

        struct NoReads;
        impl Policy for NoReads {
            fn check_fs_read(&self, _: &Path) -> Decision {
                Decision::Deny("denied by test policy".into())
            }
        }

        let dir = TempDir::new().unwrap();
        let p = dir.path().join("f");
        std::fs::write(&p, "x").unwrap();

        let mut ctx = ctx_with(json!({ "path": p.display().to_string() }));
        let h = ReadFileHandler {
            policy: Arc::new(NoReads),
        };
        let err = h
            .handle(
                &node(
                    "r",
                    NodeKind::ReadFile {
                        path_from: "trigger.path".into(),
                    },
                ),
                &mut ctx,
            )
            .unwrap_err();
        assert!(format!("{err}").contains("denied by test policy"));
    }
}
