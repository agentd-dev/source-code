// SPDX-License-Identifier: AGPL-3.0-only
//! RFC 0044 §7.P2d, P2 built, part 2: the policy each person's grant is held
//! to. The person's consent rules as agentd ships them and as an operator
//! adds to them, on the facts of each call — first use per run and tool,
//! gates of one shape answered together, a person's gates paced; a person's
//! own message theirs; agent-mode servers hidden from delegated work;
//! documents' workflows asked about unless listed; pairwise subjects; the
//! mint rate; a delegated server's question put to its person alone;
//! self-service revocation; certificate principals that delegate; and a
//! local command beside a delegation under its own uid. The deployments are
//! the family's own ([`Setup`], and [`Fed`] for signed-in people).

use super::p2c::{ALICE, BOB, DEVICES, Fed, FedOpts, PEOPLE, doc};
use super::*;

/// The tags of a server whose writes are egress and whose reads are not.
const EGRESS_WRITES: &str = "{whoami: [], create_issue: [egress], list_issues: [untrusted_input]}";

impl Setup {
    /// What `--delegation-list` says, as JSON.
    fn listing(&self, h: &Harness) -> Value {
        serde_json::from_str(&self.cli(h, &["--delegation-list"]).stdout).unwrap_or(Value::Null)
    }

    /// The open gates, once `n` are.
    fn gates(&self, h: &Harness, n: usize) -> Result<Vec<Value>, String> {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let l = self.listing(h);
            let g = l["gates"].as_array().cloned().unwrap_or_default();
            if g.len() >= n {
                return Ok(g);
            }
            if Instant::now() > deadline {
                return Err(format!(
                    "{n} gates did not open: {l}\n{}",
                    tail(&self.log_text())
                ));
            }
            std::thread::sleep(Duration::from_millis(200));
        }
    }

    /// The `tools/call`s of `tool` the server was sent.
    fn made(&self, tool: &str) -> usize {
        self.xaa
            .mcp_calls("tools/call")
            .iter()
            .filter(|c| c["body"]["params"]["name"] == tool)
            .count()
    }

    /// Edit the configuration in place.
    fn edit(&self, f: impl Fn(String) -> String) {
        let cfg = std::fs::read_to_string(&self.cfg).expect("read");
        std::fs::write(&self.cfg, f(cfg)).expect("write");
    }

    /// How many times the person was asked.
    fn asked(&self) -> usize {
        self.events("delegation.consent")
            .iter()
            .filter(|e| e["decision"] == "asked")
            .count()
    }
}

/// The audit chain's records, from `<dir>/audit/audit.log`.
fn chain(dir: &Path) -> Vec<Value> {
    std::fs::read_to_string(dir.join("audit").join("audit.log"))
        .unwrap_or_default()
        .lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect()
}

