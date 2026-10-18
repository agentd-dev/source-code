// SPDX-License-Identifier: AGPL-3.0-only
//! **Instance-tier children**: a template whose instruction defines machinery
//! spawns a FULL DAEMON — the same binary, `-c` a composed config — under the
//! supervisor's reaper, wired to the parent as an A2A peer over a unix socket,
//! retired by `ttl`/`until`/`subagent.retire` through the child's own graceful
//! drain.
//!
//! The composed document is structured data, never prose the child re-parses
//! into configuration. Directive extraction runs once at boot in
//! `config::templates`; params are folded in as data; and a spawn whose folded
//! params reintroduce directive machinery is refused outright. Together those
//! keep the child's own boot-time extraction from finding anything but prose,
//! so a caller cannot smuggle configuration into a child through a param.

use super::reactor::{Runtime, SubagentRecord, is_terminal_status};
use super::tools::ToolOutcome;
use crate::config::templates::{
    CompiledTemplate, Tier, compile_templates, fold_params, fold_params_value,
    params_introduced_machinery, validate_params,
};
use crate::state::now_ms;
use crate::supervisor::reap::Reaped;
use serde_json::{Map, Value, json};
use std::path::PathBuf;

/// Extra grace past the child's own drain window before SIGKILL.
const KILL_GRACE_MS: u64 = 5_000;

/// The peer an instance child reports home over, which its parent composes.
const PARENT_PEER: &str = "parent";

/// Whether the configured peer `name` is this process's parent: its `parent`
/// peer, in an instance child. A child's whole `a2a` section is its parent's
/// composition, so that peer is the parent's own listener.
#[cfg(feature = "a2a")]
pub(crate) fn is_parent_link(name: &str) -> bool {
    name == PARENT_PEER && std::env::var_os(crate::supervisor::reap::INSTANCE_CHILD_ENV).is_some()
}

/// Remove the copy of a key held only in memory composed into the instance
/// directory `dir` ([`crate::aauth::forget_instance_key`]), once the child it
/// was for has exited or was never started.
fn forget_instance_key(dir: &std::path::Path) {
    #[cfg(feature = "aauth")]
    crate::aauth::forget_instance_key(dir);
    #[cfg(not(feature = "aauth"))]
    let _ = dir;
}

