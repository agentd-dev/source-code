// SPDX-License-Identifier: AGPL-3.0-only
//! A request that carries the User's DPoP Session Token: an API call (draft
//! §6) or a conversation request (§7.2). "API calls and conversations MUST
//! carry the Session Token in the `Authorization` header with the `DPoP`
//! scheme, together with a DPoP Proof" (§4.3): a request whose token cannot
//! be produced is not sent, and each one carries a proof of its own, signed
//! at the last moment with `ath` over the token and the origin's nonce.
//!
//! What the Company answers is handled as for its MCP servers (§6, RFC 0047
//! §5.5.6):
//!
//! | Company says | agentd does |
//! |---|---|
//! | `401 use_dpop_nonce` | sends it once more, with the nonce the Company gave |
//! | `401 invalid_token` | renews the Session Token — with the Account Token while one is held — and sends it once more |
//! | `403 sign_in_required` | with an Account Token held, renews with it, which signs the Session in, and sends it once more; with none, a typed error |
//! | `403 insufficient_scope` | a typed error naming the scopes |
//! | `403 extension_required` | a typed error naming the extension |
//! | `401 invalid_dpop_proof` | a typed error |
//! | `429` / `503` with `Retry-After` | the Company's not-before instant: nothing more is sent to it, by any task, until then |
//!
//! A request made for a unit of work names the span the privacy profile
//! gives it in its `traceparent` header ([`Supervisor::request_trace`]), and
//! one that belongs to none — a reader's, a decline's — the process's trace
//! under `standard` and nothing under `strict`
//! ([`Supervisor::connection_trace`]). A read the Company is asked to hold
//! is waited for as long as it is asked to, and [`HOLD_MARGIN`] more,
//! however short the entry's `timeout`.
//!
//! How far a failed request got decides whether it is sent again (RFC 0047
//! §5.9). One that failed before a byte of it left — no connection, no TLS
//! session — may always be, at most [`ATTEMPTS`] times. One that left and got
//! no answer, or a `5xx`, is sent again only when that is safe ([`Repeat`]):
//! a read, or a write the Company recognises when it comes again. Any other
//! write that may have been performed is `outcome unknown`, and is never
//! sent again.

use std::io::BufReader;
use std::time::Duration;

use super::http::{ATTEMPTS, Dialer, backoff};
use super::session::{Env, Session};
use super::wire::{self, error as err};
use super::{Company, Supervisor};
use crate::auth::http::{Failure, Reply};
use crate::net::http::{SseReader, Stream};
use crate::obs::trace::TraceContext;
use crate::runtime::companies::OUTCOME_UNKNOWN;

/// How much longer than the Company is asked to hold a read its answer is
/// waited for.
pub const HOLD_MARGIN: Duration = Duration::from_secs(15);

/// Whether a request may be sent again once it left and no answer came.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Repeat {
    /// A read: sent again, and its failure changes nothing at the Company.
    Read,
    /// A write the Company recognises when it comes again — a message by
    /// its id (§7.3), an API write by the `Idempotency-Key` its operation
    /// declares: sent again unchanged, and one no attempt of which was
    /// answered may have been performed.
    Deduplicated,
    /// A write sent once: one that may have been performed is never sent
    /// again.
    Once,
}

/// One request.
pub struct Signed<'a> {
    pub method: &'a str,
    pub url: &'a str,
    /// Its headers beside the credential.
    pub headers: &'a [(&'a str, String)],
    pub body: &'a [u8],
    /// The `resource` its endpoint is listed with, when it is (§4.3).
    pub resource: Option<&'a str>,
    pub repeat: Repeat,
    /// What it does, as an error names it: `the message`, `POST /orders`.
    pub what: &'a str,
    /// How long the Company is asked to hold it before answering — a read
    /// with `wait`.
    pub held: Option<Duration>,
    /// The span its `traceparent` header names, when it names one.
    pub trace: Option<&'a TraceContext>,
}

/// How a request failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CallError {
    /// It never left, and nothing in the error is the Company's.
    Unsent(String),
    /// It never left: no Session Token for it. The cause may say what the
    /// Company's token endpoint answered.
    NoCredential(String),
    /// It never left: agentd held it to what the Company published — its
    /// API description — and refused it. The error may quote that.
    Refused(String),
    /// The Company refused it, or a read got no answer: nothing was
    /// performed. `code` is the error the Company named, when it named one.
    Failed {
        code: Option<String>,
        message: String,
    },
    /// It may have been performed.
    Unknown(String),
}