/// A person's own message is theirs (the user's decision of 2026-10-21):
/// Alice's start of her run is no outside text to the work acting with her
/// grant, so her delegated `sensitive` write goes ahead unasked; a delegated
/// read's result in her run still taints it, and her write after it asks;
/// and what she started reaches Bob's run — through an event his waits on —
/// as outside text, so his write asks, naming her message.
pub(super) fn a_persons_own_message_is_theirs(h: &Harness) -> Outcome {
    let flows = "  - name: read-write\n    steps:\n      start: {kind: manual}\n      read: {kind: mcp.tool, depends_on: [start], server: linear, tool: whoami, args: {}}\n      call: {kind: mcp.tool, depends_on: [read], server: gated, tool: create_issue, args: {title: \"after a read\"}}\n      done: {kind: finish, depends_on: [call]}\n  - name: say\n    steps:\n      start: {kind: manual}\n      e: {kind: emit, depends_on: [start], stream: inbox, subject: said, data: {x: 1}}\n      done: {kind: finish, depends_on: [e]}\n  - name: waiter\n    steps:\n      start: {kind: manual}\n      w: {kind: wait, depends_on: [start], on: event, stream: inbox, timeout: 60s}\n      call: {kind: mcp.tool, depends_on: [w], server: gated, tool: create_issue, args: {title: \"after an event\"}}\n      done: {kind: finish, depends_on: [call]}\nstreams:\n  inbox: {}\n";
    let s = Fed::new(
        h,
        &FedOpts {
            login: true,
            templates: vec![PEOPLE],
            gated_tags: Some("{\"*\": [sensitive]}"),
            extra: flows.to_string(),
            ..FedOpts::default()
        },
    );
    let _d = s.daemon(h);
    let alice = tryo!(s.login(ALICE));
    let bob = tryo!(s.login(BOB));
    let asked = || {
        s.events("delegation.consent")
            .iter()
            .filter(|e| e["decision"] == "asked")
            .count()
    };
    let written = |title: &str| {
        s.xaa
            .mcp_calls("tools/call")
            .iter()
            .filter(|c| c["body"]["params"]["arguments"]["title"] == title)
            .count()
    };
    // Her own run: her start is hers, and the write is made unasked.
    let write = tryo!(s.run_as(&alice, "write"));
    let o = completed(&write)
        .and(|| Outcome::require(asked() == 0, "her own start taints nothing she does"))
        .and(|| Outcome::require(written("From agentd") == 1, "the write is made"));
    if !o.passed {
        return o;
    }
    // A delegated read's result still taints her run.
    let n = tryo!(s.start_as(&alice, "read-write"));
    let g = tryo!(s.gate_of(&alice));
    let r = &g["rendered"];
    let alice_id = format!("user:oidc:{}#{ALICE}", s.xaa.idp);
    let mine = format!("a message of the signed-in grant holder {alice_id}");
    let taint: Vec<String> = r["taint"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|t| t.as_str().map(str::to_string))
        .collect();
    let o = Outcome::require(
        r["rule"] == "delegated_call_from_tainted_work"
            && taint.iter().any(|t| t == "delegated:linear")
            && !taint.contains(&mine),
        format!("a tool result taints her run, her start does not: {g}"),
    );
    if !o.passed {
        return o;
    }
    let no = s.op_as(
        &alice,
        "delegation.deny",
        json!({"gate": g["gate"], "nonce": g["nonce"]}),
    );
    let read_write = tryo!(s.ended("read-write", n));
    let o = Outcome::require(no.get("error").is_none(), format!("her deny: {no}"))
        .and(|| failed_with(&read_write, "policy_denied"));
    if !o.passed {
        return o;
    }
    // What she started reaches Bob's run as outside text.
    let m = tryo!(s.start_as(&bob, "waiter"));
    let deadline = Instant::now() + Duration::from_secs(20);
    while !s
        .events("step.start")
        .iter()
        .any(|e| e["step"] == "w" && e["kind"] == "wait")
    {
        if Instant::now() > deadline {
            return Outcome::fail(format!("bob's run never waited:\n{}", tail(&s.log_text())));
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    std::thread::sleep(Duration::from_millis(300));
    let said = tryo!(s.run_as(&alice, "say"));
    let gb = tryo!(s.gate_of(&bob));
    let rb = &gb["rendered"];
    let o = completed(&said).and(|| {
        Outcome::require(
            rb["rule"] == "delegated_call_from_tainted_work"
                && rb["taint"]
                    .as_array()
                    .is_some_and(|t| t.iter().any(|x| x.as_str() == Some(mine.as_str()))),
            format!("her message taints Bob's work: {gb}"),
        )
    });
    if !o.passed {
        return o;
    }
    let no = s.op_as(
        &bob,
        "delegation.deny",
        json!({"gate": gb["gate"], "nonce": gb["nonce"]}),
    );
    let waited = tryo!(s.ended("waiter", m));
    Outcome::require(no.get("error").is_none(), format!("his deny: {no}"))
        .and(|| failed_with(&waited, "policy_denied"))
        .and(|| {
            Outcome::require(
                written("after a read") == 0 && written("after an event") == 0,
                "neither denied write is made",
            )
        })
}

/// The shipped rule asks about a delegated `egress` call at its first use
/// per run and tool: with the server's results vouched for
/// (`auth.trusted_output`), so nothing taints the run, the person's approval
/// passes the run's second call unasked, and the next run asks again. An
/// operator's own `to: principal`
/// rule, on the `mode` fact, asks the person too — named
/// `security.policies[1]` — and a rule on a fact the call does not have
/// (`unattended`) holds nothing.
pub(super) fn the_persons_rules_ask_as_shipped_and_configured(h: &Harness) -> Outcome {
    let flows = "  - name: twice\n    steps:\n      start: {kind: manual}\n      one: {kind: mcp.tool, depends_on: [start], server: linear, tool: create_issue, args: {title: one}}\n      two: {kind: mcp.tool, depends_on: [one], server: linear, tool: create_issue, args: {title: two}}\n      done: {kind: finish, depends_on: [two]}\n";
    let s = Setup::new(
        h,
        json!({}),
        &Opts {
            tag_map: Some(EGRESS_WRITES),
            trusted_output: true,
            extra: flows.to_string(),
            ..Opts::default()
        },
    );
    s.edit(|c| {
        c.replace(
            "security:\n  experimental: {id_jag: true}\n",
            "security:\n  experimental: {id_jag: true}\n  policies:\n    - {match: {tool: whoami, unattended: true}, action: deny}\n    - {match: {tool: whoami, mode: delegated}, action: ask, to: principal}\n",
        )
    });
    let _d = s.daemon(h);
    tryo!(s.login(h));
    let n = s.start("twice");
    let g = tryo!(s.gates(h, 1));
    let rule = g[0]["rendered"]["rule"].clone();
    let ok = s.cli(
        h,
        &[
            "--delegation-approve",
            g[0]["gate"].as_str().unwrap_or_default(),
        ],
    );
    let run = tryo!(s.ended("twice", n, Duration::from_secs(30)));
    let o = Outcome::require(
        rule == "delegated_egress",
        format!("the first use asks under delegated_egress: {g:?}"),
    )
    .and(|| {
        Outcome::require(
            ok.code == Some(0),
            format!("approve: {} {}", ok.stdout, ok.stderr),
        )
    })
    .and(|| completed(&run))
    .and(|| Outcome::require(s.made("create_issue") == 2, "both writes are made"))
    .and(|| Outcome::require(s.asked() == 1, "the second use in the run asks nothing"));
    if !o.passed {
        return o;
    }
    // The next run asks again.
    let n = s.start("twice");
    let g = tryo!(s.gates(h, 1));
    let no = s.cli(
        h,
        &[
            "--delegation-deny",
            g[0]["gate"].as_str().unwrap_or_default(),
        ],
    );
    let run = tryo!(s.ended("twice", n, Duration::from_secs(30)));
    let o = Outcome::require(no.code == Some(0), "deny")
        .and(|| failed_with(&run, "policy_denied"))
        .and(|| Outcome::require(s.made("create_issue") == 2, "the denied write is not made"));
    if !o.passed {
        return o;
    }
    // The operator's rule on the mode fact asks the person.
    let n = s.start("who");
    let g = tryo!(s.gates(h, 1));
    let r = g[0]["rendered"].clone();
    let text = g[0]["text"].as_str().unwrap_or_default().to_string();
    let no = s.cli(
        h,
        &[
            "--delegation-deny",
            g[0]["gate"].as_str().unwrap_or_default(),
        ],
    );
    let run = tryo!(s.ended("who", n, Duration::from_secs(30)));
    Outcome::require(
        r["rule"] == "security.policies[1]"
            && text.contains("the operator's rule security.policies[1] asks for your consent"),
        format!("the operator's rule asks, as itself: {r} {text}"),
    )
    .and(|| Outcome::require(no.code == Some(0), "deny"))
    .and(|| failed_with(&run, "policy_denied"))
    .and(|| Outcome::require(s.made("whoami") == 0, "the denied read is not made"))
}

/// Gates of one shape — a run's three parallel writes — are answered
/// together, each bound to its own nonce and hash; a batch naming a gate
/// that is not open decides nothing; and past
/// `identity.delegation.consent_per_minute` a call is refused rather than
/// asked, and the refusal recorded.
pub(super) fn gates_of_one_shape_are_answered_together_and_paced(h: &Harness) -> Outcome {
    let flows = "  - name: three\n    steps:\n      start: {kind: manual}\n      a: {kind: mcp.tool, depends_on: [start], server: linear, tool: create_issue, args: {title: a}}\n      b: {kind: mcp.tool, depends_on: [start], server: linear, tool: create_issue, args: {title: b}}\n      c: {kind: mcp.tool, depends_on: [start], server: linear, tool: create_issue, args: {title: c}}\n      done: {kind: finish, depends_on: [a, b, c]}\n";
    let o = Opts {
        tag_map: Some(EGRESS_WRITES),
        extra: flows.to_string(),
        ..Opts::default()
    };
    let s = Setup::new(h, json!({}), &o);
    {
        let _d = s.daemon(h);
        tryo!(s.login(h));
        let n = s.start("three");
        let g = tryo!(s.gates(h, 3));
        let shapes: std::collections::BTreeSet<String> = g
            .iter()
            .filter_map(|x| x["shape"].as_str().map(str::to_string))
            .collect();
        let ids: Vec<String> = g
            .iter()
            .filter_map(|x| x["gate"].as_str().map(str::to_string))
            .collect();
        let mixed = s.op(
            "delegation.approve",
            json!({"gates": [
                {"gate": g[0]["gate"], "nonce": g[0]["nonce"], "args_hash": g[0]["args_hash"]},
                {"gate": "no-such-gate", "nonce": "x", "args_hash": "y"}]}),
        );
        let still = s.listing(h)["gates"].as_array().map_or(0, Vec::len);
        let ok = s.cli(h, &["--delegation-approve", &ids.join(",")]);
        let run = tryo!(s.ended("three", n, Duration::from_secs(30)));
        let o = Outcome::require(shapes.len() == 1, format!("one shape: {g:?}"))
            .and(|| {
                Outcome::require(
                    mixed.get("error").is_some() && still == 3,
                    format!("a batch naming a gate not open decides nothing: {mixed}"),
                )
            })
            .and(|| {
                Outcome::require(
                    ok.code == Some(0),
                    format!("the batch: {} {}", ok.stdout, ok.stderr),
                )
            })
            .and(|| completed(&run))
            .and(|| Outcome::require(s.made("create_issue") == 3, "all three writes are made"))
            .and(|| {
                Outcome::require(
                    s.events("delegation.consent")
                        .iter()
                        .filter(|e| e["decision"] == "approved")
                        .count()
                        == 3,
                    "each gate's approval is its own record",
                )
            });
        if !o.passed {
            return o;
        }
    }
    // Two gates a minute: the third write is refused, never asked.
    s.edit(|c| {
        c.replace(
            "  delegation:\n",
            "  delegation:\n    consent_per_minute: 2\n",
        )
    });
    let _d = s.daemon(h);
    let n = s.start("three");
    let g = tryo!(s.gates(h, 2));
    let deadline = Instant::now() + Duration::from_secs(10);
    while s.events("delegation.consent.rate_limited").is_empty() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(100));
    }
    let limited = s.events("delegation.consent.rate_limited");
    let open = s.listing(h)["gates"].as_array().map_or(0, Vec::len);
    for x in &g {
        let _ = s.cli(
            h,
            &["--delegation-deny", x["gate"].as_str().unwrap_or_default()],
        );
    }
    let run = tryo!(s.ended("three", n, Duration::from_secs(30)));
    Outcome::require(
        limited.len() == 1 && limited[0]["per_minute"] == 2 && open == 2,
        format!("the third gate is refused, recorded: {limited:?}, {open} open"),
    )
    .and(|| Outcome::require(run["status"] == "failed", format!("the run fails: {run}")))
    .and(|| {
        Outcome::require(
            s.made("create_issue") == 3,
            "no write of the second run is made",
        )
    })
}

