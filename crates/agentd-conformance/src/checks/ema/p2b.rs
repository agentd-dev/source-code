// SPDX-License-Identifier: AGPL-3.0-only
//! RFC 0044 §7.P2b, the second part of P2's prerequisites: the taint
//! engine, the person's consent gate held to the person alone, the external
//! signer's own policy, and §6.4's per-principal sealing of what a gate
//! showed. The same deployment as the rest of the family ([`Setup`]).

use super::*;

impl Setup {
    /// What `--delegation-list` says, as JSON.
    fn listed(&self, h: &Harness) -> Value {
        serde_json::from_str(&self.cli(h, &["--delegation-list"]).stdout).unwrap_or(Value::Null)
    }

    /// The `tools/call`s of `tool` the server was sent.
    fn calls_of(&self, tool: &str) -> usize {
        self.xaa
            .mcp_calls("tools/call")
            .iter()
            .filter(|c| c["body"]["params"]["name"] == tool)
            .count()
    }

    /// What `--delegation-audit` says, as JSON.
    fn audited(&self, h: &Harness) -> Value {
        serde_json::from_str(&self.cli(h, &["--delegation-audit"]).stdout).unwrap_or(Value::Null)
    }
}

/// The open gate's id and listing.
fn open_gate(h: &Harness, s: &Setup) -> Result<(String, Value), String> {
    let id = wait_gate(h, s)?;
    let doc = s.listed(h);
    let g = doc["gates"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|g| g["gate"] == id.as_str())
        .cloned()
        .ok_or_else(|| format!("gate {id} is not listed: {doc}"))?;
    Ok((id, g))
}

/// A workflow that reads from the delegated server, then POSTs to the mock
/// IdP's `/exfil` under agentd's own credential.
fn leak(idp: &str) -> String {
    format!(
        "  - name: leak\n    steps:\n      start: {{kind: manual}}\n      read: {{kind: mcp.tool, depends_on: [start], server: linear, tool: whoami, args: {{}}}}\n      post: {{kind: http, depends_on: [read], method: POST, url: \"{idp}/exfil\", allow_private: true, expect: [404], json: {{got: \"{{{{steps.read.output}}}}\"}}}}\n      done: {{kind: finish, depends_on: [post]}}\n"
    )
}

/// The read is no egress, the write `sensitive` and not `egress`: only taint
/// can make the write ask.
const TAGS: &str = "{whoami: [], create_issue: [sensitive]}";

fn asked(s: &Setup) -> bool {
    s.events("delegation.consent")
        .iter()
        .any(|e| e["decision"] == "asked")
}

/// A delegated server's results are outside text (a Linear issue body is
/// anyone's to write): after a delegated read the run is tainted and holds
/// the person's data, so its delegated `sensitive` write asks, and so does
/// its own `http` POST under agentd's credential — while the same write in
/// a run that read nothing does not ask.
pub(super) fn taint_gates_write_and_egress(h: &Harness) -> Outcome {
    let s = Setup::new(h, json!({}), &Opts::default());
    s.reconfigure(&Opts {
        tag_map: Some(TAGS),
        extra: leak(&s.xaa.idp),
        ..Opts::default()
    });
    let _d = s.daemon(h);
    tryo!(s.login(h));
    // Clean: the write alone asks nothing.
    let clean = tryo!(s.run("write"));
    let o = completed(&clean)
        .and(|| {
            Outcome::require(
                s.calls_of("create_issue") == 1,
                "the clean write reaches the server",
            )
        })
        .and(|| {
            Outcome::require(
                !asked(&s),
                "an untainted delegated sensitive write asks nothing",
            )
        });
    if !o.passed {
        return o;
    }
    // After the read: the write asks, as tainted work.
    let n = s.start("read-write");
    let (gate, g) = tryo!(open_gate(h, &s));
    let r = &g["rendered"];
    let o = Outcome::require(
        r["rule"] == "delegated_call_from_tainted_work"
            && r["tainted"] == true
            && r["holds_delegated_data"] == true
            && r["taint"]
                .as_array()
                .is_some_and(|t| t.iter().any(|x| x == "delegated:linear")),
        format!("the write's gate is the tainted rule's, saying why: {g}"),
    )
    .and(|| {
        Outcome::require(
            g["text"].as_str().is_some_and(|t| {
                t.starts_with("agentd security gate:") && t.contains("Tainted: yes")
            }),
            format!("the person reads agentd's text: {g}"),
        )
    });
    if !o.passed {
        return o;
    }
    let _ = s.cli(h, &["--delegation-deny", &gate]);
    let denied = tryo!(s.ended("read-write", n, Duration::from_secs(30)));
    let o = failed_with(&denied, "policy_denied").and(|| {
        Outcome::require(
            s.calls_of("create_issue") == 1,
            "the denied write is never made",
        )
    });
    if !o.passed {
        return o;
    }
    // After the read: agentd's own egress asks too.
    let n = s.start("leak");
    let (gate, g) = tryo!(open_gate(h, &s));
    let _ = s.cli(h, &["--delegation-deny", &gate]);
    let ended = tryo!(s.ended("leak", n, Duration::from_secs(30)));
    let posted = s
        .xaa
        .record()
        .into_iter()
        .filter(|r| r["path"] == "/exfil")
        .count();
    Outcome::require(
        g["rendered"]["tool"] == "http"
            && g["rendered"]["rule"] == "egress_from_tainted_work_holding_delegated_data",
        format!("the POST's gate is the egress rule's: {g}"),
    )
    .and(|| failed_with(&ended, "policy_denied"))
    .and(|| Outcome::require(posted == 0, "nothing leaves"))
}