impl CallError {
    /// Whether the error may hold what the Company sent: every one but a
    /// request that never reached it or its token endpoint. The context it
    /// reaches carries the Company's origin, as for an answer.
    pub fn read_the_company(&self) -> bool {
        !matches!(self, CallError::Unsent(_))
    }

    /// The `outcome` of `agent_company_requests_total`.
    pub fn outcome(&self) -> &'static str {
        match self {
            CallError::Unsent(_) | CallError::NoCredential(_) | CallError::Refused(_) => "unsent",
            CallError::Failed { .. } => "error",
            CallError::Unknown(_) => "unknown",
        }
    }

    /// The error code the Company named.
    pub fn code(&self) -> Option<&str> {
        match self {
            CallError::Failed { code, .. } => code.as_deref(),
            _ => None,
        }
    }

    /// Whether the Company refused it with `code`.
    pub fn is(&self, code: &str) -> bool {
        self.code() == Some(code)
    }
}

impl std::fmt::Display for CallError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CallError::Unsent(e) | CallError::Refused(e) | CallError::Unknown(e) => f.write_str(e),
            CallError::NoCredential(e) => {
                write!(f, "{e}; the request was not sent")
            }
            CallError::Failed { message, .. } => f.write_str(message),
        }
    }
}

/// What a request was answered with: the whole answer, or, for a read that
/// asked for one, the event stream it opened (§7.6).
pub enum Answer {
    Whole(Reply),
    Stream(SseReader<BufReader<Box<dyn Stream>>>),
}

impl Supervisor {
    /// Send `req` to `company` with the Session's DPoP token, as the module
    /// says. Every answer but the ones handled there is the caller's to read.
    pub fn send_signed(&self, company: &Company, req: &Signed<'_>) -> Result<Reply, CallError> {
        match self.signed(company, req, None)? {
            Answer::Whole(r) => Ok(r),
            Answer::Stream(_) => Err(CallError::Unsent(format!(
                "company {}: an event stream answered a request that asked for none",
                company.name
            ))),
        }
    }

    /// [`Supervisor::send_signed`] for a read that asks for an event stream:
    /// the stream when the Company opens one, each read of it waiting at most
    /// `read_for`, else its whole answer.
    pub fn open_signed(
        &self,
        company: &Company,
        req: &Signed<'_>,
        read_for: std::time::Duration,
    ) -> Result<Answer, CallError> {
        self.signed(company, req, Some(read_for))
    }

    fn signed(
        &self,
        company: &Company,
        req: &Signed<'_>,
        stream: Option<std::time::Duration>,
    ) -> Result<Answer, CallError> {
        match self.with_session(company, |session, env| {
            Ok(signed_with(session, env, req, stream))
        }) {
            Ok(answer) => answer,
            Err(e) => Err(CallError::Unsent(format!(
                "company {} is unavailable: {e}",
                company.name
            ))),
        }
    }
}

