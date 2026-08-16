// SPDX-License-Identifier: Apache-2.0
//! The **HTTP store** (RFC 0025 §4.2): the four operations as plain HTTP
//! requests built from `store.http.{get,put,list,delete}` templates —
//! `GET/PUT/POST/DELETE {url}` with an optional JSON `body`, extraction from
//! `body`/`status`/`headers`. HTTPS (loopback `http://` for dev), headers may
//! carry `{{secret:…}}` references (resolved at dial time, never logged), the
//! idempotency key rides as `Idempotency-Key`; `conflict_status` (default
//! `409`) maps to `Conflict`, `404` on a read to absent.

use super::mapping::{self, Vars};
use super::{KeySeq, PutOutcome, Store, StoreError};
use crate::config::v2::{HttpOp, StoreHttp};
use crate::net::http::{self, Url};
use serde_json::{Value, json};
use std::time::Duration;

pub struct HttpStore {
    cfg: StoreHttp,
    timeout: Duration,
}

impl HttpStore {
    pub fn new(cfg: StoreHttp, timeout: Duration) -> Result<HttpStore, StoreError> {
        Url::parse(&cfg.base_url)
            .map_err(|e| StoreError::Mapping(format!("store.http.base_url: {e}")))?;
        if cfg.get.is_none() || cfg.put.is_none() {
            return Err(StoreError::Mapping(
                "store.http needs `get` and `put` operations".into(),
            ));
        }
        Ok(HttpStore { cfg, timeout })
    }

    fn vars(&self, key: &str, seq: Option<u64>, envelope: Option<&Value>) -> Vars {
        let mut parts = key.splitn(4, '/');
        let prefix = parts.next().unwrap_or("");
        let instance = parts.next().unwrap_or("");
        let kind = parts.next().unwrap_or("");
        let id = parts.next().unwrap_or("");
        let mut v = mapping::store_vars(key, seq, prefix, instance, envelope, kind, id);
        v.insert(
            "base_url".into(),
            Value::String(self.cfg.base_url.trim_end_matches('/').to_string()),
        );
        v
    }

    /// Perform one operation; returns `(status, extraction ctx)`.
    fn request(
        &self,
        op: &HttpOp,
        vars: &Vars,
        idempotency: Option<String>,
    ) -> Result<(u16, Value), StoreError> {
        let url_text = mapping::render_text(&op.url, vars)
            .map_err(|e| StoreError::Mapping(format!("store.http url: {e}")))?;
        let url = Url::parse(&url_text)
            .map_err(|e| StoreError::Mapping(format!("store.http url {url_text:?}: {e}")))?;
        let method = op.method.as_deref().unwrap_or("GET").to_ascii_uppercase();
        let body: Vec<u8> = match &op.body {
            Some(t) => {
                let v = mapping::render_json(t, vars)
                    .map_err(|e| StoreError::Mapping(format!("store.http body: {e}")))?;
                serde_json::to_vec(&v).unwrap_or_default()
            }
            None => Vec::new(),
        };
        // Resolve header templates (secret refs → values) at dial time.
        let env = |k: &str| std::env::var(k).ok();
        let mut headers: Vec<(String, String)> = Vec::new();
        for (k, v) in &self.cfg.headers {
            let val = crate::sec::secret::resolve(v, &env)
                .map_err(|e| StoreError::Mapping(format!("store.http header {k}: {e}")))?;
            headers.push((k.clone(), val));
        }
        if !body.is_empty() {
            headers.push(("Content-Type".into(), "application/json".into()));
        }
        headers.push(("Accept".into(), "application/json".into()));
        if let Some(idem) = idempotency {
            headers.push(("Idempotency-Key".into(), idem));
        }
        let hdrs: Vec<(&str, &str)> = headers
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();
        let mut stream = connect(&url, self.timeout)?;
        let resp = http::send(
            stream.as_mut(),
            &url.host_header(),
            &method,
            &url.path,
            &hdrs,
            &body,
        )
        .map_err(|e| StoreError::Io(format!("{method} {url_text}: {e}")))?;
        let body_json: Value = serde_json::from_slice(&resp.body)
            .unwrap_or_else(|_| Value::String(resp.body_str().into_owned()));
        let hmap: serde_json::Map<String, Value> = resp
            .headers
            .iter()
            .map(|(k, v)| (k.clone(), Value::String(v.clone())))
            .collect();
        Ok((
            resp.status,
            json!({ "body": body_json, "status": resp.status, "headers": hmap }),
        ))
    }
}

fn connect(url: &Url, timeout: Duration) -> Result<Box<dyn http::Stream>, StoreError> {
    let tcp = http::connect_tcp(&url.host, url.port, timeout)
        .map_err(|e| StoreError::Io(format!("connect {}: {e}", url.host)))?;
    if url.is_tls() {
        #[cfg(feature = "tls")]
        {
            let s = crate::net::tls::connect(tcp, &url.host, None)
                .map_err(|e| StoreError::Io(format!("tls {}: {e}", url.host)))?;
            return Ok(Box::new(s));
        }
        #[cfg(not(feature = "tls"))]
        {
            return Err(StoreError::Io("https store requires --features tls".into()));
        }
    }
    Ok(Box::new(tcp))
}

