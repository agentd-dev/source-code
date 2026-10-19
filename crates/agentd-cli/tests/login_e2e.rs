// SPDX-License-Identifier: AGPL-3.0-only
//! `agentd --login`, end to end: the binary runs the OAuth 2.1
//! **device-authorization** flow against a mock authorization server — device
//! request → poll (pending, then authorized) → cache the token in a per-user
//! file the daemon would read. Proves the interactive login UX works headlessly.
//! Then what the cache holds afterwards: a login written only once a renewal
//! in flight releases the target's lock, a rotated refresh token written back
//! and kept across restarts, and `agentd --logout` deleting the token here
//! before it revokes it at the server that issued it (RFC 7009).
#![cfg(all(unix, feature = "oauth"))]

mod common;

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

/// A mock OAuth authorization server: `POST /device` returns the device auth,
/// `POST /token` returns `authorization_pending` on the first poll and the tokens
/// thereafter (simulating the human authorizing between polls).
fn spawn_mock_as() -> u16 {
    spawn_counting_mock_as().0
}

/// [`spawn_mock_as`], with the count of token polls it has answered.
fn spawn_counting_mock_as() -> (u16, Arc<AtomicUsize>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let polls = Arc::new(AtomicUsize::new(0));
    let counted = Arc::clone(&polls);
    std::thread::spawn(move || {
        for conn in listener.incoming().take(16) {
            let Ok(stream) = conn else { continue };
            handle(stream, &counted);
        }
    });
    (port, polls)
}

fn handle(stream: TcpStream, polls: &Arc<AtomicUsize>) {
    stream.set_read_timeout(Some(Duration::from_secs(5))).ok();
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    if reader.read_line(&mut line).unwrap_or(0) == 0 {
        return;
    }
    let path = line.split_whitespace().nth(1).unwrap_or("").to_string();
    let mut len = 0usize;
    loop {
        let mut h = String::new();
        if reader.read_line(&mut h).unwrap_or(0) == 0 {
            break;
        }
        if h.trim().is_empty() {
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

    let (status, payload): (&str, Vec<u8>) = if path.ends_with("/device") {
        (
            "200 OK",
            br#"{"device_code":"dev-123","user_code":"WDJB-MJHT","verification_uri":"https://idp.example/device","interval":1,"expires_in":300}"#
                .to_vec(),
        )
    } else if path.ends_with("/token") {
        let n = polls.fetch_add(1, Ordering::SeqCst);
        if n == 0 {
            (
                "400 Bad Request",
                br#"{"error":"authorization_pending"}"#.to_vec(),
            )
        } else {
            (
                "200 OK",
                br#"{"access_token":"at-xyz","refresh_token":"rt-xyz","token_type":"Bearer","expires_in":3600}"#
                    .to_vec(),
            )
        }
    } else {
        ("404 Not Found", b"{}".to_vec())
    };

    let mut s = reader.into_inner();
    let head = format!(
        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        payload.len()
    );
    let _ = s.write_all(head.as_bytes());
    let _ = s.write_all(&payload);
    let _ = s.flush();
}

fn config(port: u16) -> String {
    format!(
        "\
         agent:\n  name: login\n  instruction: x\n  preflight: never\n\
         intelligence:\n  endpoints: [https://api.openai.com/v1]\n  model: m\n\
         store:\n  kind: memory\n\
         mcp:\n  servers:\n    - name: test\n      endpoint: https://mcp.example\n\
         \x20     auth:\n        kind: oauth2\n        grant: device\n\
         \x20       token_url: http://127.0.0.1:{port}/token\n\
         \x20       device_authorization_url: http://127.0.0.1:{port}/device\n\
         \x20       client_id: agentd-cli\n        scopes: [mcp:read]\n\
         lifecycle:\n  run_until: idle\n"
    )
}

#[test]
fn agentd_login_completes_the_device_flow_and_caches_the_token() {
    let port = spawn_mock_as();
    let cfg = common::unique_path("login-cfg", "yaml");
    std::fs::write(&cfg, config(port)).unwrap();
    let cred_dir = common::unique_path("login-creds", "d");
    std::fs::create_dir_all(&cred_dir).ok();

    let out = Command::new(env!("CARGO_BIN_EXE_agentd"))
        .args(["--login", "mcp:test", "--config", &cfg])
        .env("AGENTD_CRED_DIR", &cred_dir)
        .output()
        .expect("run agentd --login");

    assert!(
        out.status.success(),
        "login exits 0: status={:?}\nstderr={}",
        out.status.code(),
        String::from_utf8_lossy(&out.stderr)
    );
    // The device prompt (URL + code) is printed, but never the token.
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("WDJB-MJHT"),
        "the user code is shown: {stderr}"
    );
    assert!(
        !stderr.contains("at-xyz"),
        "the token is never printed: {stderr}"
    );

    // The token is cached in a 0600 file the daemon would read.
    let id = agentd::auth::cache::cred_id("mcp:test");
    let path = format!("{cred_dir}/{id}.json");
    let body = std::fs::read_to_string(&path).expect("a cred file was written");
    let cred: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(cred["access_token"], "at-xyz");
    assert_eq!(cred["refresh_token"], "rt-xyz");
    assert!(cred["expires_at_ms"].as_u64().unwrap() > 0);
    assert_eq!(
        cred["authorization_server"],
        format!("http://127.0.0.1:{port}/token"),
        "a block that names no issuer: kept with its token endpoint"
    );

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "the cred file is owner-only");
    }

    // Close the loop: the daemon's own path (`signer_for`) reads the same cache
    // and injects the cached bearer on the wire — no re-login, no network (the
    // token is still valid). This is exactly what `mcp::from_spec` attaches.
    // SAFETY: no other test of this binary reads the variable in-process:
    // each passes its directory, or sets it on the child it runs.
    unsafe { std::env::set_var("AGENTD_CRED_DIR", &cred_dir) };
    let authspec = agentd::config::AuthSpec {
        kind: "oauth2".into(),
        grant: Some("device".into()),
        token_url: Some(format!("http://127.0.0.1:{port}/token")),
        client_id: Some("agentd-cli".into()),
        ..Default::default()
    };
    let signer = agentd::auth::device::signer_for(&authspec, "mcp:test", Duration::from_secs(5))
        .expect("signer_for ok")
        .expect("a signer is built for an oauth2 auth block");
    let headers = signer.sign("POST", "mcp.example", "/mcp", b"{}").unwrap();
    assert_eq!(
        headers,
        vec![("Authorization".to_string(), "Bearer at-xyz".to_string())],
        "the daemon injects the cached bearer"
    );

    std::fs::remove_file(&cfg).ok();
    std::fs::remove_dir_all(&cred_dir).ok();
}

