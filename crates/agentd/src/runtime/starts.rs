// SPDX-License-Identifier: AGPL-3.0-only
//! **Start nodes** as the triggers (RFC 0027 §4, plan §3.6.6): beyond `once`
//! and `manual`, the long-lived start kinds fire runs while the instance lives
//! — `loop` (re-run on completion, `interval`/`until`/`max_iterations`/
//! `backoff`), `schedule` (cron / `every`, `catch_up`), `subscribe` (an MCP
//! resource update, notify-then-read, `debounce`/`coalesce`/`filter`,
//! `claim`/`shard` for exactly-one-owner in a cluster), `signal` (a named
//! signal), `event` (an internal lifecycle event), and `a2a` (a principal's
//! message routed here — P5). Start-node state (last fired, iteration, missed,
//! next deadline, debounce) is durable in the manifest.

use super::events::kinds;
use super::reactor::Runtime;
use crate::engine::model::Step;
use crate::engine::run::RunStatus;
use crate::state::now_ms;
use serde_json::{Map, Value, json};

/// `(workflow, node, kind, spec)` — a start node identity + its config.
type StartSpec = (String, String, String, Map<String, Value>);

impl Runtime {
    /// The manifest key for a start node's state.
    fn start_key(workflow: &str, node: &str) -> String {
        format!("{workflow}.{node}")
    }

    /// Read a start node's durable state.
    pub(crate) fn start_state_pub(&self, workflow: &str, node: &str) -> Value {
        self.start_state(workflow, node)
    }
    pub(crate) fn set_start_state_pub(&mut self, workflow: &str, node: &str, state: Value) {
        self.set_start_state(workflow, node, state)
    }

    fn start_state(&self, workflow: &str, node: &str) -> Value {
        self.durable
            .manifest()
            .starts
            .get(&Self::start_key(workflow, node))
            .cloned()
            .unwrap_or(json!({}))
    }

    /// Update a start node's durable state.
    fn set_start_state(&mut self, workflow: &str, node: &str, state: Value) {
        let key = Self::start_key(workflow, node);
        self.durable.manifest_update(|m| {
            m.starts.insert(key, state);
        });
    }

    /// Arm the long-lived start nodes at boot/restore (called by `arm_workflows`
    /// after the `once` handling). Schedules the first deadline for `loop`/
    /// `schedule` and subscribes `subscribe` resources.
    pub(crate) fn arm_long_lived_starts(&mut self) {
        // Boot's last pass over restored state before the loop starts ticking —
        // and the only one that is NOT also a hot-reload path, so the timer
        // repair (a run restored with a suspended step whose timer is gone)
        // rides here rather than re-running on every reload.
        self.repair_orphaned_timer_waits();
        let specs: Vec<StartSpec> = self
            .workflows
            .values()
            .filter(|w| w.armed)
            .flat_map(|w| {
                w.start_steps()
                    .into_iter()
                    .map(|s| (w.name.clone(), s.id.clone(), s.kind.clone(), s.spec.clone()))
                    .collect::<Vec<_>>()
            })
            .collect();
        for (workflow, node, kind, spec) in specs {
            match kind.as_str() {
                "schedule" => self.arm_schedule(&workflow, &node, &spec),
                "loop" => {
                    // A loop fires its first run immediately unless one is live.
                    let live = self
                        .runs
                        .values()
                        .any(|r| r.workflow == workflow && !r.status.is_terminal());
                    let iteration = self.start_state(&workflow, &node)["iteration"]
                        .as_u64()
                        .unwrap_or(0);
                    let max = spec.get("max_iterations").and_then(Value::as_u64);
                    if !live && max.is_none_or(|m| iteration < m) {
                        let delay = spec
                            .get("delay")
                            .and_then(Value::as_str)
                            .and_then(|d| crate::config::parse_duration(d).ok());
                        match delay {
                            Some(d) if !d.is_zero() => self.set_start_state(&workflow, &node, json!({"iteration": iteration, "next_ms": now_ms() + d.as_millis() as u64})),
                            _ => self.fire_start(&workflow, &node, &spec, json!({"iteration": iteration}), "loop"),
                        }
                    }
                }
                "subscribe" => self.arm_subscribe(&workflow, &node, &spec),
                _ => {}
            }
        }
    }