impl Store for HttpStore {
    fn put(&self, key: &str, seq: u64, envelope: &Value) -> Result<PutOutcome, StoreError> {
        let op = self.cfg.put.as_ref().expect("validated");
        let vars = self.vars(key, Some(seq), Some(envelope));
        let (status, ctx) = self.request(op, &vars, Some(format!("{key}#{seq}")))?;
        let conflict = op.conflict_status.unwrap_or(409);
        if status == conflict {
            let latest = ctx["body"]
                .get("latest")
                .or_else(|| ctx["body"].get("seq"))
                .and_then(Value::as_u64);
            return Ok(PutOutcome::Conflict { latest_seq: latest });
        }
        if (200..300).contains(&status) {
            return Ok(PutOutcome::Ok);
        }
        Err(StoreError::Io(format!(
            "put {key}: HTTP {status}: {}",
            ctx["body"]
        )))
    }

    fn get(&self, key: &str, seq: Option<u64>) -> Result<Option<Value>, StoreError> {
        let op = self.cfg.get.as_ref().expect("validated");
        let vars = self.vars(key, seq, None);
        let (status, ctx) = self.request(op, &vars, None)?;
        if status == 404 {
            return Ok(None);
        }
        if !(200..300).contains(&status) {
            return Err(StoreError::Io(format!("get {key}: HTTP {status}")));
        }
        let v = match &op.value {
            Some(x) => mapping::extract(x, &ctx).map_err(|e| StoreError::Mapping(e.0))?,
            None => Some(ctx["body"].clone()),
        };
        Ok(v.filter(|v| !v.is_null()))
    }

    fn list(&self, prefix: &str) -> Result<Vec<KeySeq>, StoreError> {
        let Some(op) = &self.cfg.list else {
            return Err(StoreError::Unsupported("list"));
        };
        let mut vars = self.vars(prefix, None, None);
        vars.insert("prefix".into(), Value::String(prefix.to_string()));
        let (status, ctx) = self.request(op, &vars, None)?;
        if !(200..300).contains(&status) {
            return Err(StoreError::Io(format!("list {prefix}: HTTP {status}")));
        }
        let keys = match &op.keys {
            Some(x) => mapping::extract(x, &ctx).map_err(|e| StoreError::Mapping(e.0))?,
            None => Some(ctx["body"]["keys"].clone()),
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
        let Some(op) = &self.cfg.delete else {
            return Err(StoreError::Unsupported("delete"));
        };
        let vars = self.vars(key, None, None);
        let (status, _) = self.request(op, &vars, None)?;
        if (200..300).contains(&status) || status == 404 {
            return Ok(());
        }
        Err(StoreError::Io(format!("delete {key}: HTTP {status}")))
    }

    fn kind(&self) -> &'static str {
        "http"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::TcpListener;
    use std::sync::{Arc, Mutex};

    /// A tiny KV server: `PUT /kv/<key>?seq=N` (409 on stale seq, echoing
    /// `{"latest": n}`), `GET /kv/<key>` (404 when absent), `GET /kv?prefix=…`,
    /// `DELETE /kv/<key>`; records the Idempotency-Key + auth headers it saw.
    /// (method, path, headers) per request.
    type Seen = Arc<Mutex<Vec<(String, String, Vec<(String, String)>)>>>;

    fn spawn_kv() -> (String, Seen) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let seen: Seen = Arc::new(Mutex::new(Vec::new()));
        let seen_t = seen.clone();
        let data: Arc<Mutex<BTreeMap<String, (u64, Value)>>> =
            Arc::new(Mutex::new(BTreeMap::new()));
        std::thread::spawn(move || {
            for conn in listener.incoming() {
                let Ok(mut s) = conn else { continue };
                let mut r = BufReader::new(s.try_clone().unwrap());
                let mut line = String::new();
                if r.read_line(&mut line).unwrap_or(0) == 0 {
                    continue;
                }
                let mut parts = line.split_whitespace();
                let method = parts.next().unwrap_or("").to_string();
                let target = parts.next().unwrap_or("").to_string();
                let mut headers = Vec::new();
                let mut clen = 0usize;
                loop {
                    let mut h = String::new();
                    if r.read_line(&mut h).unwrap_or(0) == 0 {
                        break;
                    }
                    let t = h.trim_end();
                    if t.is_empty() {
                        break;
                    }
                    if let Some((k, v)) = t.split_once(':') {
                        let k = k.trim().to_ascii_lowercase();
                        if k == "content-length" {
                            clen = v.trim().parse().unwrap_or(0);
                        }
                        headers.push((k, v.trim().to_string()));
                    }
                }
                let mut body = vec![0u8; clen];
                let _ = r.read_exact(&mut body);
                seen_t
                    .lock()
                    .unwrap()
                    .push((method.clone(), target.clone(), headers));
                let (path, query) = target
                    .split_once('?')
                    .map(|(p, q)| (p.to_string(), q.to_string()))
                    .unwrap_or((target.clone(), String::new()));
                let mut d = data.lock().unwrap();
                let (status, resp): (u16, Value) = match (method.as_str(), path.as_str()) {
                    ("PUT", p) if p.starts_with("/kv/") => {
                        let key = p.trim_start_matches("/kv/").to_string();
                        let seq: u64 = query.trim_start_matches("seq=").parse().unwrap_or(0);
                        if d.get(&key).is_some_and(|(cur, _)| *cur >= seq) {
                            (409, json!({"latest": d[&key].0}))
                        } else {
                            let v: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
                            d.insert(key, (seq, v));
                            (200, json!({"ok": true}))
                        }
                    }
                    ("GET", "/kv") => {
                        let prefix = query.trim_start_matches("prefix=").to_string();
                        let keys: Vec<Value> = d
                            .iter()
                            .filter(|(k, _)| k.starts_with(&prefix))
                            .map(|(k, (s, _))| json!({"key": k, "seq": s}))
                            .collect();
                        (200, json!({"keys": keys}))
                    }
                    ("GET", p) if p.starts_with("/kv/") => {
                        match d.get(p.trim_start_matches("/kv/")) {
                            Some((_, v)) => (200, v.clone()),
                            None => (404, json!({"error": "not found"})),
                        }
                    }
                    ("DELETE", p) if p.starts_with("/kv/") => {
                        d.remove(p.trim_start_matches("/kv/"));
                        (204, Value::Null)
                    }
                    _ => (400, json!({"error": "bad"})),
                };
                let text = if resp.is_null() {
                    String::new()
                } else {
                    resp.to_string()
                };
                let head = format!(
                    "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    text.len()
                );
                let _ = s.write_all(head.as_bytes());
                let _ = s.write_all(text.as_bytes());
            }
        });
        (format!("http://{addr}"), seen)
    }

