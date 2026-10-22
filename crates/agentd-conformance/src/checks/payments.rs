// SPDX-License-Identifier: AGPL-3.0-only
//! agentd paying with x402 v2 (RFC 0049) — for an HTTP resource, on an EVM
//! chain and on Solana, for an MCP tool and for an A2A peer's task — held
//! to the x402 specification pinned at `e7a3b4ff47`, to the a2a-x402
//! extension's v0.2 and to ADR 0003's spend policy, each check quoting the
//! sentence it proves. The resource server is the mock x402 server (`agentd
//! --internal-mock-x402`): it answers 402 with a `PAYMENT-REQUIRED`,
//! recovers the signer of every `PAYMENT-SIGNATURE` — or, on Solana,
//! verifies the transfer as the sponsor does — holds the payment to what it
//! offered, and records what reached it. The tool that charges is the mock
//! MCP server's `mock.paid`, which verifies a payment as a resource server
//! does and records each on its stderr. The A2A peer that charges is
//! written here, from the extension and A2A 1.0.1, and settles through the
//! mock x402 server as a merchant through its facilitator. agentd is read
//! from what it logged, what it wrote to its audit chain and what crossed
//! to a child (`AGENTD_TEST_FRAMES_FILE`).

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::checks::probe::b64url;
use crate::checks::util::{free_port, rpc_as, text_params, wait_ready, write_file};
use crate::harness::{ConfServer, TempDir};
use crate::{Category, Check, Harness, Outcome};
use serde_json::{Value, json};

/// USDC on Base Sepolia, the mock's default token.
const USDC: &str = "0x036CbD53842c5426634e7929541eC2318f3dCF7e";
/// The mock's default `payTo`, the payee the policy allows.
const PAYEE: &str = "0x209693Bc6afc0C5328bA36FaF03C514EF312287C";
/// Solana's mainnet (CAIP-2), USDC's mint on it and a merchant's key: the
/// values of [EXACT-SVM]'s own example.
const SOLANA: &str = "solana:5eykt4UsFv8P8NJdTREpY1vzqKqZKvdp";
const MINT: &str = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v";
const SOL_PAYEE: &str = "2wKupLR9q6wXYppw8Gr2NvWxKBUqm4PPJKkQfoxHDBg4";
/// A key no allow-list names: the example's sponsor.
const SOL_STRANGER: &str = "EwWqGE4ZFKLofuestmU4LDdK7XM1N4ALgdZccwYugwGd";
/// An address no allow-list names.
const STRANGER: &str = "0x000000000000000000000000000000000000dEaD";
/// The a2a-x402 extension, as the x402 v2 A2A transport names it.
const A2A_X402: &str = "https://github.com/google-a2a/a2a-x402/v0.1";
/// The metadata key of a2a-x402's payment status.
const STATUS: &str = "x402.payment.status";

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
        Check {
            id: "payments/svm-pays-the-exact-transfer-within-the-policy",
            category: Category::Payments,
            desc: "[EXACT-SVM] \"a client MUST effect an on-chain payment of an exact amount of a specified asset to a specified recipient\": a Solana entry within the policy is paid from the wallet's base58 key with the transfer the sponsor verifies — the mint, the amount, the payee — and its settlement is the step's",
            run: svm_pays_within_the_policy,
        },
        Check {
            id: "payments/svm-refused-unsent-outside-the-policy",
            category: Category::Payments,
            desc: "ADR 0003: \"caps per run, per day and per payee, and an allow-list of payees and assets — a payment outside them is refused unsent\": a Solana entry past per_payment and one to a key no allow-list names are refused, and the sponsor sees no transaction",
            run: svm_refused_unsent,
        },
        Check {
            id: "payments/mcp-pays-in-meta-and-reads-the-payment-response",
            category: Category::Payments,
            desc: "[X-MCP] \"Clients send payment data using the MCP `_meta` field with key `x402/payment`\" and \"Servers communicate payment settlement results using the `_meta[\"x402/payment-response\"]` field\": a paying mcp.tool step calls the tool once more with the payment in its `_meta`, the tool verifies it, and the settlement is the step's",
            run: mcp_pays_in_meta,
        },
        Check {
            id: "payments/mcp-refused-unsent-outside-the-policy",
            category: Category::Payments,
            desc: "ADR 0003: \"caps per run, per day and per payee, and an allow-list of payees and assets — a payment outside them is refused unsent\": a tool asking past per_payment, and one asking to be paid to an unlisted payee, are refused, and neither tool sees a payment",
            run: mcp_refused_unsent,
        },
        Check {
            id: "payments/mcp-the-approver-alone-answers-above-the-threshold",
            category: Category::Payments,
            desc: "ADR 0003: \"the User's approval above a threshold, through an addressed human gate no model or judge answers\": a tool's payment above approve_above waits for the approver, an operator who is not the approver is refused, and the approver's approve pays the tool once",
            run: mcp_the_approver_alone_answers,
        },
        Check {
            id: "payments/mcp-unknown-outcome-is-never-paid-again",
            category: Category::Payments,
            desc: "ADR 0003: \"a payment whose outcome is unknown is never made a second time\": a paid tool call that gets no answer within the step's timeout is unknown, and the step's retries call nothing",
            run: mcp_unknown_is_never_paid_again,
        },
        Check {
            id: "payments/a2a-pays-on-the-peers-task",
            category: Category::Payments,
            desc: "[X-A2A] \"Clients send payment data using the A2A message metadata with task correlation\": a peer's task that asks to be paid is answered on that task, a2a-x402 activated, with `payment-submitted` and the payload the peer's facilitator verifies, and the peer's receipt is the step's settlement",
            run: a2a_pays_on_the_peers_task,
        },
        Check {
            id: "payments/a2a-payment-rejected-when-it-will-not-pay",
            category: Category::Payments,
            desc: "[A2A-X402 v0.2 §5.4] \"The client rejects all PaymentRequirements options and responds to the server agent with a message setting the message metadata field x402.payment.status to payment-rejected\": a peer asking past per_payment is answered on its task with `payment-rejected` and nothing else, and nothing is paid",
            run: a2a_payment_rejected,
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
    let held = format!(
        "\x20   wallets: {{ops: {{key_file: \"{d}/ops.key\"}}}}\n\
         \x20   assets:\n      usdc:\n        asset: \"eip155:84532/erc20:{USDC}\"\n        decimals: 6\n        limits: {{{limits}}}\n\
         \x20   payees: {{weather: {{address: \"{PAYEE}\"}}}}\n"
    );
    configured(tmp, top, more, &held, steps)
}

