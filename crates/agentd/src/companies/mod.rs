// SPDX-License-Identifier: AGPL-3.0-only
//! Acting at companies for a person: agentd as a Personal Agent under the
//! Personal Agent Protocol draft 0.1, pinned at commit `95fcbf2`
//! (rfcs/0047).
//!
//! A Company is an operator-declared `services` entry of `kind: company`.
//! The supervisor — and only the supervisor — discovers and verifies each
//! one ([`discover`]), presents the one client identity every Company sees
//! ([`identity`]), keeps the User's pseudonymous ID there ([`user_id`]),
//! holds a signed-out Session with DPoP-bound tokens ([`session`],
//! [`token`], [`dpop`]) over requests held to the host rule and the SSRF
//! guard ([`http`]), and connects to the Company's MCP servers with a
//! Bearer token for each ([`mcp`]). No turn worker, subagent or instance
//! child is handed a Company server to dial, a key or a token: their calls
//! come back to the supervisor (§5.6).
//!
//! Built with the `companies` feature. [`wire`] (every constant the
//! protocol puts on the wire) and [`mock`] (the fake Company the Personal
//! Agent is tested against) are also compiled where the mock MCP servers
//! are — debug builds, or `internal-mocks` — with `oauth` and `tls`.

#[cfg(feature = "companies")]
pub mod discover;
#[cfg(feature = "companies")]
pub mod dpop;
#[cfg(feature = "companies")]
pub mod http;
#[cfg(feature = "companies")]
pub mod identity;
#[cfg(feature = "companies")]
pub mod mcp;
#[cfg(all(
    feature = "oauth",
    feature = "tls",
    any(debug_assertions, feature = "internal-mocks")
))]
pub mod mock;
#[cfg(feature = "companies")]
pub mod session;
#[cfg(feature = "companies")]
pub mod token;
#[cfg(feature = "companies")]
pub mod user_id;
pub mod wire;

#[cfg(feature = "companies")]
pub use supervisor::{Company, Supervisor};

/// `--print-company-client <dir>`: write the client metadata document and
/// key set `settings` describe into `dir`, for static hosting at the
/// `client_id` and `jwks_uri`, creating the signing key `0600` when absent.
/// Dials nothing.
#[cfg(feature = "companies")]
pub fn print_client(
    settings: &crate::config::settings::Settings,
    dir: &std::path::Path,
) -> Result<Vec<std::path::PathBuf>, String> {
    let client = settings
        .security
        .companies
        .client
        .as_ref()
        .ok_or("security.companies.client is not configured")?;
    identity::Identity::load(client)?.print(dir)
}

/// A Company-chosen string as agentd's own text shows it (RFC 0047 §5.7):
/// control characters gone, each run of whitespace one space, at most `cap`
/// characters, quoted — so it reads as one value and cannot end the text it
/// sits in.
#[cfg(feature = "companies")]
pub fn quoted(text: &str, cap: usize) -> String {
    let words: Vec<&str> = text
        .split(|c: char| c.is_whitespace() || c.is_control())
        .filter(|w| !w.is_empty())
        .collect();
    let one = words.join(" ");
    let capped = match one.char_indices().nth(cap) {
        Some((at, _)) => format!("{}…", &one[..at]),
        None => one,
    };
    format!("{capped:?}")
}

#[cfg(feature = "companies")]
mod supervisor {
    use std::collections::{BTreeMap, BTreeSet};
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use serde_json::json;

    use super::discover::{Discovery, Entry, Note, Verified};
    use super::identity::Identity;
    use super::session::{Env, Session};
    use super::user_id::{self, UserIds};
    use super::wire;
    use crate::config::settings::companies::{self as cfg, Privacy};
    use crate::config::settings::{ServiceKind, Settings};
    use crate::obs::log::Logger;
    use crate::state::Durable;

    /// One declared Company.
    pub struct Company {
        pub name: String,
        pub discovery: Discovery,
        pub privacy: Privacy,
        /// MCP server `url`s that refused this agent (`extension_required`).
        unusable: Mutex<BTreeSet<String>>,
        /// MCP server `url`s whose RFC 9728 metadata was checked.
        resources: Mutex<BTreeSet<String>>,
        in_flight: AtomicU32,
        limit: u32,
    }