/// A delegated server the operator vouches for (`auth.trusted_output`):
/// its results hold the person's data and taint nothing, so after a read the
/// write and the POST both go ahead unasked.
pub(super) fn trusted_output_taints_nothing(h: &Harness) -> Outcome {
    let s = Setup::new(h, json!({}), &Opts::default());
    s.reconfigure(&Opts {
        tag_map: Some(TAGS),
        trusted_output: true,
        extra: leak(&s.xaa.idp),
        ..Opts::default()
    });
    let _d = s.daemon(h);
    tryo!(s.login(h));
    let write = tryo!(s.run("read-write"));
    let post = tryo!(s.run("leak"));
    completed(&write)
        .and(|| completed(&post))
        .and(|| Outcome::require(!asked(&s), "trusted results taint nothing, so nothing asks"))
}

/// A workflow that reads the agent-mode server `server`, then makes the
/// delegated `sensitive` write.
fn read_then_write(server: &str) -> String {
    format!(
        "  - name: {server}-write\n    steps:\n      start: {{kind: manual}}\n      read: {{kind: mcp.tool, depends_on: [start], server: {server}, tool: mock.ops, args: {{}}}}\n      call: {{kind: mcp.tool, depends_on: [read], server: linear, tool: create_issue, args: {{title: \"From agentd\"}}}}\n      done: {{kind: finish, depends_on: [call]}}\n"
    )
}

/// An MCP server that declares no tags counts as `untrusted_input`, at
/// runtime as in the Rule of Two: after a run reads it, the delegated
/// `sensitive` write asks under the tainted rule, naming the server, and
/// denied it is never made. The same read from the same server tagged
/// `trusted` leaves the run clean, and the write goes ahead unasked.
pub(super) fn untagged_read_gates_the_delegated_write(h: &Harness) -> Outcome {
    let s = Setup::new(h, json!({}), &Opts::default());
    let docs = h.spawn_mock_mcp(&s.tmp.path().join("docs.addr"), "mock://res", false);
    let at = format!("{}/mcp", docs.endpoint());
    s.reconfigure(&Opts {
        tag_map: Some(TAGS),
        mcp_extra: format!(
            "    - {{name: docs, endpoint: \"{at}\"}}\n    - {{name: vouched, endpoint: \"{at}\", tags: {{\"*\": [trusted]}}}}\n"
        ),
        extra: read_then_write("docs") + &read_then_write("vouched"),
        ..Opts::default()
    });
    let _d = s.daemon(h);
    tryo!(s.login(h));
    let vouched = tryo!(s.run("vouched-write"));
    let o = completed(&vouched)
        .and(|| {
            Outcome::require(
                s.calls_of("create_issue") == 1,
                "the write after the vouched-for read reaches the server",
            )
        })
        .and(|| Outcome::require(!asked(&s), "a trusted server's read taints nothing"));
    if !o.passed {
        return o;
    }
    let n = s.start("docs-write");
    let (gate, g) = tryo!(open_gate(h, &s));
    let r = &g["rendered"];
    let o = Outcome::require(
        r["rule"] == "delegated_call_from_tainted_work"
            && r["tainted"] == true
            && r["taint"].as_array().is_some_and(|t| {
                t.iter()
                    .any(|x| x == "a result of the untagged server docs")
            }),
        format!("the write's gate is the tainted rule's, naming the untagged server: {g}"),
    );
    if !o.passed {
        return o;
    }
    let _ = s.cli(h, &["--delegation-deny", &gate]);
    let denied = tryo!(s.ended("docs-write", n, Duration::from_secs(30)));
    failed_with(&denied, "policy_denied").and(|| {
        Outcome::require(
            s.calls_of("create_issue") == 1,
            "the denied write is never made",
        )
    })
}