impl Runtime {
    /// `subagent.run` with an instance-tier template: spawn a full daemon.
    pub(crate) fn instance_run(
        &mut self,
        caller: &super::tools::ToolCaller,
        tname: &str,
        args: &Value,
    ) -> ToolOutcome {
        let err = |e: String| ToolOutcome::Ready(Value::String(e), true);
        // An instance child is a whole daemon, far heavier than a flat worker,
        // so it is capped under its own `limits.subagents.instances` family
        // rather than sharing the worker budget.
        let live = self
            .subagents
            .values()
            .filter(|s| s.tier.as_deref() == Some("instance") && !is_terminal_status(&s.status))
            .count() as u32;
        let il = &self.settings.limits.subagents.instances;
        let breadth = il.breadth.unwrap_or(2);
        if live >= breadth {
            crate::obs::metrics::record_refusal("breadth");
            return err(format!(
                "subagent.run refused: {live} instance children live (limits.subagents.instances.breadth = {breadth})"
            ));
        }
        let total = il.total.unwrap_or(8) as usize;
        let lifetime = self
            .subagents
            .values()
            .filter(|s| s.tier.as_deref() == Some("instance"))
            .count();
        if lifetime >= total {
            crate::obs::metrics::record_refusal("total");
            return err(format!(
                "subagent.run refused: {lifetime} instance children spawned (limits.subagents.instances.total = {total})"
            ));
        }
        if !self.instance_bucket_take() {
            crate::obs::metrics::record_refusal("rate");
            return err(
                "subagent.run refused: instance spawn rate exceeded (limits.subagents.instances.rate)"
                    .into(),
            );
        }
        if self.pressure.shedding() {
            return err(format!(
                "subagent.run refused: {} pressure (shedding new work; in-flight work drains)",
                self.pressure.cause()
            ));
        }
        let compiled = match compile_templates(&self.settings) {
            Ok(c) => c,
            Err(es) => return err(format!("subagent.run: template compile: {}", es.join("; "))),
        };
        let Some(t) = compiled.get(tname) else {
            return err(format!("subagent.run: no template '{tname}'"));
        };
        debug_assert_eq!(t.tier, Tier::Instance);
        if t.spec.singleton
            && let Some(s) = self
                .subagents
                .values()
                .find(|s| s.template.as_deref() == Some(tname) && !is_terminal_status(&s.status))
        {
            return err(format!(
                "subagent.run refused: template '{tname}' is a singleton and '{}' is live (retire it first)",
                s.handle
            ));
        }
        let params =
            match validate_params(&t.spec.params, args.get("params").unwrap_or(&Value::Null)) {
                Ok(p) => p,
                Err(e) => return err(format!("subagent.run: template '{tname}': {e}")),
            };
        let prose = fold_params(&t.cleaned, &params);
        if params_introduced_machinery(&t.cleaned, &prose) {
            return err(format!(
                "subagent.run refused: params for template '{tname}' introduced directive machinery"
            ));
        }
        // A child reports home over its `parent` peer; one that cannot be
        // named there would run its work and then have every `_instance.*`
        // report refused, leaving a sync spawn parked until its timeout.
        // Refused here instead, before anything is created.
        let parent_peer = self.parent_peer_entry();
        if let Some(why) = self.parent_unreachable(parent_peer.as_ref()) {
            self.log.warn(
                "instance.spawn.refused",
                json!({"template": tname, "reason": why}),
            );
            return err(format!("subagent.run refused: {why}"));
        }
        // Durability class (call site > template > store.durability.work):
        // non-durable ⇒ memory-only record, child on a memory store in this
        // process's private directory, no restore-respawn — the throwaway
        // war-room shape. A parent whose store does not outlive it has only
        // non-durable children: nothing would respawn a durable one, or read
        // its state, again.
        let root = durable_instances_root(&self.settings, &self.instance);
        let durable = root.is_some()
            && args
                .get("durable")
                .and_then(Value::as_bool)
                .or(t.spec.durable)
                .unwrap_or_else(|| self.work_durable_default());
        let handle = self.next_id("inst");
        let dir = match self.instance_dir(&handle, root.filter(|_| durable)) {
            Ok(d) => d,
            Err(e) => return err(format!("subagent.run: {e}")),
        };
        let socket = match instance_socket_path(&dir, &std::env::temp_dir()) {
            Ok(s) => s,
            Err(why) => {
                self.log.warn(
                    "instance.spawn.refused",
                    json!({"template": tname, "reason": why}),
                );
                return err(format!("subagent.run refused: {why}"));
            }
        };
        if let Err(e) = std::fs::create_dir_all(&dir) {
            return err(format!("subagent.run: create {}: {e}", dir.display()));
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700));
        }
        let until = t
            .spec
            .until
            .as_ref()
            .map(|u| fold_params(u, &params))
            .filter(|u| !u.trim().is_empty());
        // `mode: sync` resolves the spawn when the child's declared result
        // workflow first completes — the composed reporter dials home.
        // `detached` returns as soon as the daemon is up.
        let mode = args
            .get("mode")
            .and_then(Value::as_str)
            .or(t.spec.mode.as_deref())
            .unwrap_or("detached")
            .to_string();
        match mode.as_str() {
            "detached" => {}
            "sync" if t.spec.result.is_some() => {}
            "sync" => {
                return err(format!(
                    "subagent.run: template '{tname}' has no `result: {{workflow}}` — mode: sync needs one"
                ));
            }
            other => {
                return err(format!(
                    "subagent.run: instance children support mode detached|sync (got '{other}')"
                ));
            }
        }
        // The child daemon is a span of the unit that spawned it: its process
        // trace continues this one, and so does the work it starts itself.
        let trace = self
            .caller_trace(caller)
            .unwrap_or_else(|| self.trace.clone());
        let doc = match self.compose_instance_doc(
            t,
            &prose,
            &params,
            &dir,
            &socket,
            until.as_deref(),
            durable,
            &handle,
            parent_peer,
            &trace,
        ) {
            Ok(d) => d,
            Err(e) => {
                self.release_instance_dir(&dir, Some(&socket), false);
                return err(format!("subagent.run: template '{tname}': {e}"));
            }
        };
        // The composed document must be a bootable config NOW — a child that
        // exits 2 on its first breath is a refusal we can make synchronously.
        if let Err(e) = validate_composed(&doc, &prose) {
            self.release_instance_dir(&dir, Some(&socket), false);
            return err(format!(
                "subagent.run: template '{tname}' composes an invalid config: {e}"
            ));
        }
        let config_path = dir.join("config.json");
        let rendered = serde_json::to_string_pretty(&doc).unwrap_or_default();
        if let Err(e) = std::fs::write(&config_path, rendered) {
            self.release_instance_dir(&dir, Some(&socket), false);
            return err(format!(
                "subagent.run: write {}: {e}",
                config_path.display()
            ));
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&config_path, std::fs::Permissions::from_mode(0o600));
        }
        let retire_at = t.spec.ttl.map(|d| now_ms() + d.0.as_millis() as u64);
        let mut record = SubagentRecord {
            handle: handle.clone(),
            instruction: prose.clone(),
            mode: mode.clone(),
            status: "spawned".into(),
            attempt: 1,
            result: None,
            error: None,
            requested_by: Some(
                json!({"caller": caller.node.map(|n| n.0), "ctx": caller.ctx, "run": caller.run, "step": caller.step, "subagent": caller.subagent, "depth": 0}),
            ),
            principal: caller.principal.clone(),
            // What the caller carries, the child is handed (`withhold`).
            carries: self.caller_carries(caller),
            tokens: 0,
            created: now_ms(),
            updated: now_ms(),
            payload: None,
            trace: Some(trace.clone()),
            template: Some(tname.to_string()),
            tier: Some("instance".into()),
            pid: None,
            config_path: Some(config_path.display().to_string()),
            socket: Some(socket.clone()),
            retire_at,
            retiring_since: None,
            durable,
            node: None,
            killed: false,
            dirty: true,
        };
        record.principal = self.spawn_principal(&record);
        match self.spawn_instance_process(&config_path, &dir, &t.spec.limits, &prose) {
            Ok(pid) => {
                record.pid = Some(pid);
                record.status = "running".into();
                self.log.in_trace(Some(&trace)).info(
                    "instance.spawn",
                    json!({"handle": handle, "template": tname, "pid": pid,
                           "socket": socket, "retire_at": retire_at, "until": until,
                           "singleton": t.spec.singleton}),
                );
                self.subagents.insert(handle.clone(), record);
                self.persist_subagent(&handle);
                if mode == "sync" {
                    ToolOutcome::Deferred(super::reactor::PendingKind::Subagent { handle })
                } else {
                    ToolOutcome::Ready(
                        json!({"handle": handle, "status": "running", "tier": "instance",
                               "peer": handle, "socket": socket}),
                        false,
                    )
                }
            }
            Err(e) => {
                self.release_instance_dir(&dir, Some(&socket), false);
                record.status = "failed".into();
                record.error = Some(format!("spawn: {e}"));
                self.subagents.insert(handle.clone(), record);
                self.persist_subagent(&handle);
                err(format!("subagent.run: instance spawn failed: {e}"))
            }
        }
    }

    /// Compose the child's settings document from four sources: the template's
    /// frozen machinery, the folded prose, the parent's inherited intelligence
    /// and service catalog, and the parent-owned store, listener and lifecycle.
    /// Structured data end to end — the child never re-parses prose as config.
    #[allow(clippy::too_many_arguments)]
    fn compose_instance_doc(
        &self,
        t: &CompiledTemplate,
        prose: &str,
        params: &Map<String, Value>,
        dir: &std::path::Path,
        socket: &str,
        until: Option<&str>,
        durable: bool,
        handle: &str,
        parent_peer: Option<Value>,
        trace: &crate::obs::trace::TraceContext,
    ) -> Result<Value, String> {
        let mut doc = t.fragment.clone();
        if !doc.is_object() {
            doc = json!({});
        }
        fold_params_value(&mut doc, params);
        let agent = doc
            .as_object_mut()
            .expect("object")
            .entry("agent")
            .or_insert_with(|| json!({}));
        agent["name"] = json!(format!("{}/{}", self.instance, t.name));
        agent["instruction"] = json!(prose);
        // The channels the template's `::!human`s put on its gates: the prose
        // the child reads no longer declares them, so they travel as the
        // operator's (`agent.gate_channels`), from the operator's template.
        // A workflow's name, and a channel, may name a param: folded as the
        // workflows are, so each still names its gate.
        if !t.gate_channels.is_empty() {
            let mut channels = json!(
                t.gate_channels
                    .iter()
                    .map(|(wf, steps)| (fold_params(wf, params), steps))
                    .collect::<std::collections::BTreeMap<_, _>>()
            );
            fold_params_value(&mut channels, params);
            agent["gate_channels"] = channels;
        }
        // Machinery workflows join whatever a `:::!config` block declared.
        let mut wfs = t.workflows.clone();
        for w in &mut wfs {
            fold_params_value(w, params);
        }
        let list = doc
            .as_object_mut()
            .expect("object")
            .entry("workflows")
            .or_insert_with(|| json!([]));
        if let Some(a) = list.as_array_mut() {
            a.extend(wfs);
            // `mode: sync` needs the child to tell the parent when it is done,
            // so compose a REPORTER workflow out of ordinary nodes — event
            // start → switch pick → workflow.wait → typed a2a.send. It dials
            // the parent's `_instance.result` op when the declared workflow
            // first completes. Building it from existing node kinds means the
            // reporting path has no privileged machinery of its own; the op
            // itself is control plane and never reaches a model.
            if let Some(rw) = t
                .spec
                .result
                .as_ref()
                .and_then(|r| r.get("workflow"))
                .and_then(Value::as_str)
            {
                a.push(json!({
                    "name": crate::config::templates::REPORTER_WORKFLOW, "steps": {
                        "ev":   {"kind": "event", "on": "workflow.finished"},
                        "pick": {"kind": "switch", "depends_on": ["ev"],
                                 "on": "{{steps.ev.output.payload.workflow}}",
                                 "cases": {rw: "get"}, "on_no_match": "skip"},
                        "get":  {"kind": "workflow.wait", "depends_on": ["pick"],
                                 "run": "{{steps.ev.output.payload.run}}", "timeout": "30s"},
                        "send": {"kind": "a2a.send", "depends_on": ["get"], "to": PARENT_PEER,
                                 "command": "_instance.result",
                                 "args": {"handle": handle,
                                          "status": "{{steps.get.output.status}}",
                                          "output": "{{steps.get.output.output}}"},
                                 "timeout": "30s", "retry": {"max": 5, "backoff": "2s"}},
                        "f":    {"kind": "finish", "depends_on": ["send"], "status": "completed"}
                    }
                }));
            }
            // `mirror_streams`: one composed forwarder per stream — every child
            // event rides the socket into the parent's same-named stream,
            // appended there with source `instance:<handle>` so the parent can
            // tell a mirrored event from one of its own.
            for m in t.spec.mirror_streams.iter().flatten() {
                a.push(json!({
                    "name": crate::config::templates::mirror_workflow(m), "steps": {
                        "ev":   {"kind": "stream", "stream": m, "from": "new"},
                        "send": {"kind": "a2a.send", "depends_on": ["ev"], "to": PARENT_PEER,
                                 "command": "_instance.emit",
                                 "args": {"handle": handle, "stream": m,
                                          "event": "{{steps.ev.output}}"},
                                 "timeout": "30s", "retry": {"max": 5, "backoff": "2s"}},
                        "f":    {"kind": "finish", "depends_on": ["send"], "status": "completed"}
                    }
                }));
            }
        }
        // Parent-owned composition: store, listener, lifecycle, security.
        doc["store"] = if durable {
            json!({"kind": "file", "file": {"path": dir.join("state").display().to_string()}})
        } else {
            json!({"kind": "memory"})
        };
        let mut a2a = json!({"listen": format!("unix://{socket}")});
        if let Some(peer) = parent_peer {
            a2a["peers"] = json!([peer]);
        }
        doc["a2a"] = a2a;
        let mut lifecycle = json!({});
        if let Some(u) = until {
            lifecycle["until_signal"] = json!(u);
        }
        doc["lifecycle"] = lifecycle;
        // The spawning unit's span, which the child's process trace continues
        // (`observability.traceparent` is operator-only, so no template wrote
        // one). Its environment is scrubbed, so this is the one way across.
        let observability = doc
            .as_object_mut()
            .expect("object")
            .entry("observability")
            .or_insert_with(|| json!({}));
        if let Some(o) = observability.as_object_mut() {
            o.insert("traceparent".into(), json!(trace.traceparent()));
        }
        // The child inherits the parent's service catalog, egress posture and
        // trifecta override. A template cannot set any of these — they are
        // `OPERATOR_ONLY`, so `document_wrote_operator_config` refuses the
        // fragment at compile — because a child must never be able to grant
        // itself a wider trust budget than the parent that spawned it.
        //
        // Assigned UNCONDITIONALLY. A parent with no catalogue of its own must
        // hand the child an empty one rather than leave in place whatever the
        // template wrote: composition must not depend on a check somewhere
        // else still holding.
        doc["services"] = without_companies(
            self.settings_doc
                .get("services")
                .cloned()
                .unwrap_or_else(|| json!({})),
        );
        let mut security = json!({});
        if self.settings.security.allow_trifecta {
            security["allow_trifecta"] = json!(true);
        }
        if self.settings.security.egress == crate::config::settings::Egress::Closed {
            security["egress"] = json!("closed");
        }
        if let Some(ca) = &self.settings.security.tls_ca {
            security["tls_ca"] = json!(ca);
        }
        // And it signs as this agent: one identity for the tree.
        if let Some(aauth) = self.instance_identity(dir)? {
            security["aauth"] = aauth;
        }
        if security.as_object().is_some_and(|o| !o.is_empty()) {
            doc["security"] = security;
        }
        // MCP Events bindings and grants are the parent operator's alone
        // (RFC 0045 §5.11.6): a child subscribing would make subscriptions
        // the model's to create by spawning children, and a mirror would
        // start one run per child for each occurrence. The template is
        // refused one at compile; whatever composed the servers, the child
        // gets none.
        if let Some(servers) = doc
            .get_mut("mcp")
            .and_then(|m| m.get_mut("servers"))
            .and_then(Value::as_array_mut)
        {
            for srv in servers.iter_mut().filter_map(Value::as_object_mut) {
                srv.remove("events");
                srv.remove("event_grants");
            }
        }
        // Intelligence: the parent's section — its providers, models and
        // roles with it — minus its budget (the child's budget is the
        // template's grant), minus its routes (the parent's per-turn policy;
        // a child's turn model is its template's choice, and a template that
        // turns preflight off would compose a child its routes refuse), and
        // minus any INLINE token, the top-level one's or a provider's (the
        // env passthrough carries it; a secret REFERENCE rides along fine).
        let mut intel = self
            .settings_doc
            .get("intelligence")
            .cloned()
            .unwrap_or(json!({}));
        if let Some(o) = intel.as_object_mut() {
            o.remove("budget");
            o.remove("routes");
            drop_inline_token(o);
            if let Some(providers) = o.get_mut("providers").and_then(Value::as_object_mut) {
                for p in providers.values_mut().filter_map(Value::as_object_mut) {
                    drop_inline_token(p);
                }
            }
        }
        // The model the template's variants were folded for
        // (`templates::template_facts`): its own, else the spawn default.
        // It is the child's turn model whatever the parent's `roles.turn`
        // says — and said once, as `model`.
        if let Some(m) = t
            .spec
            .model
            .as_deref()
            .or(self.settings.subagents.defaults.model.as_deref())
            .filter(|m| !m.trim().is_empty())
            && let Some(o) = intel.as_object_mut()
        {
            o.insert("model".into(), json!(m));
            if let Some(roles) = o.get_mut("roles").and_then(Value::as_object_mut) {
                roles.remove("turn");
            }
        }
        if let Some(b) = &t.spec.budget {
            intel["budget"] = b.clone();
        }
        doc["intelligence"] = intel;
        Ok(doc)
    }

    /// The `security.aauth` a child in `dir` is composed: this process's AAuth
    /// identity (`aauth::instance_settings`), or none.
    fn instance_identity(&self, dir: &std::path::Path) -> Result<Option<Value>, String> {
        #[cfg(feature = "aauth")]
        {
            let enroll_token = self
                .settings
                .security
                .aauth
                .as_ref()
                .and_then(|a| a.enroll_token.as_ref())
                .map(|s| s.0.as_str());
            crate::aauth::instance_settings(dir, enroll_token)
        }
        #[cfg(not(feature = "aauth"))]
        {
            let _ = dir;
            Ok(None)
        }
    }

    /// A restored child's composed config, given this life's AAuth identity
    /// in place of the one its first life composed: the operator's
    /// `security.aauth` may have changed across the restart, and a child
    /// signs as the agent that is running.
    fn recompose_identity(&self, config: &std::path::Path) -> Result<(), String> {
        let dir = config.parent().unwrap_or(std::path::Path::new("."));
        let mut doc: Value = std::fs::read(config)
            .map_err(|e| e.to_string())
            .and_then(|b| serde_json::from_slice(&b).map_err(|e| e.to_string()))
            .map_err(|e| format!("read {}: {e}", config.display()))?;
        let identity = self.instance_identity(dir)?;
        let Some(root) = doc.as_object_mut() else {
            return Err(format!("{} is not a composed config", config.display()));
        };
        let security = root.entry("security").or_insert_with(|| json!({}));
        match (identity, security.as_object_mut()) {
            (Some(aauth), Some(s)) => {
                s.insert("aauth".into(), aauth);
            }
            (None, Some(s)) => {
                s.remove("aauth");
            }
            (_, None) => return Err(format!("{}: `security` is not a map", config.display())),
        }
        if root
            .get("security")
            .and_then(Value::as_object)
            .is_some_and(|s| s.is_empty())
        {
            root.remove("security");
        }
        let rendered = serde_json::to_string_pretty(&doc).unwrap_or_default();
        std::fs::write(config, rendered).map_err(|e| format!("write {}: {e}", config.display()))
    }

    /// The child's `parent` peer entry — only when the parent has a listener,
    /// and never carrying an inline credential into a file on disk.
    fn parent_peer_entry(&self) -> Option<Value> {
        let listen = self.settings.a2a.listen.as_deref()?;
        let endpoint = if let Some(rest) = listen
            .strip_prefix("unix://")
            .or_else(|| listen.strip_prefix("unix:"))
        {
            format!("unix://{rest}")
        } else {
            // A bind authority (possibly 0.0.0.0) → a dialable loopback URL.
            listen
                .replace("0.0.0.0", "127.0.0.1")
                .replace("[::]", "[::1]")
        };
        let mut peer = json!({"name": PARENT_PEER, "endpoint": endpoint});
        if let Some(bearer) = &self.settings.a2a.bearer {
            if crate::sec::secret::has_secret_ref(&bearer.0) {
                peer["headers"] = json!({"Authorization": format!("Bearer {}", bearer.0)});
            } else {
                // An inline bearer must not be written to the child's config
                // file; over a unix listener SO_PEERCRED covers same-uid
                // children anyway.
                self.log.warn(
                    "instance.parent_peer",
                    json!({"note": "a2a.bearer is inline (not a {{secret:…}} reference); the child gets no credential for its `parent` peer"}),
                );
            }
        }
        Some(peer)
    }

    /// Why a child could not authenticate to this parent, if it could not.
    ///
    /// Every report a child sends home is an `_instance.*` op, which only the
    /// operator may send. Over a unix listener the kernel names the child's
    /// uid, and a loopback listener with no auth at all makes any caller the
    /// operator; anywhere else the child is the operator only by presenting
    /// the operator's bearer, and [`Self::parent_peer_entry`] writes one only
    /// when it is a `{{secret:…}}` reference. The posture is read from the
    /// settings in force NOW, so a reload that added a principal — ending the
    /// implicit operator — is seen by the next spawn.
    fn parent_unreachable(&self, peer: Option<&Value>) -> Option<&'static str> {
        let auth = crate::runtime::surface::auth::listener_auth_of(&self.settings.a2a);
        let tcp = self.settings.a2a.listen.is_some() && !auth.unix;
        let authorized = peer.is_some_and(|p| p["headers"].get("Authorization").is_some());
        (tcp && !auth.implicit_operator && !authorized).then_some(
            "the child cannot authenticate to its parent: make a2a.bearer a {{secret:…}} reference or use a unix listener",
        )
    }

    /// Spawn the child daemon ([`instance_command`], logging to `<dir>/log`)
    /// under the reaper. `delivered` is the instruction prose the composed
    /// config carries, already delivered here.
    fn spawn_instance_process(
        &mut self,
        config_path: &std::path::Path,
        dir: &std::path::Path,
        limits: &Option<Value>,
        delivered: &str,
    ) -> Result<i32, String> {
        let exe = std::env::current_exe().map_err(|e| format!("current_exe: {e}"))?;
        let log_file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(dir.join("log"))
            .map_err(|e| format!("open log: {e}"))?;
        let mut cmd = instance_command(&exe, config_path, log_file, limits, delivered)?;
        let child =
            crate::supervisor::reaper::spawn_tracked_pid(&self.children.reap_sender(), || {
                cmd.spawn()
            })
            .map_err(|e| e.to_string())?;
        let pid = child.id() as i32;
        // The reaper owns the exit; the std handle would only race it.
        std::mem::forget(child);
        Ok(pid)
    }

    /// The per-tick maintenance pass: `ttl` retirement and the SIGTERM→SIGKILL
    /// escalation for children that ignored the drain.
    pub(crate) fn instances_tick(&mut self) {
        self.meter_instances();
        let now = now_ms();
        let drain_ms = self.settings.lifecycle.drain_timeout().as_millis() as u64;
        let mut to_persist = Vec::new();
        let mut retire = Vec::new();
        let mut kill = Vec::new();
        for s in self.subagents.values() {
            if s.tier.as_deref() != Some("instance") || is_terminal_status(&s.status) {
                continue;
            }
            match s.retiring_since {
                None => {
                    if s.retire_at.is_some_and(|at| now >= at) {
                        retire.push((s.handle.clone(), "ttl"));
                    }
                }
                Some(since) if now >= since + drain_ms + KILL_GRACE_MS => {
                    kill.push(s.handle.clone());
                }
                Some(_) => {}
            }
        }
        for (h, why) in retire {
            self.retire_instance(&h, why);
        }
        for h in kill {
            if let Some(s) = self.subagents.get_mut(&h)
                && let Some(pid) = s.pid
            {
                self.subagent_log(&h).warn(
                    "instance.kill",
                    json!({"handle": h, "pid": pid, "note": "drain window elapsed"}),
                );
                #[cfg(unix)]
                unsafe {
                    libc::killpg(pid, libc::SIGKILL);
                }
                if let Some(s) = self.subagents.get_mut(&h) {
                    s.retiring_since = Some(now); // reset the clock; the reap closes it
                    to_persist.push(h.clone());
                }
            }
        }
        for h in to_persist {
            self.persist_subagent(&h);
        }
    }

    /// Begin graceful retirement: SIGTERM to the child's process group — the
    /// child daemon drains its own runs and exits; the reap closes the record.
    pub(crate) fn retire_instance(&mut self, handle: &str, why: &str) -> bool {
        let Some(s) = self.subagents.get_mut(handle) else {
            return false;
        };
        if s.tier.as_deref() != Some("instance") || is_terminal_status(&s.status) {
            return false;
        }
        let Some(pid) = s.pid else {
            return false;
        };
        if s.retiring_since.is_none() {
            s.retiring_since = Some(now_ms());
            s.status = "retiring".into();
            s.dirty = true;
            self.subagent_log(handle).info(
                "instance.retire",
                json!({"handle": handle, "pid": pid, "why": why}),
            );
            #[cfg(unix)]
            unsafe {
                libc::killpg(pid, libc::SIGTERM);
            }
            self.persist_subagent(handle);
        }
        true
    }

    /// A reaped pid that is not a control-channel child: an instance child
    /// exited. Close the record; the pending-poll resolves any `subagent.await`.
    pub(crate) fn on_instance_reaped(&mut self, r: &Reaped) -> bool {
        let Some(handle) = self
            .subagents
            .values()
            .find(|s| s.pid == Some(r.pid) && !is_terminal_status(&s.status))
            .map(|s| s.handle.clone())
        else {
            return false;
        };
        // One the drain asked to drain itself is left for the next life.
        if self.park_drained_instance(&handle, r) {
            self.release_reaped_instance(&handle, false);
            self.persist_subagent(&handle);
            return true;
        }
        let (status, error) = match (&r.outcome, self.subagents[&handle].retiring_since) {
            (_, Some(_)) => ("retired", None),
            (crate::supervisor::reap::WaitOutcome::Exited(0), None) => ("completed", None),
            (crate::supervisor::reap::WaitOutcome::Exited(c), None) => {
                ("failed", Some(format!("exit {c}")))
            }
            (crate::supervisor::reap::WaitOutcome::Signaled(sig), None) => {
                ("crashed", Some(format!("signal {sig}")))
            }
        };
        let failed = error.is_some();
        let cause = self
            .instance_dir_of(&handle)
            .filter(|_| failed)
            .and_then(|d| exit_cause(&d.join("log")));
        self.release_reaped_instance(&handle, failed);
        if let Some(s) = self.subagents.get_mut(&handle) {
            s.status = status.into();
            s.error = error;
            s.result =
                Some(json!({"tier": "instance", "socket": s.socket, "config": s.config_path}));
            s.updated = now_ms();
            s.dirty = true;
        }
        // A child that failed said why in its own log; the parent's says it
        // too, where it outlives the child's directory.
        let mut exited = json!({"handle": handle, "pid": r.pid, "status": status});
        if let Some(cause) = cause {
            exited["cause"] = Value::String(cause);
        }
        self.subagent_log(&handle).info("instance.exited", exited);
        self.persist_subagent(&handle);
        true
    }

    /// The directory the child `handle` was composed in, from its record.
    fn instance_dir_of(&self, handle: &str) -> Option<PathBuf> {
        let config = self.subagents.get(handle)?.config_path.as_deref()?;
        std::path::Path::new(config).parent().map(PathBuf::from)
    }

    /// [`Runtime::release_instance_dir`] for the reaped child `handle`: the
    /// child has read its key, and a respawn is composed it again. A
    /// non-durable child's directory goes with it, unless it `failed`: its log
    /// says why, so the directory stays until this process exits.
    fn release_reaped_instance(&self, handle: &str, failed: bool) {
        if let Some(dir) = self.instance_dir_of(handle) {
            let socket = self.subagents.get(handle).and_then(|s| s.socket.as_deref());
            self.release_instance_dir(&dir, socket, failed);
        }
    }

    /// Restore-time respawn: live instance records whose composed config still
    /// exists come back. PDEATHSIG takes a child down with its parent, so no
    /// process from the previous life is still running; the record's handle is
    /// the child's identity and its state is durable, so the respawned daemon
    /// picks up where the dead one stopped.
    pub(crate) fn respawn_restored_instances(&mut self) {
        let candidates: Vec<(String, String, Option<Value>)> = self
            .subagents
            .values()
            .filter(|s| s.tier.as_deref() == Some("instance") && !is_terminal_status(&s.status))
            .filter_map(|s| s.config_path.clone().map(|c| (s.handle.clone(), c, None)))
            .collect();
        for (handle, config, limits) in candidates {
            let path = PathBuf::from(&config);
            let Some(dir) = path.parent().map(|p| p.to_path_buf()) else {
                continue;
            };
            // The respawned child is told it was delivered exactly as the
            // first one was, from the composed config it is about to read —
            // the one copy of that prose the child's digest check compares,
            // so no other record of it can drift from what the child sees.
            let Some(delivered) = delivered_prose_of(&path) else {
                if let Some(s) = self.subagents.get_mut(&handle) {
                    s.status = "failed".into();
                    s.error = Some("config lost across restart".into());
                }
                self.persist_subagent(&handle);
                continue;
            };
            // It signs as this life's agent.
            if let Err(e) = self.recompose_identity(&path) {
                forget_instance_key(&dir);
                if let Some(s) = self.subagents.get_mut(&handle) {
                    s.status = "failed".into();
                    s.error = Some(format!("respawn: {e}"));
                }
                self.persist_subagent(&handle);
                continue;
            }
            match self.spawn_instance_process(&path, &dir, &limits, &delivered) {
                Ok(pid) => {
                    if let Some(s) = self.subagents.get_mut(&handle) {
                        s.pid = Some(pid);
                        s.status = "running".into();
                        s.attempt += 1;
                        s.retiring_since = None;
                    }
                    crate::obs::metrics::record_subagent_restart("restored");
                    self.subagent_log(&handle)
                        .info("instance.respawn", json!({"handle": handle, "pid": pid}));
                    self.persist_subagent(&handle);
                }
                Err(e) => {
                    forget_instance_key(&dir);
                    if let Some(s) = self.subagents.get_mut(&handle) {
                        s.status = "failed".into();
                        s.error = Some(format!("respawn: {e}"));
                    }
                    self.persist_subagent(&handle);
                }
            }
        }
    }

    /// `subagent.send` to an instance child: the message rides A2A over the
    /// child's unix socket and lands in its conversation surface.
    /// Fire-and-forget from the reactor's view (a thread does the dial). The
    /// send is a span of `trace`, the sending unit's — else of the span the
    /// child was spawned in — so the turn it becomes in the child continues
    /// the sender's trace.
    pub(crate) fn instance_send(
        &mut self,
        handle: &str,
        message: &str,
        trace: Option<&crate::obs::trace::TraceContext>,
    ) -> ToolOutcome {
        let err = |e: String| ToolOutcome::Ready(Value::String(e), true);
        let Some(s) = self.subagents.get(handle) else {
            return err(format!("no such subagent {handle:?}"));
        };
        if is_terminal_status(&s.status) {
            return err(format!("instance {handle:?} is not running ({})", s.status));
        }
        let Some(sock) = s.socket.clone() else {
            return err(format!("instance {handle:?} has no A2A socket"));
        };
        #[cfg(feature = "a2a")]
        {
            let endpoint = match crate::config::A2aEndpoint::parse(&format!("unix://{sock}")) {
                Ok(e) => e,
                Err(e) => return err(format!("instance {handle:?}: {e}")),
            };
            let parts = json!([{"text": message}]);
            let hop = trace
                .cloned()
                .or_else(|| s.trace.clone())
                .map(|t| t.child());
            let log = self.log.in_trace(hop.as_ref());
            let auth = crate::mcp::a2a_client::PeerAuth {
                trace: hop.clone(),
                tree: true,
                ..Default::default()
            };
            let h = handle.to_string();
            std::thread::Builder::new()
                .name(format!("inst.send:{handle}"))
                .spawn(move || {
                    let start = crate::obs::otel::now_unix_nanos();
                    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
                    let sent =
                        crate::mcp::a2a_client::send(&endpoint, auth, &parts, None, None, deadline);
                    // The send is the CLIENT span the child's task continues.
                    crate::obs::otel::span(
                        hop.as_ref(),
                        &format!("instance.send {h}"),
                        crate::obs::otel::Kind::Client,
                        start,
                    )
                    .attr("rpc.system", "jsonrpc")
                    .attr("rpc.method", "SendMessage")
                    .attr("agentd.instance.handle", h.as_str())
                    .end(sent.is_ok());
                    match sent {
                        Ok(_) => log.info("instance.send", json!({"handle": h})),
                        Err(e) => log.warn("instance.send", json!({"handle": h, "err": e})),
                    }
                })
                .ok();
            ToolOutcome::Ready(
                json!({"ok": true, "handle": handle, "delivery": "async"}),
                false,
            )
        }
        #[cfg(not(feature = "a2a"))]
        {
            let _ = (message, &sock, trace);
            err(format!(
                "instance {handle:?}: subagent.send to an instance child needs --features a2a"
            ))
        }
    }

    /// The A2A peer view of live instance children: the handle is the peer
    /// name, and a `singleton: true` template's name is an alias for its one
    /// live child. Consulted by `a2a_peer_conn` only after the configured
    /// peers, so a configured peer name always wins over a child's.
    #[cfg(feature = "a2a")]
    pub(crate) fn instance_peer_endpoint(&self, name: &str) -> Option<String> {
        self.subagents
            .values()
            .filter(|s| s.tier.as_deref() == Some("instance") && !is_terminal_status(&s.status))
            .find(|s| {
                s.handle == name
                    || (s.template.as_deref() == Some(name)
                        && self
                            .settings
                            .subagents
                            .templates
                            .get(name)
                            .is_some_and(|t| t.singleton))
            })
            .and_then(|s| s.socket.as_ref().map(|sock| format!("unix://{sock}")))
    }

    /// Consume a child's `_instance.*` report, sent to this instance as the
    /// command op it is. `Ok` is the text of the task that records it.
    ///
    /// The listener and `a2a_send` have both refused a non-operator by now;
    /// this is asked all the same, because consuming a report takes the
    /// [`Admitted`] only [`admit`] hands out — so no path to a child's
    /// result, present or future, can get there without the question.
    #[cfg(feature = "a2a")]
    pub(crate) fn instance_op(
        &mut self,
        principal: &crate::a2a::Principal,
        op: InstanceOp,
        args: &Value,
    ) -> Result<String, String> {
        let admitted = admit_caller(principal, op).map_err(|refused| {
            self.log.warn(
                refused.event,
                json!({"op": op.name(), "principal": principal.id, "note": refused.note}),
            );
            format!("{} is operator-only", op.name())
        })?;
        let report = self.a2a_trace.clone();
        self.consume_instance_op(admitted, args, report.as_ref())
    }

    /// An `_instance.*` report that reached the durable inbox. Returns true
    /// when the event was one — consumed either way, because a malformed
    /// report is control-plane plumbing and must never reach a model, a wait
    /// or a start node.
    #[cfg(feature = "a2a")]
    pub(crate) fn handle_instance_op(&mut self, ev: &crate::state::InboxEvent) -> bool {
        match inbox_report(ev) {
            None => false,
            Some(Ok((admitted, args))) => {
                let op = admitted.op();
                if let Err(e) = self.consume_instance_op(admitted, &args, ev.trace.as_ref()) {
                    self.log
                        .warn("instance.op.fail", json!({"op": op.name(), "err": e}));
                }
                true
            }
            Some(Err(InboxRefusal::NotOperator(op, refused))) => {
                self.log.warn(
                    refused.event,
                    json!({"op": op, "role": ev.payload["role"], "note": refused.note}),
                );
                true
            }
            Some(Err(InboxRefusal::Unknown(op))) => {
                self.log.warn("instance.op.unknown", json!({"op": op}));
                true
            }
        }
    }

    /// Consume a child's report. Its lines are in the trace of the unit that
    /// spawned the child, and name the report's own span — the child's
    /// request, continued — as their `link`.
    #[cfg(feature = "a2a")]
    fn consume_instance_op(
        &mut self,
        admitted: Admitted,
        args: &Value,
        report: Option<&crate::obs::trace::TraceContext>,
    ) -> Result<String, String> {
        let op = admitted.op();
        let link = report.map(crate::obs::trace::TraceContext::traceparent);
        let handle = args["handle"].as_str().unwrap_or("").to_string();
        let live = self.subagents.get(&handle).is_some_and(|s| {
            s.tier.as_deref() == Some("instance") && !is_terminal_status(&s.status)
        });
        if !live {
            self.log.warn(
                "instance.op.orphan",
                json!({"op": op.name(), "handle": handle, "note": "no live instance child by that handle"}),
            );
            return Err(format!("no live instance child {handle:?}"));
        }
        // A child's report is the work of the unit that spawned it.
        let ilog = self.subagent_log(&handle);
        match op {
            InstanceOp::Result => {
                if let Some(s) = self.subagents.get_mut(&handle) {
                    // First completion wins: a child may report more than once
                    // (a retry, a second run), and the handle's result must
                    // stay the answer the parent already observed.
                    if s.result.is_none() {
                        s.result = Some(json!({
                            "status": args.get("status").cloned().unwrap_or(Value::Null),
                            "output": args.get("output").cloned().unwrap_or(Value::Null),
                        }));
                        s.updated = now_ms();
                        s.dirty = true;
                        ilog.info("instance.result", json!({"handle": handle, "link": link}));
                        self.persist_subagent(&handle);
                    }
                }
                Ok(format!("result from {handle} recorded"))
            }
            InstanceOp::Emit => {
                let stream = args["stream"].as_str().unwrap_or("").to_string();
                // Only into a stream the child's template mirrors. That list
                // is the producer set the stream-taint check judges at load
                // (`config::taint`); a report into any other stream would put
                // a child's text where the check saw nothing outside feeding.
                let mirrored = self
                    .subagents
                    .get(&handle)
                    .and_then(|r| r.template.as_deref())
                    .and_then(|t| self.settings.subagents.templates.get(t))
                    .is_some_and(|t| t.mirror_streams.iter().flatten().any(|m| *m == stream));
                if !mirrored {
                    ilog.warn(
                        "instance.mirror.fail",
                        json!({"handle": handle, "stream": stream,
                               "err": "not one of the template's mirror_streams"}),
                    );
                    return Err(format!(
                        "stream {stream:?} is not one of instance child {handle:?}'s mirror_streams"
                    ));
                }
                let event = args.get("event").cloned().unwrap_or(Value::Null);
                let subject = event["subject"].as_str().unwrap_or("").to_string();
                let correlation = event["correlation"].as_str().map(str::to_string);
                let data = event.get("data").cloned().unwrap_or(Value::Null);
                let id = format!("{handle}/{}", event["id"].as_str().unwrap_or("?"));
                let source = format!("instance:{handle}");
                // The mirrored copy keeps the span the child's event was
                // produced in, so a consumer here continues the child's trace.
                let produced = crate::obs::trace::TraceContext::from_members(&event);
                match self.append_event(
                    &stream,
                    &subject,
                    correlation.as_deref(),
                    data,
                    &id,
                    &source,
                    produced.as_ref(),
                ) {
                    Ok(seq) => {
                        self.stream_dirty = true;
                        ilog.info(
                            "instance.mirror",
                            json!({"handle": handle, "stream": stream, "seq": seq, "link": link}),
                        );
                        Ok(format!("event mirrored into {stream} at {seq}"))
                    }
                    Err(e) => {
                        ilog.warn(
                            "instance.mirror.fail",
                            json!({"handle": handle, "stream": stream, "err": e}),
                        );
                        Err(e)
                    }
                }
            }
        }
    }

    /// Budget metering for instance children: a DURABLE child's manifest
    /// carries its governor counters, so read the lifetime total every few
    /// seconds and charge the DELTA against the parent's windows. Charging the
    /// delta rather than the total is what keeps repeated polls from
    /// double-billing, and it makes a spawned desk draw down its sponsor's
    /// budget instead of spending off the books. A non-durable child has no
    /// manifest, so its usage is invisible by construction.
    fn meter_instances(&mut self) {
        let now = now_ms();
        {
            static LAST: std::sync::Mutex<u64> = std::sync::Mutex::new(0);
            let mut g = LAST.lock().unwrap_or_else(|e| e.into_inner());
            if now.saturating_sub(*g) < 5_000 {
                return;
            }
            *g = now;
        }
        let candidates: Vec<(String, Option<String>, u64)> = self
            .subagents
            .values()
            .filter(|s| {
                s.tier.as_deref() == Some("instance") && s.durable && !is_terminal_status(&s.status)
            })
            .map(|s| (s.handle.clone(), s.config_path.clone(), s.tokens))
            .collect();
        for (handle, config, charged) in candidates {
            let Some(dir) = config
                .as_deref()
                .and_then(|c| std::path::Path::new(c).parent().map(|p| p.to_path_buf()))
            else {
                continue;
            };
            let Some(total) = read_child_lifetime_tokens(&dir.join("state")) else {
                continue;
            };
            let delta = total.saturating_sub(charged);
            if delta == 0 {
                continue;
            }
            self.governor.charge(
                crate::wire::intel::Usage {
                    input_tokens: delta,
                    output_tokens: 0,
                },
                &[],
            );
            if let Some(s) = self.subagents.get_mut(&handle) {
                s.tokens = total;
                s.dirty = true;
            }
            self.subagent_log(&handle).info(
                "instance.metered",
                json!({"handle": handle, "delta_tokens": delta, "total_tokens": total}),
            );
            self.persist_subagent(&handle);
        }
    }

    fn instance_bucket_take(&mut self) -> bool {
        static BUCKET: std::sync::Mutex<Option<crate::supervisor::tree::TokenBucket>> =
            std::sync::Mutex::new(None);
        let mut g = BUCKET.lock().unwrap_or_else(|e| e.into_inner());
        if g.is_none() {
            let rate = self
                .settings
                .limits
                .subagents
                .instances
                .rate
                .clone()
                .unwrap_or_else(|| "4/1h".into());
            let (burst, per_s) = crate::supervisor::tree::parse_rate(&rate).unwrap_or((4, 3600.0));
            *g = Some(crate::supervisor::tree::TokenBucket::new(
                burst,
                f64::from(burst) / per_s,
            ));
        }
        g.as_mut().map(|b| b.try_take()).unwrap_or(true)
    }

    /// The directory the child `handle` is composed in: its `config.json`,
    /// `log`, `a2a.sock` and, when durable, its own file store in `state/`. A
    /// durable child's is under `root`, this parent's
    /// [`durable_instances_root`], where the parent finds it again in its next
    /// life. A non-durable child's (no `root`) is in this process's private
    /// directory, so no other parent ever reads it and none of it outlives
    /// this process.
    fn instance_dir(&mut self, handle: &str, root: Option<PathBuf>) -> Result<PathBuf, String> {
        match root {
            Some(root) => Ok(root.join(handle)),
            None => self.private_instances_dir().map(|d| d.join(handle)),
        }
    }

    /// This process's private directory for its non-durable instance
    /// children, made at the first of them: `agentd-<process nonce>` under
    /// this user's runtime directory ([`user_runtime_dir`]), else the system
    /// temp directory, mode 0700. A directory already at that path is refused
    /// rather than used, so it holds no other process's children; it is
    /// removed when the runtime ends.
    fn private_instances_dir(&mut self) -> Result<PathBuf, String> {
        if let Some(d) = &self.private_instances {
            return Ok(d.0.clone());
        }
        let base = user_runtime_dir(std::env::var_os("XDG_RUNTIME_DIR"))
            .unwrap_or_else(std::env::temp_dir);
        let dir = base.join(format!("agentd-{}", self.process_nonce));
        let mut builder = std::fs::DirBuilder::new();
        #[cfg(unix)]
        std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
        builder
            .create(&dir)
            .map_err(|e| format!("create {}: {e}", dir.display()))?;
        self.private_instances = Some(PrivateDir(dir.clone()));
        Ok(dir)
    }

    /// Let go of the directory `dir` a child was composed in, once the child
    /// has exited or was never started: the key copy it was composed is
    /// removed ([`forget_instance_key`]), and so are a non-durable child's
    /// `socket` and, unless asked to `keep` it, its whole directory, since
    /// nothing reads them again. A kept one goes with this process's private
    /// directory.
    fn release_instance_dir(&self, dir: &std::path::Path, socket: Option<&str>, keep: bool) {
        forget_instance_key(dir);
        if self
            .private_instances
            .as_ref()
            .is_some_and(|p| dir.starts_with(&p.0))
        {
            if let Some(s) = socket {
                let _ = std::fs::remove_file(s);
            }
            if !keep {
                let _ = std::fs::remove_dir_all(dir);
            }
        }
    }

    fn persist_subagent(&mut self, handle: &str) {
        if let Some(s) = self.subagents.get(handle)
            && s.durable
        {
            let _ = self.durable.put(
                crate::state::Kind::Subagent,
                handle,
                serde_json::to_value(s).unwrap_or(Value::Null),
                None,
            );
        }
        if let Some(s) = self.subagents.get_mut(handle) {
            s.dirty = false;
        }
    }
}