    fn arm_schedule(&mut self, workflow: &str, node: &str, spec: &Map<String, Value>) {
        let st = self.start_state(workflow, node);
        let at_fired = st["at_fired"].as_bool().unwrap_or(false);
        let next = self.next_schedule_ms(spec, now_ms(), at_fired);
        if let Some(next) = next {
            let mut st = st;
            st["next_ms"] = json!(next);
            self.set_start_state(workflow, node, st);
            self.log.info(
                "start.schedule.armed",
                json!({"workflow": workflow, "node": node, "next_ms": next}),
            );
        } else if at_fired {
            // The one-shot `at` was consumed in an earlier life: nothing to arm,
            // and nothing wrong either.
            self.log.info(
                "start.schedule.done",
                json!({"workflow": workflow, "node": node, "note": "one-shot `at` already fired"}),
            );
        } else {
            self.log.warn(
                "start.schedule.invalid",
                json!({"workflow": workflow, "node": node, "note": "no cron/every"}),
            );
        }
    }

    /// The next fire time (ms) for a `schedule` start node. `at_fired` is the
    /// durable "the one-shot `at` has already gone off" flag: once set, `at` is
    /// out of the running and only a recurrence (`every`/`cron`) can arm again.
    fn next_schedule_ms(
        &self,
        spec: &Map<String, Value>,
        after_ms: u64,
        at_fired: bool,
    ) -> Option<u64> {
        if let Some(every) = spec
            .get("every")
            .and_then(Value::as_str)
            .and_then(|e| crate::config::parse_duration(e).ok())
        {
            return Some(after_ms + every.as_millis() as u64);
        }
        if let Some(at) = spec
            .get("at")
            .and_then(Value::as_str)
            .filter(|_| !at_fired)
            .and_then(|a| crate::config::parse_duration(a).ok())
        {
            // `at` (a one-shot delay) — fire once after the delay, then never
            // again: it is consumed by its own firing (see `poll_starts`).
            return Some(now_ms() + at.as_millis() as u64);
        }
        #[cfg(feature = "cron")]
        if let Some(cron) = spec.get("cron").and_then(Value::as_str) {
            return crate::triggers::timer::CronExpr::parse(cron)
                .ok()
                .and_then(|c| c.next_after(after_ms / 1000))
                .map(|s| s * 1000);
        }
        None
    }

    fn arm_subscribe(&mut self, workflow: &str, node: &str, spec: &Map<String, Value>) {
        let server = spec.get("server").and_then(Value::as_str).unwrap_or("");
        let uri = spec.get("uri").and_then(Value::as_str).unwrap_or("");
        match self.mcp.get(server) {
            Some(c) => match c.subscribe(uri) {
                Ok(()) => self.log.info(
                    "start.subscribe.armed",
                    json!({"workflow": workflow, "node": node, "server": server, "uri": uri}),
                ),
                Err(e) => self.log.warn(
                    "start.subscribe.fail",
                    json!({"workflow": workflow, "node": node, "err": e.to_string()}),
                ),
            },
            None => self.log.warn(
                "start.subscribe.no_server",
                json!({"workflow": workflow, "node": node, "server": server}),
            ),
        }
    }

