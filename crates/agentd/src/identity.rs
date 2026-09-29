// SPDX-License-Identifier: AGPL-3.0-only
//! Instance identity from the Kubernetes downward API.
//!
//! agentd surfaces pod identity by reading **operator-injected environment
//! variables** (set from `valueFrom.fieldRef`) — never by calling the kube API.
//! There is no kube client, no in-cluster config, no service-account read; that
//! coupling belongs in agentctl, not here (the minimalism moat). Env in,
//! identity out.
//!
//! Every k8s field is optional and descriptive, never load-bearing: outside
//! Kubernetes the vars are simply unset and the fields are `None`. Their absence
//! is never a config error. `run_id` is always present (minted by config when
//! unset), because every log line and durable record correlates on it.

/// The instance's correlation identity. `run_id` is always present; the k8s
/// fields are populated from the downward-API env when injected, else `None`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Identity {
    /// `AGENTD_RUN_ID`, or the run id minted at startup. Always present.
    pub run_id: String,
    /// `metadata.name` via `AGENTD_POD_NAME`.
    pub instance: Option<String>,
    /// `metadata.uid` via `AGENTD_POD_UID`.
    pub uid: Option<String>,
    /// `metadata.namespace` via `AGENTD_POD_NAMESPACE`.
    pub namespace: Option<String>,
    /// `spec.nodeName` via `AGENTD_NODE_NAME`.
    pub node: Option<String>,
}

impl Identity {
    /// Read identity from the environment (getenv only — no syscalls beyond
    /// that, no validation side effects). `run_id` comes from the already
    /// resolved config; the k8s fields each read their downward-API var and
    /// resolve to `None` when absent.
    pub fn from_env(run_id: &str) -> Identity {
        // `AGENTD_*`, the one prefix agentd reads: an `AGENT_*` spelling is
        // some other program's variable.
        Identity {
            run_id: run_id.to_string(),
            instance: var("AGENTD_POD_NAME"),
            uid: var("AGENTD_POD_UID"),
            namespace: var("AGENTD_POD_NAMESPACE"),
            node: var("AGENTD_NODE_NAME"),
        }
    }
}

/// A non-empty environment variable, or `None`. An empty value is treated as
/// unset (an operator clearing a fieldRef should not surface as `""`).
fn var(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    // The downward-API vars are process-global, so the present, absent and
    // other-prefix cases share one test to avoid cross-test races.
    #[test]
    fn from_env_populates_when_set_and_none_when_absent() {
        let all = [
            "AGENT_POD_NAME",
            "AGENT_POD_UID",
            "AGENT_POD_NAMESPACE",
            "AGENT_NODE_NAME",
            "AGENTD_POD_NAME",
            "AGENTD_POD_UID",
            "AGENTD_POD_NAMESPACE",
            "AGENTD_NODE_NAME",
        ];
        // Absent: with every var unset, each k8s field is None; run_id holds.
        for k in all {
            unsafe { std::env::remove_var(k) };
        }
        let id = Identity::from_env("run-abc");
        assert_eq!(id.run_id, "run-abc");
        assert_eq!(id.instance, None);
        assert_eq!(id.uid, None);
        assert_eq!(id.namespace, None);
        assert_eq!(id.node, None);

        // An `AGENT_*` spelling is not read: it names nothing of agentd's.
        unsafe {
            std::env::set_var("AGENT_POD_NAME", "neutral-pod");
            std::env::set_var("AGENT_POD_UID", "neutral-uid");
            std::env::set_var("AGENT_POD_NAMESPACE", "neutral-ns");
            std::env::set_var("AGENT_NODE_NAME", "neutral-node");
        }
        assert_eq!(
            Identity::from_env("r"),
            Identity {
                run_id: "r".into(),
                instance: None,
                uid: None,
                namespace: None,
                node: None
            }
        );

        // Present: each `AGENTD_*` var maps to its field.
        unsafe {
            std::env::set_var("AGENTD_POD_NAME", "agent-pod-abc");
            std::env::set_var("AGENTD_POD_UID", "f3c1-uid");
            std::env::set_var("AGENTD_POD_NAMESPACE", "agents");
            std::env::set_var("AGENTD_NODE_NAME", "node-3");
        }
        let id = Identity::from_env("run-xyz");
        assert_eq!(id.run_id, "run-xyz");
        assert_eq!(id.instance.as_deref(), Some("agent-pod-abc"));
        assert_eq!(id.uid.as_deref(), Some("f3c1-uid"));
        assert_eq!(id.namespace.as_deref(), Some("agents"));
        assert_eq!(id.node.as_deref(), Some("node-3"));

        // An empty var reads as unset.
        unsafe { std::env::set_var("AGENTD_POD_NAME", "") };
        assert_eq!(Identity::from_env("r").instance, None);

        for k in all {
            unsafe { std::env::remove_var(k) };
        }
    }

    #[test]
    fn run_id_is_always_present() {
        let id = Identity::from_env("only-run-id");
        assert_eq!(id.run_id, "only-run-id");
    }
}