/// The instance daemon's command: same binary, `--config <path>`, config-alias
/// env scrubbed (the composed file is the child's whole config; only the
/// intelligence family passes through for credentials), own process group,
/// template rlimits, stdout and stderr to `log_file` — and no other
/// descriptor the process holds, however the process was started.
///
/// `delivered` is the instruction prose the composed config carries, which
/// this parent already delivered: its digest rides beside the child marker so
/// the child uses that text as written instead of delivering it again.
fn instance_command(
    exe: &std::path::Path,
    config_path: &std::path::Path,
    log_file: std::fs::File,
    limits: &Option<Value>,
    delivered: &str,
) -> Result<std::process::Command, String> {
    let mut cmd = std::process::Command::new(exe);
    cmd.arg("--config")
        .arg(config_path)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::from(
            log_file.try_clone().map_err(|e| e.to_string())?,
        ))
        .stderr(std::process::Stdio::from(log_file));
    for (k, _) in std::env::vars() {
        let alias = k.starts_with(crate::config::paths::ENV_PREFIX);
        // The intelligence credentials ride the environment (a composed file
        // never holds an inline one); which model the child runs is in its
        // composed file, said once, so the parent's choice of it in the
        // environment would contradict the template's.
        let choice = k.ends_with("_INTELLIGENCE_MODEL")
            || k.contains("_INTELLIGENCE_MODELS_")
            || k.contains("_INTELLIGENCE_ROLES");
        let keep = k.contains("INTELLIGENCE") && !choice;
        if alias && !keep {
            cmd.env_remove(&k);
        }
    }
    cmd.env(crate::supervisor::reap::INSTANCE_CHILD_ENV, "1");
    cmd.env(
        crate::supervisor::reap::PARENT_PID_ENV,
        std::process::id().to_string(),
    );
    cmd.env(
        crate::supervisor::reap::INSTANCE_DELIVERED_ENV,
        instruction_core::digest(delivered.as_bytes()),
    );
    let (memory_bytes, cpu_seconds) = parse_instance_rlimits(limits)?;
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        unsafe {
            cmd.pre_exec(move || {
                if libc::setpgid(0, 0) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                if let Some(b) = memory_bytes {
                    let lim = libc::rlimit {
                        rlim_cur: b,
                        rlim_max: b,
                    };
                    if libc::setrlimit(libc::RLIMIT_AS, &lim) != 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                }
                if let Some(c) = cpu_seconds {
                    let lim = libc::rlimit {
                        rlim_cur: c,
                        rlim_max: c + 5,
                    };
                    if libc::setrlimit(libc::RLIMIT_CPU, &lim) != 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                }
                Ok(())
            });
        }
    }
    // An instance is handed its log and nothing else: every descriptor
    // from 3 up is marked.
    crate::signals::pass_only_stdio(&mut cmd);
    Ok(cmd)
}

