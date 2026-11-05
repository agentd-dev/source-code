// SPDX-License-Identifier: AGPL-3.0-only
//! **A document's own wallets and allowances (RFC 0057 §8), end to end.**
//! The real binary, an instruction document that picks an operator wallet
//! (`::wallet{account=ops}`) and sets an allowance (`:::allowance`), its own
//! workflow paying the mock x402 resource server
//! (`agentd::payments::mock`) under it with `payment: {allowance: <id>}`.
//!
//! Without the operator's `spending` grant the allowance is text and the
//! paying step refuses the load; under it the document's workflow pays
//! within its allowance (`doc:<id>`) and is refused past it, its payees
//! match the paid resource's origin host, the operator's
//! `document_ceiling` caps all document spending together, an approver the
//! operator bound to nobody refuses above `approve-above`, a re-release
//! that lowers the budget refuses the next payment, and a document served
//! over the wire with no attesting pin is text only.
#![cfg(all(unix, feature = "x402"))]

mod common;

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

/// Base Sepolia USDC, as the mock offers it.
const USDC: &str = "0x036CbD53842c5426634e7929541eC2318f3dCF7e";
/// The payee the operator allows, and the mock's default `payTo`.
const PAYEE: &str = "0x209693Bc6afc0C5328bA36FaF03C514EF312287C";
/// The seal key every wallet key file here is sealed under.
const SEAL: &str = "0707070707070707070707070707070707070707070707070707070707070707";
/// The bearers a listening configuration names: the operator's and the
/// user `alice`'s.
const BEARERS: [(&str, &str); 2] = [("OPS", "ops-bearer-doc"), ("ALICE", "alice-bearer-doc")];

struct Daemon {
    child: Child,
    log: PathBuf,
}

impl Daemon {
    fn spawn(cfg: &Path) -> Daemon {
        let log = cfg.with_extension("log");
        let child = Command::new(env!("CARGO_BIN_EXE_agentd"))
            .args(["--config", &cfg.display().to_string()])
            .envs(BEARERS)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::from(std::fs::File::create(&log).unwrap()))
            .spawn()
            .expect("spawn agentd");
        Daemon { child, log }
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

