// SPDX-License-Identifier: AGPL-3.0-only
//! The consumer-alignment report (RFC-0028 §3.3), off the loop: ONE binding
//! on the registry serving the instruction, created once, and each applied
//! version reported on it.
//!
//! Both are tool calls on the registry, and a registry can be slow. They run
//! on one worker thread of their own, in the order the applies came — a
//! later version's report never lands before an earlier one's — and what
//! they come to goes back to the loop as [`Event::Background`] under
//! [`BACKGROUND_ID`]: the loop writes the binding id to the durable store
//! and says each line (`instruction.binding.created`, `.reported`, `.fail`),
//! so the store keeps its one writer. The report follows
//! `instruction.applied` by one registry round trip (two, when the binding is
//! created first), and no step or read waits for it.

use super::Runtime;
use super::events::Event;
use super::worker;
use crate::mcp::client::McpClient;
use crate::state::Kind;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender};

/// The durable record of the binding: `{binding_id, instruction_id, server}`.
pub(super) const BINDING_KEY: &str = "_instruction/binding";
/// The [`Event::Background`] id the worker's results come back under.
pub(super) const BACKGROUND_ID: &str = "instruction.binding";

/// One applied version to report.
pub(super) struct Job {
    server: String,
    client: Arc<McpClient>,
    instruction_id: String,
    /// The binding the store holds for this instruction, if it holds one.
    binding_id: Option<String>,
    /// `instructions_bindings_create`'s arguments, for when none is held.
    create: Value,
    target: String,
    mode: &'static str,
    version_id: String,
    delivered_digest: Option<String>,
}

/// Where the reports go: the worker's queue, started on the first report.
#[derive(Default)]
pub(crate) struct Reports {
    tx: Option<Sender<Job>>,
}

impl Runtime {
    /// Queue the report of `version_id`, read from `server` and applied:
    /// the binding is created first when the store holds none for this
    /// instruction. Best-effort by design — a registry that cannot take it
    /// must never take the agent down with it, nor hold its loop.
    pub(super) fn report_instruction_binding(
        &mut self,
        server: &str,
        canonical: Option<&str>,
        version_id: Option<&str>,
        delivered_digest: Option<&str>,
        uri: &str,
    ) {
        let (Some(canonical), Some(version_id)) = (canonical, version_id) else {
            return; // not a versioned registry read
        };
        let instruction_id = super::reactor::instruction_id_of(canonical);
        let Some(client) = self.mcp.get(server).cloned() else {
            return;
        };
        // One binding per (server, instruction), created once and kept in the
        // durable store so a restart reuses it instead of minting another.
        let binding_id = self
            .durable
            .get(Kind::Memory, BINDING_KEY)
            .ok()
            .flatten()
            .map(|e| e.state)
            .filter(|v| v["instruction_id"] == instruction_id)
            .and_then(|v| v["binding_id"].as_str().map(str::to_string));
        let (target, mode) = super::binding_target(uri, version_id);
        let create = json!({
            "instructionId": instruction_id,
            "target": &target,
            "mode": mode,
            // The registry's consumer shape is `{name, kind, host,
            // version}` and nothing more: it refuses any other property.
            "consumer": {
                "name": self.settings.instance_name(),
                "kind": "agentd",
                "host": self.settings.agent.name.clone().unwrap_or_else(|| "agentd".into()),
                "version": crate::VERSION,
            },
        });
        let job = Job {
            server: server.to_string(),
            client,
            instruction_id,
            binding_id,
            create,
            target,
            mode,
            version_id: version_id.to_string(),
            delivered_digest: delivered_digest.map(str::to_string),
        };
        if self.binding_reports.tx.is_none() {
            let (tx, rx) = std::sync::mpsc::channel();
            let events = self.events_tx.clone();
            let spawned = std::thread::Builder::new()
                .name("agentd-binding-report".into())
                .spawn(move || work(&rx, &events));
            if let Err(e) = spawned {
                self.log.warn(
                    "instruction.binding.fail",
                    json!({"op": "report", "server": server, "err": format!("no worker: {e}")}),
                );
                return;
            }
            self.binding_reports.tx = Some(tx);
        }
        if let Some(tx) = &self.binding_reports.tx
            && let Err(e) = tx.send(job)
        {
            // The worker is gone: the next report starts another.
            self.binding_reports.tx = None;
            self.log.warn(
                "instruction.binding.fail",
                json!({"op": "report", "server": e.0.server, "err": "the report worker stopped"}),
            );
        }
    }