/// Instance rlimits: `{memory, cpu}` only (compile enforced it; parse here).
fn parse_instance_rlimits(limits: &Option<Value>) -> Result<(Option<u64>, Option<u64>), String> {
    let Some(l) = limits else {
        return Ok((None, None));
    };
    let memory = match l.get("memory").and_then(Value::as_str) {
        None => None,
        Some(m) => Some(
            crate::runtime::pressure::parse_bytes(m).map_err(|e| format!("limits.memory: {e}"))?,
        ),
    };
    let cpu = match l.get("cpu").and_then(Value::as_str) {
        None => None,
        Some(c) => Some(
            crate::config::parse_duration(c)
                .map_err(|e| format!("limits.cpu: {e}"))?
                .as_secs()
                .max(1),
        ),
    };
    Ok((memory, cpu))
}

/// Read a child's lifetime token total from its file-store manifest
/// (`<state>/agentd/<instance>/manifest/agent.json` — the instance segment is
/// found by walking, since the child's name is composed). The manifest's
/// `budget` is the child governor's `to_value()`: the `instance` scope's
/// `lifetime_used`.
fn read_child_lifetime_tokens(state_root: &std::path::Path) -> Option<u64> {
    let prefix_dir = state_root.join("agentd");
    for inst in std::fs::read_dir(prefix_dir).ok()? {
        let manifest = inst.ok()?.path().join("manifest").join("agent.json");
        if let Ok(text) = std::fs::read_to_string(&manifest)
            && let Ok(env) = serde_json::from_str::<Value>(&text)
            && let Some(used) = env
                .pointer("/state/budget/instance/lifetime_used")
                .and_then(Value::as_u64)
        {
            return Some(used);
        }
    }
    None
}