/// [`config`] on Solana: the wallet `sol`, USDC by its mint on mainnet
/// within `limits`, and the allowed payee's key.
fn svm_config(tmp: &TempDir, limits: &str, steps: &str) -> String {
    let d = tmp.path().display().to_string();
    let held = format!(
        "\x20   wallets: {{sol: {{chain: solana, key_file: \"{d}/sol.key\"}}}}\n\
         \x20   assets:\n      usdc-sol:\n        asset: \"{SOLANA}/token:{MINT}\"\n        decimals: 6\n        token_program: spl-token\n        limits: {{{limits}}}\n\
         \x20   payees: {{shop: {{address: \"{SOL_PAYEE}\"}}}}\n"
    );
    configured(tmp, "", "    ask_when_tainted: false\n", &held, steps)
}

/// A seal key, and the configuration of a job that runs `steps` once —
/// `top` beside it — paying from the wallets, assets and payees `held`
/// names, with `more` in `security.payments`.
fn configured(tmp: &TempDir, top: &str, more: &str, held: &str, steps: &str) -> String {
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
         security:\n  payments:\n    seal_key: \"{{{{secret-file:{d}/seal}}}}\"\n{more}{held}\
         workflows:\n  - name: buy\n    steps:\n      s: {{kind: once}}\n{steps}"
    );
    write_file(tmp, "agentd.yaml", &text)
}

/// `agentd --print-wallet-address ops`: the address it prints, the sealed
/// key file made.
fn wallet(h: &Harness, cfg: &str, env: &[(&str, &str)]) -> String {
    wallet_named(h, cfg, "ops", env)
}

/// [`wallet`] of the wallet `name`.
fn wallet_named(h: &Harness, cfg: &str, name: &str, env: &[(&str, &str)]) -> String {
    let r = h.run_env(&["--config", cfg, "--print-wallet-address", name], env);
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
    let steps = format!(
        "{}      f: {{kind: finish, depends_on: [get]}}\n",
        paying("get", "s", &format!("{}/data", m.url), "")
    );
    the_approver_alone_pays(h, &tmp, "", &steps, &|| m.paid().len())
}

