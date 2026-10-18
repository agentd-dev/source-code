// SPDX-License-Identifier: AGPL-3.0-only
//! The Personal Agent Protocol draft pinned at `95fcbf2` (rfcs/0047). agentd
//! is a Personal Agent under it, and nothing published tests a Personal
//! Agent, so agentd tests against its own fake Company
//! (`agentd --internal-mock-company`). These checks hold that fake to the
//! draft before anything is held to it: a [`probe`] Personal Agent, written
//! from the draft, drives the real binary's fake, and each check passes
//! only when the conforming fake does what the draft says **and** the
//! variant built to break that rule is caught breaking it.
//!
//! [`super::companies_agent`] then holds agentd itself to the draft, as the
//! Personal Agent, against the same fake.
//!
//! Every check quotes the draft sentence it tests ([`QUOTES`]), as the
//! draft's own Company validator does, and a unit test requires each quote
//! to appear verbatim, in its section, in the pages vendored at the pin
//! (`crates/agentd/tests/companies/vendor/`). A re-pin that rewords a
//! requirement fails until the check that tests it is reread.

use serde_json::{Value, json};

use crate::checks::probe::{
    self, Agent, Company, DEVICE_CODE, Discovered, JWT_BEARER, PREFIXED_ALGS, Refused, Resp,
    b64url, discover, post_form, random_id, request, sha256, well_known,
};
use crate::harness::TempDir;
use crate::{Category, Check, Harness, Outcome};

pub fn checks() -> Vec<Check> {
    let check = |id, desc, run| Check {
        id,
        category: Category::Companies,
        desc,
        run,
    };
    let mut checks = vec![
        check(
            "companies/fake-issuer-matches-its-metadata",
            "the fake's RFC 8414 metadata, at the well-known URL with the suffix inserted before the issuer's path, names exactly the discovery document's issuer; the issuer-mismatch variant is refused for it",
            issuer_matches,
        ),
        check(
            "companies/fake-lists-its-domain",
            "the metadata's poppy_domains lists the discovery document's organization.domain, the host that served it; the no-domains variant is refused for it",
            lists_its_domain,
        ),
        check(
            "companies/fake-publishes-es256",
            "the metadata's dpop_signing_alg_values_supported includes ES256; the no-es256 variant is refused, and the prefixed-algs variant is refused naming the prefixed member",
            publishes_es256,
        ),
        check(
            "companies/fake-endpoints-on-its-host",
            "every URL the two documents name is on the Company's host; the off-host-endpoints variant is refused naming the field",
            endpoints_on_its_host,
        ),
        check(
            "companies/fake-redirects-only-to-https",
            "the redirect-to-http variant's discovery document redirects to plain HTTP, and a Personal Agent that follows only HTTPS redirects refuses it",
            redirects_only_to_https,
        ),
        check(
            "companies/fake-starts-and-renews-sessions",
            "the JWT bearer grant with a Session assertion, a client assertion and a proof starts a signed-out Session (DPoP token, scope \"\", no-store); session_id renews it, asking it for a scope is invalid_scope, another User's assertion for it is invalid_session, and the record marks a use of the replaced token superseded",
            starts_and_renews_sessions,
        ),
        check(
            "companies/fake-rejects-another-aud",
            "a client assertion whose aud is the token endpoint or a list is invalid_client, and a Session assertion whose aud is the token endpoint is invalid_grant",
            rejects_another_aud,
        ),
        check(
            "companies/fake-rejects-a-reused-jti",
            "a Session assertion sent twice is invalid_grant the second time, and a client assertion sent twice is invalid_client",
            rejects_a_reused_jti,
        ),
        check(
            "companies/fake-checks-the-assertion-claims",
            "a client assertion whose iss or sub is not the client_id is invalid_client, a Session assertion whose iss is not it is invalid_grant, and either assertion expired, issued in the future, living ten minutes, or with exp not after iat is refused the same way",
            checks_the_assertion_claims,
        ),
        check(
            "companies/fake-checks-the-client-document",
            "a client metadata document whose client_id is not its URL, whose jwks_uri is on another host, or whose key set holds a private key is invalid_client",
            checks_the_client_document,
        ),
        check(
            "companies/fake-validates-every-proof",
            "at the token endpoint, no proof without an MCP resource, two DPoP headers, the wrong htm or htu, an iat ten minutes old or ahead, typ JWT, alg none, HS256 or one the Company does not publish, an ath, a private key in the jwk, a signature by another key and a replayed proof are each invalid_dpop_proof",
            validates_every_proof,
        ),
        check(
            "companies/fake-binds-tokens-to-their-key",
            "the API serves a DPoP Session Token with its proof; no proof, a missing or wrong ath, or a proof by another key is invalid_dpop_proof, the Bearer scheme is invalid_token, and a token in the URL is refused",
            binds_tokens_to_their_key,
        ),
        check(
            "companies/fake-asks-for-a-nonce",
            "the nonce-required variant answers a proof without its nonce with use_dpop_nonce and a DPoP-Nonce header, at the token endpoint (400) and the API (401), and takes the retry that carries it",
            asks_for_a_nonce,
        ),
        check(
            "companies/fake-mcp-takes-bearer-for-its-url",
            "a token request without a proof, with resource the MCP server's url, gets a Bearer token the server takes; unauthenticated, the server challenges with its RFC 9728 metadata, which lists the issuer; a DPoP token there, or a Bearer token from another MCP server, is invalid_token",
            mcp_takes_bearer_for_its_url,
        ),
        check(
            "companies/fake-mcp-is-2025-06-18-or-later",
            "the fake's MCP server answers server/discover and initialize; the mcp-2025-06-18 variant answers only initialize, at 2025-06-18",
            mcp_is_2025_06_18_or_later,
        ),
        check(
            "companies/fake-device-sign-in",
            "device authorization needs a scope it can grant (else invalid_scope); polls are authorization_pending until the scripted User approves, then an Account Token and a signed-in Session Token; a poll sooner than the interval is slow_down; one asking for more than the User approved is invalid_scope, and the next gets what was approved; a decline is access_denied; the off-host-verification-uri variant sends the User to another host",
            device_sign_in,
        ),
        check(
            "companies/fake-direct-sign-in",
            "the authorization endpoint redirects to an exactly registered redirect_uri with a code, the state and iss, and to no unregistered one; the code exchange with its PKCE verifier and session_id signs the Session in, once, and one asking for a scope the sign-in did not grant is invalid_scope",
            direct_sign_in,
        ),
        check(
            "companies/fake-account-tokens-rotate-when-it-says",
            "an Account Token gets signed-in Session Tokens without a new one; with the rotation variant each use returns a new Account Token and the spent one is invalid_grant",
            account_tokens_rotate,
        ),
        check(
            "companies/fake-revocation-signs-out",
            "RFC 7009 revocation of an Account Token makes it invalid_grant and signs its Session out at once, and the Session renews signed out; under rotation, revoking the current token signs out every Session of the sign-in, and the record calls a spent token's revocation spent",
            revocation_signs_out,
        ),
        check(
            "companies/fake-keeps-a-session-to-one-account",
            "a Session once signed in to one account answers a sign-in to another with account_mismatch and issues no tokens",
            one_account_per_session,
        ),
        check(
            "companies/fake-rate-limits-with-retry-after",
            "the retry-after variant answers 429 rate_limited with Retry-After, answers 429 to a request sent before the wait is over, and serves one sent after it",
            rate_limits_with_retry_after,
        ),
        check(
            "companies/fake-mcp-account-errors",
            "an MCP tool that needs the account answers a signed-out Bearer token with 403 sign_in_required, and a signed-in one without poppy:read with 403 insufficient_scope naming it",
            mcp_account_errors,
        ),
    ];
    // And agentd itself held to the draft, as the Personal Agent.
    checks.extend(super::companies_agent::checks());
    checks
}

/// A vendored page of the draft.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Page {
    Spec,
    Operations,
    Guides,
}

impl Page {
    pub fn file(self) -> &'static str {
        match self {
            Page::Spec => "spec.md",
            Page::Operations => "operations.md",
            Page::Guides => "guides.md",
        }
    }
}

/// A sentence of the draft a check tests: its page, the section (a numbered
/// heading's number, or a heading's text) and the words, verbatim.
pub struct Quote {
    pub check: &'static str,
    pub page: Page,
    pub section: &'static str,
    pub text: &'static str,
}

const fn spec(check: &'static str, section: &'static str, text: &'static str) -> Quote {
    Quote {
        check,
        page: Page::Spec,
        section,
        text,
    }
}

