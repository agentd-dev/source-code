//! End-to-end CLI smoke tests against the single-entry-point
//! binary. No subcommands — behaviour derives from the loaded
//! workflow, with flag + env-var overrides.

use std::io::{BufReader, BufWriter, Read, Write};
use std::net::TcpStream;
use std::os::unix::net::UnixListener;
use std::path::PathBuf;
use std::process::Command;
use std::sync::{Mutex, MutexGuard, OnceLock};
use std::thread;

use serde_json::Value;
use tempfile::TempDir;

/// Serialise tests that spawn an HTTP server on an ephemeral port.
/// `pick_ephemeral_port` + spawn racing is flaky under `cargo test`'s
/// default parallelism — the port is "free" when we release the
/// probe listener but another test can grab it before the binary
/// binds. One global lock, taken for the duration of each test,
/// makes the class deterministic.
fn http_smoke_lock() -> MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|p| p.into_inner())
}

const EXIT_OK: i32 = 0;
const EXIT_USAGE: i32 = 2;
const EXIT_SEMANTIC: i32 = 5;

fn bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_agentd"))
}

fn write_workflow(dir: &TempDir, body: &str) -> PathBuf {
    let path = dir.path().join("wf.toml");
    std::fs::write(&path, body).unwrap();
    path
}

const SIMPLE_WF: &str = r#"
name = "hello"

[[start_nodes]]
name = "main"
source = "manual"
entry_node = "a"

[[nodes]]
id = "a"
type = "merge"

[[nodes]]
id = "b"
type = "terminate"

[[edges]]
from = "a"
to = "b"
"#;

// ---------------------------------------------------------------------------
// One-shot mode
// ---------------------------------------------------------------------------

#[test]
fn one_shot_completes_on_simple_workflow() {
    let dir = TempDir::new().unwrap();
    let wf = write_workflow(&dir, SIMPLE_WF);
    let out = Command::new(bin())
        .args(["--config"])
        .arg(&wf)
        .args(["--start", "main"])
        .output()
        .unwrap();
    assert_eq!(
        out.status.code(),
        Some(EXIT_OK),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let body: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(body["status"], "completed");
    assert_eq!(body["last_node"], "b");
}

#[test]
fn env_var_twin_of_config_flag_works() {
    let dir = TempDir::new().unwrap();
    let wf = write_workflow(&dir, SIMPLE_WF);
    let out = Command::new(bin())
        .env("AGENTD_CONFIG", &wf)
        .env("AGENTD_START", "main")
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(EXIT_OK));
    let body: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(body["status"], "completed");
}

#[test]
fn start_node_auto_inferred_when_only_one_manual() {
    // SIMPLE_WF has exactly one manual start — omit --start.
    let dir = TempDir::new().unwrap();
    let wf = write_workflow(&dir, SIMPLE_WF);
    let out = Command::new(bin())
        .args(["--config"])
        .arg(&wf)
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(EXIT_OK));
}

#[test]
fn failing_workflow_exits_5() {
    let dir = TempDir::new().unwrap();
    let wf = write_workflow(
        &dir,
        r#"
name = "boom"

[[start_nodes]]
name = "main"
source = "manual"
entry_node = "f"

[[nodes]]
id = "f"
type = "fail"
reason = "nope"
"#,
    );
    let out = Command::new(bin())
        .args(["--config"])
        .arg(&wf)
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(EXIT_SEMANTIC));
    let body: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(body["status"], "failed");
    assert_eq!(body["reason"], "nope");
}

#[test]
fn missing_config_is_usage_error() {
    let out = Command::new(bin())
        .env_remove("AGENTD_CONFIG")
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(EXIT_USAGE));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("no workflow configured"),
        "stderr: {stderr}"
    );
}

// ---------------------------------------------------------------------------
// Validation mode
// ---------------------------------------------------------------------------

#[test]
fn validate_only_passes_clean_workflow() {
    let dir = TempDir::new().unwrap();
    let wf = write_workflow(&dir, SIMPLE_WF);
    let out = Command::new(bin())
        .args(["--config"])
        .arg(&wf)
        .arg("--validate-only")
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(EXIT_OK));
    let body: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(body["ok"], true);
}

#[test]
fn invalid_workflow_surfaces_every_issue() {
    let dir = TempDir::new().unwrap();
    let wf = write_workflow(
        &dir,
        r#"
name = "bad"

[[nodes]]
id = "x"
type = "merge"

[[nodes]]
id = "x"
type = "merge"

[[edges]]
from = "x"
to = "missing"
"#,
    );
    let out = Command::new(bin())
        .args(["--config"])
        .arg(&wf)
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(EXIT_SEMANTIC));
    let body: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(body["ok"], false);
    let codes: Vec<_> = body["issues"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["code"].as_str().unwrap().to_string())
        .collect();
    assert!(codes.contains(&"dup_node_id".to_string()));
    assert!(codes.contains(&"dangling_edge".to_string()));
}