/// A login is written to the cache under the target's lock, the one a
/// renewal holds in every other process: one that finishes while a renewal
/// is in flight waits for it, so the renewal's write-back never lands over
/// the new login.
#[test]
fn a_login_waits_for_a_renewal_in_flight() {
    let (port, polls) = spawn_counting_mock_as();
    let cfg = common::unique_path("login-lock-cfg", "yaml");
    std::fs::write(&cfg, config(port)).unwrap();
    let cred_dir = common::unique_path("login-lock-creds", "d");
    let dir = std::path::Path::new(&cred_dir);
    // This process is the one renewing.
    let renewing = agentd::auth::cache::lock_file(dir, "mcp:test", Duration::from_secs(5)).unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_agentd"))
        .args(["--login", "mcp:test", "--config", &cfg])
        .env("AGENTD_CRED_DIR", &cred_dir)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("run agentd --login");
    let start = std::time::Instant::now();
    while polls.load(Ordering::SeqCst) < 2 && start.elapsed() < Duration::from_secs(20) {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(polls.load(Ordering::SeqCst), 2, "the token was issued");
    std::thread::sleep(Duration::from_millis(500));
    let early = agentd::auth::cache::load_file(dir, "mcp:test");
    drop(renewing);
    let status = child.wait().unwrap();
    let kept = agentd::auth::cache::load_file(dir, "mcp:test");
    std::fs::remove_file(&cfg).ok();
    std::fs::remove_dir_all(&cred_dir).ok();
    assert!(early.is_none(), "written while the renewal held the lock");
    assert!(status.success(), "{status:?}");
    assert_eq!(kept.unwrap().access_token, "at-xyz");
}

// ------------------------------------- rotation kept, and revocation on logout

/// What the rotating server saw: each refresh token presented, each
/// revocation's form, and how many times its metadata was asked for.
#[derive(Default)]
struct Seen {
    refreshes: Vec<String>,
    revocations: Vec<String>,
    metadata: usize,
}

/// An authorization server that rotates refresh tokens and refuses a spent
/// one with `invalid_grant`. `rt-1`'s refresh answers `rt-2`; `rt-2`'s first
/// answers no refresh token at all, so `rt-2` stays good; its second answers
/// `rt-4`. Its RFC 8414 metadata names its revocation endpoint, which
/// answers `revoke_status` — or, with none, names no revocation endpoint. A
/// device login is approved at its first poll and issues `rt-1`, and the
/// RFC 9728 metadata of an MCP server at its `/mcp` names it.
fn spawn_rotating_as(revoke_status: Option<&'static str>) -> (String, Arc<std::sync::Mutex<Seen>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let seen = Arc::new(std::sync::Mutex::new(Seen::default()));
    let (issuer, log) = (base.clone(), Arc::clone(&seen));
    std::thread::spawn(move || {
        let mut good = vec!["rt-1".to_string()];
        let mut refreshes = 0;
        for conn in listener.incoming().flatten() {
            conn.set_read_timeout(Some(Duration::from_secs(5))).ok();
            let mut reader = BufReader::new(conn);
            let mut line = String::new();
            reader.read_line(&mut line).ok();
            let path = line.split_whitespace().nth(1).unwrap_or("").to_string();
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
            let form = String::from_utf8_lossy(&body).into_owned();
            let field = |k: &str| {
                form.split('&')
                    .find_map(|kv| kv.strip_prefix(&format!("{k}=")))
                    .map(str::to_string)
            };
            let (status, payload) = if path == "/.well-known/oauth-authorization-server" {
                log.lock().unwrap().metadata += 1;
                let revocation = match revoke_status {
                    Some(_) => format!(r#","revocation_endpoint":"{issuer}/revoke""#),
                    None => String::new(),
                };
                (
                    "200 OK",
                    format!(
                        r#"{{"issuer":"{issuer}","token_endpoint":"{issuer}/token","device_authorization_endpoint":"{issuer}/device"{revocation}}}"#
                    ),
                )
            } else if path == "/.well-known/oauth-protected-resource/mcp" {
                (
                    "200 OK",
                    format!(
                        r#"{{"resource":"{issuer}/mcp","authorization_servers":["{issuer}"]}}"#
                    ),
                )
            } else if path == "/device" {
                (
                    "200 OK",
                    r#"{"device_code":"dev-1","user_code":"WDJB-MJHT","verification_uri":"https://idp.example/device","interval":1,"expires_in":300}"#
                        .to_string(),
                )
            } else if path == "/token" && field("device_code").is_some() {
                (
                    "200 OK",
                    r#"{"access_token":"at-1","refresh_token":"rt-1","expires_in":3600}"#
                        .to_string(),
                )
            } else if let (Some(revoke_status), "/revoke") = (revoke_status, path.as_str()) {
                log.lock().unwrap().revocations.push(form.clone());
                (revoke_status, String::new())
            } else if path == "/token" {
                let rt = field("refresh_token").unwrap_or_default();
                log.lock().unwrap().refreshes.push(rt.clone());
                if !good.contains(&rt) {
                    (
                        "400 Bad Request",
                        r#"{"error":"invalid_grant"}"#.to_string(),
                    )
                } else {
                    refreshes += 1;
                    let (at, next) = match refreshes {
                        1 => ("at-2", Some("rt-2")),
                        2 => ("at-3", None),
                        _ => ("at-4", Some("rt-4")),
                    };
                    match next {
                        Some(n) => {
                            good.retain(|g| *g != rt);
                            good.push(n.to_string());
                            (
                                "200 OK",
                                format!(
                                    r#"{{"access_token":"{at}","refresh_token":"{n}","expires_in":1}}"#
                                ),
                            )
                        }
                        None => (
                            "200 OK",
                            format!(r#"{{"access_token":"{at}","expires_in":1}}"#),
                        ),
                    }
                }
            } else {
                ("404 Not Found", String::new())
            };
            let mut s = reader.into_inner();
            let _ = write!(
                s,
                "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{payload}",
                payload.len()
            );
        }
    });
    (base, seen)
}

/// A refresh that rotates the refresh token is written back to the login
/// cache before its access token is used, and one that carries no refresh
/// token keeps the old one there — so each restart (a new process, reading
/// the cache) renews with the token the server last issued, and a server
/// that refuses spent ones never sees one.
#[test]
fn a_rotated_refresh_token_survives_a_restart() {
    let (issuer, seen) = spawn_rotating_as(Some("200 OK"));
    let dir = tempfile::tempdir().unwrap();
    agentd::auth::cache::store_file(
        dir.path(),
        "mcp:test",
        &agentd::auth::cache::CachedCred {
            access_token: "at-1".into(),
            refresh_token: Some("rt-1".into()),
            expires_at_ms: 1,
            authorization_server: Some(format!("{issuer}/token")),
            ..Default::default()
        },
    )
    .unwrap();
    // One daemon process: its token source, seeded from the cache.
    let process = || {
        agentd::auth::device::TokenSource::new(
            agentd::auth::device::Endpoints::pinned(agentd::auth::oauth2::OAuth2Params {
                token_url: format!("{issuer}/token"),
                client_id: "agentd-cli".into(),
                ..Default::default()
            }),
            "mcp:test",
            dir.path().to_path_buf(),
            Duration::from_secs(5),
        )
    };
    let kept = || {
        agentd::auth::cache::load_file(dir.path(), "mcp:test")
            .and_then(|c| c.refresh_token)
            .unwrap()
    };
    assert_eq!(process().bearer().unwrap(), "at-2");
    assert_eq!(kept(), "rt-2", "the rotation is written back");
    // Restart: the new process renews with the rotated token.
    assert_eq!(process().bearer().unwrap(), "at-3");
    assert_eq!(
        kept(),
        "rt-2",
        "a refresh with no refresh token keeps the old"
    );
    // And again: the kept token still renews.
    assert_eq!(process().bearer().unwrap(), "at-4");
    assert_eq!(kept(), "rt-4");
    assert_eq!(seen.lock().unwrap().refreshes, ["rt-1", "rt-2", "rt-2"]);
}

fn logout_config(issuer: &str) -> String {
    format!(
        "\
         agent:\n  name: logout\n  instruction: x\n  preflight: never\n\
         intelligence:\n  endpoints: [https://api.openai.com/v1]\n  model: m\n\
         store:\n  kind: memory\n\
         mcp:\n  servers:\n    - name: test\n      endpoint: https://mcp.example\n\
         \x20     auth:\n        kind: oauth2\n        grant: device\n\
         \x20       issuer: {issuer}\n        client_id: agentd-cli\n\
         lifecycle:\n  run_until: idle\n"
    )
}

/// A config naming `issuer` and a login cache holding `mcp:test`'s login
/// there: the paths of both.
fn logged_in(issuer: &str) -> (String, String) {
    let cfg = common::unique_path("logout-cfg", "yaml");
    std::fs::write(&cfg, logout_config(issuer)).unwrap();
    let cred_dir = common::unique_path("logout-creds", "d");
    agentd::auth::cache::store_file(
        std::path::Path::new(&cred_dir),
        "mcp:test",
        &agentd::auth::cache::CachedCred {
            access_token: "at-1".into(),
            refresh_token: Some("rt-1".into()),
            expires_at_ms: 0,
            authorization_server: Some(issuer.to_string()),
            ..Default::default()
        },
    )
    .unwrap();
    (cfg, cred_dir)
}

/// The `agentd --logout mcp:test` command over [`logged_in`]'s paths.
fn logout_command(cfg: &str, cred_dir: &str) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_agentd"));
    cmd.args(["--logout", "mcp:test", "--config", cfg])
        .env("AGENTD_CRED_DIR", cred_dir);
    cmd
}

/// `agentd --logout` against a cached login with the given AS.
fn logout(issuer: &str) -> (std::process::Output, String) {
    let (cfg, cred_dir) = logged_in(issuer);
    let out = logout_command(&cfg, &cred_dir)
        .output()
        .expect("run agentd --logout");
    std::fs::remove_file(&cfg).ok();
    (out, cred_dir)
}

/// `--logout` deletes the login here and revokes the refresh token at the
/// RFC 7009 endpoint the server's metadata lists; a revocation that fails
/// has still deleted it here, and says the token stays good at the server;
/// and metadata that lists no revocation endpoint is asked for once, and
/// leaves the logout local.
#[test]
fn agentd_logout_revokes_the_token_where_it_was_issued() {
    let (issuer, seen) = spawn_rotating_as(Some("200 OK"));
    let (out, cred_dir) = logout(&issuer);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "{stdout}\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        stdout.contains("revoked at the authorization server"),
        "{stdout}"
    );
    assert_eq!(
        seen.lock().unwrap().revocations,
        ["token=rt-1&token_type_hint=refresh_token&client_id=agentd-cli"]
    );
    assert!(
        agentd::auth::cache::load_file(std::path::Path::new(&cred_dir), "mcp:test").is_none(),
        "deleted here too"
    );
    std::fs::remove_dir_all(&cred_dir).ok();

    let (issuer, seen) = spawn_rotating_as(Some("503 Service Unavailable"));
    let (out, cred_dir) = logout(&issuer);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !out.status.success(),
        "a failed revocation is not a clean logout"
    );
    assert!(
        stderr.contains("revocation at the authorization server failed")
            && stderr.contains("HTTP 503"),
        "{stderr}"
    );
    assert_eq!(seen.lock().unwrap().revocations.len(), 1);
    assert!(
        agentd::auth::cache::load_file(std::path::Path::new(&cred_dir), "mcp:test").is_none(),
        "deleted here whatever the server said"
    );
    std::fs::remove_dir_all(&cred_dir).ok();

    let (issuer, seen) = spawn_rotating_as(None);
    let (out, cred_dir) = logout(&issuer);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{stdout}");
    assert_eq!(stdout.trim(), "logged out of mcp:test");
    assert_eq!(seen.lock().unwrap().metadata, 1, "the metadata, once");
    assert!(agentd::auth::cache::load_file(std::path::Path::new(&cred_dir), "mcp:test").is_none());
    std::fs::remove_dir_all(&cred_dir).ok();
}