/// What each check tests, in the draft's words.
pub const QUOTES: &[Quote] = &[
    spec(
        "companies/fake-issuer-matches-its-metadata",
        "3.2",
        "Before using a Company's `poppy.json`, the Personal Agent fetches the metadata and MUST check that its `issuer` exactly matches `auth.issuer`",
    ),
    spec(
        "companies/fake-issuer-matches-its-metadata",
        "3.2",
        "The issuer MUST publish OAuth Authorization Server Metadata ([RFC 8414](https://www.rfc-editor.org/rfc/rfc8414))",
    ),
    spec(
        "companies/fake-lists-its-domain",
        "3.2",
        "and `poppy_domains` in it MUST list every domain whose `poppy.json` names the issuer.",
    ),
    spec(
        "companies/fake-lists-its-domain",
        "3.1",
        "`domain` MUST be the host that serves the document, after any redirects.",
    ),
    spec(
        "companies/fake-publishes-es256",
        "3.2",
        "Companies MUST publish it, and it MUST include `ES256`.",
    ),
    spec(
        "companies/fake-endpoints-on-its-host",
        "8.4",
        "each Company's metadata names its own endpoints, which nobody checks (3.2).",
    ),
    spec(
        "companies/fake-redirects-only-to-https",
        "3",
        "Personal Agents MUST follow redirects, and every redirect MUST be to an HTTPS URL.",
    ),
    spec(
        "companies/fake-starts-and-renews-sessions",
        "4.2",
        "The Company verifies both assertions against the Personal Agent's `jwks_uri`, checks that the assertion's `iss` is the authenticated `client_id`, rejects a reused `jti`, and returns a new Session:",
    ),
    spec(
        "companies/fake-starts-and-renews-sessions",
        "4.2",
        "The Company checks that the Session belongs to the same `client_id` and User ID as the assertion, and returns a new signed-out Session Token for it.",
    ),
    spec(
        "companies/fake-starts-and-renews-sessions",
        "4.3",
        "Whenever the Personal Agent gets a new Session Token for a Session, it MUST use the new token from then on.",
    ),
    spec(
        "companies/fake-rejects-another-aud",
        "4.1",
        "Companies MUST reject a client assertion with any other `aud`, including their token endpoint URL.",
    ),
    spec(
        "companies/fake-rejects-another-aud",
        "4.2",
        "As with the client assertion (4.1), the Company MUST reject any other `aud`.",
    ),
    spec(
        "companies/fake-rejects-a-reused-jti",
        "4.2",
        "The Company rejects one it has already seen, so a copied assertion can't be used again.",
    ),
    spec(
        "companies/fake-checks-the-assertion-claims",
        "4.1",
        "The Personal Agent authenticates at the token endpoint with a client assertion signed by a key from `jwks_uri` (`private_key_jwt`, [RFC 7523](https://www.rfc-editor.org/rfc/rfc7523)).",
    ),
    spec(
        "companies/fake-checks-the-assertion-claims",
        "4.2",
        "The assertion is a short-lived JWT signed with a key from its `jwks_uri`, naming itself in `iss` and the User in `sub`.",
    ),
    spec(
        "companies/fake-checks-the-assertion-claims",
        "4.2",
        "`iat` and `exp` are when the assertion was issued and when it expires, in seconds since 1970 (Unix time). The assertion is short-lived: one minute in this example.",
    ),
    spec(
        "companies/fake-checks-the-client-document",
        "4.1",
        "`client_id` MUST equal the URL the document was fetched from.",
    ),
    spec(
        "companies/fake-checks-the-client-document",
        "4.1",
        "`jwks_uri` and every entry in `redirect_uris` MUST be HTTPS URLs on the same domain as `client_id`. The key set contains only public keys.",
    ),
    spec(
        "companies/fake-validates-every-proof",
        "4.3",
        "The Company MUST validate every proof as RFC 9449 §4.3 describes.",
    ),
    spec(
        "companies/fake-validates-every-proof",
        "4.3",
        "`typ` is `dpop+jwt`, `alg` is an asymmetric algorithm the Company accepts (3.2), never `none`, and `jwk` holds a public key and no private key.",
    ),
    spec(
        "companies/fake-validates-every-proof",
        "4.3",
        "At the token endpoint, the Company MUST reject a proof whose `jti` it has already accepted within the acceptance window, because a replayed proof there could get a new token.",
    ),
    spec(
        "companies/fake-validates-every-proof",
        "4.3",
        "Proofs sent to the token endpoint don't have one.",
    ),
    spec(
        "companies/fake-binds-tokens-to-their-key",
        "4.3",
        "On requests with a Session Token, the key is the one the token is bound to, and `ath` matches the token.",
    ),
    spec(
        "companies/fake-binds-tokens-to-their-key",
        "4.3",
        "Personal Agents MUST NOT send tokens in a URL, and Companies MUST NOT accept them from one.",
    ),
    spec(
        "companies/fake-asks-for-a-nonce",
        "4.3",
        "Personal Agents MUST support nonces.",
    ),
    spec(
        "companies/fake-asks-for-a-nonce",
        "4.3",
        "A failed check returns `invalid_dpop_proof`, or `use_dpop_nonce` with the nonce to use in the `DPoP-Nonce` header.",
    ),
    spec(
        "companies/fake-mcp-takes-bearer-for-its-url",
        "4.3",
        "without the `DPoP` header, and MUST include `resource` set to the MCP server's `url` from `poppy.json`, as MCP authorization requires.",
    ),
    spec(
        "companies/fake-mcp-takes-bearer-for-its-url",
        "4.3",
        "Companies MUST accept Bearer Session Tokens only on their MCP APIs (section 6), and MUST require DPoP everywhere else.",
    ),
    spec(
        "companies/fake-mcp-takes-bearer-for-its-url",
        "6",
        "It MUST accept only tokens issued for its `url` (4.3).",
    ),
    spec(
        "companies/fake-mcp-takes-bearer-for-its-url",
        "6",
        "and its `authorization_servers` MUST include `auth.issuer`.",
    ),
    spec(
        "companies/fake-mcp-is-2025-06-18-or-later",
        "6",
        "A remote MCP server endpoint, using the Streamable HTTP transport and MCP version 2025-06-18 or later.",
    ),
    spec(
        "companies/fake-device-sign-in",
        "4.6",
        "Until the User finishes, the Company returns `authorization_pending`, or `slow_down` to ask the Personal Agent to wait longer between requests.",
    ),
    spec(
        "companies/fake-device-sign-in",
        "4.4",
        "Missing, unknown, or unavailable scopes are rejected with `invalid_scope`.",
    ),
    spec(
        "companies/fake-device-sign-in",
        "4.4",
        "The Company MUST NOT grant scopes beyond those requested and allowed for the type used.",
    ),
    spec(
        "companies/fake-device-sign-in",
        "4.3",
        "Any token request MAY include `scope`, to ask for fewer scopes than the Session has",
    ),
    spec(
        "companies/fake-device-sign-in",
        "4.6",
        "The Personal Agent shows the User `verification_uri` and `user_code`, or `verification_uri_complete` as a link or QR code.",
    ),
    spec(
        "companies/fake-direct-sign-in",
        "4.5",
        "`redirect_uri` MUST exactly match an entry in the Personal Agent's `redirect_uris` (4.1).",
    ),
    spec(
        "companies/fake-direct-sign-in",
        "4.5",
        "The Company redirects the browser to `redirect_uri` with a one-time `code`, the same `state`, and its `auth.issuer` in `iss`",
    ),
    spec(
        "companies/fake-account-tokens-rotate-when-it-says",
        "4.8",
        "A Company MAY return a new Account Token in `refresh_token`. The Personal Agent then MUST use the new one from then on.",
    ),
    spec(
        "companies/fake-revocation-signs-out",
        "4.9",
        "The Company revokes the Account Token and signs out every Session signed in with it.",
    ),
    spec(
        "companies/fake-revocation-signs-out",
        "4.8",
        "When the Account Token has expired or been revoked, the Company returns `invalid_grant`.",
    ),
    spec(
        "companies/fake-keeps-a-session-to-one-account",
        "4.4",
        "If the User signs in to a different account than the Session's, the code exchange, device poll, or Mediated Sign-In returns `account_mismatch` and issues no tokens.",
    ),
    spec(
        "companies/fake-rate-limits-with-retry-after",
        "4.3",
        "The Personal Agent MUST wait at least that long before sending the request again.",
    ),
    spec(
        "companies/fake-rate-limits-with-retry-after",
        "4.3",
        "At the token endpoint and at endpoints this protocol or an extension defines, such as conversations and Mediated Sign-In, the body is `{\"error\": \"rate_limited\"}`.",
    ),
    spec(
        "companies/fake-mcp-account-errors",
        "6",
        "The request needs a signed-in token.",
    ),
    spec(
        "companies/fake-mcp-account-errors",
        "6",
        "The token lacks required account scopes. The response names them in the `scope` attribute.",
    ),
];