/// A consent gate is the person's alone (§5.10 rules 1–2, T7, T8): a
/// message on the task that waits behind it — the person's own, which an
/// upstream agent holding their token could send as well — is refused and
/// decides nothing; and an operator who is not the person is refused the
/// answer, with the gate's own nonce and hash. Each refusal is recorded,
/// and the call is never made.
pub(super) fn consent_is_the_persons_alone(h: &Harness) -> Outcome {
    let o = Opts {
        tags: "[sensitive, egress]",
        consent_timeout: Some("6s"),
        ..Opts::default()
    };
    let s = Setup::new(h, json!({}), &o);
    let (gate, g) = {
        let _d = s.daemon(h);
        tryo!(s.login(h));
        let started = s.op("workflow.run", json!({"workflow": "write"}));
        let task = started["result"]["task"]["id"]
            .as_str()
            .unwrap_or_default()
            .to_string();
        let (gate, g) = tryo!(open_gate(h, &s));
        let answered = unix_rpc(
            &s.sock,
            "SendMessage",
            crate::checks::util::text_params("approve", Some(&task), true),
        );
        let still = s.listed(h);
        let o = Outcome::require(
            !task.is_empty() && answered.get("error").is_some(),
            format!("a message on the gate's task is refused: {answered} ({started})"),
        )
        .and(|| {
            Outcome::require(
                still["gates"]
                    .as_array()
                    .is_some_and(|a| a.iter().any(|x| x["gate"] == gate.as_str())),
                format!("the gate is still open: {still}"),
            )
        })
        .and(|| {
            Outcome::require(
                s.events("delegation.consent.refused")
                    .iter()
                    .any(|e| e["why"] == "a_message_is_no_answer"),
                "the refused message is recorded",
            )
        });
        if !o.passed {
            return o;
        }
        (gate, g)
    };
    // The same gate after a restart under a rule that names another uid:
    // this process is the bare operator now, holding the gate's binding.
    s.reconfigure(&Opts {
        person_uid: Some(uid().wrapping_add(1)),
        ..o
    });
    let _d = s.daemon(h);
    let answered = s.op(
        "delegation.approve",
        json!({"gate": gate, "nonce": g["nonce"], "args_hash": g["args_hash"]}),
    );
    let ended = tryo!(s.ended("write", 0, Duration::from_secs(30)));
    Outcome::require(
        answered.get("error").is_some(),
        format!("an operator who is not the person is refused: {answered}"),
    )
    .and(|| {
        Outcome::require(
            s.events("delegation.consent.refused")
                .iter()
                .any(|e| e["why"] == "not_the_person" && e["gate"] == gate.as_str()),
            "the operator's attempt is recorded",
        )
    })
    .and(|| failed_with(&ended, "policy_denied"))
    .and(|| Outcome::require(s.calls_of("create_issue") == 0, "the call is never made"))
}

/// One request to the reference signer: its answer.
fn signer_request(socket: &Path, req: &Value) -> Value {
    let Ok(mut c) = UnixStream::connect(socket) else {
        return Value::Null;
    };
    c.set_read_timeout(Some(Duration::from_secs(10))).ok();
    let body = req.to_string();
    let mut frame = (body.len() as u32).to_be_bytes().to_vec();
    frame.extend_from_slice(body.as_bytes());
    if c.write_all(&frame).is_err() {
        return Value::Null;
    }
    let mut len = [0u8; 4];
    if c.read_exact(&mut len).is_err() {
        return Value::Null;
    }
    let mut out = vec![0u8; u32::from_be_bytes(len) as usize];
    let _ = c.read_exact(&mut out);
    serde_json::from_slice(&out).unwrap_or(Value::Null)
}

