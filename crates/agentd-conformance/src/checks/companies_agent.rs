// SPDX-License-Identifier: AGPL-3.0-only
//! agentd itself as the Personal Agent (rfcs/0047 §10): the real binary,
//! built with `companies`, declared a Company — the fake — and driven only
//! through its configuration. Its client identity is what
//! `--print-company-client` writes, hosted by the [`probe`]'s document
//! server. Each check reads what agentd did from the fake's own record of
//! what reached it, never from agentd, and quotes the sentence of the draft
//! it holds agentd to ([`QUOTES`], verified verbatim in their sections like
//! the fake's own checks).
//!
//! The sign-in checks give agentd a unix A2A listener to ask the User on and
//! let the gate of unowned work open (`agent.ask_human_unowned: gate`); the
//! fake's scripted User signs in at the Company, as a person would, and
//! nobody answers the gate. The Operations and Direct Conversation checks
//! give it a loopback A2A listener with no credential, whose caller is the
//! operator — and with `security.companies.user` left `operator`, the User —
//! and answer the User's gate there as the User would.

use std::time::{Duration, Instant};

use serde_json::Value;

use super::companies::{Page, Quote};
use crate::checks::probe::{Agent, Company};
use crate::checks::util::{free_port, rpc, rpc_value, text_params, wait_ready};
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
        check(
            "companies/agent-sends-a-company-none-of-its-correlators",
            "the calls a workflow makes to two Companies carry none of agentd's instance, run, context, principal or label keys, and no trace or idempotency key seen at one is seen at the other; the handshake and tool list carry no trace context, no clientInfo.title and no AAuth signature",
            sends_a_company_none_of_its_correlators,
        ),
        check(
            "companies/agent-acts-at-one-company-for-another-only-with-the-user",
            "a run that read one Company's answer sends another Company nothing when the User cannot be asked, with agent.approval and the ask_human fallback both auto",
            acts_at_one_company_for_another_only_with_the_user,
        ),
        check(
            "companies/agent-waits-out-a-companys-retry-after",
            "against the retry-after variant, whose token endpoint and MCP server each answer 429 with Retry-After, nothing agentd sends reaches the Company before the wait is over, and it joins and calls once it is",
            waits_out_a_companys_retry_after,
        ),
        check(
            "companies/agent-never-repeats-a-call-that-may-have-run",
            "a tools/call that got no answer in time, or a 502, after it was sent is not sent again — by the transport or by the step's retry: — so no run's call reaches the Company twice: each run's calls go out in one trace of its own there, and no trace id is seen twice",
            never_repeats_a_call_that_may_have_run,
        ),
        check(
            "companies/agent-keeps-tokens-out-of-its-log",
            "no Session Token or Bearer token the Company issued appears in agentd's log, at debug level with content logging on",
            keeps_tokens_out_of_its_log,
        ),
        check(
            "companies/agent-signs-in-only-as-the-user-does",
            "a workflow's company.sign_in starts a Device Sign-In that asks for its scopes with the client assertion, never opens the User's verification page itself, polls an interval apart for the current Session, and once the User has approved is signed in: the account call it makes next is answered",
            signs_in_only_as_the_user_does,
        ),
        check(
            "companies/agent-keeps-account-tokens-secret",
            "the Account Token a sign-in gave agentd reaches the Company only in the refresh_token member of a token request — never in a URL, a header or another body — and appears in no line of agentd's log and no file of its state",
            keeps_account_tokens_secret,
        ),
        check(
            "companies/agent-uses-the-newest-account-token",
            "against the rotation variant, agentd uses each Account Token the Company returns from then on, across a restart: no token request it makes presents a spent one",
            uses_the_newest_account_token,
        ),
        check(
            "companies/agent-signs-out-by-revoking",
            "company.sign_out revokes the Account Token at the revocation endpoint (token_type_hint refresh_token, the client assertion), and the Session goes on signed out: the next token is the assertion's",
            signs_out_by_revoking,
        ),
        check(
            "companies/agent-retries-a-message-with-its-id",
            "a message whose answer was lost — a 502 after the Company took it — is sent again with the same id, and the Company answers it as the message it has; another message has an id of its own",
            retries_a_message_with_its_id,
        ),
        check(
            "companies/agent-says-who-wrote-each-message",
            "every message agentd sends in a conversation says sender agent, and none sends role",
            says_who_wrote_each_message,
        ),
        check(
            "companies/agent-skips-what-it-does-not-recognize",
            "agentd talks to the Company Agent over the poppy protocol, never the type it does not speak listed before it, and the stream it appends the conversation's events to holds no event of the type the draft does not define and no text-delta piece",
            skips_what_it_does_not_recognize,
        ),
        check(
            "companies/agent-reconnects-a-stream-after-the-last-event",
            "a stream of a conversation's events that the Company ends is opened again with Last-Event-ID the last event agentd handled, and no cursor",
            reconnects_a_stream_after_the_last_event,
        ),
        check(
            "companies/agent-reads-on-from-the-cursor-it-saved",
            "after a restart agentd's first read of an open conversation names the cursor it saved, the last event it handled",
            reads_on_from_the_cursor_it_saved,
        ),
        check(
            "companies/agent-says-the-user-is-not-available",
            "when the Company Agent asks to talk with the User and nobody can ask them, agentd says in the conversation that the User is not available, once, as the agent, and starts no Direct Conversation",
            says_the_user_is_not_available,
        ),
        check(
            "companies/agent-sends-a-session-token-with-every-request",
            "every conversation request, and every API call an http step or company.api makes, carries the DPoP Session Token with a proof over it that the fake validated",
            sends_a_session_token_with_every_request,
        ),
        check(
            "companies/agent-reuses-an-idempotency-key-on-retry",
            "an API write whose operation declares Idempotency-Key, its answer lost, is sent again with the same version 4 UUID, and gets the first answer",
            reuses_an_idempotency_key_on_retry,
        ),
        check(
            "companies/agent-lists-the-operations-extension",
            "with client.extensions [operations], the client metadata document the fake fetched lists operations at version 1, and the fake proposes the exchange as an operation; without it, the exchange is made, and nothing agentd sends confirms anything",
            lists_the_operations_extension,
        ),
        check(
            "companies/agent-confirms-only-what-the-user-approved",
            "a run's confirm of a proposed operation, with nobody to ask and agent.approval and the ask_human fallback auto, sends nothing to the operations endpoint; with the User asked at their gate — its text naming the Company and quoting the revision's summary — and answering approve, the confirm goes once, approved_by user, with the revision the gate showed",
            confirms_only_what_the_user_approved,
        ),
        check(
            "companies/agent-asks-again-when-the-terms-change",
            "a confirm the Company answers terms_changed is not sent again under the old approval: the User is asked about the new revision, and only their approve of it sends a confirm, of that revision",
            asks_again_when_the_terms_change,
        ),
        check(
            "companies/agent-reads-an-operation-in-progress-after-retry-after",
            "a confirm answered in_progress with Retry-After is followed by a read of the operation no sooner than the wait, with the same operation id, until it is final",
            reads_an_operation_in_progress_after_retry_after,
        ),
        check(
            "companies/agent-retries-a-confirm-with-the-same-operation",
            "a confirm whose answer is lost after the Company took it is sent again with the same operation id and revision, and the tool that proposed the operation is not called again",
            retries_a_confirm_with_the_same_operation,
        ),
        check(
            "companies/agent-asks-the-user-before-a-direct-conversation",
            "when the Company Agent asks to talk with the User, agentd asks the User at their gate and starts no Direct Conversation until they answer; their answer is the first message, as they wrote it, sender human, with parent_conversation_id the conversation the Company asked in",
            asks_the_user_before_a_direct_conversation,
        ),
        check(
            "companies/agent-says-a-decline-in-the-conversation",
            "when the User declines to talk with the Company directly, agentd says so in the conversation the Company asked in, once, as the agent, and starts no Direct Conversation",
            says_a_decline_in_the_conversation,
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

const fn ops(check: &'static str, section: &'static str, text: &'static str) -> Quote {
    Quote {
        check,
        page: Page::Operations,
        section,
        text,
    }
}

const fn guides(check: &'static str, section: &'static str, text: &'static str) -> Quote {
    Quote {
        check,
        page: Page::Guides,
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
    spec(
        "companies/agent-sends-a-company-none-of-its-correlators",
        "4.2",
        "It is opaque and different for each Company, so Companies can't match Users with each other.",
    ),
    spec(
        "companies/agent-acts-at-one-company-for-another-only-with-the-user",
        "8.1",
        "A Personal Agent MUST NOT act at one Company, or send it information, because content from another Company asked it to.",
    ),
    spec(
        "companies/agent-acts-at-one-company-for-another-only-with-the-user",
        "8.1",
        "Only the User decides what the Personal Agent does.",
    ),
    spec(
        "companies/agent-waits-out-a-companys-retry-after",
        "4.3",
        "The Personal Agent MUST wait at least that long before sending the request again.",
    ),
    guides(
        "companies/agent-waits-out-a-companys-retry-after",
        "Rate limits",
        "Personal agents: keep track of limits for each company and share them across your tasks and users, rather than letting each task find the limit on its own.",
    ),
    spec(
        "companies/agent-never-repeats-a-call-that-may-have-run",
        "4.3",
        "A request that changes something then relies on being idempotent, so a replay doesn't repeat it.",
    ),
    guides(
        "companies/agent-keeps-tokens-out-of-its-log",
        "Handling secrets",
        "Keep them out of logs, telemetry, error reports, model context, conversation messages, and URLs.",
    ),
    spec(
        "companies/agent-signs-in-only-as-the-user-does",
        "4.4",
        "The types work on trust: a Personal Agent MUST use direct or Device Sign-In only when the User signs in themselves.",
    ),
    spec(
        "companies/agent-signs-in-only-as-the-user-does",
        "4.6",
        "Every `interval` seconds, the Personal Agent asks the `token_endpoint` whether the User has finished:",
    ),
    spec(
        "companies/agent-signs-in-only-as-the-user-does",
        "4.6",
        "Opening the link MUST NOT approve the request by itself.",
    ),
    spec(
        "companies/agent-keeps-account-tokens-secret",
        "4.8",
        "Personal Agents MUST store Account Tokens as secrets, and MUST NOT put them in model context, conversation messages, logs, or URLs.",
    ),
    spec(
        "companies/agent-uses-the-newest-account-token",
        "4.8",
        "A Company MAY return a new Account Token in `refresh_token`. The Personal Agent then MUST use the new one from then on.",
    ),
    spec(
        "companies/agent-signs-out-by-revoking",
        "4.9",
        "Signing out removes the Personal Agent's access to the User's account.",
    ),
    spec(
        "companies/agent-signs-out-by-revoking",
        "4.9",
        "Usually those Sessions continue signed out, so a cart or an open conversation carries on: the Personal Agent renews them with its assertion (4.2) and gets signed-out Session Tokens.",
    ),
    spec(
        "companies/agent-retries-a-message-with-its-id",
        "7.3",
        "To retry a message, it sends it again with the same ID.",
    ),
    spec(
        "companies/agent-retries-a-message-with-its-id",
        "7.3",
        "The Personal Agent chooses each message's `id`, and MUST make it unique for the User.",
    ),
    spec(
        "companies/agent-says-who-wrote-each-message",
        "7.8",
        "On the Personal Agent's side, the Personal Agent MUST set each message's `sender`: `agent` when the Personal Agent wrote it, and `human` only when it carries the User's own words.",
    ),
    spec(
        "companies/agent-says-who-wrote-each-message",
        "7.4",
        "The Personal Agent doesn't send it, and the Company ignores it if sent.",
    ),
    spec(
        "companies/agent-skips-what-it-does-not-recognize",
        "7.1",
        "It skips unfamiliar types.",
    ),
    spec(
        "companies/agent-skips-what-it-does-not-recognize",
        "7.5",
        "Personal Agents MUST skip event types they don't recognize, so later versions and extensions can add types.",
    ),
    spec(
        "companies/agent-skips-what-it-does-not-recognize",
        "7.6",
        "It MUST NOT act on text from a `text-delta` event.",
    ),
    spec(
        "companies/agent-reconnects-a-stream-after-the-last-event",
        "7.6",
        "The Personal Agent reconnects with `Last-Event-ID` set to the last event it handled, in place of `cursor`.",
    ),
    spec(
        "companies/agent-reads-on-from-the-cursor-it-saved",
        "7.5",
        "The Personal Agent handles events in order and saves the cursor after them.",
    ),
    spec(
        "companies/agent-says-the-user-is-not-available",
        "7.10",
        "If they decline or aren't available, it says so in the conversation.",
    ),
    spec(
        "companies/agent-sends-a-session-token-with-every-request",
        "4.3",
        "Personal Agents MUST send a Session Token on every API and conversation request, and MUST NOT leave it out to appear to the Company as a person on their own.",
    ),
    guides(
        "companies/agent-reuses-an-idempotency-key-on-retry",
        "Retries",
        "Send an `Idempotency-Key` on API writes that accept one. Use a random UUID (version 4), and reuse the same key when you retry the same request.",
    ),
    ops(
        "companies/agent-lists-the-operations-extension",
        "2",
        "A Personal Agent that supports operations lists the extension in its client metadata document (spec 4.1):",
    ),
    ops(
        "companies/agent-confirms-only-what-the-user-approved",
        "5.1",
        "Before confirming, the Personal Agent MUST have the User's approval for the revision it confirms, in one of two ways:",
    ),
    ops(
        "companies/agent-confirms-only-what-the-user-approved",
        "5.1",
        "The Personal Agent MUST show the User the Company's name and the revision's `summary`, and MUST take the User's answer through its own interface.",
    ),
    ops(
        "companies/agent-asks-again-when-the-terms-change",
        "5.1",
        "An approval covers one revision. When a new revision arrives, the Personal Agent needs a new approval for it, unless a standing permission covers it.",
    ),
    ops(
        "companies/agent-reads-an-operation-in-progress-after-retry-after",
        "6",
        "While it is `in_progress`, the Company SHOULD include `Retry-After`, and the Personal Agent reads again after that time.",
    ),
    ops(
        "companies/agent-retries-a-confirm-with-the-same-operation",
        "6",
        "If a confirm request fails or times out, the Personal Agent retries it or reads the operation, with the same operation ID. It MUST NOT ask for a new proposal of the same action until the operation is final.",
    ),
    spec(
        "companies/agent-asks-the-user-before-a-direct-conversation",
        "7.10",
        "Only the Personal Agent starts Direct Conversations, so the User decides when they're brought in.",
    ),
    spec(
        "companies/agent-asks-the-user-before-a-direct-conversation",
        "7.10",
        "The Personal Agent sends the User's messages as written, with `sender: \"human\"` (7.8), and shows the Company's replies to the User.",
    ),
    spec(
        "companies/agent-says-a-decline-in-the-conversation",
        "7.10",
        "If they decline or aren't available, it says so in the conversation.",
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
    /// The Companies declared beside it, by entry.
    more: Vec<(String, Company)>,
    _host: Agent,
    tmp: TempDir,
    cfg: String,
    daemon: Option<Daemon>,
    /// The loopback A2A listener's `host:port`, when it has one.
    a2a: Option<String>,
}

/// What a check declares beside the one Company `shop`.
#[derive(Default)]
struct Extra<'a> {
    /// More Companies: each entry and its fake's variants.
    more: &'a [(&'a str, &'a [&'a str])],
    /// Fields every company entry gets.
    entry: &'a str,
    /// Lines under `agent:`.
    agent: &'a str,
    /// Configuration after the services: workflows.
    rest: &'a str,
    /// The entry lets agentd sign the User in: Device Sign-In for both of
    /// the draft's scopes, the Account Token sealed under a seal key of the
    /// run's own, a unix A2A listener to ask the User on, and the gate of
    /// unowned work let open.
    signs_in: bool,
    /// Fields the client identity gets: `extensions`.
    client: &'a str,
    /// A loopback A2A listener with no credential — its caller the operator,
    /// who is the User — and the gate of unowned work let open.
    listens: bool,
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
    prepare_with(h, variants, &Extra::default())
}

