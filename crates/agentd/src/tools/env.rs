//! Environment tool family (RFC §10.2).
//!
//! Single handler: `env.read` reads the named environment variable
//! (after the policy's `check_env_read`) and emits either
//! `{"value": "..."}` or `{"value": null, "missing": true}`.

use std::env;

use serde_json::json;

use crate::engine::{ExecutionContext, HandlerRegistry, NodeHandler, NodeOutcome};
use crate::error::{Error, Result};
use crate::tools::policy::{Decision, PolicyRef};
use crate::workflow::{Node, NodeKind};

pub(crate) fn register(registry: &mut HandlerRegistry, policy: PolicyRef) {
    registry.register("read_env", Box::new(ReadEnvHandler { policy }));
}

pub struct ReadEnvHandler {
    policy: PolicyRef,
}

impl NodeHandler for ReadEnvHandler {
    fn handle(&self, node: &Node, _ctx: &mut ExecutionContext) -> Result<NodeOutcome> {
        let NodeKind::ReadEnv { key } = &node.kind else {
            return Err(Error::Tool {
                tool: "read_env".into(),
                reason: format!(
                    "handler for `read_env` received node `{}` of kind `{}`",
                    node.id,
                    node.kind.name()
                ),
            });
        };
        match self.policy.check_env_read(key) {
            Decision::Allow => {}
            Decision::Deny(reason) => {
                return Err(Error::Policy(format!("read_env `{key}`: {reason}")));
            }
        }
        match env::var(key) {
            Ok(value) => Ok(NodeOutcome::Continue {
                value: json!({ "key": key, "value": value }),
                branch: None,
            }),
            Err(env::VarError::NotPresent) => Ok(NodeOutcome::Continue {
                value: json!({ "key": key, "value": null, "missing": true }),
                branch: None,
            }),
            Err(env::VarError::NotUnicode(_)) => Err(Error::Tool {
                tool: "read_env".into(),
                reason: format!("env var `{key}` is not valid UTF-8"),
            }),
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::context::{RunOptions, TriggerMeta};
    use crate::tools::policy::allow_all;
    use serde_json::json;

    fn ctx() -> ExecutionContext {
        ExecutionContext::new(
            "e",
            "w",
            "s",
            TriggerMeta::manual(json!({})),
            &RunOptions::default(),
        )
    }

    fn node(id: &str, key: &str) -> Node {
        Node {
            id: id.into(),
            kind: NodeKind::ReadEnv { key: key.into() },
        }
    }

    #[test]
    fn reads_known_var() {
        let key = "AGENTD_TEST_ENV_KEY_OK";
        // SAFETY: single-threaded test scope; no other thread is
        // reading the environment concurrently. Rust 2024 requires
        // the explicit unsafe marker.
        unsafe { std::env::set_var(key, "hello") };
        let h = ReadEnvHandler {
            policy: allow_all(),
        };
        let mut c = ctx();
        let out = h.handle(&node("r", key), &mut c).unwrap();
        match out {
            NodeOutcome::Continue { value, .. } => {
                assert_eq!(value["value"], "hello");
            }
            _ => panic!(),
        }
        unsafe { std::env::remove_var(key) };
    }

    #[test]
    fn missing_var_reports_missing() {
        let key = "AGENTD_TEST_ENV_KEY_DEFINITELY_UNSET_42";
        unsafe { std::env::remove_var(key) };
        let h = ReadEnvHandler {
            policy: allow_all(),
        };
        let mut c = ctx();
        let out = h.handle(&node("r", key), &mut c).unwrap();
        match out {
            NodeOutcome::Continue { value, .. } => {
                assert!(
                    value
                        .get("missing")
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false)
                );
                assert_eq!(value["value"], serde_json::Value::Null);
            }
            _ => panic!(),
        }
    }

    #[test]
    fn policy_denial_errors() {
        use super::*;
        use crate::tools::policy::{Decision, Policy};
        use std::sync::Arc;

        struct NoEnv;
        impl Policy for NoEnv {
            fn check_env_read(&self, _: &str) -> Decision {
                Decision::Deny("nope".into())
            }
        }

        let h = ReadEnvHandler {
            policy: Arc::new(NoEnv),
        };
        let mut c = ctx();
        let err = h.handle(&node("r", "ANYTHING"), &mut c).unwrap_err();
        assert!(format!("{err}").contains("nope"));
    }
}
