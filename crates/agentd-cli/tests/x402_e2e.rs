// SPDX-License-Identifier: AGPL-3.0-only
//! Paying for things with x402 (RFC 0049, unit 1), end to end: the binary,
//! a workflow's paying `http` step and the mock x402 resource server
//! (`agentd::payments::mock`, which recovers the signer of every payment
//! and holds it to what it offered). What the server saw is read from its
//! record; what agentd decided, from its log, its audit chain and what
//! crossed to a child (`AGENTD_TEST_FRAMES_FILE`).
//!
//! The spend policy of ADR 0003, rule by rule: refused unsent outside the
//! caps and the allow-lists; the approver's answer above `approve_above`,
//! and nobody else's; never twice after an outcome agentd cannot know; the
//! key sealed, opened in the supervisor alone and in no log, frame or file;
//! and every payment a record in the audit chain.
#![cfg(unix)]

mod common;

#[cfg(feature = "x402")]
use std::path::{Path, PathBuf};
#[cfg(feature = "x402")]
use std::process::{Child, Command, Stdio};
#[cfg(feature = "x402")]
use std::time::{Duration, Instant};

#[cfg(feature = "x402")]
use serde_json::{Value, json};

/// Base Sepolia USDC, as x402's own examples have it, and the mock's.
#[cfg(feature = "x402")]
const USDC: &str = "0x036CbD53842c5426634e7929541eC2318f3dCF7e";
/// The payee the policy allows, and the mock's default `payTo`.
#[cfg(feature = "x402")]
const PAYEE: &str = "0x209693Bc6afc0C5328bA36FaF03C514EF312287C";
/// A second allowed payee.
#[cfg(feature = "x402")]
const OTHER: &str = "0x857b06519E91e3A54538791bDbb0E22373e36b66";
/// The seal key every wallet key file here is sealed under.
#[cfg(feature = "x402")]
const SEAL: &str = "0707070707070707070707070707070707070707070707070707070707070707";

#[cfg(feature = "x402")]
struct Daemon {
    child: Child,
    log: PathBuf,
}