    /// A Company call in flight; dropping it frees its place.
    pub struct Slot(Arc<Company>);

    impl Slot {
        /// The company entry the call is to.
        pub fn company(&self) -> &str {
            &self.0.name
        }

        /// Whether the call goes under the strict privacy profile.
        pub fn strict(&self) -> bool {
            self.0.privacy == Privacy::Strict
        }
    }

    impl Drop for Slot {
        fn drop(&mut self) {
            self.0.in_flight.fetch_sub(1, Ordering::SeqCst);
        }
    }

    impl Company {
        pub fn allow_private(&self) -> bool {
            self.discovery.entry.allow_private
        }

        pub fn is_unusable(&self, url: &str) -> bool {
            self.unusable
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .contains(url)
        }

        pub fn mark_unusable(&self, url: &str) {
            self.unusable
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(url.to_string());
        }

        /// A place for one more call, when fewer than
        /// `security.companies.limits.in_flight` are in flight.
        pub fn enter(this: &Arc<Company>) -> Option<Slot> {
            let was = this.in_flight.fetch_add(1, Ordering::SeqCst);
            if was >= this.limit {
                this.in_flight.fetch_sub(1, Ordering::SeqCst);
                return None;
            }
            Some(Slot(Arc::clone(this)))
        }

        pub fn limit(&self) -> u32 {
            self.limit
        }
    }

    /// What the checks before the first Session came to.
    enum Ready {
        Unchecked,
        Passed,
        /// The last failure, said once until it changes.
        Failed(String),
    }

    /// The supervisor's Personal Agent: the identity, the User IDs, the
    /// Companies and their Sessions.
    pub struct Supervisor {
        pub identity: Identity,
        users: UserIds,
        principal: String,
        companies: BTreeMap<String, Arc<Company>>,
        /// Each Company MCP server's entry.
        servers: BTreeMap<String, String>,
        /// The Sessions, by issuer: several domains naming one issuer are one
        /// Company (draft §3.2).
        sessions: Mutex<BTreeMap<String, Arc<Session>>>,
        /// Whether the seal key and the published identity have checked out.
        ready: Mutex<Ready>,
        timeout: Duration,
        log: Logger,
    }

    impl Supervisor {
        /// The Personal Agent `settings` declare, or `None` when they declare
        /// no Company: its identity loaded (the key created `0600` when
        /// absent), its seal key decoded, its Companies registered. Nothing
        /// is dialled.
        pub fn build(
            settings: &Settings,
            durable: Arc<Durable>,
            log: &Logger,
        ) -> Result<Option<Arc<Supervisor>>, String> {
            if !cfg::any_company(settings) {
                return Ok(None);
            }
            let c = &settings.security.companies;
            let client = c
                .client
                .as_ref()
                .ok_or("security.companies.client is required")?;
            let identity = Identity::load(client)?;
            let seal = match &c.seal_key {
                Some(s) => {
                    let text = crate::sec::secret::resolve(&s.0, &|k| std::env::var(k).ok())
                        .map_err(|e| format!("security.companies.seal_key: {e}"))?;
                    Some(user_id::seal_bytes(&text)?)
                }
                None => None,
            };
            let users = UserIds::new(durable, &client.id, seal)?;
            let timeout = settings
                .mcp
                .default_timeout
                .map(|d| d.0)
                .unwrap_or(Duration::from_secs(60));
            let mut companies = BTreeMap::new();
            for (name, svc) in &settings.services {
                if svc.kind != ServiceKind::Company {
                    continue;
                }
                let privacy = svc.privacy.unwrap_or_default();
                if privacy == Privacy::Standard {
                    log.warn("company.privacy.standard", json!({"company": name}));
                }
                companies.insert(
                    name.clone(),
                    Arc::new(Company {
                        name: name.clone(),
                        discovery: Discovery::new(Entry::from_settings(name, svc, timeout)),
                        privacy,
                        unusable: Mutex::default(),
                        resources: Mutex::default(),
                        in_flight: AtomicU32::new(0),
                        limit: c.in_flight(),
                    }),
                );
            }
            let servers = settings
                .mcp
                .servers
                .iter()
                .filter_map(|s| {
                    s.company
                        .as_ref()
                        .map(|c| (s.name.clone(), c.entry.clone()))
                })
                .collect();
            Ok(Some(Arc::new(Supervisor {
                identity,
                users,
                principal: c.user().to_string(),
                companies,
                servers,
                sessions: Mutex::default(),
                ready: Mutex::new(Ready::Unchecked),
                timeout,
                log: log.clone(),
            })))
        }