/// Work acting with a person's grant does not reach an agent-mode server
/// tagged `egress` (mode mixing, T10): the step is refused and the server
/// sent nothing; listed in the person's template's `agent_mode_servers`,
/// the step is made, and recorded as an agent-mode call from delegated
/// work.
pub(super) fn agent_mode_servers_are_hidden_from_delegated_work(h: &Harness) -> Outcome {
    let s = Setup::new(
        h,
        json!({}),
        &Opts {
            audit_chain: true,
            extra: "  - name: bot-call\n    steps:\n      start: {kind: manual}\n      call: {kind: mcp.tool, depends_on: [start], server: bot, tool: mock.ops, args: {}}\n      done: {kind: finish, depends_on: [call]}\n  - name: read-then-bot\n    steps:\n      start: {kind: manual}\n      read: {kind: mcp.tool, depends_on: [start], server: linear, tool: whoami, args: {}}\n      call: {kind: mcp.tool, depends_on: [read], server: bot, tool: mock.ops, args: {}}\n      done: {kind: finish, depends_on: [call]}\n".into(),
            ..Opts::default()
        },
    );
    let addr = s.tmp.path().join("bot.addr");
    let bot_log = s.tmp.path().join("bot.log");
    let bot = h.spawn_mock_mcp_at(&addr, "mock://noop", "2025-11-25", &bot_log);
    let endpoint = format!("{}/mcp", bot.endpoint());
    s.edit(|c| {
        c.replace(
            "mcp:\n  servers:\n",
            &format!("mcp:\n  servers:\n    - name: bot\n      endpoint: \"{endpoint}\"\n      tags: {{\"*\": [egress]}}\n"),
        )
    });
    let calls = || {
        std::fs::read_to_string(&bot_log)
            .unwrap_or_default()
            .matches("mock.ops")
            .count()
    };
    {
        let _d = s.daemon(h);
        tryo!(s.login(h));
        let before = calls();
        let run = tryo!(s.run("bot-call"));
        let o = failed_with(&run, "mode_mixing")
            .and(|| Outcome::require(calls() == before, "the hidden server is sent nothing"))
            .and(|| {
                Outcome::require(
                    chain(s.tmp.path()).iter().any(|r| {
                        r["action"] == "delegation.agent_mode_call" && r["outcome"] == "refused"
                    }),
                    "the refusal is recorded",
                )
            });
        if !o.passed {
            return o;
        }
    }
    s.edit(|c| {
        c.replace(
            "principals: {source: operator}, idp: okta,",
            "principals: {source: operator}, idp: okta, agent_mode_servers: [bot],",
        )
    });
    let _d = s.daemon(h);
    let before = calls();
    let run = tryo!(s.run("bot-call"));
    let o = completed(&run)
        .and(|| Outcome::require(calls() > before, "the listed server is called"))
        .and(|| {
            Outcome::require(
                chain(s.tmp.path()).iter().any(|r| {
                    r["action"] == "delegation.agent_mode_call"
                        && r["outcome"] == "admitted"
                        && r["target"]["server"] == "bot"
                }),
                "the listed server's call is recorded as made from delegated work",
            )
        });
    if !o.passed {
        return o;
    }
    // Listed, it is still egress under agentd's own credential: after a
    // delegated read, the run holds the person's data and is tainted, so
    // the call asks the person.
    let before = calls();
    let n = s.start("read-then-bot");
    let g = tryo!(s.gates(h, 1));
    let rule = g[0]["rendered"]["rule"].clone();
    let no = s.cli(
        h,
        &[
            "--delegation-deny",
            g[0]["gate"].as_str().unwrap_or_default(),
        ],
    );
    let run = tryo!(s.ended("read-then-bot", n, Duration::from_secs(30)));
    Outcome::require(
        rule == "egress_from_tainted_work_holding_delegated_data",
        format!("the agent-mode egress after a delegated read asks: {g:?}"),
    )
    .and(|| Outcome::require(no.code == Some(0), "deny"))
    .and(|| failed_with(&run, "policy_denied"))
    .and(|| Outcome::require(calls() == before, "the denied call is not made"))
}

/// A workflow an instruction document defined asks the person before each
/// delegated `egress` call — not at its first use alone — until the
/// person's template lists it by name and definition digest; listed, it
/// asks at the first use per run and tool, as an operator's workflow does.
pub(super) fn a_documents_workflow_asks_unless_listed(h: &Harness) -> Outcome {
    let s = Setup::new(
        h,
        json!({}),
        &Opts {
            tag_map: Some(EGRESS_WRITES),
            trusted_output: true,
            ..Opts::default()
        },
    );
    s.edit(|c| {
        c.replace(
            "agent: {name: ema, preflight: never}\n",
            "agent:\n  name: ema\n  preflight: never\n  instruction: |\n    File what the workflows say.\n\n    :::!workflow{name=doc-write}\n    steps:\n      start: {kind: manual}\n      one: {kind: mcp.tool, depends_on: [start], server: linear, tool: create_issue, args: {title: one}}\n      two: {kind: mcp.tool, depends_on: [one], server: linear, tool: create_issue, args: {title: two}}\n      done: {kind: finish, depends_on: [two]}\n    :::\n",
        )
    });
    let digest = {
        let _d = s.daemon(h);
        tryo!(s.login(h));
        let n = s.start("doc-write");
        let g1 = tryo!(s.gates(h, 1));
        let r1 = g1[0]["rendered"].clone();
        let _ = s.cli(
            h,
            &[
                "--delegation-approve",
                g1[0]["gate"].as_str().unwrap_or_default(),
            ],
        );
        let deadline = Instant::now() + Duration::from_secs(20);
        let g2 = loop {
            let l = s.listing(h);
            if let Some(g) = l["gates"].as_array().and_then(|a| a.first()).cloned() {
                break g;
            }
            if Instant::now() > deadline {
                return Outcome::fail(format!(
                    "the second use did not ask:\n{}",
                    tail(&s.log_text())
                ));
            }
            std::thread::sleep(Duration::from_millis(200));
        };
        let _ = s.cli(
            h,
            &["--delegation-deny", g2["gate"].as_str().unwrap_or_default()],
        );
        let run = tryo!(s.ended("doc-write", n, Duration::from_secs(30)));
        let o = Outcome::require(
            r1["rule"] == "document_sourced_delegated_egress"
                && r1["document_sourced"] == true
                && r1["workflow"] == "doc-write"
                && g2["rendered"]["rule"] == "document_sourced_delegated_egress",
            format!("a document's workflow asks at every use: {r1} {g2}"),
        )
        .and(|| failed_with(&run, "policy_denied"))
        .and(|| {
            Outcome::require(
                s.made("create_issue") == 1,
                "the denied second write is not made",
            )
        });
        if !o.passed {
            return o;
        }
        let list = s.op("status", json!({}));
        let wf = list["result"]["message"]["parts"][0]["data"]["workflows"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|w| w["name"] == "doc-write")
            .cloned()
            .unwrap_or(Value::Null);
        match wf["hash"].as_str() {
            Some(h) => h.to_string(),
            None => return Outcome::fail(format!("status shows no digest: {list}")),
        }
    };
    s.edit(|c| {
        c.replace(
            "linear: {scopes: [\"issues:read\", \"issues:write\"]}",
            &format!("linear: {{scopes: [\"issues:read\", \"issues:write\"], document_workflows: [{{name: doc-write, digest: \"{digest}\"}}]}}"),
        )
    });
    let _d = s.daemon(h);
    let n = s.start("doc-write");
    let g = tryo!(s.gates(h, 1));
    let rule = g[0]["rendered"]["rule"].clone();
    let _ = s.cli(
        h,
        &[
            "--delegation-approve",
            g[0]["gate"].as_str().unwrap_or_default(),
        ],
    );
    let run = tryo!(s.ended("doc-write", n, Duration::from_secs(30)));
    Outcome::require(
        rule == "delegated_egress",
        format!("listed, it asks at the first use alone: {g:?}"),
    )
    .and(|| completed(&run))
    .and(|| Outcome::require(s.made("create_issue") == 3, "both listed writes are made"))
}

