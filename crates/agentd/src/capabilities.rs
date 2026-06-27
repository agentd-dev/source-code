//! The capabilities manifest — the shared spine of the agentctl control-plane
//! track (RFC 0015 §5.2, RFC 0014 §5). One builder feeds two surfaces (the
//! one-shot `agentd --capabilities` and, later, the live `agentd://capabilities`
//! resource) so they never drift.
//!
//! The manifest is a machine-readable description of *what this binary is and
//! what it serves right now*: contract/build versions, compiled-in features,
//! downward-API identity, the configured run shape, and the **`surfaces` block**
//! — the graceful-degradation contract (RFC 0014 §8): each control-plane surface
//! is reported honestly as served-or-not for THIS build/config, so agentctl
//! drives only what is declared.
//!
//! **No secrets, ever** (RFC 0012 §3.7): the manifest carries no tokens, no
//! credentials, no resolved `{{secret:NAME}}` values. `intelligence` is
//! structural — transport scheme + endpoint count — never an endpoint URL with
//! embedded creds. The `Secret` newtype has no `Serialize`, so it cannot reach
//! this builder.

use crate::config::Config;
use crate::identity::Identity;
use crate::supervisor::tree::Caps;
use serde_json::{Value, json};

/// The agentctl↔agentd contract version (major.minor) this binary speaks
/// (RFC 0014 §3.4 / §6.3). agentctl refuses an instance whose *major* it does
/// not understand.
const CONTRACT_VERSION: &str = "1.0";

/// The operator tools this build actually serves on the management transport
/// (RFC 0015 §4) — the authoritative `surfaces.operator_tools` list, which
/// MIRRORS what `tools/list` returns to a `Management` peer (the served gate
/// reads this same const, so the manifest and the live surface cannot drift,
/// RFC 0015 §5.2). `pause`/`resume` are DEFERRED (they need a new `ctrl/pause`
/// control message + loop turn-boundary suspension that does not yet exist), so
/// they are intentionally absent until built — capability-absence-not-error
/// (RFC 0015 §2.5 / §5.5).
pub const OPERATOR_TOOLS: &[&str] = &["drain", "lame-duck", "cancel"];

/// Build the capabilities manifest from resolved config + identity.
///
/// `live` distinguishes the two emission paths: `false` is the one-shot
/// pre-connect probe (`agentd --capabilities`) — `intelligence.healthy` is
/// reported as the JSON string `"unknown"` because nothing has connected yet;
/// the transport/count are static config and always real. `true` is for the
/// live resource path (a later chunk), where the last-known reachability is
/// filled in.
pub fn manifest(cfg: &Config, identity: &Identity, live: bool) -> Value {
    json!({
        "contract_version": CONTRACT_VERSION,
        "agentd_version": crate::VERSION,
        "build_features": build_features(),
        "identity": identity_block(identity),
        "mode": cfg.mode.as_str(),
        // The configured model id is operator-declared metadata, never a secret.
        "model": cfg.model,
        "intelligence": intelligence(cfg, live),
        "intelligence_summary": intelligence_summary(),
        "mcp_servers": mcp_servers(cfg),
        "a2a_peers": a2a_peers(cfg),
        "exec_enabled": cfg.enable_exec,
        "allow_trifecta": cfg.allow_trifecta,
        "limits": limits(cfg),
        "surfaces": surfaces(cfg),
    })
}

/// The cargo features compiled into this binary, computed at runtime via
/// `cfg!` — only the ones actually present are pushed (RFC 0014 §5).
fn build_features() -> Vec<&'static str> {
    let mut f = Vec::new();
    if cfg!(feature = "tls") {
        f.push("tls");
    }
    if cfg!(feature = "vsock") {
        f.push("vsock");
    }
    if cfg!(feature = "serve-mcp") {
        f.push("serve-mcp");
    }
    if cfg!(feature = "a2a") {
        f.push("a2a");
    }
    if cfg!(feature = "cron") {
        f.push("cron");
    }
    if cfg!(feature = "metrics") {
        f.push("metrics");
    }
    if cfg!(feature = "otel") {
        f.push("otel");
    }
    f
}