/// [`prepare`], with what `extra` declares beside it.
fn prepare_with(h: &Harness, variants: &[&str], extra: &Extra) -> Result<Run, String> {
    let tmp = h.tempdir();
    let company = Company::spawn(h, &tmp, variants);
    let more: Vec<(String, Company)> = extra
        .more
        .iter()
        .map(|(name, v)| (name.to_string(), Company::spawn(h, &tmp, v)))
        .collect();
    let host = Agent::start();
    let dir = tmp.path().display().to_string();
    let cfg = format!("{dir}/agent.yaml");
    let signs_in = if extra.signs_in {
        ", sign_in: [device], scopes: [\"poppy:read\", \"poppy:write\"]"
    } else {
        ""
    };
    let entry = |name: &str, origin: &str| {
        format!(
            "  {name}: {{kind: company, endpoint: \"{origin}\", allow_private: true{signs_in}{}}}\n",
            extra.entry
        )
    };
    let (mut agent, mut companies, mut listener) =
        (extra.agent.to_string(), String::new(), String::new());
    if extra.signs_in {
        std::fs::write(format!("{dir}/seal"), "5e".repeat(32)).map_err(|e| e.to_string())?;
        companies = format!("    seal_key: \"{{{{secret-file:{dir}/seal}}}}\"\n");
        listener = format!("a2a:\n  listen: \"unix://{dir}/a2a.sock\"\n");
        agent.push_str("  ask_human_unowned: gate\n");
    }
    let mut a2a = None;
    if extra.listens {
        let addr = format!("127.0.0.1:{}", free_port());
        listener = format!("a2a:\n  listen: \"http://{addr}\"\n");
        agent.push_str("  ask_human_unowned: gate\n");
        a2a = Some(addr);
    }
    let mut services = entry("shop", &company.origin);
    for (name, c) in &more {
        services.push_str(&entry(name, &c.origin));
    }
    std::fs::write(
        &cfg,
        format!(
            "agent:\n  name: conformance-pa\n  instruction: Shop.\n  prompt: Wait.\n  preflight: never\n{agent}\
             intelligence: {{endpoints: \"mock:final\", model: mock}}\n\
             store: {{kind: file, file: {{path: \"{dir}/state\"}}}}\n\
             lifecycle: {{run_until: drained}}\n\
             observability: {{log_level: debug, log_content: true}}\n\
             security:\n  tls_ca: \"{ca}\"\n  companies:\n    client: {{id: \"{o}/companies/client.json\", name: \"agentd conformance\", key_file: \"{dir}/client-key\"{client}}}\n{companies}\
             services:\n{services}{listener}{rest}",
            ca = tls_fixture("ca.pem").display(),
            o = host.origin,
            rest = extra.rest,
            client = extra.client,
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
        more,
        _host: host,
        tmp,
        cfg,
        daemon: None,
        a2a,
    })
}