// ---------------------------------------------------------------------------
// Dry-run
// ---------------------------------------------------------------------------

#[test]
fn dry_run_does_not_touch_disk() {
    let dir = TempDir::new().unwrap();
    let target = dir.path().join("out.txt");
    let wf = write_workflow(
        &dir,
        r#"
name = "emit"

[[start_nodes]]
name = "main"
source = "manual"
entry_node = "w"

[[nodes]]
id = "w"
type = "write_file"
path_from = "trigger.path"
content_from = "trigger.content"

[[nodes]]
id = "done"
type = "terminate"

[[edges]]
from = "w"
to = "done"
"#,
    );
    let input = dir.path().join("input.json");
    std::fs::write(
        &input,
        serde_json::to_string(&serde_json::json!({
            "path": target.display().to_string(),
            "content": "should not land",
        }))
        .unwrap(),
    )
    .unwrap();

    let out = Command::new(bin())
        .args(["--config"])
        .arg(&wf)
        .args(["--start", "main", "--input"])
        .arg(&input)
        .arg("--dry-run")
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(EXIT_OK));
    assert!(!target.exists(), "dry-run must not write the file");
}

// ---------------------------------------------------------------------------
// Write-file round trip
// ---------------------------------------------------------------------------

#[test]
fn end_to_end_write_file_through_cli() {
    let dir = TempDir::new().unwrap();
    let target = dir.path().join("emitted.txt");
    let wf = write_workflow(
        &dir,
        r#"
name = "emit"

[[start_nodes]]
name = "main"
source = "manual"
entry_node = "w"

[[nodes]]
id = "w"
type = "write_file"
path_from = "trigger.path"
content_from = "trigger.content"

[[nodes]]
id = "done"
type = "terminate"

[[edges]]
from = "w"
to = "done"
"#,
    );
    let input = dir.path().join("input.json");
    std::fs::write(
        &input,
        serde_json::to_string(&serde_json::json!({
            "path": target.display().to_string(),
            "content": "landed",
        }))
        .unwrap(),
    )
    .unwrap();

    let out = Command::new(bin())
        .args(["--config"])
        .arg(&wf)
        .args(["--start", "main", "--input"])
        .arg(&input)
        .output()
        .unwrap();
    assert_eq!(
        out.status.code(),
        Some(EXIT_OK),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "landed");
}

#[test]
fn input_file_becomes_trigger_payload() {
    let dir = TempDir::new().unwrap();
    let wf = write_workflow(
        &dir,
        r#"
name = "cond"

[[start_nodes]]
name = "main"
source = "manual"
entry_node = "c"

[[nodes]]
id = "c"
type = "condition"
expr = "trigger.flag"

[[nodes]]
id = "yes"
type = "terminate"

[[nodes]]
id = "no"
type = "fail"
reason = "flag was false"

[[edges]]
from = "c"
to = "yes"
when = "true"

[[edges]]
from = "c"
to = "no"
when = "false"
"#,
    );
    let input_path = dir.path().join("input.json");
    std::fs::write(&input_path, r#"{"flag": true}"#).unwrap();

    let out = Command::new(bin())
        .args(["--config"])
        .arg(&wf)
        .args(["--start", "main", "--input"])
        .arg(&input_path)
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(EXIT_OK));
    let body: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(body["status"], "completed");
    assert_eq!(body["last_node"], "yes");
}

// ---------------------------------------------------------------------------
// Policy
// ---------------------------------------------------------------------------

#[test]
fn workflow_policy_denies_writes_outside_allowlist() {
    let dir = TempDir::new().unwrap();
    let allowed = dir.path().join("allowed");
    std::fs::create_dir_all(&allowed).unwrap();
    let denied = dir.path().join("denied.txt");

    let wf_body = format!(
        r#"
name = "guarded"

[policy.fs]
write = ["{allowed}/**"]

[[start_nodes]]
name = "main"
source = "manual"
entry_node = "w"

[[nodes]]
id = "w"
type = "write_file"
path_from = "trigger.path"
content_from = "trigger.content"

[[nodes]]
id = "done"
type = "terminate"

[[edges]]
from = "w"
to = "done"
"#,
        allowed = allowed.display()
    );
    let wf = write_workflow(&dir, &wf_body);
    let input = dir.path().join("input.json");
    std::fs::write(
        &input,
        serde_json::to_string(&serde_json::json!({
            "path": denied.display().to_string(),
            "content": "blocked",
        }))
        .unwrap(),
    )
    .unwrap();

    let out = Command::new(bin())
        .args(["--config"])
        .arg(&wf)
        .args(["--start", "main", "--input"])
        .arg(&input)
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(EXIT_SEMANTIC));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("fs_write"), "stderr: {stderr}");
    assert!(!denied.exists(), "policy must have blocked the write");
}