/// How much of a failed child's log [`exit_cause`] reads, from its end.
const EXIT_CAUSE_TAIL: u64 = 16 * 1024;

/// What a child that failed logged it failed of: the `err` of the last
/// `proc.exit` in its `log`.
fn exit_cause(log: &std::path::Path) -> Option<String> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = std::fs::File::open(log).ok()?;
    let len = f.metadata().ok()?.len();
    f.seek(SeekFrom::Start(len.saturating_sub(EXIT_CAUSE_TAIL)))
        .ok()?;
    let mut tail = Vec::new();
    f.read_to_end(&mut tail).ok()?;
    String::from_utf8_lossy(&tail)
        .lines()
        .rev()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .find(|v| v["event"] == "proc.exit")
        .and_then(|v| v.get("err")?.as_str().map(str::to_string))
}

/// The child's unix-socket path: `a2a.sock` in its directory `dir`, or,
/// where the child's listener could not bind that
/// ([`crate::a2a::socket::fits`]), a name in the temp directory `tmp` derived
/// from `dir` — that is its parent's and its handle's alone, and the same in
/// each of a durable parent's lives. `Err` when neither fits.
fn instance_socket_path(dir: &std::path::Path, tmp: &std::path::Path) -> Result<String, String> {
    use crate::a2a::socket::fits;
    let own = dir.join("a2a.sock");
    if fits(&own) {
        return Ok(own.display().to_string());
    }
    let id = crate::sha::sha256_hex(dir.as_os_str().as_encoded_bytes());
    let fallback = tmp.join(format!("agentd-{}.sock", &id[..16]));
    if fits(&fallback) {
        return Ok(fallback.display().to_string());
    }
    Err(format!(
        "no socket path for the child is short enough to bind: {} and {} are both too long; a shorter TMPDIR makes room",
        own.display(),
        fallback.display()
    ))
}