/// What reached `c`, its steering and reading aside.
fn record_of(c: &Company) -> Vec<Value> {
    c.record()
        .into_iter()
        .filter(|r| {
            !r["path"]
                .as_str()
                .unwrap_or_default()
                .starts_with("/_mock/")
        })
        .collect()
}

fn tools_calls(record: &[Value]) -> Vec<&Value> {
    record.iter().filter(|r| r["mcp"] == "tools/call").collect()
}

/// Wait up to `secs` for `done` to hold, or say `what` never happened.
fn wait(secs: u64, what: &str, mut done: impl FnMut() -> bool) -> Result<(), String> {
    let deadline = Instant::now() + Duration::from_secs(secs);
    while !done() {
        if Instant::now() > deadline {
            return Err(format!("{what} never happened"));
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    Ok(())
}

/// A workflow `name` that runs every second, whose `steps` (YAML lines under
/// `steps:`, each depending on the start `s` or a step) call Companies, and
/// that finishes once the steps `ends` names have.
fn every_second(name: &str, steps: &str, ends: &str) -> String {
    format!(
        "workflows:\n  - name: {name}\n    steps:\n      s: {{kind: schedule, every: 1s}}\n{steps}      f: {{kind: finish, depends_on: [{ends}], status: completed}}\n"
    )
}

/// A workflow `name` that runs once, at boot, whose `steps` (YAML lines
/// under `steps:`, each depending on the start `s` or a step) end with
/// those `ends` names.
fn once(name: &str, steps: &str, ends: &str) -> String {
    format!(
        "workflows:\n  - name: {name}\n    steps:\n      s: {{kind: once}}\n{steps}      f: {{kind: finish, depends_on: [{ends}], status: completed}}\n"
    )
}

/// The steps of a workflow that signs the User in and then calls an
/// account tool.
const SIGN_IN_THEN_ORDER: &str = "      si: {kind: tool, name: company.sign_in, args: {company: shop}, depends_on: [s]}\n      o: {kind: tool, name: shop.order_status, args: {order_id: o1}, depends_on: [si]}\n";

/// Each Account Token the fake issued.
fn account_tokens(record: &[Value]) -> Vec<String> {
    record
        .iter()
        .filter_map(|r| r["issued"]["refresh_token"].as_str().map(str::to_string))
        .collect()
}

/// A sign-in, then an account call that answered: the run's record.
fn signed_in_and_called(h: &Harness, variants: &[&str], log: Option<&str>) -> Result<Run, String> {
    let mut run = prepare_with(
        h,
        variants,
        &Extra {
            signs_in: true,
            rest: &once("signin", SIGN_IN_THEN_ORDER, "o"),
            ..Extra::default()
        },
    )?;
    run.daemon = Some(match log {
        Some(file) => h.spawn_logged(&["--config", &run.cfg], &run.tmp.path().join(file)),
        None => h.spawn(&["--config", &run.cfg]),
    });
    wait(40, "a signed-in account call", || {
        tools_calls(&run.record())
            .iter()
            .any(|c| c["status"] == 200)
    })?;
    Ok(run)
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

/// The `_meta` keys of agentd's own a Company must never be sent under the
/// strict privacy profile (RFC 0047 §5.10).
const CORRELATORS: [&str; 7] = [
    "agent/instance",
    "agent/run_id",
    "agent/run",
    "agent/ctx",
    "agent/acting_for",
    "agent/labels",
    "agent/attempt",
];

fn sends_a_company_none_of_its_correlators(h: &Harness) -> Outcome {
    verdict((|| {
        let mut run = prepare_with(
            h,
            &[],
            &Extra {
                more: &[("travel", &[])],
                rest: &every_second(
                    "both",
                    "      a: {kind: tool, name: shop.search, args: {query: boots}, depends_on: [s]}\n      b: {kind: tool, name: travel.search, args: {query: flights}, depends_on: [s]}\n",
                    "a, b",
                ),
                ..Extra::default()
            },
        )?;
        run.daemon = Some(h.spawn(&["--config", &run.cfg]));
        let travel = &run.more[0].1;
        wait(40, "a call to each Company", || {
            tools_calls(&record_of(&run.company)).len() >= 2
                && tools_calls(&record_of(travel)).len() >= 2
        })?;
        let (shop, travel) = (record_of(&run.company), record_of(travel));
        let seen = |r: &[Value], key: &str| -> Vec<String> {
            tools_calls(r)
                .iter()
                .filter_map(|c| c[key].as_str().map(str::to_string))
                .collect()
        };
        for (company, record) in [("shop", &shop), ("travel", &travel)] {
            for c in tools_calls(record) {
                let meta: Vec<&str> = c["meta"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str)
                    .collect();
                if let Some(k) = CORRELATORS.iter().find(|k| meta.contains(k)) {
                    return Err(format!("{company} was sent {k}: {c}"));
                }
            }
            for r in record
                .iter()
                .filter(|r| r["path"] == "/mcp" && r["mcp"] != "tools/call")
            {
                need(
                    r["trace_id"].is_null() && r["client_title"].is_null(),
                    format!("{company}'s connection request carries agentd's trace or label: {r}"),
                )?;
            }
            for r in record.iter() {
                need(
                    r["aauth"].is_null() && r["trace_header"].is_null(),
                    format!("{company} was sent an AAuth signature or a trace header: {r}"),
                )?;
            }
        }
        for key in ["trace_id", "idempotency_key"] {
            let at_shop = seen(&shop, key);
            need(
                !at_shop.is_empty(),
                format!("no call to shop carries a {key}"),
            )?;
            if let Some(both) = seen(&travel, key).iter().find(|k| at_shop.contains(k)) {
                return Err(format!("shop and travel were both sent the {key} {both}"));
            }
        }
        Ok(())
    })())
}

fn acts_at_one_company_for_another_only_with_the_user(h: &Harness) -> Outcome {
    verdict((|| {
        let mut run = prepare_with(
            h,
            &[],
            &Extra {
                more: &[("travel", &[])],
                agent: "  approval: auto\n  ask_human_fallback: auto\n",
                rest: &every_second(
                    "cross",
                    "      a: {kind: tool, name: shop.search, args: {query: boots}, depends_on: [s]}\n      b: {kind: tool, name: travel.search, args: {query: flights}, depends_on: [a]}\n",
                    "b",
                ),
                ..Extra::default()
            },
        )?;
        run.daemon = Some(h.spawn(&["--config", &run.cfg]));
        let travel = &run.more[0].1;
        wait(40, "travel joined and shop answered two runs", || {
            record_of(travel).iter().any(|r| r["mcp"] == "tools/list")
                && tools_calls(&record_of(&run.company)).len() >= 3
        })?;
        let sent = record_of(travel);
        need(
            tools_calls(&sent).is_empty(),
            format!(
                "travel was called by work holding shop's content: {:?}",
                tools_calls(&sent)
            ),
        )
    })())
}

fn waits_out_a_companys_retry_after(h: &Harness) -> Outcome {
    verdict((|| {
        let mut run = prepare_with(
            h,
            &["retry-after"],
            &Extra {
                rest: &every_second(
                    "paced",
                    "      t: {kind: tool, name: shop.search, args: {query: boots}, depends_on: [s]}\n",
                    "t",
                ),
                ..Extra::default()
            },
        )?;
        run.daemon = Some(h.spawn(&["--config", &run.cfg]));
        wait(40, "a call after the tools/call's 429", || {
            tools_calls(&run.record())
                .iter()
                .any(|c| c["status"] == 200)
        })?;
        let record = run.record();
        need(
            record.iter().any(|r| r["status"] == 429),
            "the variant never asked agentd to wait",
        )?;
        match record.iter().find(|r| r["early"] == true) {
            Some(r) => Err(format!("sent before the Company's wait was over: {r}")),
            None => Ok(()),
        }
    })())
}

fn never_repeats_a_call_that_may_have_run(h: &Harness) -> Outcome {
    verdict((|| {
        let mut run = prepare_with(
            h,
            &[],
            &Extra {
                entry: ", timeout: 1s",
                rest: &every_second(
                    "retried",
                    "      t: {kind: tool, name: shop.search, args: {query: boots}, depends_on: [s], retry: {max: 3, backoff: 10ms}}\n",
                    "t",
                ),
                ..Extra::default()
            },
        )?;
        run.daemon = Some(h.spawn(&["--config", &run.cfg]));
        wait(40, "a first call", || {
            !tools_calls(&run.record()).is_empty()
        })?;
        run.company.control(serde_json::json!({
            "stall": {"method": "tools/call", "times": 1, "ms": 2500}
        }));
        run.company.control(serde_json::json!({
            "fail": {"method": "tools/call", "times": 1, "status": 502}
        }));
        let before = tools_calls(&run.record()).len();
        wait(40, "the calls after the stalled and the failed one", || {
            tools_calls(&run.record()).len() >= before + 4
        })?;
        std::thread::sleep(Duration::from_millis(2600));
        let record = run.record();
        // Under the strict profile a run's calls share one trace at the
        // Company, and each scheduled run is a trace of its own: a call sent
        // twice is one trace id seen twice.
        let traces: Vec<&str> = tools_calls(&record)
            .iter()
            .filter_map(|c| c["trace_id"].as_str())
            .collect();
        need(
            record
                .iter()
                .any(|r| r["stalled"] == true || r["failed"] == 502),
            "the fake never held or failed a call",
        )?;
        need(!traces.is_empty(), "no call carried a trace")?;
        for (i, t) in traces.iter().enumerate() {
            if traces[..i].contains(t) {
                return Err(format!(
                    "a run's call reached the Company twice (trace {t})"
                ));
            }
        }
        Ok(())
    })())
}

fn keeps_tokens_out_of_its_log(h: &Harness) -> Outcome {
    verdict((|| {
        let mut run = prepare_with(
            h,
            &[],
            &Extra {
                rest: &every_second(
                    "calls",
                    "      t: {kind: tool, name: shop.search, args: {query: boots}, depends_on: [s]}\n",
                    "t",
                ),
                ..Extra::default()
            },
        )?;
        let log = run.tmp.path().join("agentd.log");
        run.daemon = Some(h.spawn_logged(&["--config", &run.cfg], &log));
        wait(40, "two calls", || tools_calls(&run.record()).len() >= 2)?;
        run.stop();
        let text = std::fs::read_to_string(&log).map_err(|e| e.to_string())?;
        need(!text.is_empty(), "agentd wrote no log")?;
        let issued: Vec<String> = run
            .record()
            .iter()
            .filter_map(|r| r["issued"]["access_token"].as_str().map(str::to_string))
            .collect();
        need(issued.len() >= 2, "the Company issued no tokens")?;
        match issued.iter().find(|t| text.contains(t.as_str())) {
            Some(_) => Err("a token the Company issued is in agentd's log".into()),
            None => Ok(()),
        }
    })())
}

fn signs_in_only_as_the_user_does(h: &Harness) -> Outcome {
    verdict((|| {
        let run = signed_in_and_called(h, &[], None)?;
        let record = run.record();
        let auth = record
            .iter()
            .find(|r| r["path"] == "/auth/device")
            .ok_or("agentd started no Device Sign-In")?;
        need(
            auth["scope"] == "poppy:read poppy:write",
            format!("the device authorization asked for no scopes, or others: {auth}"),
        )?;
        need(
            auth["client_assertion"].is_object(),
            format!("the device authorization carried no client assertion: {auth}"),
        )?;
        need(
            record.iter().all(|r| r["path"] != "/auth/device/verify"),
            "agentd opened the User's verification page itself",
        )?;
        let polls: Vec<&Value> = record
            .iter()
            .filter(|r| r["grant"] == "urn:ietf:params:oauth:grant-type:device_code")
            .collect();
        need(
            polls.len() >= 2,
            format!("agentd polled {} times", polls.len()),
        )?;
        let at = |r: &Value| r["at_ms"].as_u64().unwrap_or_default();
        let mut last = at(auth);
        for p in &polls {
            need(
                at(p) + 50 >= last + 1000,
                format!("a poll came sooner than the interval after the last: {p}"),
            )?;
            need(
                p["session_id"].is_string(),
                format!("a poll named no Session: {p}"),
            )?;
            last = at(p);
        }
        need(
            polls.iter().any(|p| p["status"] == 200),
            "no poll was answered with the sign-in",
        )
    })())
}

fn keeps_account_tokens_secret(h: &Harness) -> Outcome {
    verdict((|| {
        let mut run = signed_in_and_called(h, &[], Some("agentd.log"))?;
        run.stop();
        let record = run.record();
        let issued = account_tokens(&record);
        need(!issued.is_empty(), "the Company issued no Account Token")?;
        if let Some(r) = record.iter().find(|r| r["account_token_leaked"] == true) {
            return Err(format!(
                "a request carried an Account Token outside a token request's refresh_token: {} {}",
                r["method"], r["path"]
            ));
        }
        let mut files = Vec::new();
        let mut dirs = vec![run.tmp.path().to_path_buf()];
        while let Some(d) = dirs.pop() {
            for e in std::fs::read_dir(&d).map_err(|e| e.to_string())?.flatten() {
                let p = e.path();
                if p.is_dir() {
                    dirs.push(p);
                } else if p.is_file() {
                    files.push(p);
                }
            }
        }
        for f in files {
            let bytes = std::fs::read(&f).unwrap_or_default();
            let text = String::from_utf8_lossy(&bytes);
            if issued.iter().any(|t| text.contains(t.as_str())) {
                return Err(format!(
                    "an Account Token in the clear in {}",
                    f.file_name().unwrap_or_default().to_string_lossy()
                ));
            }
        }
        Ok(())
    })())
}

fn uses_the_newest_account_token(h: &Harness) -> Outcome {
    verdict((|| {
        let mut run = signed_in_and_called(h, &["rotation"], None)?;
        run.stop();
        let before = run.record().len();
        run.daemon = Some(h.spawn(&["--config", &run.cfg]));
        wait(40, "the Account Token used after the restart", || {
            run.record()[before..]
                .iter()
                .any(|r| r["grant"] == "refresh_token" && r["status"] == 200)
        })?;
        let record = run.record();
        need(
            account_tokens(&record).len() >= 3,
            "the Company rotated the Account Token too few times to tell",
        )?;
        match record
            .iter()
            .find(|r| r["grant"] == "refresh_token" && r["error"] == "invalid_grant")
        {
            Some(r) => Err(format!("a spent Account Token was presented: {r}")),
            None => Ok(()),
        }
    })())
}

fn signs_out_by_revoking(h: &Harness) -> Outcome {
    verdict((|| {
        let mut run = prepare_with(
            h,
            &[],
            &Extra {
                signs_in: true,
                rest: &once(
                    "signout",
                    "      si: {kind: tool, name: company.sign_in, args: {company: shop}, depends_on: [s]}\n      so: {kind: tool, name: company.sign_out, args: {company: shop}, depends_on: [si]}\n      q: {kind: tool, name: shop.search, args: {query: boots}, depends_on: [so]}\n",
                    "q",
                ),
                ..Extra::default()
            },
        )?;
        run.daemon = Some(h.spawn(&["--config", &run.cfg]));
        wait(40, "a call after the sign-out", || {
            let r = run.record();
            r.iter().any(|x| x["path"] == "/auth/revoke")
                && tools_calls(&r).iter().any(|c| c["status"] == 200)
        })?;
        let record = run.record();
        let at = record
            .iter()
            .position(|r| r["path"] == "/auth/revoke")
            .ok_or("no revocation")?;
        let revoke = &record[at];
        need(
            revoke["revoked"] == true && revoke["token_type_hint"] == "refresh_token",
            format!("the revocation did not revoke the Account Token: {revoke}"),
        )?;
        need(
            revoke["client_assertion"].is_object(),
            format!("the revocation carried no client assertion: {revoke}"),
        )?;
        let next = record[at..]
            .iter()
            .find(|r| r["path"] == "/auth/token")
            .ok_or("no token request after the sign-out")?;
        need(
            next["grant"] == "urn:ietf:params:oauth:grant-type:jwt-bearer"
                && next["session_id"].is_string(),
            format!("the Session was not renewed signed out with its assertion: {next}"),
        )
    })())
}

// ---- Conversations and APIs (RFC 0047 P3) -----------------------------------

/// The Company used through its agent and its APIs.
const TALKS: &str = ", interfaces: [agent, openapi], api_allow: [\"GET /orders/{order_id}\", \"POST /orders/{order_id}/exchanges\", \"POST /notes\"]";

/// Two messages at boot: the first starts a conversation, the second goes on
/// in it.
const TWO_MESSAGES: &str = "      c: {kind: tool, name: company.converse, args: {company: shop, text: \"first\"}, depends_on: [s]}\n      m: {kind: tool, name: company.converse, args: {company: shop, conversation: \"{{steps.c.output.conversation}}\", text: \"second\"}, depends_on: [c]}\n";

/// agentd, the fake steered with `controls` first, running the workflows
/// `rest` declares.
fn talked(h: &Harness, controls: &[Value], rest: &str) -> Result<Run, String> {
    let mut run = prepare_with(
        h,
        &[],
        &Extra {
            entry: TALKS,
            rest,
            ..Extra::default()
        },
    )?;
    for c in controls {
        run.company.control(c.clone());
    }
    run.daemon = Some(h.spawn(&["--config", &run.cfg]));
    Ok(run)
}

/// The messages agentd sent, as the fake recorded them.
fn messages(record: &[Value]) -> Vec<&Value> {
    record
        .iter()
        .filter(|r| r["message_id"].is_string())
        .collect()
}

/// Every event the store under `dir` holds on `stream`, in order.
fn stream_events(dir: &std::path::Path, stream: &str) -> Vec<Value> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(p) = stack.pop() {
        for e in std::fs::read_dir(&p).into_iter().flatten().flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
            } else if let Ok(v) =
                serde_json::from_slice::<Value>(&std::fs::read(&p).unwrap_or_default())
                && v["kind"] == "event"
                && v["state"]["stream"] == stream
            {
                out.push(v["state"].clone());
            }
        }
    }
    out.sort_by_key(|e| e["seq"].as_u64());
    out
}

