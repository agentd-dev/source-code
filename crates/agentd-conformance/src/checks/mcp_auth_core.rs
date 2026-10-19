// SPDX-License-Identifier: AGPL-3.0-only
//! Core MCP authorization: the OAuth pieces agentd is a client of, built for
//! RFC 0047 P0 and RFC 0044 P0 (rfcs/0047 §12, rfcs/0044 §7). This family is
//! the one RFC 0044 defines: RFC 0047 P0 opened it with the pieces it built,
//! and RFC 0044 P0 adds the rest.
//!
//! Each check runs `agentd --login` or `--logout`, or the daemon itself — the
//! binary, black-box — against an authorization server and an MCP server
//! this module serves on plain HTTP loopback, the shape of a development
//! setup, which records every request it receives: where agentd looked for
//! metadata and in what order, what it sent to the token endpoint, what it
//! refused, and what it revoked.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::checks::util::write_file;
use crate::harness::TempDir;
use crate::{Category, Check, Harness, Outcome};

pub fn checks() -> Vec<Check> {
    vec![
        Check {
            id: "mcp-auth-core/as-metadata-path-inserted-first",
            category: Category::McpAuthCore,
            desc: "for an issuer with a path, --login reads RFC 8414 metadata first, at the well-known URL with the suffix inserted before the path, and completes the device grant with the endpoints it names",
            run: metadata_path_inserted_first,
        },
        Check {
            id: "mcp-auth-core/as-issuer-must-match",
            category: Category::McpAuthCore,
            desc: "metadata naming another issuer than the configured one, by a trailing slash, is refused: --login fails and sends no device or token request",
            run: issuer_must_match,
        },
        Check {
            id: "mcp-auth-core/resource-metadata-path-inserted-first",
            category: Category::McpAuthCore,
            desc: "with no issuer configured, --login asks the MCP server, then reads its RFC 9728 metadata at the well-known URL with the suffix inserted before the server's path, before the root, and uses the issuer it names",
            run: resource_metadata_path_inserted_first,
        },
        Check {
            id: "mcp-auth-core/resource-metadata-must-name-the-server",
            category: Category::McpAuthCore,
            desc: "RFC 9728 metadata whose `resource` is not the MCP server's URL is refused: --login fails, names RFC 9728 §3.3, and asks the authorization server it names for nothing",
            run: resource_metadata_must_name_the_server,
        },
        Check {
            id: "mcp-auth-core/server-chosen-urls-are-https-and-public",
            category: Category::McpAuthCore,
            desc: "an authorization server an MCP server's RFC 9728 metadata names off the server's own origin is refused when it is plain http:// or a link-local address",
            run: server_chosen_urls_are_https_and_public,
        },
        Check {
            id: "mcp-auth-core/resource-on-every-grant",
            category: Category::McpAuthCore,
            desc: "RFC 8707: the device authorization, the device token poll, the daemon's refresh and a client-credentials token request to an MCP server's authorization server all carry `resource` — the server's URL in canonical form",
            run: resource_on_every_grant,
        },
        Check {
            id: "mcp-auth-core/rfc9207-iss-table",
            category: Category::McpAuthCore,
            desc: "the authorization-code login decides every RFC 9207 row — `iss` equal, different, or absent, with the metadata declaring support or not — refuses metadata without S256 before handing out a URL, and refuses an ID token without the login's nonce",
            run: rfc9207_iss_table,
        },
        Check {
            id: "mcp-auth-core/insufficient-scope-union-step-up",
            category: Category::McpAuthCore,
            desc: "a `403 insufficient_scope` naming a scope the token lacks is answered by a renewal asking for the union of the held and the challenged scopes, and the call goes through with the new token",
            run: insufficient_scope_union_step_up,
        },
        Check {
            id: "mcp-auth-core/rotation-survives-a-restart",
            category: Category::McpAuthCore,
            desc: "a refresh token the authorization server rotates is kept: the daemon after a restart renews from the rotated token, never the spent one",
            run: rotation_survives_a_restart,
        },
        Check {
            id: "mcp-auth-core/google-shape-cross-origin-token-endpoint",
            category: Category::McpAuthCore,
            desc: "metadata of an operator-pinned issuer that puts its token and device endpoints on another origin (Google's documented shape) is used as declared",
            run: google_shape,
        },
        Check {
            id: "mcp-auth-core/entra-shape-no-resource-on-intelligence",
            category: Category::McpAuthCore,
            desc: "an intelligence connection's oauth2 login sends no `resource` unless told to (Microsoft Entra refuses the parameter, AADSTS901002)",
            run: entra_shape,
        },
        Check {
            id: "mcp-auth-core/logout-revokes-at-the-server",
            category: Category::McpAuthCore,
            desc: "--logout after a login revokes the refresh token at the revocation_endpoint the metadata names (RFC 7009: token and token_type_hint=refresh_token)",
            run: logout_revokes,
        },
    ]
}

