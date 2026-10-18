// SPDX-License-Identifier: AGPL-3.0-only
//! agentd itself as the Personal Agent (rfcs/0047 §10): the real binary,
//! built with `companies`, declared a Company — the fake — and driven only
//! through its configuration. Its client identity is what
//! `--print-company-client` writes, hosted by the [`probe`]'s document
//! server. Each check reads what agentd did from the fake's own record of
//! what reached it, never from agentd, and quotes the sentence of the draft
//! it holds agentd to ([`QUOTES`], verified verbatim in their sections like
//! the fake's own checks).

use std::time::{Duration, Instant};

use serde_json::Value;

use super::companies::{Page, Quote};
use crate::checks::probe::{Agent, Company};
use crate::harness::{Daemon, TempDir, tls_fixture};
use crate::{Category, Check, Harness, Outcome};

pub fn checks() -> Vec<Check> {
    let check = |id, desc, run| Check {
        id,
        category: Category::Companies,
        desc,
        run,
    };
    vec![
        check(
            "companies/agent-refuses-a-company-that-fails-discovery",
            "agentd sends no assertion, no proof and no token request to a Company whose metadata names another issuer, lists no domains, leaves out ES256, names off-host endpoints, or whose discovery document redirects to plain HTTP",
            refuses_a_failing_company,
        ),
        check(
            "companies/agent-publishes-its-client-id",
            "the client metadata document agentd prints is the one at its client_id, and the fake fetched it, held it to the kit's schema and accepted agentd's assertions against its key set",
            publishes_its_client_id,
        ),
        check(
            "companies/agent-assertions-name-the-issuer",
            "every client assertion and Session assertion agentd sends has aud the issuer as one string, iss its client_id, a minute's life and a 128-bit jti; the client assertion's sub is the client_id, the Session assertion's a User ID that names nothing about the person",
            assertions_name_the_issuer,
        ),
        check(
            "companies/agent-signs-a-proof-for-every-token-request",
            "every token request but the Bearer one carries a DPoP Proof the fake validated (htm POST, htu the token endpoint, no ath), each with its own jti",
            signs_a_proof_for_every_token_request,
        ),
        check(
            "companies/agent-uses-bearer-only-at-its-mcp-server",
            "agentd requests the MCP server's token without a proof, with resource the server's url and its session_id, and presents it at that server only, never a superseded one",
            uses_bearer_only_at_its_mcp_server,
        ),
        check(
            "companies/agent-supports-nonces",
            "against the nonce-required variant, agentd's use_dpop_nonce is followed by one retry carrying the nonce, with fresh assertions and a fresh proof",
            supports_nonces,
        ),
        check(
            "companies/agent-keeps-the-user-across-restarts",
            "after a restart agentd presents the same User ID and renews the same session_id, with a new DPoP key",
            keeps_the_user_across_restarts,
        ),
    ]
}

const fn spec(check: &'static str, section: &'static str, text: &'static str) -> Quote {
    Quote {
        check,
        page: Page::Spec,
        section,
        text,
    }
}

/// What each check holds agentd to, in the draft's words.
pub const QUOTES: &[Quote] = &[
    spec(
        "companies/agent-refuses-a-company-that-fails-discovery",
        "3.2",
        "Before using a Company's `poppy.json`, the Personal Agent fetches the metadata and MUST check that its `issuer` exactly matches `auth.issuer` and that `poppy_domains` includes `organization.domain`, ignoring case.",
    ),
    spec(
        "companies/agent-refuses-a-company-that-fails-discovery",
        "3.2",
        "Personal Agents MUST sign DPoP Proofs with an algorithm from it.",
    ),
    spec(
        "companies/agent-refuses-a-company-that-fails-discovery",
        "3",
        "Personal Agents MUST follow redirects, and every redirect MUST be to an HTTPS URL.",
    ),
    spec(
        "companies/agent-publishes-its-client-id",
        "4.1",
        "A Personal Agent's `client_id` MUST be an HTTPS URL that returns its metadata document.",
    ),
    spec(
        "companies/agent-publishes-its-client-id",
        "4.1",
        "`client_id` MUST equal the URL the document was fetched from.",
    ),
    spec(
        "companies/agent-assertions-name-the-issuer",
        "4.1",
        "The client assertion's `aud` MUST be the Company's `auth.issuer`, as a single string.",
    ),
    spec(
        "companies/agent-assertions-name-the-issuer",
        "4.2",
        "`aud` is the Company's `auth.issuer`, as a single string rather than an array, so the assertion can't be used at another Company.",
    ),
    spec(
        "companies/agent-assertions-name-the-issuer",
        "4.2",
        "It MUST NOT contain or be derived from personal information, such as an email address, phone number, or name, even with a keyed hash.",
    ),
    spec(
        "companies/agent-signs-a-proof-for-every-token-request",
        "4.3",
        "It keeps the private key to itself and signs a new proof for every request, including token requests.",
    ),
    spec(
        "companies/agent-signs-a-proof-for-every-token-request",
        "4.3",
        "Proofs sent to the token endpoint don't have one.",
    ),
    spec(
        "companies/agent-uses-bearer-only-at-its-mcp-server",
        "4.3",
        "To get one for an existing Session, the Personal Agent sends the request that renews the Session's token (4.2, or 4.8 for a signed-in Session) without the `DPoP` header, and MUST include `resource` set to the MCP server's `url` from `poppy.json`, as MCP authorization requires.",
    ),
    spec(
        "companies/agent-uses-bearer-only-at-its-mcp-server",
        "4.3",
        "Whenever the Personal Agent gets a new Session Token for a Session, it MUST use the new token from then on.",
    ),
    spec(
        "companies/agent-supports-nonces",
        "4.3",
        "Personal Agents MUST support nonces.",
    ),
    spec(
        "companies/agent-keeps-the-user-across-restarts",
        "4.2",
        "The ID is stable, so the Company sees the same User across Sessions.",
    ),
];