fn retries_a_message_with_its_id(h: &Harness) -> Outcome {
    verdict((|| {
        let run = talked(
            h,
            &[
                serde_json::json!({"http_fail": {"method": "POST", "path": "/poppy/conversations/cnv_", "times": 1, "status": 502}}),
            ],
            &once("talk", TWO_MESSAGES, "m"),
        )?;
        wait(40, "the second message answered", || {
            messages(&run.record())
                .iter()
                .any(|m| m["text"] == "second" && m["duplicate"] == true)
        })?;
        let record = run.record();
        let sent = messages(&record);
        let second: Vec<&&Value> = sent.iter().filter(|m| m["text"] == "second").collect();
        need(
            second.len() == 2,
            format!("the lost message was not sent again: {second:?}"),
        )?;
        need(
            second[0]["failed"] == 502,
            "the first attempt was not the one that failed",
        )?;
        need(
            second[0]["message_id"] == second[1]["message_id"],
            "the retry carried another id",
        )?;
        let first = sent
            .iter()
            .find(|m| m["text"] == "first")
            .ok_or("no first message")?;
        need(
            first["message_id"] != second[0]["message_id"],
            "two messages shared an id",
        )
    })())
}

fn says_who_wrote_each_message(h: &Harness) -> Outcome {
    verdict((|| {
        let run = talked(h, &[], &once("talk", TWO_MESSAGES, "m"))?;
        wait(40, "two messages", || messages(&run.record()).len() >= 2)?;
        for m in messages(&run.record()) {
            need(
                m["sender"] == "agent",
                format!("a message says sender {}", m["sender"]),
            )?;
            need(
                m["role"].is_null(),
                format!("a message sent role {}", m["role"]),
            )?;
        }
        Ok(())
    })())
}