/// A subagent a document's workflow spawns does that workflow's work
/// (T27): under `subagents: inherit` its delegated egress call asks under
/// `document_sourced_delegated_egress`, naming the workflow, as the run's
/// own step would — not at its first use alone — and denied, it is not
/// made.
pub(super) fn a_documents_subagent_is_its_workflows_work(h: &Harness) -> Outcome {
    let s = Setup::new(
        h,
        json!({}),
        &Opts {
            tag_map: Some(EGRESS_WRITES),
            trusted_output: true,
            ..Opts::default()
        },
    );
    s.edit(|c| {
        c.replace(
            "agent: {name: ema, preflight: never}\n",
            "agent:\n  name: ema\n  preflight: never\n  instruction: |\n    File what the workflows say.\n\n    :::!workflow{name=doc-sub}\n    steps:\n      start: {kind: manual}\n      c: {kind: subagent, depends_on: [start], instruction: \"File it.\", tools: [create_issue], servers: [linear]}\n      done: {kind: finish, depends_on: [c]}\n    :::\n",
        )
        .replace(
            "principals: {source: operator}, idp: okta,",
            "principals: {source: operator}, idp: okta, subagents: inherit,",
        )
    });
    let ask = json!([{"tool": "create_issue", "args": {"title": "From the subagent"}}]);
    let _d = s.daemon_env(h, &[("AGENTD_TEST_WORKER_ASK", &ask.to_string())]);
    tryo!(s.login(h));
    let n = s.start("doc-sub");
    let g = tryo!(s.gates(h, 1));
    let r = g[0]["rendered"].clone();
    let _ = s.cli(
        h,
        &[
            "--delegation-deny",
            g[0]["gate"].as_str().unwrap_or_default(),
        ],
    );
    tryo!(s.ended("doc-sub", n, Duration::from_secs(30)));
    let asked = s.events("test.worker_ask");
    Outcome::require(
        r["rule"] == "document_sourced_delegated_egress"
            && r["document_sourced"] == true
            && r["workflow"] == "doc-sub",
        format!("the subagent's call asks as its workflow's: {r}"),
    )
    .and(|| {
        Outcome::require(
            asked
                .iter()
                .any(|e| e["tool"] == "create_issue" && e["is_error"] == true),
            format!("the denied call is answered as refused: {asked:?}"),
        )
    })
    .and(|| Outcome::require(s.made("create_issue") == 0, "the denied call is not made"))
}