fn verdict(r: Result<(), String>) -> Outcome {
    match r {
        Ok(()) => Outcome::pass(),
        Err(why) => Outcome::fail(why),
    }
}

fn need(cond: bool, why: impl Into<String>) -> Result<(), String> {
    if cond { Ok(()) } else { Err(why.into()) }
}

/// agentd declared the fake as its Company, its identity hosted by the
/// probe's document server.
struct Run {
    company: Company,
    _host: Agent,
    tmp: TempDir,
    cfg: String,
    daemon: Option<Daemon>,
}

impl Run {
    /// What reached the fake, its steering and reading aside.
    fn record(&self) -> Vec<Value> {
        self.company
            .record()
            .into_iter()
            .filter(|r| {
                !r["path"]
                    .as_str()
                    .unwrap_or_default()
                    .starts_with("/_mock/")
            })
            .collect()
    }

    fn tokens(&self) -> Vec<Value> {
        self.record()
            .into_iter()
            .filter(|r| r["path"] == "/auth/token")
            .collect()
    }

    /// Start the daemon, and wait until it lists the Company's tools.
    fn start(&mut self, h: &Harness) -> Result<(), String> {
        let lists = |r: &Run| {
            r.record()
                .iter()
                .filter(|r| r["mcp"] == "tools/list")
                .count()
        };
        let before = lists(self);
        self.daemon = Some(h.spawn(&["--config", &self.cfg]));
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            if lists(self) > before {
                return Ok(());
            }
            if Instant::now() > deadline {
                return Err(format!(
                    "agentd never listed the Company's tools: {:?}",
                    self.record()
                ));
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    fn stop(&mut self) {
        self.daemon = None;
    }
}

/// The fake (with `variants`), agentd's configuration for it, and its
/// identity printed and hosted.
fn prepare(h: &Harness, variants: &[&str]) -> Result<Run, String> {
    let tmp = h.tempdir();
    let company = Company::spawn(h, &tmp, variants);
    let host = Agent::start();
    let dir = tmp.path().display().to_string();
    let cfg = format!("{dir}/agent.yaml");
    std::fs::write(
        &cfg,
        format!(
            "agent: {{name: conformance-pa, instruction: Shop., prompt: Wait., preflight: never}}\n\
             intelligence: {{endpoints: \"mock:final\", model: mock}}\n\
             store: {{kind: file, file: {{path: \"{dir}/state\"}}}}\n\
             lifecycle: {{run_until: drained}}\n\
             observability: {{log_level: warn}}\n\
             security:\n  tls_ca: \"{ca}\"\n  companies:\n    client: {{id: \"{o}/companies/client.json\", name: \"agentd conformance\", key_file: \"{dir}/client-key\"}}\n\
             services:\n  shop: {{kind: company, endpoint: \"{c}\", allow_private: true}}\n",
            ca = tls_fixture("ca.pem").display(),
            o = host.origin,
            c = company.origin,
        ),
    )
    .map_err(|e| e.to_string())?;
    let out = h.run(&[
        "--config",
        &cfg,
        "--print-company-client",
        &format!("{dir}/published"),
    ]);
    if out.code != Some(0) {
        return Err(format!("--print-company-client: {}", out.stderr));
    }
    for (file, path) in [
        ("client.json", "/companies/client.json"),
        ("jwks.json", "/companies/jwks.json"),
    ] {
        let doc: Value = serde_json::from_slice(
            &std::fs::read(format!("{dir}/published/{file}")).map_err(|e| e.to_string())?,
        )
        .map_err(|e| e.to_string())?;
        host.serve(path, doc);
    }
    Ok(Run {
        company,
        _host: host,
        tmp,
        cfg,
        daemon: None,
    })
}

fn joined(h: &Harness) -> Result<Run, String> {
    let mut run = prepare(h, &[])?;
    run.start(h)?;
    Ok(run)
}

fn refuses_a_failing_company(h: &Harness) -> Outcome {
    verdict((|| {
        for variant in [
            "issuer-mismatch",
            "no-domains",
            "no-es256",
            "off-host-endpoints",
            "redirect-to-http",
        ] {
            let mut run = prepare(h, &[variant])?;
            run.daemon = Some(h.spawn(&["--config", &run.cfg]));
            // Long enough for discovery and the join's first retries.
            let deadline = Instant::now() + Duration::from_secs(3);
            let mut asked_discovery = false;
            while Instant::now() < deadline {
                let record = run.record();
                asked_discovery |= record
                    .iter()
                    .any(|r| r["path"] == "/.well-known/poppy.json");
                if let Some(t) = record.iter().find(|r| {
                    r["path"] == "/auth/token" || r["path"] == "/mcp" || r["path"] == "/auth/device"
                }) {
                    return Err(format!("{variant}: agentd used the Company anyway: {t}"));
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            need(
                asked_discovery,
                format!("{variant}: agentd never read the discovery document"),
            )?;
            run.stop();
        }
        Ok(())
    })())
}

fn publishes_its_client_id(h: &Harness) -> Outcome {
    verdict((|| {
        let run = joined(h)?;
        let printed: Value = serde_json::from_slice(
            &std::fs::read(run.tmp.path().join("published/client.json"))
                .map_err(|e| e.to_string())?,
        )
        .map_err(|e| e.to_string())?;
        let client_id = printed["client_id"]
            .as_str()
            .unwrap_or_default()
            .to_string();
        need(
            client_id.starts_with("https://") && client_id.ends_with("/companies/client.json"),
            format!("the client_id is not the HTTPS URL of its document: {client_id}"),
        )?;
        let record = run.record();
        need(
            record.iter().any(|r| {
                r["validated"]
                    .as_array()
                    .is_some_and(|v| v.iter().any(|s| s == "client-metadata.schema.json"))
            }),
            "the fake never fetched and validated the client metadata document",
        )?;
        let toks = run.tokens();
        need(
            !toks.is_empty() && toks.iter().all(|t| t["status"] == 200),
            format!("the fake did not accept every token request: {toks:?}"),
        )?;
        need(
            toks.iter()
                .all(|t| t["client_assertion"]["iss"] == client_id.as_str()),
            "a client assertion names another client_id",
        )
    })())
}

fn assertions_name_the_issuer(h: &Harness) -> Outcome {
    verdict((|| {
        let run = joined(h)?;
        let issuer = format!("{}/auth", run.company.origin);
        for t in run.tokens() {
            for key in ["client_assertion", "assertion"] {
                let a = &t[key];
                need(
                    a["aud"] == issuer.as_str(),
                    format!("{key} aud is not the issuer as one string: {}", a["aud"]),
                )?;
                let (iat, exp) = (
                    a["iat"].as_u64().unwrap_or(0),
                    a["exp"].as_u64().unwrap_or(0),
                );
                need(
                    exp > iat && exp - iat <= 60,
                    format!("{key} lives {}s", exp.saturating_sub(iat)),
                )?;
                need(
                    a["jti"].as_str().is_some_and(|j| j.len() >= 22),
                    format!("{key} jti has fewer than 128 bits: {}", a["jti"]),
                )?;
            }
            need(
                t["client_assertion"]["sub"] == t["client_assertion"]["iss"],
                "the client assertion's sub is not its client_id",
            )?;
            let sub = t["assertion"]["sub"].as_str().unwrap_or_default();
            need(
                sub.starts_with("usr_")
                    && !sub.contains("operator")
                    && !sub.contains("conformance"),
                format!("the User ID {sub:?} is not opaque"),
            )?;
        }
        Ok(())
    })())
}

fn signs_a_proof_for_every_token_request(h: &Harness) -> Outcome {
    verdict((|| {
        let run = joined(h)?;
        let at = format!("{}/auth/token", run.company.origin);
        let mut jtis = Vec::new();
        let toks = run.tokens();
        for t in toks.iter().filter(|t| t["resource"].is_null()) {
            let p = &t["proof"];
            need(
                p.is_object(),
                format!("a token request without a proof: {t}"),
            )?;
            need(
                p["htm"] == "POST" && p["htu"] == at.as_str(),
                format!("the proof's htm/htu: {p}"),
            )?;
            need(
                p.get("ath").is_none(),
                "a proof to the token endpoint carries ath",
            )?;
            need(
                t["validated"]
                    .as_array()
                    .is_some_and(|v| v.iter().any(|s| s == "dpop-proof.schema.json")),
                "a proof the fake did not hold to the kit",
            )?;
            jtis.push(p["jti"].clone());
        }
        need(!jtis.is_empty(), "no token request with a proof")?;
        let mut unique: Vec<String> = jtis.iter().map(Value::to_string).collect();
        unique.sort();
        unique.dedup();
        need(unique.len() == jtis.len(), "a proof's jti was used twice")
    })())
}

fn uses_bearer_only_at_its_mcp_server(h: &Harness) -> Outcome {
    verdict((|| {
        let run = joined(h)?;
        let mcp = format!("{}/mcp", run.company.origin);
        let record = run.record();
        let bearer = record
            .iter()
            .find(|r| r["path"] == "/auth/token" && r["resource"] == mcp.as_str())
            .ok_or("no token request for the MCP server")?;
        need(
            bearer["dpop"] == false,
            "the MCP server's token was requested with a proof",
        )?;
        need(
            bearer["session_id"].is_string(),
            "the Bearer request names no session_id",
        )?;
        let token = bearer["issued"]["access_token"]
            .as_str()
            .ok_or("no Bearer token issued")?;
        let mark = {
            let mut s =
                crate::checks::probe::b64url(&crate::checks::probe::sha256(token.as_bytes()));
            s.truncate(8);
            s
        };
        let presented: Vec<&Value> = record
            .iter()
            .filter(|r| r["presented"] == mark.as_str())
            .collect();
        need(
            !presented.is_empty(),
            "the Bearer token was never presented",
        )?;
        need(
            presented.iter().all(|r| r["path"] == "/mcp"),
            "the Bearer token was presented away from its MCP server",
        )?;
        need(
            record.iter().all(|r| r["superseded"] != true),
            "agentd presented a token after it got a newer one",
        )
    })())
}

fn supports_nonces(h: &Harness) -> Outcome {
    verdict((|| {
        let mut run = prepare(h, &["nonce-required"])?;
        run.start(h)?;
        let toks = run.tokens();
        let first = toks.first().ok_or("no token request")?;
        need(
            first["error"] == "use_dpop_nonce",
            format!("the first proof was not asked for a nonce: {first}"),
        )?;
        let retry = toks.get(1).ok_or("no retry")?;
        need(retry["status"] == 200, format!("the retry: {retry}"))?;
        need(
            retry["proof"]["nonce"].is_string(),
            "the retry carries no nonce",
        )?;
        for key in ["client_assertion", "assertion", "proof"] {
            need(
                first[key]["jti"] != retry[key]["jti"],
                format!("the retry reused the {key}'s jti"),
            )?;
        }
        need(
            toks.iter()
                .filter(|t| t["error"] == "use_dpop_nonce")
                .count()
                == 1,
            "use_dpop_nonce was answered more than once",
        )
    })())
}

fn keeps_the_user_across_restarts(h: &Harness) -> Outcome {
    verdict((|| {
        let mut run = joined(h)?;
        run.stop();
        let first = run.tokens();
        run.start(h)?;
        let all = run.tokens();
        let second = &all[first.len()..];
        let started = first
            .iter()
            .find(|t| t["dpop"] == true)
            .ok_or("no Session started")?;
        let sid = first
            .iter()
            .find(|t| t["resource"].is_string())
            .map(|t| t["session_id"].clone())
            .ok_or("no Session id")?;
        let renewed = second
            .iter()
            .find(|t| t["dpop"] == true)
            .ok_or("no Session renewed after the restart")?;
        need(
            renewed["assertion"]["sub"] == started["assertion"]["sub"],
            "another User ID after the restart",
        )?;
        need(
            renewed["session_id"] == sid,
            "another Session after the restart",
        )?;
        need(
            renewed["jkt"] != started["jkt"],
            "the same DPoP key after the restart",
        )
    })())
}