/// Where a durable parent keeps its instance children, one directory per
/// handle — `<state>/subagents/<store>/<prefix>/<instance>` — or `None` when
/// its store does not outlive its process (`memory`, `none`).
///
/// A durable parent is the state its store keeps under `<prefix>/<instance>`
/// ([`crate::store::key`]), so the same daemon finds its children here in
/// every life and no other daemon is handed them; `<store>` says which store
/// that state is in. A file store's directory is under the store's own root,
/// which one daemon holds at a time, so it is `file`. An mcp or http store is
/// a fleet's, so its parent's children are under the default state
/// directory, and `<store>` is the kind and a digest of the store's address:
/// two parents of one name and prefix on two stores are two daemons.
fn durable_instances_root(
    settings: &crate::config::settings::Settings,
    instance: &str,
) -> Option<PathBuf> {
    use crate::config::settings::{Store, StoreKind, file_store_root};
    let store = &settings.store;
    let fleet = |kind: &str, address: &str| {
        let id = crate::sha::sha256_hex(address.as_bytes());
        (
            file_store_root(&Store::default()),
            format!("{kind}-{}", &id[..12]),
        )
    };
    let (base, which) = match store.kind {
        StoreKind::Memory | StoreKind::None => return None,
        StoreKind::File => (file_store_root(store), "file".to_string()),
        StoreKind::Mcp => {
            let server = store.mcp.as_ref().map_or("", |m| m.server.as_str());
            let address = settings
                .mcp
                .servers
                .iter()
                .find(|s| s.name == server)
                .map_or(server, |s| s.endpoint.as_str());
            fleet("mcp", address)
        }
        StoreKind::Http => fleet(
            "http",
            store.http.as_ref().map_or("", |h| h.base_url.as_str()),
        ),
    };
    let segment = crate::store::file::encode;
    Some(
        base.join("subagents")
            .join(which)
            .join(segment(store.prefix()))
            .join(segment(instance)),
    )
}

/// `$XDG_RUNTIME_DIR` (`var`) when it is what the XDG base directory
/// specification says it is: a directory this process's user owns that no
/// other user can use (mode 0700). One this user was handed by another —
/// `sudo -E` keeps root's `/run/user/0` — or any other directory is not.
fn user_runtime_dir(var: Option<std::ffi::OsString>) -> Option<PathBuf> {
    let dir = PathBuf::from(var?);
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let m = std::fs::metadata(&dir).ok()?;
        let me = unsafe { libc::geteuid() };
        (m.is_dir() && m.uid() == me && m.mode() & 0o077 == 0).then_some(dir)
    }
    #[cfg(not(unix))]
    {
        dir.is_dir().then_some(dir)
    }
}

/// A directory of this process's own for its non-durable instance children
/// ([`Runtime::private_instances_dir`]), removed with everything in it when
/// the runtime that made it ends.
pub(crate) struct PrivateDir(PathBuf);

impl Drop for PrivateDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// The parent's service catalogue as an instance child is handed it: every
/// entry but the Companies. A child holding Company channels itself would
/// mint other User IDs and hold tokens in another process, so it is given
/// none (RFC 0047 §5.6), and its config cannot name one.
fn without_companies(mut services: Value) -> Value {
    if let Some(m) = services.as_object_mut() {
        m.retain(|_, entry| entry.get("kind").and_then(Value::as_str) != Some("company"));
    }
    services
}

/// The delivered prose a composed config file carries, as the child's loader
/// reads it — `agent.instruction` is the one string its `${VAR}`
/// substitution leaves alone, so the file's text is the child's text; `None`
/// when the file is gone or is not a composed config.
fn delivered_prose_of(config: &std::path::Path) -> Option<String> {
    let doc: Value = serde_json::from_slice(&std::fs::read(config).ok()?).ok()?;
    Some(doc.get("agent")?.get("instruction")?.as_str()?.to_string())
}

/// Boot the composed document through the same typing + resolution +
/// validation a config file gets. Errors are the aggregate report. `prose`
/// is the instruction the composed document carries, delivered here: it is
/// checked as the child will read it, not delivered a second time.
fn validate_composed(doc: &Value, prose: &str) -> Result<(), String> {
    let mut settings =
        crate::config::settings::Settings::from_delivered_document(doc.clone(), "template", prose)
            .map_err(|e| e.to_string())?;
    let res = crate::config::settings::resolve_services(&mut settings);
    let loaded = crate::config::settings::Loaded {
        settings,
        doc: doc.clone(),
        file_doc: doc.clone(),
        files: Vec::new(),
        warnings: Vec::new(),
        trace: Default::default(),
    };
    let diags = crate::config::settings::validate(&loaded);
    let mut errs = res;
    errs.extend(diags.errors);
    if errs.is_empty() {
        Ok(())
    } else {
        Err(errs.join("; "))
    }
}

/// The `_instance.*` ops a child sends home.
#[cfg(feature = "a2a")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum InstanceOp {
    /// A `mode: sync` child's first result.
    Result,
    /// One event of a `mirror_streams` stream.
    Emit,
}

#[cfg(feature = "a2a")]
impl InstanceOp {
    pub(crate) fn of(op: &str) -> Option<InstanceOp> {
        match op {
            "_instance.result" => Some(InstanceOp::Result),
            "_instance.emit" => Some(InstanceOp::Emit),
            _ => None,
        }
    }

    fn name(self) -> &'static str {
        match self {
            InstanceOp::Result => "_instance.result",
            InstanceOp::Emit => "_instance.emit",
        }
    }
}

/// Who may send a child's report, as a pure function of the caller's role and
/// the report: the one rule every path that consumes one asks, and the only
/// source of the [`Admitted`] that consuming takes.
#[cfg(feature = "a2a")]
mod admission {
    use super::InstanceOp;
    use crate::config::settings::Role;