fn skips_what_it_does_not_recognize(h: &Harness) -> Outcome {
    verdict((|| {
        let run = talked(h, &[], &once("talk", TWO_MESSAGES, "m"))?;
        let state = run.tmp.path().join("state");
        wait(40, "the Company's reply on the stream", || {
            stream_events(&state, "company.shop")
                .iter()
                .any(|e| e["data"].to_string().contains("You said: second"))
        })?;
        let held: String = stream_events(&state, "company.shop")
            .iter()
            .map(|e| e.to_string())
            .collect();
        need(
            !held.contains("x-mock-receipt"),
            "the stream holds an event of a type the draft does not define",
        )?;
        need(
            !held.contains("DELTA:"),
            "the stream holds text from a text-delta piece",
        )?;
        need(
            run.record().iter().all(|r| r["path"] != "/voice"),
            "agentd used the conversation protocol it does not speak",
        )
    })())
}

fn reconnects_a_stream_after_the_last_event(h: &Harness) -> Outcome {
    verdict((|| {
        let run = talked(h, &[], &once("talk", TWO_MESSAGES, "m"))?;
        let state = run.tmp.path().join("state");
        wait(40, "the Company's reply on the stream", || {
            stream_events(&state, "company.shop")
                .iter()
                .any(|e| e["data"].to_string().contains("You said: second"))
        })?;
        let last = stream_events(&state, "company.shop")
            .last()
            .and_then(|e| e["data"]["event"]["id"].as_str().map(str::to_string))
            .ok_or("an empty stream")?;
        wait(40, "a stream opened again after the last event", || {
            run.record()
                .iter()
                .any(|r| r["stream"] == true && r["last_event_id"] == last.as_str())
        })?;
        let again: Vec<Value> = run
            .record()
            .into_iter()
            .filter(|r| r["last_event_id"] == last.as_str())
            .collect();
        need(
            again.iter().all(|r| r["cursor"].is_null()),
            "a reconnect named a cursor beside Last-Event-ID",
        )
    })())
}