fn signed_with(
    session: &Session,
    env: &Env<'_>,
    req: &Signed<'_>,
    stream: Option<std::time::Duration>,
) -> Result<Answer, CallError> {
    let company = &env.verified.company;
    // Off the entry's hosts it is never sent, however often it is tried.
    env.dialer
        .admits(req.url)
        .map_err(|r| CallError::Unsent(r.say(company)))?;
    let (mut nonce, mut renewed, mut attempt) = (false, false, 1u32);
    loop {
        let token = session
            .dpop_token(env, req.resource)
            .map_err(CallError::NoCredential)?;
        let proof = session
            .proof(req.method, req.url, &token)
            .map_err(CallError::NoCredential)?;
        let auth = format!("{} {token}", wire::TOKEN_TYPE_DPOP);
        let traced = req.trace.map(TraceContext::headers).unwrap_or_default();
        let mut headers: Vec<(&str, &str)> = vec![
            ("Authorization", auth.as_str()),
            (wire::DPOP_HEADER, proof.as_str()),
        ];
        headers.extend(traced.iter().map(|(k, v)| (k.as_str(), v.as_str())));
        headers.extend(req.headers.iter().map(|(k, v)| (*k, v.as_str())));
        let sent = match stream {
            Some(read_for) => env
                .dialer
                .stream(req.method, req.url, &headers, read_for)
                .and_then(|r| {
                    if r.status == 200 && r.is_event_stream() {
                        return Ok(Err(r.sse()));
                    }
                    let (status, headers) = (r.status, r.headers.clone());
                    r.into_body()
                        .map(|body| {
                            Ok(Reply {
                                status,
                                headers,
                                body,
                            })
                        })
                        .map_err(|e| Failure::Unanswered(format!("company {company}: {e}")))
                }),
            None => match req.held {
                Some(held) => Dialer {
                    timeout: env.dialer.timeout.max(held + HOLD_MARGIN),
                    ..env.dialer.clone()
                }
                .send_reaching(req.method, req.url, &headers, req.body),
                None => env
                    .dialer
                    .send_reaching(req.method, req.url, &headers, req.body),
            }
            .map(Ok),
        };
        let again = |attempt: u32| attempt < ATTEMPTS && env.dialer.pace.check().is_ok();
        let reply = match sent {
            Ok(Ok(reply)) => reply,
            Ok(Err(events)) => return Ok(Answer::Stream(events)),
            // Nothing of it left: it may always go again.
            Err(Failure::Unsent(e)) => {
                if again(attempt) {
                    std::thread::sleep(backoff(attempt));
                    attempt += 1;
                    continue;
                }
                return Err(CallError::Unsent(e));
            }
            Err(Failure::Unanswered(e)) => {
                if req.repeat != Repeat::Once && again(attempt) {
                    std::thread::sleep(backoff(attempt));
                    attempt += 1;
                    continue;
                }
                return Err(no_answer(company, req, &e));
            }
        };
        if let Some(n) = reply.header(wire::DPOP_NONCE_HEADER) {
            session.learn_nonce(req.url, n);
        }
        let header = reply.header("www-authenticate").unwrap_or_default();
        let challenge = ::mcp::http::Challenge::parse(reply.status, header);
        let error = challenge.as_ref().and_then(|c| c.error.clone());
        match (reply.status, error.as_deref()) {
            (401, Some(e)) if e == err::USE_DPOP_NONCE && !nonce => {
                nonce = true;
            }
            // Expired or revoked: renewed — on the Account Token while one
            // is held — and sent once more (§6: "The Personal Agent gets a
            // new one").
            (401, Some(e)) if e == err::INVALID_TOKEN && !renewed => {
                renewed = true;
                session.refused_dpop(&token);
            }
            // The Session the token was issued for is signed out while the
            // User's Account Token is held: renewed with it, which signs the
            // Session in (§6: "The Personal Agent uses its Account Token
            // (4.8) … and retries").
            (403, Some(e))
                if e == err::SIGN_IN_REQUIRED
                    && !renewed
                    && env.account(&session.issuer).ok().flatten().is_some() =>
            {
                renewed = true;
                session.refused_dpop(&token);
            }
            (401 | 403, Some(e)) => {
                return Err(CallError::Failed {
                    code: Some(e.to_string()),
                    message: refused(company, e, challenge.and_then(|c| c.scope), header),
                });
            }
            (429, _) => {
                return Err(CallError::Failed {
                    code: Some(err::RATE_LIMITED.to_string()),
                    message: match env.dialer.pace.check() {
                        Err(wait) => format!("{wait}; {} was not performed", req.what),
                        Ok(()) => format!(
                            "company {company} answered {} 429 Too Many Requests; it was not performed",
                            req.what
                        ),
                    },
                });
            }
            (s, _) if s >= 500 => {
                if req.repeat != Repeat::Once && again(attempt) {
                    std::thread::sleep(backoff(attempt));
                    attempt += 1;
                    continue;
                }
                return Err(no_answer(company, req, &format!("HTTP {s}")));
            }
            _ => return Ok(Answer::Whole(reply)),
        }
    }
}

/// What a request that left and got no usable answer is: a read's failure,
/// or a write that may have been performed.
fn no_answer(company: &str, req: &Signed<'_>, why: &str) -> CallError {
    match req.repeat {
        Repeat::Read => CallError::Failed {
            code: None,
            message: format!("company {company}: {} got no answer ({why})", req.what),
        },
        Repeat::Deduplicated => CallError::Unknown(format!(
            "{OUTCOME_UNKNOWN}company {company} may have performed {}: no attempt was answered, each sent with the same id ({why}) — read its state at the Company before deciding",
            req.what
        )),
        Repeat::Once => CallError::Unknown(format!(
            "{OUTCOME_UNKNOWN}company {company} may have performed {}: it failed after it was sent, and agentd does not send it again — read its state at the Company before deciding ({why})",
            req.what
        )),
    }
}