/// A check's verdict from its body: `Ok` passes, `Err` fails with why.
fn verdict(r: Result<(), String>) -> Outcome {
    match r {
        Ok(()) => Outcome::pass(),
        Err(why) => Outcome::fail(why),
    }
}

fn need(cond: bool, why: impl Into<String>) -> Result<(), String> {
    if cond { Ok(()) } else { Err(why.into()) }
}

/// A fake Company and the Personal Agent that talks to it, its documents
/// already verified.
struct Setup {
    company: Company,
    agent: Agent,
    d: Discovered,
    _tmp: TempDir,
}

fn setup(h: &Harness, variants: &[&str]) -> Result<Setup, String> {
    let tmp = h.tempdir();
    let company = Company::spawn(h, &tmp, variants);
    let d = discover(&company.origin).map_err(|e| format!("discovery refused the fake: {e:?}"))?;
    Ok(Setup {
        company,
        agent: Agent::start(),
        d,
        _tmp: tmp,
    })
}

/// Discovery of a fake with `variants`, which must be refused.
fn refused(h: &Harness, variants: &[&str]) -> Result<Refused, String> {
    let tmp = h.tempdir();
    let company = Company::spawn(h, &tmp, variants);
    match discover(&company.origin) {
        Ok(_) => Err(format!("the {variants:?} variant passed discovery")),
        Err(e) => Ok(e),
    }
}

fn issuer_matches(h: &Harness) -> Outcome {
    verdict((|| {
        let s = setup(h, &[])?;
        need(
            s.d.issuer.ends_with("/auth"),
            format!(
                "the issuer {} has no path to insert the suffix before",
                s.d.issuer
            ),
        )?;
        let asked: Vec<String> = s
            .company
            .record()
            .iter()
            .filter_map(|r| r["path"].as_str().map(str::to_string))
            .collect();
        need(
            asked.contains(&"/.well-known/oauth-authorization-server/auth".to_string()),
            format!("the metadata was not read at the inserted well-known URL: {asked:?}"),
        )?;
        match refused(h, &["issuer-mismatch"])? {
            Refused::Issuer(_) => Ok(()),
            other => Err(format!(
                "issuer-mismatch was refused for {other:?}, not its issuer"
            )),
        }
    })())
}

fn lists_its_domain(h: &Harness) -> Outcome {
    verdict((|| {
        let s = setup(h, &[])?;
        need(
            s.d.poppy["organization"]["domain"] == "127.0.0.1",
            format!(
                "organization.domain is not the host that served it: {}",
                s.d.poppy["organization"]
            ),
        )?;
        match refused(h, &["no-domains"])? {
            Refused::Domains => Ok(()),
            other => Err(format!(
                "no-domains was refused for {other:?}, not its domains"
            )),
        }
    })())
}

fn publishes_es256(h: &Harness) -> Outcome {
    verdict((|| {
        setup(h, &[])?;
        match refused(h, &["no-es256"])? {
            Refused::Es256 { prefixed: None } => {}
            other => return Err(format!("no-es256 was refused for {other:?}")),
        }
        match refused(h, &["prefixed-algs"])? {
            Refused::Es256 { prefixed: Some(k) } if k == PREFIXED_ALGS => Ok(()),
            other => Err(format!(
                "prefixed-algs was refused for {other:?}, not naming {PREFIXED_ALGS}"
            )),
        }
    })())
}

fn endpoints_on_its_host(h: &Harness) -> Outcome {
    verdict((|| {
        setup(h, &[])?;
        match refused(h, &["off-host-endpoints"])? {
            Refused::OffHost { field, host }
                if field.ends_with("_endpoint") && host == "localhost" =>
            {
                Ok(())
            }
            other => Err(format!("off-host-endpoints was refused for {other:?}")),
        }
    })())
}

fn redirects_only_to_https(h: &Harness) -> Outcome {
    verdict((|| {
        setup(h, &[])?;
        match refused(h, &["redirect-to-http"])? {
            Refused::Redirect(to) if to.starts_with("http://") => Ok(()),
            other => Err(format!("redirect-to-http was refused for {other:?}")),
        }
    })())
}

/// A Session assertion for `user`, signed by the agent.
fn session_assertion(a: &Agent, d: &Discovered, user: &str) -> String {
    a.signed(&a.claims(user, &d.issuer))
}

fn starts_and_renews_sessions(h: &Harness) -> Outcome {
    verdict((|| {
        let s = setup(h, &[])?;
        let (a, d) = (&s.agent, &s.d);
        let user = format!("usr_{}", random_id());
        let r = a.token(
            d,
            &[
                ("grant_type", JWT_BEARER),
                ("assertion", &session_assertion(a, d, &user)),
            ],
            true,
        );
        let b = r.json();
        need(
            r.status == 200,
            format!("starting a Session: {}", r.brief()),
        )?;
        need(
            b["token_type"] == "DPoP"
                && b["scope"] == ""
                && b["signed_in"] == false
                && b["expires_in"].as_u64().is_some_and(|n| n > 0),
            format!("a new Session's token: {b}"),
        )?;
        need(
            r.header("cache-control") == Some("no-store"),
            "the token response is not Cache-Control: no-store",
        )?;
        let session = b["session_id"].as_str().unwrap_or_default().to_string();
        let renew = |more: &[(&str, &str)]| {
            let sa = session_assertion(a, d, &user);
            let mut fields = vec![
                ("grant_type", JWT_BEARER),
                ("assertion", sa.as_str()),
                ("session_id", session.as_str()),
            ];
            fields.extend(more);
            a.token(d, &fields, true)
        };
        let r = renew(&[("scope", "poppy:read")]);
        need(
            r.status == 400 && r.error() == "invalid_scope",
            format!("a signed-out renewal asking for poppy:read: {}", r.brief()),
        )?;
        let r = renew(&[]);
        let renewed = r.json()["access_token"]
            .as_str()
            .unwrap_or_default()
            .to_string();
        need(
            r.status == 200 && r.json()["session_id"] == *session && renewed != b["access_token"],
            format!("renewing the Session: {}", r.brief()),
        )?;
        // Both tokens still work; the record says which one was replaced.
        let first = b["access_token"].as_str().unwrap_or_default();
        let url = format!("{}/api/session", s.company.origin);
        for token in [first, renewed.as_str()] {
            let r = api(
                &url,
                &[
                    ("Authorization", &format!("DPoP {token}")),
                    ("DPoP", &a.proof("GET", &url, Some(token), None)),
                ],
            )?;
            need(
                r.status == 200,
                format!("the API with a Session Token: {}", r.brief()),
            )?;
        }
        let superseded = |token: &str| {
            let hash: String = b64url(&sha256(token.as_bytes())).chars().take(8).collect();
            s.company
                .record()
                .iter()
                .find(|r| r["path"] == "/api/session" && r["presented"] == *hash)
                .map(|r| r["superseded"].clone())
        };
        need(
            superseded(first) == Some(json!(true)) && superseded(&renewed) == Some(json!(false)),
            format!(
                "the record of the replaced token and its renewal: superseded {:?} and {:?}",
                superseded(first),
                superseded(&renewed)
            ),
        )?;
        let r = a.token(
            d,
            &[
                ("grant_type", JWT_BEARER),
                ("assertion", &session_assertion(a, d, "usr_someone_else")),
                ("session_id", &session),
            ],
            true,
        );
        need(
            r.status == 400 && r.error() == "invalid_session",
            format!("another User's renewal: {}", r.brief()),
        )
    })())
}

/// A token request with `client_assertion` in place of a fresh one.
fn with_assertion(
    a: &Agent,
    d: &Discovered,
    fields: &[(&str, &str)],
    client_assertion: &str,
) -> Resp {
    let at = d.token_endpoint();
    let mut all = fields.to_vec();
    all.extend(a.client_auth(client_assertion));
    let proof = a.proof("POST", &at, None, None);
    post_form(&at, &all, &[("DPoP", &proof)]).unwrap_or_else(|e| Resp {
        status: 0,
        headers: vec![],
        body: e.into_bytes(),
    })
}