// ---------------------------------------------------------------------------
// Help + version
// ---------------------------------------------------------------------------

#[test]
fn help_exits_zero() {
    let out = Command::new(bin()).arg("--help").output().unwrap();
    assert_eq!(out.status.code(), Some(EXIT_OK));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("bounded workflow runtime"),
        "stderr: {stderr}"
    );
}

#[test]
fn version_prints_cargo_version() {
    let out = Command::new(bin()).arg("--version").output().unwrap();
    assert_eq!(out.status.code(), Some(EXIT_OK));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(!stdout.trim().is_empty());
}

// ---------------------------------------------------------------------------
// Intelligence round-trip (mock Unix server)
// ---------------------------------------------------------------------------

fn spawn_fake_intel(
    sock_path: &std::path::Path,
    response_content: &'static str,
) -> thread::JoinHandle<()> {
    let listener = UnixListener::bind(sock_path).unwrap();
    thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        let mut reader = BufReader::new(&stream);
        let mut len_buf = [0u8; 4];
        reader.read_exact(&mut len_buf).unwrap();
        let len = u32::from_le_bytes(len_buf) as usize;
        let mut body = vec![0u8; len];
        reader.read_exact(&mut body).unwrap();
        let req: Value = serde_json::from_slice(&body).unwrap();
        let resp = serde_json::json!({
            "jsonrpc": "2.0",
            "id": req["id"],
            "result": { "content": response_content, "usage": {} }
        });
        let payload = serde_json::to_vec(&resp).unwrap();
        let mut writer = BufWriter::new(&stream);
        writer
            .write_all(&(payload.len() as u32).to_le_bytes())
            .unwrap();
        writer.write_all(&payload).unwrap();
        writer.flush().unwrap();
    })
}

