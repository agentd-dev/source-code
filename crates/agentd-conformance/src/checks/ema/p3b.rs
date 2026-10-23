// SPDX-License-Identifier: AGPL-3.0-only
//! RFC 0044 §7.P3b, P3 built, part 2: unattended work. Firings of an
//! `act_for` workflow's schedule acting for the per-person operator alice
//! with nobody there — each approved by her on her own device through the
//! mock IdP's OpenID CIBA (`via: ciba`), or with her grant (`via:
//! standing`) — held to the definition she consented to and to her
//! template's `unattended_tools`; a step waiting for her to enrol again
//! (`on_revoked: ask`); a step-up's sign-in held to the `max_age` asked by
//! its `auth_time`; and DPoP on the authorization-server and MCP legs where
//! both advertise it. The deployment is the family's own [`Setup`].

use super::*;

/// The CIBA grant's `grant_type`.
const CIBA: &str = "urn:openid:params:grant-type:ciba";

/// A workflow that fires every two seconds, one run at a time, acting for
/// alice `via` as it says, its one step `tool` at the delegated server.
fn nightly(via: &str, tool: &str, args: &str) -> String {
    format!(
        "  - name: nightly\n    act_for: {{principal: {{source: operator, id: alice}}, via: {via}}}\n    concurrency: {{max_runs: 1, on_overflow: drop}}\n    steps:\n      start: {{kind: schedule, every: 2s}}\n      call: {{kind: mcp.tool, depends_on: [start], server: linear, tool: {tool}, args: {args}}}\n      done: {{kind: finish, depends_on: [call]}}\n"
    )
}

/// A deployment whose template lets alice's grant act `via: ciba`, at an
/// IdP with CIBA (`per_day` a day), [`nightly`] firing `via: ciba`.
fn ciba_opts(per_day: u32) -> Opts {
    Opts {
        template_extra: ", unattended: ciba",
        provider_extra: Box::leak(
            format!("      ciba: {{enabled: true, per_day: {per_day}}}\n").into_boxed_str(),
        ),
        extra: nightly("ciba", "whoami", "{}"),
        ..Opts::default()
    }
}

/// A deployment whose template lets alice's grant act `via: standing`,
/// [`nightly`] calling `tool` with `args`.
fn standing_opts(tool: &str, args: &str) -> Opts {
    Opts {
        template_extra: ", unattended: standing",
        extra: nightly("standing", tool, args),
        ..Opts::default()
    }
}

impl Setup {
    /// The next run of `nightly` to end after alice has enrolled, once she
    /// has.
    fn firing(&self, within: Duration) -> Result<Value, String> {
        let n = self.runs_done("nightly").len();
        self.ended("nightly", n, within)
    }

    /// The requests to the mock IdP's backchannel authentication endpoint.
    fn ciba_requests(&self) -> Vec<Value> {
        self.xaa
            .record()
            .into_iter()
            .filter(|r| r["at"] == "idp" && r["path"] == "/bc/authorize")
            .collect()
    }