    /// What the worker came to: the binding id kept, and the line said.
    pub(crate) fn on_binding_report(&mut self, result: &Value) {
        if let Some(record) = result.get("store") {
            let _ = self
                .durable
                .put(Kind::Memory, BINDING_KEY, record.clone(), None);
        }
        let event = result["event"]
            .as_str()
            .unwrap_or("instruction.binding.fail");
        let fields = result["fields"].clone();
        if result["level"] == "info" {
            self.log.info(event, fields);
        } else {
            self.log.warn(event, fields);
        }
    }
}

/// Send one line back to the loop.
fn say(events: &Sender<Event>, result: Value) {
    let _ = events.send(Event::Background {
        id: BACKGROUND_ID.into(),
        result,
    });
}

/// The worker: each job in turn, to the end of the queue. A binding it made
/// is remembered here too, so a report queued before the loop kept it does
/// not create another.
fn work(rx: &Receiver<Job>, events: &Sender<Event>) {
    let mut made: BTreeMap<(String, String), String> = BTreeMap::new();
    for job in rx {
        let ident = (job.server.clone(), job.instruction_id.clone());
        let mut binding_id = job.binding_id.clone().or_else(|| made.get(&ident).cloned());
        let server = job.server.as_str();
        if binding_id.is_none() {
            match job
                .client
                .call_tool("instructions_bindings_create", Some(job.create.clone()))
            {
                Ok(r) if !r.is_error() => {
                    binding_id = worker::tool_result_json(&r).and_then(|v| {
                        v.get("bindingId")
                            .or_else(|| v.pointer("/binding/id"))
                            .or_else(|| v.get("id"))
                            .and_then(Value::as_str)
                            .map(str::to_string)
                    });
                    match &binding_id {
                        Some(id) => {
                            made.insert(ident, id.clone());
                            say(
                                events,
                                json!({"level": "info", "event": "instruction.binding.created",
                                   "store": {"binding_id": id, "instruction_id": job.instruction_id,
                                             "server": server},
                                   "fields": {"server": server, "instruction_id": job.instruction_id,
                                              "binding_id": id, "target": job.target,
                                              "mode": job.mode}}),
                            );
                        }
                        // Said: a binding never made is a consumer the
                        // registry lists as behind, with nothing here.
                        None => say(
                            events,
                            json!({"level": "warn", "event": "instruction.binding.fail",
                               "fields": {"op": "create", "server": server,
                                          "err": "the result names no binding id (bindingId, binding.id or id)"}}),
                        ),
                    }
                }
                Ok(r) => say(
                    events,
                    json!({"level": "warn", "event": "instruction.binding.fail",
                           "fields": {"op": "create", "server": server, "err": r.text()}}),
                ),
                Err(e) => say(
                    events,
                    json!({"level": "warn", "event": "instruction.binding.fail",
                           "fields": {"op": "create", "server": server, "err": e.to_string()}}),
                ),
            }
        }
        let Some(binding_id) = binding_id else {
            continue;
        };
        let args = json!({
            "instructionId": job.instruction_id,
            "bindingId": binding_id,
            "versionId": job.version_id,
            "deliveredDigest": job.delivered_digest,
            "client": {"name": "agentd", "version": crate::VERSION},
        });
        let line = match job
            .client
            .call_tool("instructions_bindings_report", Some(args))
        {
            Ok(r) if !r.is_error() => {
                json!({"level": "info", "event": "instruction.binding.reported",
                       "fields": {"binding_id": binding_id, "version_id": job.version_id}})
            }
            Ok(r) => json!({"level": "warn", "event": "instruction.binding.fail",
                            "fields": {"op": "report", "server": server, "err": r.text()}}),
            Err(e) => json!({"level": "warn", "event": "instruction.binding.fail",
                             "fields": {"op": "report", "server": server, "err": e.to_string()}}),
        };
        say(events, line);
    }
}