        pub fn log(&self) -> &Logger {
            &self.log
        }

        /// The User every Company sees this instance act for.
        pub fn principal(&self) -> &str {
            &self.principal
        }

        pub fn company(&self, name: &str) -> Option<Arc<Company>> {
            self.companies.get(name).cloned()
        }

        /// The Company the MCP server `server` belongs to.
        pub fn company_of_server(&self, server: &str) -> Option<Arc<Company>> {
            self.servers.get(server).and_then(|c| self.company(c))
        }

        /// The seal key and the published identity, checked before the first
        /// Session: a seal key that opens none of the store's records, or an
        /// identity the Company could not fetch as configured, makes every
        /// Company unavailable, saying why. A success is kept; a failure is
        /// checked again at the next use.
        fn ready(&self) -> Result<(), String> {
            let mut ready = self.ready.lock().unwrap_or_else(|e| e.into_inner());
            if matches!(*ready, Ready::Passed) {
                return Ok(());
            }
            let outcome = self
                .users
                .check_seal()
                .map_err(|e| ("company.seal_key.mismatch", e))
                .and_then(|()| {
                    self.identity
                        .self_check(self.timeout.min(Duration::from_secs(10)))
                        .map_err(|e| ("company.identity.unpublished", e))
                });
            match outcome {
                Ok(()) => {
                    *ready = Ready::Passed;
                    Ok(())
                }
                Err((event, e)) => {
                    if !matches!(&*ready, Ready::Failed(was) if *was == e) {
                        let fields = match event {
                            "company.identity.unpublished" => {
                                json!({"kid": self.identity.kid(), "url": self.identity.client_id, "error": e})
                            }
                            _ => json!({"error": e}),
                        };
                        self.log.error(event, fields);
                        *ready = Ready::Failed(e.clone());
                    }
                    Err(e)
                }
            }
        }

        /// `company`'s verified documents, fetched again once they expire,
        /// and what changed said on the log. A failure is reported for
        /// target `company:<name>` (`auth.unavailable`); a success is not,
        /// because the Company has not recovered until a token is produced
        /// ([`Self::bearer`] reports that).
        pub fn verified(&self, company: &Company) -> Result<Arc<Verified>, String> {
            self.verify(company)
                .or_else(|e| self.report(company, Err(e)))
        }

        /// [`Self::verified`], unreported: for the caller to report as part
        /// of its own attempt.
        fn verify(&self, company: &Company) -> Result<Arc<Verified>, String> {
            self.ready().and_then(|()| {
                let (out, notes) = company.discovery.current();
                for n in notes {
                    self.say(&company.name, n);
                }
                out
            })
        }

        /// One attempt to produce `company`'s credential, reported for target
        /// `company:<name>`: `auth.unavailable` / `auth.recovered`.
        fn report<T>(&self, company: &Company, attempt: Result<T, String>) -> Result<T, String> {
            crate::auth::report::outcome(&format!("company:{}", company.name), attempt)
        }

        fn say(&self, company: &str, note: Note) {
            match note {
                Note::Verified(line) => self.log.info("company.verified", line),
                Note::Refused(r) => {
                    if r.reason == "endpoint" {
                        self.log.warn(
                            "company.endpoint.refused",
                            json!({"company": company, "field": r.field, "host": r.host}),
                        );
                    }
                    self.log.warn(
                        "company.refused",
                        json!({"company": company, "reason": r.reason, "error": r.detail}),
                    );
                }
                Note::Stale { age_s, error } => self.log.warn(
                    "company.discovery.stale",
                    json!({"company": company, "age_s": age_s, "error": error}),
                ),
                Note::IssuerChanged { old, new } => self.log.warn(
                    "company.issuer_changed",
                    json!({"company": company, "old": old, "new": new}),
                ),
                Note::ApisChanged { added, removed } => self.log.warn(
                    "company.apis_changed",
                    json!({"company": company, "added": added, "removed": removed}),
                ),
            }
        }