fn rejects_another_aud(h: &Harness) -> Outcome {
    verdict((|| {
        let s = setup(h, &[])?;
        let (a, d) = (&s.agent, &s.d);
        fn start(assertion: &str) -> [(&'static str, &str); 2] {
            [("grant_type", JWT_BEARER), ("assertion", assertion)]
        }
        let token_endpoint = d.token_endpoint();
        let sa = session_assertion(a, d, "usr_aud");
        let r = with_assertion(a, d, &start(&sa), &a.client_assertion(&token_endpoint));
        need(
            r.status == 401 && r.error() == "invalid_client",
            format!("a client assertion for the token endpoint: {}", r.brief()),
        )?;
        let mut listed = a.claims(&a.client_id, &d.issuer);
        listed["aud"] = json!([d.issuer]);
        let sa = session_assertion(a, d, "usr_aud");
        let r = with_assertion(a, d, &start(&sa), &a.signed(&listed));
        need(
            r.status == 401 && r.error() == "invalid_client",
            format!("a client assertion with a list for aud: {}", r.brief()),
        )?;
        let sa = a.signed(&a.claims("usr_aud", &token_endpoint));
        let r = a.token(d, &start(&sa), true);
        need(
            r.status == 400 && r.error() == "invalid_grant",
            format!("a Session assertion for the token endpoint: {}", r.brief()),
        )?;
        let r = a.token(d, &start(&session_assertion(a, d, "usr_aud")), true);
        need(
            r.status == 200,
            format!("the same request with aud the issuer: {}", r.brief()),
        )
    })())
}

fn rejects_a_reused_jti(h: &Harness) -> Outcome {
    verdict((|| {
        let s = setup(h, &[])?;
        let (a, d) = (&s.agent, &s.d);
        let sa = session_assertion(a, d, "usr_jti");
        let first = a.token(d, &[("grant_type", JWT_BEARER), ("assertion", &sa)], true);
        need(
            first.status == 200,
            format!("the first use: {}", first.brief()),
        )?;
        let again = a.token(d, &[("grant_type", JWT_BEARER), ("assertion", &sa)], true);
        need(
            again.status == 400 && again.error() == "invalid_grant",
            format!("the Session assertion again: {}", again.brief()),
        )?;
        let ca = a.client_assertion(&d.issuer);
        let one = session_assertion(a, d, "usr_jti");
        let r = with_assertion(
            a,
            d,
            &[("grant_type", JWT_BEARER), ("assertion", &one)],
            &ca,
        );
        need(
            r.status == 200,
            format!("a client assertion's first use: {}", r.brief()),
        )?;
        let two = session_assertion(a, d, "usr_jti");
        let r = with_assertion(
            a,
            d,
            &[("grant_type", JWT_BEARER), ("assertion", &two)],
            &ca,
        );
        need(
            r.status == 401 && r.error() == "invalid_client",
            format!("the client assertion again: {}", r.brief()),
        )
    })())
}

/// `claims` with each member of `over` put in place of its own.
fn with_claims(mut claims: Value, over: &Value) -> Value {
    for (k, v) in over.as_object().into_iter().flatten() {
        claims[k] = v.clone();
    }
    claims
}

fn checks_the_assertion_claims(h: &Harness) -> Outcome {
    verdict((|| {
        let s = setup(h, &[])?;
        let (a, d) = (&s.agent, &s.d);
        let someone = format!("{}/someone.json", a.origin);
        let t = probe::now();
        let lives = [
            (
                "expired nine minutes ago",
                json!({"iat": t - 600, "exp": t - 540}),
            ),
            (
                "issued five minutes ahead",
                json!({"iat": t + 300, "exp": t + 360}),
            ),
            ("living ten minutes", json!({"iat": t, "exp": t + 600})),
            ("exp not after iat", json!({"iat": t, "exp": t})),
        ];
        let not_the_client = [
            ("iss another client", json!({"iss": someone})),
            ("sub another client", json!({"sub": someone})),
        ];
        for (what, over) in not_the_client.iter().chain(&lives) {
            let claims = with_claims(a.claims(&a.client_id, &d.issuer), over);
            let sa = session_assertion(a, d, "usr_claims");
            let r = with_assertion(
                a,
                d,
                &[("grant_type", JWT_BEARER), ("assertion", &sa)],
                &a.signed(&claims),
            );
            need(
                r.status == 401 && r.error() == "invalid_client",
                format!("a client assertion with {what}: {}", r.brief()),
            )?;
        }
        let start = |over: &Value| {
            let claims = with_claims(a.claims("usr_claims", &d.issuer), over);
            a.token(
                d,
                &[
                    ("grant_type", JWT_BEARER),
                    ("assertion", &a.signed(&claims)),
                ],
                true,
            )
        };
        for (what, over) in not_the_client[..1].iter().chain(&lives) {
            let r = start(over);
            need(
                r.status == 400 && r.error() == "invalid_grant",
                format!("a Session assertion with {what}: {}", r.brief()),
            )?;
        }
        let r = start(&json!({}));
        need(
            r.status == 200,
            format!("the same request with good claims: {}", r.brief()),
        )
    })())
}

fn checks_the_client_document(h: &Harness) -> Outcome {
    verdict((|| {
        let s = setup(h, &[])?;
        let (a, d) = (&s.agent, &s.d);
        let o = &a.origin;
        let port = o.rsplit(':').next().unwrap_or_default();
        let doc = |id: &str, jwks: &str| json!({"client_id": id, "client_name": "x", "jwks_uri": jwks, "redirect_uris": [], "token_endpoint_auth_method": "private_key_jwt"});
        // Served at /other.json, but naming /client.json.
        a.serve("/other.json", doc(&a.client_id, &format!("{o}/jwks.json")));
        let off = format!("{o}/off-host.json");
        a.serve(
            "/off-host.json",
            doc(&off, &format!("https://localhost:{port}/jwks.json")),
        );
        let private = format!("{o}/private.json");
        a.serve(
            "/private.json",
            doc(&private, &format!("{o}/private-jwks.json")),
        );
        let mut leaked = a.key.jwk.clone();
        leaked["kid"] = json!(a.key.kid);
        leaked["d"] = json!(b64url(&[7u8; 32]));
        a.serve("/private-jwks.json", json!({"keys": [leaked]}));
        for (cid, what, why) in [
            (
                format!("{o}/other.json"),
                "a document naming another client_id",
                "not the URL it was fetched from",
            ),
            (
                off,
                "a jwks_uri on another host",
                "jwks_uri is not on the client_id's host",
            ),
            (
                private,
                "a key set holding a private key",
                "holds a private key",
            ),
        ] {
            let at = d.token_endpoint();
            // Assertions that are good in every other way, for this client.
            let mut claims = a.claims(&cid, &d.issuer);
            claims["iss"] = json!(cid);
            let assertion = a.signed(&claims);
            let mut claims = a.claims("usr_doc", &d.issuer);
            claims["iss"] = json!(cid);
            let sa = a.signed(&claims);
            let proof = a.proof("POST", &at, None, None);
            let r = post_form(
                &at,
                &[
                    ("grant_type", JWT_BEARER),
                    ("assertion", &sa),
                    ("client_id", &cid),
                    ("client_assertion_type", probe::ASSERTION_TYPE),
                    ("client_assertion", &assertion),
                ],
                &[("DPoP", &proof)],
            )?;
            need(
                r.status == 401
                    && r.error() == "invalid_client"
                    && r.json()["error_description"]
                        .as_str()
                        .is_some_and(|e| e.contains(why)),
                format!("{what}: {}", r.brief()),
            )?;
        }
        Ok(())
    })())
}

fn validates_every_proof(h: &Harness) -> Outcome {
    verdict((|| {
        let s = setup(h, &[])?;
        let (a, d) = (&s.agent, &s.d);
        let at = d.token_endpoint();
        // One start request with `dpop` as its DPoP headers.
        let send = |dpop: &[&str]| -> Result<Resp, String> {
            let sa = session_assertion(a, d, "usr_proof");
            let ca = a.client_assertion(&d.issuer);
            let mut fields = vec![("grant_type", JWT_BEARER), ("assertion", sa.as_str())];
            fields.extend(a.client_auth(&ca));
            let headers: Vec<(&str, &str)> = dpop.iter().map(|p| ("DPoP", *p)).collect();
            post_form(&at, &fields, &headers)
        };
        let claims = |htm: &str, htu: &str, iat: u64| json!({"jti": random_id(), "htm": htm, "htu": htu, "iat": iat});
        let t = probe::now();
        let other = probe::Key::generate();
        let mut private_jwk = a.dpop.jwk.clone();
        private_jwk["d"] = json!(b64url(&[1u8; 32]));
        let mut with_ath = claims("POST", &at, t);
        with_ath["ath"] = json!(b64url(&sha256(b"a token")));
        // A proof whose header says `typ` and `alg`, signed ES256 all the same.
        let headed = |typ: &str, alg: &str| {
            a.dpop.sign(
                &json!({"typ": typ, "alg": alg, "jwk": a.dpop.jwk}),
                &claims("POST", &at, t),
            )
        };
        let unsigned = {
            let p = headed("dpop+jwt", "none");
            format!("{}.", p.rsplit_once('.').map_or("", |(input, _)| input))
        };
        let cases: Vec<(&str, Vec<String>)> = vec![
            ("no proof and no MCP resource", vec![]),
            (
                "two DPoP headers",
                vec![
                    a.proof("POST", &at, None, None),
                    a.proof("POST", &at, None, None),
                ],
            ),
            (
                "the wrong htm",
                vec![a.dpop_proof(&a.dpop, &claims("GET", &at, t))],
            ),
            (
                "the wrong htu",
                vec![a.dpop_proof(
                    &a.dpop,
                    &claims("POST", &format!("{}/elsewhere", d.issuer), t),
                )],
            ),
            (
                "an iat ten minutes old",
                vec![a.dpop_proof(&a.dpop, &claims("POST", &at, t - 600))],
            ),
            (
                "an iat ten minutes ahead",
                vec![a.dpop_proof(&a.dpop, &claims("POST", &at, t + 600))],
            ),
            ("typ JWT", vec![headed("JWT", "ES256")]),
            ("alg none, unsigned", vec![unsigned]),
            ("alg HS256", vec![headed("dpop+jwt", "HS256")]),
            (
                "alg ES384, which the Company does not accept",
                vec![headed("dpop+jwt", "ES384")],
            ),
            (
                "an ath at the token endpoint",
                vec![a.dpop_proof(&a.dpop, &with_ath)],
            ),
            (
                "a private key in the jwk",
                vec![a.dpop.sign(
                    &json!({"typ": "dpop+jwt", "alg": "ES256", "jwk": private_jwk}),
                    &claims("POST", &at, t),
                )],
            ),
            (
                "a signature by another key than the jwk's",
                vec![other.sign(
                    &json!({"typ": "dpop+jwt", "alg": "ES256", "jwk": a.dpop.jwk}),
                    &claims("POST", &at, t),
                )],
            ),
        ];
        for (what, proofs) in cases {
            let refs: Vec<&str> = proofs.iter().map(String::as_str).collect();
            let r = send(&refs)?;
            need(
                r.status == 400 && r.error() == "invalid_dpop_proof",
                format!("{what}: {}", r.brief()),
            )?;
        }
        let proof = a.proof("POST", &at, None, None);
        let r = send(&[&proof])?;
        need(r.status == 200, format!("a good proof: {}", r.brief()))?;
        let r = send(&[&proof])?;
        need(
            r.status == 400 && r.error() == "invalid_dpop_proof",
            format!("the same proof replayed: {}", r.brief()),
        )
    })())
}

/// GET the API's Session operation with `headers`, at `url`.
fn api(url: &str, headers: &[(&str, &str)]) -> Result<Resp, String> {
    request("GET", url, headers, &[])
}

fn binds_tokens_to_their_key(h: &Harness) -> Outcome {
    verdict((|| {
        let s = setup(h, &[])?;
        let (a, d) = (&s.agent, &s.d);
        let (token, session) = a.start_session(d, "usr_bind")?;
        let url = format!("{}/api/session", s.company.origin);
        let auth = format!("DPoP {token}");
        let good = a.proof("GET", &url, Some(&token), None);
        let r = api(&url, &[("Authorization", &auth), ("DPoP", &good)])?;
        need(
            r.status == 200 && r.json()["session_id"] == *session && r.json()["signed_in"] == false,
            format!("the token with its proof: {}", r.brief()),
        )?;
        let other = probe::Key::generate();
        let by_other = a.dpop_proof(
            &other,
            &json!({"jti": random_id(), "htm": "GET", "htu": url, "iat": probe::now(), "ath": b64url(&sha256(token.as_bytes()))}),
        );
        let refused_with =
            |error: &str, headers: &[(&str, &str)], what: &str| -> Result<(), String> {
                let r = api(&url, headers)?;
                need(
                    r.status == 401 && r.error() == error,
                    format!("{what}: {}", r.brief()),
                )
            };
        refused_with(
            "invalid_dpop_proof",
            &[("Authorization", &auth)],
            "no proof",
        )?;
        refused_with(
            "invalid_dpop_proof",
            &[
                ("Authorization", &auth),
                ("DPoP", &a.proof("GET", &url, None, None)),
            ],
            "a proof without ath",
        )?;
        refused_with(
            "invalid_dpop_proof",
            &[
                ("Authorization", &auth),
                ("DPoP", &a.proof("GET", &url, Some("another token"), None)),
            ],
            "a proof whose ath is another token's",
        )?;
        refused_with(
            "invalid_dpop_proof",
            &[("Authorization", &auth), ("DPoP", &by_other)],
            "a proof by another key",
        )?;
        refused_with(
            "invalid_token",
            &[
                ("Authorization", &format!("Bearer {token}")),
                ("DPoP", &a.proof("GET", &url, Some(&token), None)),
            ],
            "the Bearer scheme",
        )?;
        let r = api(
            &format!("{url}?{}", probe::form(&[("access_token", &token)])),
            &[("DPoP", &a.proof("GET", &url, Some(&token), None))],
        )?;
        need(
            r.status == 400,
            format!("a token in the URL: {}", r.brief()),
        )
    })())
}

fn asks_for_a_nonce(h: &Harness) -> Outcome {
    verdict((|| {
        let s = setup(h, &["nonce-required"])?;
        let (a, d) = (&s.agent, &s.d);
        let at = d.token_endpoint();
        let start = |proof: &str| -> Result<Resp, String> {
            let sa = session_assertion(a, d, "usr_nonce");
            let ca = a.client_assertion(&d.issuer);
            let mut fields = vec![("grant_type", JWT_BEARER), ("assertion", sa.as_str())];
            fields.extend(a.client_auth(&ca));
            post_form(&at, &fields, &[("DPoP", proof)])
        };
        let r = start(&a.proof("POST", &at, None, None))?;
        let nonce = r.header("dpop-nonce").unwrap_or_default().to_string();
        need(
            r.status == 400 && r.error() == "use_dpop_nonce" && !nonce.is_empty(),
            format!("a proof without the nonce: {}", r.brief()),
        )?;
        let r = start(&a.proof("POST", &at, None, Some(&nonce)))?;
        need(
            r.status == 200,
            format!("the retry with the nonce: {}", r.brief()),
        )?;
        let token = r.json()["access_token"]
            .as_str()
            .unwrap_or_default()
            .to_string();
        let url = format!("{}/api/session", s.company.origin);
        let auth = format!("DPoP {token}");
        let r = api(
            &url,
            &[
                ("Authorization", &auth),
                ("DPoP", &a.proof("GET", &url, Some(&token), Some("stale"))),
            ],
        )?;
        let nonce = r.header("dpop-nonce").unwrap_or_default().to_string();
        need(
            r.status == 401 && r.error() == "use_dpop_nonce" && !nonce.is_empty(),
            format!("the API, a proof without its nonce: {}", r.brief()),
        )?;
        let r = api(
            &url,
            &[
                ("Authorization", &auth),
                ("DPoP", &a.proof("GET", &url, Some(&token), Some(&nonce))),
            ],
        )?;
        need(
            r.status == 200,
            format!("the API, the retry with the nonce: {}", r.brief()),
        )
    })())
}

/// One JSON-RPC request to the MCP server at `url`.
fn mcp(url: &str, token: Option<&str>, method: &str, params: Value) -> Result<Resp, String> {
    let auth = token.map(|t| format!("Bearer {t}"));
    let mut headers = vec![
        ("Content-Type", "application/json"),
        ("Accept", "application/json, text/event-stream"),
    ];
    if let Some(a) = &auth {
        headers.push(("Authorization", a));
    }
    let body = json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params});
    request("POST", url, &headers, body.to_string().as_bytes())
}