#[cfg(feature = "x402")]
impl Daemon {
    fn spawn(cfg: &Path, env: &[(&str, &str)]) -> Daemon {
        let log = cfg.with_extension("log");
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_agentd"));
        cmd.args(["--config", &cfg.display().to_string()])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::from(std::fs::File::create(&log).unwrap()));
        for (k, v) in env {
            cmd.env(k, v);
        }
        Daemon {
            child: cmd.spawn().expect("spawn agentd"),
            log,
        }
    }

    fn read(&self) -> String {
        std::fs::read_to_string(&self.log).unwrap_or_default()
    }

    fn events(&self, name: &str) -> Vec<Value> {
        self.read()
            .lines()
            .filter_map(|l| serde_json::from_str::<Value>(l).ok())
            .filter(|v| v["event"] == name)
            .collect()
    }

    /// The `step.done` line of `step`, once it has one.
    fn step(&self, step: &str) -> Value {
        self.wait(&format!("step {step} done"), |d| {
            d.events("step.done")
                .into_iter()
                .find(|e| e["step"] == step)
        })
    }

    /// Wait for `f` to find something, for 30 s.
    fn wait<T>(&self, what: &str, f: impl Fn(&Daemon) -> Option<T>) -> T {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            if let Some(t) = f(self) {
                return t;
            }
            assert!(
                Instant::now() < deadline,
                "waited for {what}:\n{}",
                self.read()
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// Wait for the daemon to exit by itself (`run_until: drained`).
    fn exits(&mut self) -> Option<i32> {
        let deadline = Instant::now() + Duration::from_secs(30);
        while Instant::now() < deadline {
            if let Ok(Some(s)) = self.child.try_wait() {
                return s.code();
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        panic!("the daemon did not exit:\n{}", self.read())
    }

    /// SIGKILL: the process stops wherever it is.
    fn kill(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[cfg(feature = "x402")]
impl Drop for Daemon {
    fn drop(&mut self) {
        // SAFETY: a signal to the child this test spawned.
        unsafe { libc::kill(self.child.id() as i32, libc::SIGTERM) };
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if matches!(self.child.try_wait(), Ok(Some(_))) {
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        self.kill();
    }
}

/// A helper process, killed when the test ends — however it ends.
#[cfg(feature = "x402")]
struct Killed(Child);

#[cfg(feature = "x402")]
impl Drop for Killed {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// A scratch directory with the seal key in it.
#[cfg(feature = "x402")]
fn scratch() -> tempfile::TempDir {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let seal = dir.path().join("seal");
    std::fs::write(&seal, SEAL).unwrap();
    std::fs::set_permissions(&seal, std::fs::Permissions::from_mode(0o600)).unwrap();
    dir
}

/// The policy's `assets:` entry for USDC on Base Sepolia, with `limits` and
/// anything `more` adds.
#[cfg(feature = "x402")]
fn usdc(limits: &str, more: &str) -> String {
    format!(
        "      usdc:\n        asset: \"eip155:84532/erc20:{USDC}\"\n        decimals: 6\n        limits: {{{limits}}}\n{more}"
    )
}

/// `security.payments` with the wallet `ops` sealed in `dir`, the assets
/// `assets`, the payees `weather` and `other`, and `more` at its top.
#[cfg(feature = "x402")]
fn local(dir: &Path, assets: &str, more: &str) -> String {
    let d = dir.display();
    format!(
        "    seal_key: \"{{{{secret-file:{d}/seal}}}}\"\n{more}    wallets: {{ops: {{key_file: \"{d}/ops.key\"}}}}\n    assets:\n{assets}    payees:\n      weather: {{address: \"{PAYEE}\"}}\n      other: {{address: \"{OTHER}\"}}\n"
    )
}

/// A daemon's configuration: a file store and an audit chain in `dir`, the
/// `payments` block, `top` beside it and `steps` as the workflow `name`,
/// started once. A job that exits once its work is done, unless `top` says
/// otherwise; `top` may name the agent too.
#[cfg(feature = "x402")]
fn config(dir: &Path, payments: &str, top: &str, name: &str, steps: &str) -> PathBuf {
    let d = dir.display();
    let mut head = String::new();
    if !top.contains("agent:") {
        head.push_str("agent: {name: payer, preflight: never}\n");
    }
    if !top.contains("lifecycle:") {
        head.push_str("lifecycle: {run_until: idle, idle_grace: 1s}\n");
    }
    let text = format!(
        "{head}\
         intelligence: {{endpoints: \"mock:final\", model: mock}}\n\
         store: {{kind: file, file: {{path: \"{d}/state\"}}}}\n\
         observability: {{log_level: info, log_content: true, audit: {{chain: {{path: \"{d}/audit/audit.log\"}}}}}}\n\
         {top}\
         security:\n  payments:\n{payments}\
         workflows:\n  - name: {name}\n    steps:\n      s: {{kind: once}}\n{steps}"
    );
    let path = dir.join(format!("{name}.yaml"));
    std::fs::write(&path, text).unwrap();
    path
}

/// A paying `http` step to `url`, after `after`, paying at most `max`, with
/// `more` fields.
#[cfg(feature = "x402")]
fn paying(id: &str, after: &str, url: &str, max: &str, more: &str) -> String {
    format!(
        "      {id}: {{kind: http, depends_on: [{after}], url: \"{url}\", allow_private: true, payment: {{wallet: ops, max: \"{max} usdc\"}}{more}}}\n"
    )
}

/// The mock x402 server, with `knobs` over its defaults.
#[cfg(feature = "x402")]
fn mock(knobs: Value) -> agentd::payments::mock::Mock {
    agentd::payments::mock::start(&knobs).expect("start the mock x402 server")
}

#[cfg(feature = "x402")]
fn paid(m: &agentd::payments::mock::Mock) -> Vec<Value> {
    m.record()
        .into_iter()
        .filter(|r| r["paid"] == true)
        .collect()
}

/// `agentd --print-wallet-address ops`: the address it prints.
#[cfg(feature = "x402")]
fn wallet_address(cfg: &Path) -> String {
    address_of(cfg, "ops")
}

/// `agentd --print-wallet-address <wallet>`: the address it prints.
#[cfg(feature = "x402")]
fn address_of(cfg: &Path, wallet: &str) -> String {
    let out = Command::new(env!("CARGO_BIN_EXE_agentd"))
        .args([
            "--config",
            &cfg.display().to_string(),
            "--print-wallet-address",
            wallet,
        ])
        // The bearers an A2A configuration here names.
        .env("OPS", "ops-bearer-x402")
        .env("ALICE", "alice-bearer-x402")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// The `payment` records of the audit chain in `dir`, by outcome.
#[cfg(feature = "x402")]
fn audit(dir: &Path) -> Vec<Value> {
    std::fs::read_to_string(dir.join("audit/audit.log"))
        .unwrap_or_default()
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter(|r| r["action"] == "payment")
        .collect()
}

#[cfg(feature = "x402")]
fn outcomes(dir: &Path) -> Vec<String> {
    audit(dir)
        .iter()
        .map(|r| r["outcome"].as_str().unwrap_or_default().to_string())
        .collect()
}

/// `bytes` in base64 without padding, the standard alphabet or the URL-safe
/// one.
#[cfg(feature = "x402")]
fn base64(bytes: &[u8], url: bool) -> String {
    let abc: &[u8] = if url {
        b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_"
    } else {
        b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/"
    };
    let mut out = String::new();
    for c in bytes.chunks(3) {
        let n = (u32::from(c[0]) << 16)
            | (u32::from(*c.get(1).unwrap_or(&0)) << 8)
            | u32::from(*c.get(2).unwrap_or(&0));
        for i in 0..=c.len() {
            out.push(abc[((n >> (18 - 6 * i)) & 63) as usize] as char);
        }
    }
    out
}

/// Every file under `dir`, read as text where it is.
#[cfg(feature = "x402")]
fn every_file(dir: &Path, out: &mut Vec<(PathBuf, String)>) {
    for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let p = e.path();
        if p.is_dir() {
            every_file(&p, out);
        } else if let Ok(bytes) = std::fs::read(&p) {
            out.push((p, String::from_utf8_lossy(&bytes).into_owned()));
        }
    }
}

/// A workflow pays for an HTTP resource and gets it: one 402, one paid
/// request whose signature the server recovers to the wallet `agentd
/// --print-wallet-address` printed, the settlement in the step's output and
/// the audit chain — `authorized` before `settled` — and the key, the
/// signature and the signed header in no log line, no frame to a child and
/// no file but the sealed key's own.
#[cfg(feature = "x402")]
#[test]
fn a_workflow_pays_once_and_gets_the_resource() {
    use std::os::unix::fs::PermissionsExt;
    let dir = scratch();
    let m = mock(json!({}));
    let payments = local(
        dir.path(),
        &usdc(
            "per_payment: \"0.05\", per_run: \"0.10\", per_day: \"1\", per_payee: \"1\"",
            "",
        ),
        "",
    );
    let steps = format!(
        "{}      read: {{kind: subagent, depends_on: [get], instruction: \"Summarize {{{{ steps.get.output }}}}\"}}\n      done: {{kind: finish, depends_on: [read], output: \"{{{{ steps.get.output }}}}\"}}\n",
        paying(
            "get",
            "s",
            &format!("{}/premium-data?ticker=X", m.url),
            "0.02",
            ""
        )
    );
    let cfg = config(dir.path(), &payments, "", "buy", &steps);
    let address = wallet_address(&cfg);
    let key_file = dir.path().join("ops.key");
    assert_eq!(
        std::fs::metadata(&key_file).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert_eq!(
        wallet_address(&cfg),
        address,
        "the key file, once made, is read"
    );
    let frames = dir.path().join("frames.jsonl");
    let mut d = Daemon::spawn(
        &cfg,
        &[("AGENTD_TEST_FRAMES_FILE", &frames.display().to_string())],
    );
    let done = d.wait("run.done", |d| d.events("run.done").into_iter().next());
    assert_eq!(done["status"], "completed", "{}", d.read());
    let out = &done["output"];
    assert_eq!(out["status"], 200, "{out}");
    assert_eq!(out["json"]["data"], "premium market data", "{out}");
    let p = &out["payment"];
    assert_eq!(p["state"], "settled", "{p}");
    assert_eq!(p["payer"], address.as_str());
    assert_eq!(
        (p["asset"].clone(), p["amount"].clone()),
        (json!("usdc"), json!("10000"))
    );
    assert_eq!(p["payee"], "weather");
    assert_eq!(d.exits(), Some(0));

    // The server saw the request twice: unpaid, then paid by the wallet,
    // the entry it offered echoed exactly, for the resource it named.
    let rec = m.record();
    assert_eq!(rec.len(), 2, "{rec:#?}");
    assert_eq!(rec[0]["paid"], false);
    assert_eq!(rec[1]["valid"], true, "{:#?}", rec[1]);
    assert_eq!(rec[1]["from"], address.as_str());
    assert_eq!(rec[1]["path"], "/premium-data?ticker=X");
    assert_eq!(p["transaction"], rec[1]["response"]["transaction"]);
    assert_eq!(p["nonce"], rec[1]["nonce"]);
    let payment = &rec[1]["payment"];
    assert_eq!(payment["accepted"]["payTo"], PAYEE);

    // The chain: authorized, then settled, each naming the run, the step
    // and the nonce.
    let chain = audit(dir.path());
    assert_eq!(
        outcomes(dir.path()),
        ["authorized", "settled"],
        "{chain:#?}"
    );
    for r in &chain {
        assert_eq!(r["target"]["step"], "get");
        assert_eq!(r["target"]["nonce"], p["nonce"]);
        assert_eq!(r["target"]["workflow"], "buy");
    }
    assert_eq!(chain[1]["target"]["transaction"], p["transaction"]);
    let log = d.read();
    assert!(
        log.find("\"event\":\"payment.sent\"") < log.find("\"event\":\"payment.settled\""),
        "{log}"
    );

    // What must never be anywhere but the sealed file: the key, the
    // signature, the signed header.
    let seal = agentd::auth::seal::SealKey::new(&[7u8; 32]).unwrap();
    let (_, key) = agentd::payments::wallet::read(&key_file, &seal, "ops").unwrap();
    let raw = key.to_bytes();
    let signature = payment["payload"]["signature"]
        .as_str()
        .unwrap()
        .to_string();
    let secrets = [
        agentd::sha::to_hex(&raw),
        base64(&raw, false),
        base64(&raw, true),
        signature.trim_start_matches("0x").to_string(),
        rec[1]["header"].as_str().unwrap().to_string(),
    ];
    let mut files = Vec::new();
    every_file(dir.path(), &mut files);
    files.push((d.log.clone(), log.clone()));
    let spawns = std::fs::read_to_string(&frames).unwrap_or_default();
    assert!(
        spawns.contains("\"dir\":\"spawn\"") && spawns.contains("premium market data"),
        "the subagent was spawned and handed the step's output: {spawns}"
    );
    for (path, text) in &files {
        if *path == key_file || *path == dir.path().join("seal") {
            continue;
        }
        for s in &secrets {
            assert!(
                !text.contains(s.as_str()),
                "{} holds a key, a signature or a signed header",
                path.display()
            );
        }
    }
}

/// On Solana: a workflow pays for an HTTP resource with an SPL transfer
/// from a Solana wallet. The server's entry names no blockhash, so agentd
/// asks the network's RPC endpoint for one; it signs the transfer the
/// entry asks for, the sponsor verifies it — its payer the wallet `agentd
/// --print-wallet-address` printed, its memo the record's nonce — and
/// settles it; the chain has `authorized` then `settled`; and the seed and
/// the signed transaction are in no log line and no file but the key's.
#[cfg(feature = "x402")]
#[test]
fn a_workflow_pays_on_solana_and_gets_the_resource() {
    const MAINNET: &str = "solana:5eykt4UsFv8P8NJdTREpY1vzqKqZKvdp";
    const MINT: &str = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v";
    const PAY_TO: &str = "2wKupLR9q6wXYppw8Gr2NvWxKBUqm4PPJKkQfoxHDBg4";
    let dir = scratch();
    let m = mock(json!({"network": MAINNET, "asset": MINT, "pay_to": PAY_TO, "blockhash": null}));
    let d = dir.path().display();
    let payments = format!(
        "    seal_key: \"{{{{secret-file:{d}/seal}}}}\"\n\
         \x20   wallets: {{sol: {{chain: solana, key_file: \"{d}/sol.key\"}}}}\n\
         \x20   assets:\n      usdc-sol:\n        asset: \"{MAINNET}/token:{MINT}\"\n        decimals: 6\n\
         \x20       token_program: spl-token\n\
         \x20       limits: {{per_payment: \"0.05\", per_run: \"0.10\", per_day: \"1\", per_payee: \"1\"}}\n\
         \x20   payees:\n      weather: {{address: \"{PAY_TO}\"}}\n\
         \x20   rpc:\n      \"{MAINNET}\": \"{}/_mock/rpc\"\n",
        m.url
    );
    let steps = format!(
        "      get: {{kind: http, depends_on: [s], url: \"{}/premium-data\", allow_private: true, payment: {{wallet: sol, max: \"0.02 usdc-sol\"}}}}\n      done: {{kind: finish, depends_on: [get], output: \"{{{{ steps.get.output }}}}\"}}\n",
        m.url
    );
    let cfg = config(dir.path(), &payments, "", "buy", &steps);
    let address = address_of(&cfg, "sol");
    assert!(
        (32..=44).contains(&address.len()) && !address.starts_with("0x"),
        "a base58 address: {address}"
    );
    let mut daemon = Daemon::spawn(&cfg, &[]);
    let done = daemon.wait("run.done", |d| d.events("run.done").into_iter().next());
    assert_eq!(done["status"], "completed", "{}", daemon.read());
    let p = &done["output"]["payment"];
    assert_eq!(p["state"], "settled", "{p}");
    assert_eq!(p["payer"], address.as_str());
    assert_eq!(p["pay_to"], PAY_TO);
    assert_eq!(p["network"], MAINNET);
    assert_eq!(daemon.exits(), Some(0));

    // The RPC was asked for a blockhash, then the server saw the request
    // unpaid and paid by the wallet, for the transfer it offered.
    let rec = m.record();
    assert_eq!(rec.len(), 3, "{rec:#?}");
    assert_eq!(rec[0]["paid"], false);
    assert_eq!(rec[1]["rpc"], "getLatestBlockhash", "{:#?}", rec[1]);
    assert_eq!(rec[1]["params"][0]["commitment"], "finalized");
    assert_eq!(rec[2]["valid"], true, "{:#?}", rec[2]);
    assert_eq!(rec[2]["from"], address.as_str());
    assert_eq!(p["transaction"], rec[2]["response"]["transaction"]);
    assert_eq!(
        p["nonce"], rec[2]["nonce"],
        "the record's nonce is the memo"
    );
    assert_eq!(
        p["nonce"].as_str().unwrap().len(),
        32,
        "16 random bytes in hex"
    );
    assert_eq!(outcomes(dir.path()), ["authorized", "settled"]);

    // The seed and the signed transaction are nowhere but the key file.
    let seal = agentd::auth::seal::SealKey::new(&[7u8; 32]).unwrap();
    let key_file = dir.path().join("sol.key");
    assert!(agentd::payments::wallet::read_svm(&key_file, &seal, "sol").is_ok());
    let transaction = rec[2]["payment"]["payload"]["transaction"]
        .as_str()
        .unwrap()
        .to_string();
    let mut files = Vec::new();
    every_file(dir.path(), &mut files);
    files.push((daemon.log.clone(), daemon.read()));
    for (path, text) in &files {
        if *path == key_file || *path == dir.path().join("seal") {
            continue;
        }
        for s in [&transaction, rec[2]["header"].as_str().unwrap()] {
            assert!(
                !text.contains(s),
                "{} holds the signed transaction",
                path.display()
            );
        }
    }
}

/// ADR 0003: "a payment outside them refused unsent". Each entry the policy
/// does not allow — past the run's cap, over a payment's cap and the
/// step's `max`, for a payee, an asset or a network not on its lists, or
/// for another resource — is refused naming why, the chain records it, and
/// the server never sees a payment.
#[cfg(feature = "x402")]
#[test]
fn a_payment_outside_the_policy_is_refused_unsent() {
    let dir = scratch();
    let ok = mock(json!({"price": "30000"}));
    let cases: Vec<(&str, agentd::payments::mock::Mock, &str)> = vec![
        ("run_cap", mock(json!({"price": "30000"})), "per_run:"),
        (
            "payee",
            mock(json!({"pay_to": "0x000000000000000000000000000000000000dEaD"})),
            "not an allowed payee",
        ),
        (
            "asset",
            mock(json!({"asset": "0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913"})),
            "not an allowed asset",
        ),
        (
            "network",
            mock(json!({"network": "eip155:1"})),
            "no allowed asset is on it",
        ),
        (
            "solana",
            mock(json!({"network": "solana:5eykt4UsFv8P8NJdTREpY1vzqKqZKvdp"})),
            "no allowed asset is on it",
        ),
        (
            "bitcoin",
            mock(json!({"network": "bip122:000000000019d6689c085ae165831e93"})),
            "no chain of this kind",
        ),
        ("too_much", mock(json!({"price": "60000"})), "per_payment:"),
        (
            "elsewhere",
            mock(json!({"resource_url": "http://127.0.0.1:9/other"})),
            "resource_mismatch",
        ),
    ];
    let mut steps = paying("ok", "s", &format!("{}/a", ok.url), "0.05", "");
    let mut after = "ok".to_string();
    for (id, m, _) in &cases {
        steps.push_str(&paying(
            id,
            &after,
            &format!("{}/{id}", m.url),
            "0.05",
            ", on_error: continue",
        ));
        after = id.to_string();
    }
    steps.push_str(&format!(
        "      done: {{kind: finish, depends_on: [{after}]}}\n"
    ));
    let payments = local(
        dir.path(),
        &usdc(
            "per_payment: \"0.05\", per_run: \"0.05\", per_day: \"1\", per_payee: \"1\"",
            "",
        ),
        "    ask_when_tainted: false\n",
    );
    let cfg = config(dir.path(), &payments, "", "buy", &steps);
    wallet_address(&cfg);
    let mut d = Daemon::spawn(&cfg, &[]);
    d.wait("run.done", |d| d.events("run.done").into_iter().next());
    assert_eq!(d.exits(), Some(0));
    assert_eq!(paid(&ok).len(), 1, "the first payment is within everything");
    let refused = d.events("payment.refused");
    for (id, m, why) in &cases {
        let line = refused
            .iter()
            .find(|e| e["step"] == *id)
            .unwrap_or_else(|| panic!("{id} was not refused:\n{}", d.read()));
        let text = line["why"].as_str().unwrap();
        assert!(
            text.contains("refused unsent") && text.contains(why),
            "{id}: {text}"
        );
        assert!(paid(m).is_empty(), "{id}: the server saw a payment");
        assert_eq!(m.record().len(), 1, "{id}: one request, unpaid");
        let step = d.step(id);
        assert!(step["err"].as_str().unwrap().contains(why), "{step}");
    }
    let too_much = refused.iter().find(|e| e["step"] == "too_much").unwrap();
    assert!(
        too_much["why"].as_str().unwrap().contains("max:"),
        "the step's own bound too"
    );
    assert_eq!(
        outcomes(dir.path())
            .iter()
            .filter(|o| *o == "refused")
            .count(),
        cases.len(),
        "each refusal is a record in the chain"
    );
}

/// The check and the reservation are one step on the supervisor's loop:
/// four elements of a `foreach`, all asked to pay at once, never pass a cap
/// together — the run's cap holds two payments, and two are made.
#[cfg(feature = "x402")]
#[test]
fn concurrent_steps_never_pass_a_cap_together() {
    let dir = scratch();
    let m = mock(json!({"price": "20000"}));
    let payments = local(
        dir.path(),
        &usdc("per_run: \"0.05\", per_day: \"1\", per_payee: \"1\"", ""),
        "    ask_when_tainted: false\n",
    );
    let steps = format!(
        "      each:\n        kind: foreach\n        depends_on: [s]\n        over: [1, 2, 3, 4]\n        batch: {{parallel: 4}}\n        on_error: continue\n        body:\n          steps:\n{}      done: {{kind: finish, depends_on: [each]}}\n",
        paying("pay", "[]", &format!("{}/item", m.url), "0.05", "")
            .replace("depends_on: [[]], ", "")
            .replace("      pay:", "            pay:")
    );
    let cfg = config(dir.path(), &payments, "", "buy", &steps);
    wallet_address(&cfg);
    let mut d = Daemon::spawn(&cfg, &[]);
    d.wait("run.done", |d| d.events("run.done").into_iter().next());
    assert_eq!(d.exits(), Some(0));
    assert_eq!(paid(&m).len(), 2, "{:#?}", m.record());
    let refused = d.events("payment.refused");
    assert_eq!(refused.len(), 2, "{}", d.read());
    for r in &refused {
        assert!(r["why"].as_str().unwrap().contains("per_run:"), "{r}");
    }
}

/// ADR 0003: caps "per day and per payee". They count every payment that
/// left agentd, across runs and across a restart: the ledger is in the
/// store, read again at start.
#[cfg(feature = "x402")]
#[test]
fn the_day_and_payee_caps_hold_across_runs_and_a_restart() {
    let dir = scratch();
    let to_weather = mock(json!({"price": "30000"}));
    let again = mock(json!({"price": "30000"}));
    let to_other = mock(json!({"price": "30000", "pay_to": OTHER}));
    let payments = local(
        dir.path(),
        &usdc("per_run: \"1\", per_day: \"0.05\", per_payee: \"0.04\"", ""),
        "    ask_when_tainted: false\n",
    );
    let steps = format!(
        "{}{}{}      done: {{kind: finish, depends_on: [c]}}\n",
        paying("a", "s", &format!("{}/a", to_weather.url), "0.05", ""),
        paying(
            "b",
            "a",
            &format!("{}/b", again.url),
            "0.05",
            ", on_error: continue"
        ),
        paying(
            "c",
            "b",
            &format!("{}/c", to_other.url),
            "0.05",
            ", on_error: continue"
        ),
    );
    let cfg = config(dir.path(), &payments, "", "buy", &steps);
    wallet_address(&cfg);
    let mut d = Daemon::spawn(&cfg, &[]);
    assert_eq!(d.step("a")["status"], "done", "{}", d.read());
    let b = d.step("b")["err"].as_str().unwrap().to_string();
    assert!(b.contains("per_payee:") && b.contains("per_day:"), "{b}");
    let c = d.step("c")["err"].as_str().unwrap().to_string();
    assert!(c.contains("per_day:") && !c.contains("per_payee:"), "{c}");
    assert_eq!(d.exits(), Some(0));
    drop(d);

    // Another daemon on the same store, another workflow: the day's 0.03
    // is still counted.
    let later = mock(json!({"price": "30000", "pay_to": OTHER}));
    let steps = format!(
        "{}      done: {{kind: finish, depends_on: [l]}}\n",
        paying(
            "l",
            "s",
            &format!("{}/l", later.url),
            "0.05",
            ", on_error: continue"
        )
    );
    let cfg = config(dir.path(), &payments, "", "later", &steps);
    let mut d = Daemon::spawn(&cfg, &[]);
    let l = d.step("l")["err"].as_str().unwrap().to_string();
    assert!(
        l.contains("per_day:") && l.contains("0.03 usdc in the last 24 hours"),
        "{l}"
    );
    assert!(paid(&later).is_empty());
    assert_eq!(d.exits(), Some(0));
}

/// ADR 0003: "the User's approval above a threshold". Above
/// `approve_above`, or for work that read outside text, the approver is
/// asked; with no A2A listener nobody can be, and the payment is refused
/// unsent (`no_approver`).
#[cfg(feature = "x402")]
#[test]
fn a_payment_that_needs_an_approver_nobody_can_be_asked_for_is_refused() {
    let dir = scratch();
    let big = mock(json!({"price": "30000"}));
    let small = mock(json!({"price": "10000"}));
    let payments = local(
        dir.path(),
        &usdc(
            "per_run: \"1\", per_day: \"1\", per_payee: \"1\"",
            "        approve_above: \"0.02\"\n",
        ),
        "",
    );
    let steps = format!(
        "{}      peek: {{kind: http, depends_on: [big], url: \"{}/_mock/record\", allow_private: true}}\n{}      done: {{kind: finish, depends_on: [small]}}\n",
        paying(
            "big",
            "s",
            &format!("{}/big", big.url),
            "0.05",
            ", on_error: continue"
        ),
        small.url,
        paying(
            "small",
            "peek",
            &format!("{}/small", small.url),
            "0.05",
            ", on_error: continue"
        ),
    );
    let cfg = config(dir.path(), &payments, "", "buy", &steps);
    wallet_address(&cfg);
    let mut d = Daemon::spawn(&cfg, &[]);
    let big_err = d.step("big")["err"].as_str().unwrap().to_string();
    assert!(
        big_err.contains("no_approver") && big_err.contains("above approve_above 0.02 usdc"),
        "{big_err}"
    );
    let small_err = d.step("small")["err"].as_str().unwrap().to_string();
    assert!(
        small_err.contains("no_approver") && small_err.contains("outside text"),
        "work that read an http response asks, whatever the amount: {small_err}"
    );
    assert!(paid(&big).is_empty() && paid(&small).is_empty());
    assert_eq!(d.exits(), Some(0));
}

/// ADR 0003: "through an addressed human gate no model or judge answers".
/// The gate is the approver's: an operator who is not the approver is
/// refused and the gate stays open; the approver's `decline` pays nothing,
/// their `approve` pays once — the chain records each.
#[cfg(all(feature = "x402", feature = "a2a"))]
#[test]
fn above_approve_above_only_the_approver_answers() {
    let dir = scratch();
    let first = mock(json!({"price": "30000"}));
    let second = mock(json!({"price": "30000"}));
    let payments = local(
        dir.path(),
        &usdc(
            "per_run: \"1\", per_day: \"1\", per_payee: \"1\"",
            "        approve_above: \"0.02\"\n",
        ),
        "    approver: \"user:alice\"\n",
    );
    let steps = format!(
        "{}{}      done: {{kind: finish, depends_on: [b]}}\n",
        paying(
            "a",
            "s",
            &format!("{}/a", first.url),
            "0.05",
            ", on_error: continue"
        ),
        paying("b", "a", &format!("{}/b", second.url), "0.05", ""),
    );
    let (d, addr) = common::spawn_bound(|port| {
        let top = format!(
            "lifecycle: {{run_until: drained}}\na2a:\n  listen: \"http://127.0.0.1:{port}\"\n  bearer: \"{{{{secret:OPS}}}}\"\n  principals:\n    - {{id: alice, match: {{bearer_ref: \"{{{{secret:ALICE}}}}\"}}, role: user}}\n"
        );
        let cfg = config(dir.path(), &payments, &top, "buy", &steps);
        if !dir.path().join("ops.key").exists() {
            wallet_address(&cfg);
        }
        let d = Daemon::spawn(
            &cfg,
            &[("OPS", "ops-bearer-x402"), ("ALICE", "alice-bearer-x402")],
        );
        let log = d.log.display().to_string();
        (d, log)
    });
    let asked = |step: &str| {
        d.wait(&format!("payment.asked for {step}"), |d| {
            d.events("payment.asked")
                .into_iter()
                .find(|e| e["step"] == step)
        })["task"]
            .as_str()
            .unwrap()
            .to_string()
    };
    let task = asked("a");
    // The question is agentd's, the request it names quoted.
    let shown = common::rpc_as(
        &addr,
        "alice-bearer-x402",
        7,
        "GetTask",
        json!({"id": task}),
    );
    let text = shown["result"]["status"]["message"]["parts"][0]["text"]
        .as_str()
        .unwrap_or_default();
    assert!(
        text.contains(&format!(
            " for \"GET {}/a\", from the wallet ops",
            first.url
        )),
        "{shown}"
    );
    // The operator is not the approver.
    let v = common::SendMessage::text("approve")
        .task(&task)
        .bearer("ops-bearer-x402")
        .post(&addr);
    let (_, msg) = common::error_of(&v);
    assert!(msg.contains("approval is for user:alice"), "{v}");
    assert!(paid(&first).is_empty());
    // The approver declines: nothing is paid.
    let v = common::SendMessage::text("decline")
        .task(&task)
        .bearer("alice-bearer-x402")
        .post(&addr);
    assert!(v.get("error").is_none(), "{v}");
    let a = d.step("a");
    assert!(
        a["err"]
            .as_str()
            .unwrap()
            .contains("declined by user:alice"),
        "{a}"
    );
    // The next one asked, the approver approves: paid once.
    let task = asked("b");
    let v = common::SendMessage::text("approve")
        .task(&task)
        .bearer("alice-bearer-x402")
        .post(&addr);
    assert!(v.get("error").is_none(), "{v}");
    assert_eq!(d.step("b")["status"], "done", "{}", d.read());
    assert!(paid(&first).is_empty());
    assert_eq!(paid(&second).len(), 1);
    // A late answer on a closed gate answers nothing.
    let v = common::SendMessage::text("approve")
        .task(&task)
        .bearer("alice-bearer-x402")
        .post(&addr);
    assert!(v.get("error").is_some(), "{v}");
    let o = outcomes(dir.path());
    assert_eq!(
        o,
        [
            "asked",
            "not_addressed",
            "declined",
            "asked",
            "approved",
            "authorized",
            "settled"
        ],
        "{o:?}"
    );
    let approved = audit(dir.path())
        .into_iter()
        .find(|r| r["outcome"] == "authorized")
        .unwrap();
    assert_eq!(approved["target"]["approved_by"], "user:alice");
}

/// ADR 0003: "a payment whose outcome is unknown is never made a second
/// time". A settlement still pending, a server that never answers the paid
/// request: the step fails, and its `retry:` sends nothing — not even the
/// unpaid request. A payment the server refused may be made again, but its
/// authorization, still valid, counts against the run's cap.
#[cfg(feature = "x402")]
#[test]
fn a_payment_whose_outcome_is_unknown_is_never_made_twice() {
    let dir = scratch();
    let pending = mock(json!({"settle": "pending"}));
    let stall = mock(json!({"settle": "stall", "stall_ms": 5000}));
    let silent = mock(json!({"settle": "none"}));
    let refused = mock(json!({"settle": "fail", "price": "30000"}));
    let retry = ", retry: {max: 1, backoff: 100ms}, on_error: continue";
    let steps = format!(
        "{}{}{}{}      done: {{kind: finish, depends_on: [refused]}}\n",
        paying("pending", "s", &format!("{}/p", pending.url), "0.05", retry),
        paying(
            "stall",
            "pending",
            &format!("{}/s", stall.url),
            "0.05",
            &format!("{retry}, timeout: 1s")
        ),
        paying(
            "silent",
            "stall",
            &format!("{}/n", silent.url),
            "0.05",
            retry
        ),
        paying(
            "refused",
            "silent",
            &format!("{}/r", refused.url),
            "0.05",
            retry
        ),
    );
    let payments = local(
        dir.path(),
        &usdc("per_run: \"0.08\", per_day: \"1\", per_payee: \"1\"", ""),
        "    ask_when_tainted: false\n",
    );
    let cfg = config(dir.path(), &payments, "", "buy", &steps);
    wallet_address(&cfg);
    let mut d = Daemon::spawn(&cfg, &[]);
    d.wait("run.done", |d| d.events("run.done").into_iter().next());
    let attempts = |step: &str| -> Vec<String> {
        d.events("step.done")
            .into_iter()
            .filter(|e| e["step"] == step)
            .map(|e| e["err"].as_str().unwrap_or_default().to_string())
            .collect()
    };
    for (step, m) in [
        ("pending", &pending),
        ("stall", &stall),
        ("silent", &silent),
    ] {
        let errs = attempts(step);
        assert_eq!(errs.len(), 2, "{step}: {errs:?}");
        assert!(
            errs[0].contains("payment outcome unknown"),
            "{step}: {}",
            errs[0]
        );
        assert!(errs[1].contains("payment_unknown"), "{step}: {}", errs[1]);
        assert_eq!(
            m.record().len(),
            2,
            "{step}: one unpaid request, one paid, and no more"
        );
    }
    let errs = attempts("refused");
    assert!(
        errs[0].contains("did not settle it (insufficient_funds)"),
        "{errs:?}"
    );
    assert!(
        errs[1].contains("per_run:") && errs[1].contains("refused unsent"),
        "the refused authorization still counts: {errs:?}"
    );
    assert_eq!(paid(&refused).len(), 1);
    assert_eq!(d.exits(), Some(0));
}

/// A restart mid-payment: the signed request was out when agentd was
/// killed. The next life reads its record as unknown, says so — in the log
/// and in the audit chain, which closes the `authorized` record — and the
/// step replayed sends nothing.
#[cfg(feature = "x402")]
#[test]
fn a_restart_mid_payment_never_pays_again() {
    let dir = scratch();
    let m = mock(json!({"settle": "stall", "stall_ms": 60_000}));
    let payments = local(
        dir.path(),
        &usdc("per_run: \"1\", per_day: \"1\", per_payee: \"1\"", ""),
        "    ask_when_tainted: false\n",
    );
    let steps = format!(
        "{}      done: {{kind: finish, depends_on: [get]}}\n",
        paying(
            "get",
            "s",
            &format!("{}/data", m.url),
            "0.05",
            ", timeout: 60s"
        )
    );
    let cfg = config(dir.path(), &payments, "", "buy", &steps);
    wallet_address(&cfg);
    let mut d = Daemon::spawn(&cfg, &[]);
    d.wait("the paid request", |_| (paid(&m).len() == 1).then_some(()));
    d.kill();
    assert_eq!(outcomes(dir.path()), ["authorized"]);
    let d = Daemon::spawn(&cfg, &[]);
    let restored = d.wait("payment.ledger.restored", |d| {
        d.events("payment.ledger.restored").into_iter().next()
    });
    assert_eq!(restored["step"], "get");
    let err = d.step("get")["err"].as_str().unwrap().to_string();
    assert!(err.contains("payment_unknown"), "{err}");
    assert_eq!(m.record().len(), 2, "nothing more reached the server");
    assert_eq!(
        outcomes(dir.path()),
        ["authorized", "unknown", "refused"],
        "the replay is refused before it sends"
    );
    let unknown = audit(dir.path())
        .into_iter()
        .find(|r| r["outcome"] == "unknown")
        .unwrap();
    assert_eq!(unknown["target"]["step"], "get");
    assert_eq!(unknown["target"]["state"], "unknown");
    assert_eq!(
        unknown["target"]["error_reason"],
        "agentd stopped while the payment was out"
    );
}

/// A gate a restart interrupts ends its step, nothing paid, and the chain
/// closes the `asked` record with `interrupted`.
#[cfg(all(feature = "x402", feature = "a2a"))]
#[test]
fn a_gate_a_restart_interrupts_pays_nothing_and_the_chain_says_so() {
    let dir = scratch();
    let m = mock(json!({"price": "30000"}));
    let payments = local(
        dir.path(),
        &usdc(
            "per_run: \"1\", per_day: \"1\", per_payee: \"1\"",
            "        approve_above: \"0.02\"\n",
        ),
        "",
    );
    let steps = format!(
        "{}      done: {{kind: finish, depends_on: [get]}}\n",
        paying("get", "s", &format!("{}/data", m.url), "0.05", "")
    );
    let start = || {
        common::spawn_bound(|port| {
            let top = format!(
                "lifecycle: {{run_until: drained}}\na2a:\n  listen: \"http://127.0.0.1:{port}\"\n"
            );
            let cfg = config(dir.path(), &payments, &top, "buy", &steps);
            if !dir.path().join("ops.key").exists() {
                wallet_address(&cfg);
            }
            let d = Daemon::spawn(&cfg, &[]);
            let log = d.log.display().to_string();
            (d, log)
        })
        .0
    };
    let mut d = start();
    d.wait("payment.asked", |d| {
        d.events("payment.asked").into_iter().next()
    });
    d.kill();
    let d = start();
    let err = d.step("get")["err"].as_str().unwrap().to_string();
    assert!(err.contains("payment interrupted"), "{err}");
    assert!(paid(&m).is_empty());
    assert_eq!(outcomes(dir.path()), ["asked", "interrupted"]);
    let r = audit(dir.path())
        .into_iter()
        .find(|r| r["outcome"] == "interrupted")
        .unwrap();
    assert_eq!(r["target"]["step"], "get");
    assert_eq!(r["target"]["workflow"], "buy");
    assert!(r["target"]["task"].is_string(), "{r}");
}

/// The external signer (RFC 0049 D8): agentd holds no wallet key; the
/// signer derives the address from its own key, is sent the typed data and
/// signs only the EIP-3009 shape its policy allows — here one wallet's key
/// is held to another chain's token, and its payment is never sent.
#[cfg(feature = "x402")]
#[test]
fn the_external_signer_signs_only_what_its_policy_allows() {
    let dir = scratch();
    let d = dir.path().display();
    let socket = dir.path().join("signer.sock");
    let record = dir.path().join("signer.jsonl");
    let knobs = json!({
        "keys": {
            "w1": {"alg": "ES256K", "assets": [format!("eip155:84532/erc20:{USDC}")],
                   "payees": [PAYEE], "max_value": "50000", "max_validity": 600},
            "w2": {"alg": "ES256K", "assets": ["eip155:8453/erc20:0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913"]},
        },
        "record": record,
    });
    let _signer = Killed(
        Command::new(env!("CARGO_BIN_EXE_agentd"))
            .args([
                "--internal-mock-signer",
                &socket.display().to_string(),
                "--knobs",
                &knobs.to_string(),
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    common::wait_for_file(&socket.display().to_string());
    let uid = unsafe { libc::geteuid() };
    let good = mock(json!({}));
    let bad = mock(json!({}));
    let payments = format!(
        "    key_custody: external\n    signer: {{socket: \"{d}/signer.sock\", uid: {uid}}}\n    ask_when_tainted: false\n    wallets:\n      ops: {{key: w1}}\n      wrong: {{key: w2}}\n    assets:\n{}    payees:\n      weather: {{address: \"{PAYEE}\"}}\n",
        usdc("per_run: \"1\", per_day: \"1\", per_payee: \"1\"", "")
    );
    let steps = format!(
        "{}      b: {{kind: http, depends_on: [a], url: \"{}/b\", allow_private: true, payment: {{wallet: wrong, max: \"0.05 usdc\"}}, on_error: continue}}\n      done: {{kind: finish, depends_on: [b]}}\n",
        paying("a", "s", &format!("{}/a", good.url), "0.05", ""),
        bad.url
    );
    let cfg = config(dir.path(), &payments, "", "buy", &steps);
    let address = wallet_address(&cfg);
    let mut dm = Daemon::spawn(&cfg, &[]);
    assert_eq!(dm.step("a")["status"], "done", "{}", dm.read());
    let rec = paid(&good);
    assert_eq!(rec.len(), 1);
    assert_eq!(rec[0]["from"], address.as_str(), "the signer's key paid");
    let b = dm.step("b")["err"].as_str().unwrap().to_string();
    assert!(
        b.contains("payment not sent") && b.contains("denied"),
        "{b}"
    );
    assert!(
        paid(&bad).is_empty(),
        "a signature the signer refused is never sent"
    );
    assert_eq!(dm.exits(), Some(0));
    let asked: Vec<Value> = std::fs::read_to_string(&record)
        .unwrap()
        .lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect();
    assert!(
        asked
            .iter()
            .any(|r| r["op"] == "sign_typed_data" && r["key"] == "w2" && r["denied"].is_string()),
        "{asked:?}"
    );
    assert!(
        !dir.path().join("ops.key").exists(),
        "agentd holds no wallet key"
    );
}

/// RFC 0042, RFC 0049 D2: a served document configures no payment — not
/// `security.payments` in a `:::!config` fragment, and not a paying step in
/// a `:::!workflow` — and a step's `payment` is literal.
#[cfg(feature = "x402")]
#[test]
fn a_served_document_configures_no_payment() {
    let dir = scratch();
    let payments = local(
        dir.path(),
        &usdc("per_run: \"1\", per_day: \"1\", per_payee: \"1\"", ""),
        "",
    );
    let validate = |top: &str, steps: &str| {
        let cfg = config(dir.path(), &payments, top, "buy", steps);
        let out = Command::new(env!("CARGO_BIN_EXE_agentd"))
            .args(["--config", &cfg.display().to_string(), "--validate-config"])
            .output()
            .unwrap();
        (
            out.status.code(),
            String::from_utf8_lossy(&out.stderr).into_owned(),
        )
    };
    let plain = "      done: {kind: finish, depends_on: [s]}\n";
    let (code, err) = validate("", plain);
    assert_eq!(code, Some(0), "{err}");
    let doc = dir.path().join("desk.md");
    std::fs::write(
        &doc,
        "You buy data.\n\n:::!workflow{name=buyer}\nsteps:\n  s: {kind: manual}\n  get: {kind: http, depends_on: [s], url: \"https://api.example/data\", payment: {wallet: ops, max: \"0.01 usdc\"}}\n:::\n",
    )
    .unwrap();
    let (code, err) = validate(
        &format!(
            "agent: {{name: payer, preflight: never, instruction: {}}}\n",
            doc.display()
        ),
        plain,
    );
    assert_eq!(code, Some(2), "{err}");
    assert!(
        err.contains("a served document configures no payment"),
        "{err}"
    );
    std::fs::write(
        &doc,
        "You buy data.\n\n:::!config\nsecurity:\n  payments:\n    approver: \"user:mallory\"\n:::\n",
    )
    .unwrap();
    let (code, err) = validate(
        &format!(
            "agent: {{name: payer, preflight: never, instruction: {}}}\n",
            doc.display()
        ),
        plain,
    );
    assert_eq!(code, Some(2), "{err}");
    assert!(err.contains("security"), "{err}");
    let (code, err) = validate(
        "",
        &paying(
            "get",
            "s",
            "https://api.example/{{ inputs.which }}",
            "{{ inputs.max }}",
            "",
        ),
    );
    assert_eq!(code, Some(2), "{err}");
    assert!(err.contains("payment is literal"), "{err}");
}

/// A build without the feature refuses the policy and every paying step by
/// name, rather than loading what it cannot do.
#[cfg(not(feature = "x402"))]
#[test]
fn without_the_feature_a_spend_policy_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = dir.path().join("c.yaml");
    std::fs::write(
        &cfg,
        "agent: {name: payer, preflight: never}\nstore: {kind: memory}\nsecurity:\n  payments:\n    approver: operator\nworkflows:\n  - name: buy\n    steps:\n      s: {kind: manual}\n      get: {kind: http, depends_on: [s], url: \"https://api.example/data\", payment: {wallet: ops, max: \"0.01 usdc\"}}\n      done: {kind: finish, depends_on: [get]}\n",
    )
    .unwrap();
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_agentd"))
        .args(["--config", &cfg.display().to_string(), "--validate-config"])
        .output()
        .unwrap();
    let err = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(2), "{err}");
    assert!(
        err.contains("security.payments needs the `x402` feature")
            && err.contains("payment needs the `x402` feature"),
        "{err}"
    );
}