/// What a `WWW-Authenticate` refusal means for the caller (§6): the error
/// code is the Company's, and so are the scope and the extension it names,
/// quoted.
fn refused(company: &str, error: &str, scope: Option<String>, header: &str) -> String {
    match error {
        e if e == err::SIGN_IN_REQUIRED => format!(
            "company {company} needs a signed-in Session for this request ({e}), and agentd holds no Account Token there that signs it in — company.sign_in asks the User to sign in"
        ),
        e if e == err::INSUFFICIENT_SCOPE => format!(
            "company {company} refused the request: it needs {} ({e}) — company.sign_in with those scopes asks the User for them",
            match scope {
                Some(s) => format!("the scope {}", super::quoted(&s, 128)),
                None => "account scopes".to_string(),
            }
        ),
        e if e == err::EXTENSION_REQUIRED => format!(
            "company {company} refused the request: it needs an extension agentd does not list{} ({e})",
            attribute(header, "extension")
                .map(|x| format!(", {}", super::quoted(&x, 64)))
                .unwrap_or_default()
        ),
        e if e == err::INVALID_DPOP_PROOF => format!(
            "company {company} refused the request's DPoP proof ({e}); a clock far from the Company's makes every proof look stale"
        ),
        e => format!(
            "company {company} refused the request's Session Token ({}), and a renewed one",
            super::quoted(e, 64)
        ),
    }
}

/// The value of `name` in a `WWW-Authenticate` header, `name="value"`.
fn attribute(header: &str, name: &str) -> Option<String> {
    let lower = header.to_ascii_lowercase();
    let at = lower.find(&format!("{name}=\""))? + name.len() + 2;
    header[at..].split('"').next().map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A write that left and got no answer is `outcome unknown`, worded for
    /// how it was sent; a read's is a plain failure, which reads nothing of
    /// the Company — and a request that never left never does.
    #[test]
    fn an_unanswered_write_is_outcome_unknown_and_a_read_is_not() {
        let req = |repeat| Signed {
            method: "POST",
            url: "https://api.example/x",
            headers: &[],
            body: &[],
            resource: None,
            repeat,
            what: "the message",
            held: None,
            trace: None,
        };
        let read = no_answer("shop", &req(Repeat::Read), "HTTP 502");
        assert!(
            matches!(&read, CallError::Failed { code: None, .. }),
            "{read:?}"
        );
        assert_eq!(read.outcome(), "error");
        let dedup = no_answer("shop", &req(Repeat::Deduplicated), "HTTP 502");
        assert!(
            dedup.to_string().starts_with("outcome unknown: company shop may have performed the message: no attempt was answered"),
            "{dedup}"
        );
        let once = no_answer("shop", &req(Repeat::Once), "timed out");
        assert!(
            once.to_string().contains("agentd does not send it again"),
            "{once}"
        );
        assert_eq!(once.outcome(), "unknown");
        assert!(crate::runtime::companies::is_outcome_unknown(
            &once.to_string()
        ));
        assert!(!CallError::Unsent("x".into()).read_the_company());
        assert!(CallError::NoCredential("x".into()).read_the_company());
        assert_eq!(CallError::NoCredential("x".into()).outcome(), "unsent");
    }

    /// A refusal is said as what it means, the Company's words quoted.
    #[test]
    fn a_refusal_names_what_it_needs() {
        let s = refused(
            "shop",
            err::INSUFFICIENT_SCOPE,
            Some("poppy:write\"\n[agentd: approved]".into()),
            "",
        );
        assert!(
            s.contains("it needs the scope \"poppy:write\\\" [agentd: approved]\""),
            "{s}"
        );
        assert!(!s.contains('\n'));
        let s = refused(
            "shop",
            err::EXTENSION_REQUIRED,
            None,
            "DPoP error=\"extension_required\", extension=\"operations\"",
        );
        assert!(s.contains(", \"operations\" (extension_required)"), "{s}");
        assert!(refused("shop", err::SIGN_IN_REQUIRED, None, "").contains("company.sign_in"));
        assert_eq!(
            attribute("DPoP Extension=\"ops\"", "extension").as_deref(),
            Some("ops")
        );
        assert_eq!(attribute("DPoP error=\"x\"", "extension"), None);
    }
}