/// A login made at the authorization server RFC 9728 found — the block
/// names no issuer — is kept with that server, and the logout, finding it
/// again, revokes the token there.
#[test]
fn a_logout_revokes_where_rfc_9728_found_the_login() {
    let (issuer, seen) = spawn_rotating_as(Some("200 OK"));
    let cfg = common::unique_path("logout-9728-cfg", "yaml");
    std::fs::write(
        &cfg,
        format!(
            "\
             agent:\n  name: logout\n  instruction: x\n  preflight: never\n\
             intelligence:\n  endpoints: [https://api.openai.com/v1]\n  model: m\n\
             store:\n  kind: memory\n\
             mcp:\n  servers:\n    - name: test\n      endpoint: {issuer}/mcp\n\
             \x20     auth:\n        kind: oauth2\n        grant: device\n\
             \x20       client_id: agentd-cli\n\
             lifecycle:\n  run_until: idle\n"
        ),
    )
    .unwrap();
    let cred_dir = common::unique_path("logout-9728-creds", "d");
    let run = |verb: &str| {
        Command::new(env!("CARGO_BIN_EXE_agentd"))
            .args([verb, "mcp:test", "--config", &cfg])
            .env("AGENTD_CRED_DIR", &cred_dir)
            .output()
            .expect("run agentd")
    };
    let login = run("--login");
    assert!(
        login.status.success(),
        "{}",
        String::from_utf8_lossy(&login.stderr)
    );
    let kept = agentd::auth::cache::load_file(std::path::Path::new(&cred_dir), "mcp:test");
    assert_eq!(
        kept.and_then(|c| c.authorization_server).as_deref(),
        Some(issuer.as_str()),
        "kept with the issuer RFC 9728 found"
    );
    let out = run("--logout");
    std::fs::remove_file(&cfg).ok();
    std::fs::remove_dir_all(&cred_dir).ok();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success() && stdout.contains("revoked at the authorization server"),
        "{stdout}\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        seen.lock().unwrap().revocations,
        ["token=rt-1&token_type_hint=refresh_token&client_id=agentd-cli"]
    );
}

/// A logout deletes the login here before it dials anything, so one cut
/// short — by an authorization server that never answers, or a Ctrl-C — has
/// already removed it: when the server is first asked, the cache is empty.
#[test]
fn a_logout_cut_short_has_already_deleted_the_credential() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let issuer = format!("http://{}", listener.local_addr().unwrap());
    let (dialled, dial) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        // Every connection is held open and never answered.
        let mut held = Vec::new();
        for conn in listener.incoming().flatten() {
            held.push(conn);
            let _ = dialled.send(());
        }
    });
    let (cfg, cred_dir) = logged_in(&issuer);
    let mut child = logout_command(&cfg, &cred_dir)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("run agentd --logout");
    dial.recv_timeout(Duration::from_secs(20))
        .expect("the logout asks the issuer");
    let kept = agentd::auth::cache::load_file(std::path::Path::new(&cred_dir), "mcp:test");
    let _ = child.kill();
    let _ = child.wait();
    std::fs::remove_file(&cfg).ok();
    std::fs::remove_dir_all(&cred_dir).ok();
    assert!(
        kept.is_none(),
        "the login was still cached when the server was asked"
    );
}