/// A Bearer token for the MCP server at `mcp_url` in Session `session`
/// (§4.3): the renewal request, without a proof, with `resource`.
fn bearer(
    a: &Agent,
    d: &Discovered,
    user: &str,
    session: &str,
    mcp_url: &str,
) -> Result<String, String> {
    let r = a.token(
        d,
        &[
            ("grant_type", JWT_BEARER),
            ("assertion", &session_assertion(a, d, user)),
            ("session_id", session),
            ("resource", mcp_url),
        ],
        false,
    );
    match (
        r.status,
        r.json()["token_type"].as_str(),
        r.json()["access_token"].as_str(),
    ) {
        (200, Some("Bearer"), Some(t)) => Ok(t.to_string()),
        _ => Err(format!("a Bearer token for {mcp_url}: {}", r.brief())),
    }
}

fn mcp_takes_bearer_for_its_url(h: &Harness) -> Outcome {
    verdict((|| {
        let s = setup(h, &["two-mcp-apis"])?;
        let (a, d) = (&s.agent, &s.d);
        let urls = d.apis("mcp");
        need(
            urls.len() == 2,
            format!("the two-mcp-apis variant lists {urls:?}"),
        )?;
        let (first, second) = (&urls[0], &urls[1]);
        let metadata = well_known(first, "oauth-protected-resource");
        let prm = request("GET", &metadata, &[], &[])?.json();
        need(
            prm["resource"] == **first
                && prm["authorization_servers"]
                    .as_array()
                    .is_some_and(|s| s.contains(&json!(d.issuer))),
            format!("the MCP server's RFC 9728 metadata: {prm}"),
        )?;
        let r = mcp(first, None, "initialize", json!({}))?;
        need(
            r.status == 401
                && r.header("www-authenticate")
                    .is_some_and(|c| c.contains(&format!("resource_metadata=\"{metadata}\""))),
            format!("an unauthenticated request: {}", r.brief()),
        )?;
        let (dpop_token, session) = a.start_session(d, "usr_mcp")?;
        let token = bearer(a, d, "usr_mcp", &session, first)?;
        let init = json!({"protocolVersion": "2025-11-25", "capabilities": {}, "clientInfo": {"name": "conformance", "version": "1"}});
        let r = mcp(first, Some(&token), "initialize", init.clone())?;
        need(
            r.status == 200 && r.json()["result"]["protocolVersion"].is_string(),
            format!("initialize with the Bearer token: {}", r.brief()),
        )?;
        let r = mcp(first, Some(&dpop_token), "initialize", init.clone())?;
        need(
            r.status == 401 && r.error() == "invalid_token",
            format!("a DPoP Session Token at the MCP server: {}", r.brief()),
        )?;
        let r = mcp(second, Some(&token), "initialize", init)?;
        need(
            r.status == 401 && r.error() == "invalid_token",
            format!(
                "a Bearer token issued for {first} at {second}: {}",
                r.brief()
            ),
        )?;
        let url = format!("{}/api/session", s.company.origin);
        let r = api(
            &url,
            &[
                ("Authorization", &format!("DPoP {token}")),
                ("DPoP", &a.proof("GET", &url, Some(&token), None)),
            ],
        )?;
        need(
            r.status == 401 && r.error() == "invalid_token",
            format!("the Bearer token at the API: {}", r.brief()),
        )?;
        let r = a.token(
            d,
            &[
                ("grant_type", JWT_BEARER),
                ("assertion", &session_assertion(a, d, "usr_mcp")),
                ("session_id", &session),
                ("resource", first),
            ],
            true,
        );
        need(
            r.status == 400,
            format!(
                "a token for the MCP server requested with a proof: {}",
                r.brief()
            ),
        )
    })())
}