/// The downward-API identity block (§6). `run_id` is always present; absent
/// k8s fields serialize as JSON `null`.
fn identity_block(id: &Identity) -> Value {
    json!({
        "run_id": id.run_id,
        "instance": id.instance,
        "uid": id.uid,
        "node": id.node,
        "namespace": id.namespace,
    })
}

/// Structural intelligence summary: transport scheme + configured endpoint
/// count + reachability. Never the endpoint URL (no embedded creds), never the
/// token (RFC 0012 §3.7). `healthy` is `"unknown"` on the pre-connect one-shot
/// path; the count/transport are static config.
fn intelligence(cfg: &Config, live: bool) -> Value {
    // `live` is the live-resource path (a later RFC chunk), which will fill in
    // last-known reachability; the one-shot pre-connect probe (`live == false`)
    // and the not-yet-wired live path both report `"unknown"` for now. The
    // transport/count are static config and always real.
    let _ = live;
    json!({
        "transport": transport_scheme(cfg.intelligence.as_deref()),
        // v1 configures a single endpoint; multi-endpoint is RFC 0018.
        "endpoints": cfg.intelligence.as_ref().map_or(0, |_| 1),
        "healthy": "unknown",
    })
}

/// Map the intelligence URI to its structural transport scheme (RFC 0006),
/// never leaking the URL itself. `None`/unrecognised ⇒ JSON `null`.
fn transport_scheme(uri: Option<&str>) -> Value {
    match uri {
        Some(u) if u.starts_with("unix:") => json!("unix"),
        Some(u) if u.starts_with("vsock:") => json!("vsock"),
        Some(u) if u.starts_with("https://") || u.starts_with("http://") => json!("https"),
        _ => Value::Null,
    }
}

/// Coarse capability hints for placement (no secrets, RFC 0012). `toolmode` is
/// `"native"`: the runtime's intelligence adapters use native tool-calling
/// (RFC 0006), with the JSON-action shape as a parse fallback, not a configured
/// mode. `max_context_hint` is an operator-declared hint with no config field
/// in this chunk, so it is omitted unless configured.
fn intelligence_summary() -> Value {
    json!({
        "toolmode": "native",
    })
}

/// Operator-declared MCP client servers, name + trifecta tags (RFC 0004 / 0012
/// §3.1). Only the structural name+tags — never the spawn command.
fn mcp_servers(cfg: &Config) -> Value {
    let servers: Vec<Value> = cfg
        .mcp_servers
        .iter()
        .map(|s| json!({ "name": s.name, "tags": s.tags }))
        .collect();
    Value::Array(servers)
}

/// Operator-declared remote-A2A delegation peers (RFC 0020 §3): name + structural
/// transport scheme only — never the endpoint path/cid (no addressing leak,
/// mirroring the intelligence/mcp blocks). The `a2a.delegate` self-tool dials
/// these; the operator/gateway can see which peers a pod is wired to.
fn a2a_peers(cfg: &Config) -> Value {
    let peers: Vec<Value> = cfg
        .a2a_peers
        .iter()
        .map(|p| {
            let transport = if p.endpoint.starts_with("vsock:") {
                "vsock"
            } else if p.endpoint.starts_with("unix:") {
                "unix"
            } else {
                "unknown"
            };
            json!({ "name": p.name, "transport": transport })
        })
        .collect();
    Value::Array(peers)
}

/// The bounding box (RFC 0007/0009/0003). The per-run limits come from config;
/// the tree caps (`max_children`, `max_total_subagents`, `tree_token_budget`)
/// are the supervisor's fork-bomb defaults — read from `Caps` so the manifest
/// can never drift from what the spawn chokepoint actually enforces.
fn limits(cfg: &Config) -> Value {
    let caps = Caps::default();
    // ~10 years if no deadline, matching the root payload's overflow-safe sentinel.
    let deadline_ms = cfg
        .deadline
        .map(|d| d.as_millis() as u64)
        .unwrap_or(315_360_000_000);
    json!({
        "max_depth": cfg.max_depth,
        "max_children": caps.max_children,
        "max_total_subagents": caps.max_total,
        "max_steps": cfg.max_steps,
        "max_tokens": cfg.max_tokens,
        "tree_token_budget": caps.tree_token_ceiling,
        "deadline_ms": deadline_ms,
        "drain_timeout_ms": cfg.drain_timeout.as_millis() as u64,
    })
}