#[test]
fn cli_routes_llm_infer_through_intel_unix() {
    let dir = TempDir::new().unwrap();
    let wf = write_workflow(
        &dir,
        r#"
name = "review"

[[start_nodes]]
name = "main"
source = "manual"
entry_node = "analyze"

[[nodes]]
id = "analyze"
type = "llm_infer"
backend = "default"
prompt = "Summarise: {{text}}"
input_from = "trigger"

[[nodes]]
id = "done"
type = "terminate"

[[edges]]
from = "analyze"
to = "done"
"#,
    );

    let sock = dir.path().join("intel.sock");
    let server = spawn_fake_intel(&sock, "short summary");

    let input = dir.path().join("input.json");
    std::fs::write(&input, r#"{"text": "some long document"}"#).unwrap();

    let out = Command::new(bin())
        .args(["--config"])
        .arg(&wf)
        .args(["--start", "main", "--input"])
        .arg(&input)
        .args(["--intel-unix"])
        .arg(&sock)
        .output()
        .unwrap();

    server.join().unwrap();
    assert_eq!(
        out.status.code(),
        Some(EXIT_OK),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let body: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(body["status"], "completed");
    assert_eq!(body["last_node"], "done");
}

// ---------------------------------------------------------------------------
// Shell + HTTP tools (feature-gated; run only when the features are enabled)
// ---------------------------------------------------------------------------

#[cfg(feature = "tools-shell")]
#[test]
fn shell_run_end_to_end() {
    let dir = TempDir::new().unwrap();
    let wf = write_workflow(
        &dir,
        r#"
name = "sh"

[policy.shell]
# `/bin/echo` canonicalises to `/usr/bin/echo` on modern distros —
# allow both via prefix wildcards to stay portable.
commands = ["/bin/echo", "/usr/bin/echo"]

[[start_nodes]]
name = "main"
source = "manual"
entry_node = "run"

[[nodes]]
id = "run"
type = "shell_run"
command = "/bin/echo"
args_from = "trigger.args"

[[nodes]]
id = "done"
type = "terminate"

[[edges]]
from = "run"
to = "done"
"#,
    );
    let input = dir.path().join("input.json");
    std::fs::write(&input, r#"{"args":["hello","from","agentd"]}"#).unwrap();

    let out = Command::new(bin())
        .args(["--config"])
        .arg(&wf)
        .args(["--start", "main", "--input"])
        .arg(&input)
        .output()
        .unwrap();
    assert_eq!(
        out.status.code(),
        Some(EXIT_OK),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let body: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(body["status"], "completed");
    // The `run` node's stored output holds the command result.
    // Engine's final_value here is whatever terminate produced; the
    // side-effect presence is what we want to assert — the workflow
    // completed, so shell_run succeeded.
}

#[cfg(feature = "tools-shell")]
#[test]
fn shell_run_denied_by_allowlist() {
    let dir = TempDir::new().unwrap();
    let wf = write_workflow(
        &dir,
        r#"
name = "sh_deny"

[policy.shell]
commands = ["/bin/true"]

[[start_nodes]]
name = "main"
source = "manual"
entry_node = "run"

[[nodes]]
id = "run"
type = "shell_run"
command = "/bin/echo"

[[nodes]]
id = "done"
type = "terminate"

[[edges]]
from = "run"
to = "done"
"#,
    );

    let out = Command::new(bin())
        .args(["--config"])
        .arg(&wf)
        .args(["--start", "main"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(EXIT_SEMANTIC));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("shell_run"), "stderr: {stderr}");
    assert!(
        stderr.contains("not covered") || stderr.contains("allowlist"),
        "stderr: {stderr}"
    );
}

// ---------------------------------------------------------------------------
// Auth — bearer + HMAC-signed webhook
// ---------------------------------------------------------------------------

#[cfg(feature = "auth")]
fn pick_ephemeral_port() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let p = listener.local_addr().unwrap().port();
    drop(listener);
    p
}

#[cfg(feature = "auth")]
fn wait_accepts(addr: &str) {
    use std::time::Duration;
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    while std::time::Instant::now() < deadline {
        if TcpStream::connect(addr).is_ok() {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("server did not accept in time on {addr}");
}

#[cfg(feature = "auth")]
#[test]
fn bearer_auth_allows_matching_token_and_denies_others() {
    let _lock = http_smoke_lock();
    let dir = TempDir::new().unwrap();
    let wf = write_workflow(
        &dir,
        r#"
name = "b"

[auth.bearer.ops]
tokens_env = "AGENTD_CLI_TEST_BEARER_TOKENS"

[[start_nodes]]
name = "on_http"
source = "http"
entry_node = "a"

[[http_routes]]
method = "POST"
path = "/run"
start_node = "on_http"
auth = "bearer:ops"

[[nodes]]
id = "a"
type = "merge"

[[nodes]]
id = "b"
type = "terminate"

[[edges]]
from = "a"
to = "b"
"#,
    );

    let port = pick_ephemeral_port();
    let addr = format!("127.0.0.1:{port}");
    let mut child = Command::new(bin())
        .env("AGENTD_CLI_TEST_BEARER_TOKENS", "s3cret")
        .args(["--config"])
        .arg(&wf)
        .args(["--bind", &addr])
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    wait_accepts(&addr);

    // Happy path: Bearer with matching token.
    let mut s = TcpStream::connect(&addr).unwrap();
    s.write_all(
        b"POST /run HTTP/1.1\r\n\
         Host: localhost\r\n\
         Authorization: Bearer s3cret\r\n\
         Content-Length: 2\r\n\
         Connection: close\r\n\
         \r\n\
         {}",
    )
    .unwrap();
    let mut buf = String::new();
    s.read_to_string(&mut buf).unwrap();
    assert!(buf.contains("HTTP/1.1 200"), "expected 200, got: {buf}");

    // Wrong token: 401.
    let mut s = TcpStream::connect(&addr).unwrap();
    s.write_all(
        b"POST /run HTTP/1.1\r\n\
         Host: localhost\r\n\
         Authorization: Bearer wrong\r\n\
         Content-Length: 2\r\n\
         Connection: close\r\n\
         \r\n\
         {}",
    )
    .unwrap();
    let mut buf = String::new();
    s.read_to_string(&mut buf).unwrap();
    assert!(buf.contains("HTTP/1.1 401"), "expected 401, got: {buf}");
    assert!(buf.contains("unauthorized"));

    // Missing header: 401.
    let mut s = TcpStream::connect(&addr).unwrap();
    s.write_all(
        b"POST /run HTTP/1.1\r\n\
         Host: localhost\r\n\
         Content-Length: 2\r\n\
         Connection: close\r\n\
         \r\n\
         {}",
    )
    .unwrap();
    let mut buf = String::new();
    s.read_to_string(&mut buf).unwrap();
    assert!(buf.contains("HTTP/1.1 401"), "expected 401, got: {buf}");

    let _ = child.kill();
    let _ = child.wait();
}

#[cfg(feature = "auth")]
#[test]
fn hmac_webhook_verifies_signature() {
    let _lock = http_smoke_lock();
    // Deterministic HMAC signature computed from the body+secret.
    // We use the lib's sign_hex helper exposed for exactly this.
    let body = br#"{"event":"push"}"#;
    let secret = "shhh";
    let sig = agentd::auth::hmac::sign_hex(secret.as_bytes(), body);

    let dir = TempDir::new().unwrap();
    let wf = write_workflow(
        &dir,
        r#"
name = "h"

[auth.hmac.github]
secret_env = "AGENTD_CLI_TEST_WEBHOOK_SECRET"
header = "X-Hub-Signature-256"
prefix = "sha256="

[[start_nodes]]
name = "on_http"
source = "http"
entry_node = "a"

[[http_routes]]
method = "POST"
path = "/webhook"
start_node = "on_http"
auth = "hmac:github"

[[nodes]]
id = "a"
type = "merge"

[[nodes]]
id = "b"
type = "terminate"

[[edges]]
from = "a"
to = "b"
"#,
    );

    let port = pick_ephemeral_port();
    let addr = format!("127.0.0.1:{port}");
    let mut child = Command::new(bin())
        .env("AGENTD_CLI_TEST_WEBHOOK_SECRET", secret)
        .args(["--config"])
        .arg(&wf)
        .args(["--bind", &addr])
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    wait_accepts(&addr);

    // Valid signature.
    let req = format!(
        "POST /webhook HTTP/1.1\r\n\
         Host: localhost\r\n\
         X-Hub-Signature-256: sha256={sig}\r\n\
         Content-Length: {len}\r\n\
         Connection: close\r\n\
         \r\n",
        len = body.len()
    );
    let mut s = TcpStream::connect(&addr).unwrap();
    s.write_all(req.as_bytes()).unwrap();
    s.write_all(body).unwrap();
    let mut buf = String::new();
    s.read_to_string(&mut buf).unwrap();
    assert!(buf.contains("HTTP/1.1 200"), "expected 200, got: {buf}");

    // Tampered body → signature no longer valid.
    let tampered = br#"{"event":"tampered"}"#;
    let req = format!(
        "POST /webhook HTTP/1.1\r\n\
         Host: localhost\r\n\
         X-Hub-Signature-256: sha256={sig}\r\n\
         Content-Length: {len}\r\n\
         Connection: close\r\n\
         \r\n",
        len = tampered.len()
    );
    let mut s = TcpStream::connect(&addr).unwrap();
    s.write_all(req.as_bytes()).unwrap();
    s.write_all(tampered).unwrap();
    let mut buf = String::new();
    s.read_to_string(&mut buf).unwrap();
    assert!(buf.contains("HTTP/1.1 401"), "expected 401, got: {buf}");

    // Missing signature header → 401.
    let req = format!(
        "POST /webhook HTTP/1.1\r\n\
         Host: localhost\r\n\
         Content-Length: {len}\r\n\
         Connection: close\r\n\
         \r\n",
        len = body.len()
    );
    let mut s = TcpStream::connect(&addr).unwrap();
    s.write_all(req.as_bytes()).unwrap();
    s.write_all(body).unwrap();
    let mut buf = String::new();
    s.read_to_string(&mut buf).unwrap();
    assert!(buf.contains("HTTP/1.1 401"), "expected 401, got: {buf}");

    let _ = child.kill();
    let _ = child.wait();
}

#[cfg(feature = "auth")]
#[test]
fn missing_auth_binding_fails_at_startup() {
    let _lock = http_smoke_lock();
    let dir = TempDir::new().unwrap();
    let wf = write_workflow(
        &dir,
        r#"
name = "x"

[[start_nodes]]
name = "on_http"
source = "http"
entry_node = "a"

[[http_routes]]
method = "POST"
path = "/x"
start_node = "on_http"
auth = "bearer:nowhere"

[[nodes]]
id = "a"
type = "terminate"
"#,
    );

    let out = Command::new(bin())
        .args(["--config"])
        .arg(&wf)
        .args(["--bind", "127.0.0.1:0"])
        .output()
        .unwrap();
    // Serve mode fails fast — exit 5, stderr names the missing binding.
    assert_eq!(out.status.code(), Some(EXIT_SEMANTIC));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("bearer:nowhere"), "stderr: {stderr}");
}

// ---------------------------------------------------------------------------
// Rate limiting
// ---------------------------------------------------------------------------

#[test]
fn rate_limit_returns_429_after_burst() {
    let _lock = http_smoke_lock();
    use std::time::Duration;

    let dir = TempDir::new().unwrap();
    let wf = write_workflow(
        &dir,
        r#"
name = "rl"

[[start_nodes]]
name = "on_http"
source = "http"
entry_node = "a"

[[http_routes]]
method = "POST"
path   = "/x"
start_node = "on_http"

[http_routes.rate_limit]
capacity   = 1
per_second = 0.1

[[nodes]]
id = "a"
type = "terminate"
"#,
    );

    let port = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let p = l.local_addr().unwrap().port();
        drop(l);
        p
    };
    let addr = format!("127.0.0.1:{port}");
    let mut child = Command::new(bin())
        .args(["--config"])
        .arg(&wf)
        .args(["--bind", &addr])
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();

    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    while std::time::Instant::now() < deadline {
        if TcpStream::connect(&addr).is_ok() {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }

    let send = |addr: &str| -> String {
        let mut s = TcpStream::connect(addr).unwrap();
        s.write_all(
            b"POST /x HTTP/1.1\r\n\
              Host: localhost\r\n\
              Content-Length: 2\r\n\
              Connection: close\r\n\
              \r\n\
              {}",
        )
        .unwrap();
        let mut buf = String::new();
        s.read_to_string(&mut buf).unwrap();
        buf
    };

    // First burst allowed, subsequent requests get 429.
    let r1 = send(&addr);
    assert!(r1.contains("HTTP/1.1 200"), "resp1: {r1}");
    let r2 = send(&addr);
    assert!(r2.contains("HTTP/1.1 429"), "resp2: {r2}");
    assert!(r2.contains("Retry-After"), "resp2: {r2}");

    let _ = child.kill();
    let _ = child.wait();
}

// ---------------------------------------------------------------------------
// Graceful shutdown
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// TLS — happy path end-to-end via rustls client (behind server-tls)
// ---------------------------------------------------------------------------

#[cfg(feature = "server-tls")]
#[test]
fn tls_roundtrip_against_self_signed_server() {
    use std::io::{Read, Write};
    use std::sync::Arc;
    use std::time::Duration;

    let _lock = http_smoke_lock();

    let dir = TempDir::new().unwrap();

    // Generate a self-signed cert + key the server will load.
    let issued = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
    let cert_pem_path = dir.path().join("server.pem");
    let key_pem_path = dir.path().join("server.key");
    std::fs::write(&cert_pem_path, issued.cert.pem()).unwrap();
    std::fs::write(&key_pem_path, issued.key_pair.serialize_pem()).unwrap();
    let cert_der = issued.cert.der().to_vec();

    let wf_body = format!(
        r#"
name = "tls_test"

[server.tls]
cert_file = "{cert}"
key_file  = "{key}"

[[start_nodes]]
name = "on_http"
source = "http"
entry_node = "a"

[[http_routes]]
method = "POST"
path   = "/ping"
start_node = "on_http"

[[nodes]]
id = "a"
type = "terminate"
"#,
        cert = cert_pem_path.display(),
        key = key_pem_path.display(),
    );
    let wf = write_workflow(&dir, &wf_body);

    let port = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let p = l.local_addr().unwrap().port();
        drop(l);
        p
    };
    let addr = format!("127.0.0.1:{port}");

    let mut child = Command::new(bin())
        .args(["--config"])
        .arg(&wf)
        .args(["--bind", &addr])
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();

    // Wait for a TCP connection to succeed.
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    let mut ready = false;
    while std::time::Instant::now() < deadline {
        if TcpStream::connect(&addr).is_ok() {
            ready = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    if !ready {
        // Drain stderr for diagnostics.
        let _ = child.kill();
        let output = child.wait_with_output().unwrap();
        panic!(
            "server did not start; stderr:\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    // rustls client — pin exactly the server's cert as the trust
    // root (self-signed).
    let _ = agentd::rustls::crypto::aws_lc_rs::default_provider().install_default();
    let mut roots = agentd::rustls::RootCertStore::empty();
    roots
        .add(agentd::rustls::pki_types::CertificateDer::from(cert_der))
        .unwrap();
    let client_config = agentd::rustls::ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    let server_name = agentd::rustls::pki_types::ServerName::try_from("localhost").unwrap();
    let mut conn =
        agentd::rustls::ClientConnection::new(Arc::new(client_config), server_name).unwrap();
    let mut tcp = TcpStream::connect(&addr).unwrap();
    let mut stream = agentd::rustls::Stream::new(&mut conn, &mut tcp);

    stream
        .write_all(
            b"POST /ping HTTP/1.1\r\n\
             Host: localhost\r\n\
             Content-Length: 2\r\n\
             Connection: close\r\n\
             \r\n\
             {}",
        )
        .unwrap();
    let mut buf = String::new();
    let _ = stream.read_to_string(&mut buf);

    let _ = child.kill();
    let _ = child.wait();

    assert!(buf.contains("HTTP/1.1 200"), "response: {buf}");
    let body = buf.split("\r\n\r\n").nth(1).unwrap_or("");
    let parsed: Value = serde_json::from_str(body.trim()).unwrap();
    assert_eq!(parsed["status"], "completed");
}

#[cfg(feature = "server-tls")]
#[test]
fn mtls_required_rejects_connections_without_client_cert() {
    use std::io::{Read, Write};
    use std::sync::Arc;
    use std::time::Duration;

    let _lock = http_smoke_lock();

    let dir = TempDir::new().unwrap();

    // CA → server cert signed by it.
    let ca_key = rcgen::KeyPair::generate().unwrap();
    let mut ca_params = rcgen::CertificateParams::new(vec![]).unwrap();
    ca_params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    let ca_cert = ca_params.self_signed(&ca_key).unwrap();
    let ca_pem = ca_cert.pem();
    let ca_path = dir.path().join("ca.pem");
    std::fs::write(&ca_path, &ca_pem).unwrap();
    let ca_der = ca_cert.der().to_vec();

    let srv_key = rcgen::KeyPair::generate().unwrap();
    let srv_params = rcgen::CertificateParams::new(vec!["localhost".into()]).unwrap();
    let srv_cert = srv_params.signed_by(&srv_key, &ca_cert, &ca_key).unwrap();
    let srv_cert_path = dir.path().join("server.pem");
    let srv_key_path = dir.path().join("server.key");
    std::fs::write(&srv_cert_path, srv_cert.pem()).unwrap();
    std::fs::write(&srv_key_path, srv_key.serialize_pem()).unwrap();

    let wf_body = format!(
        r#"
name = "mtls_denial"

[server.tls]
cert_file = "{cert}"
key_file  = "{key}"

[server.tls.client_auth]
mode    = "required"
ca_file = "{ca}"

[[start_nodes]]
name = "on_http"
source = "http"
entry_node = "a"

[[http_routes]]
method = "POST"
path   = "/ping"
start_node = "on_http"
auth = "mtls"

[[nodes]]
id = "a"
type = "terminate"
"#,
        cert = srv_cert_path.display(),
        key = srv_key_path.display(),
        ca = ca_path.display(),
    );
    let wf = write_workflow(&dir, &wf_body);

    let port = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let p = l.local_addr().unwrap().port();
        drop(l);
        p
    };
    let addr = format!("127.0.0.1:{port}");

    let mut child = Command::new(bin())
        .args(["--config"])
        .arg(&wf)
        .args(["--bind", &addr])
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();

    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    while std::time::Instant::now() < deadline {
        if TcpStream::connect(&addr).is_ok() {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }

    // Client without a client cert. Handshake must fail.
    let _ = agentd::rustls::crypto::aws_lc_rs::default_provider().install_default();
    let mut roots = agentd::rustls::RootCertStore::empty();
    roots
        .add(agentd::rustls::pki_types::CertificateDer::from(ca_der))
        .unwrap();
    let client_config = agentd::rustls::ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    let server_name = agentd::rustls::pki_types::ServerName::try_from("localhost").unwrap();
    let mut conn =
        agentd::rustls::ClientConnection::new(Arc::new(client_config), server_name).unwrap();
    let mut tcp = TcpStream::connect(&addr).unwrap();
    let mut stream = agentd::rustls::Stream::new(&mut conn, &mut tcp);

    // Either writing or reading fails at the TLS layer because the
    // server demanded a client cert.
    let _ = stream.write_all(b"POST /ping HTTP/1.1\r\n\r\n");
    let mut buf = [0u8; 128];
    let read_res = stream.read(&mut buf);
    let flushed = stream.flush();

    let _ = child.kill();
    let _ = child.wait();

    // At least one of the IO calls must surface a TLS error.
    assert!(
        read_res.is_err() || flushed.is_err(),
        "expected handshake failure; read_res={read_res:?} flushed={flushed:?}"
    );
}

// ---------------------------------------------------------------------------
// Logging — target + workflow-block + overrides
// ---------------------------------------------------------------------------

#[test]
fn log_target_file_writes_to_disk() {
    // Run a workflow that raises a warn-level `workflow.failed`
    // event (Fail node) with target routed to a file. Prove the
    // file contains the audit event.
    let dir = TempDir::new().unwrap();
    let wf = write_workflow(
        &dir,
        r#"
name = "log_to_file"

[logging]
level  = "warn"
format = "json"

[[start_nodes]]
name = "main"
source = "manual"
entry_node = "f"

[[nodes]]
id = "f"
type = "fail"
reason = "intentional"
"#,
    );
    let log_path = dir.path().join("agentd.log");

    let out = Command::new(bin())
        .args(["--config"])
        .arg(&wf)
        .args(["--log-target", &format!("file:{}", log_path.display())])
        .output()
        .unwrap();
    // Fail node → exit 5.
    assert_eq!(out.status.code(), Some(EXIT_SEMANTIC));

    // File should have been created and contain the audit event.
    assert!(log_path.exists(), "log file was not created");
    let content = std::fs::read_to_string(&log_path).unwrap();
    assert!(
        content.contains("workflow.failed"),
        "log content:\n{content}"
    );
    // JSON format → lines should look like JSON objects.
    assert!(
        content.lines().any(|l| l.trim_start().starts_with('{')),
        "expected JSON lines; got:\n{content}"
    );
}

#[test]
fn log_level_workflow_config_honoured() {
    // Workflow sets level=debug; no CLI/env override → debug events
    // should show up in stderr.
    let dir = TempDir::new().unwrap();
    let wf = write_workflow(
        &dir,
        r#"
name = "debug_level"

[logging]
level = "debug"
format = "text"

[[start_nodes]]
name = "main"
source = "manual"
entry_node = "a"

[[nodes]]
id = "a"
type = "merge"

[[nodes]]
id = "b"
type = "terminate"

[[edges]]
from = "a"
to = "b"
"#,
    );
    let out = Command::new(bin())
        .args(["--config"])
        .arg(&wf)
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(EXIT_OK));
    // `node.completed` is a debug-level event — only visible when
    // level is debug or finer.
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("node.completed"),
        "expected debug-level events in stderr; got:\n{stderr}"
    );
}

#[test]
fn cli_log_level_overrides_workflow_block() {
    // Workflow says debug; CLI says error → only ≥error events emitted.
    let dir = TempDir::new().unwrap();
    let wf = write_workflow(
        &dir,
        r#"
name = "override"

[logging]
level = "debug"

[[start_nodes]]
name = "main"
source = "manual"
entry_node = "a"

[[nodes]]
id = "a"
type = "merge"

[[nodes]]
id = "b"
type = "terminate"

[[edges]]
from = "a"
to = "b"
"#,
    );
    let out = Command::new(bin())
        .args(["--config"])
        .arg(&wf)
        .args(["--log-level", "error"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(EXIT_OK));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !stderr.contains("node.completed"),
        "expected CLI override to suppress debug events; got:\n{stderr}"
    );
}

#[test]
#[cfg(unix)]
fn sigterm_triggers_clean_shutdown() {
    let _lock = http_smoke_lock();
    use std::time::Duration;

    let dir = TempDir::new().unwrap();
    let wf = write_workflow(
        &dir,
        r#"
name = "svc"

[[start_nodes]]
name = "on_http"
source = "http"
entry_node = "a"

[[http_routes]]
method = "POST"
path = "/run"
start_node = "on_http"

[[nodes]]
id = "a"
type = "terminate"
"#,
    );

    let port = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let p = l.local_addr().unwrap().port();
        drop(l);
        p
    };
    let addr = format!("127.0.0.1:{port}");

    let mut child = Command::new(bin())
        .args(["--config"])
        .arg(&wf)
        .args(["--bind", &addr])
        .args(["--drain-timeout-secs", "5"])
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();

    // Wait for ready.
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    while std::time::Instant::now() < deadline {
        if TcpStream::connect(&addr).is_ok() {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }

    // Send SIGTERM.
    let pid = child.id() as i32;
    unsafe {
        libc::kill(pid, libc::SIGTERM);
    }

    // Process must exit within a reasonable window.
    let exit_deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                assert!(
                    status.success(),
                    "clean drain should exit 0, got {status:?}"
                );
                return;
            }
            Ok(None) => {
                if std::time::Instant::now() >= exit_deadline {
                    let _ = child.kill();
                    panic!("process did not exit within drain deadline");
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(e) => panic!("try_wait: {e}"),
        }
    }
}

// ---------------------------------------------------------------------------
// Serve mode — infer from http_routes, connect, shoot the binary
// ---------------------------------------------------------------------------

#[test]
fn serve_mode_inferred_from_http_routes() {
    let _lock = http_smoke_lock();
    use std::time::Duration;

    let dir = TempDir::new().unwrap();
    let wf = write_workflow(
        &dir,
        r#"
name = "svc"

[[start_nodes]]
name = "on_http"
source = "http"
entry_node = "a"

[[http_routes]]
method = "POST"
path = "/run"
start_node = "on_http"

[[nodes]]
id = "a"
type = "merge"

[[nodes]]
id = "b"
type = "terminate"

[[edges]]
from = "a"
to = "b"
"#,
    );

    // Pick an ephemeral port by binding-and-releasing.
    let port = {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let p = listener.local_addr().unwrap().port();
        drop(listener);
        p
    };
    let bind = format!("127.0.0.1:{port}");

    let mut child = Command::new(bin())
        .args(["--config"])
        .arg(&wf)
        .args(["--bind", &bind])
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();

    // Wait for the listener.
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    let mut ready = false;
    while std::time::Instant::now() < deadline {
        if TcpStream::connect(&bind).is_ok() {
            ready = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(ready, "server did not start accepting in time");

    let mut stream = TcpStream::connect(&bind).unwrap();
    stream
        .write_all(
            b"POST /run HTTP/1.1\r\n\
             Host: localhost\r\n\
             Content-Length: 2\r\n\
             Connection: close\r\n\
             \r\n\
             {}",
        )
        .unwrap();
    let mut buf = String::new();
    stream.read_to_string(&mut buf).unwrap();
    let _ = child.kill();
    let _ = child.wait();

    assert!(buf.contains("HTTP/1.1 200"), "response: {buf}");
    let body = buf.split("\r\n\r\n").nth(1).unwrap_or("");
    let parsed: Value = serde_json::from_str(body.trim()).unwrap();
    assert_eq!(parsed["status"], "completed");
}
