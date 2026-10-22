// SPDX-License-Identifier: AGPL-3.0-only
//! agentd paying for an HTTP resource with x402 v2 (RFC 0049), held to the
//! x402 specification pinned at `e7a3b4ff47` and to ADR 0003's spend
//! policy, each check quoting the sentence it proves. The resource server is
//! the mock x402 server (`agentd --internal-mock-x402`): it answers 402 with
//! a `PAYMENT-REQUIRED`, recovers the signer of every `PAYMENT-SIGNATURE`
//! and holds the authorization to what it offered, and records what reached
//! it. agentd is read from what it logged, what it wrote to its audit chain
//! and what crossed to a child (`AGENTD_TEST_FRAMES_FILE`).

use std::io::{Read, Write};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use crate::checks::util::{free_port, rpc_as, text_params, wait_ready, write_file};
use crate::harness::TempDir;
use crate::{Category, Check, Harness, Outcome};
use serde_json::{Value, json};

/// USDC on Base Sepolia, the mock's default token.
const USDC: &str = "0x036CbD53842c5426634e7929541eC2318f3dCF7e";
/// The mock's default `payTo`, the payee the policy allows.
const PAYEE: &str = "0x209693Bc6afc0C5328bA36FaF03C514EF312287C";

pub fn checks() -> Vec<Check> {
    vec![
        Check {
            id: "payments/pays-in-payment-signature-and-reads-payment-response",
            category: Category::Payments,
            desc: "[X-HTTP] \"Clients send payment data using the `PAYMENT-SIGNATURE` HTTP header\" and \"Servers communicate payment settlement results using the `PAYMENT-RESPONSE` header\": the server recovers the wallet from the signature, and the settlement is the step's",
            run: pays_and_reads_the_settlement,
        },
        Check {
            id: "payments/skips-a-payment-flow-it-does-not-recognize",
            category: Category::Payments,
            desc: "[X §5.1.2] \"Clients MUST NOT construct a payment for a `paymentFlow` they do not recognize, and SHOULD skip such `accepts[]` entries when selecting\": an `upfront` entry is passed over and the next one paid, echoed exactly as `accepted`",
            run: skips_a_payment_flow,
        },
        Check {
            id: "payments/refused-unsent-outside-the-caps-and-allow-lists",
            category: Category::Payments,
            desc: "ADR 0003: \"caps per run, per day and per payee, and an allow-list of payees and assets — a payment outside them is refused unsent\": over a cap and to an unlisted payee, nothing reaches the server",
            run: refused_unsent,
        },
        Check {
            id: "payments/settlement-pending-is-never-paid-again",
            category: Category::Payments,
            desc: "ADR 0003: \"a payment whose outcome is unknown is never made a second time\"; [X §9] `settlement_pending` \"the transaction may still confirm on chain\": the step's retry sends nothing",
            run: pending_is_never_paid_again,
        },
        Check {
            id: "payments/the-approver-alone-answers-above-the-threshold",
            category: Category::Payments,
            desc: "ADR 0003: \"the User's approval above a threshold, through an addressed human gate no model or judge answers\": an operator who is not the approver is refused, and the approver's approve pays once",
            run: the_approver_alone_answers,
        },
        Check {
            id: "payments/every-payment-is-an-audit-record",
            category: Category::Payments,
            desc: "ADR 0003: \"every payment an audit record, tied to the request that asked for it\": authorized before it is sent, then settled, in the hash-chained audit log, naming the run, the step and the nonce",
            run: every_payment_is_an_audit_record,
        },
        Check {
            id: "payments/no-signature-in-a-log-or-a-childs-payload",
            category: Category::Payments,
            desc: "ADR 0003: \"the signing key held by the supervisor alone, sealed at rest, never in a model's context, a child's payload or a log\": the key file is sealed and 0600, and the signature and the signed header are in no log line and no frame to a child",
            run: no_signature_in_a_log_or_a_child,
        },
    ]
}

