// SPDX-License-Identifier: AGPL-3.0-only
//! RFC 0044 §7.P3a, P3 built, part 1: inbound identity. A deployment on a
//! loopback TCP listener ([`Fed`]) that takes the mock IdP's access tokens
//! as an Okta custom authorization server issues them (`a2a.inbound_tokens`,
//! profile `okta`): whom a token names, every token it refuses and why, the
//! card's schemes and the protected-resource metadata, the scope a send
//! needs, the sign-in a task waits on at admission, a peer's
//! `AUTH_REQUIRED`, the same-domain exchange (M1b), a webhook a person's
//! token signs — and, on the family's own [`Setup`], the policies' reload
//! and a first-use approval across a restart. This process plays the
//! front-end client: it has the mock IdP sign each token as the IdP would
//! issue it to that client.

use super::p2c::{ALICE, BOB, Fed, FedOpts, PEOPLE, at_idp, back, open_start, sign_in_at};
use super::*;
use crate::checks::util::{self, header};

/// The front-end client the issuer lists.
const FRONT_END: &str = "front-end";
/// The listener as a resource server for the mock IdP's tokens.
const INBOUND: &str = "  resource: \"http://127.0.0.1:__PORT__\"\n  inbound_tokens:\n    - issuer: \"__ISS__\"\n      profile: okta\n      clients: [front-end]\n      scopes_to_skills: {agent.read: [conversation], agent.run: [\"workflow:*\"]}\n";
/// A workflow that calls nothing.
const QUICK: &str = "  - name: quick\n    steps:\n      start: {kind: manual}\n      done: {kind: finish, depends_on: [start]}\n";
/// A server whose token is exchanged at the person's own IdP (M1b), for
/// `__SUBJECT__`.
const M1B: &str = "    - name: m1b\n      endpoint: \"__MCP__\"\n      tags: {\"*\": [untrusted_input]}\n      auth:\n        kind: oauth2\n        grant: token_exchange\n        idp: okta\n        subject: __SUBJECT__\n        audience: \"api://issues\"\n        resource: \"__MCP__\"\n        scopes: [\"issues:read\"]\n__TOOLS__";
const M1B_TEMPLATE: &str = "{name: m1b, principals: {source: oidc}, idp: okta, servers: {m1b: {scopes: [\"issues:read\"]}}}";
const M1B_WHO: &str = "  - name: m1b-who\n    steps:\n      start: {kind: manual}\n      call: {kind: mcp.tool, depends_on: [start], server: m1b, tool: whoami, args: {}}\n      done: {kind: finish, depends_on: [call]}\n";
const ACCESS_TOKEN: &str = "urn:ietf:params:oauth:token-type:access_token";

fn now_s() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// An access token's claims as an Okta custom authorization server issues
/// them to the front end for `uid`, for `aud`, with scopes `scp`.
fn at_claims(x: &Xaa, aud: &str, uid: &str, scp: &[&str]) -> Value {
    let t = now_s();
    json!({"ver": 1, "jti": format!("AT.{}", b64url(&rand16())), "iss": x.idp, "aud": aud,
           "iat": t, "exp": t + 600, "cid": FRONT_END, "uid": uid, "scp": scp,
           "sub": format!("{uid}@acme.example"), "auth_time": t})
}

/// A deployment that takes the mock IdP's tokens, `o` around it; its
/// people under [`PEOPLE`] when `o` names no template.
fn fed(h: &Harness, o: FedOpts) -> Fed {
    let templates = if o.templates.is_empty() {
        vec![PEOPLE]
    } else {
        o.templates.clone()
    };
    let o = FedOpts {
        templates,
        a2a_extra: format!("{INBOUND}{}", o.a2a_extra),
        extra: format!("{QUICK}{}", o.extra),
        ..o
    };
    Fed::new(h, &o)
}