    /// Wait up to `within` for `cond`.
    fn until(&self, what: &str, within: Duration, cond: &dyn Fn() -> bool) -> Result<(), String> {
        let deadline = Instant::now() + within;
        while !cond() {
            if Instant::now() > deadline {
                return Err(format!("{what}:\n{}", tail(&self.log_text())));
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        Ok(())
    }

    /// Send the daemon SIGHUP, and wait for its reload to be `event`.
    fn reload(&self, event: &str) -> Result<(), String> {
        let pid = self
            .log_events()
            .iter()
            .find_map(|e| e["pid"].as_i64())
            .unwrap_or_default() as i32;
        let n = self.events(event).len();
        // SAFETY: a signal to the daemon this check started.
        unsafe {
            libc::kill(pid, libc::SIGHUP);
        }
        self.until(&format!("no {event}"), Duration::from_secs(20), &|| {
            self.events(event).len() > n
        })
    }
}

/// The run of `nightly` whose id ends as `binding` (a CIBA binding message)
/// names it.
fn run_of(binding: &str, run: &Value) -> bool {
    run["run"]
        .as_str()
        .is_some_and(|r| binding.contains(&r[r.len().saturating_sub(8)..]))
}

/// A firing acting for alice `via: ciba` asks her by CIBA once she holds a
/// grant: the request names her by an ID token fresh from a refresh, asks
/// for `openid` alone with agentd's binding message naming the workflow
/// and how long it waits; the poll waits through `authorization_pending`
/// and `slow_down` — 5 s longer from then on — and her approval's ID token
/// is the subject of the firing's exchange; the run completes, the mint is
/// recorded as unattended, and the refresh token the IdP issued with the
/// approval is revoked at the run's end.
pub(super) fn a_ciba_firing_acts_on_its_persons_approval(h: &Harness) -> Outcome {
    let s = Setup::new(
        h,
        json!({"ciba_pending": 1, "ciba_slow_down": 1, "ciba_refresh_token": true}),
        &ciba_opts(10),
    );
    let _d = s.daemon(h);
    tryo!(s.login(h));
    let run = tryo!(s.firing(Duration::from_secs(60)));
    let record = s.xaa.record();
    let bc = record
        .iter()
        .position(|r| r["path"] == "/bc/authorize")
        .map(|i| (i, record[i].clone()));
    let Some((at, ask)) = bc else {
        return Outcome::fail(format!("no CIBA request: {}", tail(&s.log_text())));
    };
    let body = &ask["body"];
    let hint = claims(body["id_token_hint"].as_str().unwrap_or_default());
    let refreshed_before = record[..at]
        .iter()
        .rev()
        .find(|r| r["at"] == "idp" && r["path"] == "/token")
        .is_some_and(|r| r["body"]["grant_type"] == "refresh_token");
    let auth_req = |r: &Value| r["body"]["grant_type"] == CIBA;
    let polls: Vec<&Value> = record.iter().filter(|r| auth_req(r)).collect();
    let gap = |a: &Value, b: &Value| {
        b["t"]
            .as_u64()
            .unwrap_or(0)
            .saturating_sub(a["t"].as_u64().unwrap_or(0))
    };
    let exchange = record
        .iter()
        .skip(at)
        .find(|r| r["path"] == "/token" && r["body"]["grant_type"] == EXCHANGE)
        .cloned()
        .unwrap_or(Value::Null);
    let subject = claims(
        exchange["body"]["subject_token"]
            .as_str()
            .unwrap_or_default(),
    );
    let minted = s.events("delegation.minted");
    let started = s
        .events("run.start")
        .into_iter()
        .find(|e| Some(e["run"].as_str()) == Some(run["run"].as_str()))
        .unwrap_or(Value::Null);
    // The approval's refresh token goes once the run has ended.
    let revoked = s.until(
        "the approval's refresh token revoked",
        Duration::from_secs(20),
        &|| {
            s.xaa.record().iter().any(|r| {
                r["path"] == "/revoke"
                    && r["body"]["token"]
                        .as_str()
                        .is_some_and(|t| t.starts_with("ciba-rt-"))
            })
        },
    );
    completed(&run)
        .and(|| {
            Outcome::require(
                hint["sub"] == "00u1alice"
                    && body.get("login_hint").is_none()
                    && body["scope"] == "openid"
                    && body["requested_expiry"] == "300"
                    && body["client_assertion"].is_string()
                    && body["binding_message"]
                        .as_str()
                        .is_some_and(|m| m.starts_with("agentd") && m.contains("nightly")),
                format!("the request: {body}"),
            )
        })
        .and(|| {
            Outcome::require(
                refreshed_before,
                "the hint is an ID token fresh from a refresh made just before the request",
            )
        })
        .and(|| {
            Outcome::require(
                polls.len() >= 3 && gap(polls[1], polls[2]) >= 5_000,
                format!(
                    "the poll waits through pending and slow_down, 5 s longer after slow_down: {} polls, {} ms after slow_down",
                    polls.len(),
                    polls.get(1).zip(polls.get(2)).map_or(0, |(a, b)| gap(a, b))
                ),
            )
        })
        .and(|| {
            Outcome::require(
                exchange["body"]["subject_token_type"] == "urn:ietf:params:oauth:token-type:id_token"
                    && subject["sub"] == "00u1alice"
                    && subject["auth_time"].as_u64() >= hint["iat"].as_u64(),
                format!("the exchange presents the approval's ID token: {exchange}"),
            )
        })
        .and(|| {
            Outcome::require(
                minted
                    .iter()
                    .any(|m| m["unattended"] == true && m["via"] == "ciba"),
                format!("the mint is recorded as an unattended firing's: {minted:?}"),
            )
        })
        .and(|| {
            Outcome::require(
                started["unattended"] == "ciba" && started["acting_for"] == "operator:alice",
                format!("the run acts for alice, unattended: {started}"),
            )
        })
        .and(|| Outcome::require(revoked.is_ok(), revoked.err().unwrap_or_default()))
}

/// A firing alice denies on her device, or whose request expires
/// unanswered — said by the IdP well before agentd would see it run out,
/// or seen pending past its expiry — fails `ciba_denied` and is exchanged
/// for nothing; and past `ciba.per_day` requests she is asked no more that
/// day.
pub(super) fn a_ciba_firing_denied_or_expired_fails_closed(h: &Harness) -> Outcome {
    // A poll answers as `ciba` says once one has answered pending. The
    // denied request lives a minute, so her answer at the second poll is
    // never beaten by its expiry however slow the host; only the one meant
    // to run out unanswered lives 3 s.
    let s = Setup::new(
        h,
        json!({"ciba": "deny", "ciba_expires_in": 60}),
        &ciba_opts(3),
    );
    let _d = s.daemon(h);
    tryo!(s.login(h));
    let exchanges = || s.xaa.grants("idp", EXCHANGE).len();
    let before = exchanges();
    let denied = tryo!(s.firing(Duration::from_secs(40)));
    // The IdP says expired_token at its second poll, of a request that
    // would run out here only after a minute.
    s.xaa
        .control(json!({"ciba": "expire", "ciba_expires_in": 60}));
    let expired = tryo!(s.firing(Duration::from_secs(20)));
    s.xaa
        .control(json!({"ciba": "pending", "ciba_expires_in": 3}));
    let ran_out = tryo!(s.firing(Duration::from_secs(40)));
    let asked = s.ciba_requests().len();
    let capped = tryo!(s.firing(Duration::from_secs(40)));
    failed_with(&denied, "ciba_denied")
        .and(|| {
            Outcome::require(
                denied["err"]
                    .as_str()
                    .is_some_and(|e| e.contains("denied the request")),
                format!("denied on her device: {denied}"),
            )
        })
        .and(|| failed_with(&expired, "ciba_denied"))
        .and(|| failed_with(&ran_out, "ciba_denied"))
        .and(|| {
            Outcome::require(
                [&expired, &ran_out]
                    .iter()
                    .all(|r| r["err"].as_str().is_some_and(|e| e.contains("expired"))),
                format!("expired unanswered: {expired} {ran_out}"),
            )
        })
        .and(|| {
            Outcome::require(
                expired["err"]
                    .as_str()
                    .is_some_and(|e| e.contains("before operator:alice answered")),
                format!("the IdP's expired_token ends the wait: {expired}"),
            )
        })
        .and(|| {
            Outcome::require(
                exchanges() == before,
                "a firing nobody approved is exchanged for nothing",
            )
        })
        .and(|| failed_with(&capped, "budget_exhausted"))
        .and(|| {
            Outcome::require(
                asked == 3 && s.ciba_requests().len() == 3,
                format!(
                    "three requests a day, and no fourth: {asked} then {}",
                    s.ciba_requests().len()
                ),
            )
        })
}

/// An IdP that returns no ID token on refresh is sent the name alice
/// enrolled with (her `preferred_username`, kept sealed) as the request's
/// `login_hint`; and an approval whose ID token names another person than
/// her grant's fails the firing closed (`binding_mismatch`), exchanged for
/// nothing.
pub(super) fn a_ciba_hint_falls_back_and_binds_its_person(h: &Harness) -> Outcome {
    let s = Setup::new(h, json!({"refresh_id_token": false}), &ciba_opts(10));
    let _d = s.daemon(h);
    tryo!(s.login(h));
    let run = tryo!(s.firing(Duration::from_secs(40)));
    let ask = s.ciba_requests().last().cloned().unwrap_or(Value::Null);
    s.xaa.control(json!({"ciba_sub": "00u9mallory"}));
    let exchanges = s.xaa.grants("idp", EXCHANGE).len();
    let other = tryo!(s.firing(Duration::from_secs(40)));
    let stored = s.store_bytes();
    completed(&run)
        .and(|| {
            Outcome::require(
                ask["body"]["login_hint"] == "alice@acme.example"
                    && ask["body"].get("id_token_hint").is_none(),
                format!("the login hint she enrolled with: {ask}"),
            )
        })
        .and(|| {
            Outcome::require(
                !stored
                    .iter()
                    .any(|(_, b)| String::from_utf8_lossy(b).contains("alice@acme.example")),
                "the hint is kept sealed, never in the clear",
            )
        })
        .and(|| failed_with(&other, "binding_mismatch"))
        .and(|| {
            Outcome::require(
                s.xaa.grants("idp", EXCHANGE).len() == exchanges,
                "an approval by someone else is exchanged for nothing",
            )
        })
}

/// `nightly` acting for alice `via: ciba`: a call, a wait for the signal
/// `go`, and a second call.
const TWICE: &str = "  - name: nightly\n    act_for: {principal: {source: operator, id: alice}, via: ciba}\n    concurrency: {max_runs: 1, on_overflow: drop}\n    steps:\n      start: {kind: schedule, every: 2s}\n      one: {kind: mcp.tool, depends_on: [start], server: linear, tool: whoami, args: {}}\n      w: {kind: wait, depends_on: [one], on: signal, signal: go}\n      two: {kind: mcp.tool, depends_on: [w], server: linear, tool: whoami, args: {}}\n      done: {kind: finish, depends_on: [two]}\n";

impl Setup {
    /// The binding message of the first CIBA request, once one is sent.
    fn first_binding(&self) -> Result<String, String> {
        self.until("a CIBA request", Duration::from_secs(30), &|| {
            !self.ciba_requests().is_empty()
        })?;
        Ok(self.ciba_requests()[0]["body"]["binding_message"]
            .as_str()
            .unwrap_or_default()
            .to_string())
    }

    /// The CIBA requests sent for the run `binding` names.
    fn asked_for(&self, binding: &str) -> Vec<String> {
        let run = binding
            .split(' ')
            .skip_while(|w| *w != "run")
            .nth(1)
            .unwrap_or("?")
            .to_string();
        self.ciba_requests()
            .iter()
            .filter_map(|r| r["body"]["binding_message"].as_str())
            .filter(|m| m.split(' ').skip_while(|w| *w != "run").nth(1) == Some(&run))
            .map(str::to_string)
            .collect()
    }

    /// Kill the daemon as a crash would: nothing it holds in memory is
    /// revoked or written.
    fn crash(&self) {
        let pid = self
            .log_events()
            .iter()
            .find_map(|e| e["pid"].as_i64())
            .unwrap_or_default() as i32;
        if pid > 0 {
            // SAFETY: a signal to the daemon this check started.
            unsafe {
                libc::kill(pid, libc::SIGKILL);
            }
        }
        std::thread::sleep(Duration::from_millis(300));
    }

    /// The store's files that keep an approval's refresh token.
    fn approvals_kept(&self) -> Vec<(PathBuf, Vec<u8>)> {
        self.store_bytes()
            .into_iter()
            .filter(|(p, _)| p.components().any(|c| c.as_os_str() == "approval"))
            .collect()
    }

    /// The revocations at the mock IdP of a refresh token a CIBA approval
    /// came with, each with its place in the record.
    fn ciba_revokes(&self) -> Vec<(usize, Value)> {
        self.xaa
            .record()
            .into_iter()
            .enumerate()
            .filter(|(_, r)| {
                r["path"] == "/revoke"
                    && r["body"]["token"]
                        .as_str()
                        .is_some_and(|t| t.starts_with("ciba-rt-"))
            })
            .collect()
    }

    /// How the runs' records the store keeps say their firings asked, per
    /// grant: `once`, `again` or `refused`.
    fn asked_recorded(&self) -> Vec<String> {
        self.store_bytes()
            .into_iter()
            .filter_map(|(_, b)| serde_json::from_slice::<Value>(&b).ok())
            .filter_map(|v| v["state"]["unattended"]["asked"].as_object().cloned())
            .flat_map(|m| {
                m.into_values()
                    .filter_map(|v| v.as_str().map(str::to_string))
            })
            .collect()
    }
}

/// Whether the requests `asked` for one run are a first and one more that
/// says it is asked again.
fn asked_twice(asked: &[String]) -> bool {
    asked.len() == 2
        && !asked[0].split(' ').any(|w| w == "again")
        && asked[1].split(' ').any(|w| w == "again")
}

/// A restart loses a `via: ciba` firing's request, or the approval it
/// gave, and the firing asks alice exactly once more, the request's
/// binding message saying `again` (RFC 0044 §7.P3c): a step that waited on
/// her answer when the daemon stopped waits on a new request — approved,
/// the run completes; denied, it fails `ciba_denied` — and one whose
/// approval a restart lost between two calls asks at its next call, and
/// completes once she approves again.
pub(super) fn a_ciba_firing_asks_once_more_after_a_restart(h: &Harness) -> Outcome {
    let waiting = |answer: &str| -> Result<(Value, Vec<String>), String> {
        let s = Setup::new(h, json!({"ciba": "pending"}), &ciba_opts(10));
        let d = s.daemon(h);
        s.login(h)?;
        let binding = s.first_binding()?;
        std::thread::sleep(Duration::from_millis(500));
        drop(d);
        s.xaa.control(json!({"ciba": answer}));
        let _d = s.daemon(h);
        let run = s.ended("nightly", 0, Duration::from_secs(40))?;
        if !run_of(&binding, &run) {
            return Err(format!("another run ended first: {run} ({binding})"));
        }
        Ok((run, s.asked_for(&binding)))
    };
    let (approved, asked) = tryo!(waiting("approve"));
    let o = completed(&approved).and(|| {
        Outcome::require(
            asked_twice(&asked),
            format!("the waiting firing asks once more, saying again: {asked:?}"),
        )
    });
    if !o.passed {
        return o;
    }
    let (denied, asked) = tryo!(waiting("deny"));
    let o = failed_with(&denied, "ciba_denied").and(|| {
        Outcome::require(
            asked_twice(&asked),
            format!("denied, the once-more request was the last: {asked:?}"),
        )
    });
    if !o.passed {
        return o;
    }
    // Approved, then a restart between two calls.
    let s = Setup::new(
        h,
        json!({}),
        &Opts {
            extra: TWICE.to_string(),
            ..ciba_opts(10)
        },
    );
    let d = s.daemon(h);
    tryo!(s.login(h));
    tryo!(s.until(
        "the first call made",
        Duration::from_secs(40),
        &|| s.made("whoami") >= 1
    ));
    let binding = tryo!(s.first_binding());
    std::thread::sleep(Duration::from_millis(500));
    drop(d);
    let _d = s.daemon(h);
    std::thread::sleep(Duration::from_millis(500));
    let fired = s.op("workflow.signal", json!({"name": "go"}));
    if fired.get("error").is_some() {
        return Outcome::fail(format!("signal: {fired}"));
    }
    let run = tryo!(s.ended("nightly", 0, Duration::from_secs(40)));
    let asked = s.asked_for(&binding);
    let again = s
        .events("delegation.ciba")
        .into_iter()
        .any(|e| e["decision"] == "asked_again");
    completed(&run)
        .and(|| {
            Outcome::require(
                run_of(&binding, &run) && s.made("whoami") == 2,
                format!("the approved firing's second call is made after the restart: {run}"),
            )
        })
        .and(|| {
            Outcome::require(
                asked_twice(&asked) && again,
                format!("its second call asks once more, saying again: {asked:?}"),
            )
        })
}

/// A second restart while the request a restart made a `via: ciba` firing
/// send is still out fails the firing closed: the run's record says it
/// asked once more, so the step fails `ciba_denied` and no third request
/// is ever sent.
pub(super) fn a_ciba_firing_fails_closed_on_a_second_restart(h: &Harness) -> Outcome {
    let s = Setup::new(h, json!({"ciba": "pending"}), &ciba_opts(10));
    let d = s.daemon(h);
    tryo!(s.login(h));
    let binding = tryo!(s.first_binding());
    std::thread::sleep(Duration::from_millis(500));
    drop(d);
    let d = s.daemon(h);
    tryo!(s.until(
        "the request asked once more",
        Duration::from_secs(30),
        &|| s.asked_for(&binding).len() >= 2
    ));
    std::thread::sleep(Duration::from_millis(500));
    drop(d);
    let _d = s.daemon(h);
    let run = tryo!(s.ended("nightly", 0, Duration::from_secs(30)));
    std::thread::sleep(Duration::from_secs(2));
    let asked = s.asked_for(&binding);
    failed_with(&run, "ciba_denied")
        .and(|| {
            Outcome::require(
                run_of(&binding, &run)
                    && run["err"].as_str().is_some_and(|e| e.contains("once more")),
                format!("the firing fails closed on the second restart: {run} ({binding})"),
            )
        })
        .and(|| {
            Outcome::require(
                asked_twice(&asked),
                format!("never a third request: {asked:?}"),
            )
        })
}

/// The refresh token alice's IdP issues with her CIBA approval, though
/// none was asked for, is kept sealed in the store — never a byte of it in
/// the clear — while the run lasts. A crash leaves it there; the next
/// start revokes it at the IdP and forgets it, before the request the
/// restart makes the firing send — however long the revocation takes — so
/// the store keeps none when that request is sent; and the refresh token
/// issued with that approval is revoked at the run's end, the store
/// keeping none.
pub(super) fn a_ciba_refresh_token_is_revoked_after_a_crash(h: &Harness) -> Outcome {
    let s = Setup::new(
        h,
        json!({"ciba_refresh_token": true}),
        &Opts {
            extra: TWICE.to_string(),
            ..ciba_opts(10)
        },
    );
    let d = s.daemon(h);
    tryo!(s.login(h));
    tryo!(s.until(
        "the first call made",
        Duration::from_secs(40),
        &|| s.made("whoami") >= 1
    ));
    let kept = s.approvals_kept();
    let clear = s
        .store_bytes()
        .iter()
        .any(|(_, b)| String::from_utf8_lossy(b).contains("ciba-rt-"));
    s.crash();
    drop(d);
    let after_crash = s.ciba_revokes().len();
    let mark = s.xaa.record().len();
    // The new request waits on alice until the store is looked at.
    s.xaa
        .control(json!({"slow_revoke_ms": 3000, "ciba": "pending"}));
    let _d = s.daemon(h);
    let fired = s.op("workflow.signal", json!({"name": "go"}));
    if fired.get("error").is_some() {
        return Outcome::fail(format!("signal: {fired}"));
    }
    let sent = s.until(
        "the request the restart makes the firing send",
        Duration::from_secs(30),
        &|| {
            s.xaa
                .record()
                .iter()
                .skip(mark)
                .any(|r| r["path"] == "/bc/authorize")
        },
    );
    let kept_when_sent = s.approvals_kept().len();
    s.xaa.control(json!({"ciba": "approve"}));
    let swept = s.until(
        "the approval's refresh token revoked at the next start",
        Duration::from_secs(20),
        &|| s.ciba_revokes().iter().any(|(i, _)| *i >= mark),
    );
    let run = tryo!(s.ended("nightly", 0, Duration::from_secs(40)));
    let record = s.xaa.record();
    let revoke = s.ciba_revokes().into_iter().find(|(i, _)| *i >= mark);
    let asked = record
        .iter()
        .enumerate()
        .skip(mark)
        .find(|(_, r)| r["path"] == "/bc/authorize");
    let ended = s.until(
        "the second approval's refresh token revoked at the run's end",
        Duration::from_secs(20),
        &|| s.ciba_revokes().len() >= 2 && s.approvals_kept().is_empty(),
    );
    let t = |r: &Value| r["t"].as_u64().unwrap_or(0);
    completed(&run)
        .and(|| {
            Outcome::require(
                kept.len() == 1 && !clear,
                format!(
                    "kept sealed while the run lasts: {} kept, in the clear: {clear}",
                    kept.len()
                ),
            )
        })
        .and(|| {
            Outcome::require(
                after_crash == 0,
                "nothing is revoked by a crash",
            )
        })
        .and(|| Outcome::require(swept.is_ok(), swept.err().unwrap_or_default()))
        .and(|| Outcome::require(sent.is_ok(), sent.err().unwrap_or_default()))
        .and(|| {
            Outcome::require(
                kept_when_sent == 0,
                format!(
                    "the start forgets the record it revoked, before the restart's request: {kept_when_sent} kept when it was sent"
                ),
            )
        })
        .and(|| match (&revoke, asked) {
            (Some((ri, r)), Some((ai, a))) => Outcome::require(
                ri < &ai && t(a) >= t(r) + 2_500,
                format!(
                    "revoked before the restart's request is sent, its revocation answered first: revoke #{ri} at {}, request #{ai} at {}",
                    t(r),
                    t(a)
                ),
            ),
            _ => Outcome::fail(format!(
                "a revocation and a request after the restart: {revoke:?} {asked:?}"
            )),
        })
        .and(|| Outcome::require(ended.is_ok(), ended.err().unwrap_or_default()))
}

/// Under `key_custody: external`, the refresh token alice's CIBA approval
/// came with, which a crash left in the store, stays there while the
/// signer is down at the next start — not revoked, not forgotten, and no
/// request is sent — and once the signer is back a later pass revokes it
/// at the IdP and forgets it, though no CIBA request comes to make it.
pub(super) fn a_ciba_refresh_token_is_revoked_once_the_signer_is_back(h: &Harness) -> Outcome {
    let s = Setup::new(
        h,
        json!({"ciba_refresh_token": true}),
        &Opts {
            signer_uid: Some(uid()),
            extra: TWICE.to_string(),
            ..ciba_opts(10)
        },
    );
    let d = s.daemon(h);
    tryo!(s.login(h));
    tryo!(s.until(
        "the first call made",
        Duration::from_secs(40),
        &|| s.made("whoami") >= 1
    ));
    let kept = s.approvals_kept().len();
    s.crash();
    drop(d);
    s.stop_signer();
    let mark = s.xaa.record().len();
    let since = |path: &str| {
        s.xaa
            .record()
            .iter()
            .skip(mark)
            .filter(|r| r["path"] == path)
            .count()
    };
    let _d = s.daemon(h);
    let held = s.until(
        "the start's pass kept from finishing by the signer",
        Duration::from_secs(20),
        &|| !s.events("delegation.ciba.unrevoked").is_empty(),
    );
    std::thread::sleep(Duration::from_secs(1));
    let (revoked_while_down, left) = (since("/revoke"), s.approvals_kept().len());
    s.start_signer(h);
    let swept = s.until(
        "the refresh token revoked once the signer is back",
        Duration::from_secs(30),
        &|| s.ciba_revokes().iter().any(|(i, _)| *i >= mark) && s.approvals_kept().is_empty(),
    );
    Outcome::require(kept == 1, format!("kept while the run lasts: {kept}"))
        .and(|| Outcome::require(held.is_ok(), held.err().unwrap_or_default()))
        .and(|| {
            Outcome::require(
                revoked_while_down == 0 && left == 1,
                format!(
                    "left for a later pass while the signer is down: {revoked_while_down} revoked, {left} kept"
                ),
            )
        })
        .and(|| Outcome::require(swept.is_ok(), swept.err().unwrap_or_default()))
        .and(|| {
            Outcome::require(
                since("/bc/authorize") == 0,
                "no CIBA request made the pass run",
            )
        })
}

/// `nightly` acting for alice `via: ciba`: a call the run goes on past
/// when it fails, a wait for the signal `go`, and a second call.
const PAST_A_REFUSAL: &str = "  - name: nightly\n    act_for: {principal: {source: operator, id: alice}, via: ciba}\n    concurrency: {max_runs: 1, on_overflow: drop}\n    steps:\n      start: {kind: schedule, every: 2s}\n      one: {kind: mcp.tool, depends_on: [start], server: linear, tool: whoami, args: {}, on_error: continue}\n      w: {kind: wait, depends_on: [one], on: signal, signal: go}\n      two: {kind: mcp.tool, depends_on: [w], server: linear, tool: whoami, args: {}}\n      done: {kind: finish, depends_on: [two]}\n";

/// A firing whose request alice denied on her device, or whose wait she
/// cancelled with `--delegation-deny`, asks her no more at that grant, a
/// restart included (RFC 0044 §7.P3c): its run goes on past the refused
/// call, its record says `refused` before the restart, and the call it
/// makes after the restart fails `subject_expired`, no second request
/// sent.
pub(super) fn a_ciba_refusal_holds_across_a_restart(h: &Harness) -> Outcome {
    let refused = |cancel: bool| -> Result<(Value, Vec<String>, Vec<String>), String> {
        let s = Setup::new(
            h,
            json!({"ciba": if cancel { "pending" } else { "deny" }}),
            &Opts {
                extra: PAST_A_REFUSAL.to_string(),
                ..ciba_opts(10)
            },
        );
        let d = s.daemon(h);
        s.login(h)?;
        let binding = s.first_binding()?;
        if cancel {
            let gate = s.gates(h, 1)?;
            let id = gate[0]["gate"].as_str().unwrap_or_default().to_string();
            let r = s.cli(h, &["--delegation-deny", &id]);
            if r.code != Some(0) {
                return Err(format!("--delegation-deny: {} {}", r.stdout, r.stderr));
            }
        }
        s.until(
            "the run's record saying the firing was refused",
            Duration::from_secs(20),
            &|| s.asked_recorded().iter().any(|a| a == "refused"),
        )?;
        let recorded = s.asked_recorded();
        std::thread::sleep(Duration::from_millis(500));
        drop(d);
        let _d = s.daemon(h);
        std::thread::sleep(Duration::from_millis(500));
        let fired = s.op("workflow.signal", json!({"name": "go"}));
        if fired.get("error").is_some() {
            return Err(format!("signal: {fired}"));
        }
        let run = s.ended("nightly", 0, Duration::from_secs(30))?;
        if !run_of(&binding, &run) {
            return Err(format!("another run ended first: {run} ({binding})"));
        }
        std::thread::sleep(Duration::from_secs(1));
        Ok((run, s.asked_for(&binding), recorded))
    };
    for (cancel, how) in [(false, "denied on her device"), (true, "cancelled")] {
        let (run, asked, recorded) = tryo!(refused(cancel));
        let o = failed_with(&run, "subject_expired")
            .and(|| {
                Outcome::require(
                    recorded == ["refused"],
                    format!("{how}: the record says refused before the restart: {recorded:?}"),
                )
            })
            .and(|| {
                Outcome::require(
                    asked.len() == 1,
                    format!("{how}: asked once, and not again after the restart: {asked:?}"),
                )
            });
        if !o.passed {
            return o;
        }
    }
    Outcome::pass()
}

/// The approval's ID token alice's CIBA approval gave a `via: ciba`
/// firing is the subject of its same-domain exchange (`grant:
/// token_exchange`, M1b): every exchange the firing makes presents that
/// very token, typed as an ID token, and none the grant's access token —
/// the token the MCP server is sent is hers. An authorization server that
/// takes no ID token as the subject (`invalid_request`) fails the call
/// `config`, and nothing falls back to the grant's token; under `profile:
/// okta`, whose on-behalf-of exchange takes an access token alone, the
/// call is refused before she is asked.
pub(super) fn a_ciba_exchange_binds_to_the_approval(h: &Harness) -> Outcome {
    let m1b = |provider_extra: &'static str| Opts {
        exchange: true,
        template: Some("linear: {scopes: [\"issues:read\"]}"),
        provider_extra: Box::leak(
            format!("      ciba: {{enabled: true, per_day: 10}}\n{provider_extra}")
                .into_boxed_str(),
        ),
        ..ciba_opts(10)
    };
    let s = Setup::new(h, json!({}), &m1b(""));
    // The MCP server's own authorization server is the person's IdP.
    s.xaa
        .control(json!({"prm_authorization_servers": [s.xaa.idp]}));
    let _d = s.daemon(h);
    tryo!(s.login(h));
    let binding = tryo!(s.first_binding());
    let run = tryo!(s.ended("nightly", 0, Duration::from_secs(40)));
    let record = s.xaa.record();
    let first_ask = record
        .iter()
        .position(|r| r["path"] == "/bc/authorize")
        .unwrap_or(record.len());
    let approval = record
        .iter()
        .find_map(|r| r["issued_id_token"].as_str())
        .unwrap_or_default()
        .to_string();
    let exchanges: Vec<&Value> = record[first_ask..]
        .iter()
        .filter(|r| r["at"] == "idp" && r["body"]["grant_type"] == EXCHANGE)
        .collect();
    // The first firing's, before the next firing asks.
    let next_ask = record[first_ask + 1..]
        .iter()
        .position(|r| r["path"] == "/bc/authorize")
        .map_or(record.len(), |i| first_ask + 1 + i);
    let of_this: Vec<&Value> = record[first_ask..next_ask]
        .iter()
        .filter(|r| r["at"] == "idp" && r["body"]["grant_type"] == EXCHANGE)
        .collect();
    let sent = s
        .xaa
        .mcp_calls("tools/call")
        .iter()
        .filter_map(|c| c["authorization"].as_str().map(str::to_string))
        .next()
        .unwrap_or_default();
    let at = claims(sent.strip_prefix("Bearer ").unwrap_or_default());
    let minted = s.events("delegation.minted");
    let o = completed(&run)
        .and(|| {
            Outcome::require(
                run_of(&binding, &run) && !approval.is_empty() && !of_this.is_empty(),
                format!("the firing's exchange after her approval: {of_this:?}"),
            )
        })
        .and(|| {
            Outcome::require(
                exchanges.iter().all(|r| {
                    r["body"]["subject_token_type"] == "urn:ietf:params:oauth:token-type:id_token"
                }) && of_this
                    .iter()
                    .all(|r| r["body"]["subject_token"] == approval.as_str()),
                format!(
                    "every exchange of a firing presents its approval's ID token, never the grant's access token: {exchanges:?}"
                ),
            )
        })
        .and(|| {
            Outcome::require(
                at["aud"] == "api://issues" && at["sub"] == "00u1alice",
                format!("the MCP server is sent the exchanged token, hers: {sent}"),
            )
        })
        .and(|| {
            Outcome::require(
                minted
                    .iter()
                    .any(|m| m["subject"] == "approval" && m["via"] == "ciba"),
                format!("the exchange is recorded as of the approval: {minted:?}"),
            )
        });
    if !o.passed {
        return o;
    }
    // An authorization server that takes no ID token as the subject.
    s.xaa.control(json!({"m1b_id_token_subject": false}));
    let n = s.runs_done("nightly").len();
    let before = s.xaa.grants("idp", EXCHANGE).len();
    let refused = tryo!(s.ended("nightly", n, Duration::from_secs(40)));
    let tried: Vec<Value> = s
        .xaa
        .grants("idp", EXCHANGE)
        .into_iter()
        .skip(before)
        .collect();
    let o = failed_with(&refused, "config")
        .and(|| {
            Outcome::require(
                refused["err"]
                    .as_str()
                    .is_some_and(|e| e.contains("did not take the CIBA approval's ID token")),
                format!("the refusal says the server takes no ID token subject: {refused}"),
            )
        })
        .and(|| {
            Outcome::require(
                tried.iter().all(|r| {
                    r["body"]["subject_token_type"] == "urn:ietf:params:oauth:token-type:id_token"
                }),
                format!("nothing falls back to the grant's token: {tried:?}"),
            )
        });
    if !o.passed {
        return o;
    }
    // Okta's on-behalf-of exchange takes an access token alone. (The mock
    // serves its keys where its metadata says, not at Okta's path.)
    let s = Setup::new(h, json!({}), &m1b(""));
    s.reconfigure(&m1b(Box::leak(
        format!(
            "      profile: okta\n      jwks_uri: \"{}/keys\"\n",
            s.xaa.idp
        )
        .into_boxed_str(),
    )));
    s.xaa
        .control(json!({"prm_authorization_servers": [s.xaa.idp]}));
    let _d = s.daemon(h);
    tryo!(s.login(h));
    let okta = tryo!(s.firing(Duration::from_secs(40)));
    failed_with(&okta, "config")
        .and(|| {
            Outcome::require(
                okta["err"]
                    .as_str()
                    .is_some_and(|e| e.contains("Okta's on-behalf-of exchange")),
                format!("refused, naming Okta's exchange: {okta}"),
            )
        })
        .and(|| {
            Outcome::require(
                s.ciba_requests().is_empty(),
                "she is never asked for an approval the exchange could not use",
            )
        })
}

/// `act_for` beside a webhook start, or a signal start, is refused at
/// validate time (`act_for_trigger_not_allowed`): whoever can fire one
/// could make the person's phone buzz at every firing.
pub(super) fn act_for_on_a_webhook_is_refused(h: &Harness) -> Outcome {
    let mut out = Outcome::pass();
    for start in [
        "{kind: webhook, path: /nightly, auth: {none: true}}",
        "{kind: signal, name: go}",
    ] {
        let wf = format!(
            "  - name: hooked\n    act_for: {{principal: {{source: operator, id: alice}}, via: ciba}}\n    steps:\n      start: {start}\n      call: {{kind: mcp.tool, depends_on: [start], server: linear, tool: whoami, args: {{}}}}\n      done: {{kind: finish, depends_on: [call]}}\n"
        );
        let s = Setup::new(
            h,
            json!({}),
            &Opts {
                extra: wf,
                ..ciba_opts(10)
            },
        );
        let r = s.cli(h, &["--validate-config"]);
        let said = format!("{}{}", r.stdout, r.stderr);
        out = out.and(|| {
            Outcome::require(
                r.code == Some(2) && said.contains("act_for_trigger_not_allowed"),
                format!("act_for beside {start}: {:?} {said}", r.code),
            )
        });
    }
    let s = Setup::new(h, json!({}), &ciba_opts(10));
    let r = s.cli(h, &["--validate-config"]);
    out.and(|| {
        Outcome::require(
            r.code == Some(0),
            format!("beside a schedule it loads: {} {}", r.stdout, r.stderr),
        )
    })
}

/// `unattended: standing` at a `profile: okta` IdP is refused at validate
/// time without `allow_against_idp_guidance` (`standing_against_idp_guidance`),
/// and loads with it, warned about.
pub(super) fn standing_under_okta_needs_the_override(h: &Harness) -> Outcome {
    let okta = |extra: &'static str| Opts {
        template_extra: extra,
        provider_extra: "      profile: okta\n",
        extra: nightly("standing", "whoami", "{}"),
        ..Opts::default()
    };
    let s = Setup::new(h, json!({}), &okta(", unattended: standing"));
    let refused = s.cli(h, &["--validate-config"]);
    let s = Setup::new(
        h,
        json!({}),
        &okta(", unattended: standing, allow_against_idp_guidance: true"),
    );
    let allowed = s.cli(h, &["--validate-config"]);
    let said = |r: &crate::harness::RunResult| format!("{}{}", r.stdout, r.stderr);
    Outcome::require(
        refused.code == Some(2) && said(&refused).contains("standing_against_idp_guidance"),
        format!("standing under okta: {:?} {}", refused.code, said(&refused)),
    )
    .and(|| {
        Outcome::require(
            allowed.code == Some(0) && said(&allowed).contains("against Okta's guidance"),
            format!("with the override: {:?} {}", allowed.code, said(&allowed)),
        )
    })
}