    /// A report [`admit`] let through. Its field is private to this module,
    /// so nothing outside it can make one: a consumer that takes an
    /// `Admitted` cannot be reached without asking.
    #[derive(Debug, PartialEq)]
    pub(crate) struct Admitted(InstanceOp);

    impl Admitted {
        pub(crate) fn op(&self) -> InstanceOp {
            self.0
        }
    }

    /// Why a report was refused: the log event it is recorded under, and
    /// what the line says.
    #[derive(Debug, PartialEq)]
    pub(crate) struct Refused {
        pub(crate) event: &'static str,
        pub(crate) note: &'static str,
    }

    /// Only the operator speaks for a child: an `agent` that could write a
    /// child's result could decide what the parent believes its own subagent
    /// answered. A child dials home with the parent's own operator credential
    /// (its unix socket, or the `a2a.bearer` reference it was composed with)
    /// — but the resolver tries the configured principal rules first, so on
    /// a parent with a catch-all `any` rule that credential resolves to the
    /// rule's role, and the child's reports are refused here with
    /// `instance.op.refused`. Refusing such a spawn up front, by name, is the
    /// spawn's job, not this check's.
    pub(crate) fn admit(role: Role, op: InstanceOp) -> Result<Admitted, Refused> {
        if role == Role::Operator {
            Ok(Admitted(op))
        } else {
            Err(Refused {
                event: "instance.op.refused",
                note: "only the operator speaks for a child",
            })
        }
    }

    /// [`admit`], asked of the caller a request resolved to — its own role,
    /// never one the asker picks. A request's path asks this; only the inbox,
    /// which holds a role and no principal, asks `admit` directly.
    pub(crate) fn admit_caller(
        principal: &crate::a2a::Principal,
        op: InstanceOp,
    ) -> Result<Admitted, Refused> {
        admit(principal.role, op)
    }
}
#[cfg(feature = "a2a")]
use admission::{Admitted, Refused, admit, admit_caller};

/// Why an `_instance.*` report in the inbox is consumed without effect.
#[cfg(feature = "a2a")]
#[derive(Debug, PartialEq)]
enum InboxRefusal {
    /// The caller it arrived from was not the operator.
    NotOperator(String, Refused),
    /// No such report.
    Unknown(String),
}

/// Read an inbox event as a child's report: `None` when it is not one (it is
/// ordinary traffic for the readers after this), else the report — or why it
/// is refused. Separate from the consuming so the refusal is testable without
/// a runtime.
#[cfg(feature = "a2a")]
fn inbox_report(ev: &crate::state::InboxEvent) -> Option<Result<(Admitted, Value), InboxRefusal>> {
    use crate::config::settings::Role;
    let message = json!({"parts": ev.payload.get("parts").cloned().unwrap_or(Value::Null)});
    let op = super::a2a_server::command_op(&message)?;
    if !op.starts_with("_instance.") {
        return None;
    }
    // A role the event does not name is nobody's, and refused as one.
    let role: Role = serde_json::from_value(ev.payload["role"].clone()).unwrap_or(Role::Anonymous);
    let Some(kind) = InstanceOp::of(&op) else {
        // Asked of the role first all the same, so a non-operator is told
        // it may not report rather than which reports exist.
        return Some(Err(match admit(role, InstanceOp::Result) {
            Err(refused) => InboxRefusal::NotOperator(op, refused),
            Ok(_) => InboxRefusal::Unknown(op),
        }));
    };
    let admitted = match admit(role, kind) {
        Ok(admitted) => admitted,
        Err(refused) => return Some(Err(InboxRefusal::NotOperator(op, refused))),
    };
    let args = super::a2a_server::command_data(&message).unwrap_or_else(|| json!({}));
    Some(Ok((admitted, args)))
}

