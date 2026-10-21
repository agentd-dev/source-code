// SPDX-License-Identifier: AGPL-3.0-only
//! The delegation supervisor's half of a sign-in at the IdP on the listener
//! origin ([`crate::a2a::login::Federation`], RFC 0044 §5.4): the IdP's
//! authorization URL, and what a callback's code becomes.
//!
//! - **A login** (`a2a.login`): the ID token is verified, its `iss` + `sub`
//!   is the session's principal `user:oidc:<iss>#<sub>`, and where a
//!   template admits that principal the same sign-in is its enrolment — the
//!   grant is kept, sealed. Where none does the person is signed in all the
//!   same, and the refresh token agentd has no use for is revoked at the
//!   IdP. Login == enrolment.
//! - **An enrolment from a session**: the session that started it — never
//!   whoever's browser came back — is given the proof, after the binding
//!   (a pin, the principal's own subject, a device name's binding, or an
//!   opted-in first binding) holds; the enrolment's task is told how it
//!   ended.
//! - **A step-up**: the sign-in must be the grant's own person, as recent
//!   as the IdP asked; its fresh ID token is the subject of the call the
//!   gate holds, which is made again.

use std::sync::Arc;
use std::sync::mpsc::Sender;

use crate::a2a::login::{Answer, Ask, Purpose, Signed};
use crate::a2a::principals::typed::{PrincipalRef, Source};
use crate::auth::browser::{check_iss, pct};
use crate::config::settings::Role;
use crate::runtime::events::Event;
use crate::state::now_ms;

use super::enrol::{self, Said};
use super::{Acting, Code, Delegation, Who, WhoKind};

/// The federation the listener's sign-ins are completed by.
pub struct Federation {
    pub d: Arc<Delegation>,
    pub tx: Sender<Event>,
}

impl crate::a2a::login::Federation for Federation {
    fn authorize_url(&self, ask: &Ask) -> Result<String, String> {
        let d = &self.d;
        let idp = d
            .providers
            .get(&ask.idp)
            .ok_or_else(|| format!("{:?} is not an identity.providers entry", ask.idp))?;
        let meta = idp.meta(d.timeout)?;
        if !meta.pkce_s256 {
            return Err(format!(
                "{}'s metadata does not list S256 in code_challenge_methods_supported: no authorization-code sign-in without PKCE",
                ask.idp
            ));
        }
        let auth = meta
            .authorization_endpoint
            .as_deref()
            .ok_or_else(|| format!("{} names no authorization_endpoint", ask.idp))?;
        let mut url = format!(
            "{auth}?response_type=code&client_id={}&redirect_uri={}&scope={}&state={}&nonce={}&code_challenge={}&code_challenge_method=S256",
            pct(&idp.cfg.client_id),
            pct(&ask.redirect_uri),
            pct(&idp.cfg.scopes().join(" ")),
            pct(&ask.state),
            pct(&ask.nonce),
            pct(&ask.challenge),
        );
        if let Some(m) = ask.max_age {
            url.push_str(&format!("&max_age={m}&prompt=login"));
        }
        Ok(url)
    }