/// A firing acts only for the definition alice consented to when she
/// signed in: `via: standing`, it runs with her grant, its mint recorded
/// as unattended; once a reload changes the workflow its firings start
/// nothing (`start.unattended.suspended`, `unattended_suspended`) and mint
/// nothing; once she signs in again they run.
pub(super) fn a_changed_digest_suspends_unattended_use(h: &Harness) -> Outcome {
    let s = Setup::new(h, json!({}), &standing_opts("whoami", "{}"));
    let d = s.daemon(h);
    tryo!(s.login(h));
    // A restart drops the token the enrolment pooled: the firing mints.
    drop(d);
    let _d = s.daemon(h);
    let first = tryo!(s.firing(Duration::from_secs(30)));
    let minted = s.events("delegation.minted");
    s.edit(|c| {
        c.replace(
            "  - name: nightly\n",
            "  - name: nightly\n    description: \"now does something else\"\n",
        )
    });
    tryo!(s.reload("config.reloaded"));
    let suspended_before = s.events("start.unattended.suspended").len();
    tryo!(s.until("a suspended firing", Duration::from_secs(20), &|| {
        s.events("start.unattended.suspended").len() > suspended_before + 1
    }));
    let suspended = s.events("start.unattended.suspended");
    let exchanges = s.xaa.grants("idp", EXCHANGE).len();
    let runs = s.runs_done("nightly").len();
    std::thread::sleep(Duration::from_secs(3));
    let quiet =
        s.xaa.grants("idp", EXCHANGE).len() == exchanges && s.runs_done("nightly").len() == runs;
    tryo!(s.login(h));
    let again = tryo!(s.firing(Duration::from_secs(30)));
    completed(&first)
        .and(|| {
            Outcome::require(
                minted
                    .iter()
                    .any(|m| m["unattended"] == true && m["via"] == "standing"),
                format!("a standing firing's mint is recorded as unattended: {minted:?}"),
            )
        })
        .and(|| {
            Outcome::require(
                suspended
                    .last()
                    .is_some_and(|e| e["code"] == "unattended_suspended"),
                format!("the changed workflow's firings are suspended: {suspended:?}"),
            )
        })
        .and(|| Outcome::require(quiet, "a suspended firing starts no run and mints nothing"))
        .and(|| completed(&again))
}