fn reads_on_from_the_cursor_it_saved(h: &Harness) -> Outcome {
    verdict((|| {
        let mut run = talked(h, &[], &once("talk", TWO_MESSAGES, "m"))?;
        let state = run.tmp.path().join("state");
        wait(40, "the Company's reply on the stream", || {
            stream_events(&state, "company.shop")
                .iter()
                .any(|e| e["data"].to_string().contains("You said: second"))
        })?;
        run.stop();
        let last = stream_events(&state, "company.shop")
            .last()
            .and_then(|e| e["data"]["event"]["id"].as_str().map(str::to_string))
            .ok_or("an empty stream")?;
        let before = run.record().len();
        run.daemon = Some(h.spawn(&["--config", &run.cfg]));
        wait(40, "a read after the restart", || {
            run.record()[before..]
                .iter()
                .any(|r| r["path"].as_str().is_some_and(|p| p.ends_with("/events")))
        })?;
        let record = run.record();
        let first = record[before..]
            .iter()
            .find(|r| r["path"].as_str().is_some_and(|p| p.ends_with("/events")))
            .ok_or("no read")?;
        need(
            first["cursor"] == last.as_str() && first["last_event_id"].is_null(),
            format!(
                "the first read after the restart is not from the saved cursor {last}: {first}"
            ),
        )
    })())
}

fn says_the_user_is_not_available(h: &Harness) -> Outcome {
    verdict((|| {
        let run = talked(
            h,
            &[
                serde_json::json!({"conversation": {"user_requested": "A specialist needs to confirm the exchange."}}),
            ],
            &once(
                "talk",
                "      c: {kind: tool, name: company.converse, args: {company: shop, text: \"I would like an exchange.\"}, depends_on: [s]}\n",
                "c",
            ),
        )?;
        let said = |r: &[Value]| -> Vec<Value> {
            messages(r)
                .into_iter()
                .filter(|m| {
                    m["text"]
                        .as_str()
                        .is_some_and(|t| t.contains("not available"))
                })
                .cloned()
                .collect()
        };
        wait(40, "agentd saying the User is not available", || {
            !said(&run.record()).is_empty()
        })?;
        std::thread::sleep(Duration::from_millis(1500));
        let declined = said(&run.record());
        need(
            declined.len() == 1,
            format!("said {} times", declined.len()),
        )?;
        need(declined[0]["sender"] == "agent", "not said as the agent")?;
        need(
            declined[0]["status"] == 202,
            format!("the Company did not take it: {}", declined[0]),
        )
    })())
}

fn sends_a_session_token_with_every_request(h: &Harness) -> Outcome {
    verdict((|| {
        // Each in a run of its own: the order is refused signed out, and a
        // run ends at its first failed step.
        let run = talked(
            h,
            &[],
            &format!(
                "{}{}{}",
                once("talk", TWO_MESSAGES, "m"),
                once(
                    "note",
                    "      n: {kind: http, method: POST, url: /notes, auth: {kind: company, company: shop}, json: {text: hi}, depends_on: [s]}\n",
                    "n",
                )
                .replacen("workflows:\n", "", 1),
                once(
                    "order",
                    "      o: {kind: tool, name: company.api, args: {company: shop, method: GET, path: /orders/o1}, depends_on: [s]}\n",
                    "o",
                )
                .replacen("workflows:\n", "", 1),
            ),
        )?;
        wait(40, "the conversation and the API calls", || {
            let r = run.record();
            r.iter().any(|x| x["path"] == "/api/notes")
                && r.iter().any(|x| x["path"] == "/api/orders/o1")
                && messages(&r).len() >= 2
        })?;
        for r in run.record().iter().filter(|r| {
            r["conversation_request"] == true
                || r["path"]
                    .as_str()
                    .is_some_and(|p| p.starts_with("/api/") && p != "/api/openapi.json")
        }) {
            need(
                r["presented"].is_string() && r["proof"]["ath"].is_string(),
                format!("a request without the Session Token and a proof over it: {r}"),
            )?;
            need(
                r["error"] != "invalid_dpop_proof" && r["error"] != "invalid_token",
                format!("the fake refused a request's token or proof: {r}"),
            )?;
        }
        Ok(())
    })())
}