/// The graceful-degradation contract (RFC 0014 §8): report HONESTLY which
/// control-plane surfaces THIS build/config serves right now. Surfaces not yet
/// implemented in this chunk are reported `false`/empty; later RFC chunks flip
/// them true. agentctl reads this and drives only what is declared.
fn surfaces(cfg: &Config) -> Value {
    json!({
        // The served self-MCP management transport: its address string if
        // configured (and built), else false. RFC 0015 §3.
        "management": cfg.serve_mcp.clone().map_or(Value::Bool(false), Value::String),
        // Operator tools listed to a `Management` peer (RFC 0015 §4). They exist
        // only on the management transport, so they're advertised only when this
        // build can serve it (`serve-mcp`); otherwise the surface is empty —
        // capability-absence-not-error (RFC 0015 §2.5). `pause`/`resume` are
        // deferred (see OPERATOR_TOOLS).
        "operator_tools": operator_tools(),
        // The A2A external-agent surface (RFC 0020). When this build serves A2A
        // (the `a2a` feature, which rides the management transport), advertise the
        // served unary method set and `streaming:false` (A2A-2 adds streaming);
        // else `false`. The Agent Card itself is the gateway's projection of this
        // manifest (RFC 0020 §2.3) — agentd only advertises the capability here.
        "a2a": a2a_surface(),
        // The /metrics addr if configured, else false. RFC 0010 / 0016.
        "metrics": cfg.metrics_addr.clone().map_or(Value::Bool(false), Value::String),
        "metrics_schema": "1.0",
        // Not yet built in this chunk (later RFC chunks flip these true).
        "events": false,
        "hot_reload": false,
        "config_validate": false,
        // The exit-code table version this binary honours (RFC 0011 §5).
        "exit_codes": "RFC-0011-§5",
    })
}

/// The operator tools this build advertises (RFC 0015 §4 / §5.2). Non-empty only
/// with the `serve-mcp` feature — without the management transport there is
/// nothing to serve them on, so the surface is honestly empty.
fn operator_tools() -> Vec<&'static str> {
    if cfg!(feature = "serve-mcp") {
        OPERATOR_TOOLS.to_vec()
    } else {
        Vec::new()
    }
}