/// A firing sends nothing off the box with alice's grant unless her
/// template lists the tool: its `create_issue`, tagged `egress`, is refused
/// unasked (`policy_denied`) and never made; listed in `unattended_tools`,
/// it is asked about — the work is unattended, so tainted — and made once
/// she approves.
pub(super) fn unattended_egress_is_denied_by_default(h: &Harness) -> Outcome {
    let tags = "{whoami: [], create_issue: [egress], list_issues: [untrusted_input]}";
    let s = Setup::new(
        h,
        json!({}),
        &Opts {
            tag_map: Some(tags),
            ..standing_opts("create_issue", "{title: nightly}")
        },
    );
    let _d = s.daemon(h);
    tryo!(s.login(h));
    let refused = tryo!(s.firing(Duration::from_secs(30)));
    let made = s.made("create_issue");
    let asked = s
        .events("delegation.consent")
        .iter()
        .filter(|e| e["decision"] == "asked")
        .count();
    s.edit(|c| {
        c.replace(
            ", unattended: standing}",
            ", unattended: standing, unattended_tools: [linear.create_issue]}",
        )
    });
    tryo!(s.reload("config.reloaded"));
    let gate = tryo!(s.gates(h, 1));
    let id = gate[0]["gate"].as_str().unwrap_or_default().to_string();
    let ok = s.cli(h, &["--delegation-approve", &id]);
    tryo!(s.until("an approved firing", Duration::from_secs(30), &|| {
        s.runs_done("nightly")
            .iter()
            .any(|r| r["status"] == "completed")
    }));
    let approved = s
        .runs_done("nightly")
        .into_iter()
        .find(|r| r["status"] == "completed")
        .unwrap_or(Value::Null);
    failed_with(&refused, "policy_denied")
        .and(|| {
            Outcome::require(
                refused["err"]
                    .as_str()
                    .is_some_and(|e| e.contains("unattended_tools")),
                format!("refused as unlisted: {refused}"),
            )
        })
        .and(|| {
            Outcome::require(
                made == 0 && asked == 0,
                format!("refused unasked and never made: {made} made, {asked} asked"),
            )
        })
        .and(|| {
            Outcome::require(
                ok.code == Some(0),
                format!("--delegation-approve: {} {}", ok.stdout, ok.stderr),
            )
        })
        .and(|| completed(&approved))
        .and(|| Outcome::require(s.made("create_issue") == 1, "made once approved"))
}