/// One request the mock received.
#[derive(Debug, Clone)]
struct Seen {
    method: String,
    path: String,
    body: String,
}

impl Seen {
    /// A form field of the body, percent-decoded.
    fn field(&self, k: &str) -> Option<String> {
        self.body
            .split('&')
            .find_map(|kv| kv.strip_prefix(&format!("{k}=")))
            .map(decode)
    }
}

/// What a check makes the mock do.
#[derive(Clone)]
struct Knobs {
    /// The metadata's `issuer` names the issuer with a trailing `/`.
    mismatch: bool,
    /// `authorization_response_iss_parameter_supported`.
    iss_supported: bool,
    /// `code_challenge_methods_supported`; `None` leaves it out.
    pkce: Option<Vec<&'static str>>,
    /// The RFC 9728 document's `resource`, when not the server's URL.
    prm_resource: Option<String>,
    /// The authorization server the RFC 9728 document names, when not this
    /// one's issuer.
    prm_as: Option<String>,
    /// The base the token and device endpoints are on, when not this one.
    endpoints_at: Option<String>,
    /// How long a token the device, code and client-credentials grants
    /// issue lives; a refresh's lives an hour.
    expires_in: u64,
    /// The scope `tools/call` needs.
    tool_scope: Option<&'static str>,
    /// The scopes a refresh may ask for.
    grantable: Vec<&'static str>,
    /// The `nonce` the code grant's ID token carries; `None` issues none.
    id_token_nonce: Option<String>,
}

impl Default for Knobs {
    fn default() -> Knobs {
        Knobs {
            mismatch: false,
            iss_supported: false,
            pkce: Some(vec!["S256"]),
            prm_resource: None,
            prm_as: None,
            endpoints_at: None,
            expires_in: 3600,
            tool_scope: None,
            grantable: vec!["read"],
            id_token_nonce: None,
        }
    }
}

/// What the authorization server holds.
#[derive(Default)]
struct Held {
    polls: u32,
    /// The generation of the one refresh token it accepts.
    generation: u32,
    /// Each access token's scope.
    scopes: HashMap<String, String>,
}

/// An authorization server and an MCP endpoint on one loopback port.
struct Server {
    base: String,
    seen: Arc<Mutex<Vec<Seen>>>,
    knobs: Arc<Mutex<Knobs>>,
}

impl Server {
    fn issuer(&self) -> String {
        format!("{}/tenant", self.base)
    }

    fn endpoint(&self) -> String {
        format!("{}/team/mcp", self.base)
    }