/// A daemon whose payment for `steps` — `top` beside the listener — is
/// above `approve_above` 0.02 and asked of the approver, `user:alice`: an
/// operator's approve is refused and pays nothing; alice's pays once.
/// `paid` counts the payments that reached the server.
fn the_approver_alone_pays(
    h: &Harness,
    tmp: &TempDir,
    top: &str,
    steps: &str,
    paid: &dyn Fn() -> usize,
) -> Outcome {
    let port = free_port();
    let addr = format!("127.0.0.1:{port}");
    let top = format!(
        "lifecycle: {{run_until: drained}}\n\
         a2a:\n  listen: \"http://{addr}\"\n  bearer: \"{{{{secret:OPS}}}}\"\n  principals:\n    - {{id: alice, match: {{bearer_ref: \"{{{{secret:ALICE}}}}\"}}, role: user}}\n{top}"
    );
    let cfg = config(
        tmp,
        &top,
        "    approver: \"user:alice\"\n",
        "per_run: \"1\", per_day: \"1\", per_payee: \"1\"",
        steps,
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
    let paid_before = paid();
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
            alice.get("error").is_none() && settled.is_some() && paid() == 1,
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

// ── Solana ──────────────────────────────────────────────────────────────────

/// The mock x402 server offering an `exact` entry on Solana — USDC's mint,
/// `price` atomic units, to `pay_to` — with its sponsor as fee payer and a
/// blockhash.
fn svm_server(h: &Harness, tmp: &TempDir, name: &str, price: &str, pay_to: &str) -> Server {
    Server::spawn(
        h,
        tmp,
        name,
        json!({"network": SOLANA, "asset": MINT, "pay_to": pay_to, "price": price}),
    )
}

/// A paying `http` step to `url` from the Solana wallet `sol`.
fn paying_svm(id: &str, after: &str, url: &str, more: &str) -> String {
    format!(
        "      {id}: {{kind: http, depends_on: [{after}], url: \"{url}\", allow_private: true, payment: {{wallet: sol, max: \"0.05 usdc-sol\"}}{more}}}\n"
    )
}

fn svm_pays_within_the_policy(h: &Harness) -> Outcome {
    let tmp = h.tempdir();
    let m = svm_server(h, &tmp, "svm", "10000", SOL_PAYEE);
    let steps = format!(
        "{}      f: {{kind: finish, depends_on: [get], output: \"{{{{ steps.get.output }}}}\"}}\n",
        paying_svm("get", "s", &format!("{}/premium-data", m.url), "")
    );
    let cfg = svm_config(
        &tmp,
        "per_payment: \"0.05\", per_run: \"1\", per_day: \"1\", per_payee: \"1\"",
        &steps,
    );
    let address = wallet_named(h, &cfg, "sol", &[]);
    let r = h.run(&["--config", &cfg]);
    let rec = m.record();
    let out = r
        .events()
        .into_iter()
        .find(|e| e["event"] == "run.done")
        .map(|e| e["output"].clone())
        .unwrap_or_default();
    Outcome::require(
        r.code == Some(0) && (32..=44).contains(&address.len()) && !address.starts_with("0x"),
        format!(
            "exit {:?}, wallet {address:?} (a base58 key); stderr:\n{}",
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
        let accepted = &rec[1]["payment"]["accepted"];
        Outcome::require(
            rec[1]["valid"] == true
                && rec[1]["from"] == address.as_str()
                && accepted["asset"] == MINT
                && accepted["amount"] == "10000"
                && accepted["payTo"] == SOL_PAYEE,
            format!(
                "the sponsor did not verify a transfer of 10000 of {MINT} to {SOL_PAYEE} from {address}: {:?}",
                rec[1]
            ),
        )
    })
    .and(|| {
        Outcome::require(
            out["payment"]["state"] == "settled"
                && out["payment"]["network"] == SOLANA
                && out["payment"]["transaction"] == rec[1]["response"]["transaction"],
            format!("the settlement is not the step's: {out}"),
        )
    })
}

fn svm_refused_unsent(h: &Harness) -> Outcome {
    let tmp = h.tempdir();
    let over = svm_server(h, &tmp, "over", "60000", SOL_PAYEE);
    let stranger = svm_server(h, &tmp, "stranger", "10000", SOL_STRANGER);
    let steps = format!(
        "{}{}      f: {{kind: finish, depends_on: [b]}}\n",
        paying_svm("a", "s", &format!("{}/a", over.url), ", on_error: continue"),
        paying_svm(
            "b",
            "a",
            &format!("{}/b", stranger.url),
            ", on_error: continue"
        ),
    );
    let cfg = svm_config(
        &tmp,
        "per_payment: \"0.05\", per_run: \"1\", per_day: \"1\", per_payee: \"1\"",
        &steps,
    );
    wallet_named(h, &cfg, "sol", &[]);
    let r = h.run(&["--config", &cfg]);
    refused_both(&r, &|| over.paid().len() + stranger.paid().len())
}

/// Each of the steps `a` (past `per_payment`) and `b` (to an unlisted
/// payee) refused unsent, saying why, and `paid` none.
fn refused_both(r: &crate::harness::RunResult, paid: &dyn Fn() -> usize) -> Outcome {
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
            format!("past per_payment: {}", err("a")),
        )
    })
    .and(|| {
        Outcome::require(
            err("b").contains("refused unsent") && err("b").contains("not an allowed payee"),
            format!("an unlisted payee: {}", err("b")),
        )
    })
    .and(|| Outcome::require(paid() == 0, "a payment reached a server"))
}

