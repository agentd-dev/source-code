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
            return err(format!(
                "subagent.run refused: {lifetime} instance children spawned (limits.subagents.instances.total = {total})"
            ));
        }
        if !self.instance_bucket_take() {
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
        if params_introduced_machinery(&prose) {
            return err(format!(
                "subagent.run refused: params for template '{tname}' introduced directive machinery"
            ));
        }
        let handle = self.next_id("inst");
        let dir = self.instance_dir(&handle);
        if let Err(e) = std::fs::create_dir_all(&dir) {
            return err(format!("subagent.run: create {}: {e}", dir.display()));
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700));
        }
        let socket = instance_socket_path(&dir, &handle);
        let until = t
            .spec
            .until
            .as_ref()
            .map(|u| fold_params(u, &params))
            .filter(|u| !u.trim().is_empty());
        // Durability class (call site > template > store.durability.work):
        // non-durable ⇒ memory-only record, child on a memory store, no
        // restore-respawn — the throwaway war-room shape.
        let durable = args
            .get("durable")
            .and_then(Value::as_bool)
            .or(t.spec.durable)
            .unwrap_or_else(|| self.work_durable_default());
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
        let doc = match self.compose_instance_doc(
            t,
            &prose,
            &params,
            &dir,
            &socket,
            until.as_deref(),
            durable,
            &handle,
        ) {
            Ok(d) => d,
            Err(e) => return err(format!("subagent.run: template '{tname}': {e}")),
        };
        // The composed document must be a bootable config NOW — a child that
        // exits 2 on its first breath is a refusal we can make synchronously.
        if let Err(e) = validate_composed(&doc) {
            return err(format!(
                "subagent.run: template '{tname}' composes an invalid config: {e}"
            ));
        }
        let config_path = dir.join("config.json");
        let rendered = serde_json::to_string_pretty(&doc).unwrap_or_default();
        if let Err(e) = std::fs::write(&config_path, rendered) {
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
            tokens: 0,
            created: now_ms(),
            updated: now_ms(),
            payload: None,
            template: Some(tname.to_string()),
            tier: Some("instance".into()),
            pid: None,
            config_path: Some(config_path.display().to_string()),
            socket: Some(socket.clone()),
            retire_at,
            retiring_since: None,
            durable,
            node: None,
            dirty: true,
        };
        match self.spawn_instance_process(&config_path, &dir, &t.spec.limits) {
            Ok(pid) => {
                record.pid = Some(pid);
                record.status = "running".into();
                self.log.info(
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
    ) -> Result<Value, String> {
        let mut doc = t.fragment.clone();
        if !doc.is_object() {
            doc = json!({});
        }
        fold_params_value(&mut doc, params);
        doc["config_version"] = json!(crate::config::v2::schema::CONFIG_VERSION);
        let agent = doc
            .as_object_mut()
            .expect("object")
            .entry("agent")
            .or_insert_with(|| json!({}));
        agent["name"] = json!(format!("{}/{}", self.instance, t.name));
        agent["instruction"] = json!(prose);
        // Machinery workflows join whatever a `:::config` block declared.
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
                    "name": "_agentd_report", "version": 3, "steps": {
                        "ev":   {"kind": "event", "on": "workflow.finished"},
                        "pick": {"kind": "switch", "depends_on": ["ev"],
                                 "on": "{{steps.ev.output.payload.workflow}}",
                                 "cases": {rw: "get"}, "on_no_match": "skip"},
                        "get":  {"kind": "workflow.wait", "depends_on": ["pick"],
                                 "run": "{{steps.ev.output.payload.run}}", "timeout": "30s"},
                        "send": {"kind": "a2a.send", "depends_on": ["get"], "to": "parent",
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
                    "name": format!("_agentd_mirror_{m}"), "version": 3, "steps": {
                        "ev":   {"kind": "stream", "stream": m, "from": "new"},
                        "send": {"kind": "a2a.send", "depends_on": ["ev"], "to": "parent",
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
        if let Some(peer) = self.parent_peer_entry() {
            a2a["peers"] = json!([peer]);
        }
        doc["a2a"] = a2a;
        let mut lifecycle = json!({});
        if let Some(u) = until {
            lifecycle["until_signal"] = json!(u);
        }
        doc["lifecycle"] = lifecycle;
        // The child inherits the parent's service catalog, egress posture and
        // trifecta override. A template cannot set these (they are in
        // REFUSED_FRAGMENT_KEYS): a child must never be able to grant itself a
        // wider trust budget than the parent that spawned it.
        if let Some(services) = self.settings_doc.get("services") {
            doc["services"] = services.clone();
        }
        let mut security = json!({});
        if self.settings.security.allow_trifecta {
            security["allow_trifecta"] = json!(true);
        }
        if self.settings.security.egress == crate::config::v2::Egress::Closed {
            security["egress"] = json!("closed");
        }
        if let Some(ca) = &self.settings.security.tls_ca {
            security["tls_ca"] = json!(ca);
        }
        if security.as_object().is_some_and(|o| !o.is_empty()) {
            doc["security"] = security;
        }
        // Intelligence: the parent's section, minus its budget (the child's
        // budget is the template's grant) and minus any INLINE token (the env
        // passthrough carries it; a secret REFERENCE rides along fine).
        let mut intel = self
            .settings_doc
            .get("intelligence")
            .cloned()
            .unwrap_or(json!({}));
        if let Some(o) = intel.as_object_mut() {
            o.remove("budget");
            if let Some(tok) = o.get("token").and_then(Value::as_str)
                && !crate::sec::secret::has_secret_ref(tok)
            {
                o.remove("token");
            }
        }
        if let Some(m) = &t.spec.model {
            intel["model"] = json!(m);
        }
        if let Some(b) = &t.spec.budget {
            intel["budget"] = b.clone();
        }
        doc["intelligence"] = intel;
        Ok(doc)
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
        let mut peer = json!({"name": "parent", "endpoint": endpoint});
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

    /// Spawn the child daemon: same binary, `--config <path>`, config-alias env
    /// scrubbed (the composed file is the child's whole config; only the
    /// intelligence family passes through for credentials), own process group,
    /// template rlimits, log to `<dir>/log`.
    fn spawn_instance_process(
        &mut self,
        config_path: &std::path::Path,
        dir: &std::path::Path,
        limits: &Option<Value>,
    ) -> Result<i32, String> {
        let exe = std::env::current_exe().map_err(|e| format!("current_exe: {e}"))?;
        let log_file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(dir.join("log"))
            .map_err(|e| format!("open log: {e}"))?;
        let mut cmd = std::process::Command::new(exe);
        cmd.arg("--config")
            .arg(config_path)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::from(
                log_file.try_clone().map_err(|e| e.to_string())?,
            ))
            .stderr(std::process::Stdio::from(log_file));
        for (k, _) in std::env::vars() {
            let alias = k.starts_with("AGENTD_") || k.starts_with("AGENT_");
            let keep = k.contains("INTELLIGENCE");
            if alias && !keep {
                cmd.env_remove(&k);
            }
        }
        cmd.env(crate::supervisor::reap::INSTANCE_CHILD_ENV, "1");
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
                self.log.warn(
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
            self.log.info(
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
        if let Some(s) = self.subagents.get_mut(&handle) {
            s.status = status.into();
            s.error = error;
            s.result =
                Some(json!({"tier": "instance", "socket": s.socket, "config": s.config_path}));
            s.updated = now_ms();
            s.dirty = true;
        }
        self.log.info(
            "instance.exited",
            json!({"handle": handle, "pid": r.pid, "status": status}),
        );
        self.persist_subagent(&handle);
        true
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
            if !path.exists() {
                if let Some(s) = self.subagents.get_mut(&handle) {
                    s.status = "failed".into();
                    s.error = Some("config lost across restart".into());
                }
                self.persist_subagent(&handle);
                continue;
            }
            match self.spawn_instance_process(&path, &dir, &limits) {
                Ok(pid) => {
                    if let Some(s) = self.subagents.get_mut(&handle) {
                        s.pid = Some(pid);
                        s.status = "running".into();
                        s.attempt += 1;
                        s.retiring_since = None;
                    }
                    self.log
                        .info("instance.respawn", json!({"handle": handle, "pid": pid}));
                    self.persist_subagent(&handle);
                }
                Err(e) => {
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
    /// Fire-and-forget from the reactor's view (a thread does the dial).
    pub(crate) fn instance_send(&mut self, handle: &str, message: &str) -> ToolOutcome {
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
            let log = self.log.clone();
            let h = handle.to_string();
            std::thread::Builder::new()
                .name(format!("inst.send:{handle}"))
                .spawn(move || {
                    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
                    match crate::mcp::a2a_client::send(
                        &endpoint,
                        Default::default(),
                        &parts,
                        None,
                        None,
                        deadline,
                    ) {
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
            let _ = (message, &sock);
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

    /// Consume a child's `_instance.*` report. Returns true when the event was
    /// an internal op — consumed either way, because a malformed report is
    /// control-plane plumbing and is logged rather than surfaced to a model.
    #[cfg(feature = "a2a")]
    pub(crate) fn handle_instance_op(&mut self, ev: &crate::state::InboxEvent) -> bool {
        let message = json!({"parts": ev.payload.get("parts").cloned().unwrap_or(Value::Null)});
        let Some(op) = super::a2a_server::command_op(&message) else {
            return false;
        };
        if !op.starts_with("_instance.") {
            return false;
        }
        // Only the operator/agent trust levels may speak for a child (a unix
        // same-uid caller is operator; a bearer-authenticated child is agent).
        let role = ev.payload["role"].as_str().unwrap_or("");
        if !matches!(role, "operator" | "agent") {
            self.log.warn(
                "instance.op.refused",
                json!({"op": op, "role": role, "note": "internal ops need an operator/agent principal"}),
            );
            return true;
        }
        let args = super::a2a_server::command_data(&message).unwrap_or_else(|| json!({}));
        let handle = args["handle"].as_str().unwrap_or("").to_string();
        let live = self.subagents.get(&handle).is_some_and(|s| {
            s.tier.as_deref() == Some("instance") && !is_terminal_status(&s.status)
        });
        if !live {
            self.log.warn(
                "instance.op.orphan",
                json!({"op": op, "handle": handle, "note": "no live instance child by that handle"}),
            );
            return true;
        }
        match op.as_str() {
            "_instance.result" => {
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
                        self.log.info("instance.result", json!({"handle": handle}));
                        self.persist_subagent(&handle);
                    }
                }
            }
            "_instance.emit" => {
                let stream = args["stream"].as_str().unwrap_or("").to_string();
                let event = args.get("event").cloned().unwrap_or(Value::Null);
                let subject = event["subject"].as_str().unwrap_or("").to_string();
                let correlation = event["correlation"].as_str().map(str::to_string);
                let data = event.get("data").cloned().unwrap_or(Value::Null);
                let id = format!("{handle}/{}", event["id"].as_str().unwrap_or("?"));
                let source = format!("instance:{handle}");
                match self.append_event(
                    &stream,
                    &subject,
                    correlation.as_deref(),
                    data,
                    &id,
                    &source,
                ) {
                    Ok(seq) => {
                        self.stream_dirty = true;
                        self.log.info(
                            "instance.mirror",
                            json!({"handle": handle, "stream": stream, "seq": seq}),
                        );
                    }
                    Err(e) => self.log.warn(
                        "instance.mirror.fail",
                        json!({"handle": handle, "stream": stream, "err": e}),
                    ),
                }
            }
            other => self.log.warn(
                "instance.op.unknown",
                json!({"op": other, "handle": handle}),
            ),
        }
        true
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
            self.log.info(
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

    fn instance_dir(&self, handle: &str) -> PathBuf {
        crate::config::v2::file_store_root(&self.settings.store)
            .join("subagents")
            .join(handle)
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

/// The unix-socket path — kept under the sun path limit (~108 bytes) by
/// falling back to the temp dir for deep state roots.
fn instance_socket_path(dir: &std::path::Path, handle: &str) -> String {
    let p = dir.join("a2a.sock");
    let s = p.display().to_string();
    if s.len() <= 100 {
        s
    } else {
        std::env::temp_dir()
            .join(format!("agentd-{handle}.sock"))
            .display()
            .to_string()
    }
}

/// Boot the composed document through the same typing + resolution +
/// validation a config file gets. Errors are the aggregate report.
fn validate_composed(doc: &Value) -> Result<(), String> {
    let mut settings = crate::config::v2::Settings::from_document(doc.clone(), "template")
        .map_err(|e| e.to_string())?;
    let res = crate::config::v2::resolve_services(&mut settings);
    let loaded = crate::config::v2::Loaded {
        settings,
        doc: doc.clone(),
        file_doc: doc.clone(),
        files: Vec::new(),
        warnings: Vec::new(),
    };
    let diags = crate::config::v2::validate(&loaded);
    let mut errs = res;
    errs.extend(diags.errors);
    if errs.is_empty() {
        Ok(())
    } else {
        Err(errs.join("; "))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_child_manifest_yields_its_lifetime_tokens() {
        let dir = std::env::temp_dir().join(format!("agentd-meter-{}", std::process::id()));
        let m = dir.join("agentd").join("parent%2Froom").join("manifest");
        std::fs::create_dir_all(&m).unwrap();
        std::fs::write(
            m.join("agent.json"),
            serde_json::to_string(&json!({
                "v": 2, "kind": "manifest", "id": "agent", "seq": 9,
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
}