/// A workflow step whose grant was revoked, at a server that says
/// `on_revoked: ask`, waits for its person rather than failing: the wait is
/// said and recorded, and once alice enrols again the step is made again
/// and the run completes — with no message to it. A call held so before
/// any rule was consulted meets them all when it is made again: her
/// `create_issue`, tagged `egress`, asks her consent first, and is made
/// only once she approves.
pub(super) fn on_revoked_ask_waits_for_re_enrolment(h: &Harness) -> Outcome {
    let s = Setup::new(
        h,
        json!({}),
        &Opts {
            server_extra: "        on_revoked: ask\n",
            ..Opts::default()
        },
    );
    let d = s.daemon(h);
    tryo!(s.login(h));
    let first = tryo!(s.run("who"));
    let revoked = s.cli(h, &["--delegation-revoke", "operator:alice"]);
    let n = s.start("who");
    tryo!(
        s.until("a re-enrolment wait", Duration::from_secs(20), &|| {
            s.events("delegation.reenrol")
                .iter()
                .any(|e| e["decision"] == "asked")
        })
    );
    std::thread::sleep(Duration::from_secs(2));
    let waiting = s.runs_done("who").len() == n;
    let listed = s.listing(h);
    tryo!(s.login(h));
    let run = tryo!(s.ended("who", n, Duration::from_secs(30)));
    let reenrol = s.events("delegation.reenrol");
    let o = completed(&first)
        .and(|| {
            Outcome::require(
                revoked.code == Some(0),
                format!("--delegation-revoke: {} {}", revoked.stdout, revoked.stderr),
            )
        })
        .and(|| Outcome::require(waiting, "the step waits rather than failing"))
        .and(|| {
            Outcome::require(
                listed["gates"].as_array().is_some_and(|g| {
                    g.iter()
                        .any(|x| x["tool"].as_str().is_some_and(|t| t.ends_with("whoami")))
                }),
                format!("the wait is among alice's gates: {listed}"),
            )
        })
        .and(|| completed(&run))
        .and(|| {
            Outcome::require(
                reenrol.iter().any(|e| e["decision"] == "signed_in"),
                format!("signed in again, the step is made again: {reenrol:?}"),
            )
        });
    if !o.passed {
        return o;
    }
    drop(d);
    // Every tool sends data off the box: revoked, a write waits; signed in
    // again, it is asked about.
    let s = Setup::new(
        h,
        json!({}),
        &Opts {
            server_extra: "        on_revoked: ask\n",
            tags: "[untrusted_input, egress]",
            ..Opts::default()
        },
    );
    let _d = s.daemon(h);
    tryo!(s.login(h));
    let again = s.cli(h, &["--delegation-revoke", "operator:alice"]);
    let w = s.start("write");
    tryo!(
        s.until("a re-enrolment wait", Duration::from_secs(20), &|| {
            s.events("delegation.reenrol")
                .iter()
                .any(|e| e["decision"] == "asked")
        })
    );
    tryo!(s.login(h));
    tryo!(
        s.until("the write asked about", Duration::from_secs(20), &|| {
            s.events("delegation.consent")
                .iter()
                .any(|e| e["decision"] == "asked")
        })
    );
    let unmade = s.made("create_issue");
    let gate = tryo!(s.gates(h, 1));
    let id = gate[0]["gate"].as_str().unwrap_or_default().to_string();
    let ok = s.cli(h, &["--delegation-approve", &id]);
    let wrote = tryo!(s.ended("write", w, Duration::from_secs(30)));
    Outcome::require(
        again.code == Some(0) && ok.code == Some(0),
        format!(
            "revoked, then approved: {} {} {} {}",
            again.stdout, again.stderr, ok.stdout, ok.stderr
        ),
    )
    .and(|| {
        Outcome::require(
            unmade == 0,
            "a write held before the rules is not made before its consent",
        )
    })
    .and(|| completed(&wrote))
    .and(|| Outcome::require(s.made("create_issue") == 1, "made once approved"))
}