fn reuses_an_idempotency_key_on_retry(h: &Harness) -> Outcome {
    verdict((|| {
        let run = talked(
            h,
            &[
                serde_json::json!({"http_fail": {"method": "POST", "path": "/api/orders/o1/exchanges", "times": 1, "status": 502}}),
            ],
            &once(
                "exchange",
                "      x: {kind: tool, name: company.api, args: {company: shop, method: POST, path: /orders/o1/exchanges, json: {item_id: itm_4Qa, replacement_sku: stormline-insulated-m}}, depends_on: [s]}\n",
                "x",
            ),
        )?;
        let sent = |r: &[Value]| -> Vec<Value> {
            r.iter()
                .filter(|x| x["path"] == "/api/orders/o1/exchanges")
                .cloned()
                .collect()
        };
        wait(40, "the exchange sent again", || {
            sent(&run.record()).len() >= 2
        })?;
        let s = sent(&run.record());
        let key = s[0]["idempotency_key"]
            .as_str()
            .unwrap_or_default()
            .to_string();
        need(
            key.len() == 36 && &key[14..15] == "4",
            format!("not a version 4 UUID: {key:?}"),
        )?;
        need(
            s[1]["idempotency_key"] == key.as_str(),
            "the retry carried another key",
        )?;
        need(
            s[1]["duplicate"] == true && s[1]["status"] == 201,
            format!("the retry was not answered as the first: {}", s[1]),
        )
    })())
}

/// The client identity listing the Operations extension.
const LISTS: &str = ", extensions: [operations]";

/// A run that proposes an exchange with the Company's MCP tool — tried
/// again until the Company's server has joined — and confirms the operation
/// it answers with.
const EXCHANGE: &str = "      p: {kind: tool, name: shop.exchange_item, args: {order_id: ord_7Hk2, item_id: itm_4Qa, replacement_sku: stormline-insulated-m}, depends_on: [s], retry: {max: 40, backoff: 250ms}}\n      c: {kind: tool, name: company.operation.confirm, args: {company: shop, operation_id: \"{{steps.p.output.operation.operation_id}}\"}, depends_on: [p]}\n";

/// agentd listing the Operations extension, `controls` steering the fake
/// first, `agent` lines under `agent:`, and — with `listens` — a loopback
/// listener the User is asked on; its run proposes and confirms an exchange.
fn operating(h: &Harness, controls: &[Value], agent: &str, listens: bool) -> Result<Run, String> {
    let rest = once("exchange", EXCHANGE, "c");
    let mut run = prepare_with(
        h,
        &[],
        &Extra {
            client: LISTS,
            agent,
            listens,
            rest: &rest,
            ..Extra::default()
        },
    )?;
    for c in controls {
        run.company.control(c.clone());
    }
    run.daemon = Some(h.spawn(&["--config", &run.cfg]));
    if let Some(addr) = &run.a2a {
        wait_ready(addr);
    }
    Ok(run)
}

/// The text of a task's status message.
fn status_text(t: &Value) -> String {
    t["status"]["message"]["parts"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|p| p["text"].as_str())
        .collect::<Vec<_>>()
        .join("\n")
}

/// The task on `addr` whose question holds `text`, once one does.
fn question(addr: &str, text: &str) -> Result<Value, String> {
    let mut found = None;
    wait(40, &format!("a question holding {text:?}"), || {
        let tasks = rpc(addr, 70, "ListTasks", serde_json::json!({"pageSize": 100}));
        found = tasks["tasks"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|t| {
                t["status"]["state"] == "TASK_STATE_INPUT_REQUIRED" && status_text(t).contains(text)
            })
            .cloned();
        found.is_some()
    })?;
    found.ok_or_else(|| "no question".to_string())
}

/// Answer the question `task` with `text`, as the User.
fn answer(addr: &str, task: &Value, text: &str) -> Result<(), String> {
    let id = task["id"].as_str().ok_or("a task without an id")?;
    let v = rpc_value(addr, 71, "SendMessage", text_params(text, Some(id), true));
    need(
        v.get("error").is_none(),
        format!("the answer was refused: {v}"),
    )
}

/// The operation requests that reached the fake: `/confirm`s, or reads.
fn operation_requests(record: &[Value], confirm: bool) -> Vec<Value> {
    record
        .iter()
        .filter(|r| {
            r["operation_request"] == true
                && (r["method"] == "POST"
                    && r["path"].as_str().is_some_and(|p| p.ends_with("/confirm")))
                    == confirm
                && (confirm || r["method"] == "GET")
        })
        .cloned()
        .collect()
}

/// The operation the fake proposed to agentd.
fn proposed(record: &[Value]) -> Vec<String> {
    record
        .iter()
        .filter_map(|r| r["operation_proposed"].as_str().map(str::to_string))
        .collect()
}

fn lists_the_operations_extension(h: &Harness) -> Outcome {
    verdict((|| {
        let run = operating(
            h,
            &[],
            "  approval: auto\n  ask_human_fallback: auto\n",
            false,
        )?;
        wait(40, "an operation proposed", || {
            !proposed(&run.record()).is_empty()
        })?;
        let fetched = run
            .record()
            .into_iter()
            .find(|r| {
                r["validated"]
                    .as_array()
                    .is_some_and(|v| v.contains(&serde_json::json!("client-metadata.schema.json")))
            })
            .ok_or("the fake never fetched the client metadata document")?;
        let _ = fetched;
        let doc: Value = serde_json::from_slice(
            &std::fs::read(run.tmp.path().join("published/client.json"))
                .map_err(|e| e.to_string())?,
        )
        .map_err(|e| e.to_string())?;
        need(
            doc["extensions"]["operations"]["version"] == "1",
            format!("the client metadata document lists no operations, version 1: {doc}"),
        )?;
        // Without the extension, the same call makes the exchange.
        let plain = prepare_with(
            h,
            &[],
            &Extra {
                rest: &once(
                    "exchange",
                    "      p: {kind: tool, name: shop.exchange_item, args: {order_id: ord_7Hk2, item_id: itm_4Qa, replacement_sku: stormline-insulated-m}, depends_on: [s], retry: {max: 40, backoff: 250ms}}\n",
                    "p",
                ),
                ..Extra::default()
            },
        )?;
        let mut plain = plain;
        plain.daemon = Some(h.spawn(&["--config", &plain.cfg]));
        wait(40, "the exchange made", || {
            tools_calls(&plain.record())
                .iter()
                .any(|c| c["status"] == 200)
        })?;
        need(
            proposed(&plain.record()).is_empty(),
            "the fake proposed an operation to an agent that does not list the extension",
        )
    })())
}

fn confirms_only_what_the_user_approved(h: &Harness) -> Outcome {
    verdict((|| {
        // Nobody to ask, and every fallback a judge: nothing is confirmed.
        let run = operating(
            h,
            &[],
            "  approval: auto\n  ask_human_fallback: auto\n",
            false,
        )?;
        wait(40, "the operation read before the confirm", || {
            !operation_requests(&run.record(), false).is_empty()
        })?;
        std::thread::sleep(Duration::from_millis(1500));
        need(
            operation_requests(&run.record(), true).is_empty(),
            "a confirm reached the Company with nobody to approve it",
        )?;
        // The User asked, and approving.
        let run = operating(h, &[], "", true)?;
        let addr = run.a2a.clone().ok_or("no listener")?;
        let gate = question(&addr, "asks you to approve this")?;
        let q = status_text(&gate);
        need(
            q.contains("\"Fake Company\" (127.0.0.1)")
                && q.contains("The Company says: \"Exchange item itm_4Qa on order ord_7Hk2"),
            format!("the question names no Company or summary: {q}"),
        )?;
        need(
            operation_requests(&run.record(), true).is_empty(),
            "a confirm reached the Company before the User answered",
        )?;
        answer(&addr, &gate, "approve")?;
        wait(40, "the confirm", || {
            !operation_requests(&run.record(), true).is_empty()
        })?;
        std::thread::sleep(Duration::from_millis(500));
        let sent = operation_requests(&run.record(), true);
        need(
            sent.len() == 1,
            format!("{} confirms were sent", sent.len()),
        )?;
        need(
            sent[0]["approved_by"] == "user" && sent[0]["revision"] == 1,
            format!(
                "the confirm is not the User's approval of revision 1: {}",
                sent[0]
            ),
        )
    })())
}