fn mcp_is_2025_06_18_or_later(h: &Harness) -> Outcome {
    verdict((|| {
        let init = json!({"protocolVersion": "2025-11-25", "capabilities": {}, "clientInfo": {"name": "conformance", "version": "1"}});
        for (variants, discovers, speaks) in [
            (&[][..], true, "2025-11-25"),
            (&["mcp-2025-06-18"][..], false, "2025-06-18"),
        ] {
            let s = setup(h, variants)?;
            let (a, d) = (&s.agent, &s.d);
            let url = d.apis("mcp").first().cloned().unwrap_or_default();
            let (_, session) = a.start_session(d, "usr_v")?;
            let token = bearer(a, d, "usr_v", &session, &url)?;
            let r = mcp(&url, Some(&token), "server/discover", json!({}))?;
            let answered = r.json()["result"]["supportedVersions"].is_array();
            let missing = r.json()["error"]["code"] == -32601;
            need(
                if discovers { answered } else { missing },
                format!("{variants:?}: server/discover: {}", r.brief()),
            )?;
            let r = mcp(&url, Some(&token), "initialize", init.clone())?;
            need(
                r.json()["result"]["protocolVersion"] == speaks,
                format!("{variants:?}: initialize asking 2025-11-25: {}", r.brief()),
            )?;
        }
        Ok(())
    })())
}

fn device_sign_in(h: &Harness) -> Outcome {
    verdict((|| {
        let s = setup(h, &[])?;
        let (a, d) = (&s.agent, &s.d);
        let at = d.endpoint("device_authorization_endpoint");
        let start = |scope: Option<&str>| -> Result<Resp, String> {
            let ca = a.client_assertion(&d.issuer);
            let mut fields: Vec<(&str, &str)> = scope.map(|s| ("scope", s)).into_iter().collect();
            fields.extend(a.client_auth(&ca));
            post_form(&at, &fields, &[])
        };
        for (scope, what) in [
            (None, "no scope"),
            (Some("poppy:admin"), "a scope it cannot grant"),
        ] {
            let r = start(scope)?;
            need(
                r.status == 400 && r.error() == "invalid_scope",
                format!("device authorization with {what}: {}", r.brief()),
            )?;
        }
        let r = start(Some("poppy:read poppy:write"))?;
        let b = r.json();
        need(
            r.status == 200,
            format!("device authorization: {}", r.brief()),
        )?;
        let host = |u: &Value| {
            net::http::Url::parse(u.as_str().unwrap_or_default())
                .map(|u| u.host)
                .unwrap_or_default()
        };
        need(
            host(&b["verification_uri"]) == "127.0.0.1"
                && host(&b["verification_uri_complete"]) == "127.0.0.1",
            format!("the User is sent off the Company's host: {b}"),
        )?;
        // A poll of the device code `b` names into `session`, with `scope`.
        let poll = |b: &Value, session: &str, scope: Option<&str>| {
            let mut fields = vec![
                ("grant_type", DEVICE_CODE),
                ("device_code", b["device_code"].as_str().unwrap_or_default()),
                ("session_id", session),
            ];
            fields.extend(scope.map(|s| ("scope", s)));
            a.token(d, &fields, true)
        };
        let wait = || std::thread::sleep(std::time::Duration::from_secs(1));
        let (_, session) = a.start_session(d, "usr_device")?;
        wait();
        let r = poll(&b, &session, None);
        need(
            r.status == 400 && r.error() == "authorization_pending",
            format!("the first poll: {}", r.brief()),
        )?;
        wait();
        let r = poll(&b, &session, None);
        let t = r.json();
        need(
            r.status == 200
                && t["signed_in"] == true
                && t["refresh_token"].is_string()
                && t["scope"] == "poppy:read poppy:write"
                && t["session_id"] == *session,
            format!("the poll after approval: {}", r.brief()),
        )?;
        let (_, session) = a.start_session(d, "usr_hasty")?;
        let b = start(Some("poppy:read"))?.json();
        let r = poll(&b, &session, None);
        need(
            r.status == 400 && r.error() == "authorization_pending",
            format!("the first of two quick polls: {}", r.brief()),
        )?;
        let r = poll(&b, &session, None);
        need(
            r.status == 400 && r.error() == "slow_down",
            format!("a poll sooner than the interval: {}", r.brief()),
        )?;
        // The User approves only poppy:read of the two asked for.
        s.company
            .control(json!({"approver": {"scope": "poppy:read"}}));
        let (_, session) = a.start_session(d, "usr_partly")?;
        let b = start(Some("poppy:read poppy:write"))?.json();
        wait();
        let r = poll(&b, &session, None);
        need(
            r.status == 400 && r.error() == "authorization_pending",
            format!("the first poll: {}", r.brief()),
        )?;
        wait();
        let r = poll(&b, &session, Some("poppy:read poppy:write"));
        need(
            r.status == 400 && r.error() == "invalid_scope",
            format!(
                "a poll asking for poppy:write the User did not approve: {}",
                r.brief()
            ),
        )?;
        wait();
        let r = poll(&b, &session, None);
        need(
            r.status == 200 && r.json()["scope"] == "poppy:read",
            format!("the next poll, for what the User approved: {}", r.brief()),
        )?;
        s.company
            .control(json!({"approver": {"decision": "deny", "after_polls": 0}}));
        let (_, session) = a.start_session(d, "usr_declines")?;
        let err = a
            .device_sign_in(d, &session, "poppy:read")
            .err()
            .unwrap_or_default();
        need(
            err.contains("access_denied"),
            format!("a declined sign-in: {err}"),
        )?;
        let off = setup(h, &["off-host-verification-uri"])?;
        let ca = off.agent.client_assertion(&off.d.issuer);
        let mut fields = vec![("scope", "poppy:read")];
        fields.extend(off.agent.client_auth(&ca));
        let r = post_form(
            &off.d.endpoint("device_authorization_endpoint"),
            &fields,
            &[],
        )?;
        need(
            r.status == 200 && host(&r.json()["verification_uri"]) == "localhost",
            format!("off-host-verification-uri: {}", r.brief()),
        )
    })())
}