    fn seen(&self) -> Vec<Seen> {
        self.seen.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    fn paths(&self) -> Vec<String> {
        self.seen().into_iter().map(|s| s.path).collect()
    }

    /// The token endpoint's forms of grant `grant`.
    fn grants(&self, grant: &str) -> Vec<Seen> {
        self.seen()
            .into_iter()
            .filter(|s| s.path.ends_with("/tenant/token"))
            .filter(|s| s.field("grant_type").as_deref() == Some(grant))
            .collect()
    }

    fn set(&self, f: impl FnOnce(&mut Knobs)) {
        f(&mut self.knobs.lock().unwrap_or_else(|e| e.into_inner()));
    }
}

/// Serve the authorization server at issuer `<base>/tenant` and an MCP
/// server at `<base>/team/mcp`, as `knobs` say: RFC 8414 metadata only at
/// the path-inserted URL; a device grant answered `authorization_pending`
/// once, then with tokens; the code, refresh (rotating, and granting what
/// `grantable` allows) and client-credentials grants; RFC 7009 revocation;
/// RFC 9728 metadata at the path-inserted URL; and an MCP server that
/// answers `401` to a request without one of its tokens.
fn serve(knobs: Knobs) -> Server {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let base = format!("http://{}", listener.local_addr().expect("address"));
    let seen = Arc::new(Mutex::new(Vec::new()));
    let knobs = Arc::new(Mutex::new(knobs));
    let (log, k, at) = (Arc::clone(&seen), Arc::clone(&knobs), base.clone());
    let held = Arc::new(Mutex::new(Held::default()));
    std::thread::spawn(move || {
        for conn in listener.incoming().flatten() {
            let (log, k, at, held) = (
                Arc::clone(&log),
                Arc::clone(&k),
                at.clone(),
                Arc::clone(&held),
            );
            std::thread::spawn(move || answer(conn, &log, &k, &at, &held));
        }
    });
    Server { base, seen, knobs }
}

/// One connection's request, answered.
fn answer(
    conn: std::net::TcpStream,
    log: &Mutex<Vec<Seen>>,
    knobs: &Mutex<Knobs>,
    base: &str,
    held: &Mutex<Held>,
) {
    conn.set_read_timeout(Some(Duration::from_secs(5))).ok();
    let mut reader = BufReader::new(conn);
    let mut line = String::new();
    if reader.read_line(&mut line).unwrap_or(0) == 0 {
        return;
    }
    let mut words = line.split_whitespace();
    let method = words.next().unwrap_or_default().to_string();
    let path = words.next().unwrap_or_default().to_string();
    let (mut len, mut bearer) = (0usize, None);
    loop {
        let mut h = String::new();
        if reader.read_line(&mut h).unwrap_or(0) == 0 || h.trim().is_empty() {
            break;
        }
        if let Some((name, v)) = h.split_once(':') {
            if name.trim().eq_ignore_ascii_case("content-length") {
                len = v.trim().parse().unwrap_or(0);
            }
            if name.trim().eq_ignore_ascii_case("authorization") {
                bearer = v.trim().strip_prefix("Bearer ").map(str::to_string);
            }
        }
    }
    let mut body = vec![0u8; len];
    reader.read_exact(&mut body).ok();
    let body = String::from_utf8_lossy(&body).into_owned();
    let seen = Seen {
        method: method.clone(),
        path: path.clone(),
        body: body.clone(),
    };
    log.lock()
        .unwrap_or_else(|e| e.into_inner())
        .push(seen.clone());
    let k = knobs.lock().unwrap_or_else(|e| e.into_inner()).clone();
    let issuer = format!("{base}/tenant");
    let mut held = held.lock().unwrap_or_else(|e| e.into_inner());
    let ok = |v: Value| ("200 OK", String::new(), v.to_string());
    let error = |status: &'static str, code: &str| {
        (status, String::new(), json!({"error": code}).to_string())
    };
    let issue = |held: &mut Held, scope: &str, id_token: Option<String>, life: u64| {
        held.generation += 1;
        let n = held.generation;
        held.scopes.insert(format!("at-{n}"), scope.to_string());
        let mut t = json!({"access_token": format!("at-{n}"), "refresh_token": format!("rt-{n}"),
                           "token_type": "Bearer", "expires_in": life, "scope": scope});
        if let Some(i) = id_token {
            t["id_token"] = json!(i);
        }
        ok(t)
    };
    let (status, extra, payload) = match (method.as_str(), path.as_str()) {
        ("GET", "/.well-known/oauth-authorization-server/tenant") => {
            let at = k.endpoints_at.clone().unwrap_or_else(|| base.to_string());
            let mut m = json!({
                "issuer": if k.mismatch { format!("{issuer}/") } else { issuer.clone() },
                "token_endpoint": format!("{at}/tenant/token"),
                "device_authorization_endpoint": format!("{at}/tenant/device"),
                "authorization_endpoint": format!("{issuer}/authorize"),
                "revocation_endpoint": format!("{issuer}/revoke"),
                "authorization_response_iss_parameter_supported": k.iss_supported,
            });
            if let Some(p) = &k.pkce {
                m["code_challenge_methods_supported"] = json!(p);
            }
            ok(m)
        }
        ("POST", "/tenant/device") => ok(json!({"device_code": "dev-1", "user_code": "WDJB-MJHT",
            "verification_uri": format!("{issuer}/verify"), "interval": 1, "expires_in": 300})),
        ("POST", "/tenant/token") => match seen.field("grant_type").as_deref() {
            Some("urn:ietf:params:oauth:grant-type:device_code") => {
                held.polls += 1;
                if held.polls == 1 {
                    error("400 Bad Request", "authorization_pending")
                } else {
                    issue(&mut held, "read", None, k.expires_in)
                }
            }
            Some("authorization_code") => {
                let id = k.id_token_nonce.as_ref().map(|n| {
                    let claims = json!({"iss": issuer, "aud": "agentd-conformance", "nonce": n,
                                        "sub": "u1", "exp": 4_102_444_800u64});
                    format!("e30.{}.sig", b64url(claims.to_string().as_bytes()))
                });
                issue(&mut held, "read", id, k.expires_in)
            }
            Some("refresh_token") => {
                let want = format!("rt-{}", held.generation);
                let scope = seen.field("scope").unwrap_or_else(|| "read".into());
                if seen.field("refresh_token").as_deref() != Some(want.as_str()) {
                    error("400 Bad Request", "invalid_grant")
                } else if !scope.split(' ').all(|s| k.grantable.contains(&s)) {
                    error("400 Bad Request", "invalid_scope")
                } else {
                    issue(&mut held, &scope, None, 3600)
                }
            }
            Some("client_credentials") => issue(&mut held, "read", None, k.expires_in),
            _ => error("400 Bad Request", "unsupported_grant_type"),
        },
        ("POST", "/tenant/revoke") => ("200 OK", String::new(), String::new()),
        ("GET", "/.well-known/oauth-protected-resource/team/mcp") => ok(json!({
            "resource": k.prm_resource.clone().unwrap_or_else(|| format!("{base}/team/mcp")),
            "authorization_servers": [k.prm_as.clone().unwrap_or(issuer.clone())],
        })),
        ("POST", "/team/mcp") => {
            let scope = bearer.as_ref().and_then(|b| held.scopes.get(b)).cloned();
            match scope {
                None => (
                    "401 Unauthorized",
                    "WWW-Authenticate: Bearer realm=\"mcp\"\r\n".to_string(),
                    String::new(),
                ),
                Some(scope) => mcp(&body, &scope, k.tool_scope),
            }
        }
        _ => ("404 Not Found", String::new(), String::new()),
    };
    drop(held);
    let mut s = reader.into_inner();
    let _ = write!(
        s,
        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\n{extra}Content-Length: {}\r\nConnection: close\r\n\r\n{payload}",
        payload.len()
    );
    let _ = s.flush();
}