fn asks_again_when_the_terms_change(h: &Harness) -> Outcome {
    verdict((|| {
        let run = operating(
            h,
            &[serde_json::json!({"operations": {"revise": 1}})],
            "",
            true,
        )?;
        let addr = run.a2a.clone().ok_or("no listener")?;
        let gate = question(&addr, "This is revision 1 of operation")?;
        answer(&addr, &gate, "approve")?;
        let again = question(&addr, "This is revision 2 of operation")?;
        let sent = operation_requests(&run.record(), true);
        need(
            sent.len() == 1 && sent[0]["error"] == "terms_changed",
            format!("the first confirm was not the one the terms changed under: {sent:?}"),
        )?;
        answer(&addr, &again, "approve")?;
        wait(40, "the second confirm", || {
            operation_requests(&run.record(), true).len() >= 2
        })?;
        let sent = operation_requests(&run.record(), true);
        need(
            sent.len() == 2 && sent[1]["revision"] == 2 && sent[1]["approved_by"] == "user",
            format!("revision 2 was not confirmed with the User's approval of it: {sent:?}"),
        )
    })())
}

fn reads_an_operation_in_progress_after_retry_after(h: &Harness) -> Outcome {
    verdict((|| {
        let run = operating(
            h,
            &[serde_json::json!({"operations": {"in_progress": 1}})],
            "",
            true,
        )?;
        let addr = run.a2a.clone().ok_or("no listener")?;
        let gate = question(&addr, "asks you to approve this")?;
        answer(&addr, &gate, "approve")?;
        wait(40, "the confirm", || {
            !operation_requests(&run.record(), true).is_empty()
        })?;
        let confirmed = operation_requests(&run.record(), true)[0].clone();
        need(
            confirmed["op_state"] == "in_progress",
            format!("the confirm was not answered in_progress: {confirmed}"),
        )?;
        let at = confirmed["at_ms"].as_u64().unwrap_or(0);
        wait(40, "a read after the confirm", || {
            operation_requests(&run.record(), false)
                .iter()
                .any(|r| r["at_ms"].as_u64() > Some(at))
        })?;
        let read = operation_requests(&run.record(), false)
            .into_iter()
            .find(|r| r["at_ms"].as_u64() > Some(at))
            .ok_or("no read")?;
        need(
            read["at_ms"].as_u64().unwrap_or(0) >= at + 2_000,
            format!(
                "read {} ms after the confirm's answer, which asked for 2 s",
                read["at_ms"].as_u64().unwrap_or(0) - at
            ),
        )?;
        need(
            read["operation_id"] == confirmed["operation_id"],
            "the read named another operation",
        )
    })())
}

fn retries_a_confirm_with_the_same_operation(h: &Harness) -> Outcome {
    verdict((|| {
        let run = operating(
            h,
            &[
                serde_json::json!({"http_fail": {"method": "POST", "path": "/poppy/operations/", "times": 1, "status": 502}}),
            ],
            "",
            true,
        )?;
        let addr = run.a2a.clone().ok_or("no listener")?;
        let gate = question(&addr, "asks you to approve this")?;
        answer(&addr, &gate, "approve")?;
        wait(40, "the confirm sent again", || {
            operation_requests(&run.record(), true).len() >= 2
        })?;
        std::thread::sleep(Duration::from_millis(500));
        let sent = operation_requests(&run.record(), true);
        need(
            sent[0]["failed"] == 502,
            format!(
                "the first confirm was not the one whose answer was lost: {}",
                sent[0]
            ),
        )?;
        need(
            sent[0]["operation_id"] == sent[1]["operation_id"]
                && sent[0]["revision"] == sent[1]["revision"],
            format!("the confirm was sent again for another operation or revision: {sent:?}"),
        )?;
        need(
            proposed(&run.record()).len() == 1,
            "the tool that proposed the operation was called again",
        )
    })())
}

/// agentd talking with the Company's agent, which asks to talk with the
/// User after its first reply; the User is asked on a loopback listener.
fn asked_for_the_user(h: &Harness) -> Result<Run, String> {
    let rest = once(
        "talk",
        "      c: {kind: tool, name: company.converse, args: {company: shop, text: \"I would like some help.\"}, depends_on: [s]}\n",
        "c",
    );
    let mut run = prepare_with(
        h,
        &[],
        &Extra {
            entry: ", interfaces: [agent]",
            listens: true,
            rest: &rest,
            ..Extra::default()
        },
    )?;
    run.company.control(
        serde_json::json!({"conversation": {"user_requested": "A specialist needs to confirm the exchange."}}),
    );
    run.daemon = Some(h.spawn(&["--config", &run.cfg]));
    if let Some(addr) = &run.a2a {
        wait_ready(addr);
    }
    Ok(run)
}

fn asks_the_user_before_a_direct_conversation(h: &Harness) -> Outcome {
    verdict((|| {
        let run = asked_for_the_user(h)?;
        let addr = run.a2a.clone().ok_or("no listener")?;
        let gate = question(&addr, "asks to talk with you directly")?;
        need(
            status_text(&gate).contains("It says: \"A specialist needs to confirm the exchange.\""),
            format!(
                "the question does not quote the Company: {}",
                status_text(&gate)
            ),
        )?;
        let started = |r: &[Value]| -> Vec<Value> {
            r.iter()
                .filter(|m| m["parent_conversation_id"].is_string())
                .cloned()
                .collect()
        };
        need(
            started(&run.record()).is_empty(),
            "a Direct Conversation started before the User answered",
        )?;
        let parent = run
            .record()
            .into_iter()
            .find_map(|r| r["conversation"].as_str().map(str::to_string))
            .ok_or("no conversation")?;
        let words = "Hello — this is the User, about the exchange.";
        answer(&addr, &gate, words)?;
        wait(40, "the Direct Conversation", || {
            !started(&run.record()).is_empty()
        })?;
        let first = started(&run.record())[0].clone();
        need(
            first["sender"] == "human" && first["text"] == words,
            format!("the first message is not the User's words as they wrote them: {first}"),
        )?;
        need(
            first["parent_conversation_id"] == parent.as_str(),
            format!("it does not follow the conversation the Company asked in: {first}"),
        )?;
        for m in messages(&run.record()) {
            need(
                m["sender"] == "agent" || (m["sender"] == "human" && m["text"] == words),
                format!("a message carries sender human that is not the User's words: {m}"),
            )?;
        }
        Ok(())
    })())
}

fn says_a_decline_in_the_conversation(h: &Harness) -> Outcome {
    verdict((|| {
        let run = asked_for_the_user(h)?;
        let addr = run.a2a.clone().ok_or("no listener")?;
        let gate = question(&addr, "asks to talk with you directly")?;
        answer(&addr, &gate, "decline")?;
        let said = |r: &[Value]| -> Vec<Value> {
            messages(r)
                .into_iter()
                .filter(|m| m["text"].as_str().is_some_and(|t| t.contains("declined")))
                .cloned()
                .collect()
        };
        wait(40, "the decline said", || !said(&run.record()).is_empty())?;
        std::thread::sleep(Duration::from_millis(1000));
        let declined = said(&run.record());
        need(
            declined.len() == 1,
            format!("said {} times", declined.len()),
        )?;
        need(declined[0]["sender"] == "agent", "not said as the agent")?;
        need(
            !run.record()
                .iter()
                .any(|r| r["parent_conversation_id"].is_string()),
            "a Direct Conversation started after the User declined",
        )
    })())
}
