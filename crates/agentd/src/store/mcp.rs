// SPDX-License-Identifier: AGPL-3.0-only
//! The **MCP-mapped store** (RFC 0025 §4.1): the four store operations are
//! `tools/call`s against a declared MCP server, with argument templates and
//! result extraction from `store.mcp.{put,get,list,delete}`. The **default
//! mapping is the RFC 0021 §8.3 checkpointer profile** (`state.put/get/list`,
//! `state.delete`), so a server advertising those tools needs no mapping.
//!
//! Every call carries `_meta["agent/idempotency_key"] = "<key>#<seq>"` and
//! `_meta["agent/instance"]`, and is bounded by the store timeout.

use super::mapping::{self, Vars};
use super::{KeySeq, PutOutcome, Store, StoreError};
use crate::config::v2::{StoreMcp, StoreOp};
use crate::wire::mcp::CallToolResult;
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::Duration;

/// The one thing the adapter needs from a connected server: `tools/call`
/// with per-call `_meta` and a timeout. Implemented by [`crate::mcp::client::McpClient`]
/// and by test doubles.
pub trait McpCall: Send + Sync {
    fn call(
        &self,
        tool: &str,
        args: Value,
        meta: Value,
        timeout: Duration,
    ) -> Result<CallToolResult, String>;
    fn server_name(&self) -> String;
}

impl McpCall for crate::mcp::client::McpClient {
    fn call(
        &self,
        tool: &str,
        args: Value,
        meta: Value,
        timeout: Duration,
    ) -> Result<CallToolResult, String> {
        self.call_tool_with_meta_within(tool, Some(args), meta, timeout)
            .map_err(|e| e.to_string())
    }
    fn server_name(&self) -> String {
        self.name().to_string()
    }
}