// ── MCP ─────────────────────────────────────────────────────────────────────

/// The mock MCP server as `shop`, whose `mock.paid` charges, and the file
/// its own record of each call goes to: `MOCK_CALL <params>`,
/// `MOCK_X402_REQUIRED …`, `MOCK_X402_PAID {nonce, from, …}`.
fn shop(h: &Harness, tmp: &TempDir) -> (ConfServer, PathBuf, String) {
    let log = tmp.path().join("mcp.log");
    let mock = h.spawn_mock_mcp_at(
        &tmp.path().join("mcp.addr"),
        "mock://res",
        "2025-11-25",
        &log,
    );
    let top = format!(
        "mcp:\n  servers:\n    - {{name: shop, endpoint: \"{}/mcp\"}}\n",
        mock.endpoint()
    );
    (mock, log, top)
}

/// The lines of the mock's record that begin with `marker`, the marker cut.
fn served(log: &Path, marker: &str) -> Vec<String> {
    let with = format!("{marker} ");
    std::fs::read_to_string(log)
        .unwrap_or_default()
        .lines()
        .filter_map(|l| l.strip_prefix(&with).map(str::to_string))
        .collect()
}

/// A paying `mcp.tool` step calling `mock.paid` with `args`.
fn paying_tool(id: &str, after: &str, args: &str, more: &str) -> String {
    format!(
        "      {id}: {{kind: mcp.tool, depends_on: [{after}], server: shop, tool: mock.paid, args: {{{args}}}, payment: {{wallet: ops, max: \"0.05 usdc\"}}{more}}}\n"
    )
}

fn mcp_pays_in_meta(h: &Harness) -> Outcome {
    let tmp = h.tempdir();
    let (_mock, log, top) = shop(h, &tmp);
    let steps = format!(
        "{}      f: {{kind: finish, depends_on: [pay], output: \"{{{{ steps.pay.output }}}}\"}}\n",
        paying_tool("pay", "s", "amount: \"10000\"", "")
    );
    let cfg = config(
        &tmp,
        &top,
        "",
        "per_run: \"1\", per_day: \"1\", per_payee: \"1\"",
        &steps,
    );
    let address = wallet(h, &cfg, &[]);
    let r = h.run(&["--config", &cfg]);
    let out = r
        .events()
        .into_iter()
        .find(|e| e["event"] == "run.done")
        .map(|e| e["output"].clone())
        .unwrap_or_default();
    let calls: Vec<Value> = served(&log, "MOCK_CALL")
        .iter()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter(|c| c["name"] == "mock.paid")
        .collect();
    let paid: Vec<Value> = served(&log, "MOCK_X402_PAID")
        .iter()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .collect();
    Outcome::require(
        r.code == Some(0),
        format!("exit {:?}; stderr:\n{}", r.code, r.stderr),
    )
    .and(|| {
        Outcome::require(
            calls.len() == 2
                && calls[0]["_meta"].get("x402/payment").is_none()
                && calls[1]["arguments"] == calls[0]["arguments"],
            format!("not one call unpaid and the same call once more: {calls:?}"),
        )
    })
    .and(|| {
        let payment = &calls[1]["_meta"]["x402/payment"];
        Outcome::require(
            payment["x402Version"] == 2
                && payment["resource"]["url"] == "mcp://tool/mock.paid"
                && payment["accepted"]["payTo"] == PAYEE,
            format!("the paid call carries no payment in _meta[\"x402/payment\"]: {payment}"),
        )
    })
    .and(|| {
        Outcome::require(
            paid.len() == 1 && paid[0]["from"] == address.as_str(),
            format!("the tool did not verify one payment from {address}: {paid:?}"),
        )
    })
    .and(|| {
        Outcome::require(
            out["payment"]["state"] == "settled"
                && out["payment"]["payer"] == address.as_str()
                && out["payment"]["nonce"] == paid[0]["nonce"]
                && out["result"]["report"] == "the paid report",
            format!(
                "the _meta[\"x402/payment-response\"] and the result are not the step's: {out}"
            ),
        )
    })
}

