// SPDX-License-Identifier: AGPL-3.0-only
//! Core MCP authorization: the OAuth pieces agentd is a client of, built once
//! for RFC 0047 P0 and RFC 0044 P0 (rfcs/0047 §12). This family is the one
//! RFC 0044 defines; RFC 0047 P0 opens it with the pieces it built, and RFC
//! 0044 adds the rest.
//!
//! Each check runs `agentd --login` or `--logout` — the binary, black-box —
//! against an authorization server this module serves on plain HTTP
//! loopback, the shape of a development setup, which records every request
//! it receives: where agentd looked for metadata and in what order, what it
//! refused, and what it revoked.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::json;

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
            id: "mcp-auth-core/logout-revokes-at-the-server",
            category: Category::McpAuthCore,
            desc: "--logout after a login revokes the refresh token at the revocation_endpoint the metadata names (RFC 7009: token and token_type_hint=refresh_token)",
            run: logout_revokes,
        },
    ]
}

/// One request the authorization server received.
#[derive(Debug, Clone)]
struct Seen {
    method: String,
    path: String,
    body: String,
}

/// An authorization server and an MCP endpoint on one loopback port.
struct Server {
    base: String,
    seen: Arc<Mutex<Vec<Seen>>>,
}

impl Server {
    fn issuer(&self) -> String {
        format!("{}/tenant", self.base)
    }

    fn seen(&self) -> Vec<Seen> {
        self.seen.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    fn paths(&self) -> Vec<String> {
        self.seen().into_iter().map(|s| s.path).collect()
    }
}

/// Serve the authorization server at issuer `<base>/tenant`: its RFC 8414
/// metadata only at the path-inserted URL, naming that issuer — or, with
/// `mismatch`, the issuer with a trailing `/`; a device grant answered
/// `authorization_pending` once, then with tokens; RFC 7009 revocation; and
/// an MCP endpoint at `/team/mcp` that answers every request `401` with no
/// `resource_metadata`, its RFC 9728 metadata only at the path-inserted URL.
fn serve(mismatch: bool) -> Server {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let base = format!("http://{}", listener.local_addr().expect("address"));
    let seen = Arc::new(Mutex::new(Vec::new()));
    let (log, issuer) = (Arc::clone(&seen), format!("{base}/tenant"));
    let named = if mismatch {
        format!("{issuer}/")
    } else {
        issuer.clone()
    };
    let at = base.clone();
    std::thread::spawn(move || {
        let mut polls = 0;
        for conn in listener.incoming().flatten() {
            conn.set_read_timeout(Some(Duration::from_secs(5))).ok();
            let mut reader = BufReader::new(conn);
            let mut line = String::new();
            if reader.read_line(&mut line).unwrap_or(0) == 0 {
                continue;
            }
            let mut words = line.split_whitespace();
            let method = words.next().unwrap_or_default().to_string();
            let path = words.next().unwrap_or_default().to_string();
            let mut len = 0usize;
            loop {
                let mut h = String::new();
                if reader.read_line(&mut h).unwrap_or(0) == 0 || h.trim().is_empty() {
                    break;
                }
                if let Some((k, v)) = h.split_once(':')
                    && k.trim().eq_ignore_ascii_case("content-length")
                {
                    len = v.trim().parse().unwrap_or(0);
                }
            }
            let mut body = vec![0u8; len];
            reader.read_exact(&mut body).ok();
            let body = String::from_utf8_lossy(&body).into_owned();
            log.lock().unwrap_or_else(|e| e.into_inner()).push(Seen {
                method: method.clone(),
                path: path.clone(),
                body,
            });
            let (status, extra, payload) = match (method.as_str(), path.as_str()) {
                ("GET", "/.well-known/oauth-authorization-server/tenant") => (
                    "200 OK",
                    String::new(),
                    json!({
                        "issuer": named,
                        "token_endpoint": format!("{issuer}/token"),
                        "device_authorization_endpoint": format!("{issuer}/device"),
                        "revocation_endpoint": format!("{issuer}/revoke"),
                    })
                    .to_string(),
                ),
                ("POST", "/tenant/device") => (
                    "200 OK",
                    String::new(),
                    json!({"device_code": "dev-1", "user_code": "WDJB-MJHT", "verification_uri": format!("{issuer}/verify"), "interval": 1, "expires_in": 300}).to_string(),
                ),
                ("POST", "/tenant/token") => {
                    polls += 1;
                    if polls == 1 {
                        ("400 Bad Request", String::new(), json!({"error": "authorization_pending"}).to_string())
                    } else {
                        (
                            "200 OK",
                            String::new(),
                            json!({"access_token": "at-1", "refresh_token": "rt-1", "token_type": "Bearer", "expires_in": 3600}).to_string(),
                        )
                    }
                }
                ("POST", "/tenant/revoke") => ("200 OK", String::new(), String::new()),
                ("POST", "/team/mcp") => (
                    "401 Unauthorized",
                    "WWW-Authenticate: Bearer realm=\"mcp\"\r\n".to_string(),
                    String::new(),
                ),
                ("GET", "/.well-known/oauth-protected-resource/team/mcp") => (
                    "200 OK",
                    String::new(),
                    json!({"resource": format!("{at}/team/mcp"), "authorization_servers": [issuer]}).to_string(),
                ),
                _ => ("404 Not Found", String::new(), String::new()),
            };
            let mut s = reader.into_inner();
            let _ = write!(
                s,
                "HTTP/1.1 {status}\r\nContent-Type: application/json\r\n{extra}Content-Length: {}\r\nConnection: close\r\n\r\n{payload}",
                payload.len()
            );
            let _ = s.flush();
        }
    });
    Server { base, seen }
}

/// A configuration whose MCP server `test` is at the server's `/team/mcp`,
/// with a device-grant login at `issuer`, or none.
fn config(tmp: &TempDir, server: &Server, issuer: Option<&str>) -> String {
    let issuer = issuer
        .map(|i| format!("        issuer: {i}\n"))
        .unwrap_or_default();
    write_file(
        tmp,
        "login.yaml",
        &format!(
            "\
             agent:\n  name: login\n  instruction: x\n  preflight: never\n\
             intelligence:\n  endpoints: [https://api.openai.com/v1]\n  model: m\n\
             store:\n  kind: memory\n\
             mcp:\n  servers:\n    - name: test\n      endpoint: {}/team/mcp\n\
             \x20     auth:\n        kind: oauth2\n        grant: device\n{issuer}\
             \x20       client_id: agentd-conformance\n\
             lifecycle:\n  run_until: idle\n",
            server.base
        ),
    )
}

/// Run `agentd <verb> mcp:test` over `cfg`, its credentials under `tmp`.
fn run(h: &Harness, tmp: &TempDir, verb: &str, cfg: &str) -> crate::harness::RunResult {
    let creds = tmp.path().join("creds");
    let _ = std::fs::create_dir_all(&creds);
    h.run_env(
        &[verb, "mcp:test", "--config", cfg],
        &[("AGENTD_CRED_DIR", creds.to_str().unwrap_or_default())],
    )
}

fn metadata_path_inserted_first(h: &Harness) -> Outcome {
    let tmp = h.tempdir();
    let server = serve(false);
    let cfg = config(&tmp, &server, Some(&server.issuer()));
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
    let server = serve(true);
    let cfg = config(&tmp, &server, Some(&server.issuer()));
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
    let server = serve(false);
    let cfg = config(&tmp, &server, None);
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

fn logout_revokes(h: &Harness) -> Outcome {
    let tmp = h.tempdir();
    let server = serve(false);
    let cfg = config(&tmp, &server, Some(&server.issuer()));
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