    /// Every tick: fire due `schedule`/`loop` starts and flush debounced
    /// `subscribe` firings.
    pub(crate) fn poll_starts(&mut self) {
        let now = now_ms();
        let due: Vec<StartSpec> = self
            .workflows
            .values()
            .filter(|w| w.armed)
            .flat_map(|w| {
                w.start_steps()
                    .into_iter()
                    .filter(|s| matches!(s.kind.as_str(), "schedule" | "loop" | "subscribe"))
                    .map(|s| (w.name.clone(), s.id.clone(), s.kind.clone(), s.spec.clone()))
                    .collect::<Vec<_>>()
            })
            .collect();
        for (workflow, node, kind, spec) in due {
            let st = self.start_state(&workflow, &node);
            match kind.as_str() {
                "schedule" => {
                    if let Some(next) = st["next_ms"].as_u64()
                        && now >= next
                    {
                        // A one-shot `at:` is CONSUMED by this firing. The flag
                        // is durable in the start state (not an in-memory one)
                        // because a restart re-arms from that state: without it
                        // every tick past the instant re-armed `now + at`, so a
                        // workflow the operator asked to run once at 03:00 ran
                        // continuously from 03:00 on. A `cron` alongside `at`
                        // still takes over from here — `at` is then just the
                        // first occurrence.
                        let at_fired =
                            st["at_fired"].as_bool().unwrap_or(false) || spec.contains_key("at");
                        self.fire_start(
                            &workflow,
                            &node,
                            &spec,
                            json!({"scheduled_for": next}),
                            "schedule",
                        );
                        // Arm the following occurrence (catch_up: one — fire once, skip missed).
                        let following = self.next_schedule_ms(&spec, now, at_fired);
                        let mut st = self.start_state(&workflow, &node);
                        match following {
                            Some(n) => st["next_ms"] = json!(n),
                            None => {
                                st.as_object_mut().map(|o| o.remove("next_ms"));
                            }
                        }
                        if at_fired {
                            st["at_fired"] = json!(true);
                        }
                        st["last_fired"] = json!(now);
                        self.set_start_state(&workflow, &node, st);
                    }
                }
                "loop" => {
                    let live = self
                        .runs
                        .values()
                        .any(|r| r.workflow == workflow && !r.status.is_terminal());
                    if !live
                        && let Some(next) = st["next_ms"].as_u64()
                        && now >= next
                    {
                        // Consume the armed deadline: only on_loop_run_finished
                        // re-arms (after the `until`/`max` check).
                        let mut st2 = self.start_state(&workflow, &node);
                        st2.as_object_mut().map(|o| o.remove("next_ms"));
                        self.set_start_state(&workflow, &node, st2);
                        let iteration = st["iteration"].as_u64().unwrap_or(0);
                        let max = spec.get("max_iterations").and_then(Value::as_u64);
                        if max.is_none_or(|m| iteration < m) {
                            self.fire_start(
                                &workflow,
                                &node,
                                &spec,
                                json!({"iteration": iteration}),
                                "loop",
                            );
                        }
                    }
                }
                "subscribe" => {
                    // A debounced firing whose window elapsed.
                    if let Some(fire_at) = st["debounce_until"].as_u64()
                        && now >= fire_at
                    {
                        let mut payload = st["pending_payload"].clone();
                        // The sample ring may have grown since the payload was
                        // coalesced — deliver the ring as of NOW, not as of the
                        // update that armed the debounce.
                        if payload.get("window").is_some() {
                            payload["window"] = st["window"].clone();
                        }
                        let mut st2 = self.start_state(&workflow, &node);
                        st2.as_object_mut().map(|o| {
                            o.remove("debounce_until");
                            o.remove("pending_payload")
                        });
                        self.set_start_state(&workflow, &node, st2);
                        self.fire_start(&workflow, &node, &spec, payload, "subscribe");
                    }
                }
                _ => {}
            }
        }
    }

    /// A `loop`/`schedule`/`subscribe` start fires: `deliver: run` (default)
    /// accepts a durable start event; `deliver: wait` would resolve a `wait`
    /// step (P4b). Applies the per-start `inputs` mapping.
    pub(crate) fn fire_start(
        &mut self,
        workflow: &str,
        node: &str,
        spec: &Map<String, Value>,
        payload: Value,
        kind: &str,
    ) {
        self.fire_start_run(workflow, node, spec, payload, kind, None);
    }