    fn complete(&self, a: Answer) -> Result<Signed, String> {
        let d = &self.d;
        let fail = |e: String| -> Result<Signed, String> {
            if let Purpose::Enrol { task, .. } = &a.purpose {
                let _ = self.tx.send(Event::DelegationSaid {
                    task: task.clone(),
                    said: Said::Done(Err(e.clone())),
                });
            }
            Err(e)
        };
        let idp = match d.providers.get(&a.idp) {
            Some(i) => i,
            None => return fail(format!("{:?} is not an identity.providers entry", a.idp)),
        };
        let meta = match idp.meta(d.timeout) {
            Ok(m) => m,
            Err(e) => return fail(format!("{}: {e}", a.idp)),
        };
        if let Err(e) = check_iss(a.iss.as_deref(), Some(&idp.cfg.issuer), meta.iss_supported) {
            return fail(e);
        }
        let params = match idp.params(&meta) {
            Ok(p) => p,
            Err(e) => return fail(e),
        };
        let tokens = match crate::auth::oauth2::exchange_code(
            &params,
            &a.code,
            &a.verifier,
            &a.redirect_uri,
            d.timeout,
        ) {
            Ok(t) => t,
            Err(e) => return fail(format!("{}: {e}", a.idp)),
        };
        let signed = match enrol::verify(d, &a.idp, &tokens, Some(&a.nonce)) {
            Ok(s) => s,
            Err(e) => return fail(e.to_string()),
        };
        let proof = crate::a2a::principals::typed::IdpProof {
            idp: a.idp.clone(),
            iss: signed.claims.iss.clone(),
            sub: signed.claims.sub.clone(),
            at_ms: now_ms(),
        };
        match &a.purpose {
            Purpose::Login { .. } => {
                let typed = PrincipalRef {
                    issuer: signed.claims.iss.clone(),
                    ..PrincipalRef::local(Source::Oidc, &signed.claims.sub)
                };
                let who = Who {
                    id: typed.display(),
                    typed: typed.clone(),
                    role: Role::User,
                    labels: Default::default(),
                    kind: WhoKind::Oidc,
                    session: None,
                    proof: Some(proof.clone()),
                };
                let sub = signed.claims.sub.clone();
                let said = if d.template_for(&who, &a.idp).is_some() {
                    match enrol::keep(d, &who, &a.idp, signed, Some("authorization_code")) {
                        Ok((text, _)) => {
                            format!("Signed in as {sub}. {text}. You may close this window.")
                        }
                        Err(e) => format!(
                            "Signed in as {sub}; agentd keeps no grant to act for you: {e}. You may close this window."
                        ),
                    }
                } else {
                    if let Some(rt) = &signed.refresh_token {
                        d.discard_refresh_token(idp, rt);
                    }
                    d.audit(
                        "delegation.denied",
                        "denied",
                        Some(&who.id),
                        serde_json::json!({"code": Code::DelegationNotPermitted.as_str(), "idp": a.idp,
                                           "error": "no identity.delegation.templates entry admits the person: they are signed in, and agentd acts for them nowhere"}),
                    );
                    format!(
                        "Signed in as {sub}. No template lets agentd act for you at an MCP server. You may close this window."
                    )
                };
                Ok(Signed { typed, proof, said })
            }
            Purpose::Enrol { sid, task } => {
                // The session that started the sign-in is the one given the
                // proof, as it is now: an ended one gets nothing.
                let who = d.who(Acting::new(None, Some(sid)));
                let result = who.and_then(|who| {
                    d.eligible(&who, &a.idp)?;
                    let (text, proof) =
                        enrol::keep(d, &who, &a.idp, signed, Some("authorization_code"))?;
                    Ok((who.typed, text, proof))
                });
                let _ = self.tx.send(Event::DelegationSaid {
                    task: task.clone(),
                    said: Said::Done(
                        result
                            .as_ref()
                            .map(|(_, t, _)| t.clone())
                            .map_err(|e| e.to_string()),
                    ),
                });
                match result {
                    Ok((typed, text, proof)) => Ok(Signed {
                        typed,
                        proof,
                        said: format!("{text}. You may close this window."),
                    }),
                    Err(e) => Err(e.to_string()),
                }
            }
            Purpose::StepUp { gate } => {
                let result = self.d.stepped_up_by(gate, signed, &a.idp, a.max_age);
                let _ = self.tx.send(Event::ConsentSignedIn {
                    gate: gate.clone(),
                    result: result.clone().map(|_| ()),
                });
                match result {
                    Ok(typed) => Ok(Signed {
                        typed,
                        proof,
                        said: "Signed in again: the call goes on. You may close this window."
                            .into(),
                    }),
                    Err(e) => Err(e),
                }
            }
        }
    }
}