/// The external signer signs only what a client assertion is, for the
/// audiences it is configured with — the reference signer's own policy,
/// which `docs/external-signer.md` asks of every signer: a payload of
/// another shape is `denied`, and so is the authorization server's assertion
/// when its key signs for another audience; the first redemption — the
/// catalogue listing an enrolment makes — then fails `signer_unavailable`,
/// and nothing reaches the authorization server.
pub(super) fn signer_signs_only_assertions(h: &Harness) -> Outcome {
    let s = Setup::new(
        h,
        json!({}),
        &Opts {
            signer_uid: Some(uid()),
            ras_audience: Some("https://elsewhere.example/token"),
            ..Opts::default()
        },
    );
    let socket = s.tmp.path().join("signer.sock");
    let shapeless = signer_request(
        &socket,
        &json!({"op": "sign", "key": "idp", "alg": "ES256",
                "data": b64url(b"transfer 1000 to mallory")}),
    );
    let _d = s.daemon(h);
    let enrolled = tryo!(s.login(h));
    let asked = s.signer_record();
    let redeemed = s.xaa.grants("ras", JWT_BEARER).len();
    Outcome::require(
        shapeless["error"]["code"] == "denied",
        format!("a payload that is no client assertion is denied: {shapeless}"),
    )
    .and(|| {
        Outcome::require(
            enrolled.contains("not listed") && enrolled.contains("signer_unavailable"),
            format!("the catalogue's redemption fails signer_unavailable: {enrolled}"),
        )
    })
    .and(|| Outcome::require(redeemed == 0, "nothing reaches the authorization server"))
    .and(|| {
        Outcome::require(
            asked
                .iter()
                .any(|r| r["key"] == "ras" && r["denied"].is_string()),
            format!(
                "the signer refused the assertion for an audience it does not sign for: {asked:?}"
            ),
        )
    })
}

/// §6.4: what a person's consent gate showed is kept in the audit chain only
/// sealed under their personal key — never in the clear — and reads back
/// whole until they are forgotten; `--delegation-forget` destroys the key,
/// after which the record is unreadable, and the chain, record and all,
/// still verifies at the next start.
pub(super) fn forgotten_content_unreadable(h: &Harness) -> Outcome {
    let o = Opts {
        tags: "[sensitive, egress]",
        audit_chain: true,
        ..Opts::default()
    };
    let s = Setup::new(h, json!({}), &o);
    let chain = s.tmp.path().join("audit/audit.log");
    let shown = |doc: &Value| doc["records"][0]["target"]["personal"].clone();
    let (before, forgot, after) = {
        let _d = s.daemon(h);
        tryo!(s.login(h));
        let n = s.start("write");
        let gate = tryo!(wait_gate(h, &s));
        let _ = s.cli(h, &["--delegation-approve", &gate]);
        let run = tryo!(s.ended("write", n, Duration::from_secs(30)));
        let o = completed(&run);
        if !o.passed {
            return o;
        }
        let before = s.audited(h);
        let forgot = s.cli(h, &["--delegation-forget", "operator:alice"]);
        (before, forgot, s.audited(h))
    };
    let text = std::fs::read_to_string(&chain).unwrap_or_default();
    let sealed = text
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .find(|r| r["action"] == "delegation.consent")
        .map(|r| r["target"]["personal"].clone())
        .unwrap_or(Value::Null);
    // The next start verifies the chain, the sealed record in it.
    let _d = s.daemon(h);
    let later = s.audited(h);
    Outcome::require(
        sealed["sealed"].is_string() && !text.contains("From agentd"),
        format!("the chain holds the gate's content only sealed: {sealed}"),
    )
    .and(|| {
        Outcome::require(
            shown(&before)["rendered"]["args"]["title"] == "From agentd",
            format!("the person's record reads back whole: {before}"),
        )
    })
    .and(|| {
        Outcome::require(
            forgot.code == Some(0),
            format!("--delegation-forget: {} {}", forgot.stdout, forgot.stderr),
        )
    })
    .and(|| {
        Outcome::require(
            shown(&after) == "unreadable" && shown(&later) == "unreadable",
            format!("forgotten, it is unreadable, now and after a restart: {after} {later}"),
        )
    })
    .and(|| {
        Outcome::require(
            !s.events("audit.chain.open").is_empty(),
            "the chain verified at the next start",
        )
    })
}