    /// Like [`Runtime::fire_start`], with an optional pre-generated `run_id` (so a
    /// caller — e.g. a `respond: sync` webhook — can link the run before it starts).
    pub(crate) fn fire_start_run(
        &mut self,
        workflow: &str,
        node: &str,
        spec: &Map<String, Value>,
        payload: Value,
        kind: &str,
        run_id: Option<&str>,
    ) {
        // Admission gate: a fired start under pressure is SKIPPED — logged with
        // its cause, so a schedule that quietly stopped firing while the disk
        // filled is a story the log tells, not a mystery. In-flight runs keep
        // draining; that is the point of shedding here rather than dying at the
        // next checkpoint.
        let low = self
            .workflows
            .get(workflow)
            .is_some_and(|w| w.priority == crate::engine::model::Priority::Low);
        if let Some(cause) = self.pressure.refusal(low) {
            self.log.warn(
                "start.shed",
                json!({"workflow": workflow, "node": node, "kind": kind, "cause": cause}),
            );
            return;
        }
        let inputs = match spec.get("inputs") {
            Some(mapping) => {
                let mut data = crate::engine::template::Data::new();
                data.insert("payload".into(), payload.clone());
                data.insert(
                    "env".into(),
                    json!({"instance": self.instance, "ts": now_ms()}),
                );
                match crate::engine::template::render(mapping, &data) {
                    Ok(v) => v,
                    Err(e) => {
                        // Fail closed, loudly. This used to fall back to `{}`,
                        // which fired the run with silently-empty inputs — a
                        // typo in the mapping became a mystery three steps
                        // later instead of one line here.
                        self.log.warn(
                            "start.inputs.invalid",
                            json!({"workflow": workflow, "node": node, "kind": kind, "err": e}),
                        );
                        return;
                    }
                }
            }
            None => json!({}),
        };
        self.log.info(
            "start.fired",
            json!({"workflow": workflow, "node": node, "kind": kind}),
        );
        let mut st = self.start_state(workflow, node);
        st["last_fired"] = json!(now_ms());
        if kind == "loop" {
            st["iteration"] = json!(st["iteration"].as_u64().unwrap_or(0) + 1);
        }
        self.set_start_state(workflow, node, st);
        let mut ev =
            json!({"workflow": workflow, "node": node, "payload": payload, "inputs": inputs});
        if let Some(rid) = run_id {
            ev["run_id"] = json!(rid);
        }
        let _ = self.accept_event(kinds::START_FIRED, None, ev);
    }

    /// A `loop`'s run finished: re-arm the next iteration (interval / backoff /
    /// `until`).
    pub(crate) fn on_loop_run_finished(
        &mut self,
        workflow: &str,
        node: &str,
        spec: &Map<String, Value>,
        ok: bool,
        last_output: &Value,
    ) {
        // `until` (CEL over the last outcome) stops the loop.
        if let Some(until) = spec.get("until").and_then(Value::as_str) {
            let mut data = crate::engine::template::Data::new();
            data.insert("outcome".into(), json!({"ok": ok, "output": last_output}));
            data.insert("last".into(), last_output.clone());
            let vars: Vec<(&str, &Value)> = data.iter().map(|(k, v)| (k.as_str(), v)).collect();
            if crate::cel::eval_bool(until.trim().trim_start_matches("CEL:").trim(), &vars)
                == Ok(true)
            {
                self.log.info(
                    "start.loop.stopped",
                    json!({"workflow": workflow, "node": node, "reason": "until"}),
                );
                let mut st = self.start_state(workflow, node);
                st.as_object_mut().map(|o| o.remove("next_ms"));
                self.set_start_state(workflow, node, st);
                return;
            }
        }
        let st = self.start_state(workflow, node);
        let iteration = st["iteration"].as_u64().unwrap_or(0);
        if let Some(max) = spec.get("max_iterations").and_then(Value::as_u64)
            && iteration >= max
        {
            self.log.info(
                "start.loop.stopped",
                json!({"workflow": workflow, "node": node, "reason": "max_iterations"}),
            );
            let mut st = self.start_state(workflow, node);
            st.as_object_mut().map(|o| o.remove("next_ms"));
            self.set_start_state(workflow, node, st);
            return;
        }
        // interval / backoff on failure.
        let interval = spec
            .get("interval")
            .and_then(Value::as_str)
            .and_then(|i| crate::config::parse_duration(i).ok())
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        let delay = if !ok {
            spec.get("backoff")
                .and_then(|b| b.get("initial"))
                .and_then(Value::as_str)
                .and_then(|i| crate::config::parse_duration(i).ok())
                .map(|d| d.as_millis() as u64)
                .unwrap_or(interval)
        } else {
            interval
        };
        let mut st = self.start_state(workflow, node);
        st["next_ms"] = json!(now_ms() + delay);
        self.set_start_state(workflow, node, st);
    }