/// The default checkpointer profile (RFC 0021 §8.3 + `state.delete`).
pub fn default_ops() -> (StoreOp, StoreOp, StoreOp, StoreOp) {
    let op = |tool: &str, args: &str| StoreOp {
        tool: tool.into(),
        args: Some(args.into()),
        ok: None,
        conflict: None,
        value: None,
        keys: None,
    };
    (
        StoreOp {
            ok: Some("result.structuredContent.ok".into()),
            conflict: Some("result.structuredContent.latest".into()),
            ..op(
                "state.put",
                r#"{"key": "{key}", "seq": {seq}, "state": {envelope}}"#,
            )
        },
        StoreOp {
            value: Some("result.structuredContent.state".into()),
            ..op("state.get", r#"{"key": "{key}", "seq": {seq}}"#)
        },
        StoreOp {
            keys: Some("result.structuredContent.keys".into()),
            ..op("state.list", r#"{"prefix": "{prefix}"}"#)
        },
        op("state.delete", r#"{"key": "{key}"}"#),
    )
}

pub struct McpStore {
    client: Arc<dyn McpCall>,
    put: StoreOp,
    get: StoreOp,
    list: Option<StoreOp>,
    delete: Option<StoreOp>,
    timeout: Duration,
    /// Filled from the key at call time (the key layout carries them).
    prefix_hint: std::sync::Mutex<Option<(String, String)>>,
}

impl McpStore {
    pub fn new(client: Arc<dyn McpCall>, cfg: StoreMcp, timeout: Duration) -> McpStore {
        let (dput, dget, dlist, ddelete) = default_ops();
        McpStore {
            client,
            put: cfg.put.unwrap_or(dput),
            get: cfg.get.unwrap_or(dget),
            list: Some(cfg.list.unwrap_or(dlist)),
            delete: Some(cfg.delete.unwrap_or(ddelete)),
            timeout,
            prefix_hint: std::sync::Mutex::new(None),
        }
    }

    /// Restrict to the ops a server actually advertises (called by the runtime
    /// after `tools/list`): absent `list`/`delete` become `Unsupported`.
    pub fn with_advertised(mut self, tools: &[String]) -> McpStore {
        let has = |t: &str| tools.iter().any(|x| x == t);
        if !self.list.as_ref().is_some_and(|op| has(&op.tool)) {
            self.list = None;
        }
        if !self.delete.as_ref().is_some_and(|op| has(&op.tool)) {
            self.delete = None;
        }
        self
    }

    fn vars(&self, key: &str, seq: Option<u64>, envelope: Option<&Value>) -> Vars {
        // key = <prefix>/<instance>/<kind>/<id>
        let mut parts = key.splitn(4, '/');
        let prefix = parts.next().unwrap_or("");
        let instance = parts.next().unwrap_or("");
        let kind = parts.next().unwrap_or("");
        let id = parts.next().unwrap_or("");
        if let Ok(mut h) = self.prefix_hint.lock() {
            *h = Some((prefix.to_string(), instance.to_string()));
        }
        mapping::store_vars(key, seq, prefix, instance, envelope, kind, id)
    }

    fn call(
        &self,
        op: &StoreOp,
        vars: &Vars,
        key: &str,
        seq: Option<u64>,
    ) -> Result<Value, StoreError> {
        let args = match &op.args {
            Some(t) => mapping::render_json(t, vars)
                .map_err(|e| StoreError::Mapping(format!("{}: {e}", op.tool)))?,
            None => json!({ "key": key }),
        };
        let idem = match seq {
            Some(s) => format!("{key}#{s}"),
            None => key.to_string(),
        };
        let meta = json!({
            "agent/idempotency_key": idem,
            "agent/instance": vars.get("instance").cloned().unwrap_or(Value::Null),
        });
        let res = self
            .client
            .call(&op.tool, args, meta, self.timeout)
            .map_err(|e| {
                StoreError::Io(format!(
                    "{} on '{}': {e}",
                    op.tool,
                    self.client.server_name()
                ))
            })?;
        Ok(result_ctx(&res))
    }
}

/// The extraction context for a tool result: `structuredContent` (or the text
/// content parsed as JSON), `isError`, `text`, `content`.
pub fn result_ctx(res: &CallToolResult) -> Value {
    let text = res.text();
    let structured = res
        .structured_content
        .clone()
        .or_else(|| serde_json::from_str::<Value>(&text).ok())
        .unwrap_or(Value::Null);
    json!({
        "result": {
            "structuredContent": structured,
            "isError": res.is_error(),
            "text": text,
            "content": res.content,
        }
    })
}

impl Store for McpStore {
    fn put(&self, key: &str, seq: u64, envelope: &Value) -> Result<PutOutcome, StoreError> {
        let vars = self.vars(key, Some(seq), Some(envelope));
        let ctx = self.call(&self.put, &vars, key, Some(seq))?;
        let is_error = ctx["result"]["isError"].as_bool().unwrap_or(false);
        if let Some(okx) = &self.put.ok
            && let Some(v) = mapping::extract(okx, &ctx).map_err(|e| StoreError::Mapping(e.0))?
            && mapping::truthy(&v)
        {
            return Ok(PutOutcome::Ok);
        }
        if let Some(cx) = &self.put.conflict
            && let Some(v) = mapping::extract(cx, &ctx).map_err(|e| StoreError::Mapping(e.0))?
            && !v.is_null()
        {
            return Ok(PutOutcome::Conflict {
                latest_seq: v.as_u64(),
            });
        }
        if is_error {
            return Err(StoreError::Io(format!(
                "{} failed: {}",
                self.put.tool, ctx["result"]["text"]
            )));
        }
        // No `ok` predicate configured and no error ⇒ success.
        if self.put.ok.is_none() {
            return Ok(PutOutcome::Ok);
        }
        Err(StoreError::Io(format!(
            "{} not acknowledged: {}",
            self.put.tool, ctx["result"]["text"]
        )))
    }

    fn get(&self, key: &str, seq: Option<u64>) -> Result<Option<Value>, StoreError> {
        let vars = self.vars(key, seq, None);
        let ctx = self.call(&self.get, &vars, key, None)?;
        if ctx["result"]["isError"].as_bool().unwrap_or(false) {
            // A tool-domain error on a read is "absent" (the checkpointer
            // profile answers `no such key` that way); transport errors are Io.
            return Ok(None);
        }
        let v = match &self.get.value {
            Some(x) => mapping::extract(x, &ctx).map_err(|e| StoreError::Mapping(e.0))?,
            None => Some(ctx["result"]["structuredContent"].clone()),
        };
        Ok(v.filter(|v| !v.is_null()))
    }

    fn list(&self, prefix: &str) -> Result<Vec<KeySeq>, StoreError> {
        let Some(op) = &self.list else {
            return Err(StoreError::Unsupported("list"));
        };
        let mut vars = self.vars(prefix, None, None);
        vars.insert("prefix".into(), Value::String(prefix.to_string()));
        let ctx = self.call(op, &vars, prefix, None)?;
        if ctx["result"]["isError"].as_bool().unwrap_or(false) {
            return Err(StoreError::Io(format!(
                "{} failed: {}",
                op.tool, ctx["result"]["text"]
            )));
        }
        let keys = match &op.keys {
            Some(x) => mapping::extract(x, &ctx).map_err(|e| StoreError::Mapping(e.0))?,
            None => Some(ctx["result"]["structuredContent"]["keys"].clone()),
        };
        let mut out = Vec::new();
        if let Some(Value::Array(items)) = keys {
            for it in items {
                match it {
                    Value::String(k) => out.push(KeySeq { key: k, seq: None }),
                    Value::Object(o) => {
                        if let Some(k) = o.get("key").and_then(Value::as_str) {
                            out.push(KeySeq {
                                key: k.to_string(),
                                seq: o.get("seq").and_then(Value::as_u64),
                            });
                        }
                    }
                    _ => {}
                }
            }
        }
        Ok(out)
    }

    fn delete(&self, key: &str) -> Result<(), StoreError> {
        let Some(op) = &self.delete else {
            return Err(StoreError::Unsupported("delete"));
        };
        let vars = self.vars(key, None, None);
        let ctx = self.call(op, &vars, key, None)?;
        if ctx["result"]["isError"].as_bool().unwrap_or(false) {
            return Err(StoreError::Io(format!(
                "{} failed: {}",
                op.tool, ctx["result"]["text"]
            )));
        }
        Ok(())
    }

    fn kind(&self) -> &'static str {
        "mcp"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::sync::Mutex;

    /// A scripted checkpointer server speaking the default profile, with
    /// history + CAS — the shape `mcp/mock_http.rs` implements over HTTP.
    #[derive(Default)]
    struct FakeServer {
        data: Mutex<BTreeMap<String, BTreeMap<u64, Value>>>,
        calls: Mutex<Vec<(String, Value, Value)>>,
        fail: Mutex<bool>,
    }

    fn ok(v: Value) -> CallToolResult {
        CallToolResult {
            content: vec![json!({"type": "text", "text": v.to_string()})],
            is_error: Some(false),
            structured_content: None, // text-JSON only, like the mock
        }
    }
    fn err(msg: &str) -> CallToolResult {
        CallToolResult {
            content: vec![json!({"type": "text", "text": msg})],
            is_error: Some(true),
            structured_content: None,
        }
    }

    impl McpCall for FakeServer {
        fn call(
            &self,
            tool: &str,
            args: Value,
            meta: Value,
            _t: Duration,
        ) -> Result<CallToolResult, String> {
            self.calls
                .lock()
                .unwrap()
                .push((tool.to_string(), args.clone(), meta));
            if *self.fail.lock().unwrap() {
                return Err("connection reset".into());
            }
            let key = args["key"].as_str().unwrap_or("").to_string();
            let mut data = self.data.lock().unwrap();
            Ok(match tool {
                "state.put" => {
                    let seq = args["seq"].as_u64().unwrap_or(0);
                    let hist = data.entry(key).or_default();
                    let latest = hist.keys().next_back().copied().unwrap_or(0);
                    if seq <= latest {
                        ok(json!({"ok": false, "latest": latest}))
                    } else {
                        hist.insert(seq, args["state"].clone());
                        ok(json!({"ok": true, "seq": seq}))
                    }
                }
                "state.get" => match data.get(&key) {
                    None => err("no such key"),
                    Some(h) => {
                        let picked = match args["seq"].as_u64() {
                            Some(s) => h.get(&s),
                            None => h.values().next_back(),
                        };
                        match picked {
                            Some(v) => ok(json!({"state": v})),
                            None => err("no such seq"),
                        }
                    }
                },
                "state.list" => {
                    let prefix = args["prefix"].as_str().unwrap_or("");
                    let keys: Vec<Value> = data
                        .iter()
                        .filter(|(k, _)| k.starts_with(prefix))
                        .map(|(k, h)| json!({"key": k, "seq": h.keys().next_back()}))
                        .collect();
                    ok(json!({"keys": keys}))
                }
                "state.delete" => {
                    data.remove(&key);
                    ok(json!({"ok": true}))
                }
                _ => err("unknown tool"),
            })
        }
        fn server_name(&self) -> String {
            "fake".into()
        }
    }

    fn store(server: Arc<FakeServer>) -> McpStore {
        McpStore::new(
            server,
            StoreMcp {
                server: "fake".into(),
                put: None,
                get: None,
                list: None,
                delete: None,
            },
            Duration::from_secs(1),
        )
    }

    #[test]
    fn default_profile_round_trips_with_cas_and_meta() {
        let srv = Arc::new(FakeServer::default());
        let s = store(srv.clone());
        let env = json!({"v": 2, "kind": "run", "id": "1", "seq": 1, "state": {"x": 1}});
        assert_eq!(s.put("agentd/i/run/1", 1, &env).unwrap(), PutOutcome::Ok);
        assert_eq!(
            s.put("agentd/i/run/1", 1, &env).unwrap(),
            PutOutcome::Conflict {
                latest_seq: Some(1)
            }
        );
        assert_eq!(s.get("agentd/i/run/1", None).unwrap(), Some(env.clone()));
        assert_eq!(
            s.get("agentd/i/run/nope", None).unwrap(),
            None,
            "tool error on read = absent"
        );
        let l = s.list("agentd/i/").unwrap();
        assert_eq!(l.len(), 1);
        assert_eq!(l[0].key, "agentd/i/run/1");
        assert_eq!(l[0].seq, Some(1));
        s.delete("agentd/i/run/1").unwrap();
        assert_eq!(s.get("agentd/i/run/1", None).unwrap(), None);
        // The idempotency meta rode along.
        {
            let calls = srv.calls.lock().unwrap();
            let (tool, args, meta) = &calls[0];
            assert_eq!(tool, "state.put");
            assert_eq!(args["key"], json!("agentd/i/run/1"));
            assert_eq!(args["seq"], json!(1));
            assert_eq!(meta["agent/idempotency_key"], json!("agentd/i/run/1#1"));
            assert_eq!(meta["agent/instance"], json!("i"));
        }
        // Transport failure is Io.
        *srv.fail.lock().unwrap() = true;
        assert!(matches!(
            s.get("agentd/i/run/1", None),
            Err(StoreError::Io(_))
        ));
    }

    #[test]
    fn custom_mapping_and_advertised_ops() {
        // A server whose put is `kv.set {k, version, doc}` returning {stored: true}
        // and whose get is `kv.fetch {k}` returning {doc}.
        struct Kv(Mutex<BTreeMap<String, (u64, Value)>>);
        impl McpCall for Kv {
            fn call(
                &self,
                tool: &str,
                args: Value,
                _m: Value,
                _t: Duration,
            ) -> Result<CallToolResult, String> {
                let mut d = self.0.lock().unwrap();
                Ok(match tool {
                    "kv.set" => {
                        let k = args["k"].as_str().unwrap().to_string();
                        let v = args["version"].as_u64().unwrap();
                        if d.get(&k).is_some_and(|(cur, _)| *cur >= v) {
                            ok(json!({"stored": false, "current": d[&k].0}))
                        } else {
                            d.insert(k, (v, args["doc"].clone()));
                            ok(json!({"stored": true}))
                        }
                    }
                    "kv.fetch" => match d.get(args["k"].as_str().unwrap()) {
                        Some((_, doc)) => ok(json!({"doc": doc})),
                        None => ok(json!({"doc": null})),
                    },
                    _ => err("nope"),
                })
            }
            fn server_name(&self) -> String {
                "kv".into()
            }
        }
        let cfg = StoreMcp {
            server: "kv".into(),
            put: Some(StoreOp {
                tool: "kv.set".into(),
                args: Some(r#"{"k": "{key}", "version": {seq}, "doc": {envelope}}"#.into()),
                ok: Some("result.structuredContent.stored".into()),
                conflict: Some("result.structuredContent.current".into()),
                value: None,
                keys: None,
            }),
            get: Some(StoreOp {
                tool: "kv.fetch".into(),
                args: Some(r#"{"k": "{key}"}"#.into()),
                ok: None,
                conflict: None,
                value: Some("result.structuredContent.doc".into()),
                keys: None,
            }),
            list: None,
            delete: None,
        };
        let s = McpStore::new(
            Arc::new(Kv(Mutex::new(BTreeMap::new()))),
            cfg,
            Duration::from_secs(1),
        )
        .with_advertised(&["kv.set".into(), "kv.fetch".into()]);
        assert_eq!(
            s.put("p/i/k/1", 3, &json!({"a": 1})).unwrap(),
            PutOutcome::Ok
        );
        assert_eq!(
            s.put("p/i/k/1", 3, &json!({"a": 2})).unwrap(),
            PutOutcome::Conflict {
                latest_seq: Some(3)
            }
        );
        assert_eq!(s.get("p/i/k/1", None).unwrap(), Some(json!({"a": 1})));
        assert_eq!(s.get("p/i/k/2", None).unwrap(), None);
        assert!(matches!(s.list("p/"), Err(StoreError::Unsupported("list"))));
        assert!(matches!(
            s.delete("p/i/k/1"),
            Err(StoreError::Unsupported("delete"))
        ));
    }
}