fn mcp_refused_unsent(h: &Harness) -> Outcome {
    let tmp = h.tempdir();
    let (_mock, log, top) = shop(h, &tmp);
    let steps = format!(
        "{}{}      f: {{kind: finish, depends_on: [b]}}\n",
        paying_tool("a", "s", "amount: \"60000\"", ", on_error: continue"),
        paying_tool(
            "b",
            "a",
            &format!("amount: \"10000\", pay_to: \"{STRANGER}\""),
            ", on_error: continue"
        ),
    );
    let cfg = config(
        &tmp,
        &top,
        "",
        "per_payment: \"0.05\", per_run: \"1\", per_day: \"1\", per_payee: \"1\"",
        &steps,
    );
    wallet(h, &cfg, &[]);
    let r = h.run(&["--config", &cfg]);
    refused_both(&r, &|| served(&log, "MOCK_X402_PAID").len())
}

fn mcp_the_approver_alone_answers(h: &Harness) -> Outcome {
    let tmp = h.tempdir();
    let (_mock, log, top) = shop(h, &tmp);
    let steps = format!(
        "{}      f: {{kind: finish, depends_on: [pay]}}\n",
        paying_tool("pay", "s", "amount: \"30000\"", "")
    );
    the_approver_alone_pays(h, &tmp, &top, &steps, &|| {
        served(&log, "MOCK_X402_PAID").len()
    })
}

fn mcp_unknown_is_never_paid_again(h: &Harness) -> Outcome {
    let tmp = h.tempdir();
    let (_mock, log, top) = shop(h, &tmp);
    let steps = format!(
        "{}      f: {{kind: finish, depends_on: [pay]}}\n",
        paying_tool(
            "pay",
            "s",
            "amount: \"10000\", outcome: stall, stall_ms: 4000",
            ", timeout: 1s, retry: {max: 2, backoff: 100ms}, on_error: continue"
        )
    );
    let cfg = config(
        &tmp,
        &top,
        "    ask_when_tainted: false\n",
        "per_run: \"1\", per_day: \"1\", per_payee: \"1\"",
        &steps,
    );
    wallet(h, &cfg, &[]);
    let r = h.run(&["--config", &cfg]);
    let attempts = done_of(&r.events(), "pay");
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
        let (asked, paid) = (
            served(&log, "MOCK_X402_REQUIRED").len(),
            served(&log, "MOCK_X402_PAID").len(),
        );
        Outcome::require(
            asked == 1 && paid == 1,
            format!("the retries called the tool: asked {asked}, paid {paid}"),
        )
    })
}

// ── A2A ─────────────────────────────────────────────────────────────────────

/// A peer that charges for its work over a2a-x402's standalone flow (v0.2
/// §5), written from the extension and A2A 1.0.1. A message starts its task
/// `m-1` in `input-required`, asking to be paid with the `PaymentRequired`
/// its facilitator — the mock x402 server — answers for `/report`. A
/// `payment-submitted` on the task is handed to the facilitator as a
/// `PAYMENT-SIGNATURE`; its `PAYMENT-RESPONSE` is the receipt the finished
/// task carries. A `payment-rejected` ends the task. Every request it
/// received is kept: its head and its body.
struct Peer {
    url: String,
    seen: Arc<Mutex<Vec<(String, Value)>>>,
    facilitator: Server,
}