/// The MCP server's answer to `body`, for a request whose token holds
/// `scope`: one tool, `t`, that needs `tool_scope`.
fn mcp(body: &str, scope: &str, tool_scope: Option<&str>) -> (&'static str, String, String) {
    let req: Value = serde_json::from_str(body).unwrap_or_default();
    let id = req["id"].clone();
    let result = |r: Value| {
        (
            "200 OK",
            String::new(),
            json!({"jsonrpc": "2.0", "id": id, "result": r}).to_string(),
        )
    };
    match req["method"].as_str().unwrap_or_default() {
        "initialize" => result(json!({"protocolVersion": "2025-11-25",
            "capabilities": {"tools": {}}, "serverInfo": {"name": "auth-core", "version": "0"}})),
        "tools/list" => result(json!({"tools": [{"name": "t", "inputSchema": {"type": "object"}}]})),
        "tools/call" => match tool_scope {
            Some(need) if !scope.split(' ').any(|s| s == need) => (
                "403 Forbidden",
                format!("WWW-Authenticate: Bearer error=\"insufficient_scope\", scope=\"{need}\"\r\n"),
                String::new(),
            ),
            _ => result(json!({"content": [{"type": "text", "text": "done"}], "isError": false})),
        },
        m if m.starts_with("notifications/") => ("202 Accepted", String::new(), String::new()),
        _ => (
            "200 OK",
            String::new(),
            json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32601, "message": "method not found"}})
                .to_string(),
        ),
    }
}