/// A message carries what its sender holds (§5.13): a run that read the
/// person's data — tainted, as every delegated result is — sends a
/// `message` step, and an agent step's model a `message.send`, into a new
/// conversation, whose delegated `sensitive` write then asks under the
/// tainted rule; the same write from a conversation the person opened
/// themselves goes ahead unasked. Denied, neither carried write is made.
pub(super) fn a_message_carries_what_its_sender_read(h: &Harness) -> Outcome {
    let tmp = h.tempdir();
    // After any tool result the model is done; a turn that reads the mark
    // writes, and the agent step sends the mark on.
    let playbook = json!({"match": [
        {"when_contains": "\"role\":\"tool\"", "content": "done"},
        {"when_contains": "TELL-MARK", "tool_calls": [{"name": "create_issue", "arguments": {"title": "From agentd"}}]},
        {"when_contains": "SEND-MARK", "tool_calls": [{"name": "message.send", "arguments": {"to": "new", "text": "TELL-MARK"}}]}
    ], "turns": [{"content": "done"}]});
    let pb = crate::checks::util::write_file(&tmp, "playbook.json", &playbook.to_string());
    let llm = h.mock_llm(&format!("file:{pb}"));
    let flows = "  - name: tell\n    steps:\n      start: {kind: manual}\n      read: {kind: mcp.tool, depends_on: [start], server: linear, tool: whoami, args: {}}\n      say: {kind: message, depends_on: [read], to: new, text: TELL-MARK}\n      done: {kind: finish, depends_on: [say]}\n  - name: tell-tool\n    steps:\n      start: {kind: manual}\n      read: {kind: mcp.tool, depends_on: [start], server: linear, tool: whoami, args: {}}\n      send: {kind: agent, depends_on: [read], instruction: SEND-MARK}\n      done: {kind: finish, depends_on: [send]}\n";
    let s = Setup::new(h, json!({}), &Opts::default());
    s.reconfigure(&Opts {
        tag_map: Some(TAGS),
        extra: flows.to_string(),
        ..Opts::default()
    });
    let cfg = std::fs::read_to_string(&s.cfg).expect("read").replace(
        "intelligence: {endpoints: \"mock:final\", model: m}",
        &format!("intelligence: {{endpoints: \"{}\", model: m}}", llm.uri),
    );
    std::fs::write(&s.cfg, cfg).expect("write");
    let _d = s.daemon(h);
    tryo!(s.login(h));
    // The person's own conversation is clean: its write asks nothing.
    let params = json!({"message": {"role": "ROLE_USER", "messageId": b64url(&rand16()),
        "parts": [{"text": "TELL-MARK"}]}, "configuration": {"returnImmediately": true}});
    let sent = unix_rpc(&s.sock, "SendMessage", params);
    let deadline = Instant::now() + Duration::from_secs(30);
    while s.calls_of("create_issue") == 0 && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(100));
    }
    let o = Outcome::require(
        sent.get("error").is_none() && s.calls_of("create_issue") == 1 && !asked(&s),
        format!(
            "the person's own conversation writes unasked: {sent}\n{}",
            tail(&s.log_text())
        ),
    );
    if !o.passed {
        return o;
    }
    for wf in ["tell", "tell-tool"] {
        let run = tryo!(s.run(wf));
        let o = completed(&run);
        if !o.passed {
            return o;
        }
        let (gate, g) = tryo!(open_gate(h, &s));
        let r = &g["rendered"];
        let o = Outcome::require(
            r["rule"] == "delegated_call_from_tainted_work"
                && r["run"].is_null()
                && r["context"].is_string()
                && r["tainted"] == true
                && r["holds_delegated_data"] == true,
            format!(
                "{wf}: the conversation the message started is tainted and holds the person's data: {g}"
            ),
        );
        if !o.passed {
            return o;
        }
        let denied = s.cli(h, &["--delegation-deny", &gate]);
        if denied.code != Some(0) {
            return Outcome::fail(format!(
                "{wf}: --delegation-deny: {} {}",
                denied.stdout, denied.stderr
            ));
        }
    }
    Outcome::require(
        s.calls_of("create_issue") == 1,
        "neither carried write is made",
    )
}