/// What the peer serves from.
struct Merchant {
    card: Value,
    asking: Value,
    facilitator: String,
    task: Mutex<Value>,
    seen: Arc<Mutex<Vec<(String, Value)>>>,
}

/// The peer's task id and context.
const PEER_TASK: &str = "m-1";
const PEER_CONTEXT: &str = "m-ctx";

impl Peer {
    /// A peer that charges `price` atomic units of USDC on Base Sepolia.
    fn start(h: &Harness, tmp: &TempDir, price: &str) -> Peer {
        let facilitator = Server::spawn(h, tmp, "facilitator", json!({"price": price}));
        let (status, headers, _) = http_get(&format!("{}/report", facilitator.url), &[]);
        let required = headers
            .iter()
            .find(|(k, _)| k == "payment-required")
            .and_then(|(_, v)| unbase64(v))
            .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
            .unwrap_or_else(|| panic!("the facilitator offered no PAYMENT-REQUIRED ({status})"));
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind the peer");
        let url = format!("http://{}", listener.local_addr().expect("addr"));
        let asking = json!({"id": PEER_TASK, "contextId": PEER_CONTEXT, "status": {
            "state": "TASK_STATE_INPUT_REQUIRED",
            "message": {"messageId": "m-ask", "role": "ROLE_AGENT",
                "parts": [{"text": "Payment is required for the report."}],
                "metadata": {STATUS: "payment-required", "x402.payment.required": required}}}});
        let card = json!({"name": "merchant", "description": "a peer that charges", "version": "1",
            "supportedInterfaces": [{"url": format!("{url}/"), "protocolBinding": "JSONRPC", "protocolVersion": "1.0"}],
            "capabilities": {"streaming": false, "extensions": [
                {"uri": A2A_X402, "description": "x402 payments", "required": false}]},
            "defaultInputModes": ["text/plain"], "defaultOutputModes": ["text/plain"], "skills": []});
        let seen: Arc<Mutex<Vec<(String, Value)>>> = Arc::default();
        let merchant = Arc::new(Merchant {
            card,
            task: Mutex::new(asking.clone()),
            asking,
            facilitator: facilitator.url.clone(),
            seen: Arc::clone(&seen),
        });
        std::thread::spawn(move || {
            for s in listener.incoming().flatten() {
                let merchant = Arc::clone(&merchant);
                std::thread::spawn(move || merchant.serve(s));
            }
        });
        Peer {
            url,
            seen,
            facilitator,
        }
    }

    /// The `SendMessage`s on the peer's task: the request's head, and its
    /// message.
    fn on_task(&self) -> Vec<(String, Value)> {
        self.seen
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .filter(|(_, b)| {
                b["method"] == "SendMessage" && b["params"]["message"]["taskId"] == PEER_TASK
            })
            .map(|(h, b)| (h.clone(), b["params"]["message"].clone()))
            .collect()
    }
}

impl Merchant {
    /// Answer one request.
    fn serve(&self, mut s: TcpStream) {
        s.set_read_timeout(Some(Duration::from_secs(5))).ok();
        let Some((head, body)) = read_request(&s) else {
            return;
        };
        let body: Value = serde_json::from_slice(&body).unwrap_or_default();
        self.seen
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push((head.clone(), body.clone()));
        let id = body["id"].clone();
        let result = |r: Value| json!({"jsonrpc": "2.0", "id": id, "result": r}).to_string();
        let message = &body["params"]["message"];
        let reply = if head.starts_with("GET /.well-known/agent-card.json ") {
            self.card.to_string()
        } else {
            match body["method"].as_str() {
                Some("SendMessage") if message["taskId"] == PEER_TASK => {
                    let ended = match message["metadata"][STATUS].as_str() {
                        Some("payment-submitted") => {
                            self.settle(&message["metadata"]["x402.payment.payload"])
                        }
                        _ => ended("TASK_STATE_FAILED", "payment-rejected", None),
                    };
                    *self.task.lock().unwrap_or_else(|e| e.into_inner()) = ended.clone();
                    result(json!({"task": ended}))
                }
                Some("SendMessage") => result(json!({"task": self.asking})),
                Some("GetTask") => {
                    result(self.task.lock().unwrap_or_else(|e| e.into_inner()).clone())
                }
                _ => json!({"jsonrpc": "2.0", "id": id,
                            "error": {"code": -32601, "message": "method not found"}})
                .to_string(),
            }
        };
        let _ = write!(
            s,
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{reply}",
            reply.len()
        );
    }