impl Fed {
    fn resource(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    fn prm(&self) -> String {
        format!("{}/.well-known/oauth-protected-resource", self.resource())
    }

    /// `uid`'s access token for this listener, with scopes `scp`.
    fn token(&self, uid: &str, scp: &[&str]) -> String {
        self.xaa
            .mint(json!({}), &at_claims(&self.xaa, &self.resource(), uid, scp))
    }

    /// `method` as `token` (none when empty): the status, the
    /// `WWW-Authenticate` header, the envelope.
    fn call_as(&self, token: &str, method: &str, params: Value) -> (u16, String, Value) {
        let auth = format!("Bearer {token}");
        let exts = format!("{}, {}", util::COMMAND, util::ANNOTATIONS);
        let mut headers = vec![("A2A-Extensions", exts.as_str())];
        if !token.is_empty() {
            headers.push(("Authorization", auth.as_str()));
        }
        let (status, h, body) =
            util::post_full(&self.addr(), &util::rpc_body(1, method, params), &headers);
        (
            status,
            header(&h, "www-authenticate")
                .unwrap_or_default()
                .to_string(),
            serde_json::from_str(&body).unwrap_or(json!({"unparsed": body})),
        )
    }

    /// Start workflow `name` as `token`.
    fn run_with(&self, token: &str, name: &str) -> (u16, String, Value) {
        self.call_as(
            token,
            "SendMessage",
            util::command_send_params("workflow.run", &json!({"workflow": name})),
        )
    }

    /// The `run.start` lines of workflow `name`.
    fn started(&self, name: &str) -> Vec<Value> {
        self.events("run.start")
            .into_iter()
            .filter(|e| e["workflow"] == name)
            .collect()
    }
}

/// Wait up to 20 s for `cond`.
fn until(what: &str, log: &dyn Fn() -> String, cond: &dyn Fn() -> bool) -> Result<(), String> {
    let deadline = Instant::now() + Duration::from_secs(20);
    while !cond() {
        if Instant::now() > deadline {
            return Err(format!("{what}:\n{}", tail(&log())));
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    Ok(())
}

// --- the tokens a resource server refuses -------------------------------------

/// A good token starts a run; each of `bad`'s is a 401 whose challenge says
/// `invalid_token` and names the protected-resource metadata, starts
/// nothing, and is logged with `why` (an issuer's refusal) or as a bearer
/// nobody holds (`None`) — never the token itself.
fn refused(h: &Harness, bad: impl Fn(&Fed) -> Vec<String>, why: Option<&str>) -> Outcome {
    let s = fed(h, FedOpts::default());
    let _d = s.daemon(h);
    let good = s.token(ALICE, &["agent.run"]);
    let (status, _, ok) = s.run_with(&good, "quick");
    let o = Outcome::require(
        status == 200 && ok.get("error").is_none(),
        format!("a good token is taken: {status} {ok}"),
    );
    if !o.passed {
        return o;
    }
    let first = until(
        "the good token's run never ended",
        &|| s.log_text(),
        &|| !s.runs_done("quick").is_empty(),
    );
    if let Err(e) = first {
        return Outcome::fail(e);
    }
    let tokens = bad(&s);
    for t in &tokens {
        let (status, www, body) = s.run_with(t, "quick");
        let o = Outcome::require(
            status == 401
                && www.contains("error=\"invalid_token\"")
                && www.contains(&format!("resource_metadata=\"{}\"", s.prm())),
            format!(
                "a bad token is a 401 naming invalid_token and the metadata: {status} {www:?} {body}"
            ),
        );
        if !o.passed {
            return o;
        }
    }
    std::thread::sleep(Duration::from_millis(500));
    let denied = s.events("a2a.denied");
    let log = s.log_text();
    Outcome::require(
        match why {
            Some(w) => denied.iter().any(|e| {
                e["reason"] == "invalid_token"
                    && e["issuer"] == s.xaa.idp.as_str()
                    && e["why"].as_str().is_some_and(|x| x.contains(w))
            }),
            None => denied.iter().any(|e| e["reason"] == "invalid_credential"),
        },
        format!("the refusal is logged, saying why ({why:?}): {denied:?}"),
    )
    .and(|| {
        Outcome::require(
            s.started("quick").len() == 1,
            "no refused token starts anything",
        )
    })
    .and(|| {
        Outcome::require(
            tokens.iter().all(|t| !log.contains(t.as_str())),
            "no token is logged",
        )
    })
}

/// A JWS's segments, base64url.
fn jws(header: &Value, claims: &Value, sig: &[u8]) -> String {
    format!(
        "{}.{}.{}",
        b64url(header.to_string().as_bytes()),
        b64url(claims.to_string().as_bytes()),
        b64url(sig)
    )
}

pub(super) fn another_issuers_token_is_refused(h: &Harness) -> Outcome {
    refused(
        h,
        |s| {
            let mut c = at_claims(&s.xaa, &s.resource(), ALICE, &["agent.run"]);
            c["iss"] = json!("http://127.0.0.1:9/oauth2/elsewhere");
            vec![s.xaa.mint(json!({}), &c)]
        },
        None,
    )
}

pub(super) fn a_token_for_another_audience_is_refused(h: &Harness) -> Outcome {
    refused(
        h,
        |s| {
            vec![s.xaa.mint(
                json!({}),
                &at_claims(&s.xaa, "http://127.0.0.1:9/a2a", ALICE, &["agent.run"]),
            )]
        },
        Some("names none of"),
    )
}

pub(super) fn an_expired_token_is_refused(h: &Harness) -> Outcome {
    refused(
        h,
        |s| {
            let mut c = at_claims(&s.xaa, &s.resource(), ALICE, &["agent.run"]);
            c["iat"] = json!(now_s() - 1200);
            c["exp"] = json!(now_s() - 600);
            vec![s.xaa.mint(json!({}), &c)]
        },
        Some("has expired"),
    )
}

pub(super) fn an_unsigned_token_is_refused(h: &Harness) -> Outcome {
    refused(
        h,
        |s| {
            let c = at_claims(&s.xaa, &s.resource(), ALICE, &["agent.run"]);
            vec![
                jws(&json!({"alg": "none", "typ": "at+jwt"}), &c, b""),
                jws(&json!({"alg": "None"}), &c, b""),
            ]
        },
        Some("alg none"),
    )
}

/// The key confusion: an HMAC over the token keyed with the issuer's
/// public key set, as text.
pub(super) fn an_hmac_token_is_refused(h: &Harness) -> Outcome {
    refused(
        h,
        |s| {
            let (_, jwks) = http("GET", &format!("{}/keys", s.xaa.idp), None);
            let c = at_claims(&s.xaa, &s.resource(), ALICE, &["agent.run"]);
            let header = json!({"alg": "HS256", "kid": "mock-idp-1"});
            let input = format!(
                "{}.{}",
                b64url(header.to_string().as_bytes()),
                b64url(c.to_string().as_bytes())
            );
            let key = ring::hmac::Key::new(ring::hmac::HMAC_SHA256, jwks.as_bytes());
            let tag = ring::hmac::sign(&key, input.as_bytes());
            vec![format!("{input}.{}", b64url(tag.as_ref()))]
        },
        Some("HMAC"),
    )
}

pub(super) fn a_token_of_a_forbidden_typ_is_refused(h: &Harness) -> Outcome {
    refused(
        h,
        |s| {
            let c = at_claims(&s.xaa, &s.resource(), ALICE, &["agent.run"]);
            vec![
                s.xaa.mint(json!({"typ": "JWT"}), &c),
                s.xaa
                    .mint(json!({"typ": "application/okta-internal-at+jwt"}), &c),
            ]
        },
        Some("typ"),
    )
}

/// The front end's ID token, presented as an access token: `typ: JWT` with
/// its nonce, and with no `typ` (as Okta allows an access token) — its
/// `aud` the client, never this agent.
pub(super) fn an_id_token_is_no_access_token(h: &Harness) -> Outcome {
    refused(
        h,
        |s| {
            let t = now_s();
            let c = json!({"iss": s.xaa.idp, "aud": FRONT_END, "azp": FRONT_END, "sub": ALICE,
                           "iat": t, "exp": t + 600, "auth_time": t, "email": "alice@acme.example"});
            let mut with_nonce = c.clone();
            with_nonce["nonce"] = json!("n-0S6_WzA2Mj");
            // The one with no typ first: a source's refusals of one kind
            // are logged once a window, and its why is the one to read.
            vec![
                s.xaa.mint(json!({}), &c),
                s.xaa.mint(json!({"typ": "JWT"}), &with_nonce),
            ]
        },
        Some("aud names a client"),
    )
}

pub(super) fn an_unlisted_clients_token_is_refused(h: &Harness) -> Outcome {
    refused(
        h,
        |s| {
            let mut c = at_claims(&s.xaa, &s.resource(), ALICE, &["agent.run"]);
            c["cid"] = json!("other-app");
            vec![s.xaa.mint(json!({}), &c)]
        },
        Some("not among the issuer's clients"),
    )
}

/// `client_credentials`: a token issued to the client alone names no
/// person, so it is never a user — and with `require_user_token` (the
/// default) it is refused.
pub(super) fn a_clients_own_token_names_no_user(h: &Harness) -> Outcome {
    refused(
        h,
        |s| {
            let mut c = at_claims(&s.xaa, &s.resource(), ALICE, &["agent.run"]);
            c.as_object_mut().map(|o| o.remove("uid"));
            c["sub"] = json!(FRONT_END);
            vec![s.xaa.mint(json!({}), &c)]
        },
        Some("issued to a client alone"),
    )
}

// --- whom a token names --------------------------------------------------------

/// A token names its person, `user:oidc:<iss>#<uid>`: the run they start
/// acts for them, the operator lists the inbound session it is kept as —
/// its issuer, client, scopes and actor chain, never the token — and an
/// `act` chain deeper than the issuer's bound is refused, never cut.
pub(super) fn a_token_names_its_person(h: &Harness) -> Outcome {
    let s = fed(h, FedOpts::default());
    let _d = s.daemon(h);
    let mut c = at_claims(&s.xaa, &s.resource(), ALICE, &["agent.run"]);
    c["act"] = json!({"sub": "agent-a", "act": {"sub": "agent-b"}});
    let token = s.xaa.mint(json!({}), &c);
    let (status, _, r) = s.run_with(&token, "quick");
    let o = Outcome::require(status == 200, format!("the token is taken: {status} {r}"));
    if !o.passed {
        return o;
    }
    let done = tryo!(s.ended("quick", 0));
    let principal = format!("user:oidc:{}#{ALICE}", s.xaa.idp);
    let start = s.started("quick");
    let sessions = s.sessions();
    let theirs = sessions
        .iter()
        .find(|x| x["kind"] == "inbound" && x["principal"] == principal.as_str())
        .cloned();
    let mut deep = c.clone();
    let mut chain = json!({"sub": "agent-0"});
    for i in 1..=5 {
        chain = json!({"sub": format!("agent-{i}"), "act": chain});
    }
    deep["act"] = chain;
    deep["jti"] = json!("AT.deep");
    let (deep_status, _, _) = s.run_with(&s.xaa.mint(json!({}), &deep), "quick");
    completed(&done)
        .and(|| {
            Outcome::require(
                start.first().is_some_and(|e| {
                    e["acting_for"] == principal.as_str()
                        && e["act"] == json!(["agent-a", "agent-b"])
                }),
                format!("the run acts for {principal}: {start:?}"),
            )
        })
        .and(|| {
            Outcome::require(
                theirs.as_ref().is_some_and(|x| {
                    x["issuer"] == s.xaa.idp.as_str()
                        && x["client_id"] == FRONT_END
                        && x["scopes"] == json!(["agent.run"])
                        && x["act"] == json!(["agent-a", "agent-b"])
                        && x["role"] == "user"
                }),
                format!("the operator lists the inbound session: {sessions:?}"),
            )
        })
        .and(|| {
            Outcome::require(
                !Value::Array(sessions.clone()).to_string().contains(&token),
                "the listing carries no token",
            )
        })
        .and(|| {
            Outcome::require(
                deep_status == 401,
                format!("a chain of six is past the bound of four: {deep_status}"),
            )
        })
}

/// The card declares the issuer as an `oauth2` scheme — its metadata URL
/// and the authorization-code flow with PKCE — beside the operator's
/// bearer, and every requirement names a scheme the card declares; the
/// protected-resource metadata (RFC 9728) names the resource and the
/// issuer; a request with no credential is challenged with it.
pub(super) fn the_card_and_the_metadata_name_the_issuer(h: &Harness) -> Outcome {
    let s = fed(h, FedOpts::default());
    let _d = s.daemon(h);
    let card = util::get_card(&s.addr());
    let schemes = card["securitySchemes"].clone();
    let oauth = schemes["enterprise"]["oauth2SecurityScheme"].clone();
    let declared: Vec<String> = schemes
        .as_object()
        .map(|o| o.keys().cloned().collect())
        .unwrap_or_default();
    let required: Vec<String> = card["securityRequirements"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|r| r["schemes"].as_object())
        .flat_map(|o| o.keys().cloned())
        .collect();
    let (prm_status, _, prm) = util::exchange(
        &s.addr(),
        "GET",
        "/.well-known/oauth-protected-resource",
        &[],
        "",
    );
    let prm: Value = serde_json::from_str(&prm).unwrap_or_default();
    let (status, www, _) = s.call_as("", "SendMessage", util::text_params("hi", None, false));
    Outcome::require(
        oauth["oauth2MetadataUrl"]
            == format!("{}/.well-known/oauth-authorization-server", s.xaa.idp).as_str()
            && oauth["flows"]["authorizationCode"]["authorizationUrl"]
                == format!("{}/authorize", s.xaa.idp).as_str()
            && oauth["flows"]["authorizationCode"]["tokenUrl"]
                == format!("{}/token", s.xaa.idp).as_str()
            && oauth["flows"]["authorizationCode"]["pkceRequired"] == true,
        format!("the issuer is an oauth2 scheme with its metadata and flow: {schemes}"),
    )
    .and(|| {
        Outcome::require(
            declared.iter().any(|d| d == "bearer")
                && !required.is_empty()
                && required.iter().all(|r| declared.contains(r))
                && required.iter().any(|r| r == "enterprise"),
            format!("every requirement names a declared scheme: {declared:?} {required:?}"),
        )
    })
    .and(|| {
        Outcome::require(
            prm_status == 200
                && prm["resource"] == s.resource().as_str()
                && prm["authorization_servers"] == json!([s.xaa.idp])
                && prm["scopes_supported"]
                    .as_array()
                    .is_some_and(|a| a.iter().any(|x| x == "agent.run")),
            format!("the protected-resource metadata: {prm_status} {prm}"),
        )
    })
    .and(|| {
        Outcome::require(
            status == 401
                && www.starts_with("Bearer")
                && www.contains(&format!("resource_metadata=\"{}\"", s.prm()))
                && !www.contains("error="),
            format!("no credential: a challenge naming the metadata, no error: {status} {www:?}"),
        )
    })
}

/// A send the token's scopes do not reach is a 403 naming
/// `insufficient_scope` and the scope that would — not a 401: the token is
/// good — and the same token reaches what its scope maps to.
pub(super) fn a_send_needs_the_scope_its_skill_maps_to(h: &Harness) -> Outcome {
    let s = fed(h, FedOpts::default());
    let _d = s.daemon(h);
    let reader = s.token(ALICE, &["agent.read"]);
    let (status, www, r) = s.run_with(&reader, "quick");
    let (talk, _, said) = s.call_as(
        &reader,
        "SendMessage",
        util::text_params("hello", None, true),
    );
    let runner = s.token(ALICE, &["agent.run"]);
    let (ran, _, _) = s.run_with(&runner, "quick");
    let denied = s.events("a2a.denied");
    Outcome::require(
        status == 403
            && www.contains("error=\"insufficient_scope\"")
            && www.contains("scope=\"agent.run\"")
            && www.contains(&format!("resource_metadata=\"{}\"", s.prm())),
        format!("the workflow needs agent.run: {status} {www:?} {r}"),
    )
    .and(|| {
        Outcome::require(
            denied.iter().any(|e| e["reason"] == "insufficient_scope"),
            format!("the refusal is logged: {denied:?}"),
        )
    })
    .and(|| {
        Outcome::require(
            talk == 200 && said.get("error").is_none(),
            format!("agent.read reaches a conversation: {talk} {said}"),
        )
    })
    .and(|| Outcome::require(ran == 200, format!("agent.run reaches it: {ran}")))
}

/// A task is its caller's: another person's token reads, cancels and
/// continues none of it (`TaskNotFound`), while its own caller's does.
pub(super) fn a_callers_tasks_are_theirs_alone(h: &Harness) -> Outcome {
    let s = fed(h, FedOpts::default());
    let _d = s.daemon(h);
    let alice = s.token(ALICE, &["agent.read"]);
    let bob = s.token(BOB, &["agent.read", "agent.run"]);
    let (_, _, r) = s.call_as(
        &alice,
        "SendMessage",
        util::text_params("hello", None, true),
    );
    let Some(id) = r["result"]["task"]["id"].as_str().map(str::to_string) else {
        return Outcome::fail(format!("no task: {r}"));
    };
    let (_, _, mine) = s.call_as(&alice, "GetTask", json!({"id": id}));
    let (_, _, read) = s.call_as(&bob, "GetTask", json!({"id": id}));
    let (_, _, cancel) = s.call_as(&bob, "CancelTask", json!({"id": id}));
    let (_, _, more) = s.call_as(
        &bob,
        "SendMessage",
        util::text_params("me too", Some(&id), true),
    );
    let (_, _, listed) = s.call_as(&bob, "ListTasks", json!({}));
    let not_found = |v: &Value| v["error"]["code"] == -32001;
    Outcome::require(
        mine["result"]["id"] == id.as_str(),
        format!("its caller reads it: {mine}"),
    )
    .and(|| {
        Outcome::require(
            not_found(&read) && not_found(&cancel) && not_found(&more),
            format!("another person finds none of it: {read} {cancel} {more}"),
        )
    })
    .and(|| {
        Outcome::require(
            !listed.to_string().contains(&id),
            format!("nor lists it: {listed}"),
        )
    })
}

// --- the sign-in a task waits on -------------------------------------------------

/// A caller an inbound token names asks for a workflow acting with a grant
/// they do not hold: the task waits in `TASK_STATE_AUTH_REQUIRED` with a
/// start link, no step runs, a message on it is refused and someone else
/// finds nothing; they sign in at the IdP through the link and the run goes
/// on with no follow-up message, acting with their new grant. The sign-in
/// authorizes that run alone: a later message on the task is held to its
/// own credential — the caller's expired token is a 401, another person's
/// finds nothing.
pub(super) fn admission_waits_on_the_callers_sign_in(h: &Harness) -> Outcome {
    let s = fed(h, FedOpts::default());
    let _d = s.daemon(h);
    s.xaa.control(json!({"sub": ALICE}));
    let alice = s.token(ALICE, &["agent.read", "agent.run"]);
    let (status, _, r) = s.run_with(&alice, "who");
    let task = r["result"]["task"].clone();
    let id = task["id"].as_str().unwrap_or_default().to_string();
    let link = task["metadata"][util::ANNOTATIONS]["signIn"]["start_uri"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    let text = task["status"]["message"]["parts"][0]["text"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    let o = Outcome::require(
        status == 200
            && task["status"]["state"] == "TASK_STATE_AUTH_REQUIRED"
            && !link.is_empty()
            && link.starts_with(&s.resource())
            && text.contains(&link),
        format!("the task waits on a sign-in, with its link: {status} {r}"),
    );
    if !o.passed {
        return o;
    }
    std::thread::sleep(Duration::from_millis(300));
    let started_early = s.started("who").len();
    let (_, _, nudged) = s.call_as(
        &alice,
        "SendMessage",
        util::text_params("go on", Some(&id), true),
    );
    let (_, _, still) = s.call_as(&alice, "GetTask", json!({"id": id}));
    let bob = s.token(BOB, &["agent.read"]);
    let (_, _, bobs) = s.call_as(&bob, "GetTask", json!({"id": id}));
    let o = Outcome::require(started_early == 0, "no step runs before the sign-in")
        .and(|| {
            Outcome::require(
                nudged.get("error").is_some()
                    && still["result"]["status"]["state"] == "TASK_STATE_AUTH_REQUIRED",
                format!("a message answers nothing, and it still waits: {nudged} {still}"),
            )
        })
        .and(|| {
            Outcome::require(
                bobs["error"]["code"] == -32001,
                format!("another person finds nothing: {bobs}"),
            )
        });
    if !o.passed {
        return o;
    }
    let (page, body) = tryo!(sign_in_at(&link));
    let (state, end) = tryo!(s.task_end(&alice, &id));
    let run = tryo!(s.ended("who", 0));
    // What the sign-in obtained authorizes that run alone: a later message
    // on the task is held to the credential it carries — the caller's own
    // token, expired, names nobody however recently they signed in, and
    // another person's finds no task — and the page handed the browser no
    // credential of its own.
    let mut stale = at_claims(&s.xaa, &s.resource(), ALICE, &["agent.read", "agent.run"]);
    stale["exp"] = json!(now_s() - 120);
    let expired = s.xaa.mint(json!({}), &stale);
    let (later, www, _) = s.call_as(
        &expired,
        "SendMessage",
        util::text_params("and now?", Some(&id), true),
    );
    let (_, _, bobs_later) = s.call_as(
        &bob,
        "SendMessage",
        util::text_params("and now?", Some(&id), true),
    );
    let (again, _, second) = s.run_with(&alice, "who");
    Outcome::require(page == 200, format!("the callback: {page} {body}"))
        .and(|| {
            Outcome::require(
                state == "TASK_STATE_COMPLETED",
                format!("the run went on with no message: {state} {end}"),
            )
        })
        .and(|| completed(&run))
        .and(|| {
            Outcome::require(
                s.minted_for().last().map(String::as_str) == Some(ALICE),
                "the call was made with the grant the sign-in kept",
            )
        })
        .and(|| {
            Outcome::require(
                later == 401 && www.contains("invalid_token"),
                format!("the caller's expired token is no credential, signed in or not: {later} {www:?}"),
            )
        })
        .and(|| {
            Outcome::require(
                bobs_later["error"]["code"] == -32001 && !body.contains("agentd_at_"),
                format!("another person finds no task, and the page handed out no credential: {bobs_later}"),
            )
        })
        .and(|| {
            Outcome::require(
                again == 200
                    && second["result"]["task"]["status"]["state"] != "TASK_STATE_AUTH_REQUIRED",
                format!("held a grant now, the next run asks nothing: {second}"),
            )
        })
}

/// A caller who cancels a task waiting on their admission sign-in ends the
/// sign-in with it: finished in the browser tab they had open, it comes back
/// to a page saying it has ended — no code redeemed, no grant kept — and the
/// workflow they cancelled never runs.
pub(super) fn a_cancelled_admission_starts_nothing(h: &Harness) -> Outcome {
    let s = fed(h, FedOpts::default());
    let _d = s.daemon(h);
    s.xaa.control(json!({"sub": ALICE}));
    let alice = s.token(ALICE, &["agent.read", "agent.run"]);
    let (_, _, r) = s.run_with(&alice, "who");
    let task = &r["result"]["task"];
    let id = task["id"].as_str().unwrap_or_default().to_string();
    let link = task["metadata"][util::ANNOTATIONS]["signIn"]["start_uri"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    if task["status"]["state"] != "TASK_STATE_AUTH_REQUIRED" || link.is_empty() {
        return Outcome::fail(format!("the task waits on a sign-in: {r}"));
    }
    let (idp, cookie) = tryo!(open_start(&link));
    let (_, _, cancelled) = s.call_as(&alice, "CancelTask", json!({"id": id}));
    let callback = tryo!(at_idp(&idp));
    let (page, body) = back(&callback, Some(&cookie));
    std::thread::sleep(Duration::from_millis(500));
    let (_, _, after) = s.call_as(&alice, "GetTask", json!({"id": id}));
    let redeemed = s.xaa.grants("idp", "authorization_code");
    Outcome::require(
        cancelled["result"]["status"]["state"] == "TASK_STATE_CANCELED",
        format!("the task is cancelled: {cancelled}"),
    )
    .and(|| {
        Outcome::require(
            page != 200 && body.contains("ended"),
            format!("the sign-in it waited on has ended: {page} {body}"),
        )
    })
    .and(|| {
        Outcome::require(
            redeemed.is_empty(),
            format!("no code is redeemed: {redeemed:?}"),
        )
    })
    .and(|| {
        Outcome::require(
            s.started("who").is_empty()
                && after["result"]["status"]["state"] == "TASK_STATE_CANCELED",
            format!("the cancelled workflow never runs: {after}"),
        )
    })
}

/// A token whose scopes reach workflows alone answers its own run's
/// `human` step — the run its scope admitted asks it — with a plain
/// message on the run's task; a message on that task that would become a
/// conversation turn is a 403 `insufficient_scope`.
pub(super) fn a_workflow_token_answers_its_own_runs_question(h: &Harness) -> Outcome {
    let flows = "  - name: ask-me\n    steps:\n      start: {kind: manual}\n      q: {kind: human, depends_on: [start], question: \"Ship it?\", timeout: 1m}\n      w: {kind: wait, depends_on: [q], on: signal, signal: shipped}\n      done: {kind: finish, depends_on: [w]}\n";
    let s = fed(
        h,
        FedOpts {
            extra: flows.into(),
            ..FedOpts::default()
        },
    );
    let _d = s.daemon(h);
    let runner = s.token(ALICE, &["agent.run"]);
    let (_, _, r) = s.run_with(&runner, "ask-me");
    let Some(id) = r["result"]["task"]["id"].as_str().map(str::to_string) else {
        return Outcome::fail(format!("no task: {r}"));
    };
    tryo!(until(
        "the question never opened",
        &|| s.log_text(),
        &|| { !s.events("human.ask").is_empty() }
    ));
    let (answered, _, said) = s.call_as(
        &runner,
        "SendMessage",
        util::text_params("yes", Some(&id), true),
    );
    tryo!(until("the run never parked", &|| s.log_text(), &|| {
        s.events("step.start").iter().any(|e| e["step"] == "w")
    }));
    let (turn, www, refused) = s.call_as(
        &runner,
        "SendMessage",
        util::text_params("and tell me a story", Some(&id), true),
    );
    let fired = s.rpc_as(
        &s.ops,
        "SendMessage",
        util::command_send_params("workflow.signal", &json!({"name": "shipped"})),
    );
    let run = tryo!(s.ended("ask-me", 0));
    Outcome::require(
        answered == 200 && said.get("error").is_none(),
        format!("its answer is taken: {answered} {said}"),
    )
    .and(|| {
        Outcome::require(
            turn == 403 && www.contains("insufficient_scope"),
            format!("a turn on the task needs a conversation's scope: {turn} {www:?} {refused}"),
        )
    })
    .and(|| Outcome::require(fired.get("error").is_none(), format!("signal: {fired}")))
    .and(|| completed(&run))
}

/// A peer's task waits on its person (`TASK_STATE_AUTH_REQUIRED`): the
/// run's task says so, quoting the peer; the wait survives a restart
/// without sending the objective again; and once the person authorizes the
/// peer directly, the delegation completes. The run is a token's caller's,
/// whose token agentd holds for an exchange back at its issuer: neither it
/// nor any credential of agentd's reaches the peer, nor anything else the
/// mocks saw.
pub(super) fn a_peers_sign_in_request_waits_for_its_person(h: &Harness) -> Outcome {
    let flows = "  - name: ask-peer\n    steps:\n      start: {kind: manual}\n      ask: {kind: a2a.delegate, depends_on: [start], peer: cal, objective: \"read my calendar\", timeout: 20s, auth_timeout: 2m}\n      done: {kind: finish, depends_on: [ask]}\n";
    let s = m1b_with(
        h,
        "inbound",
        flows,
        "  peers:\n    - name: cal\n      endpoint: \"__PEER__\"\n",
    );
    s.xaa.control(json!({"peer_auth": true}));
    let d = s.daemon(h);
    let alice = s.token(ALICE, &["agent.run"]);
    let (status, _, started) = s.run_with(&alice, "ask-peer");
    if status != 200 || started.get("error").is_some() {
        return Outcome::fail(format!("the caller's run: {status} {started}"));
    }
    let n = 0;
    let asked = |s: &Fed| {
        s.events("a2a.peer.auth_required")
            .into_iter()
            .filter(|e| e.get("decision").is_none())
            .collect::<Vec<_>>()
    };
    tryo!(until(
        "the peer's request never showed",
        &|| s.log_text(),
        &|| { !asked(&s).is_empty() }
    ));
    let task = asked(&s)[0]["task"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    let t = s.rpc_as(&s.ops, "GetTask", json!({"id": task}));
    let text = t["result"]["status"]["message"]["parts"][0]["text"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    let o = Outcome::require(
        t["result"]["status"]["state"] == "TASK_STATE_AUTH_REQUIRED"
            && text.contains("cal")
            && text.contains("so I may read your calendar"),
        format!("the run's task waits, quoting the peer: {t}"),
    );
    if !o.passed {
        return o;
    }
    drop(d);
    let _d = s.daemon(h);
    std::thread::sleep(Duration::from_secs(2));
    s.xaa.control(json!({"peer_authorized": true}));
    let run = tryo!(s.ended("ask-peer", n));
    let peer: Vec<Value> = s
        .xaa
        .record()
        .into_iter()
        .filter(|r| r["at"] == "peer")
        .collect();
    let sends = peer
        .iter()
        .filter(|r| r["body"]["method"] == "SendMessage")
        .count();
    let resumed = s
        .events("a2a.peer.auth_required")
        .iter()
        .any(|e| e["decision"] == "resumed");
    let carrying: Vec<Value> = s
        .xaa
        .record()
        .into_iter()
        .filter(|r| r.to_string().contains(&alice))
        .collect();
    completed(&run)
        .and(|| Outcome::require(resumed, "the wait's end is logged"))
        .and(|| {
            Outcome::require(
                sends == 1,
                format!("the objective is sent once, across the restart: {peer:?}"),
            )
        })
        .and(|| {
            Outcome::require(
                peer.iter().all(|r| r["authorization"].is_null()),
                format!("no credential reaches the peer: {peer:?}"),
            )
        })
        .and(|| {
            Outcome::require(
                carrying.is_empty(),
                format!("the caller's token reaches no mock at all: {carrying:?}"),
            )
        })
}

/// A run whose peer waits on its person (`TASK_STATE_AUTH_REQUIRED` on the
/// run's task) while a `human` step of a parallel branch asks on the same
/// task: the question still takes its answer, and the run completes once
/// the peer is authorized.
pub(super) fn a_question_beside_a_peers_sign_in_is_answered(h: &Harness) -> Outcome {
    let flows = "  - name: both\n    steps:\n      start: {kind: manual}\n      ask: {kind: a2a.delegate, depends_on: [start], peer: cal, objective: \"read my calendar\", timeout: 20s, auth_timeout: 2m}\n      q: {kind: human, depends_on: [start], question: \"Go?\", timeout: 1m}\n      done: {kind: finish, depends_on: [ask, q]}\n";
    let s = fed(
        h,
        FedOpts {
            a2a_extra: "  peers:\n    - name: cal\n      endpoint: \"__PEER__\"\n".into(),
            extra: flows.into(),
            ..FedOpts::default()
        },
    );
    s.xaa.control(json!({"peer_auth": true}));
    let _d = s.daemon(h);
    let r = s.op_as(&s.ops, "workflow.run", json!({"workflow": "both"}));
    let Some(id) = r["result"]["task"]["id"].as_str().map(str::to_string) else {
        return Outcome::fail(format!("no task: {r}"));
    };
    tryo!(until(
        "the question and the peer's request never both showed",
        &|| s.log_text(),
        &|| {
            !s.events("human.ask").is_empty()
                && s.events("a2a.peer.auth_required")
                    .iter()
                    .any(|e| e.get("decision").is_none())
        }
    ));
    let t = s.rpc_as(&s.ops, "GetTask", json!({"id": id}));
    let o = Outcome::require(
        t["result"]["status"]["state"] == "TASK_STATE_AUTH_REQUIRED",
        format!("the run's task waits on the peer's sign-in: {t}"),
    );
    if !o.passed {
        return o;
    }
    let answered = s.rpc_as(
        &s.ops,
        "SendMessage",
        util::text_params("go", Some(&id), true),
    );
    s.xaa.control(json!({"peer_authorized": true}));
    let run = tryo!(s.ended("both", 0));
    Outcome::require(
        answered.get("error").is_none(),
        format!("the question takes its answer: {answered}"),
    )
    .and(|| completed(&run))
}

/// A step-up the IdP asks for puts the run's task in
/// `TASK_STATE_AUTH_REQUIRED` with a start link; a message on it is refused;
/// the person signs in through the link and the call is made with the fresh
/// ID token, the run going on with no message. Nothing of that sign-in
/// reaches the caller or outlives the IdP's `max_age`: past it, the next
/// run's call asks again.
pub(super) fn a_step_up_waits_in_auth_required_for_one_call(h: &Harness) -> Outcome {
    let s = fed(
        h,
        FedOpts {
            login: true,
            ..FedOpts::default()
        },
    );
    // Access tokens that live a second, and ID-JAGs the authorization
    // server takes once: every call goes to the IdP anew.
    s.xaa
        .control(json!({"at_expires_in": 1, "ras_single_use": true}));
    let _d = s.daemon(h);
    let alice = tryo!(s.login(ALICE));
    s.xaa.control(json!({"step_up_max_age": 2}));
    std::thread::sleep(Duration::from_secs(3));
    let gated = |s: &Fed| -> Result<(String, String), String> {
        let (_, _, r) = s.run_with(&alice, "who");
        let id = r["result"]["task"]["id"]
            .as_str()
            .ok_or(format!("no task: {r}"))?
            .to_string();
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let (_, _, t) = s.call_as(&alice, "GetTask", json!({"id": id}));
            let link = t["result"]["metadata"][util::ANNOTATIONS]["consentGate"]["start_uri"]
                .as_str()
                .map(str::to_string);
            if t["result"]["status"]["state"] == "TASK_STATE_AUTH_REQUIRED"
                && let Some(l) = link
            {
                return Ok((id, l));
            }
            if Instant::now() > deadline {
                return Err(format!(
                    "no sign-in was asked for: {t}\n{}",
                    tail(&s.log_text())
                ));
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    };
    let (id, link) = tryo!(gated(&s));
    let (_, _, nudged) = s.call_as(
        &alice,
        "SendMessage",
        util::text_params("go on", Some(&id), true),
    );
    let (page, body) = tryo!(sign_in_at(&link));
    let (state, text) = tryo!(s.task_end(&alice, &id));
    let run = tryo!(s.ended("who", 0));
    let last = s
        .xaa
        .grants("idp", EXCHANGE)
        .last()
        .cloned()
        .unwrap_or_default();
    // Past `max_age` again, the next run's call is asked for anew.
    std::thread::sleep(Duration::from_secs(3));
    let (again, _) = tryo!(gated(&s));
    let asked = s
        .events("delegation.step_up")
        .iter()
        .filter(|e| e["decision"] == "asked")
        .count();
    Outcome::require(
        nudged.get("error").is_some(),
        format!("a message answers no sign-in: {nudged}"),
    )
    .and(|| Outcome::require(page == 200, format!("the callback: {page} {body}")))
    .and(|| {
        Outcome::require(
            state == "TASK_STATE_COMPLETED",
            format!("the run went on with no message: {state} {text}"),
        )
    })
    .and(|| completed(&run))
    .and(|| {
        Outcome::require(
            last["body"]["subject_token_type"] == "urn:ietf:params:oauth:token-type:id_token",
            format!("the call presented the fresh ID token: {last}"),
        )
    })
    .and(|| {
        Outcome::require(
            again != id && asked == 2,
            format!("the next run's call asks again: {asked}"),
        )
    })
}

// --- the same-domain exchange (M1b) ------------------------------------------------

/// A deployment whose `m1b` server exchanges `subject`'s token.
fn m1b(h: &Harness, subject: &str, extra: &str) -> Fed {
    m1b_with(h, subject, extra, "")
}

/// [`m1b`], `a2a_extra` appended to its `a2a` section.
fn m1b_with(h: &Harness, subject: &str, extra: &str, a2a_extra: &str) -> Fed {
    let tools = if subject == "inbound" {
        "        tools_from: file\n        tools_file: \"__DIR__/tools.json\"\n"
    } else {
        ""
    };
    let s = fed(
        h,
        FedOpts {
            templates: vec![M1B_TEMPLATE],
            login: true,
            servers_extra: M1B
                .replace("__SUBJECT__", subject)
                .replace("__TOOLS__", tools),
            extra: format!("{M1B_WHO}{extra}"),
            a2a_extra: a2a_extra.to_string(),
            ..FedOpts::default()
        },
    );
    std::fs::write(
        s.tmp.path().join("tools.json"),
        json!({"tools": [{"name": "whoami", "description": "Who the token is for",
                          "inputSchema": {"type": "object"}}]})
        .to_string(),
    )
    .expect("write");
    // The MCP server's own authorization server is the person's IdP.
    s.xaa
        .control(json!({"prm_authorization_servers": [s.xaa.idp]}));
    s
}

/// What the MCP server's `whoami` answered the run with.
fn whoami_of(s: &Fed) -> Vec<Value> {
    s.xaa
        .mcp_calls("tools/call")
        .iter()
        .filter(|c| c["body"]["params"]["name"] == "whoami")
        .map(|c| c["authorization"].clone())
        .collect()
}

/// `subject: inbound`: the caller's own token is exchanged at the IdP that
/// issued it — and sent nowhere else — for one the pinned audience takes,
/// whose it is the person; the MCP server is sent only that. An exchange
/// the IdP refuses with `invalid_grant` is `subject_expired`.
pub(super) fn the_callers_token_is_exchanged_at_its_issuer(h: &Harness) -> Outcome {
    let s = m1b(h, "inbound", "");
    let d = s.daemon(h);
    let alice = s.token(ALICE, &["agent.run"]);
    let (status, _, r) = s.run_with(&alice, "m1b-who");
    let o = Outcome::require(status == 200, format!("the run starts: {status} {r}"));
    if !o.passed {
        return o;
    }
    let run = tryo!(s.ended("m1b-who", 0));
    let exchanges = s.xaa.grants("idp", EXCHANGE);
    let record = s.xaa.record();
    let carrying: Vec<&Value> = record
        .iter()
        .filter(|r| r.to_string().contains(&alice))
        .collect();
    let sent = whoami_of(&s);
    let at = sent
        .first()
        .and_then(|a| a.as_str())
        .and_then(|a| a.strip_prefix("Bearer "))
        .map(claims)
        .unwrap_or_default();
    let o = completed(&run)
        .and(|| {
            Outcome::require(
                exchanges.len() == 1 && {
                    let b = &exchanges[0]["body"];
                    b["subject_token"] == alice.as_str()
                        && b["subject_token_type"] == ACCESS_TOKEN
                        && b["requested_token_type"] == ACCESS_TOKEN
                        && b["audience"] == "api://issues"
                        && b["resource"] == s.xaa.mcp.as_str()
                        && b["scope"] == "issues:read"
                },
                format!("one on-behalf-of exchange of the caller's token: {exchanges:?}"),
            )
        })
        .and(|| {
            Outcome::require(
                carrying.len() == 1
                    && carrying[0]["at"] == "idp"
                    && carrying[0]["path"] == "/token",
                format!(
                    "the token goes to its issuer's token endpoint and nowhere else: {carrying:?}"
                ),
            )
        })
        .and(|| {
            Outcome::require(
                at["aud"] == "api://issues" && at["uid"] == ALICE,
                format!("the MCP server is sent the exchanged token, for the person: {sent:?}"),
            )
        });
    if !o.passed {
        return o;
    }
    // The IdP will not exchange it again: the caller's token is no longer
    // good there. (A restart empties the pool, so the run exchanges anew.)
    drop(d);
    s.xaa
        .control(json!({"exchange_error": {"error": "invalid_grant"}}));
    let _d = s.daemon(h);
    let fresh = s.token(ALICE, &["agent.run"]);
    let (_, _, _) = s.run_with(&fresh, "m1b-who");
    let gone = tryo!(s.ended("m1b-who", 0));
    failed_with(&gone, "subject_expired")
}

/// The exchanged token is held to the pinned audience: one the IdP issues
/// for another is refused, and never sent to the MCP server.
pub(super) fn the_exchanged_token_is_held_to_its_audience(h: &Harness) -> Outcome {
    let s = m1b(h, "inbound", "");
    s.xaa.control(json!({"m1b_aud": "api://elsewhere"}));
    let _d = s.daemon(h);
    let alice = s.token(ALICE, &["agent.run"]);
    let _ = s.run_with(&alice, "m1b-who");
    let run = tryo!(s.ended("m1b-who", 0));
    failed_with(&run, "config")
        .and(|| {
            Outcome::require(
                s.log_text().contains("not the pinned api://issues"),
                "the refusal names the pinned audience",
            )
        })
        .and(|| Outcome::require(whoami_of(&s).is_empty(), "nothing is sent the MCP server"))
}

/// `subject: grant`: the person's grant — kept by their sign-in — is
/// refreshed at the IdP for an access token for agentd, which is exchanged
/// there on their behalf for the pinned audience; the MCP server is sent
/// the result, for them.
pub(super) fn the_grants_token_is_exchanged_on_behalf_of_its_person(h: &Harness) -> Outcome {
    let s = m1b(h, "grant", "");
    let _d = s.daemon(h);
    let session = tryo!(s.login(ALICE));
    let run = tryo!(s.run_as(&session, "m1b-who"));
    let exchanges = s.xaa.grants("idp", EXCHANGE);
    let refreshed = s.xaa.grants("idp", "refresh_token");
    let sent = whoami_of(&s);
    let at = sent
        .last()
        .and_then(|a| a.as_str())
        .and_then(|a| a.strip_prefix("Bearer "))
        .map(claims)
        .unwrap_or_default();
    let last = exchanges
        .last()
        .map(|e| e["body"].clone())
        .unwrap_or_default();
    completed(&run)
        .and(|| {
            Outcome::require(
                !exchanges.is_empty()
                    && last["subject_token_type"] == ACCESS_TOKEN
                    && last["requested_token_type"] == ACCESS_TOKEN
                    && last["audience"] == "api://issues"
                    && last["subject_token"]
                        .as_str()
                        .is_some_and(|t| t.starts_with("idp-at-")),
                format!(
                    "the exchange presents agentd's access token from the grant: {exchanges:?}"
                ),
            )
        })
        .and(|| {
            Outcome::require(
                !refreshed.is_empty(),
                "the access token came from the grant's refresh token",
            )
        })
        .and(|| {
            Outcome::require(
                at["aud"] == "api://issues" && at["uid"] == ALICE,
                format!("the MCP server is sent the exchanged token, for the person: {sent:?}"),
            )
        })
}

// --- a webhook a person's token signs ---------------------------------------------

/// A webhook route a person's token signs — its sender's idempotency key in
/// the `Idempotency-Key` header, as by default: the deployment, the route's
/// audience and the webhook listener's port.
fn hooked(h: &Harness, a2a_extra: &str) -> (Fed, String, u16) {
    let wport = util::free_port();
    let aud = format!("http://127.0.0.1:{wport}/in");
    let flows = format!(
        "  - name: hooked\n    steps:\n      in: {{kind: webhook, path: /in, auth: {{jwt: {{issuer: \"__ISS__\", audience: \"{aud}\"}}}}}}\n      done: {{kind: finish, depends_on: [in]}}\nwebhooks:\n  listen: \"http://127.0.0.1:{wport}\"\n"
    );
    let s = fed(
        h,
        FedOpts {
            extra: flows,
            a2a_extra: a2a_extra.to_string(),
            ..FedOpts::default()
        },
    );
    (s, aud, wport)
}

/// POST a delivery to the webhook on `wport`, signed by `token`, its
/// sender's idempotency key `delivery`: the status and the body.
fn post_hook(wport: u16, token: &str, delivery: &str) -> (u16, String) {
    let auth = format!("Bearer {token}");
    let (status, _, body) = util::exchange(
        &format!("127.0.0.1:{wport}"),
        "POST",
        "/in",
        &[
            ("Authorization", &auth),
            ("Content-Type", "application/json"),
            ("Idempotency-Key", delivery),
        ],
        "{}",
    );
    (status, body)
}

/// `uid`'s short-lived token for the webhook audience `aud`.
fn hook_token(s: &Fed, aud: &str, uid: &str) -> String {
    let mut c = at_claims(&s.xaa, aud, uid, &["agent.run"]);
    c["exp"] = json!(now_s() + 120);
    s.xaa.mint(json!({}), &c)
}

/// A webhook start whose `auth.jwt` names the issuer: a person's short-lived
/// token for the webhook's own audience starts a run acting for them; the
/// same token presented again — its `jti` seen — is refused, and after a
/// restart, under another idempotency key of its sender's, it is answered
/// `duplicate` and starts nothing.
pub(super) fn a_webhook_token_is_never_taken_twice(h: &Harness) -> Outcome {
    let (s, aud, wport) = hooked(h, "");
    let d = s.daemon(h);
    let token = hook_token(&s, &aud, ALICE);
    let (first, _) = post_hook(wport, &token, "d-1");
    let run = tryo!(s.ended("hooked", 0));
    let (again, _) = post_hook(wport, &token, "d-2");
    // A token for the listener is no token for the webhook.
    let (listener, _) = post_hook(wport, &s.token(ALICE, &["agent.run"]), "d-3");
    std::thread::sleep(Duration::from_millis(500));
    let principal = format!("user:oidc:{}#{ALICE}", s.xaa.idp);
    let started = s.started("hooked");
    drop(d);
    let _d = s.daemon(h);
    let (replayed, said) = post_hook(wport, &token, "d-4");
    std::thread::sleep(Duration::from_millis(500));
    let after = s.started("hooked");
    Outcome::require(
        (200..300).contains(&first),
        format!("the first presentation starts a run: {first}"),
    )
    .and(|| completed(&run))
    .and(|| {
        Outcome::require(
            started.len() == 1 && started[0]["acting_for"] == principal.as_str(),
            format!("one run, acting for {principal}: {started:?}"),
        )
    })
    .and(|| {
        Outcome::require(
            !(200..300).contains(&again) && !(200..300).contains(&listener),
            format!("a replay and a listener's token are refused: {again} {listener}"),
        )
    })
    .and(|| {
        Outcome::require(
            said.contains("duplicate") && after.is_empty(),
            format!(
                "after a restart, under a new idempotency key, the token starts nothing: {replayed} {said} {after:?}"
            ),
        )
    })
}

/// The person a webhook's token names is the one the listener's own rules
/// name: a rule naming the issuer that admits nobody the token names admits
/// no delivery either. A delivery is kept in a webhook session of the
/// person's own, apart from the session their A2A calls keep, which keeps
/// its token's expiry.
pub(super) fn a_webhook_token_is_named_by_the_listeners_rules(h: &Harness) -> Outcome {
    let (s, aud, wport) = hooked(
        h,
        "  principals:\n    - {match: {token_iss: \"__ISS__\", sub: \"00u1*\"}, role: user, labels: {team: ops}}\n",
    );
    let _d = s.daemon(h);
    let alice = s.token(ALICE, &["agent.read", "agent.run"]);
    let (talked, _, _) = s.call_as(
        &alice,
        "SendMessage",
        util::text_params("hello", None, true),
    );
    let principal = format!("user:oidc:{}#{ALICE}", s.xaa.idp);
    let inbound = |s: &Fed| -> Vec<Value> {
        s.sessions()
            .into_iter()
            .filter(|x| x["kind"] == "inbound" && x["principal"] == principal.as_str())
            .collect()
    };
    let before = inbound(&s);
    let (alices, _) = post_hook(wport, &hook_token(&s, &aud, ALICE), "a-1");
    let run = tryo!(s.ended("hooked", 0));
    let (bobs, _) = post_hook(wport, &hook_token(&s, &aud, BOB), "b-1");
    std::thread::sleep(Duration::from_millis(500));
    let after = inbound(&s);
    let listener = before.first().cloned().unwrap_or_default();
    let kept = after.iter().find(|x| x["sid"] == listener["sid"]);
    Outcome::require(
        talked == 200 && before.len() == 1,
        format!("the A2A call keeps one session: {talked} {before:?}"),
    )
    .and(|| {
        Outcome::require(
            (200..300).contains(&alices) && completed(&run).passed,
            format!("the person the rule admits starts the run: {alices} {run}"),
        )
    })
    .and(|| {
        Outcome::require(
            after.len() == 2
                && kept.is_some_and(|k| k["expires_at"] == listener["expires_at"]),
            format!("the delivery's session is its own, and the A2A one is as it was: {before:?} {after:?}"),
        )
    })
    .and(|| {
        Outcome::require(
            !(200..300).contains(&bobs) && s.started("hooked").len() == 1,
            format!("a person no rule admits starts nothing: {bobs}"),
        )
    })
}

// --- the policies -----------------------------------------------------------------

/// `security.policies` reload whole or not at all: a list that does not
/// load refuses the reload and the old rule still holds; one that loads
/// replaces it from the next call.
pub(super) fn the_policies_reload_whole_or_not_at_all(h: &Harness) -> Outcome {
    let s = Setup::new(h, json!({}), &Opts::default());
    let with = |rules: &str| {
        let cfg = std::fs::read_to_string(&s.cfg).expect("read");
        let base = cfg
            .split_once("  policies:\n")
            .map(|(a, b)| {
                let rest: String = b
                    .lines()
                    .skip_while(|l| l.starts_with("    "))
                    .map(|l| format!("{l}\n"))
                    .collect();
                format!("{a}{rest}")
            })
            .unwrap_or(cfg);
        std::fs::write(
            &s.cfg,
            base.replace(
                "security:\n  experimental: {id_jag: true}\n",
                &format!("security:\n  experimental: {{id_jag: true}}\n  policies:\n{rules}"),
            ),
        )
        .expect("write");
    };
    with("    - {match: {tool: whoami}, action: deny}\n");
    let _d = s.daemon(h);
    tryo!(s.login(h));
    let pid = s
        .log_events()
        .iter()
        .find_map(|e| e["pid"].as_i64())
        .unwrap_or_default() as i32;
    let hup = |event: &str| -> Result<(), String> {
        let n = s.events(event).len();
        // SAFETY: a signal to the daemon this check started.
        unsafe {
            libc::kill(pid, libc::SIGHUP);
        }
        until(&format!("no {event}"), &|| s.log_text(), &|| {
            s.events(event).len() > n
        })
    };
    let denied = tryo!(s.run("who"));
    with(
        "    - {match: {tool: whoami}, action: deny}\n    - {match: {tool: whoami}, action: maybe}\n",
    );
    tryo!(hup("config.reload.invalid"));
    let still = tryo!(s.run("who"));
    with("    - {match: {tool: create_issue}, action: deny}\n");
    tryo!(hup("config.reloaded"));
    let reloaded = s.events("security.policies.reloaded");
    let open = tryo!(s.run("who"));
    let write = tryo!(s.run("write"));
    let by_rule = |run: &Value| {
        Outcome::require(
            run["status"] == "failed"
                && run["err"]
                    .as_str()
                    .is_some_and(|e| e.contains("denied by security.policies[0]")),
            format!("the run is denied by the rule: {run}"),
        )
    };
    by_rule(&denied)
        .and(|| by_rule(&still))
        .and(|| {
            Outcome::require(
                reloaded.iter().any(|e| e["rules"] == 1),
                format!("the swap is logged: {reloaded:?}"),
            )
        })
        .and(|| completed(&open))
        .and(|| by_rule(&write))
}

/// A run that writes twice with a signal between: each write's first use
/// in the run is the person's to approve.
const HELD: &str = "  - name: held\n    steps:\n      start: {kind: manual}\n      one: {kind: mcp.tool, depends_on: [start], server: linear, tool: create_issue, args: {title: one}}\n      w: {kind: wait, depends_on: [one], on: signal, signal: go}\n      two: {kind: mcp.tool, depends_on: [w], server: linear, tool: create_issue, args: {title: two}}\n      done: {kind: finish, depends_on: [two]}\n";

/// The deployment [`HELD`] runs in, its template's `ttl` as given.
fn held_opts(ttl: Option<&'static str>) -> Opts {
    Opts {
        tag_map: Some("{whoami: [], create_issue: [egress], list_issues: [untrusted_input]}"),
        trusted_output: true,
        consent_timeout: Some("8s"),
        ttl,
        extra: HELD.to_string(),
        ..Opts::default()
    }
}

/// Start [`HELD`], approve its first write's gate, and wait for the run to
/// park on its signal: how many runs had ended before, or why not.
fn held_and_approved(s: &Setup, h: &Harness) -> Result<usize, String> {
    s.login(h)?;
    let n = s.start("held");
    let deadline = Instant::now() + Duration::from_secs(20);
    let gate = loop {
        let l: Value =
            serde_json::from_str(&s.cli(h, &["--delegation-list"]).stdout).unwrap_or_default();
        if let Some(g) = l["gates"].as_array().and_then(|g| g.first()) {
            break g.clone();
        }
        if Instant::now() > deadline {
            return Err(format!("no gate opened:\n{}", tail(&s.log_text())));
        }
        std::thread::sleep(Duration::from_millis(200));
    };
    let ok = s.cli(
        h,
        &[
            "--delegation-approve",
            gate["gate"].as_str().unwrap_or_default(),
        ],
    );
    if ok.code != Some(0) {
        return Err(format!("approve: {} {}", ok.stdout, ok.stderr));
    }
    until("the run never parked", &|| s.log_text(), &|| {
        s.events("step.start")
            .iter()
            .any(|e| e["step"] == "w" && e["kind"] == "wait")
    })?;
    std::thread::sleep(Duration::from_millis(500));
    Ok(n)
}

/// [`HELD`] approved and parked, the daemon restarted onto its template's
/// `ttl` as `then` says and the signal sent: how the run ended, how many
/// gates the restarted daemon asked, and how many writes the server was
/// sent.
fn held_across_a_restart(
    h: &Harness,
    then: Option<&'static str>,
) -> Result<(Value, usize, usize), String> {
    let s = Setup::new(h, json!({}), &held_opts(None));
    let d = s.daemon(h);
    let n = held_and_approved(&s, h)?;
    drop(d);
    s.reconfigure(&held_opts(then));
    let _d = s.daemon(h);
    std::thread::sleep(Duration::from_millis(500));
    let fired = s.op("workflow.signal", json!({"name": "go"}));
    if fired.get("error").is_some() {
        return Err(format!("signal: {fired}"));
    }
    let run = s.ended("held", n, Duration::from_secs(30))?;
    let asked = s
        .events("delegation.consent")
        .iter()
        .filter(|e| e["decision"] == "asked")
        .count();
    let written = s
        .xaa
        .mcp_calls("tools/call")
        .iter()
        .filter(|c| c["body"]["params"]["name"] == "create_issue")
        .count();
    Ok((run, asked, written))
}

/// A first-use approval is kept with the run: the run's second use of the
/// tool, after a restart, asks nothing.
pub(super) fn a_first_use_approval_outlives_a_restart(h: &Harness) -> Outcome {
    let (run, asked, written) = tryo!(held_across_a_restart(h, None));
    completed(&run)
        .and(|| Outcome::require(written == 2, format!("both writes are made: {written}")))
        .and(|| {
            Outcome::require(
                asked == 0,
                format!("the restarted daemon asks nothing for the run's second use: {asked}"),
            )
        })
}

/// A first-use approval is bound to the templates the work acts under: a
/// restart onto a changed template — its `ttl` another — forgets it, and
/// the run's second use of the tool asks again.
pub(super) fn a_first_use_approval_ends_with_its_template(h: &Harness) -> Outcome {
    let (run, asked, written) = tryo!(held_across_a_restart(h, Some("12h")));
    Outcome::require(
        asked == 1,
        format!("under the changed template the second use asks again: {asked}"),
    )
    .and(|| {
        Outcome::require(
            written == 1 && run["status"] != "completed",
            format!("unanswered, the second write is not made: {written} {run}"),
        )
    })
}