/// A step-up's sign-in is held to the IdP's `max_age` by its ID token's
/// `auth_time`: a sign-in of an older authentication is refused, and so is
/// one whose ID token carries no `auth_time` — the call fails `step_up` —
/// and a fresh one is the call's subject, the ID-JAG carrying its
/// `auth_time`.
pub(super) fn a_step_up_is_held_to_its_max_age(h: &Harness) -> Outcome {
    let s = Setup::new(h, json!({}), &Opts::default());
    let d = s.daemon(h);
    tryo!(s.login(h));
    s.xaa
        .control(json!({"step_up_max_age": 2, "signin_auth_time": 100}));
    std::thread::sleep(Duration::from_secs(3));
    // A restart drops the token the enrolment pooled: the next call mints.
    drop(d);
    let _d = s.daemon(h);
    let old = tryo!(s.run("who"));
    s.xaa.control(json!({"signin_auth_time": false}));
    let none = tryo!(s.run("who"));
    s.xaa.control(json!({"signin_auth_time": null}));
    let fresh = tryo!(s.run("who"));
    let last = s
        .xaa
        .grants("idp", EXCHANGE)
        .last()
        .cloned()
        .unwrap_or(Value::Null);
    let at = claims(last["body"]["subject_token"].as_str().unwrap_or_default())["auth_time"]
        .as_u64()
        .unwrap_or(0);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    failed_with(&old, "step_up")
        .and(|| {
            Outcome::require(
                old["err"].as_str().is_some_and(|e| {
                    e.contains("s ago, and the IdP asked for a sign-in within 2s")
                }),
                format!("a sign-in older than max_age: {old}"),
            )
        })
        .and(|| failed_with(&none, "step_up"))
        .and(|| {
            Outcome::require(
                none["err"]
                    .as_str()
                    .is_some_and(|e| e.contains("no auth_time")),
                format!("a sign-in with no auth_time: {none}"),
            )
        })
        .and(|| completed(&fresh))
        .and(|| {
            Outcome::require(
                last["body"]["subject_token_type"] == "urn:ietf:params:oauth:token-type:id_token"
                    && at + 30 >= now,
                format!("the retry presents the fresh sign-in: {last}"),
            )
        })
}