    /// Hand `payload` to the facilitator, and the task as it ends: the
    /// facilitator's settlement its receipt.
    fn settle(&self, payload: &Value) -> Value {
        let signature = b64url(payload.to_string().as_bytes());
        let (_, headers, _) = http_get(
            &format!("{}/report", self.facilitator),
            &[("PAYMENT-SIGNATURE", &signature)],
        );
        let receipt = headers
            .iter()
            .find(|(k, _)| k == "payment-response")
            .and_then(|(_, v)| unbase64(v))
            .and_then(|b| serde_json::from_slice::<Value>(&b).ok());
        match receipt {
            Some(r) if r["success"] == true => {
                ended("TASK_STATE_COMPLETED", "payment-completed", Some(r))
            }
            r => ended("TASK_STATE_FAILED", "payment-failed", r),
        }
    }
}

/// The peer's task, ended in `state`, its status message saying `status`
/// and carrying `receipt`.
fn ended(state: &str, status: &str, receipt: Option<Value>) -> Value {
    let mut metadata = json!({STATUS: status});
    if let Some(r) = receipt {
        metadata["x402.payment.receipts"] = json!([r]);
    }
    json!({"id": PEER_TASK, "contextId": PEER_CONTEXT, "status": {"state": state,
        "message": {"messageId": "m-end", "role": "ROLE_AGENT",
            "parts": [{"text": "the report"}], "metadata": metadata}}})
}

/// One HTTP request: its head, and its body by its `Content-Length`.
fn read_request(s: &TcpStream) -> Option<(String, Vec<u8>)> {
    let mut r = BufReader::new(s);
    let mut head = String::new();
    let mut len = 0usize;
    loop {
        let mut line = String::new();
        if r.read_line(&mut line).ok()? == 0 || line.trim().is_empty() {
            break;
        }
        if let Some((k, v)) = line.split_once(':')
            && k.trim().eq_ignore_ascii_case("content-length")
        {
            len = v.trim().parse().unwrap_or(0);
        }
        head.push_str(&line);
    }
    let mut body = vec![0u8; len];
    r.read_exact(&mut body).ok()?;
    Some((head, body))
}

/// `GET url` with `headers`: the status, the response's headers (names
/// lowercased) and its body.
fn http_get(url: &str, headers: &[(&str, &str)]) -> (u16, Vec<(String, String)>, String) {
    let rest = url.strip_prefix("http://").expect("an http URL");
    let (host, path) = rest
        .split_once('/')
        .map_or((rest, "/".to_string()), |(h, p)| (h, format!("/{p}")));
    let mut s = TcpStream::connect(host).expect("connect");
    s.set_read_timeout(Some(Duration::from_secs(10))).ok();
    let mut req = format!("GET {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n");
    for (k, v) in headers {
        req.push_str(&format!("{k}: {v}\r\n"));
    }
    req.push_str("\r\n");
    let _ = s.write_all(req.as_bytes());
    let mut out = String::new();
    let _ = s.read_to_string(&mut out);
    let (head, body) = out.split_once("\r\n\r\n").unwrap_or((&out, ""));
    let mut lines = head.lines();
    let status = lines
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|c| c.parse().ok())
        .unwrap_or(0);
    let headers = lines
        .filter_map(|l| l.split_once(':'))
        .map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_string()))
        .collect();
    (status, headers, body.to_string())
}

/// Base64 in either alphabet, padded or not (RFC 4648 §4, §5), as x402's
/// headers carry it; `None` for anything else.
fn unbase64(s: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    let (mut acc, mut bits) = (0u32, 0u32);
    for c in s.trim().trim_end_matches('=').bytes() {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' | b'-' => 62,
            b'/' | b'_' => 63,
            _ => return None,
        };
        acc = (acc << 6) | u32::from(v);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
            acc &= (1 << bits) - 1;
        }
    }
    Some(out)
}