/// A run that holds the person's delegated data keeps nothing in the step
/// cache, which is every run's and keyed by no principal: the second run of
/// a cached delegated read reads the server again, admitted as every read
/// is, and says why it was not kept.
pub(super) fn a_cached_step_keeps_no_delegated_data(h: &Harness) -> Outcome {
    let cached = "  - name: cached\n    steps:\n      start: {kind: manual}\n      read: {kind: mcp.tool, depends_on: [start], server: linear, tool: whoami, args: {}, cache: {ttl: 1h}}\n      done: {kind: finish, depends_on: [read]}\n";
    let s = Setup::new(
        h,
        json!({}),
        &Opts {
            extra: cached.to_string(),
            ..Opts::default()
        },
    );
    let _d = s.daemon(h);
    tryo!(s.login(h));
    let before = s.calls_of("whoami");
    let first = tryo!(s.run("cached"));
    let second = tryo!(s.run("cached"));
    let reads = s.calls_of("whoami") - before;
    completed(&first)
        .and(|| completed(&second))
        .and(|| {
            Outcome::require(
                reads == 2 && s.events("step.cache_hit").is_empty(),
                format!("each run reads the server itself: {reads} read(s)"),
            )
        })
        .and(|| {
            Outcome::require(
                s.events("step.cache.not_kept").len() == 2,
                "each run says its output was not kept",
            )
        })
}

/// Forgetting a person closes their open consent gates first (§6.4): the
/// call waiting on one is denied at once, its record sealed under the key
/// the erasure then destroys — never left to be sealed later under a key
/// made for them again. Under the external signer, which holds the keys:
/// the person's key is opened off the loop when their gate opens, so the
/// approval before is sealed and reads back whole, and afterwards every
/// record of theirs is unreadable.
pub(super) fn forgetting_closes_the_persons_gates(h: &Harness) -> Outcome {
    let s = Setup::new(
        h,
        json!({}),
        &Opts {
            tags: "[sensitive, egress]",
            audit_chain: true,
            signer_uid: Some(uid()),
            consent_timeout: Some("120s"),
            ..Opts::default()
        },
    );
    let _d = s.daemon(h);
    tryo!(s.login(h));
    let n = s.start("write");
    let gate = tryo!(wait_gate(h, &s));
    let approved = s.cli(h, &["--delegation-approve", &gate]);
    let first = tryo!(s.ended("write", n, Duration::from_secs(30)));
    let before = s.audited(h);
    let o = Outcome::require(
        approved.code == Some(0),
        format!(
            "the approval is sealed under the key the signer holds: {} {}",
            approved.stdout, approved.stderr
        ),
    )
    .and(|| completed(&first))
    .and(|| {
        Outcome::require(
            before["records"][0]["target"]["personal"]["rendered"]["args"]["title"]
                == "From agentd",
            format!("the person's record reads back whole: {before}"),
        )
    });
    if !o.passed {
        return o;
    }
    let n = s.start("write");
    let open = tryo!(wait_gate(h, &s));
    let at = Instant::now();
    let forgot = s.cli(h, &["--delegation-forget", "operator:alice"]);
    let ended = tryo!(s.ended("write", n, Duration::from_secs(20)));
    let after = s.audited(h);
    let records = after["records"].as_array().cloned().unwrap_or_default();
    Outcome::require(
        forgot.code == Some(0),
        format!("--delegation-forget: {} {}", forgot.stdout, forgot.stderr),
    )
    .and(|| failed_with(&ended, "policy_denied"))
    .and(|| {
        Outcome::require(
            ended["err"]
                .as_str()
                .is_some_and(|e| e.contains("forgotten"))
                && at.elapsed() < Duration::from_secs(60),
            format!("the waiting call is denied at the erasure, not at its deadline: {ended}"),
        )
    })
    .and(|| {
        Outcome::require(
            s.events("delegation.consent")
                .iter()
                .any(|e| e["gate"] == open.as_str() && e["via"] == "forgotten"),
            "the closed gate is recorded as the erasure's",
        )
    })
    .and(|| {
        Outcome::require(
            records.len() == 2
                && records
                    .iter()
                    .all(|r| r["target"]["personal"] == "unreadable"),
            format!("both records are unreadable now: {after}"),
        )
    })
}