    /// A subscribed resource updated: debounce/coalesce/filter, then fire a run.
    pub(crate) fn on_subscribe_resource(&mut self, server: &str, uri: &str) {
        let matches: Vec<(String, String, Map<String, Value>)> = self
            .workflows
            .values()
            .filter(|w| w.armed)
            .flat_map(|w| {
                w.start_steps()
                    .into_iter()
                    .filter(|s| {
                        s.kind == "subscribe"
                            && s.field_str("server") == Some(server)
                            && s.field_str("uri") == Some(uri)
                    })
                    .map(|s| (w.name.clone(), s.id.clone(), s.spec.clone()))
                    .collect::<Vec<_>>()
            })
            .collect();
        if matches.is_empty() {
            return;
        }
        // Notify-then-read, OFF the loop (same shape as `on_resource_updated`):
        // the read is a network round trip bounded only by the MCP server's
        // patience, and subscriptions are the reactivity hot path — an inline
        // read here handed a slow server the whole daemon per update. The read
        // thread reports back as an event; the filter/window/debounce state
        // machine below runs on the loop when it lands.
        let Some(client) = self.mcp.get(server).cloned() else {
            return; // the server went away between the notification and here
        };
        let tx = self.events_tx.clone();
        let (srv, u) = (server.to_string(), uri.to_string());
        std::thread::Builder::new()
            .name(format!("mcp.subscribe:{server}"))
            .spawn(move || {
                let content = client.read_resource(&u).ok().map(|r| {
                    let t = r.text();
                    serde_json::from_str::<Value>(&t).unwrap_or(Value::String(t))
                });
                let _ = tx.send(super::events::Event::SubscribeRead {
                    server: srv,
                    uri: u,
                    content,
                });
            })
            .ok();
    }

    /// The loop half of a `subscribe` update: the off-loop read landed; apply
    /// filter → window ring → debounce/fire per matching start node.
    pub(crate) fn on_subscribe_read(&mut self, server: &str, uri: &str, content: Option<Value>) {
        let matches: Vec<(String, String, Map<String, Value>)> = self
            .workflows
            .values()
            .filter(|w| w.armed)
            .flat_map(|w| {
                w.start_steps()
                    .into_iter()
                    .filter(|s| {
                        s.kind == "subscribe"
                            && s.field_str("server") == Some(server)
                            && s.field_str("uri") == Some(uri)
                    })
                    .map(|s| (w.name.clone(), s.id.clone(), s.spec.clone()))
                    .collect::<Vec<_>>()
            })
            .collect();
        for (workflow, node, spec) in matches {
            // filter (CEL over the read).
            if let Some(filter) = spec.get("filter").and_then(Value::as_str) {
                let mut data = crate::engine::template::Data::new();
                data.insert("content".into(), content.clone().unwrap_or(Value::Null));
                let vars: Vec<(&str, &Value)> = data.iter().map(|(k, v)| (k.as_str(), v)).collect();
                if crate::cel::eval_bool(filter.trim().trim_start_matches("CEL:").trim(), &vars)
                    != Ok(true)
                {
                    continue;
                }
            }
            // `window: {samples: N}`: keep a ring of the last N read values in
            // the durable start-state, so the fired run sees the trend, not
            // just the reading that happened to fire it. The ring accrues on
            // every (filter-passing) update — including updates a debounce
            // window coalesces away, which is the point: coalescing drops
            // FIRINGS, the window keeps the SAMPLES.
            let window_n = spec
                .get("window")
                .and_then(|w| w.get("samples"))
                .and_then(Value::as_u64)
                .map(|n| n as usize);
            if let Some(n) = window_n {
                let mut st = self.start_state(&workflow, &node);
                let mut ring: Vec<Value> = st["window"].as_array().cloned().unwrap_or_default();
                ring.push(content.clone().unwrap_or(Value::Null));
                if ring.len() > n {
                    let drop = ring.len() - n;
                    ring.drain(..drop);
                }
                st["window"] = Value::Array(ring);
                self.set_start_state(&workflow, &node, st);
            }
            let mut payload = json!({"server": server, "uri": uri, "content": content});
            if window_n.is_some() {
                payload["window"] = self.start_state(&workflow, &node)["window"]
                    .as_array()
                    .cloned()
                    .map(Value::Array)
                    .unwrap_or_else(|| json!([]));
            }
            let debounce = spec.get("debounce_ms").and_then(Value::as_u64).unwrap_or(0);
            if debounce > 0 {
                // Coalesce: newest payload wins; fire when the window elapses.
                let mut st = self.start_state(&workflow, &node);
                st["debounce_until"] = json!(now_ms() + debounce);
                st["pending_payload"] = payload;
                self.set_start_state(&workflow, &node, st);
            } else {
                self.fire_start(&workflow, &node, &spec, payload, "subscribe");
            }
        }
    }

