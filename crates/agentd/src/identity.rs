// SPDX-License-Identifier: Apache-2.0
//! Instance identity from the Kubernetes downward API (RFC 0015 §6, RFC 0014 §6.4).
//!
//! agentd surfaces pod identity by reading **operator-injected environment
//! variables** (set from `valueFrom.fieldRef`) — never by calling the kube API.
//! There is no kube client, no in-cluster config, no service-account read; that
//! coupling belongs in agentctl, not here (the minimalism moat). Env in,
//! identity out.
//!
//! Every k8s field is optional and descriptive, never load-bearing: outside
//! Kubernetes the vars are simply unset and the fields are `None`. Their absence
//! is never a config error. `run_id` is always present (minted by config if
//! unset, RFC 0011 §6).

/// The instance's correlation identity. `run_id` is always present; the k8s
/// fields are populated from the downward-API env when injected, else `None`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Identity {
    /// AGENTD_RUN_ID / the minted run id (RFC 0011 §6). Always present.
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
        // De-branding (ACC SPEC L4 / env-convention.json): the neutral `AGENT_*`
        // downward-API vars are read FIRST, falling back to the branded `AGENTD_*`
        // spelling (still accepted, never dropped). `var()` keeps the empty⇒unset
        // coercion, so an empty neutral var also falls through to the branded one.
        Identity {
            run_id: run_id.to_string(),
            instance: var("AGENT_POD_NAME").or_else(|| var("AGENTD_POD_NAME")),
            uid: var("AGENT_POD_UID").or_else(|| var("AGENTD_POD_UID")),
            namespace: var("AGENT_POD_NAMESPACE").or_else(|| var("AGENTD_POD_NAMESPACE")),
            node: var("AGENT_NODE_NAME").or_else(|| var("AGENTD_NODE_NAME")),
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

    // The downward-API vars are process-global, so the env-present and
    // env-absent cases share one test to avoid cross-test races.
    #[test]
    fn from_env_populates_when_set_and_none_when_absent() {
        // The downward-API vars (branded + neutral) are process-global, so the
        // present/absent/branded/neutral cases share one test to avoid races.
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
        // Absent: with every spelling unset, each k8s field is None; run_id holds.
        for k in all {
            unsafe { std::env::remove_var(k) };
        }
        let id = Identity::from_env("run-abc");
        assert_eq!(id.run_id, "run-abc");
        assert_eq!(id.instance, None);
        assert_eq!(id.uid, None);
        assert_eq!(id.namespace, None);
        assert_eq!(id.node, None);

        // Branded present (the back-compat path): each `AGENTD_*` var maps to its
        // field via the branded fallback (no neutral var set).
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

        // Neutral present, branded cleared (ACC SPEC L4): the `AGENT_*` spelling is
        // accepted on its own.
        for k in [
            "AGENTD_POD_NAME",
            "AGENTD_POD_UID",
            "AGENTD_POD_NAMESPACE",
            "AGENTD_NODE_NAME",
        ] {
            unsafe { std::env::remove_var(k) };
        }
        unsafe {
            std::env::set_var("AGENT_POD_NAME", "neutral-pod");
            std::env::set_var("AGENT_POD_UID", "neutral-uid");
            std::env::set_var("AGENT_POD_NAMESPACE", "neutral-ns");
            std::env::set_var("AGENT_NODE_NAME", "neutral-node");
        }
        let id = Identity::from_env("r");
        assert_eq!(id.instance.as_deref(), Some("neutral-pod"));
        assert_eq!(id.uid.as_deref(), Some("neutral-uid"));
        assert_eq!(id.namespace.as_deref(), Some("neutral-ns"));
        assert_eq!(id.node.as_deref(), Some("neutral-node"));

        // Both present ⇒ neutral-first wins (the read order is AGENT_* then AGENTD_*).
        unsafe { std::env::set_var("AGENTD_POD_NAME", "branded-pod") };
        assert_eq!(
            Identity::from_env("r").instance.as_deref(),
            Some("neutral-pod")
        );

        // An empty neutral var reads as unset and falls through to the branded one.
        unsafe { std::env::set_var("AGENT_POD_NAME", "") };
        assert_eq!(
            Identity::from_env("r").instance.as_deref(),
            Some("branded-pod")
        );
        // …and with BOTH empty/unset, the field is None.
        unsafe { std::env::remove_var("AGENTD_POD_NAME") };
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