/// A paying `a2a.send` step to the peer `merchant`, and `top` naming it.
fn paying_send(peer: &Peer) -> (String, String) {
    let top = format!(
        "a2a:\n  peers:\n    - {{name: merchant, endpoint: \"{}\"}}\n",
        peer.url
    );
    let steps = "      pay: {kind: a2a.send, depends_on: [s], to: merchant, parts: \"the report, please\", payment: {wallet: ops, max: \"0.05 usdc\"}}\n      f: {kind: finish, depends_on: [pay], output: \"{{ steps.pay.output }}\"}\n".to_string();
    (top, steps)
}

/// The `A2A-Extensions` header of a request's `head`.
fn activated(head: &str) -> String {
    head.lines()
        .find_map(|l| {
            l.split_once(':')
                .filter(|(k, _)| k.trim().eq_ignore_ascii_case("a2a-extensions"))
                .map(|(_, v)| v.trim().to_string())
        })
        .unwrap_or_default()
}

fn a2a_pays_on_the_peers_task(h: &Harness) -> Outcome {
    let tmp = h.tempdir();
    let peer = Peer::start(h, &tmp, "10000");
    let (top, steps) = paying_send(&peer);
    let cfg = config(
        &tmp,
        &top,
        "",
        "per_payment: \"0.05\", per_run: \"1\", per_day: \"1\", per_payee: \"1\"",
        &steps,
    );
    let address = wallet(h, &cfg, &[]);
    let r = h.run(&["--config", &cfg]);
    let out = r
        .events()
        .into_iter()
        .find(|e| e["event"] == "run.done")
        .map(|e| e["output"].clone())
        .unwrap_or_default();
    let on_task = peer.on_task();
    let paid = peer.facilitator.paid();
    Outcome::require(
        r.code == Some(0),
        format!("exit {:?}; stderr:\n{}", r.code, r.stderr),
    )
    .and(|| {
        Outcome::require(
            on_task.len() == 1
                && on_task[0].1["metadata"][STATUS] == "payment-submitted"
                && on_task[0].1["contextId"] == PEER_CONTEXT
                && activated(&on_task[0].0).contains(A2A_X402),
            format!(
                "not one payment-submitted on the peer's task and context, a2a-x402 activated: {on_task:?}"
            ),
        )
    })
    .and(|| {
        let payload = &on_task[0].1["metadata"]["x402.payment.payload"];
        Outcome::require(
            payload["x402Version"] == 2 && payload["accepted"]["payTo"] == PAYEE,
            format!("the message carries no x402.payment.payload: {payload}"),
        )
    })
    .and(|| {
        Outcome::require(
            paid.len() == 1 && paid[0]["valid"] == true && paid[0]["from"] == address.as_str(),
            format!("the facilitator did not verify one payment from {address}: {paid:?}"),
        )
    })
    .and(|| {
        Outcome::require(
            out["payment"]["state"] == "settled"
                && out["payment"]["transaction"] == paid[0]["response"]["transaction"],
            format!("the peer's receipt is not the step's settlement: {out}"),
        )
    })
}

fn a2a_payment_rejected(h: &Harness) -> Outcome {
    let tmp = h.tempdir();
    let peer = Peer::start(h, &tmp, "60000");
    let (top, steps) = paying_send(&peer);
    let cfg = config(
        &tmp,
        &top,
        "",
        "per_payment: \"0.05\", per_run: \"1\", per_day: \"1\", per_payee: \"1\"",
        &steps,
    );
    wallet(h, &cfg, &[]);
    let r = h.run(&["--config", &cfg]);
    let err = done_of(&r.events(), "pay")
        .first()
        .and_then(|e| e["err"].as_str().map(str::to_string))
        .unwrap_or_default();
    let on_task = peer.on_task();
    Outcome::require(
        err.contains("refused unsent") && err.contains("per_payment:"),
        format!("not refused past per_payment: {err:?}; stderr:\n{}", r.stderr),
    )
    .and(|| {
        Outcome::require(
            on_task.len() == 1
                && on_task[0].1["metadata"] == json!({STATUS: "payment-rejected"})
                && on_task[0].1["contextId"] == PEER_CONTEXT
                && activated(&on_task[0].0).contains(A2A_X402),
            format!(
                "not one payment-rejected, and nothing else, on the peer's task, a2a-x402 activated: {on_task:?}"
            ),
        )
    })
    .and(|| {
        Outcome::require(
            peer.facilitator.paid().is_empty(),
            "a payment reached the facilitator",
        )
    })
}