    /// Fire `signal` start nodes for a named signal. Returns how many fired.
    pub(crate) fn fire_signal_starts(
        &mut self,
        name: &str,
        payload: &Value,
        _broadcast: bool,
    ) -> u64 {
        let matches: Vec<(String, String, Map<String, Value>)> = self
            .workflows
            .values()
            .filter(|w| w.armed)
            .flat_map(|w| {
                w.start_steps()
                    .into_iter()
                    .filter(|s| s.kind == "signal" && s.field_str("name") == Some(name))
                    .map(|s| (w.name.clone(), s.id.clone(), s.spec.clone()))
                    .collect::<Vec<_>>()
            })
            .collect();
        let mut fired = 0;
        for (workflow, node, spec) in matches {
            if let Some(filter) = spec.get("filter").and_then(Value::as_str) {
                let mut data = crate::engine::template::Data::new();
                data.insert("payload".into(), payload.clone());
                let vars: Vec<(&str, &Value)> = data.iter().map(|(k, v)| (k.as_str(), v)).collect();
                if crate::cel::eval_bool(filter.trim().trim_start_matches("CEL:").trim(), &vars)
                    != Ok(true)
                {
                    continue;
                }
            }
            self.fire_start(
                &workflow,
                &node,
                &spec,
                json!({"signal": name, "payload": payload}),
                "signal",
            );
            fired += 1;
        }
        fired
    }

    /// Fire `event` start nodes for an internal lifecycle event.
    pub(crate) fn fire_event_starts(&mut self, event: &str, payload: &Value) {
        let matches: Vec<(String, String, Map<String, Value>)> = self
            .workflows
            .values()
            .filter(|w| w.armed)
            .flat_map(|w| {
                w.start_steps()
                    .into_iter()
                    .filter(|s| s.kind == "event" && s.field_str("on") == Some(event))
                    .map(|s| (w.name.clone(), s.id.clone(), s.spec.clone()))
                    .collect::<Vec<_>>()
            })
            .collect();
        for (workflow, node, spec) in matches {
            // Self-trigger suppression: a watcher on `workflow.finished` with
            // no (or a too-loose) filter must not fire on ITS OWN completions
            // — that is an infinite loop of runs, not a reaction. An event
            // about workflow W never fires W's own event start.
            if payload.get("workflow").and_then(Value::as_str) == Some(workflow.as_str()) {
                continue;
            }
            if let Some(filter) = spec.get("filter").and_then(Value::as_str) {
                let mut data = crate::engine::template::Data::new();
                data.insert("payload".into(), payload.clone());
                let vars: Vec<(&str, &Value)> = data.iter().map(|(k, v)| (k.as_str(), v)).collect();
                if crate::cel::eval_bool(filter.trim().trim_start_matches("CEL:").trim(), &vars)
                    != Ok(true)
                {
                    continue;
                }
            }
            self.fire_start(
                &workflow,
                &node,
                &spec,
                json!({"event": event, "payload": payload}),
                "event",
            );
        }
    }

    /// The start-node spec of a run's `run.start` node (for loop re-arming).
    pub(crate) fn run_start_spec(
        &self,
        run_id: &str,
    ) -> Option<(String, String, Map<String, Value>, String)> {
        let run = self.runs.get(run_id)?;
        let w = self.workflows.get(&run.workflow)?;
        let s: &Step = w.step(&run.start.node)?;
        Some((
            run.workflow.clone(),
            run.start.node.clone(),
            s.spec.clone(),
            s.kind.clone(),
        ))
    }
}

/// Whether a status is a success for `event on: workflow.finished|failed`.
pub fn run_event(status: RunStatus) -> Option<&'static str> {
    match status {
        RunStatus::Completed => Some("workflow.finished"),
        RunStatus::Failed | RunStatus::Stalled | RunStatus::Cancelled | RunStatus::Refused => {
            Some("workflow.failed")
        }
        _ => None,
    }
}