        /// The Session at `issuer`, made with a new DPoP key the first time.
        fn session(&self, issuer: &str) -> Result<Arc<Session>, String> {
            let mut s = self.sessions.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(x) = s.get(issuer) {
                return Ok(Arc::clone(x));
            }
            let x = Arc::new(Session::new(issuer)?);
            s.insert(issuer.to_string(), Arc::clone(&x));
            Ok(x)
        }

        /// A Bearer token for `company`'s MCP server at `url`: discovery, the
        /// Session and the token, reported as one outcome for target
        /// `company:<name>`.
        pub fn bearer(&self, company: &Company, url: &str) -> Result<String, String> {
            let attempt = self.verify(company).and_then(|verified| {
                let session = self.session(&verified.issuer)?;
                let dialer = verified.dialer(&company.discovery.entry);
                let env = Env {
                    identity: &self.identity,
                    verified: &verified,
                    dialer: &dialer,
                    users: &self.users,
                    principal: &self.principal,
                    log: &self.log,
                };
                session.open(&env)?;
                session.bearer(&env, url)
            });
            self.report(company, attempt)
        }

        /// `url` refused `token`: the next request gets a new one.
        pub fn session_refused(&self, company: &Company, url: &str, token: &str) {
            if let Ok(v) = self.verify(company)
                && let Ok(s) = self.session(&v.issuer)
            {
                s.refused(url, token);
            }
        }

        /// The MCP server at `url` names the Company's issuer among its RFC
        /// 9728 `authorization_servers` (draft §6), checked once before its
        /// first token.
        pub fn check_resource(
            &self,
            company: &Company,
            verified: &Verified,
            url: &str,
        ) -> Result<(), String> {
            if company
                .resources
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .contains(url)
            {
                return Ok(());
            }
            let at = crate::auth::http::insert_well_known(url, wire::RESOURCE_METADATA_SUFFIX)
                .ok_or_else(|| format!("company {}: {url} is not a URL", company.name))?;
            let doc = verified
                .dialer(&company.discovery.entry)
                .document(&at)
                .map_err(|r| r.say(&company.name))?;
            let listed = doc.json["authorization_servers"]
                .as_array()
                .is_some_and(|a| {
                    a.iter()
                        .any(|s| s.as_str() == Some(verified.issuer.as_str()))
                });
            if !listed {
                return Err(format!(
                    "company {}: the MCP server at {url} does not list the issuer {} in its authorization_servers",
                    company.name, verified.issuer
                ));
            }
            company
                .resources
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(url.to_string());
            Ok(())
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        /// At most `limits.in_flight` calls are in flight at a Company: one
        /// more is refused until one ends.
        #[test]
        fn a_company_takes_its_limit_of_calls_in_flight() {
            let svc: crate::config::settings::Service = serde_json::from_value(
                json!({"kind": "company", "endpoint": "https://shop.example"}),
            )
            .unwrap();
            let c = Arc::new(Company {
                name: "shop".into(),
                discovery: Discovery::new(Entry::from_settings(
                    "shop",
                    &svc,
                    Duration::from_secs(1),
                )),
                privacy: Privacy::Strict,
                unusable: Mutex::default(),
                resources: Mutex::default(),
                in_flight: AtomicU32::new(0),
                limit: 2,
            });
            let a = Company::enter(&c).unwrap();
            let b = Company::enter(&c).unwrap();
            assert!(Company::enter(&c).is_none(), "a third is refused");
            drop(a);
            let again = Company::enter(&c);
            assert!(again.is_some(), "a place freed is taken");
            assert_eq!(b.company(), "shop");
        }
    }
}