/// A query parameter of a URL, percent-decoded.
fn query_param(url: &str, name: &str) -> Option<String> {
    let q = url.split_once('?')?.1;
    let (_, v) = q
        .split('&')
        .filter_map(|kv| kv.split_once('='))
        .find(|(k, _)| *k == name)?;
    let b = v.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        let hex = b
            .get(i + 1..i + 3)
            .and_then(|h| std::str::from_utf8(h).ok())
            .and_then(|h| u8::from_str_radix(h, 16).ok());
        match (b[i], hex) {
            (b'%', Some(x)) => {
                out.push(x);
                i += 3;
            }
            (b'+', _) => {
                out.push(b' ');
                i += 1;
            }
            (c, _) => {
                out.push(c);
                i += 1;
            }
        }
    }
    Some(String::from_utf8_lossy(&out).into_owned())
}

fn direct_sign_in(h: &Harness) -> Outcome {
    verdict((|| {
        let s = setup(h, &[])?;
        let (a, d) = (&s.agent, &s.d);
        let verifier = random_id() + &random_id();
        let challenge = b64url(&sha256(verifier.as_bytes()));
        let authorize = |redirect_uri: &str, state: &str| {
            let q = probe::form(&[
                ("response_type", "code"),
                ("client_id", &a.client_id),
                ("redirect_uri", redirect_uri),
                ("scope", "poppy:read"),
                ("state", state),
                ("code_challenge", &challenge),
                ("code_challenge_method", "S256"),
            ]);
            request(
                "GET",
                &format!("{}?{q}", d.endpoint("authorization_endpoint")),
                &[],
                &[],
            )
        };
        let r = authorize(&format!("{}/elsewhere", a.origin), "st-1")?;
        need(
            r.status == 400 && r.header("location").is_none(),
            format!("an unregistered redirect_uri: {}", r.brief()),
        )?;
        let r = authorize(&a.redirect_uri, "st-2")?;
        let to = r.header("location").unwrap_or_default().to_string();
        need(
            r.status == 302 && to.starts_with(&format!("{}?", a.redirect_uri)),
            format!("the authorization redirect: {}", r.brief()),
        )?;
        need(
            query_param(&to, "state").as_deref() == Some("st-2")
                && query_param(&to, "iss").as_deref() == Some(d.issuer.as_str()),
            format!("the redirect's state and iss: {to}"),
        )?;
        let code = query_param(&to, "code").unwrap_or_default();
        let (_, session) = a.start_session(d, "usr_direct")?;
        let exchange_for = |code: &str, verifier: &str, scope: Option<&str>| {
            let mut fields = vec![
                ("grant_type", "authorization_code"),
                ("code", code),
                ("redirect_uri", a.redirect_uri.as_str()),
                ("code_verifier", verifier),
                ("session_id", session.as_str()),
            ];
            fields.extend(scope.map(|s| ("scope", s)));
            a.token(d, &fields, true)
        };
        let exchange = |code: &str, verifier: &str| exchange_for(code, verifier, None);
        let r = exchange(&code, &verifier);
        need(
            r.status == 200
                && r.json()["signed_in"] == true
                && r.json()["refresh_token"].is_string(),
            format!("the code exchange: {}", r.brief()),
        )?;
        let r = exchange(&code, &verifier);
        need(
            r.status == 400 && r.error() == "invalid_grant",
            format!("the code exchanged again: {}", r.brief()),
        )?;
        let r = authorize(&a.redirect_uri, "st-3")?;
        let code =
            query_param(r.header("location").unwrap_or_default(), "code").unwrap_or_default();
        let r = exchange(&code, &format!("{verifier}x"));
        need(
            r.status == 400 && r.error() == "invalid_grant",
            format!("the wrong code_verifier: {}", r.brief()),
        )?;
        let r = authorize(&a.redirect_uri, "st-4")?;
        let code =
            query_param(r.header("location").unwrap_or_default(), "code").unwrap_or_default();
        let r = exchange_for(&code, &verifier, Some("poppy:read poppy:write"));
        need(
            r.status == 400 && r.error() == "invalid_scope",
            format!(
                "an exchange asking for poppy:write, which the sign-in did not ask for: {}",
                r.brief()
            ),
        )
    })())
}

/// A signed-in Session for `user`: its Session id and Account Token.
fn signed_in(
    a: &Agent,
    d: &Discovered,
    user: &str,
    scope: &str,
) -> Result<(String, String), String> {
    let (_, session) = a.start_session(d, user)?;
    let t = a.device_sign_in(d, &session, scope)?;
    let rt = t["refresh_token"]
        .as_str()
        .ok_or("no Account Token")?
        .to_string();
    Ok((session, rt))
}

/// The `refresh_token` grant with `rt`, into `session`.
fn refresh(a: &Agent, d: &Discovered, rt: &str, session: &str) -> Resp {
    a.token(
        d,
        &[
            ("grant_type", "refresh_token"),
            ("refresh_token", rt),
            ("session_id", session),
        ],
        true,
    )
}

fn account_tokens_rotate(h: &Harness) -> Outcome {
    verdict((|| {
        let s = setup(h, &[])?;
        let (session, rt) = signed_in(&s.agent, &s.d, "usr_keep", "poppy:read")?;
        for n in 1..=2 {
            let r = refresh(&s.agent, &s.d, &rt, &session);
            need(
                r.status == 200
                    && r.json()["signed_in"] == true
                    && r.json().get("refresh_token").is_none(),
                format!("use {n} of the Account Token: {}", r.brief()),
            )?;
        }
        let s = setup(h, &["rotation"])?;
        let (session, rt) = signed_in(&s.agent, &s.d, "usr_rotate", "poppy:read")?;
        let r = refresh(&s.agent, &s.d, &rt, &session);
        let next = r.json()["refresh_token"]
            .as_str()
            .unwrap_or_default()
            .to_string();
        need(
            r.status == 200 && !next.is_empty() && next != rt,
            format!("a use under rotation: {}", r.brief()),
        )?;
        let r = refresh(&s.agent, &s.d, &rt, &session);
        need(
            r.status == 400 && r.error() == "invalid_grant",
            format!("the spent Account Token: {}", r.brief()),
        )?;
        let r = refresh(&s.agent, &s.d, &next, &session);
        need(
            r.status == 200,
            format!("the rotated Account Token: {}", r.brief()),
        )
    })())
}

/// Revoke the Account Token `rt` (§4.9).
fn revoke(a: &Agent, d: &Discovered, rt: &str) -> Result<Resp, String> {
    let ca = a.client_assertion(&d.issuer);
    let mut fields = vec![("token", rt), ("token_type_hint", "refresh_token")];
    fields.extend(a.client_auth(&ca));
    post_form(&d.endpoint("revocation_endpoint"), &fields, &[])
}

/// Whether the fake's API reads the DPoP Session Token `token` signed in.
fn reads_signed_in(s: &Setup, token: &str) -> Result<bool, String> {
    let url = format!("{}/api/session", s.company.origin);
    let r = api(
        &url,
        &[
            ("Authorization", &format!("DPoP {token}")),
            ("DPoP", &s.agent.proof("GET", &url, Some(token), None)),
        ],
    )?;
    match r.json()["signed_in"].as_bool() {
        Some(b) if r.status == 200 => Ok(b),
        _ => Err(format!("the API: {}", r.brief())),
    }
}

fn revocation_signs_out(h: &Harness) -> Outcome {
    verdict((|| {
        let s = setup(h, &[])?;
        let (a, d) = (&s.agent, &s.d);
        let (session, rt) = signed_in(a, d, "usr_out", "poppy:read")?;
        let r = refresh(a, d, &rt, &session);
        let token = r.json()["access_token"]
            .as_str()
            .unwrap_or_default()
            .to_string();
        need(
            reads_signed_in(&s, &token)?,
            "the signed-in token reads signed in",
        )?;
        let r = revoke(a, d, &rt)?;
        need(r.status == 200, format!("the revocation: {}", r.brief()))?;
        need(
            !reads_signed_in(&s, &token)?,
            "a token issued signed in still reads signed in after the revocation",
        )?;
        let r = refresh(a, d, &rt, &session);
        need(
            r.status == 400 && r.error() == "invalid_grant",
            format!("the revoked Account Token: {}", r.brief()),
        )?;
        let r = a.token(
            d,
            &[
                ("grant_type", JWT_BEARER),
                ("assertion", &session_assertion(a, d, "usr_out")),
                ("session_id", &session),
            ],
            true,
        );
        need(
            r.status == 200 && r.json()["signed_in"] == false,
            format!("the Session, renewed signed out: {}", r.brief()),
        )?;
        // Under rotation, two Sessions of one sign-in: the first signed in
        // with the first Account Token, the second with its replacement.
        let s = setup(h, &["rotation"])?;
        let (a, d) = (&s.agent, &s.d);
        let (_, first) = a.start_session(d, "usr_both")?;
        let t = a.device_sign_in(d, &first, "poppy:read")?;
        let (first_token, rt1) = (
            t["access_token"].as_str().unwrap_or_default().to_string(),
            t["refresh_token"].as_str().unwrap_or_default().to_string(),
        );
        let (_, second) = a.start_session(d, "usr_both")?;
        let r = refresh(a, d, &rt1, &second);
        let (second_token, rt2) = (
            r.json()["access_token"]
                .as_str()
                .unwrap_or_default()
                .to_string(),
            r.json()["refresh_token"]
                .as_str()
                .unwrap_or_default()
                .to_string(),
        );
        need(
            r.status == 200 && !rt2.is_empty() && rt2 != rt1,
            format!(
                "the second Session, signing in with the rotated token: {}",
                r.brief()
            ),
        )?;
        revoke(a, d, &rt1)?;
        let spent = s
            .company
            .record()
            .iter()
            .rfind(|r| r["path"] == "/auth/revoke")
            .map(|r| (r["revoked"].clone(), r["spent"].clone()));
        need(
            spent == Some((json!(false), json!(true))),
            format!(
                "revoking the spent Account Token is recorded as spent, revoking nothing: {spent:?}"
            ),
        )?;
        let r = revoke(a, d, &rt2)?;
        need(r.status == 200, format!("the revocation: {}", r.brief()))?;
        for (which, token) in [("first", &first_token), ("second", &second_token)] {
            need(
                !reads_signed_in(&s, token)?,
                format!("the {which} Session still reads signed in after its sign-in was revoked"),
            )?;
        }
        let r = refresh(a, d, &rt2, &second);
        need(
            r.status == 400 && r.error() == "invalid_grant",
            format!("the revoked rotated Account Token: {}", r.brief()),
        )
    })())
}

