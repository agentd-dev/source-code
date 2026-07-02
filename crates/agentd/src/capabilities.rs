// SPDX-License-Identifier: Apache-2.0
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
const CONTRACT_VERSION: &str = "2.0";

/// The operator tools this build actually serves on the management transport
/// (RFC 0015 §4) — the authoritative `surfaces.operator_tools` list, which
/// MIRRORS what `tools/list` returns to a `Management` peer (the served gate
/// reads this same const, so the manifest and the live surface cannot drift,
/// RFC 0015 §5.2). `pause`/`resume` fan `ctrl/pause`/`ctrl/resume` to suspend the
/// agentic tree at turn boundaries (RFC 0015 §4.3).
pub const OPERATOR_TOOLS: &[&str] = &["drain", "lame-duck", "pause", "resume", "cancel"];

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
        // Neutral de-branded version key (ACC SPEC L4). The runtime is fully
        // de-branded to neutral spellings; the legacy `agentd_version` is no longer
        // emitted (the manifest root anyOf is satisfied by `agent_version` alone).
        "agent_version": crate::VERSION,
        "build_features": build_features(),
        "identity": identity_block(identity),
        "mode": cfg.mode.as_str(),
        // The configured model id is operator-declared metadata, never a secret.
        "model": cfg.model,
        "intelligence": intelligence(cfg, live),
        "intelligence_summary": intelligence_summary(),
        "mcp_servers": mcp_servers(cfg),
        "a2a_peers": a2a_peers(cfg),
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
    if cfg!(feature = "serve-mcp") {
        f.push("serve-mcp");
    }
    if cfg!(feature = "serve-https") {
        f.push("serve-https");
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
///
/// RFC 0018 §5.4 model-discovery (`discovery`/`models`) is **NOT** probed here:
/// this builder is network-free so the one-shot `agentd --capabilities`
/// (`live == false`) is side-effect-free admission (RFC 0015 §5.2 — no socket).
/// The one-shot reports `discovery:false` and `models` as the configured model
/// only (a config-capability view). The LIVE served read overlays the actual
/// probed discovery via [`intelligence_discovery_overlay`] (gated on `live`).
fn intelligence(cfg: &Config, live: bool) -> Value {
    // `live` is the live-resource path (a later RFC chunk), which will fill in
    // last-known reachability; the one-shot pre-connect probe (`live == false`)
    // and the not-yet-wired live path both report `"unknown"` for now. The
    // transport/count are static config and always real.
    let _ = live;
    json!({
        // The PRIMARY endpoint's transport scheme (the list may mix transports,
        // RFC 0018 §3.1; the primary is `eps[0]`). Never the URL/creds.
        "transport": transport_scheme(primary_endpoint(cfg.intelligence.as_deref())),
        // The configured endpoint-list length (RFC 0018 §3.1 / §5.4): a
        // comma-separated `--intelligence` list parses to N endpoints; a single
        // element is exactly RFC 0006 (count 1). The full per-endpoint health
        // view lives at `agentd://intelligence` (§4.4) — the manifest carries
        // only the bounded count.
        "endpoints": endpoint_count(cfg.intelligence.as_deref()),
        "healthy": "unknown",
        // RFC 0018 §5.4 discovery — additive (RFC 0014 §3; absent/false ⇒ agentctl
        // assumes only the configured model). Network-free baseline: NOT probed on
        // the one-shot admission path; `false` + the configured model only. The
        // served read overlays the real probe ([`intelligence_discovery_overlay`]).
        "discovery": false,
        "models": configured_models(cfg.model.as_deref()),
    })
}

/// The configured model as a one-element list (`[]` when unset) — the
/// network-free `intelligence.models` baseline for the one-shot manifest (RFC
/// 0018 §5.4: the configured model is always usable, even with discovery off).
fn configured_models(model: Option<&str>) -> Vec<String> {
    model
        .filter(|m| !m.is_empty())
        .map(|m| vec![m.to_string()])
        .unwrap_or_default()
}

/// Overlay RFC 0018 §5.4 discovery (`discovery` + `models`) onto an already-built
/// manifest's `intelligence` block, for the LIVE served `agentd://capabilities`
/// read ONLY. The supervisor probes lazily + cached (off the hot path) and passes
/// the result here; the one-shot admission manifest never calls this (it stays
/// network-free, RFC 0015 §5.2). `models` is the union of discovered + configured
/// (already folded by [`crate::intel::discovery::discover`]).
pub fn intelligence_discovery_overlay(manifest: &mut Value, discovery: bool, models: &[String]) {
    if let Some(intel) = manifest
        .get_mut("intelligence")
        .and_then(Value::as_object_mut)
    {
        intel.insert("discovery".into(), json!(discovery));
        intel.insert("models".into(), json!(models));
    }
}

/// The first (primary) element of the comma-list `--intelligence` value, for the
/// manifest's `transport` scheme. `None` ⇒ no endpoint configured.
fn primary_endpoint(uri: Option<&str>) -> Option<&str> {
    uri.and_then(|u| u.split(',').map(str::trim).find(|s| !s.is_empty()))
}

/// The number of non-empty endpoints in the comma-list `--intelligence` value
/// (RFC 0018 §3.1). `0` when unset; a single element is `1` (RFC 0006).
fn endpoint_count(uri: Option<&str>) -> usize {
    uri.map_or(0, |u| {
        u.split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .count()
    })
}

/// Map the intelligence URI to its structural transport scheme, never leaking
/// the URL itself. HTTPS-only (target-vision pivot): both `https://` and the
/// loopback-dev `http://` report `"https"` — the structural transport is HTTP
/// over TCP; TLS-ness is a deployment property. `None`/unrecognised ⇒ `null`.
fn transport_scheme(uri: Option<&str>) -> Value {
    match uri {
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
            // A2A peers are HTTP(S)-only (pivot Phase 3); report the structural
            // scheme, never the URL (RFC 0012 §3.7).
            let transport =
                if p.endpoint.starts_with("https://") || p.endpoint.starts_with("http://") {
                    "https"
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
    // The events stream is served only with the `events` feature AND a management
    // transport to serve it on (RFC 0016 §7). Computed once: it gates both the
    // `events` bool and the `events_schema` envelope-version insert below.
    let events_served = cfg!(feature = "events") && cfg.serve_mcp.is_some();
    // `mut` is needed for the conditional `claim` / `events_schema` inserts below.
    #[cfg_attr(not(feature = "cluster"), allow(unused_mut))]
    let mut s = json!({
        // The served self-MCP management transport: its address string if
        // configured (and built), else false. RFC 0015 §3.
        "management": cfg.serve_mcp.clone().map_or(Value::Bool(false), Value::String),
        // Operator tools listed to a `Management` peer (RFC 0015 §4). They exist
        // only on the management transport, so they're advertised only when this
        // build can serve it (`serve-mcp`); otherwise the surface is empty —
        // capability-absence-not-error (RFC 0015 §2.5).
        "operator_tools": operator_tools(),
        // The A2A external-agent surface (RFC 0020). When this build serves A2A
        // (the `a2a` feature, which rides the management transport), advertise the
        // served unary method set and `streaming:false` (A2A-2 adds streaming);
        // else `false`. The Agent Card itself is the gateway's projection of this
        // manifest (RFC 0020 §2.3) — agentd only advertises the capability here.
        "a2a": a2a_surface(),
        // The /metrics addr if configured, else false. RFC 0010 / 0016. The
        // frozen metrics-schema version (RFC 0016 §4/§8.1) is read from its owning
        // module — never hardcoded — so the manifest can't drift from the surface.
        "metrics": cfg.metrics_addr.clone().map_or(Value::Bool(false), Value::String),
        "metrics_schema": crate::obs::metrics::METRICS_SCHEMA,
        // The agentd://events stream (RFC 0016 §7): served only when this build has
        // the `events` feature AND a management transport to serve it on. The
        // envelope-version `events_schema` is emitted alongside it below (when served).
        "events": events_served,
        // Run-outcome reports (RFC 0016 §6) — the report schema this binary writes.
        "report_schema": crate::report::REPORT_SCHEMA,
        // The frozen exit-code contract version (RFC 0016 §5, around RFC 0011 §5).
        "exit_codes": crate::exit::EXIT_CODES,
        // The agentd://intelligence endpoint-health resource (RFC 0018 §4.4). The
        // failover/health core is always on, but the observable resource rides the
        // management transport, so it's advertised only with `serve-mcp`.
        "intelligence": cfg!(feature = "serve-mcp"),
        // The agentd://config/effective resource (RFC 0017 §4.2 / §5.6): the live,
        // redacted reloadable-config view, Management-only + subscribable. Rides the
        // management transport, so it's advertised only with `serve-mcp`.
        "config_effective": cfg!(feature = "serve-mcp"),
        // RFC 0017 control-plane surfaces. `config_validate` (--validate-config,
        // §4.1) and `config_schema` (--config-schema, §4.2) are dependency-free
        // default-build flags — always available, so always advertised true.
        // `hot_reload` (§5) is the SIGHUP-triggered reload of the reloadable
        // subset — served only in a `hot-reload` build (the SIGHUP handler is
        // feature-gated; without it SIGHUP keeps its default disposition). The
        // inotify file-watch trigger is a documented follow-up.
        "hot_reload": cfg!(feature = "hot-reload"),
        "config_validate": true,
        "config_schema": true,
        // RFC 0019 horizontal-scaling surface. `cluster` is true in a `cluster`
        // build (sharding + the capacity resource are present). `shard` is this
        // instance's "K/N" identity, or null when unsharded (N==1) / no cluster.
        // `standby` reflects `--standby` (RFC 0019 §7): agentctl routes a
        // directed assignment only to instances reporting `standby:true`. The
        // `claim` key is added below only in a `cluster` build
        // (capability-absence-not-error, RFC 0015 §2.5 — omitted, not false).
        "cluster": cfg!(feature = "cluster"),
        "shard": cfg.shard.label().map_or(Value::Null, Value::String),
        "standby": cfg.standby,
    });
    // RFC 0019 §9 / RFC 0015 §5.6: a `cluster` build that has wired the claim
    // path advertises the styles it speaks; a build without the feature OMITS the
    // key entirely (agentctl places a `claim` route only on instances advertising
    // it). Conditional insert keeps the key absent — never `false`.
    #[cfg(feature = "cluster")]
    if let Some(obj) = s.as_object_mut() {
        obj.insert("claim".into(), json!({ "styles": ["tool", "resource"] }));
    }
    // The events ENVELOPE schema version (RFC 0016 §7), emitted ALONGSIDE
    // surfaces.events when the stream is actually served — exactly like
    // metrics/metrics_schema. Sourced from the owning obs module (never hardcoded);
    // OMITTED when events is unserved (capability-absence-not-error, RFC 0015 §2.5).
    if events_served && let Some(obj) = s.as_object_mut() {
        obj.insert(
            "events_schema".into(),
            json!(crate::obs::log::EVENTS_SCHEMA),
        );
    }
    s
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
            "agent_version",
            "build_features",
            "identity",
            "mode",
            "model",
            "intelligence",
            "intelligence_summary",
            "mcp_servers",
            "allow_trifecta",
            "limits",
            "surfaces",
        ] {
            assert!(m.get(key).is_some(), "manifest missing top-level key {key}");
        }
        assert_eq!(m["contract_version"], json!("2.0"));
        // De-branded (ACC SPEC L4): only the neutral `agent_version` is emitted;
        // the legacy `agentd_version` key is gone (the root anyOf is satisfied by
        // `agent_version` alone).
        assert_eq!(m["agent_version"], json!(crate::VERSION));
        assert!(
            m.get("agentd_version").is_none(),
            "legacy agentd_version dropped"
        );
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
                "serve-mcp" => cfg!(feature = "serve-mcp"),
                "serve-https" => cfg!(feature = "serve-https"),
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
    fn one_shot_intelligence_discovery_is_network_free_baseline() {
        // RFC 0018 §5.4 + RFC 0015 §5.2: the one-shot manifest builder is
        // network-free. `discovery` is `false` (no probe ran) and `models` is the
        // configured model only — the served read overlays the real probe.
        let cfg = cfg_with(
            &[
                ("INSTRUCTION", "x"),
                ("AGENTD_INTELLIGENCE", "https://api.example/v1"),
            ],
            &["--model", "claude-opus-4"],
        );
        let id = Identity::from_env(&cfg.run_id);
        let m = manifest(&cfg, &id, false);
        assert_eq!(m["intelligence"]["discovery"], json!(false));
        assert_eq!(m["intelligence"]["models"], json!(["claude-opus-4"]));
    }

    #[test]
    fn intelligence_models_is_empty_when_no_model_configured() {
        // No `--model`: the network-free baseline `models` is `[]` (RFC 0018 §5.4
        // — `[]` if none discovered AND no configured model).
        let cfg = base();
        let id = Identity::from_env(&cfg.run_id);
        let m = manifest(&cfg, &id, false);
        assert_eq!(m["intelligence"]["models"], json!([]));
    }

    #[test]
    fn discovery_overlay_replaces_the_baseline_on_the_served_read() {
        // The supervisor overlays the probed discovery onto the manifest's
        // `intelligence` block (the LIVE served path). The additive keys take the
        // probed values; the structural keys are untouched.
        let cfg = base();
        let id = Identity::from_env(&cfg.run_id);
        let mut m = manifest(&cfg, &id, true);
        intelligence_discovery_overlay(
            &mut m,
            true,
            &["claude-opus-4".to_string(), "claude-haiku-4".to_string()],
        );
        assert_eq!(m["intelligence"]["discovery"], json!(true));
        assert_eq!(
            m["intelligence"]["models"],
            json!(["claude-opus-4", "claude-haiku-4"])
        );
        // Structural fields stay intact under the overlay.
        assert_eq!(m["intelligence"]["transport"], json!("https"));
        assert_eq!(m["intelligence"]["endpoints"], json!(1));
    }

    #[test]
    fn multi_endpoint_list_reports_count_and_primary_transport() {
        // RFC 0018 §3.1: a comma-list parses to N endpoints; the manifest
        // `endpoints` is the count, `transport` is the PRIMARY's scheme.
        let cfg = cfg_with(
            &[
                ("INSTRUCTION", "x"),
                (
                    "AGENTD_INTELLIGENCE",
                    "https://gw-a.example,https://gw-b.example,https://gw-c.example",
                ),
            ],
            &[],
        );
        let id = Identity::from_env(&cfg.run_id);
        let m = manifest(&cfg, &id, false);
        assert_eq!(m["intelligence"]["endpoints"], json!(3));
        assert_eq!(m["intelligence"]["transport"], json!("https"));
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
        // the full drain/lame-duck/pause/resume/cancel set with `serve-mcp`,
        // empty without it. pause/resume are PRESENT (RFC 0015 §4.3 — shipped).
        if cfg!(feature = "serve-mcp") {
            assert_eq!(
                s["operator_tools"],
                json!(["drain", "lame-duck", "pause", "resume", "cancel"])
            );
        } else {
            assert_eq!(s["operator_tools"], json!([]));
        }
        // events needs `events` + a management transport (neither configured here).
        assert_eq!(s["events"], json!(false));
        // events_schema is emitted ONLY alongside a served events surface — here it
        // is unserved, so the key is OMITTED (not false/null), like surfaces.claim.
        assert!(
            s.get("events_schema").is_none(),
            "events_schema must be omitted when events is unserved"
        );
        // RFC 0017: config_validate/config_schema are always-available default-build
        // flags (true); hot_reload reflects the `hot-reload` build feature (§5).
        assert_eq!(s["hot_reload"], json!(cfg!(feature = "hot-reload")));
        assert_eq!(s["config_validate"], json!(true));
        assert_eq!(s["config_schema"], json!(true));
        // RFC 0017 §4.2 / §5.6: the agentd://config/effective resource rides the
        // management transport, so it's advertised only with `serve-mcp`.
        assert_eq!(s["config_effective"], json!(cfg!(feature = "serve-mcp")));
        // Frozen contract versions, read from their owning modules (RFC 0016).
        assert_eq!(
            s["metrics_schema"],
            json!(crate::obs::metrics::METRICS_SCHEMA)
        );
        assert_eq!(s["report_schema"], json!(crate::report::REPORT_SCHEMA));
        assert_eq!(s["exit_codes"], json!(crate::exit::EXIT_CODES));
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
    fn surfaces_advertise_cluster_and_shard_per_build(/* RFC 0019 §9 */) {
        // Unsharded base: `standby` is always false; `cluster` mirrors the build;
        // `shard` is null when N==1. The `claim` surface is present iff this is a
        // `cluster` build (RFC 0015 §5.6 / §2.5 capability-absence-not-error).
        let cfg = base();
        let s = &manifest(&cfg, &Identity::from_env(&cfg.run_id), false)["surfaces"];
        assert_eq!(s["standby"], json!(false));
        assert_eq!(s["cluster"], json!(cfg!(feature = "cluster")));
        assert_eq!(s["shard"], Value::Null); // unsharded ⇒ null

        #[cfg(feature = "cluster")]
        {
            // The claim path is wired: advertise the styles agentctl can place.
            assert_eq!(s["claim"], json!({ "styles": ["tool", "resource"] }));

            // With a real shard (N>1 needs the feature), `shard` is the "K/N"
            // string. Gated like surfaces_advertise_a2a_per_build.
            let cfg = cfg_with(
                &[
                    ("INSTRUCTION", "x"),
                    ("AGENTD_INTELLIGENCE", "https://api.example/v1"),
                    ("AGENTD_SHARD", "3/8"),
                ],
                &[],
            );
            let s = &manifest(&cfg, &Identity::from_env(&cfg.run_id), false)["surfaces"];
            assert_eq!(s["cluster"], json!(true));
            assert_eq!(s["shard"], json!("3/8"));
            assert_eq!(s["standby"], json!(false));
            assert_eq!(s["claim"], json!({ "styles": ["tool", "resource"] }));
        }
        // Without the feature the key is OMITTED, never `false`.
        #[cfg(not(feature = "cluster"))]
        assert!(
            s.get("claim").is_none(),
            "claim must be omitted without the cluster feature"
        );
    }

    #[cfg(feature = "events")]
    #[test]
    fn events_schema_is_emitted_alongside_a_served_events_surface() {
        // With the `events` feature + a management transport, the events surface is
        // served, so events_schema (the envelope version) is emitted next to it —
        // sourced from the owning obs module, mirroring metrics/metrics_schema.
        let cfg = cfg_with(
            &[
                ("INSTRUCTION", "x"),
                ("AGENTD_INTELLIGENCE", "https://intel.example"),
                ("AGENTD_SERVE_MCP", "http://127.0.0.1:8443"),
            ],
            &[],
        );
        let id = Identity::from_env(&cfg.run_id);
        let s = &manifest(&cfg, &id, false)["surfaces"];
        assert_eq!(s["events"], json!(true));
        assert_eq!(s["events_schema"], json!(crate::obs::log::EVENTS_SCHEMA));
    }

    #[test]
    fn surfaces_report_configured_addresses() {
        // serve-mcp + metrics-addr configured ⇒ their address strings surface.
        let cfg = cfg_with(
            &[
                ("INSTRUCTION", "x"),
                ("AGENTD_INTELLIGENCE", "https://intel.example"),
                ("AGENTD_SERVE_MCP", "http://127.0.0.1:8443"),
                ("AGENTD_METRICS_ADDR", ":9090"),
            ],
            &[],
        );
        let id = Identity::from_env(&cfg.run_id);
        let s = &manifest(&cfg, &id, false)["surfaces"];
        assert_eq!(s["management"], json!("http://127.0.0.1:8443"));
        assert_eq!(s["metrics"], json!(":9090"));
    }

    #[test]
    fn limits_block_carries_caps_and_config() {
        let cfg = cfg_with(
            &[("INSTRUCTION", "x"), ("AGENTD_INTELLIGENCE", "https://intel.example")],
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
            &[("INSTRUCTION", "x"), ("AGENTD_INTELLIGENCE", "https://intel.example")],
            &[
                "--mcp",
                "vault=https://vault.example/mcp",
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