/// DPoP on the authorization-server and MCP legs where both advertise ES256
/// proofs: the redemption carries a proof — sent again with the nonce the
/// server asks for — and every MCP request presents the token as `DPoP`
/// with a proof for that request, the one the MCP server refused for want
/// of its nonce sent again at once with it; where they do not, neither leg
/// has one and the token is a `Bearer`; and a server that takes bound
/// tokens alone, behind an authorization server that does not advertise
/// DPoP, is refused before anything is minted for it.
pub(super) fn dpop_where_advertised_and_absent_otherwise(h: &Harness) -> Outcome {
    let legs = |knobs: Value| -> Result<(Value, Vec<Value>, Vec<Value>, String), String> {
        let s = Setup::new(h, knobs, &Opts::default());
        let _d = s.daemon(h);
        let enrolled = s.login(h)?;
        let run = s.run("who")?;
        let record = s.xaa.record();
        let redeem: Vec<Value> = record
            .iter()
            .filter(|r| r["at"] == "ras" && r["path"] == "/token")
            .cloned()
            .collect();
        let mcp: Vec<Value> = record
            .iter()
            .filter(|r| r["at"] == "mcp" && r["method"] == "POST")
            .cloned()
            .collect();
        Ok((run, redeem, mcp, enrolled))
    };
    let (bound, redeem, mcp, enrolled) = tryo!(legs(
        json!({"dpop": true, "prm_dpop": true, "dpop_nonce": "n-1", "mcp_dpop_nonce": "m-1"})
    ));
    let proofs = |r: &Vec<Value>| r.iter().all(|x| x["dpop"].is_string());
    let nonce_of = |r: &Value| claims(r["dpop"].as_str().unwrap_or_default())["nonce"].clone();
    let o = completed(&bound)
        .and(|| {
            Outcome::require(
                redeem.len() >= 2
                    && proofs(&redeem)
                    && claims(redeem[0]["dpop"].as_str().unwrap_or_default())
                        .get("nonce")
                        .is_none()
                    && claims(
                        redeem
                            .last()
                            .and_then(|r| r["dpop"].as_str())
                            .unwrap_or_default(),
                    )["nonce"]
                        == "n-1",
                format!("the redemption carries a proof, again with the nonce: {redeem:?}"),
            )
        })
        .and(|| {
            Outcome::require(
                !mcp.is_empty()
                    && proofs(&mcp)
                    && mcp.iter().all(|r| {
                        r["authorization"]
                            .as_str()
                            .is_some_and(|a| a.starts_with("DPoP "))
                    }),
                format!("every MCP request is DPoP with a proof: {mcp:?}"),
            )
        })
        .and(|| {
            Outcome::require(
                mcp.len() >= 2
                    && nonce_of(&mcp[0]).is_null()
                    && mcp[1]["body"]["method"] == mcp[0]["body"]["method"]
                    && mcp[1..].iter().all(|r| nonce_of(r) == "m-1")
                    && !enrolled.contains("not listed"),
                format!(
                    "the MCP request refused for its nonce is sent again at once with it: {mcp:?} ({enrolled})"
                ),
            )
        });
    if !o.passed {
        return o;
    }
    let (plain, redeem, mcp, _) = tryo!(legs(json!({"dpop": true})));
    let o = completed(&plain).and(|| {
        Outcome::require(
            redeem.iter().all(|r| r["dpop"].is_null())
                && mcp.iter().all(|r| {
                    r["dpop"].is_null()
                        && r["authorization"]
                            .as_str()
                            .is_some_and(|a| a.starts_with("Bearer "))
                }),
            format!("the resource advertises none: no proof on either leg: {redeem:?} {mcp:?}"),
        )
    });
    if !o.passed {
        return o;
    }
    // Its catalogue is listed with a delegated token at enrolment: listing
    // it is refused, said, and nothing is redeemed.
    let s = Setup::new(
        h,
        json!({"prm_dpop": true, "dpop_required": true}),
        &Opts::default(),
    );
    let _d = s.daemon(h);
    let enrolled = tryo!(s.login(h));
    Outcome::require(
        enrolled.contains("not listed") && enrolled.contains("DPoP-bound tokens alone"),
        format!("refused as needing bound tokens: {enrolled}"),
    )
    .and(|| {
        Outcome::require(
            s.xaa.grants("ras", JWT_BEARER).is_empty(),
            "nothing is redeemed for it",
        )
    })
}

/// `nightly` acting for alice `via` as it says, its call and a wait for
/// the signal `go` side by side: the signal ends the run whatever the call
/// is doing.
fn ended_by_signal(via: &str) -> String {
    format!(
        "  - name: nightly\n    act_for: {{principal: {{source: operator, id: alice}}, via: {via}}}\n    concurrency: {{max_runs: 1, on_overflow: drop}}\n    steps:\n      start: {{kind: schedule, every: 2s}}\n      call: {{kind: mcp.tool, depends_on: [start], server: linear, tool: whoami, args: {{}}}}\n      w: {{kind: wait, depends_on: [start], on: signal, signal: go}}\n      done: {{kind: finish, depends_on: [w]}}\n"
    )
}

/// A firing whose run ends while its call is on its way — here, while the
/// call proves the grant alive at the IdP — acts for nobody from then on.
/// `via: ciba`, it mints nothing: the call keeps the firing it was
/// admitted as, whose approval ended with the run, never falls back to
/// alice's refresh token, and fails `subject_expired`. `via: standing`, the
/// token it got is presented nowhere: the call is not made (`revoked`).
pub(super) fn a_firing_ended_mid_call_mints_nothing(h: &Harness) -> Outcome {
    let wf = ended_by_signal("ciba");
    // Every refresh takes 4 s; the grant is proved alive at every call.
    let s = Setup::new(
        h,
        json!({"slow_refresh_ms": 4000}),
        &Opts {
            liveness: Some("1s"),
            extra: wf,
            ..ciba_opts(10)
        },
    );
    let d = s.daemon(h);
    tryo!(s.login(h));
    // What the enrolment minted, listing the server's tools.
    let enrolled = (
        s.xaa.grants("idp", EXCHANGE).len(),
        s.events("delegation.minted").len(),
    );
    // The approval, then the approved call's liveness refresh: the run ends
    // while the IdP holds it.
    let probing = || {
        let record = s.xaa.record();
        let polls: Vec<usize> = record
            .iter()
            .enumerate()
            .filter(|(_, r)| r["body"]["grant_type"] == CIBA)
            .map(|(i, _)| i)
            .collect();
        polls.len() >= 2
            && record[polls[1]..]
                .iter()
                .any(|r| r["at"] == "idp" && r["body"]["grant_type"] == "refresh_token")
    };
    tryo!(s.until(
        "the approved call proving the grant alive",
        Duration::from_secs(60),
        &probing
    ));
    let fired = s.op("workflow.signal", json!({"name": "go"}));
    if fired.get("error").is_some() {
        return Outcome::fail(format!("signal: {fired}"));
    }
    let ended = tryo!(s.ended("nightly", 0, Duration::from_secs(10)));
    let refused = s.until(
        "the call refused once its probe returned",
        Duration::from_secs(15),
        &|| {
            s.events("delegation.denied")
                .iter()
                .any(|e| e["code"] == "subject_expired")
        },
    );
    std::thread::sleep(Duration::from_secs(1));
    let exchanged: Vec<Value> = s
        .xaa
        .grants("idp", EXCHANGE)
        .into_iter()
        .skip(enrolled.0)
        .collect();
    let minted: Vec<Value> = s
        .events("delegation.minted")
        .into_iter()
        .skip(enrolled.1)
        .collect();
    let o = completed(&ended)
        .and(|| Outcome::require(refused.is_ok(), refused.err().unwrap_or_default()))
        .and(|| {
            Outcome::require(
                exchanged.is_empty() && minted.is_empty(),
                format!(
                    "nothing is minted for the ended firing, and never with her refresh token: {exchanged:?} {minted:?}"
                ),
            )
        });
    if !o.passed {
        return o;
    }
    drop(d);
    let s = Setup::new(
        h,
        json!({"slow_refresh_ms": 4000}),
        &Opts {
            liveness: Some("1s"),
            ..standing_opts("whoami", "{}")
        },
    );
    s.edit(|c| {
        c.replace(
            &nightly("standing", "whoami", "{}"),
            &ended_by_signal("standing"),
        )
    });
    let _d = s.daemon(h);
    tryo!(s.login(h));
    let refreshes = || {
        s.xaa
            .record()
            .iter()
            .filter(|r| r["at"] == "idp" && r["body"]["grant_type"] == "refresh_token")
            .count()
    };
    let before = refreshes();
    tryo!(s.until(
        "the call proving the grant alive",
        Duration::from_secs(30),
        &|| refreshes() > before
    ));
    let fired = s.op("workflow.signal", json!({"name": "go"}));
    if fired.get("error").is_some() {
        return Outcome::fail(format!("signal: {fired}"));
    }
    let ended = tryo!(s.ended("nightly", 0, Duration::from_secs(10)));
    let refused = s.until(
        "the call refused once its token was got",
        Duration::from_secs(15),
        &|| {
            s.events("delegation.denied")
                .iter()
                .any(|e| e["code"] == "revoked")
        },
    );
    std::thread::sleep(Duration::from_secs(1));
    completed(&ended)
        .and(|| Outcome::require(refused.is_ok(), refused.err().unwrap_or_default()))
        .and(|| {
            Outcome::require(
                s.made("whoami") == 0,
                "the ended standing firing's call is not made",
            )
        })
}