/// The A2A surface advertisement (RFC 0020). The `a2a` build serves the four unary
/// methods plus the status-level streaming pair over the management transport with
/// `streaming:true` (A2A-2); without the feature the surface is honestly `false`
/// (capability-absence-not-error, RFC 0015 §2.5). The method names here MIRROR the
/// `a2a.*` dispatch in [`crate::mcp::server`] / [`crate::mcp::a2a`] — the gateway
/// reads this to build the Agent Card and to know which methods to bridge.
fn a2a_surface() -> Value {
    if cfg!(feature = "a2a") {
        json!({
            "version": "1.0",
            // Status-level streaming: SendStreamingMessage + SubscribeToTask emit a
            // StreamResponse frame stream (distillate-only artifact on completion).
            "streaming": true,
            "methods": [
                "a2a.SendMessage",
                "a2a.GetTask",
                "a2a.CancelTask",
                "a2a.ListTasks",
                "a2a.SendStreamingMessage",
                "a2a.SubscribeToTask",
            ],
        })
    } else {
        Value::Bool(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    fn cfg_with(env: &[(&str, &str)], args: &[&str]) -> Config {
        let env: Vec<(String, String)> = env
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        let args: Vec<String> = args.iter().map(|s| s.to_string()).collect();
        Config::load(&args, &env).unwrap()
    }

    fn base() -> Config {
        cfg_with(
            &[
                ("INSTRUCTION", "x"),
                ("AGENTD_INTELLIGENCE", "https://api.example/v1"),
            ],
            &[],
        )
    }

    #[test]
    fn manifest_has_the_frozen_top_level_keys() {
        let cfg = base();
        let id = Identity::from_env(&cfg.run_id);
        let m = manifest(&cfg, &id, false);
        for key in [
            "contract_version",
            "agentd_version",
            "build_features",
            "identity",
            "mode",
            "model",
            "intelligence",
            "intelligence_summary",
            "mcp_servers",
            "exec_enabled",
            "allow_trifecta",
            "limits",
            "surfaces",
        ] {
            assert!(m.get(key).is_some(), "manifest missing top-level key {key}");
        }
        assert_eq!(m["contract_version"], json!("1.0"));
        assert_eq!(m["agentd_version"], json!(crate::VERSION));
        // run_id is always present in identity.
        assert_eq!(m["identity"]["run_id"], json!(cfg.run_id));
    }

    #[test]
    fn build_features_reflects_cfg() {
        let feats = build_features();
        // Every reported feature must actually be compiled in.
        for f in &feats {
            let present = match *f {
                "tls" => cfg!(feature = "tls"),
                "vsock" => cfg!(feature = "vsock"),
                "serve-mcp" => cfg!(feature = "serve-mcp"),
                "a2a" => cfg!(feature = "a2a"),
                "cron" => cfg!(feature = "cron"),
                "metrics" => cfg!(feature = "metrics"),
                "otel" => cfg!(feature = "otel"),
                other => panic!("unexpected build feature {other}"),
            };
            assert!(present, "reported feature {f} is not compiled in");
        }
        // And the converse: a compiled-in feature is reported.
        if cfg!(feature = "serve-mcp") {
            assert!(feats.contains(&"serve-mcp"));
        }
    }

    #[test]
    fn one_shot_intelligence_healthy_is_unknown_and_transport_is_structural() {
        let cfg = base();
        let id = Identity::from_env(&cfg.run_id);
        let m = manifest(&cfg, &id, false);
        assert_eq!(m["intelligence"]["healthy"], json!("unknown"));
        assert_eq!(m["intelligence"]["transport"], json!("https"));
        assert_eq!(m["intelligence"]["endpoints"], json!(1));
    }

    #[test]
    fn surfaces_reports_not_yet_built_ones_off() {
        let cfg = base();
        let id = Identity::from_env(&cfg.run_id);
        let s = &manifest(&cfg, &id, false)["surfaces"];
        // Not configured ⇒ false.
        assert_eq!(s["management"], json!(false));
        assert_eq!(s["metrics"], json!(false));
        // operator_tools mirrors the built management surface (RFC 0015 §5.2):
        // the drain/lame-duck/cancel set with `serve-mcp`, empty without it.
        // pause/resume are deferred and never appear in either build.
        if cfg!(feature = "serve-mcp") {
            assert_eq!(s["operator_tools"], json!(["drain", "lame-duck", "cancel"]));
        } else {
            assert_eq!(s["operator_tools"], json!([]));
        }
        assert_eq!(s["events"], json!(false));
        assert_eq!(s["hot_reload"], json!(false));
        assert_eq!(s["config_validate"], json!(false));
        // Frozen strings.
        assert_eq!(s["metrics_schema"], json!("1.0"));
        assert_eq!(s["exit_codes"], json!("RFC-0011-§5"));
    }

    #[test]
    fn surfaces_advertise_a2a_per_build(/* RFC 0020 §integration */) {
        let cfg = base();
        let id = Identity::from_env(&cfg.run_id);
        let a2a = &manifest(&cfg, &id, false)["surfaces"]["a2a"];
        if cfg!(feature = "a2a") {
            // The served unary set + the A2A-2 status-level streaming pair, with
            // streaming honestly true.
            assert_eq!(a2a["version"], json!("1.0"));
            assert_eq!(a2a["streaming"], json!(true));
            assert_eq!(
                a2a["methods"],
                json!([
                    "a2a.SendMessage",
                    "a2a.GetTask",
                    "a2a.CancelTask",
                    "a2a.ListTasks",
                    "a2a.SendStreamingMessage",
                    "a2a.SubscribeToTask"
                ])
            );
        } else {
            // Capability-absence-not-error: no a2a build ⇒ the surface is `false`.
            assert_eq!(a2a, &json!(false));
        }
    }

    #[test]
    fn surfaces_report_configured_addresses() {
        // serve-mcp + metrics-addr configured ⇒ their address strings surface.
        let cfg = cfg_with(
            &[
                ("INSTRUCTION", "x"),
                ("AGENTD_INTELLIGENCE", "unix:/x"),
                ("AGENTD_SERVE_MCP", "unix:/run/agentd.sock"),
                ("AGENTD_METRICS_ADDR", ":9090"),
            ],
            &[],
        );
        let id = Identity::from_env(&cfg.run_id);
        let s = &manifest(&cfg, &id, false)["surfaces"];
        assert_eq!(s["management"], json!("unix:/run/agentd.sock"));
        assert_eq!(s["metrics"], json!(":9090"));
    }

    #[test]
    fn limits_block_carries_caps_and_config() {
        let cfg = cfg_with(
            &[("INSTRUCTION", "x"), ("AGENTD_INTELLIGENCE", "unix:/x")],
            &["--max-depth", "3", "--max-steps", "99"],
        );
        let id = Identity::from_env(&cfg.run_id);
        let l = &manifest(&cfg, &id, false)["limits"];
        assert_eq!(l["max_depth"], json!(3));
        assert_eq!(l["max_steps"], json!(99));
        assert_eq!(l["max_children"], json!(8));
        assert_eq!(l["max_total_subagents"], json!(64));
        assert!(l["tree_token_budget"].is_u64());
        assert!(l["deadline_ms"].is_u64());
        assert!(l["drain_timeout_ms"].is_u64());
    }

    #[test]
    fn mcp_servers_carry_name_and_tags_not_command() {
        let cfg = cfg_with(
            &[("INSTRUCTION", "x"), ("AGENTD_INTELLIGENCE", "unix:/x")],
            &[
                "--mcp",
                "vault=mcp-vault --secret-cmd",
                "--mcp-tags",
                "vault=sensitive",
            ],
        );
        let id = Identity::from_env(&cfg.run_id);
        let m = manifest(&cfg, &id, false);
        let servers = m["mcp_servers"].as_array().unwrap();
        assert_eq!(servers.len(), 1);
        assert_eq!(servers[0]["name"], json!("vault"));
        assert_eq!(servers[0]["tags"], json!(["sensitive"]));
        // The spawn command must NOT appear anywhere in the manifest.
        let blob = serde_json::to_string(&m).unwrap();
        assert!(
            !blob.contains("secret-cmd"),
            "mcp command leaked into manifest"
        );
        assert!(servers[0].get("command").is_none());
    }

    #[test]
    fn no_secret_or_token_appears_in_the_manifest() {
        // A token IS configured; it must never be serialized into the manifest.
        const TOKEN: &str = "super-secret-token-value";
        let cfg = cfg_with(
            &[
                ("INSTRUCTION", "x"),
                (
                    "AGENTD_INTELLIGENCE",
                    "https://user:embedded-cred@api.example/v1",
                ),
                ("AGENTD_INTELLIGENCE_TOKEN", TOKEN),
            ],
            &[],
        );
        let id = Identity::from_env(&cfg.run_id);
        let m = manifest(&cfg, &id, false);
        let blob = serde_json::to_string(&m).unwrap();
        assert!(
            !blob.contains(TOKEN),
            "intelligence token leaked into manifest"
        );
        // The endpoint URL (which can embed creds) must not appear either —
        // only the structural transport scheme.
        assert!(
            !blob.contains("embedded-cred"),
            "endpoint URL leaked into manifest"
        );
        assert!(
            !blob.contains("api.example"),
            "endpoint host leaked into manifest"
        );
        assert_eq!(m["intelligence"]["transport"], json!("https"));
    }
}