fn one_account_per_session(h: &Harness) -> Outcome {
    verdict((|| {
        let s = setup(h, &[])?;
        let (a, d) = (&s.agent, &s.d);
        let (session, rt) = signed_in(a, d, "usr_two", "poppy:read")?;
        revoke(a, d, &rt)?;
        s.company
            .control(json!({"approver": {"account": "acct_2"}}));
        let err = a
            .device_sign_in(d, &session, "poppy:read")
            .err()
            .unwrap_or_default();
        need(
            err.contains("account_mismatch") && !err.contains("refresh_token"),
            format!("signing the Session in to another account: {err}"),
        )
    })())
}

fn rate_limits_with_retry_after(h: &Harness) -> Outcome {
    verdict((|| {
        let s = setup(h, &["retry-after"])?;
        let (a, d) = (&s.agent, &s.d);
        let start = || {
            a.token(
                d,
                &[
                    ("grant_type", JWT_BEARER),
                    ("assertion", &session_assertion(a, d, "usr_wait")),
                ],
                true,
            )
        };
        let r = start();
        let wait = r.header("retry-after").and_then(|v| v.parse::<u64>().ok());
        need(
            r.status == 429 && r.error() == "rate_limited" && wait.is_some(),
            format!("the first request: {}", r.brief()),
        )?;
        let r = start();
        need(
            r.status == 429,
            format!("a request before the wait is over: {}", r.brief()),
        )?;
        need(
            s.company.record().iter().any(|r| r["early"] == true),
            "the fake did not record the request sent before the wait was over",
        )?;
        std::thread::sleep(std::time::Duration::from_secs(wait.unwrap_or(2)));
        let r = start();
        need(
            r.status == 200,
            format!("a request after the wait: {}", r.brief()),
        )
    })())
}

fn mcp_account_errors(h: &Harness) -> Outcome {
    verdict((|| {
        let s = setup(h, &[])?;
        let (a, d) = (&s.agent, &s.d);
        let url = d.apis("mcp").first().cloned().unwrap_or_default();
        let call = |token: &str, tool: &str| {
            mcp(
                &url,
                Some(token),
                "tools/call",
                json!({"name": tool, "arguments": {"order_id": "ord_1", "query": "coat"}}),
            )
        };
        let (_, session) = a.start_session(d, "usr_tools")?;
        let out = bearer(a, d, "usr_tools", &session, &url)?;
        let r = call(&out, "search")?;
        need(
            r.status == 200 && r.json()["result"].is_object(),
            format!("a signed-out search: {}", r.brief()),
        )?;
        let r = call(&out, "order_status")?;
        need(
            r.status == 403 && r.error() == "sign_in_required",
            format!("a signed-out order_status: {}", r.brief()),
        )?;
        let in_bearer = |rt: &str, session: &str| -> Result<String, String> {
            let r = a.token(
                d,
                &[
                    ("grant_type", "refresh_token"),
                    ("refresh_token", rt),
                    ("session_id", session),
                    ("resource", &url),
                ],
                false,
            );
            r.json()["access_token"]
                .as_str()
                .map(str::to_string)
                .ok_or(format!("a signed-in Bearer token: {}", r.brief()))
        };
        s.company
            .control(json!({"approver": {"scope": "poppy:write"}}));
        let (session, rt) = signed_in(a, d, "usr_narrow", "poppy:read poppy:write")?;
        let r = call(&in_bearer(&rt, &session)?, "order_status")?;
        need(
            r.status == 403
                && r.error() == "insufficient_scope"
                && r.header("www-authenticate")
                    .is_some_and(|c| c.contains("scope=\"poppy:read\"")),
            format!("order_status without poppy:read: {}", r.brief()),
        )?;
        s.company
            .control(json!({"approver": {"scope": "poppy:read poppy:write"}}));
        let (session, rt) = signed_in(a, d, "usr_full", "poppy:read")?;
        let r = call(&in_bearer(&rt, &session)?, "order_status")?;
        need(
            r.status == 200 && r.json()["result"].is_object(),
            format!("a signed-in order_status: {}", r.brief()),
        )
    })())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The fake's checks' quotes and agentd's.
    fn all_quotes() -> impl Iterator<Item = &'static Quote> {
        QUOTES.iter().chain(super::super::companies_agent::QUOTES)
    }

    const VENDOR: &str = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../agentd/tests/companies/vendor"
    );

    /// The text of `section` of a page: from the heading whose title is
    /// `section` (a numbered heading's number, or a heading's whole title)
    /// to the next heading at its level or above.
    fn sections<'a>(page: &'a str, section: &str) -> Vec<&'a str> {
        // Each line's start offset and its heading level (0 for prose).
        let mut lines = Vec::new();
        let mut at = 0;
        for line in page.split_inclusive('\n') {
            let hashes = line.bytes().take_while(|b| *b == b'#').count();
            let level = if line.as_bytes().get(hashes) == Some(&b' ') {
                hashes
            } else {
                0
            };
            lines.push((at, level, line.trim()));
            at += line.len();
        }
        let mut out = Vec::new();
        for (i, &(start, level, line)) in lines.iter().enumerate() {
            if level == 0 {
                continue;
            }
            let title = line[level..].trim();
            let numbered = title
                .split_once(' ')
                .is_some_and(|(n, _)| n.trim_end_matches('.') == section);
            if title != section && !numbered {
                continue;
            }
            let end = lines[i + 1..]
                .iter()
                .find(|(_, l, _)| *l > 0 && *l <= level)
                .map_or(page.len(), |(s, _, _)| *s);
            out.push(&page[start..end]);
        }
        out
    }

    /// Every check quotes the draft, every quote is a check's, and each
    /// quote is in the vendored page, verbatim, in the section it names.
    #[test]
    fn every_quote_is_the_drafts_own_words() {
        let ids: Vec<&str> = checks().iter().map(|c| c.id).collect();
        for id in &ids {
            assert!(
                all_quotes().any(|q| q.check == *id),
                "{id} quotes no sentence of the draft"
            );
        }
        for q in all_quotes() {
            assert!(
                ids.contains(&q.check),
                "a quote for {}, which is not a check",
                q.check
            );
            let path = format!("{VENDOR}/{}", q.page.file());
            let page = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{path}: {e}"));
            let found = sections(&page, q.section);
            assert!(
                !found.is_empty(),
                "{}: {} has no section {}",
                q.check,
                q.page.file(),
                q.section
            );
            assert!(
                found.iter().any(|s| s.contains(q.text)),
                "{}: not in {} §{}, verbatim: {:?}",
                q.check,
                q.page.file(),
                q.section,
                q.text
            );
        }
    }

    /// A section ends at the next heading at its level or above, so a quote
    /// from a later section is not found in an earlier one.
    #[test]
    fn a_quote_is_found_only_in_its_section() {
        let page = "# T\n\n## 3. A\n\nalpha\n\n### 3.1 B\n\nbeta\n\n## 4. C\n\ngamma\n";
        assert!(sections(page, "3")[0].contains("beta"));
        assert!(!sections(page, "3")[0].contains("gamma"));
        assert!(!sections(page, "3.1")[0].contains("alpha"));
        assert!(sections(page, "4")[0].contains("gamma"));
        assert!(sections(page, "9").is_empty());
    }
}