/// Percent-decode a form value.
fn decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < b.len() => {
                let h = std::str::from_utf8(&b[i + 1..i + 3])
                    .ok()
                    .and_then(|h| u8::from_str_radix(h, 16).ok());
                match h {
                    Some(v) => {
                        out.push(v);
                        i += 3;
                    }
                    None => {
                        out.push(b[i]);
                        i += 1;
                    }
                }
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// base64url without padding.
fn b64url(input: &[u8]) -> String {
    const A: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::new();
    for chunk in input.chunks(3) {
        let n = (chunk[0] as u32) << 16
            | (*chunk.get(1).unwrap_or(&0) as u32) << 8
            | *chunk.get(2).unwrap_or(&0) as u32;
        out.push(A[(n >> 18 & 63) as usize] as char);
        out.push(A[(n >> 12 & 63) as usize] as char);
        if chunk.len() > 1 {
            out.push(A[(n >> 6 & 63) as usize] as char);
        }
        if chunk.len() > 2 {
            out.push(A[(n & 63) as usize] as char);
        }
    }
    out
}

/// A configuration whose MCP server `test` is the mock's, its `auth` block
/// the oauth2 fields in `auth` (YAML, one level in), and a workflow that
/// calls the server's tool `t` once when `call`.
fn config(tmp: &TempDir, server: &Server, auth: &str, call: bool) -> String {
    let workflow = if call {
        "workflows:\n  - name: w\n    steps:\n\
         \x20     s: {kind: once}\n\
         \x20     c: {kind: mcp.tool, depends_on: [s], server: test, tool: t, args: {}}\n\
         \x20     f: {kind: finish, depends_on: [c], status: completed}\n"
    } else {
        ""
    };
    write_file(
        tmp,
        "auth.yaml",
        &format!(
            "\
             agent:\n  name: auth-core\n  instruction: x\n  preflight: never\n\
             intelligence:\n  endpoints: [https://api.openai.com/v1]\n  model: m\n\
             store:\n  kind: memory\n\
             mcp:\n  servers:\n    - name: test\n      endpoint: {}\n\
             \x20     auth:\n        kind: oauth2\n        client_id: agentd-conformance\n{auth}\
             {workflow}\
             lifecycle:\n  run_until: idle\n  idle_grace: 1s\n\
             observability:\n  log_level: info\n",
            server.endpoint()
        ),
    )
}

/// The `auth` lines of a device-grant login at `issuer`, or with none.
fn device_at(issuer: Option<&str>) -> String {
    let mut out = "        grant: device\n".to_string();
    if let Some(i) = issuer {
        out.push_str(&format!("        issuer: {i}\n"));
    }
    out
}

/// Run `agentd <verb> <target>` over `cfg`, its credentials under `tmp`.
fn run_target(
    h: &Harness,
    tmp: &TempDir,
    verb: &str,
    target: &str,
    cfg: &str,
) -> crate::harness::RunResult {
    let creds = tmp.path().join("creds");
    let _ = std::fs::create_dir_all(&creds);
    h.run_env(
        &[verb, target, "--config", cfg],
        &[("AGENTD_CRED_DIR", creds.to_str().unwrap_or_default())],
    )
}

/// Run `agentd <verb> mcp:test` over `cfg`.
fn run(h: &Harness, tmp: &TempDir, verb: &str, cfg: &str) -> crate::harness::RunResult {
    run_target(h, tmp, verb, "mcp:test", cfg)
}

/// Run the daemon over `cfg`, its credentials under `tmp`, to its end.
fn daemon(
    h: &Harness,
    tmp: &TempDir,
    cfg: &str,
    env: &[(&str, &str)],
) -> crate::harness::RunResult {
    let creds = tmp.path().join("creds");
    let _ = std::fs::create_dir_all(&creds);
    let mut all = vec![("AGENTD_CRED_DIR", creds.to_str().unwrap_or_default())];
    all.extend_from_slice(env);
    h.run_env(&["--config", cfg], &all)
}

/// Mark every login under `tmp` as due, as an hour passing would.
fn expire_logins(tmp: &TempDir) {
    let Ok(dir) = std::fs::read_dir(tmp.path().join("creds")) else {
        return;
    };
    for e in dir.flatten() {
        let p = e.path();
        if p.extension().is_some_and(|x| x == "json")
            && let Ok(text) = std::fs::read_to_string(&p)
            && let Ok(mut v) = serde_json::from_str::<Value>(&text)
        {
            v["expires_at_ms"] = json!(1);
            let _ = std::fs::write(&p, v.to_string());
        }
    }
}

/// Whether a daemon run's workflow `w` completed.
fn completed(out: &crate::harness::RunResult) -> bool {
    out.events()
        .iter()
        .any(|e| e["event"] == "run.done" && e["workflow"] == "w" && e["status"] == "completed")
}

fn metadata_path_inserted_first(h: &Harness) -> Outcome {
    let tmp = h.tempdir();
    let server = serve(Knobs::default());
    let cfg = config(&tmp, &server, &device_at(Some(&server.issuer())), false);
    let out = run(h, &tmp, "--login", &cfg);
    let paths = server.paths();
    Outcome::require(
        out.code == Some(0),
        format!("--login exit {:?}: {}", out.code, out.stderr),
    )
    .and(|| {
        Outcome::require(
            paths.first().map(String::as_str)
                == Some("/.well-known/oauth-authorization-server/tenant"),
            format!("the first request was not RFC 8414's path-inserted metadata: {paths:?}"),
        )
    })
    .and(|| {
        Outcome::require(
            paths.iter().any(|p| p == "/tenant/device")
                && paths.iter().any(|p| p == "/tenant/token"),
            format!("the device grant did not use the endpoints the metadata names: {paths:?}"),
        )
    })
}

fn issuer_must_match(h: &Harness) -> Outcome {
    let tmp = h.tempdir();
    let server = serve(Knobs {
        mismatch: true,
        ..Knobs::default()
    });
    let cfg = config(&tmp, &server, &device_at(Some(&server.issuer())), false);
    let out = run(h, &tmp, "--login", &cfg);
    let paths = server.paths();
    Outcome::require(
        out.code != Some(0),
        format!("--login succeeded against metadata naming another issuer: {paths:?}"),
    )
    .and(|| {
        Outcome::require(
            out.stderr.contains("names the issuer"),
            format!("the refusal does not say why: {}", out.stderr),
        )
    })
    .and(|| {
        Outcome::require(
            !paths.iter().any(|p| p.starts_with("/tenant/")),
            format!("a request went to an endpoint of the mismatched metadata: {paths:?}"),
        )
    })
}

fn resource_metadata_path_inserted_first(h: &Harness) -> Outcome {
    let tmp = h.tempdir();
    let server = serve(Knobs::default());
    let cfg = config(&tmp, &server, &device_at(None), false);
    let out = run(h, &tmp, "--login", &cfg);
    let paths = server.paths();
    let prm = paths
        .iter()
        .position(|p| p.starts_with("/.well-known/oauth-protected-resource"));
    Outcome::require(
        out.code == Some(0),
        format!("--login exit {:?}: {}", out.code, out.stderr),
    )
    .and(|| {
        Outcome::require(
            paths.first().map(String::as_str) == Some("/team/mcp"),
            format!("agentd did not ask the MCP server first: {paths:?}"),
        )
    })
    .and(|| {
        Outcome::require(
            prm.map(|i| paths[i].as_str())
                == Some("/.well-known/oauth-protected-resource/team/mcp"),
            format!("the first RFC 9728 request was not the path-inserted URL: {paths:?}"),
        )
    })
    .and(|| {
        Outcome::require(
            paths
                .iter()
                .any(|p| p == "/.well-known/oauth-authorization-server/tenant"),
            format!("the issuer the metadata names was not used: {paths:?}"),
        )
    })
}

fn resource_metadata_must_name_the_server(h: &Harness) -> Outcome {
    let tmp = h.tempdir();
    let server = serve(Knobs::default());
    server.set(|k| k.prm_resource = Some(format!("{}/elsewhere", server.base)));
    let cfg = config(&tmp, &server, &device_at(None), false);
    let out = run(h, &tmp, "--login", &cfg);
    let paths = server.paths();
    Outcome::require(
        out.code != Some(0),
        format!("--login used metadata describing another resource: {paths:?}"),
    )
    .and(|| {
        Outcome::require(
            out.stderr.contains("RFC 9728 §3.3"),
            format!("the refusal does not say why: {}", out.stderr),
        )
    })
    .and(|| {
        Outcome::require(
            !paths
                .iter()
                .any(|p| p.contains("oauth-authorization-server")),
            format!("the authorization server it names was asked for metadata: {paths:?}"),
        )
    })
}

fn server_chosen_urls_are_https_and_public(h: &Harness) -> Outcome {
    let mut out = Outcome::pass();
    for (named, why) in [
        ("http://203.0.113.9/tenant", "not an https URL"),
        ("https://169.254.169.254/latest", "link-local"),
    ] {
        let tmp = h.tempdir();
        let server = serve(Knobs::default());
        server.set(|k| k.prm_as = Some(named.to_string()));
        let cfg = config(&tmp, &server, &device_at(None), false);
        let r = run(h, &tmp, "--login", &cfg);
        out = out.and(|| {
            Outcome::require(
                r.code != Some(0) && r.stderr.contains(why),
                format!(
                    "an authorization server at {named} that the MCP server named was not refused ({why}): exit {:?}: {}",
                    r.code, r.stderr
                ),
            )
        });
    }
    out
}

fn resource_on_every_grant(h: &Harness) -> Outcome {
    let tmp = h.tempdir();
    let server = serve(Knobs {
        expires_in: 30,
        ..Knobs::default()
    });
    let want = server.endpoint();
    let cfg = config(&tmp, &server, &device_at(Some(&server.issuer())), true);
    let login = run(h, &tmp, "--login", &cfg);
    if login.code != Some(0) {
        return Outcome::fail(format!("--login exit {:?}: {}", login.code, login.stderr));
    }
    // The token it holds is due: the daemon renews it before its first call.
    let ran = daemon(h, &tmp, &cfg, &[]);
    let device = server
        .seen()
        .into_iter()
        .find(|s| s.path == "/tenant/device");
    let polls = server.grants("urn:ietf:params:oauth:grant-type:device_code");
    let refreshes = server.grants("refresh_token");
    // A client-credentials server, at the same authorization server.
    let tmp2 = h.tempdir();
    let cc = config(
        &tmp2,
        &server,
        &format!(
            "        grant: client_credentials\n        token_url: {}/token\n        client_secret: \"{{{{secret:AUTH_CORE_CC}}}}\"\n",
            server.issuer()
        ),
        true,
    );
    let ran_cc = daemon(h, &tmp2, &cc, &[("AUTH_CORE_CC", "s3cret")]);
    let credentials = server.grants("client_credentials");
    let has = |s: &Seen| s.field("resource").as_deref() == Some(want.as_str());
    Outcome::require(
        completed(&ran) && completed(&ran_cc),
        format!(
            "a daemon's call did not complete:\n{}\n{}",
            ran.stderr, ran_cc.stderr
        ),
    )
    .and(|| {
        Outcome::require(
            device.as_ref().is_some_and(has),
            format!("the device authorization: {device:?}"),
        )
    })
    .and(|| {
        Outcome::require(
            !polls.is_empty() && polls.iter().all(has),
            format!("the device token polls: {polls:?}"),
        )
    })
    .and(|| {
        Outcome::require(
            !refreshes.is_empty() && refreshes.iter().all(has),
            format!("the daemon's refreshes: {refreshes:?}"),
        )
    })
    .and(|| {
        Outcome::require(
            !credentials.is_empty() && credentials.iter().all(has),
            format!("the client-credentials requests: {credentials:?}"),
        )
    })
}

/// Run an authorization-code `--login` against `server`, asking for
/// `openid`, and answer its browser redirect with `iss` (`None`: none); the
/// ID token the code grant issues carries `nonce`, or — `None` — the one the
/// authorization URL sent. The exit code, the stderr, and whether an
/// authorization URL was printed.
fn code_login(
    h: &Harness,
    server: &Server,
    iss: Option<&str>,
    nonce: Option<&str>,
) -> (Option<i32>, String, bool) {
    let tmp = h.tempdir();
    let auth = format!(
        "        grant: authorization_code\n        issuer: {}\n        scopes: [openid]\n",
        server.issuer()
    );
    let cfg = config(&tmp, server, &auth, false);
    let creds = tmp.path().join("creds");
    let _ = std::fs::create_dir_all(&creds);
    let mut child = match Command::new(h.agentd())
        .args(["--login", "mcp:test", "--config", &cfg])
        .env("AGENTD_CRED_DIR", &creds)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => return (None, format!("spawn: {e}"), false),
    };
    let stderr = child.stderr.take().expect("piped");
    let (tx, rx) = std::sync::mpsc::channel::<String>();
    std::thread::spawn(move || {
        for line in BufReader::new(stderr).lines().map_while(Result::ok) {
            if tx.send(line).is_err() {
                break;
            }
        }
    });
    let mut said = String::new();
    let mut url = None;
    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline {
        match rx.recv_timeout(Duration::from_millis(100)) {
            Ok(line) => {
                said.push_str(&line);
                said.push('\n');
                if let Some(u) = line.trim().strip_prefix("│  open").map(str::trim) {
                    url = Some(u.to_string());
                    break;
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                if let Ok(Some(_)) = child.try_wait() {
                    break;
                }
            }
            Err(_) => break,
        }
    }
    let printed = url.is_some();
    if let Some(url) = url {
        let query = url.split_once('?').map(|(_, q)| q).unwrap_or_default();
        let param = |k: &str| {
            query
                .split('&')
                .find_map(|kv| kv.strip_prefix(&format!("{k}=")))
                .map(decode)
                .unwrap_or_default()
        };
        let sent_nonce = param("nonce");
        server.set(|k| {
            k.id_token_nonce = Some(nonce.map(str::to_string).unwrap_or(sent_nonce));
        });
        let mut callback = format!("{}?code=c1&state={}", param("redirect_uri"), param("state"));
        if let Some(i) = iss {
            callback.push_str(&format!("&iss={i}"));
        }
        let _ = get(&callback);
    }
    let deadline = Instant::now() + Duration::from_secs(20);
    let code = loop {
        if let Ok(Some(st)) = child.try_wait() {
            break st.code();
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            break None;
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    while let Ok(line) = rx.recv_timeout(Duration::from_millis(200)) {
        said.push_str(&line);
        said.push('\n');
    }
    (code, said, printed)
}

/// GET `url` (`http://host:port/path?query`), the answer discarded.
fn get(url: &str) -> std::io::Result<()> {
    let rest = url.trim_start_matches("http://");
    let (authority, target) = rest.split_at(rest.find('/').unwrap_or(rest.len()));
    let mut s = std::net::TcpStream::connect(authority)?;
    s.set_read_timeout(Some(Duration::from_secs(5))).ok();
    write!(
        s,
        "GET {target} HTTP/1.1\r\nHost: {authority}\r\nConnection: close\r\n\r\n"
    )?;
    let mut sink = Vec::new();
    let _ = s.read_to_end(&mut sink);
    Ok(())
}

fn rfc9207_iss_table(h: &Harness) -> Outcome {
    let mut out = Outcome::pass();
    // (declared, iss sent, accepted)
    for (declared, iss, accepted) in [
        (false, Some("same"), true),
        (true, Some("same"), true),
        (false, Some("other"), false),
        (true, Some("other"), false),
        (false, None, true),
        (true, None, false),
    ] {
        let server = serve(Knobs {
            iss_supported: declared,
            ..Knobs::default()
        });
        let named = match iss {
            Some("same") => Some(server.issuer()),
            Some(_) => Some("https://evil.example".to_string()),
            None => None,
        };
        let (code, said, _) = code_login(h, &server, named.as_deref(), None);
        let exchanged = !server.grants("authorization_code").is_empty();
        out = out.and(|| {
            Outcome::require(
                (code == Some(0)) == accepted && exchanged == accepted,
                format!(
                    "declared {declared}, iss {iss:?}: exit {code:?}, code exchanged {exchanged}, want accepted {accepted}:\n{said}"
                ),
            )
        });
    }
    // No S256 in the metadata: refused before any URL is handed out.
    let server = serve(Knobs {
        pkce: None,
        ..Knobs::default()
    });
    let (code, said, printed) = code_login(h, &server, None, None);
    out = out.and(|| {
        Outcome::require(
            code != Some(0) && !printed && said.contains("S256"),
            format!("metadata without S256 was not refused first: exit {code:?}:\n{said}"),
        )
    });
    // An ID token that does not carry the login's nonce.
    let server = serve(Knobs::default());
    let (code, said, _) = code_login(h, &server, Some(&server.issuer()), Some("forged"));
    out.and(|| {
        Outcome::require(
            code != Some(0) && said.contains("nonce"),
            format!("an ID token with another nonce was not refused: exit {code:?}:\n{said}"),
        )
    })
}

fn insufficient_scope_union_step_up(h: &Harness) -> Outcome {
    let tmp = h.tempdir();
    let server = serve(Knobs {
        tool_scope: Some("write"),
        grantable: vec!["read", "write"],
        ..Knobs::default()
    });
    let mut auth = device_at(Some(&server.issuer()));
    auth.push_str("        scopes: [read]\n");
    let cfg = config(&tmp, &server, &auth, true);
    let login = run(h, &tmp, "--login", &cfg);
    if login.code != Some(0) {
        return Outcome::fail(format!("--login exit {:?}: {}", login.code, login.stderr));
    }
    let ran = daemon(h, &tmp, &cfg, &[]);
    let refreshes = server.grants("refresh_token");
    Outcome::require(
        completed(&ran),
        format!(
            "the call did not go through after the step-up:\n{}",
            ran.stderr
        ),
    )
    .and(|| {
        Outcome::require(
            refreshes.len() == 1 && refreshes[0].field("scope").as_deref() == Some("read write"),
            format!("the renewal did not ask for the union: {refreshes:?}"),
        )
    })
}

fn rotation_survives_a_restart(h: &Harness) -> Outcome {
    let tmp = h.tempdir();
    let server = serve(Knobs {
        expires_in: 30,
        ..Knobs::default()
    });
    let cfg = config(&tmp, &server, &device_at(Some(&server.issuer())), true);
    let login = run(h, &tmp, "--login", &cfg);
    if login.code != Some(0) {
        return Outcome::fail(format!("--login exit {:?}: {}", login.code, login.stderr));
    }
    let first = daemon(h, &tmp, &cfg, &[]);
    // Time passes: the token the first life renewed is due again.
    expire_logins(&tmp);
    let second = daemon(h, &tmp, &cfg, &[]);
    let presented: Vec<String> = server
        .grants("refresh_token")
        .iter()
        .filter_map(|s| s.field("refresh_token"))
        .collect();
    Outcome::require(
        completed(&first) && completed(&second),
        format!(
            "a daemon's call did not complete:\n{}\n{}",
            first.stderr, second.stderr
        ),
    )
    .and(|| {
        Outcome::require(
            presented.len() == 2 && presented[0] != presented[1],
            format!("the restart did not renew from the rotated token: {presented:?}"),
        )
    })
}

fn google_shape(h: &Harness) -> Outcome {
    let tmp = h.tempdir();
    let tokens = serve(Knobs::default());
    let server = serve(Knobs::default());
    server.set(|k| k.endpoints_at = Some(tokens.base.clone()));
    let cfg = config(&tmp, &server, &device_at(Some(&server.issuer())), false);
    let out = run(h, &tmp, "--login", &cfg);
    let at_tokens = tokens.paths();
    Outcome::require(
        out.code == Some(0),
        format!("--login exit {:?}: {}", out.code, out.stderr),
    )
    .and(|| {
        Outcome::require(
            at_tokens.iter().any(|p| p == "/tenant/device")
                && at_tokens.iter().any(|p| p == "/tenant/token"),
            format!("the endpoints the pinned issuer declared were not used: {at_tokens:?}"),
        )
    })
}

fn entra_shape(h: &Harness) -> Outcome {
    let tmp = h.tempdir();
    let server = serve(Knobs::default());
    let cfg = write_file(
        &tmp,
        "entra.yaml",
        &format!(
            "\
             agent:\n  name: entra\n  instruction: x\n  preflight: never\n\
             intelligence:\n  endpoints: [https://entra-openai.example/openai/v1]\n  model: m\n\
             \x20 auth:\n    kind: oauth2\n    grant: device\n    issuer: {}\n    client_id: agentd-conformance\n\
             store:\n  kind: memory\n\
             lifecycle:\n  run_until: idle\n",
            server.issuer()
        ),
    );
    let out = run_target(h, &tmp, "--login", "intelligence", &cfg);
    let sent: Vec<Seen> = server
        .seen()
        .into_iter()
        .filter(|s| s.method == "POST")
        .collect();
    Outcome::require(
        out.code == Some(0),
        format!("--login intelligence exit {:?}: {}", out.code, out.stderr),
    )
    .and(|| {
        Outcome::require(
            !sent.is_empty() && sent.iter().all(|s| s.field("resource").is_none()),
            format!("an intelligence login sent `resource`: {sent:?}"),
        )
    })
}

fn logout_revokes(h: &Harness) -> Outcome {
    let tmp = h.tempdir();
    let server = serve(Knobs::default());
    let cfg = config(&tmp, &server, &device_at(Some(&server.issuer())), false);
    let login = run(h, &tmp, "--login", &cfg);
    if login.code != Some(0) {
        return Outcome::fail(format!("--login exit {:?}: {}", login.code, login.stderr));
    }
    let out = run(h, &tmp, "--logout", &cfg);
    let revoked: Vec<String> = server
        .seen()
        .into_iter()
        .filter(|s| s.method == "POST" && s.path == "/tenant/revoke")
        .map(|s| s.body)
        .collect();
    Outcome::require(
        out.code == Some(0),
        format!("--logout exit {:?}: {}", out.code, out.stderr),
    )
    .and(|| {
        Outcome::require(
            revoked.len() == 1
                && revoked[0].split('&').any(|kv| kv == "token=rt-1")
                && revoked[0]
                    .split('&')
                    .any(|kv| kv == "token_type_hint=refresh_token"),
            format!("the revocation requests: {revoked:?}"),
        )
    })
}