/// A firing's authority is its run's: a run its work starts — a
/// `workflow` step's child — acts for alice only as her consent to that
/// run's own workflow says. One declaring no `act_for` for her has none:
/// its delegated call fails `unattended_suspended` and mints nothing; once
/// it declares one and she signs in again, it acts for her.
pub(super) fn work_a_firing_starts_acts_only_as_consented(h: &Harness) -> Outcome {
    let wf = "  - name: nightly\n    act_for: {principal: {source: operator, id: alice}, via: standing}\n    concurrency: {max_runs: 1, on_overflow: drop}\n    steps:\n      start: {kind: schedule, every: 2s}\n      sub: {kind: workflow, depends_on: [start], name: body}\n      done: {kind: finish, depends_on: [sub]}\n  - name: body\n    steps:\n      start: {kind: manual}\n      call: {kind: mcp.tool, depends_on: [start], server: linear, tool: whoami, args: {}}\n      done: {kind: finish, depends_on: [call]}\n";
    let s = Setup::new(
        h,
        json!({}),
        &Opts {
            template_extra: ", unattended: standing",
            extra: wf.to_string(),
            ..Opts::default()
        },
    );
    let _d = s.daemon(h);
    tryo!(s.login(h));
    let exchanges = s.xaa.grants("idp", EXCHANGE).len();
    let child = tryo!(s.ended("body", 0, Duration::from_secs(30)));
    tryo!(s.ended("nightly", 0, Duration::from_secs(30)));
    let minted = s.xaa.grants("idp", EXCHANGE).len() - exchanges;
    let carried = s.events("start.unattended.carried");
    s.edit(|c| {
        c.replace(
            "  - name: body\n",
            "  - name: body\n    act_for: {principal: {source: operator, id: alice}, via: standing}\n",
        )
    });
    tryo!(s.reload("config.reloaded"));
    tryo!(s.login(h));
    tryo!(s.until(
        "a child run acting for her",
        Duration::from_secs(30),
        &|| {
            s.runs_done("body")
                .iter()
                .any(|r| r["status"] == "completed")
        }
    ));
    let acted = s.events("delegation.used").into_iter().any(|e| {
        e["tool"] == "whoami" && e["run"].as_str().is_some_and(|r| r.starts_with("body-"))
    });
    failed_with(&child, "unattended_suspended")
        .and(|| {
            Outcome::require(
                child["err"]
                    .as_str()
                    .is_some_and(|e| e.contains("declares no act_for")),
                format!("the child's workflow declares no act_for for her: {child}"),
            )
        })
        .and(|| Outcome::require(minted == 0, "nothing is minted for the child"))
        .and(|| {
            Outcome::require(
                carried
                    .iter()
                    .any(|e| e["workflow"] == "body" && e["from_workflow"] == "nightly"),
                format!("the child run is a firing of its own: {carried:?}"),
            )
        })
        .and(|| Outcome::require(acted, "consented, the child acts for her"))
}

/// A `via: ciba` firing waits on one request at a time: a second call it
/// makes while its request waits on alice waits on that same request, and
/// both are made once she approves — one request, no second; and once a
/// firing's wait closes unanswered — here, she cancels it in her display
/// client — another firing of the workflow may ask her, though the first
/// firing's run goes on.
pub(super) fn a_ciba_firing_waits_on_one_request_at_a_time(h: &Harness) -> Outcome {
    let two = "  - name: nightly\n    act_for: {principal: {source: operator, id: alice}, via: ciba}\n    concurrency: {max_runs: 1, on_overflow: drop}\n    steps:\n      start: {kind: schedule, every: 2s}\n      a: {kind: mcp.tool, depends_on: [start], server: linear, tool: whoami, args: {}}\n      w: {kind: wait, depends_on: [start], on: signal, signal: go}\n      b: {kind: mcp.tool, depends_on: [w], server: linear, tool: whoami, args: {}}\n      done: {kind: finish, depends_on: [a, b]}\n";
    let s = Setup::new(
        h,
        json!({"ciba": "pending"}),
        &Opts {
            extra: two.to_string(),
            ..ciba_opts(10)
        },
    );
    let d = s.daemon(h);
    tryo!(s.login(h));
    tryo!(s.until("a CIBA request", Duration::from_secs(30), &|| {
        !s.ciba_requests().is_empty()
    }));
    let fired = s.op("workflow.signal", json!({"name": "go"}));
    if fired.get("error").is_some() {
        return Outcome::fail(format!("signal: {fired}"));
    }
    let waits = s.until(
        "the second call waiting on the same request",
        Duration::from_secs(20),
        &|| {
            s.events("delegation.ciba")
                .iter()
                .any(|e| e["decision"] == "waits")
                || !s.runs_done("nightly").is_empty()
        },
    );
    s.xaa.control(json!({"ciba": "approve"}));
    let run = tryo!(s.ended("nightly", 0, Duration::from_secs(30)));
    let asked = s.ciba_requests().len();
    let made = s.made("whoami");
    let o = completed(&run)
        .and(|| Outcome::require(waits.is_ok(), waits.err().unwrap_or_default()))
        .and(|| {
            Outcome::require(
                asked == 1 && made == 2,
                format!("one request for both calls, both made: {asked} asked, {made} made"),
            )
        });
    if !o.passed {
        return o;
    }
    drop(d);
    // A firing whose call fails goes on to sleep; three may run at once.
    let long = "  - name: nightly\n    act_for: {principal: {source: operator, id: alice}, via: ciba}\n    concurrency: {max_runs: 3, on_overflow: drop}\n    steps:\n      start: {kind: schedule, every: 3s}\n      call: {kind: mcp.tool, depends_on: [start], server: linear, tool: whoami, args: {}, on_error: continue}\n      rest: {kind: sleep, depends_on: [call], duration: 120s}\n      done: {kind: finish, depends_on: [rest]}\n";
    let s = Setup::new(
        h,
        json!({"ciba": "pending"}),
        &Opts {
            extra: long.to_string(),
            ..ciba_opts(10)
        },
    );
    let _d = s.daemon(h);
    tryo!(s.login(h));
    tryo!(s.until("a CIBA request", Duration::from_secs(30), &|| {
        !s.ciba_requests().is_empty()
    }));
    let gate = tryo!(s.gates(h, 1));
    let id = gate[0]["gate"].as_str().unwrap_or_default().to_string();
    let cancelled = s.cli(h, &["--delegation-deny", &id]);
    let again = s.until(
        "another firing asking once the first one's wait closed",
        Duration::from_secs(20),
        &|| s.ciba_requests().len() >= 2,
    );
    Outcome::require(
        cancelled.code == Some(0),
        format!(
            "--delegation-deny: {} {}",
            cancelled.stdout, cancelled.stderr
        ),
    )
    .and(|| Outcome::require(again.is_ok(), again.err().unwrap_or_default()))
    .and(|| {
        Outcome::require(
            s.runs_done("nightly").is_empty(),
            "the first firing's run is still going",
        )
    })
}