/// The mock x402 server; killed on drop.
struct Server {
    child: Child,
    url: String,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Server {
    fn spawn(h: &Harness, tmp: &TempDir, name: &str, knobs: Value) -> Server {
        let addr = tmp.path().join(format!("{name}.addr"));
        let child = Command::new(h.agentd())
            .args([
                "--internal-mock-x402",
                addr.to_str().expect("utf8"),
                "--knobs",
                &knobs.to_string(),
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn the mock x402 server");
        let deadline = Instant::now() + Duration::from_secs(10);
        while !addr.exists() {
            assert!(
                Instant::now() < deadline,
                "the mock x402 server never announced itself"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        let doc: Value =
            serde_json::from_str(&std::fs::read_to_string(&addr).expect("read")).expect("json");
        Server {
            child,
            url: doc["url"].as_str().unwrap_or_default().to_string(),
        }
    }

    /// Every request it received.
    fn record(&self) -> Vec<Value> {
        let rest = self.url.strip_prefix("http://").expect("an http URL");
        let mut s = std::net::TcpStream::connect(rest).expect("connect");
        s.set_read_timeout(Some(Duration::from_secs(10))).ok();
        let _ = write!(
            s,
            "GET /_mock/record HTTP/1.1\r\nHost: {rest}\r\nConnection: close\r\n\r\n"
        );
        let mut out = String::new();
        let _ = s.read_to_string(&mut out);
        out.split_once("\r\n\r\n")
            .and_then(|(_, b)| serde_json::from_str(b).ok())
            .unwrap_or_default()
    }

    fn paid(&self) -> Vec<Value> {
        self.record()
            .into_iter()
            .filter(|r| r["paid"] == true)
            .collect()
    }
}

/// A seal key, and the configuration of a job that runs `steps` once —
/// `top` beside it — paying from a wallet sealed in `tmp` within `limits`,
/// to the allowed payee, with `more` in `security.payments`.
fn config(tmp: &TempDir, top: &str, more: &str, limits: &str, steps: &str) -> String {
    let d = tmp.path().display().to_string();
    write_file(tmp, "seal", &"07".repeat(32));
    let lifecycle = if top.contains("lifecycle:") {
        ""
    } else {
        "lifecycle: {run_until: idle, idle_grace: 500ms}\n"
    };
    let text = format!(
        "agent: {{name: payer, preflight: never}}\n\
         intelligence: {{endpoints: \"mock:final\", model: mock}}\n\
         store: {{kind: file, file: {{path: \"{d}/state\"}}}}\n\
         observability: {{log_level: info, log_content: true, audit: {{chain: {{path: \"{d}/audit.log\"}}}}}}\n\
         {lifecycle}{top}\
         security:\n  payments:\n    seal_key: \"{{{{secret-file:{d}/seal}}}}\"\n{more}\
         \x20   wallets: {{ops: {{key_file: \"{d}/ops.key\"}}}}\n\
         \x20   assets:\n      usdc:\n        asset: \"eip155:84532/erc20:{USDC}\"\n        decimals: 6\n        limits: {{{limits}}}\n\
         \x20   payees: {{weather: {{address: \"{PAYEE}\"}}}}\n\
         workflows:\n  - name: buy\n    steps:\n      s: {{kind: once}}\n{steps}"
    );
    write_file(tmp, "agentd.yaml", &text)
}

/// `agentd --print-wallet-address ops`: the address it prints, the sealed
/// key file made.
fn wallet(h: &Harness, cfg: &str, env: &[(&str, &str)]) -> String {
    let r = h.run_env(&["--config", cfg, "--print-wallet-address", "ops"], env);
    r.stdout.trim().to_string()
}

/// A paying `http` step to `url`.
fn paying(id: &str, after: &str, url: &str, more: &str) -> String {
    format!(
        "      {id}: {{kind: http, depends_on: [{after}], url: \"{url}\", allow_private: true, payment: {{wallet: ops, max: \"0.05 usdc\"}}{more}}}\n"
    )
}

/// The `step.done` lines of `step`, in order.
fn done_of(events: &[Value], step: &str) -> Vec<Value> {
    events
        .iter()
        .filter(|e| e["event"] == "step.done" && e["step"] == step)
        .cloned()
        .collect()
}

/// The `payment` records of the audit chain in `tmp`.
fn chain(tmp: &TempDir) -> Vec<Value> {
    std::fs::read_to_string(tmp.path().join("audit.log"))
        .unwrap_or_default()
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter(|r| r["action"] == "payment")
        .collect()
}

fn pays_and_reads_the_settlement(h: &Harness) -> Outcome {
    let tmp = h.tempdir();
    let m = Server::spawn(h, &tmp, "x402", json!({}));
    let steps = format!(
        "{}      f: {{kind: finish, depends_on: [get], output: \"{{{{ steps.get.output }}}}\"}}\n",
        paying("get", "s", &format!("{}/premium-data", m.url), "")
    );
    let cfg = config(
        &tmp,
        "",
        "",
        "per_run: \"1\", per_day: \"1\", per_payee: \"1\"",
        &steps,
    );
    let address = wallet(h, &cfg, &[]);
    let r = h.run(&["--config", &cfg]);
    let rec = m.record();
    let out = r
        .events()
        .into_iter()
        .find(|e| e["event"] == "run.done")
        .map(|e| e["output"].clone())
        .unwrap_or_default();
    Outcome::require(
        r.code == Some(0) && address.starts_with("0x"),
        format!(
            "exit {:?}, wallet {address:?}; stderr:\n{}",
            r.code, r.stderr
        ),
    )
    .and(|| {
        Outcome::require(
            rec.len() == 2 && rec[0]["paid"] == false && rec[1]["paid"] == true,
            format!("not one unpaid request and one paid: {rec:?}"),
        )
    })
    .and(|| {
        Outcome::require(
            rec[1]["valid"] == true && rec[1]["from"] == address.as_str(),
            format!(
                "the PAYMENT-SIGNATURE does not recover to the wallet {address}: {:?}",
                rec[1]
            ),
        )
    })
    .and(|| {
        Outcome::require(
            out["payment"]["state"] == "settled"
                && out["payment"]["transaction"] == rec[1]["response"]["transaction"]
                && out["json"]["data"] == "premium market data",
            format!("the PAYMENT-RESPONSE is not the step's: {out}"),
        )
    })
}

fn skips_a_payment_flow(h: &Harness) -> Outcome {
    let tmp = h.tempdir();
    let entry = |flow: Option<&str>| {
        let mut e = json!({"scheme": "exact", "network": "eip155:84532", "amount": "10000",
                           "asset": USDC, "payTo": PAYEE, "maxTimeoutSeconds": 60,
                           "extra": {"name": "USDC", "version": "2"}});
        if let Some(f) = flow {
            e["extra"]["paymentFlow"] = json!(f);
        }
        e
    };
    let offered = json!([entry(Some("upfront")), entry(None)]);
    let m = Server::spawn(h, &tmp, "x402", json!({"accepts": offered}));
    let steps = format!(
        "{}      f: {{kind: finish, depends_on: [get]}}\n",
        paying("get", "s", &format!("{}/data", m.url), "")
    );
    let cfg = config(
        &tmp,
        "",
        "",
        "per_run: \"1\", per_day: \"1\", per_payee: \"1\"",
        &steps,
    );
    wallet(h, &cfg, &[]);
    let r = h.run(&["--config", &cfg]);
    let paid = m.paid();
    Outcome::require(
        r.code == Some(0) && paid.len() == 1,
        format!("exit {:?}, paid {paid:?}; stderr:\n{}", r.code, r.stderr),
    )
    .and(|| {
        Outcome::require(
            paid[0]["payment"]["accepted"] == offered[1],
            format!(
                "the entry paid is not the authorization one, exactly: {}",
                paid[0]["payment"]["accepted"]
            ),
        )
    })
}

fn refused_unsent(h: &Harness) -> Outcome {
    let tmp = h.tempdir();
    let over = Server::spawn(h, &tmp, "over", json!({"price": "60000"}));
    let stranger = Server::spawn(
        h,
        &tmp,
        "stranger",
        json!({"pay_to": "0x000000000000000000000000000000000000dEaD"}),
    );
    let steps = format!(
        "{}{}      f: {{kind: finish, depends_on: [b]}}\n",
        paying("a", "s", &format!("{}/a", over.url), ", on_error: continue"),
        paying(
            "b",
            "a",
            &format!("{}/b", stranger.url),
            ", on_error: continue"
        ),
    );
    let cfg = config(
        &tmp,
        "",
        "    ask_when_tainted: false\n",
        "per_payment: \"0.05\", per_run: \"1\", per_day: \"1\", per_payee: \"1\"",
        &steps,
    );
    wallet(h, &cfg, &[]);
    let r = h.run(&["--config", &cfg]);
    let ev = r.events();
    let err = |s: &str| {
        done_of(&ev, s)
            .first()
            .and_then(|e| e["err"].as_str().map(str::to_string))
            .unwrap_or_default()
    };
    Outcome::require(
        r.code == Some(0),
        format!("exit {:?}; stderr:\n{}", r.code, r.stderr),
    )
    .and(|| {
        Outcome::require(
            err("a").contains("refused unsent") && err("a").contains("per_payment:"),
            format!("over the cap: {}", err("a")),
        )
    })
    .and(|| {
        Outcome::require(
            err("b").contains("refused unsent") && err("b").contains("not an allowed payee"),
            format!("an unlisted payee: {}", err("b")),
        )
    })
    .and(|| {
        Outcome::require(
            over.paid().is_empty() && stranger.paid().is_empty(),
            "a payment reached a server",
        )
    })
}

fn pending_is_never_paid_again(h: &Harness) -> Outcome {
    let tmp = h.tempdir();
    let m = Server::spawn(h, &tmp, "x402", json!({"settle": "pending"}));
    let steps = format!(
        "{}      f: {{kind: finish, depends_on: [get]}}\n",
        paying(
            "get",
            "s",
            &format!("{}/data", m.url),
            ", retry: {max: 2, backoff: 100ms}, on_error: continue"
        )
    );
    let cfg = config(
        &tmp,
        "",
        "    ask_when_tainted: false\n",
        "per_run: \"1\", per_day: \"1\", per_payee: \"1\"",
        &steps,
    );
    wallet(h, &cfg, &[]);
    let r = h.run(&["--config", &cfg]);
    let attempts = done_of(&r.events(), "get");
    let errs: Vec<&str> = attempts
        .iter()
        .map(|e| e["err"].as_str().unwrap_or_default())
        .collect();
    Outcome::require(
        errs.len() == 3 && errs[0].contains("outcome unknown"),
        format!(
            "not three attempts, the first unknown: {errs:?}\n{}",
            r.stderr
        ),
    )
    .and(|| {
        Outcome::require(
            errs[1..].iter().all(|e| e.contains("payment_unknown")),
            format!("a retry was not refused: {errs:?}"),
        )
    })
    .and(|| {
        Outcome::require(
            m.record().len() == 2,
            format!("the retries reached the server: {:?}", m.record()),
        )
    })
}

fn the_approver_alone_answers(h: &Harness) -> Outcome {
    let tmp = h.tempdir();
    let m = Server::spawn(h, &tmp, "x402", json!({"price": "30000"}));
    let port = free_port();
    let addr = format!("127.0.0.1:{port}");
    let top = format!(
        "lifecycle: {{run_until: drained}}\n\
         a2a:\n  listen: \"http://{addr}\"\n  bearer: \"{{{{secret:OPS}}}}\"\n  principals:\n    - {{id: alice, match: {{bearer_ref: \"{{{{secret:ALICE}}}}\"}}, role: user}}\n"
    );
    let steps = format!(
        "{}      f: {{kind: finish, depends_on: [get]}}\n",
        paying("get", "s", &format!("{}/data", m.url), "")
    );
    let cfg = config(
        &tmp,
        &top,
        "    approver: \"user:alice\"\n",
        "per_run: \"1\", per_day: \"1\", per_payee: \"1\"",
        &steps,
    );
    // Above 0.02 the approver is asked; the step pays 0.03.
    let text = std::fs::read_to_string(&cfg).unwrap_or_default().replace(
        "        limits:",
        "        approve_above: \"0.02\"\n        limits:",
    );
    std::fs::write(&cfg, text).expect("write");
    let env = [("OPS", "ops-conf-x402"), ("ALICE", "alice-conf-x402")];
    wallet(h, &cfg, &env);
    let log = tmp.path().join("agentd.log");
    let _d = h.spawn_logged_env(&["--config", &cfg], &env, &log);
    wait_ready(&addr);
    let read = || std::fs::read_to_string(&log).unwrap_or_default();
    let line = |event: &str| {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            if let Some(v) = read()
                .lines()
                .filter_map(|l| serde_json::from_str::<Value>(l).ok())
                .find(|v| v["event"] == event)
            {
                return Some(v);
            }
            if Instant::now() > deadline {
                return None;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    };
    let Some(asked) = line("payment.asked") else {
        return Outcome::fail(format!("the approver was not asked:\n{}", read()));
    };
    let task = asked["task"].as_str().unwrap_or_default().to_string();
    let answer = |bearer: &str, text: &str| {
        rpc_as(
            &addr,
            bearer,
            1,
            "SendMessage",
            text_params(text, Some(&task), false),
        )
        .2
    };
    let operator = answer("ops-conf-x402", "approve");
    let refused = operator.get("error").is_some()
        && operator["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("approval is for user:alice");
    let paid_before = m.paid().len();
    let alice = answer("alice-conf-x402", "approve");
    let settled = line("payment.settled");
    Outcome::require(
        refused,
        format!("an operator who is not the approver answered: {operator}"),
    )
    .and(|| {
        Outcome::require(
            paid_before == 0,
            "a payment was sent before the approver answered",
        )
    })
    .and(|| {
        Outcome::require(
            alice.get("error").is_none() && settled.is_some() && m.paid().len() == 1,
            format!(
                "the approver's approve did not pay once: {alice}\n{}",
                read()
            ),
        )
    })
}

fn every_payment_is_an_audit_record(h: &Harness) -> Outcome {
    let tmp = h.tempdir();
    let m = Server::spawn(h, &tmp, "x402", json!({}));
    let steps = format!(
        "{}      f: {{kind: finish, depends_on: [get]}}\n",
        paying("get", "s", &format!("{}/data", m.url), "")
    );
    let cfg = config(
        &tmp,
        "",
        "",
        "per_run: \"1\", per_day: \"1\", per_payee: \"1\"",
        &steps,
    );
    wallet(h, &cfg, &[]);
    let r = h.run(&["--config", &cfg]);
    let records = chain(&tmp);
    let nonce = m
        .paid()
        .first()
        .map(|p| p["nonce"].clone())
        .unwrap_or_default();
    let outcomes: Vec<&str> = records
        .iter()
        .map(|r| r["outcome"].as_str().unwrap_or_default())
        .collect();
    let lines = std::fs::read_to_string(tmp.path().join("audit.log")).unwrap_or_default();
    Outcome::require(
        r.code == Some(0),
        format!("exit {:?}; stderr:\n{}", r.code, r.stderr),
    )
    .and(|| {
        Outcome::require(
            outcomes == ["authorized", "settled"],
            format!("the chain's payment records: {outcomes:?}"),
        )
    })
    .and(|| {
        Outcome::require(
            records.iter().all(|r| {
                r["target"]["step"] == "get"
                    && r["target"]["workflow"] == "buy"
                    && r["target"]["run"].is_string()
                    && r["target"]["nonce"] == nonce
            }),
            format!("a record names not the request that paid: {records:?}"),
        )
    })
    .and(|| {
        Outcome::require(
            lines
                .lines()
                .all(|l| serde_json::from_str::<Value>(l).is_ok_and(|v| v["prev"].is_string())),
            "a record of the chain carries no hash of the one before",
        )
    })
}

fn no_signature_in_a_log_or_a_child(h: &Harness) -> Outcome {
    let tmp = h.tempdir();
    let m = Server::spawn(h, &tmp, "x402", json!({}));
    let steps = format!(
        "{}      read: {{kind: subagent, depends_on: [get], instruction: \"Summarize {{{{ steps.get.output }}}}\"}}\n      f: {{kind: finish, depends_on: [read]}}\n",
        paying("get", "s", &format!("{}/data", m.url), "")
    );
    let cfg = config(
        &tmp,
        "",
        "",
        "per_run: \"1\", per_day: \"1\", per_payee: \"1\"",
        &steps,
    );
    let address = wallet(h, &cfg, &[]);
    let frames = tmp.path().join("frames.jsonl");
    let r = h.run_env(
        &["--config", &cfg],
        &[(
            "AGENTD_TEST_FRAMES_FILE",
            frames.to_str().unwrap_or_default(),
        )],
    );
    let crossed = std::fs::read_to_string(&frames).unwrap_or_default();
    let key_file = std::fs::read_to_string(tmp.path().join("ops.key")).unwrap_or_default();
    let mode = {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(tmp.path().join("ops.key"))
            .map(|m| m.permissions().mode() & 0o777)
            .unwrap_or(0)
    };
    let paid = m.paid();
    let signature = paid
        .first()
        .and_then(|p| p["payment"]["payload"]["signature"].as_str())
        .unwrap_or("0x-none")
        .trim_start_matches("0x")
        .to_string();
    let header = paid
        .first()
        .and_then(|p| p["header"].as_str())
        .unwrap_or("no-header")
        .to_string();
    let key: Value = serde_json::from_str(&key_file).unwrap_or_default();
    Outcome::require(
        r.code == Some(0) && paid.len() == 1 && crossed.contains("\"dir\":\"spawn\""),
        format!(
            "exit {:?}, {} paid, a child spawned: {}; stderr:\n{}",
            r.code,
            paid.len(),
            crossed.contains("\"dir\":\"spawn\""),
            r.stderr
        ),
    )
    .and(|| {
        Outcome::require(
            mode == 0o600 && key["sealed"].is_string() && key["address"] == address.as_str(),
            format!("the key file is not sealed and 0600: mode {mode:o}"),
        )
    })
    .and(|| {
        Outcome::require(
            [&signature, &header]
                .iter()
                .all(|s| !r.stderr.contains(s.as_str()) && !crossed.contains(s.as_str())),
            "the signature or the signed header is in the log or in a frame to a child",
        )
    })
}