/// Under `subjects: pairwise` an ID-JAG naming another `sub` than the
/// grant's is taken (T25): its `sub` is what `agent/acting_for` carries,
/// and `agent/grant` is a random handle per server — the same across calls
/// to one server, another at the next, and never the grant's id.
pub(super) fn pairwise_subjects_are_recorded_not_compared(h: &Harness) -> Outcome {
    let s = Setup::new(
        h,
        json!({"id_jag": {"sub": "pw-0042"}}),
        &Opts {
            two_servers: true,
            ..Opts::default()
        },
    );
    s.edit(|c| {
        c.replace(
            "      subject: refresh_token\n",
            "      subject: refresh_token\n      subjects: pairwise\n",
        )
    });
    let _d = s.daemon(h);
    tryo!(s.login(h));
    let one = tryo!(s.run("who"));
    let two = tryo!(s.run("who"));
    let other = tryo!(s.run("who2"));
    let grant = s.listing(h)["grants"][0]["grant_id"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    let metas: Vec<(String, Value)> = s
        .xaa
        .mcp_calls("tools/call")
        .iter()
        .map(|c| {
            (
                c["path"].as_str().unwrap_or_default().to_string(),
                c["body"]["params"]["_meta"].clone(),
            )
        })
        .collect();
    let handles = |path: &str| -> Vec<String> {
        metas
            .iter()
            .filter(|(p, _)| p == path)
            .filter_map(|(_, m)| m["agent/grant"].as_str().map(str::to_string))
            .collect()
    };
    let (h1, h2) = (handles("/mcp"), handles("/mcp2"));
    completed(&one)
        .and(|| completed(&two))
        .and(|| completed(&other))
        .and(|| {
            Outcome::require(
                metas
                    .iter()
                    .all(|(_, m)| m["agent/acting_for"] == "pw-0042"),
                format!("acting_for is the ID-JAG's own sub: {metas:?}"),
            )
        })
        .and(|| {
            Outcome::require(
                h1.len() == 2
                    && h1[0] == h1[1]
                    && h2.len() == 1
                    && h2[0] != h1[0]
                    && !h1[0].is_empty()
                    && h1[0] != grant
                    && h2[0] != grant,
                format!("a random handle per server: {h1:?} {h2:?} (grant {grant})"),
            )
        })
}

/// A person's mints at one resource are paced (`budget.per_minute`): with
/// one a minute and a token per scope set, the read the enrolment's
/// catalogue mint already covers is made, and the write that needs a second
/// mint within the minute is refused, `budget_exhausted`, before the IdP is
/// asked.
pub(super) fn a_persons_mints_are_paced(h: &Harness) -> Outcome {
    let s = Setup::new(
        h,
        json!({}),
        &Opts {
            tag_map: Some("{whoami: [], create_issue: [], list_issues: [untrusted_input]}"),
            ttl: Some("30d, budget: {per_minute: 1}"),
            ..Opts::default()
        },
    );
    s.edit(|c| {
        c.replace(
            "        tool_scopes: {create_issue: [\"issues:write\"]}\n",
            "        tool_scopes: {create_issue: [\"issues:write\"]}\n        scope_mode: per_tool\n",
        )
    });
    let _d = s.daemon(h);
    tryo!(s.login(h));
    let exchanges = s.xaa.grants("idp", EXCHANGE).len();
    let run = tryo!(s.run("read-write"));
    failed_with(&run, "budget_exhausted")
        .and(|| {
            Outcome::require(
                run["err"]
                    .as_str()
                    .is_some_and(|e| e.contains("budget.per_minute")),
                format!("the refusal names the rate: {run}"),
            )
        })
        .and(|| Outcome::require(s.made("whoami") == 1, "the read is made"))
        .and(|| {
            Outcome::require(
                s.xaa.grants("idp", EXCHANGE).len() == exchanges,
                "the paced mint never reaches the IdP",
            )
        })
}

/// A delegated server's question (`elicitation/create`) is put to the
/// person whose grant its connection carries, alone (T24): on a gate of its
/// own in agentd's words, the server's quoted; an operator's answer is
/// refused and the gate stays open; the person's is shaped to the server's
/// schema and handed back, and the call goes on.
pub(super) fn an_elicitation_asks_the_grants_person(h: &Harness) -> Outcome {
    let s = Fed::new(
        h,
        &FedOpts {
            login: true,
            templates: vec![PEOPLE],
            gated_tags: Some("{\"*\": [sensitive]}"),
            ..FedOpts::default()
        },
    );
    s.xaa.control(
        json!({"elicit": {"tool": "create_issue", "message": "Which project?",
        "schema": {"type": "object", "properties": {"project": {"type": "string"}},
                   "required": ["project"]}}}),
    );
    let _d = s.daemon(h);
    let alice = tryo!(s.login(ALICE));
    let n = tryo!(s.start_as(&alice, "write"));
    let deadline = Instant::now() + Duration::from_secs(20);
    let ask = loop {
        if let Some(a) = s.events("human.ask").first().cloned() {
            break a;
        }
        if Instant::now() > deadline {
            return Outcome::fail(format!("no gate asked:\n{}", tail(&s.log_text())));
        }
        std::thread::sleep(Duration::from_millis(100));
    };
    let task = ask["task"].as_str().unwrap_or_default().to_string();
    let question = ask["question"].as_str().unwrap_or_default().to_string();
    let by_operator = s.rpc_as(
        &s.ops,
        "SendMessage",
        crate::checks::util::text_params("ops-answer", Some(&task), true),
    );
    let by_alice = s.rpc_as(
        &alice,
        "SendMessage",
        crate::checks::util::text_params("atlas", Some(&task), true),
    );
    let run = tryo!(s.ended("write", n));
    let answered: Vec<Value> = s
        .xaa
        .record()
        .into_iter()
        .filter(|r| r["at"] == "mcp" && r["body"]["id"] == "elicit-1")
        .collect();
    Outcome::require(
        question.starts_with("agentd: the delegated server \"gated\" asks you")
            && question.contains("\"Which project?\""),
        format!("agentd's words, the server's quoted: {question}"),
    )
    .and(|| {
        Outcome::require(
            by_operator["error"]["message"]
                .as_str()
                .is_some_and(|m| m.contains("this decision is for")),
            format!("an operator's answer is refused: {by_operator}"),
        )
    })
    .and(|| Outcome::require(by_alice.get("error").is_none(), format!("hers: {by_alice}")))
    .and(|| completed(&run))
    .and(|| {
        Outcome::require(
            answered.len() == 1
                && answered[0]["body"]["result"]["action"] == "accept"
                && answered[0]["body"]["result"]["content"]["project"] == "atlas",
            format!("the server is handed her answer, shaped: {answered:?}"),
        )
    })
}

/// A delegated server's question is answered only through a session that
/// holds the grant it was asked through (T24), as a consent gate is: of two
/// devices approved under one name, the one that never signed in at the IdP
/// as the identity the name is bound to is refused (`no_idp_sign_in`), the
/// gate left open and the server handed nothing; the bound one's answer is
/// taken.
pub(super) fn an_elicitation_is_answered_through_the_grant(h: &Harness) -> Outcome {
    let s = Fed::new(
        h,
        &FedOpts {
            device: true,
            templates: vec![DEVICES],
            first_binding: true,
            gated_tags: Some("{\"*\": [sensitive]}"),
            ..FedOpts::default()
        },
    );
    s.xaa.control(
        json!({"elicit": {"tool": "create_issue", "message": "Which project?",
        "schema": {"type": "object", "properties": {"project": {"type": "string"}},
                   "required": ["project"]}}}),
    );
    let _d = s.daemon(h);
    let (bound, _) = tryo!(s.device_session("laptop"));
    let (other, _) = tryo!(s.device_session("laptop"));
    let (state, text) = tryo!(s.enrol_signed_in(&bound, ALICE));
    if state != "TASK_STATE_COMPLETED" {
        return Outcome::fail(format!("the enrolment: {state} {text}"));
    }
    let n = tryo!(s.start_as(&bound, "write"));
    let deadline = Instant::now() + Duration::from_secs(20);
    let ask = loop {
        if let Some(a) = s.events("human.ask").first().cloned() {
            break a;
        }
        if Instant::now() > deadline {
            return Outcome::fail(format!("no gate asked:\n{}", tail(&s.log_text())));
        }
        std::thread::sleep(Duration::from_millis(100));
    };
    let task = ask["task"].as_str().unwrap_or_default().to_string();
    let by_other = s.rpc_as(
        &other,
        "SendMessage",
        crate::checks::util::text_params("mallory", Some(&task), true),
    );
    let by_bound = s.rpc_as(
        &bound,
        "SendMessage",
        crate::checks::util::text_params("atlas", Some(&task), true),
    );
    let run = tryo!(s.ended("write", n));
    let refused = s.events("delegation.elicitation.refused");
    let answered: Vec<Value> = s
        .xaa
        .record()
        .into_iter()
        .filter(|r| r["at"] == "mcp" && r["body"]["id"] == "elicit-1")
        .collect();
    Outcome::require(
        by_other["error"]["message"]
            .as_str()
            .is_some_and(|m| m.contains("has not signed in at the IdP")),
        format!("the session that never proved the binding is refused: {by_other}"),
    )
    .and(|| {
        Outcome::require(
            refused.len() == 1 && refused[0]["why"] == "no_idp_sign_in",
            format!("and the refusal recorded: {refused:?}"),
        )
    })
    .and(|| {
        Outcome::require(
            by_bound.get("error").is_none(),
            format!("the bound session's answer: {by_bound}"),
        )
    })
    .and(|| completed(&run))
    .and(|| {
        Outcome::require(
            answered.len() == 1 && answered[0]["body"]["result"]["content"]["project"] == "atlas",
            format!("the server is handed the bound session's answer alone: {answered:?}"),
        )
    })
}

/// A person revokes their own grant from their own session (self-service,
/// RFC 0044 §5.8): their next call is revoked; another person may not
/// revoke it for them; an operator revokes anyone's. Of two devices approved
/// under one name, the one that never proved the identity the name is bound
/// to holds no grant, so revokes none: the bound one's runs go on.
pub(super) fn a_person_revokes_their_own_grant(h: &Harness) -> Outcome {
    let devices = {
        let s = Fed::new(
            h,
            &FedOpts {
                device: true,
                templates: vec![DEVICES],
                first_binding: true,
                ..FedOpts::default()
            },
        );
        let _d = s.daemon(h);
        let (bound, _) = tryo!(s.device_session("laptop"));
        let (other, _) = tryo!(s.device_session("laptop"));
        let (state, text) = tryo!(s.enrol_signed_in(&bound, ALICE));
        if state != "TASK_STATE_COMPLETED" {
            return Outcome::fail(format!("the enrolment: {state} {text}"));
        }
        let before = s.listed(&s.ops);
        let by_other = s.op_as(&other, "delegation.revoke", json!({}));
        let (ostate, otext) = match by_other["result"]["task"]["id"].as_str() {
            Some(id) => tryo!(s.task_end(&other, id)),
            None => return Outcome::fail(format!("delegation.revoke: {by_other}")),
        };
        let after = s.listed(&s.ops);
        let still = tryo!(s.run_as(&bound, "who"));
        Outcome::require(
            ostate == "TASK_STATE_FAILED" && otext.contains("holds no grant this session may use"),
            format!("the unbound device revokes nothing: {ostate} {otext}"),
        )
        .and(|| {
            Outcome::require(
                after["grants"].as_array().is_some_and(|g| g.len() == 1)
                    && after["grants"][0]["epoch"] == before["grants"][0]["epoch"],
                format!("the grant is untouched: {before} then {after}"),
            )
        })
        .and(|| completed(&still))
    };
    if !devices.passed {
        return devices;
    }
    let s = Fed::new(
        h,
        &FedOpts {
            login: true,
            templates: vec![PEOPLE],
            ..FedOpts::default()
        },
    );
    let _d = s.daemon(h);
    let alice = tryo!(s.login(ALICE));
    let bob = tryo!(s.login(BOB));
    let alice_id = format!("user:oidc:{}#{ALICE}", s.xaa.idp);
    let bob_id = format!("user:oidc:{}#{BOB}", s.xaa.idp);
    let not_his = s.op_as(&bob, "delegation.revoke", json!({"target": alice_id}));
    let own = s.op_as(&alice, "delegation.revoke", json!({}));
    let (state, text) = match own["result"]["task"]["id"].as_str() {
        Some(id) => tryo!(s.task_end(&alice, id)),
        None => return Outcome::fail(format!("delegation.revoke: {own}")),
    };
    let hers = tryo!(s.run_as(&alice, "who"));
    let his = tryo!(s.run_as(&bob, "who"));
    let by_op = s.operator("delegation.revoke", json!({"target": bob_id}));
    if let Some(id) = by_op["result"]["task"]["id"].as_str() {
        tryo!(s.task_end(&s.ops, id));
    }
    let his_after = tryo!(s.run_as(&bob, "who"));
    Outcome::require(
        not_his["error"]["message"]
            .as_str()
            .is_some_and(|m| m.contains("revokes another's grants")),
        format!("another person may not revoke hers: {not_his}"),
    )
    .and(|| {
        Outcome::require(
            state == "TASK_STATE_COMPLETED" && text.contains(&alice_id),
            format!("she revokes her own: {state} {text}"),
        )
    })
    .and(|| failed_with(&hers, "revoked"))
    .and(|| completed(&his))
    .and(|| failed_with(&his_after, "revoked"))
}

/// The SHA-256 of a PEM certificate's SubjectPublicKeyInfo, hex: a DER walk
/// to the seventh element of the TBSCertificate (the sixth without an
/// explicit version).
fn spki_sha256(pem: &str) -> String {
    let b64: String = pem
        .lines()
        .filter(|l| !l.starts_with("-----"))
        .collect::<Vec<_>>()
        .concat();
    let der = std_b64_decode(&b64);
    // One TLV at `at`: (tag, header length, content length).
    let tlv = |at: usize| -> (u8, usize, usize) {
        let tag = der[at];
        let first = der[at + 1];
        if first < 0x80 {
            return (tag, 2, usize::from(first));
        }
        let n = usize::from(first & 0x7f);
        let len = der[at + 2..at + 2 + n]
            .iter()
            .fold(0usize, |a, b| (a << 8) | usize::from(*b));
        (tag, 2 + n, len)
    };
    let (_, cert_h, _) = tlv(0);
    let tbs = cert_h;
    let (_, tbs_h, _) = tlv(tbs);
    let mut at = tbs + tbs_h;
    if der[at] == 0xa0 {
        let (_, hl, l) = tlv(at);
        at += hl + l;
    }
    // serialNumber, signature, issuer, validity, subject: then the key.
    for _ in 0..5 {
        let (_, hl, l) = tlv(at);
        at += hl + l;
    }
    let (_, hl, l) = tlv(at);
    let spki = &der[at..at + hl + l];
    ring::digest::digest(&ring::digest::SHA256, spki)
        .as_ref()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// One JSON-RPC call over mutual TLS to `addr` as the client certificate
/// `cert`/`key`: the envelope.
fn mtls_rpc(addr: &str, cert: &Path, key: &Path, method: &str, params: Value) -> Value {
    let identity = net::tls::ClientIdentity::from_pem(
        &std::fs::read(cert).expect("cert"),
        &std::fs::read(key).expect("key"),
    )
    .expect("identity");
    let ca = std::fs::read(crate::harness::tls_fixture("ca.pem")).expect("ca");
    let tcp = std::net::TcpStream::connect(addr).expect("connect");
    tcp.set_read_timeout(Some(Duration::from_secs(30))).ok();
    let mut tls = net::tls::connect_with_ca(tcp, "127.0.0.1", &ca, Some(&identity)).expect("tls");
    let body = json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params}).to_string();
    let _ = write!(
        tls,
        "POST / HTTP/1.1\r\nHost: {addr}\r\nContent-Type: application/json\r\nA2A-Version: 1.0\r\nA2A-Extensions: {COMMAND}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let mut out = Vec::new();
    let _ = tls.read_to_end(&mut out);
    let text = String::from_utf8_lossy(&out).to_string();
    let payload = text
        .split_once("\r\n\r\n")
        .map(|(_, b)| b)
        .unwrap_or_default();
    serde_json::from_str(payload).unwrap_or(json!({"unparsed": text}))
}

/// A certificate principal delegates (RFC 0044 §5.2): its rule admits one
/// client certificate's key and pins its IdP identity; over that
/// certificate it enrols with the IdP's device grant and its run is made
/// with its own grant, which is the key's (`x509:<SPKI SHA-256>`) — and a
/// template admitting `x509` is what lets it.
pub(super) fn a_certificate_principal_delegates(h: &Harness) -> Outcome {
    let s = Setup::new(h, json!({}), &Opts::default());
    let (cert, key) = (
        crate::harness::tls_fixture("client.pem"),
        crate::harness::tls_fixture("client.key"),
    );
    let spki = spki_sha256(&std::fs::read_to_string(&cert).expect("cert"));
    let port = crate::checks::util::free_port();
    let addr = format!("127.0.0.1:{port}");
    s.edit(|c| {
        c.replace(
            &format!("  listen: \"unix://{}\"\n", s.sock.display()),
            &format!(
                "  listen: \"https://{addr}\"\n  tls: {{cert: \"{}\", key: \"{}\", client_ca: \"{}\"}}\n",
                crate::harness::tls_fixture("server.pem").display(),
                crate::harness::tls_fixture("server.key").display(),
                crate::harness::tls_fixture("ca.pem").display()
            ),
        )
        .replace(
            &format!("    - {{match: {{uid: {}}}, role: operator, id: alice,", uid()),
            &format!("    - {{match: {{spki: \"{spki}\"}}, role: user, id: alice,"),
        )
        .replace("principals: {source: operator}", "principals: {source: x509}")
    });
    let log = s.tmp.path().join("daemon-x509.log");
    let _d = h.spawn_logged(&["--config", s.cfg()], &log);
    let deadline = Instant::now() + Duration::from_secs(15);
    while std::net::TcpStream::connect(&addr).is_err() {
        if Instant::now() > deadline {
            return Outcome::fail(format!(
                "the daemon never listened: {}",
                std::fs::read_to_string(&log).unwrap_or_default()
            ));
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let _ = std::fs::remove_file(&s.log);
    let _ = std::os::unix::fs::symlink(&log, &s.log);
    let op = |op: &str, args: Value| {
        mtls_rpc(
            &addr,
            &cert,
            &key,
            "SendMessage",
            crate::checks::util::command_send_params(op, &args),
        )
    };
    let enrol = op(
        "delegation.enrol",
        json!({"idp": "okta", "grant": "device"}),
    );
    let Some(task) = enrol["result"]["task"]["id"].as_str().map(str::to_string) else {
        return Outcome::fail(format!("delegation.enrol: {enrol}"));
    };
    let deadline = Instant::now() + Duration::from_secs(30);
    let state = loop {
        let t = mtls_rpc(&addr, &cert, &key, "GetTask", json!({"id": task}));
        let st = t["result"]["status"]["state"]
            .as_str()
            .unwrap_or_default()
            .to_string();
        if st == "TASK_STATE_COMPLETED" || st == "TASK_STATE_FAILED" {
            break (st, t);
        }
        if Instant::now() > deadline {
            return Outcome::fail(format!("the enrolment did not end: {t}"));
        }
        std::thread::sleep(Duration::from_millis(200));
    };
    let n = s.runs_done("who").len();
    let started = op("workflow.run", json!({"workflow": "who"}));
    let run = tryo!(s.ended("who", n, Duration::from_secs(30)));
    let listed = doc(&op("delegation.list", json!({})));
    Outcome::require(
        state.0 == "TASK_STATE_COMPLETED",
        format!("the certificate principal enrols: {}", state.1),
    )
    .and(|| {
        Outcome::require(
            started.get("error").is_none(),
            format!("workflow.run: {started}"),
        )
    })
    .and(|| completed(&run))
    .and(|| {
        Outcome::require(
            listed["grants"][0]["principal"] == format!("x509:{spki}").as_str()
                && listed["grants"][0]["sub"] == "00u1alice",
            format!("the grant is the key's: {listed}"),
        )
    })
}

/// A stale instruction makes no delegated call (T27, §5.12 I): once the
/// registry the instruction came from stops answering past its freshness
/// deadline (`unavailable: freeze`), a model turn in a run already in
/// flight — its prompt holds the instruction — is refused its delegated
/// call (`policy_denied`) and the server is sent nothing; so is the
/// operator's workflow the turn starts with `workflow.run`, whose inputs
/// are the model's — and the child run that run starts — and the operator's
/// run the turn's `workflow.signal` wakes; a conversation's turn writes no
/// workflow (`workflow.create`, which only a conversation's turn has);
/// while the operator's own run of their workflow still makes its call.
pub(super) fn a_stale_instruction_makes_no_delegated_call(h: &Harness) -> Outcome {
    let tmp = h.tempdir();
    let made = json!({"name": "model-made", "steps": {"start": {"kind": "manual"},
                      "done": {"kind": "finish", "depends_on": ["start"]}}});
    let playbook = json!({"turns": [
        {"tool_calls": [{"name": "whoami", "arguments": {}},
                        {"name": "workflow.run", "arguments": {"name": "direct"}},
                        {"name": "workflow.signal", "arguments": {"name": "go"}}]},
        {"content": "done"}],
        "match": [
            {"when_contains": "model-made", "content": "done"},
            {"when_contains": "CREATE-ONE",
             "tool_calls": [{"name": "workflow.create", "arguments": {"definition": made}}]}]});
    let pb = crate::checks::util::write_file(&tmp, "playbook.json", &playbook.to_string());
    let llm = h.mock_llm(&format!("file:{pb}"));
    let flows = "  - name: late-turn\n    steps:\n      start: {kind: manual}\n      nap: {kind: sleep, depends_on: [start], duration: 6s}\n      ask: {kind: agent, depends_on: [nap], instruction: \"Who am I at linear?\"}\n      done: {kind: finish, depends_on: [ask]}\n  - name: late-step\n    steps:\n      start: {kind: manual}\n      nap: {kind: sleep, depends_on: [start], duration: 6s}\n      read: {kind: mcp.tool, depends_on: [nap], server: linear, tool: whoami, args: {}}\n      done: {kind: finish, depends_on: [read]}\n  - name: direct\n    steps:\n      start: {kind: manual}\n      child: {kind: workflow, depends_on: [start], name: direct-child, mode: sync}\n      done: {kind: finish, depends_on: [child]}\n  - name: direct-child\n    steps:\n      start: {kind: manual}\n      read: {kind: mcp.tool, depends_on: [start], server: linear, tool: whoami, args: {}}\n      done: {kind: finish, depends_on: [read]}\n  - name: waiter\n    steps:\n      start: {kind: manual}\n      w: {kind: wait, depends_on: [start], on: signal, signal: go, timeout: 1m}\n      read: {kind: mcp.tool, depends_on: [w], server: linear, tool: whoami, args: {}}\n      done: {kind: finish, depends_on: [read]}\n";
    let s = Setup::new(
        h,
        json!({}),
        &Opts {
            extra: flows.to_string(),
            ..Opts::default()
        },
    );
    let addr = s.tmp.path().join("registry.addr");
    let mut registry = Some(h.spawn_mock_mcp_at(
        &addr,
        "mock://watched",
        "2025-11-25",
        &s.tmp.path().join("registry.log"),
    ));
    let endpoint = registry.as_ref().map(|r| r.endpoint()).unwrap_or_default();
    s.edit(|c| {
        c.replace(
            "agent: {name: ema, preflight: never}\n",
            "agent:\n  name: ema\n  preflight: never\n  instruction: {mcp: \"instruction://ins_mock@stable\", refresh: 1s, unavailable: freeze}\n  document_capabilities: [compute]\n",
        )
        .replace(
            "mcp:\n  servers:\n",
            &format!("mcp:\n  servers:\n    - name: registry\n      endpoint: \"{endpoint}/mcp\"\n"),
        )
        .replace(
            "intelligence: {endpoints: \"mock:final\", model: m}",
            &format!("intelligence: {{endpoints: \"{}\", model: m}}", llm.uri),
        )
    });
    let _d = s.daemon(h);
    tryo!(s.login(h));
    let turn = s.start("late-turn");
    let step = s.start("late-step");
    let waiter = s.start("waiter");
    let started = Instant::now();
    // The registry dies while both runs sleep; the watch freezes the agent.
    drop(registry.take());
    let deadline = Instant::now() + Duration::from_secs(20);
    while s.events("instruction.unavailable").is_empty() {
        if Instant::now() > deadline {
            return Outcome::fail(format!("the watch never froze:\n{}", tail(&s.log_text())));
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    if started.elapsed() >= Duration::from_secs(6) {
        return Outcome::note("skipped: the freeze came after the runs woke on this host");
    }
    let turned = tryo!(s.ended("late-turn", turn, Duration::from_secs(30)));
    let stepped = tryo!(s.ended("late-step", step, Duration::from_secs(30)));
    let direct = tryo!(s.ended("direct-child", 0, Duration::from_secs(30)));
    let woken = tryo!(s.ended("waiter", waiter, Duration::from_secs(30)));
    // Still frozen: a conversation's turn, which alone has
    // `workflow.create`.
    let sent = unix_rpc(
        &s.sock,
        "SendMessage",
        crate::checks::util::text_params("CREATE-ONE", None, true),
    );
    if sent.get("error").is_some() {
        return Outcome::fail(format!("SendMessage: {sent}"));
    }
    let deadline = Instant::now() + Duration::from_secs(20);
    let create = loop {
        let r: Vec<Value> = s
            .events("tool.result")
            .into_iter()
            .filter(|e| e["tool"] == "workflow.create")
            .collect();
        if !r.is_empty() {
            break r;
        }
        if Instant::now() > deadline {
            return Outcome::fail(format!(
                "the turn made no workflow.create:\n{}",
                tail(&s.log_text())
            ));
        }
        std::thread::sleep(Duration::from_millis(100));
    };
    let defined: Vec<Value> = s
        .events("workflow.defined")
        .into_iter()
        .filter(|e| e["name"] == "model-made")
        .collect();
    let denied: Vec<Value> = s
        .events("delegation.denied")
        .into_iter()
        .filter(|e| e["code"] == "policy_denied")
        .collect();
    let frozen = &s.events("instruction.unavailable")[0];
    Outcome::require(
        frozen["policy"] == "freeze",
        format!("the policy: {frozen}"),
    )
    .and(|| {
        Outcome::require(
            denied.len() == 1
                && denied[0]["run"] == turned["run"]
                && denied[0]["error"]
                    .as_str()
                    .is_some_and(|e| e.contains("could not be confirmed current")),
            format!("the turn's delegated call is refused, saying why: {denied:?}"),
        )
    })
    .and(|| {
        let refused = |run: &Value| {
            run["status"] == "failed"
                && run["err"].as_str().is_some_and(|e| {
                    e.contains("(policy_denied)") && e.contains("could not be confirmed current")
                })
        };
        Outcome::require(
            refused(&direct) && refused(&woken),
            format!(
                "so are the child of the run the turn started and the run its signal woke, saying why: {direct} {woken}"
            ),
        )
    })
    .and(|| {
        Outcome::require(
            create[0]["is_error"] == true && defined.is_empty(),
            format!("the conversation's turn writes no workflow: {create:?} {defined:?}"),
        )
    })
    .and(|| completed(&stepped))
    .and(|| {
        Outcome::require(
            s.made("whoami") == 1,
            "the operator's step makes its call and the turn's reaches nothing",
        )
    })
}

/// A worker uses the delegated tools it was offered and no other (RFC 0044
/// §5.7, use-authorisation per `(step, tool)`): a turn worker whose step
/// offers `whoami` alone, and a subagent spawned with `tools: [whoami]`
/// under `subagents: inherit`, each ask the supervisor for `whoami` and for
/// `create_issue`, as a worker that ignored what it was offered would (a
/// debug build's `AGENTD_TEST_WORKER_ASK` hook): `whoami` is made for each,
/// `create_issue` refused before any check or mint (`tool.refused`, `why:
/// not_offered`) and never sent.
pub(super) fn a_worker_uses_only_what_it_was_offered(h: &Harness) -> Outcome {
    let flows = "  - name: narrow-turn\n    steps:\n      start: {kind: manual}\n      ask: {kind: agent, depends_on: [start], instruction: \"Say done.\", tools: [whoami]}\n      done: {kind: finish, depends_on: [ask]}\n  - name: narrow-sub\n    steps:\n      start: {kind: manual}\n      c: {kind: subagent, depends_on: [start], instruction: \"Say done.\", tools: [whoami], servers: [linear]}\n      done: {kind: finish, depends_on: [c]}\n";
    let s = Setup::new(
        h,
        json!({}),
        &Opts {
            extra: flows.to_string(),
            ..Opts::default()
        },
    );
    s.edit(|c| {
        c.replace(
            "principals: {source: operator}, idp: okta,",
            "principals: {source: operator}, idp: okta, subagents: inherit,",
        )
    });
    let ask = json!([{"tool": "whoami"},
                     {"tool": "create_issue", "args": {"title": "From a worker"}}]);
    let _d = s.daemon_env(h, &[("AGENTD_TEST_WORKER_ASK", &ask.to_string())]);
    tryo!(s.login(h));
    let turn = tryo!(s.run("narrow-turn"));
    let sub = tryo!(s.run("narrow-sub"));
    let asked = s.events("test.worker_ask");
    let answer = |turn: bool, tool: &str| {
        asked
            .iter()
            .find(|e| e["turn"] == turn && e["tool"] == tool)
            .cloned()
            .unwrap_or(Value::Null)
    };
    let made = |a: &Value| a["is_error"] == false;
    let refused = |a: &Value| {
        a["is_error"] == true
            && a["result"]
                .as_str()
                .is_some_and(|r| r.contains("did not offer it"))
    };
    let not_offered = s
        .events("tool.refused")
        .iter()
        .filter(|e| e["why"] == "not_offered" && e["tool"] == "create_issue")
        .count();
    completed(&turn)
        .and(|| completed(&sub))
        .and(|| {
            Outcome::require(
                made(&answer(true, "whoami")) && made(&answer(false, "whoami")),
                format!("what each was offered is made: {asked:?}"),
            )
        })
        .and(|| {
            Outcome::require(
                refused(&answer(true, "create_issue")) && refused(&answer(false, "create_issue")),
                format!("what neither was offered is refused, saying why: {asked:?}"),
            )
        })
        .and(|| {
            Outcome::require(
                not_offered == 2,
                format!("each refusal recorded: {not_offered}"),
            )
        })
        .and(|| {
            Outcome::require(
                s.made("create_issue") == 0 && s.made("whoami") == 2,
                "the refused tool is never sent",
            )
        })
}

/// A local command runs beside a delegation only under the whole isolation
/// set (the user's decision of 2026-10-20): the load refuses it without a
/// worker uid of its own, naming the piece missing; with `spawn.uid`, inside
/// Landlock, every supervisor-only path closed to that uid, it loads, and a
/// command from a run holding the person's data asks them first
/// (`exec_holding_delegated_data`), then runs as the worker uid. Needs root
/// to start a command as another uid, and Landlock; skipped otherwise.
pub(super) fn a_command_beside_a_delegation_runs_isolated_and_asks(h: &Harness) -> Outcome {
    use std::os::unix::fs::PermissionsExt;
    // SAFETY: geteuid has no failure mode.
    if unsafe { libc::geteuid() } != 0 {
        return Outcome::note("skipped: starting a command as another uid needs root");
    }
    // A copy of the binary every uid may run, and a working directory the
    // worker uid may use.
    let open = h.tempdir();
    for d in [open.path().to_path_buf(), open.path().join("work")] {
        std::fs::create_dir_all(&d).expect("mkdir");
        std::fs::set_permissions(&d, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    }
    std::fs::set_permissions(
        open.path().join("work"),
        std::fs::Permissions::from_mode(0o777),
    )
    .expect("chmod");
    let exe = open.path().join("agentd");
    std::fs::copy(h.agentd(), &exe).expect("copy");
    std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    let hx = Harness::at(exe);
    let flows = "  - name: read-exec\n    steps:\n      start: {kind: manual}\n      read: {kind: mcp.tool, depends_on: [start], server: linear, tool: whoami, args: {}}\n      x: {kind: tool, depends_on: [read], name: exec, args: {cmd: touch, args: [made-by-exec]}}\n      done: {kind: finish, depends_on: [x]}\n  - name: peek\n    steps:\n      start: {kind: manual}\n      key: {kind: tool, depends_on: [start], name: exec, args: {cmd: cp, args: [\"DIR/seal\", stolen-key]}}\n      vault: {kind: tool, depends_on: [start], name: exec, args: {cmd: cp, args: [-r, \"DIR/state\", stolen-vault]}}\n      done: {kind: finish, depends_on: [key, vault]}\n";
    // Nothing of the root grant is `untrusted_input`: beside `exec`'s
    // `sensitive` and `egress` it would be the lethal trifecta. The
    // server's results vouched for, the run holds the person's data
    // untainted, so it is the command's own rule that asks.
    let s = Setup::new(
        &hx,
        json!({}),
        &Opts {
            tags: "[sensitive]",
            trusted_output: true,
            extra: flows.to_string(),
            ..Opts::default()
        },
    );
    // Every key file, the store and the chain beneath a directory only
    // agentd's uid may enter.
    std::fs::set_permissions(s.tmp.path(), std::fs::Permissions::from_mode(0o700)).expect("chmod");
    let exec = format!(
        "security:\n  experimental: {{id_jag: true}}\n  exec: {{enabled: true, allow: [touch, cp], workdir: \"{}\"}}\n",
        open.path().join("work").display()
    );
    s.edit(|c| c.replace("security:\n  experimental: {id_jag: true}\n", &exec));
    s.edit(|c| c.replace("\"DIR/", &format!("\"{}/", s.tmp.path().display())));
    let refused = s.cli(&hx, &["--validate-config"]);
    s.edit(|c| format!("spawn: {{uid: 65534, gid: 65534}}\n{c}"));
    let checked = s.cli(&hx, &["--validate-config"]);
    let o = Outcome::require(
        refused.code == Some(2) && refused.stderr.contains("missing: spawn.uid and spawn.gid"),
        format!(
            "without a worker uid it is refused, naming it: {:?} {}",
            refused.code, refused.stderr
        ),
    );
    if !o.passed {
        return o;
    }
    if checked.stderr.contains("Landlock — this kernel has none") {
        return Outcome::note("skipped: this kernel has no Landlock");
    }
    let o = Outcome::require(
        checked.code == Some(0),
        format!("the whole set loads: {:?} {}", checked.code, checked.stderr),
    );
    if !o.passed {
        return o;
    }
    let _d = s.daemon(&hx);
    tryo!(s.login(&hx));
    let n = s.start("read-exec");
    let g = tryo!(s.gates(&hx, 1));
    let rule = g[0]["rendered"]["rule"].clone();
    let ok = s.cli(
        &hx,
        &[
            "--delegation-approve",
            g[0]["gate"].as_str().unwrap_or_default(),
        ],
    );
    let run = tryo!(s.ended("read-exec", n, Duration::from_secs(30)));
    let o = Outcome::require(
        rule == "exec_holding_delegated_data",
        format!("a command from work holding the person's data asks them: {g:?}"),
    )
    .and(|| {
        Outcome::require(
            ok.code == Some(0),
            format!("approve: {} {}", ok.stdout, ok.stderr),
        )
    })
    .and(|| completed(&run))
    .and(|| {
        use std::os::unix::fs::MetadataExt;
        let made = std::fs::metadata(open.path().join("work").join("made-by-exec"));
        Outcome::require(
            made.as_ref().is_ok_and(|m| m.uid() == 65534),
            format!("the command runs as the worker uid: {made:?}"),
        )
    });
    if !o.passed {
        return o;
    }
    // The worker cannot open the vault: a command copying the seal key or
    // the store copies nothing.
    let n = s.start("peek");
    let _ = tryo!(s.ended("peek", n, Duration::from_secs(30)));
    let work = open.path().join("work");
    let stolen: Vec<String> = std::fs::read_dir(&work)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.starts_with("stolen"))
        .collect();
    let ran = s
        .events("exec.run")
        .iter()
        .filter(|e| e["cmd"] == "cp")
        .count();
    Outcome::require(
        stolen.is_empty() && ran == 2,
        format!(
            "a command copies neither the key nor the vault: {stolen:?}\n{}",
            tail(&s.log_text())
        ),
    )
}