    fn warnings(&self) -> String {
        self.events("config.warning")
            .iter()
            .map(|w| w["warning"].as_str().unwrap_or_default().to_string())
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// The `step.done` line of `step`, once it has one.
    fn step(&self, step: &str) -> Value {
        self.wait(&format!("step {step} done"), |d| {
            d.events("step.done")
                .into_iter()
                .find(|e| e["step"] == step)
        })
    }

    fn wait<T>(&self, what: &str, f: impl Fn(&Daemon) -> Option<T>) -> T {
        let deadline = Instant::now() + Duration::from_secs(40);
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

    fn exits(&mut self) -> Option<i32> {
        let deadline = Instant::now() + Duration::from_secs(40);
        while Instant::now() < deadline {
            if let Ok(Some(s)) = self.child.try_wait() {
                return s.code();
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        panic!("the daemon did not exit:\n{}", self.read())
    }

    #[cfg(feature = "hot-reload")]
    fn sighup(&self) {
        // SAFETY: a signal to the child this test spawned.
        unsafe { libc::kill(self.child.id() as i32, libc::SIGHUP) };
    }
}

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
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// A scratch directory with the seal key in it.
fn scratch() -> tempfile::TempDir {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let seal = dir.path().join("seal");
    std::fs::write(&seal, SEAL).unwrap();
    std::fs::set_permissions(&seal, std::fs::Permissions::from_mode(0o600)).unwrap();
    dir
}

/// `security.payments`: the wallet `ops` sealed in `dir`, USDC worth a
/// dollar, the payee `weather` (the mock's), and `more` at its top.
fn payments(dir: &Path, more: &str) -> String {
    let d = dir.display();
    format!(
        "    seal_key: \"{{{{secret-file:{d}/seal}}}}\"\n    ask_when_tainted: false\n{more}    wallets: {{ops: {{key_file: \"{d}/ops.key\"}}}}\n    assets:\n      usdc:\n        asset: \"eip155:84532/erc20:{USDC}\"\n        decimals: 6\n        currency: USD\n        limits: {{per_run: \"1\", per_day: \"1\", per_payee: \"1\"}}\n    payees:\n      weather: {{address: \"{PAYEE}\"}}\n"
    )
}

/// The configuration: a file store and an audit chain in `dir`, the
/// document `doc.md` as the instruction under `grants`, the `payments`
/// block, `top` beside them. A job that exits once its work is done, unless
/// `top` says otherwise.
fn config(dir: &Path, grants: &str, payments: &str, top: &str) -> PathBuf {
    let d = dir.display();
    let mut head = String::new();
    if !top.contains("lifecycle:") {
        head.push_str("lifecycle: {run_until: idle, idle_grace: 1s}\n");
    }
    let text = format!(
        "agent: {{name: payer, preflight: never, instruction: \"{d}/doc.md\", document_capabilities: [{grants}]}}\n\
         {head}\
         intelligence: {{endpoints: \"mock:final\", model: mock}}\n\
         store: {{kind: file, file: {{path: \"{d}/state\"}}}}\n\
         observability: {{log_level: info, log_content: true, audit: {{chain: {{path: \"{d}/audit/audit.log\"}}}}}}\n\
         {top}\
         security:\n  payments:\n{payments}"
    );
    let path = dir.join("agentd.yaml");
    std::fs::write(&path, text).unwrap();
    path
}

/// A document: the wallet `card` (the operator's `ops`), the service `api`
/// at `api`, `allowances` and the workflow `buy` of `steps`.
fn document(api: &str, allowances: &str, steps: &str) -> String {
    format!(
        "---\nspec: \"1\"\n---\n# Buyer\n\nYou buy market data.\n\n\
         ::person{{id=nataliia title=\"Nataliia\"}}\n\n\
         ::wallet{{id=card title=\"Data card\" account=ops rails=\"x402/2\"}}\n\n\
         ::service{{id=api title=\"Market data\" src=\"{api}\"}}\n\n\
         {allowances}\n\
         :::workflow{{id=buy dialect=\"instruction/workflow\"}}\n\
         name: buy\nsteps:\n  s: {{kind: once}}\n{steps}:::\n"
    )
}

/// An allowance block.
fn allowance(id: &str, attrs: &str) -> String {
    format!(
        ":::allowance{{id={id} title=\"{id}\" wallet=#card currency=USD {attrs}}}\nMarket data only.\n:::\n"
    )
}

/// A paying `http` step to `url` under the document's allowance `under`.
fn paying(id: &str, after: &str, url: &str, under: &str, more: &str) -> String {
    format!(
        "  {id}: {{kind: http, depends_on: [{after}], url: \"{url}\", allow_private: true, payment: {{allowance: {under}}}{more}}}\n"
    )
}

fn mock(price: &str) -> agentd::payments::mock::Mock {
    agentd::payments::mock::start(&json!({"price": price})).expect("start the mock x402 server")
}

fn paid(m: &agentd::payments::mock::Mock) -> Vec<Value> {
    m.record()
        .into_iter()
        .filter(|r| r["paid"] == true)
        .collect()
}

/// `agentd --print-wallet-address ops`: seals the wallet's key file.
fn seal_wallet(cfg: &Path) {
    let out = Command::new(env!("CARGO_BIN_EXE_agentd"))
        .args([
            "--config",
            &cfg.display().to_string(),
            "--print-wallet-address",
            "ops",
        ])
        .envs(BEARERS)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn validate(cfg: &Path) -> (Option<i32>, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_agentd"))
        .args(["--config", &cfg.display().to_string(), "--validate-config"])
        .output()
        .unwrap();
    (
        out.status.code(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

fn audit(dir: &Path) -> Vec<Value> {
    std::fs::read_to_string(dir.join("audit/audit.log"))
        .unwrap_or_default()
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter(|r| r["action"] == "payment")
        .collect()
}

/// Without the `spending` grant a document's allowance is delivered as
/// text and nothing is paid under it: its workflow's paying step refuses
/// the load, naming the grant; with no paying step the document loads, and
/// the load says once that its allowance and wallet are text only.
#[test]
fn without_the_grant_an_allowance_is_text_and_a_paying_step_is_refused() {
    let dir = scratch();
    let m = mock("10000");
    std::fs::write(
        dir.path().join("doc.md"),
        document(
            &m.url,
            &allowance("data", "max=0.05 budget=0.05"),
            &format!(
                "{}  f: {{kind: finish, depends_on: [a]}}\n",
                paying("a", "s", &format!("{}/a", m.url), "data", "")
            ),
        ),
    )
    .unwrap();
    let cfg = config(dir.path(), "", &payments(dir.path(), ""), "");
    let (code, err) = validate(&cfg);
    assert_eq!(code, Some(2), "{err}");
    assert!(
        err.contains("a served document configures no payment")
            && err.contains("`agent.document_capabilities: [spending]`"),
        "{err}"
    );
    std::fs::write(
        dir.path().join("doc.md"),
        document(
            &m.url,
            &allowance("data", "max=0.05 budget=0.05"),
            "  f: {kind: finish, depends_on: [s]}\n",
        ),
    )
    .unwrap();
    seal_wallet(&cfg);
    let mut d = Daemon::spawn(&cfg);
    assert_eq!(d.exits(), Some(0), "{}", d.read());
    let said = d.warnings();
    assert_eq!(
        said.matches("delivered as text only").count(),
        1,
        "said once: {said}"
    );
    assert!(
        said.contains("allowance \"data\", wallet \"card\" are delivered as text only")
            && said.contains("needs the `spending` grant"),
        "{said}"
    );
    assert!(paid(&m).is_empty());
}

/// Under the grant the document's workflow pays under its own allowance,
/// `doc:data`: within it the payment is made and its record names the
/// allowance; the next one, past the budget, is refused unsent naming
/// `doc:data`; and the workflow reads what is left with the
/// `payment.allowances` tool — the document's allowance marked
/// `source: document`, with the document's URI.
// Its `when` branch is CEL, which a build without the `cel` feature refuses.
#[cfg(feature = "cel")]
#[test]
fn a_document_workflow_pays_under_its_allowance_and_not_past_it() {
    let dir = scratch();
    let first = mock("30000");
    let second = mock("30000");
    // The workflow branches on what is left before it tries again: 0.02 is
    // less than the 0.03 a third payment would be, so `more` is pruned.
    let steps = format!(
        "{}{}  left: {{kind: tool, name: payment.allowances, depends_on: [b]}}\n{}  f: {{kind: finish, depends_on: [left, more], output: \"{{{{ steps.left.output }}}}\"}}\n",
        paying("a", "s", &format!("{}/a", first.url), "data", ""),
        paying(
            "b",
            "a",
            &format!("{}/b", second.url),
            "data",
            ", on_error: continue"
        ),
        paying(
            "more",
            "left",
            &format!("{}/c", second.url),
            "data",
            ", when: \"steps.left.output.allowances.exists(a, a.id == 'doc:data' && double(a.left) >= 0.03)\""
        ),
    );
    std::fs::write(
        dir.path().join("doc.md"),
        document(
            &first.url,
            &allowance("data", "max=0.05 budget=0.05 per=month payees=\"&api\""),
            &steps,
        ),
    )
    .unwrap();
    let cfg = config(dir.path(), "spending", &payments(dir.path(), ""), "");
    seal_wallet(&cfg);
    let mut d = Daemon::spawn(&cfg);
    assert_eq!(d.step("a")["status"], "done", "{}", d.read());
    let b = d.step("b")["err"].as_str().unwrap().to_string();
    assert!(
        b.contains("allowance doc:data: budget 0.05 USD per month, 0.03 spent, this payment 0.03"),
        "{b}"
    );
    assert_eq!(paid(&first).len(), 1);
    assert!(paid(&second).is_empty(), "refused unsent");
    let done = d.wait("run.done", |d| d.events("run.done").into_iter().next());
    assert_eq!(done["status"], "completed", "{}", d.read());
    let out = match &done["output"] {
        Value::String(s) => serde_json::from_str(s).unwrap_or(Value::String(s.clone())),
        v => v.clone(),
    };
    let a = &out["allowances"][0];
    assert_eq!(a["id"], "doc:data", "{out}");
    assert_eq!(a["source"], "document");
    assert_eq!(
        a["document"],
        dir.path().join("doc.md").display().to_string()
    );
    assert_eq!(a["spent"], "0.03");
    assert_eq!(a["left"], "0.02");
    assert!(
        d.events("step.start").iter().all(|e| e["step"] != "more"),
        "pruned: {}",
        d.read()
    );
    assert_eq!(d.exits(), Some(0));
    let authorized = audit(dir.path())
        .into_iter()
        .find(|r| r["outcome"] == "authorized")
        .unwrap();
    assert_eq!(authorized["target"]["allowance"], "doc:data");
    assert_eq!(authorized["target"]["charge"], "0.03");
}

/// A document's payees are origin hosts: a payment to a resource on
/// another host is refused unsent, naming the host and the payees.
#[test]
fn a_payee_domain_that_is_not_the_resources_host_is_refused() {
    let dir = scratch();
    let m = mock("10000");
    std::fs::write(
        dir.path().join("doc.md"),
        document(
            &m.url,
            &allowance("data", "max=0.05 budget=1 payees=\"shop.example.com\""),
            &format!(
                "{}  f: {{kind: finish, depends_on: [a]}}\n",
                paying(
                    "a",
                    "s",
                    &format!("{}/a", m.url),
                    "data",
                    ", on_error: continue"
                )
            ),
        ),
    )
    .unwrap();
    let cfg = config(dir.path(), "spending", &payments(dir.path(), ""), "");
    seal_wallet(&cfg);
    let mut d = Daemon::spawn(&cfg);
    let a = d.step("a")["err"].as_str().unwrap().to_string();
    assert!(
        a.contains(
            "allowance doc:data: payee 127.0.0.1 is not one of its payees (shop.example.com)"
        ),
        "{a}"
    );
    assert!(paid(&m).is_empty());
    assert_eq!(d.exits(), Some(0));
}

/// `security.payments.document_ceiling`: every payment under any of the
/// document's allowances counts against the operator's allowance too, so
/// two document allowances together are held to it.
#[test]
fn the_document_ceiling_caps_two_document_allowances_together() {
    let dir = scratch();
    let first = mock("30000");
    let second = mock("30000");
    let steps = format!(
        "{}{}  f: {{kind: finish, depends_on: [b]}}\n",
        paying("a", "s", &format!("{}/a", first.url), "one", ""),
        paying(
            "b",
            "a",
            &format!("{}/b", second.url),
            "two",
            ", on_error: continue"
        ),
    );
    std::fs::write(
        dir.path().join("doc.md"),
        document(
            &first.url,
            &format!(
                "{}\n{}",
                allowance("one", "max=0.05 budget=1"),
                allowance("two", "max=0.05 budget=1")
            ),
            &steps,
        ),
    )
    .unwrap();
    let cfg = config(
        dir.path(),
        "spending",
        &payments(
            dir.path(),
            "    document_ceiling: docs\n    allowances:\n      docs: {currency: USD, max: \"0.05\", budget: \"0.05\", per: month}\n",
        ),
        "",
    );
    seal_wallet(&cfg);
    let mut d = Daemon::spawn(&cfg);
    assert_eq!(d.step("a")["status"], "done", "{}", d.read());
    let b = d.step("b")["err"].as_str().unwrap().to_string();
    assert!(
        b.contains("allowance docs: budget 0.05 USD per month, 0.03 spent, this payment 0.03"),
        "{b}"
    );
    assert!(
        !b.contains("allowance doc:two: budget"),
        "within its own: {b}"
    );
    assert_eq!(paid(&first).len(), 1);
    assert!(paid(&second).is_empty());
    assert_eq!(d.exits(), Some(0));
    let authorized = audit(dir.path())
        .into_iter()
        .find(|r| r["outcome"] == "authorized")
        .unwrap();
    assert_eq!(authorized["target"]["allowance"], "doc:one");
    assert_eq!(authorized["target"]["ceiling"], "docs");
}

/// An approver the operator bound to nobody (`security.payments.approvers`
/// says nothing of `nataliia`): the load warns `approver-unbound`; a
/// payment up to `approve-above` is paid, one above it refused, naming it.
#[test]
fn an_unbound_approver_refuses_above_approve_above() {
    let dir = scratch();
    let small = mock("10000");
    let large = mock("30000");
    let steps = format!(
        "{}{}  f: {{kind: finish, depends_on: [b]}}\n",
        paying("a", "s", &format!("{}/a", small.url), "data", ""),
        paying(
            "b",
            "a",
            &format!("{}/b", large.url),
            "data",
            ", on_error: continue"
        ),
    );
    std::fs::write(
        dir.path().join("doc.md"),
        document(
            &small.url,
            &allowance(
                "data",
                "max=0.05 budget=1 approve-above=0.02 approver=#nataliia",
            ),
            &steps,
        ),
    )
    .unwrap();
    let cfg = config(dir.path(), "spending", &payments(dir.path(), ""), "");
    seal_wallet(&cfg);
    let mut d = Daemon::spawn(&cfg);
    assert_eq!(d.step("a")["status"], "done", "{}", d.read());
    let b = d.step("b")["err"].as_str().unwrap().to_string();
    assert!(
        b.contains("allowance doc:data: approver-unbound: this payment 0.03 is above approve-above 0.02 USD"),
        "{b}"
    );
    assert_eq!(paid(&small).len(), 1);
    assert!(paid(&large).is_empty());
    assert_eq!(d.exits(), Some(0));
    let said = d.warnings();
    assert_eq!(said.matches("(approver-unbound)").count(), 1, "{said}");
    assert!(
        said.contains("its approver #nataliia is not bound (security.payments.approvers.nataliia)"),
        "{said}"
    );
}

/// A re-release of the document that lowers its allowance's budget to what
/// was spent applies at once: the reload says `payment.allowance.changed`
/// for `doc:data`, and the next payment is refused, what was spent still
/// counted.
#[cfg(feature = "hot-reload")]
#[test]
fn a_re_release_lowering_the_budget_refuses_the_next_payment() {
    let dir = scratch();
    let first = mock("30000");
    let second = mock("10000");
    let steps = format!(
        "{}  n: {{kind: sleep, depends_on: [a], duration: 3s}}\n{}  f: {{kind: finish, depends_on: [b]}}\n",
        paying("a", "s", &format!("{}/a", first.url), "data", ""),
        paying(
            "b",
            "n",
            &format!("{}/b", second.url),
            "data",
            ", on_error: continue"
        ),
    );
    let doc = |budget: &str| {
        document(
            &first.url,
            &allowance("data", &format!("max={budget} budget={budget} per=month")),
            &steps,
        )
    };
    std::fs::write(dir.path().join("doc.md"), doc("1")).unwrap();
    let cfg = config(dir.path(), "spending", &payments(dir.path(), ""), "");
    seal_wallet(&cfg);
    let mut d = Daemon::spawn(&cfg);
    assert_eq!(d.step("a")["status"], "done", "{}", d.read());
    std::fs::write(dir.path().join("doc.md"), doc("0.03")).unwrap();
    d.sighup();
    let changed = d.wait("payment.allowance.changed", |d| {
        d.events("payment.allowance.changed").into_iter().next()
    });
    assert_eq!(changed["id"], "doc:data", "{changed}");
    assert_eq!(changed["fields"], json!(["max", "budget"]));
    let b = d.step("b")["err"].as_str().unwrap().to_string();
    assert!(
        b.contains("allowance doc:data: budget 0.03 USD per month, 0.03 spent, this payment 0.01"),
        "{b}"
    );
    assert!(paid(&second).is_empty());
    assert_eq!(d.exits(), Some(0));
}

/// Served over the wire (`mcp:`) with the grant but no pin that attests
/// `spending`, the document's allowance is text only, said why.
#[test]
fn over_the_wire_without_the_attestation_an_allowance_is_text_only() {
    let dir = scratch();
    let m = mock("10000");
    let registry = common::spawn_mock_mcp("mock://unused", false);
    registry.control(
        "mock.serve_document",
        json!({"text": document(
            &m.url,
            &allowance("data", "max=0.05 budget=1"),
            "  f: {kind: finish, depends_on: [s]}\n",
        )}),
    );
    let d = dir.path().display();
    let text = format!(
        "agent:\n  name: payer\n  preflight: never\n  instruction: {{mcp: {{server: registry, resource: \"instruction://ins_live@stable\"}}}}\n  document_capabilities: [spending]\n\
         mcp: {{servers: [{{name: registry, endpoint: \"{}/mcp\"}}]}}\n\
         lifecycle: {{run_until: drained}}\n\
         intelligence: {{endpoints: \"mock:final\", model: mock}}\n\
         store: {{kind: file, file: {{path: \"{d}/state\"}}}}\n\
         observability: {{log_level: info, log_content: true, audit: {{chain: {{path: \"{d}/audit/audit.log\"}}}}}}\n\
         workflows:\n  - name: tick\n    steps:\n      every: {{kind: schedule, every: 1h}}\n      done: {{kind: finish, depends_on: [every], status: completed}}\n\
         security:\n  payments:\n{}",
        registry.uri(),
        payments(dir.path(), "")
    );
    let cfg = dir.path().join("agentd.yaml");
    std::fs::write(&cfg, text).unwrap();
    seal_wallet(&cfg);
    let d = Daemon::spawn(&cfg);
    d.wait("the first read to fold", |d| {
        d.events("instruction.machinery.applied").into_iter().next()
    });
    let said = d.wait("the text-only warning", |d| {
        let w = d.warnings();
        w.contains("delivered as text only").then_some(w)
    });
    assert!(
        said.contains("allowance \"data\"") && said.contains("is not verified"),
        "{said}"
    );
    assert!(paid(&m).is_empty());
}

// ---- Who approves what (RFC 0057 §8) --------------------------------------

/// The A2A top of a daemon that stays up, serving the operator (`OPS`) and
/// the user `alice` (`ALICE`), whom the operator binds to the document's
/// `#nataliia`.
#[cfg(feature = "a2a")]
fn listening(port: u16) -> String {
    format!(
        "lifecycle: {{run_until: drained}}\na2a:\n  listen: \"http://127.0.0.1:{port}\"\n  bearer: \"{{{{secret:OPS}}}}\"\n  principals:\n    - {{id: alice, match: {{bearer_ref: \"{{{{secret:ALICE}}}}\"}}, role: user}}\n"
    )
}

/// A listening daemon: `doc` as the document, `more` at the top of the
/// spend policy, the document's `#nataliia` bound to `user:alice`, and the
/// asset's own `approve_above` when `asset_above` is set.
#[cfg(feature = "a2a")]
fn approving(
    dir: &Path,
    doc: &str,
    more: &str,
    asset_above: Option<&str>,
) -> (Daemon, String, PathBuf) {
    std::fs::write(dir.join("doc.md"), doc).unwrap();
    let mut policy = payments(
        dir,
        &format!("    approvers: {{nataliia: \"user:alice\"}}\n{more}"),
    );
    if let Some(a) = asset_above {
        policy = policy.replace(
            "        currency: USD\n",
            &format!("        currency: USD\n        approve_above: \"{a}\"\n"),
        );
    }
    let mut path = PathBuf::new();
    let (d, addr) = common::spawn_bound(|port| {
        let cfg = config(dir, "spending", &policy, &listening(port));
        if !dir.join("ops.key").exists() {
            seal_wallet(&cfg);
        }
        path = cfg.clone();
        let d = Daemon::spawn(&cfg);
        let log = d.log.display().to_string();
        (d, log)
    });
    (d, addr, path)
}

/// The `n`th `payment.asked`, once there is one.
#[cfg(feature = "a2a")]
fn asked(d: &Daemon, n: usize) -> Value {
    d.wait(&format!("payment.asked #{n}"), |d| {
        d.events("payment.asked").into_iter().nth(n)
    })
}

/// Answer the gate on `task` as the holder of `bearer`.
#[cfg(feature = "a2a")]
fn answer(addr: &str, task: &str, bearer: &str, decision: &str) -> Value {
    common::SendMessage::text(decision)
        .task(task)
        .bearer(bearer)
        .post(addr)
}

/// Review finding 1: a payment above both the document allowance's
/// `approve-above` and the ceiling's `approve_above` needs both approvers —
/// the document's (`user:alice`), then the ceiling's (`operator`), each on
/// a gate of its own — and a no from either declines it.
#[cfg(feature = "a2a")]
#[test]
fn both_tiers_ask_both_approvers_and_either_no_declines() {
    let dir = scratch();
    let first = mock("30000");
    let second = mock("30000");
    let steps = format!(
        "{}{}  f: {{kind: finish, depends_on: [b]}}\n",
        paying("a", "s", &format!("{}/a", first.url), "data", ""),
        paying(
            "b",
            "a",
            &format!("{}/b", second.url),
            "data",
            ", on_error: continue"
        ),
    );
    let doc = document(
        &first.url,
        &allowance(
            "data",
            "max=0.05 budget=1 approve-above=0.01 approver=#nataliia",
        ),
        &steps,
    );
    let (d, addr, _) = approving(
        dir.path(),
        &doc,
        "    document_ceiling: docs\n    allowances:\n      docs: {currency: USD, max: \"1\", budget: \"1\", per: month, approve_above: \"0.02\"}\n",
        None,
    );
    let one = asked(&d, 0);
    assert_eq!(one["approver"], "user:alice", "{one}");
    let v = answer(
        &addr,
        one["task"].as_str().unwrap(),
        "alice-bearer-doc",
        "approve",
    );
    assert!(v.get("error").is_none(), "{v}");
    let two = asked(&d, 1);
    assert_eq!(two["approver"], "operator", "{two}");
    assert_eq!(two["tiers"], json!(["ceiling"]));
    assert_eq!(two["approved_by"], json!(["user:alice"]));
    assert!(paid(&first).is_empty(), "nothing before the second yes");
    // alice cannot answer the ceiling's gate.
    let v = answer(
        &addr,
        two["task"].as_str().unwrap(),
        "alice-bearer-doc",
        "approve",
    );
    assert!(v.get("error").is_some(), "{v}");
    let v = answer(
        &addr,
        two["task"].as_str().unwrap(),
        "ops-bearer-doc",
        "approve",
    );
    assert!(v.get("error").is_none(), "{v}");
    assert_eq!(d.step("a")["status"], "done", "{}", d.read());
    assert_eq!(paid(&first).len(), 1);
    let authorized = audit(dir.path())
        .into_iter()
        .find(|r| r["outcome"] == "authorized")
        .unwrap();
    let by = authorized["target"]["approved_by"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(
        by.starts_with("user:alice, "),
        "each approval, who gave it: {by}"
    );
    // The second payment: alice says yes, the operator no.
    let three = asked(&d, 2);
    assert_eq!(three["approver"], "user:alice");
    answer(
        &addr,
        three["task"].as_str().unwrap(),
        "alice-bearer-doc",
        "approve",
    );
    let four = asked(&d, 3);
    assert_eq!(four["approver"], "operator");
    answer(
        &addr,
        four["task"].as_str().unwrap(),
        "ops-bearer-doc",
        "decline",
    );
    let b = d.step("b")["err"].as_str().unwrap().to_string();
    assert!(b.contains("payment declined"), "{b}");
    assert!(paid(&second).is_empty());
    assert_eq!(asked(&d, 0)["tiers"], json!(["document"]));
}

/// Review finding 4: an approval only the ceiling asks for goes to the
/// ceiling's approver — the operator — and their yes pays, though the
/// document's approver is someone else.
#[cfg(feature = "a2a")]
#[test]
fn a_ceiling_only_ask_goes_to_the_operator_who_can_approve_it() {
    let dir = scratch();
    let m = mock("30000");
    let doc = document(
        &m.url,
        &allowance(
            "data",
            "max=0.05 budget=1 approve-above=0.04 approver=#nataliia",
        ),
        &format!(
            "{}  f: {{kind: finish, depends_on: [a]}}\n",
            paying("a", "s", &format!("{}/a", m.url), "data", "")
        ),
    );
    let (d, addr, _) = approving(
        dir.path(),
        &doc,
        "    document_ceiling: docs\n    allowances:\n      docs: {currency: USD, max: \"1\", budget: \"1\", per: month, approve_above: \"0.02\"}\n",
        None,
    );
    let one = asked(&d, 0);
    assert_eq!(one["approver"], "operator", "{one}");
    let v = answer(
        &addr,
        one["task"].as_str().unwrap(),
        "ops-bearer-doc",
        "approve",
    );
    assert!(v.get("error").is_none(), "{v}");
    assert_eq!(d.step("a")["status"], "done", "{}", d.read());
    assert_eq!(paid(&m).len(), 1);
    assert_eq!(asked(&d, 0)["tiers"], json!(["ceiling"]));
}

/// Review finding 3: under a document allowance whose approver is bound,
/// the asset's own `approve_above` is still the operator's to answer — not
/// the document's approver's, who cannot.
#[cfg(feature = "a2a")]
#[test]
fn the_assets_threshold_asks_the_operator_not_the_documents_approver() {
    let dir = scratch();
    let m = mock("30000");
    let doc = document(
        &m.url,
        &allowance(
            "data",
            "max=0.05 budget=1 approve-above=0.04 approver=#nataliia",
        ),
        &format!(
            "{}  f: {{kind: finish, depends_on: [a]}}\n",
            paying("a", "s", &format!("{}/a", m.url), "data", "")
        ),
    );
    let (d, addr, _) = approving(dir.path(), &doc, "", Some("0.02"));
    let one = asked(&d, 0);
    assert_eq!(one["approver"], "operator", "{one}");
    let v = answer(
        &addr,
        one["task"].as_str().unwrap(),
        "alice-bearer-doc",
        "approve",
    );
    assert!(v.get("error").is_some(), "the document's approver: {v}");
    let v = answer(
        &addr,
        one["task"].as_str().unwrap(),
        "ops-bearer-doc",
        "approve",
    );
    assert!(v.get("error").is_none(), "{v}");
    assert_eq!(d.step("a")["status"], "done", "{}", d.read());
    assert_eq!(paid(&m).len(), 1);
    assert_eq!(asked(&d, 0)["tiers"], json!(["operator"]));
}

/// Review finding 1, on landing: a reload lowers the ceiling's
/// `approve_above` while the document's approver is being asked. Their yes
/// no longer covers the payment: the ceiling's approver is asked too, and
/// nothing is paid until they say yes.
#[cfg(all(feature = "a2a", feature = "hot-reload"))]
#[test]
fn a_reload_lowering_the_ceilings_threshold_asks_its_approver_too() {
    let dir = scratch();
    let m = mock("30000");
    let doc = document(
        &m.url,
        &allowance(
            "data",
            "max=0.05 budget=1 approve-above=0.01 approver=#nataliia",
        ),
        &format!(
            "{}  f: {{kind: finish, depends_on: [a]}}\n",
            paying("a", "s", &format!("{}/a", m.url), "data", "")
        ),
    );
    let (d, addr, cfg) = approving(
        dir.path(),
        &doc,
        "    document_ceiling: docs\n    allowances:\n      docs: {currency: USD, max: \"1\", budget: \"1\", per: month, approve_above: \"0.50\"}\n",
        None,
    );
    let one = asked(&d, 0);
    assert_eq!(one["approver"], "user:alice", "{one}");
    let text = std::fs::read_to_string(&cfg)
        .unwrap()
        .replace("approve_above: \"0.50\"", "approve_above: \"0.02\"");
    std::fs::write(&cfg, text).unwrap();
    d.sighup();
    d.wait("payment.allowance.changed", |d| {
        d.events("payment.allowance.changed").into_iter().next()
    });
    let v = answer(
        &addr,
        one["task"].as_str().unwrap(),
        "alice-bearer-doc",
        "approve",
    );
    assert!(v.get("error").is_none(), "{v}");
    let two = asked(&d, 1);
    assert_eq!(two["approver"], "operator", "{two}");
    assert_eq!(two["tiers"], json!(["ceiling"]));
    assert!(paid(&m).is_empty(), "not paid on the document's yes alone");
    let v = answer(
        &addr,
        two["task"].as_str().unwrap(),
        "ops-bearer-doc",
        "approve",
    );
    assert!(v.get("error").is_none(), "{v}");
    assert_eq!(d.step("a")["status"], "done", "{}", d.read());
    assert_eq!(paid(&m).len(), 1);
    assert_eq!(asked(&d, 0)["tiers"], json!(["document"]));
}