    #[test]
    fn http_store_round_trips_with_conflicts_and_headers() {
        // SAFETY: single-threaded test; unique var name.
        unsafe { std::env::set_var("HTTP_STORE_TEST_TOKEN", "t0k") };
        let (base, seen) = spawn_kv();
        let cfg = StoreHttp {
            base_url: base.clone(),
            headers: [(
                "authorization".to_string(),
                "Bearer {{secret:HTTP_STORE_TEST_TOKEN}}".to_string(),
            )]
            .into_iter()
            .collect(),
            get: Some(HttpOp {
                method: Some("GET".into()),
                url: "{base_url}/kv/{key}".into(),
                body: None,
                value: Some("body".into()),
                keys: None,
                conflict_status: None,
            }),
            put: Some(HttpOp {
                method: Some("PUT".into()),
                url: "{base_url}/kv/{key}?seq={seq}".into(),
                body: Some("{envelope}".into()),
                value: None,
                keys: None,
                conflict_status: Some(409),
            }),
            list: Some(HttpOp {
                method: Some("GET".into()),
                url: "{base_url}/kv?prefix={prefix}".into(),
                body: None,
                value: None,
                keys: Some("body.keys".into()),
                conflict_status: None,
            }),
            delete: Some(HttpOp {
                method: Some("DELETE".into()),
                url: "{base_url}/kv/{key}".into(),
                body: None,
                value: None,
                keys: None,
                conflict_status: None,
            }),
        };
        let s = HttpStore::new(cfg, Duration::from_secs(5)).unwrap();
        let env = json!({"v": 2, "kind": "run", "id": "1", "seq": 1, "state": {"x": 1}});
        assert_eq!(s.put("agentd/i/run/1", 1, &env).unwrap(), PutOutcome::Ok);
        assert_eq!(
            s.put("agentd/i/run/1", 1, &env).unwrap(),
            PutOutcome::Conflict {
                latest_seq: Some(1)
            }
        );
        assert_eq!(s.get("agentd/i/run/1", None).unwrap(), Some(env.clone()));
        assert_eq!(s.get("agentd/i/run/2", None).unwrap(), None);
        let l = s.list("agentd/i/").unwrap();
        assert_eq!(l.len(), 1);
        assert_eq!(l[0].seq, Some(1));
        s.delete("agentd/i/run/1").unwrap();
        assert_eq!(s.get("agentd/i/run/1", None).unwrap(), None);
        // The auth header (secret resolved) and the idempotency key were sent.
        let seen = seen.lock().unwrap();
        let (m, t, h) = &seen[0];
        assert_eq!(m, "PUT");
        assert!(t.contains("seq=1"));
        assert!(
            h.iter()
                .any(|(k, v)| k == "authorization" && v == "Bearer t0k"),
            "{h:?}"
        );
        assert!(
            h.iter()
                .any(|(k, v)| k == "idempotency-key" && v == "agentd/i/run/1#1"),
            "{h:?}"
        );
        unsafe { std::env::remove_var("HTTP_STORE_TEST_TOKEN") };
    }
}