/// Remove a connection's `token` when it is an inline value rather than a
/// secret reference: a composed child document is a file on disk.
fn drop_inline_token(conn: &mut serde_json::Map<String, Value>) {
    if let Some(tok) = conn.get("token").and_then(Value::as_str)
        && !crate::sec::secret::has_secret_ref(tok)
    {
        conn.remove("token");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An instance child is composed every service entry but a Company's.
    #[test]
    fn an_instance_child_is_handed_no_company() {
        let services = json!({
            "crm": {"kind": "mcp", "endpoint": "https://crm.example/mcp"},
            "shop": {"kind": "company", "endpoint": "https://shop.example"}
        });
        let composed = without_companies(services);
        assert!(composed.get("crm").is_some());
        assert!(composed.get("shop").is_none(), "{composed}");
    }

    /// The composed config's prose reaches the child's loader byte for byte,
    /// a `${VAR}` and a `$$` it quotes included — the digest the parent marks
    /// the child with is of that prose, so an expanded copy would be
    /// delivered a second time — and a respawn reads back the same prose.
    #[test]
    fn a_composed_instruction_survives_the_childs_env_substitution() {
        let prose = "Quote `${HOME}` and `$${HOME}`, $$ and $5 as written.\n";
        let doc = json!({"agent": {"name": "p/t", "instruction": prose},
                         "store": {"kind": "memory"}});
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        std::fs::write(&path, serde_json::to_string_pretty(&doc).unwrap()).unwrap();
        let (l, _) = crate::config::settings::load(
            &["--config".to_string(), path.display().to_string()],
            &[("HOME".into(), "/elsewhere".into())],
        )
        .unwrap();
        assert_eq!(l.settings.agent.instruction.as_deref(), Some(prose));
        assert_eq!(delivered_prose_of(&path).as_deref(), Some(prose));
        assert_eq!(delivered_prose_of(&dir.path().join("gone.json")), None);
    }

    /// Only the operator speaks for a child, from one pure rule that every
    /// consumer asks: `admit` refuses each report for an agent, a user and a
    /// caller nobody named — as `instance.op.refused` — and admits it for the
    /// operator. Consuming a report takes the `Admitted` only `admit` makes,
    /// so `instance_op` cannot consume one without asking, whatever the
    /// listener and `a2a_send` refused before it.
    #[cfg(feature = "a2a")]
    #[test]
    fn only_the_operator_speaks_for_a_child() {
        use crate::config::settings::Role;
        for op in [InstanceOp::Result, InstanceOp::Emit] {
            assert_eq!(admit(Role::Operator, op).map(|a| a.op()), Ok(op));
            for role in [Role::Agent, Role::User, Role::Anonymous] {
                let refused = admit(role, op).expect_err("refused");
                assert_eq!(refused.event, "instance.op.refused", "{role:?} {op:?}");
            }
        }
        // …and the inbox path asks it: a report that reached the inbox from
        // anyone but the operator is consumed as a refusal, never as the
        // child's result.
        let report = |role: &str, op: &str| {
            crate::state::InboxEvent::new(
                "message",
                None,
                json!({
                    "role": role,
                    "parts": [{"data": {"agentd": {"op": op, "handle": "c1", "status": "completed"}}}],
                }),
            )
        };
        let refused = |op: &str| match inbox_report(&report("agent", op)) {
            Some(Err(InboxRefusal::NotOperator(o, r))) => (o, r.event),
            other => panic!("{op}: {other:?}"),
        };
        for role in ["agent", "user", "anonymous", "nonsense"] {
            match inbox_report(&report(role, "_instance.result")) {
                Some(Err(InboxRefusal::NotOperator(op, r))) => {
                    assert_eq!(
                        (op.as_str(), r.event),
                        ("_instance.result", "instance.op.refused")
                    );
                }
                other => panic!("{role}: {other:?}"),
            }
        }
        // An op that is no report is refused for its sender first.
        assert_eq!(
            refused("_instance.nope"),
            ("_instance.nope".to_string(), "instance.op.refused")
        );
        let Some(Ok((admitted, args))) = inbox_report(&report("operator", "_instance.result"))
        else {
            panic!("the operator's report is read");
        };
        assert_eq!(
            (admitted.op(), args["handle"].as_str()),
            (InstanceOp::Result, Some("c1"))
        );
        assert!(matches!(
            inbox_report(&report("operator", "_instance.nope")),
            Some(Err(InboxRefusal::Unknown(op))) if op == "_instance.nope"
        ));
        // Ordinary traffic is not a report at all.
        assert!(inbox_report(&report("agent", "review.start")).is_none());

        // A request asks it of the principal it resolved to, whatever its
        // grants: only the operator is admitted.
        let who = |role| crate::a2a::Principal {
            role,
            grants: vec!["*".into()],
            ..crate::a2a::Principal::anonymous()
        };
        for op in [InstanceOp::Result, InstanceOp::Emit] {
            assert_eq!(
                admit_caller(&who(Role::Operator), op).map(|a| a.op()),
                Ok(op)
            );
            for role in [Role::Agent, Role::User, Role::Anonymous] {
                let refused = admit_caller(&who(role), op).expect_err("refused");
                assert_eq!(refused.event, "instance.op.refused", "{role:?} {op:?}");
            }
        }

        // And `instance_op` asks exactly that. End to end the listener and
        // `a2a_send` refuse a non-operator first, so no request can show the
        // question missing, and a runtime cannot be built in a unit test;
        // `Admitted` stops a consumer that skips the question from compiling,
        // and this pins the one it must ask: the caller's, once, with no
        // other admission — of a role of its own choosing — beside it.
        let src = include_str!("instances.rs");
        let body = src
            .split("pub(crate) fn instance_op(")
            .nth(1)
            .and_then(|rest| rest.split("pub(crate) fn handle_instance_op(").next())
            .expect("instance_op");
        assert!(
            body.contains("admit_caller(principal, op)"),
            "instance_op must admit the caller it was given"
        );
        assert_eq!(
            (
                body.matches("admit_caller(").count(),
                body.matches("admit(").count()
            ),
            (1, 0),
            "instance_op must admit nothing but its caller: {body}"
        );
    }

    /// The reports this handler consumes are exactly the members the op table
    /// publishes under `$defs.reserved`: a report added on one side only would
    /// be served without a schema, or published and never served.
    #[cfg(feature = "a2a")]
    #[test]
    fn the_published_reports_are_the_consumed_ones() {
        use crate::runtime::surface::INSTANCE_OPS;
        let published: Vec<&str> = INSTANCE_OPS.iter().map(|m| m.name).collect();
        let consumed: Vec<&str> = [InstanceOp::Result, InstanceOp::Emit]
            .iter()
            .map(|op| op.name())
            .collect();
        assert_eq!(published, consumed);
        for name in published {
            assert_eq!(InstanceOp::of(name).map(InstanceOp::name), Some(name));
        }
    }

    /// A socket the child's listener could not bind in its directory falls
    /// back to the temp directory under a name of the child's directory:
    /// each parent's `inst-1` gets one of its own, the same one in every life
    /// of a durable parent, and the name stays short. What decides is the
    /// address the listener stages the bind at, not the socket's own path;
    /// and a spawn with no path that fits is refused.
    #[test]
    fn a_long_socket_path_falls_back_to_a_name_of_its_parents_own() {
        use crate::a2a::socket::{fits, staged};
        let tmp = std::path::Path::new("/tmp");
        let deep = |parent: &str| {
            std::path::PathBuf::from(format!("/{}/{parent}/inst-1", "d".repeat(100)))
        };
        let at = |dir: &std::path::Path| instance_socket_path(dir, tmp).unwrap();
        let (a, b) = (at(&deep("a")), at(&deep("b")));
        assert_ne!(a, b, "two parents' inst-1 share a socket");
        assert_eq!(
            a,
            at(&deep("a")),
            "a durable child's socket moves between its parent's lives"
        );
        let path = std::path::Path::new(&a);
        assert_eq!(path.parent(), Some(tmp), "{a}");
        let name = path.file_name().unwrap().to_string_lossy();
        assert!(!name.contains("inst-1"), "{name}");
        assert_eq!(name.len(), "agentd-.sock".len() + 16, "{name}");
        // One that fits stays in the child's directory.
        assert_eq!(at(std::path::Path::new("/s/inst-1")), "/s/inst-1/a2a.sock");
        // Where `a2a.sock` itself is short enough but the address the child
        // binds it at is not, the socket falls back too.
        let edge = |len: usize| std::path::PathBuf::from(format!("/{}", "e".repeat(len - 1)));
        let deepest = (1..200)
            .take_while(|&n| fits(&edge(n).join("a2a.sock")))
            .last()
            .unwrap();
        assert_eq!(
            at(&edge(deepest)),
            format!("{}/a2a.sock", edge(deepest).display())
        );
        let over = edge(deepest + 1);
        assert!(over.join("a2a.sock").as_os_str().len() < 100, "{over:?}");
        assert!(
            staged(&over.join("a2a.sock"), 4_194_304).as_os_str().len() >= 108,
            "a seven-digit pid could not bind it"
        );
        assert_eq!(std::path::Path::new(&at(&over)).parent(), Some(tmp));
        // A temp directory too deep for the fallback either is refused.
        let tmp_deep = std::path::PathBuf::from(format!("/{}", "t".repeat(90)));
        let e = instance_socket_path(&deep("a"), &tmp_deep).expect_err("refused");
        assert!(e.contains("TMPDIR"), "{e}");
    }

    /// The runtime directory is this user's own only when the XDG
    /// specification's rule holds: a directory the user owns, mode 0700. One
    /// another user owns, one others can enter, and a file are not.
    #[cfg(unix)]
    #[test]
    fn only_a_runtime_dir_of_this_users_own_is_used() {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("run");
        std::fs::create_dir(&dir).unwrap();
        let of = |p: &std::path::Path| user_runtime_dir(Some(p.as_os_str().to_owned()));
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert_eq!(of(&dir), Some(dir.clone()));
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(of(&dir), None, "a runtime directory others can enter");
        assert_eq!(of(std::path::Path::new("/proc")), None);
        let file = root.path().join("file");
        std::fs::write(&file, "").unwrap();
        assert_eq!(of(&file), None);
        assert_eq!(of(&root.path().join("gone")), None);
        assert_eq!(user_runtime_dir(None), None);
        // Root can hand a directory to another user and see it refused.
        if unsafe { libc::geteuid() } == 0 {
            let theirs = root.path().join("theirs");
            std::fs::create_dir(&theirs).unwrap();
            std::fs::set_permissions(&theirs, std::fs::Permissions::from_mode(0o700)).unwrap();
            std::os::unix::fs::chown(&theirs, Some(4_000_000), None).unwrap();
            assert_eq!(of(&theirs), None, "another user's runtime directory");
        }
    }

    /// A failed child's cause is the `err` of the last `proc.exit` its log
    /// has, however much it logged before it; a log without one has none.
    #[test]
    fn a_failed_childs_cause_is_its_last_exit_error() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("log");
        let mut text = String::new();
        text.push_str(r#"{"event":"proc.exit","err":"an earlier life"}"#);
        text.push('\n');
        for i in 0..2_000 {
            text.push_str(&format!("{{\"event\":\"tick\",\"n\":{i}}}\n"));
        }
        text.push_str("not json\n");
        text.push_str(r#"{"event":"proc.exit","code":2,"err":"a2a bind: too long"}"#);
        text.push_str("\nthread 'main' panicked\n");
        std::fs::write(&log, &text).unwrap();
        assert_eq!(exit_cause(&log).as_deref(), Some("a2a bind: too long"));
        std::fs::write(&log, r#"{"event":"proc.exit","code":0}"#).unwrap();
        assert_eq!(exit_cause(&log), None);
        assert_eq!(exit_cause(&dir.path().join("gone")), None);
    }

    /// A durable parent's children are kept under the identity its state is
    /// kept under — its file store's root, or the default state directory
    /// and the fleet store's address — with its prefix and instance name,
    /// each one path segment. A parent whose store does not outlive it has
    /// no such place.
    #[test]
    fn a_durable_parents_children_are_kept_under_its_own_identity() {
        let root = |store: Value| {
            let doc = json!({
                "agent": {"instruction": "x"},
                "store": store,
                "mcp": {"servers": [
                    {"name": "a", "endpoint": "https://a.example/mcp"},
                    {"name": "b", "endpoint": "https://b.example/mcp"},
                ]},
            });
            let s = crate::config::settings::Settings::from_document(doc, "t").unwrap();
            durable_instances_root(&s, "desk/room")
        };
        assert_eq!(
            root(json!({"kind": "file", "file": {"path": "/var/lib/agentd"}})),
            Some(std::path::PathBuf::from(
                "/var/lib/agentd/subagents/file/agentd/desk%2Froom"
            ))
        );
        assert_eq!(
            root(json!({"kind": "file", "prefix": "acme/x", "file": {"path": "/s"}})),
            Some(std::path::PathBuf::from(
                "/s/subagents/file/acme%2Fx/desk%2Froom"
            ))
        );
        let mcp = |server: &str| root(json!({"kind": "mcp", "mcp": {"server": server}})).unwrap();
        let http = root(json!({"kind": "http", "http": {"base_url": "https://a.example/mcp"}}));
        let defaulted = crate::config::settings::file_store_root(&Default::default());
        for fleet in [mcp("a"), mcp("b"), http.clone().unwrap()] {
            assert!(fleet.starts_with(defaulted.join("subagents")), "{fleet:?}");
            assert!(fleet.ends_with("agentd/desk%2Froom"), "{fleet:?}");
        }
        assert_eq!(mcp("a"), mcp("a"));
        assert_ne!(
            mcp("a"),
            mcp("b"),
            "two fleet stores share a parent's children"
        );
        assert_ne!(Some(mcp("a")), http, "an mcp and an http store share them");
        for kind in ["memory", "none"] {
            assert_eq!(root(json!({"kind": kind})), None, "{kind}");
        }
    }

    #[test]
    fn a_child_manifest_yields_its_lifetime_tokens() {
        let dir = std::env::temp_dir().join(format!("agentd-meter-{}", std::process::id()));
        let m = dir.join("agentd").join("parent%2Froom").join("manifest");
        std::fs::create_dir_all(&m).unwrap();
        std::fs::write(
            m.join("agent.json"),
            serde_json::to_string(&json!({
                "kind": "manifest", "id": "agent", "seq": 9,
                "state": {"generation": 1, "budget": {
                    "instance": {"windows": [], "lifetime_used": 4242},
                    "scopes": {}}}
            }))
            .unwrap(),
        )
        .unwrap();
        assert_eq!(read_child_lifetime_tokens(&dir), Some(4242));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// An embedder starts instances from its own binary, in a process the
    /// agentd binary's start-up marking never ran in: a descriptor the host
    /// left inheritable reaches a plain spawn, and not the instance.
    #[cfg(target_os = "linux")]
    #[test]
    fn an_instance_holds_no_descriptor_the_host_left_open() {
        use crate::signals::stray_fd::{StrayPipe, cleanup, lister, listing, lock, run};
        let _g = lock();
        let stray = StrayPipe::open();
        let exe = lister("instance");
        let plain = listing(&mut std::process::Command::new(&exe));
        assert!(stray.held_in(&plain), "a plain spawn inherits it: {plain}");
        let log = exe.with_file_name("log");
        let file = std::fs::File::create(&log).unwrap();
        let mut cmd = instance_command(&exe, std::path::Path::new("unused.yaml"), file, &None, "")
            .expect("the command builds");
        assert!(run(&mut cmd).status.success());
        let fds = std::fs::read_to_string(&log).unwrap();
        cleanup(&exe);
        assert!(fds.contains("-> /dev/null"), "the listing ran: {fds}");
        assert!(!stray.held_in(&fds), "the instance holds the stray: {fds}");
    }

    /// The child's check for a parent that died during the spawn compares with
    /// the pid written here; without it the check is off.
    #[test]
    fn an_instance_is_told_which_process_spawned_it() {
        let log = std::fs::File::open("/dev/null").unwrap();
        let cmd = instance_command(
            std::path::Path::new("agentd"),
            std::path::Path::new("unused.yaml"),
            log,
            &None,
            "",
        )
        .expect("the command builds");
        let mark = cmd
            .get_envs()
            .find(|(k, _)| *k == crate::supervisor::reap::PARENT_PID_ENV)
            .and_then(|(_, v)| v);
        let me = std::process::id().to_string();
        assert_eq!(mark, Some(std::ffi::OsStr::new(&me)));
    }
}
