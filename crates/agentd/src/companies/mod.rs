// SPDX-License-Identifier: AGPL-3.0-only
//! Acting at companies for a person: agentd as a Personal Agent under the
//! Personal Agent Protocol draft 0.1, pinned at commit `95fcbf2`
//! (rfcs/0047).
//!
//! A Company is an operator-declared `services` entry of `kind: company`.
//! The supervisor — and only the supervisor — discovers and verifies each
//! one ([`discover`]), presents the one client identity every Company sees
//! ([`identity`]), keeps the User's pseudonymous ID there ([`user_id`]),
//! holds a Session with DPoP-bound tokens ([`session`], [`token`],
//! [`dpop`]) over requests held to the host rule, the SSRF guard and the
//! Company's not-before instant ([`http`]), and connects to the Company's
//! MCP servers with a Bearer token for each ([`mcp`]). It talks with the
//! Company's agent in conversations a reader follows ([`conversation`]) and
//! calls its OpenAPI operations ([`api`]), each request carrying the DPoP
//! Session Token and a proof ([`request`]); what it keeps of them — the
//! conversations and their cursors, the ids a retry reuses — is sealed with
//! the User ID ([`ledger`]). The Session is signed
//! out until the User signs in themselves, at a gate agentd opens for them
//! ([`signin`]); the Account Token that gives agentd is kept sealed, and
//! only the supervisor ever reads it ([`account`]). The User may hold several
//! accounts at one Company, each named by a label of agentd's own and signed
//! in to in a Session of its own. An action a Company
//! proposes is confirmed only with the User's approval of its terms, or a
//! standing permission the operator declared ([`operations`]), and the User
//! talks with the Company themselves only when they choose to
//! ([`direct`]). No turn
//! worker, subagent or instance child is handed a Company server to dial, a
//! key or a token: their calls come back to the supervisor (§5.6). What a
//! Company is sent carries none of agentd's correlators under the strict
//! privacy profile ([`privacy`]), and which Companies' content each context
//! holds decides when a call must ask the User first ([`carry`]).
//!
//! Built with the `companies` feature. [`wire`] (every constant the
//! protocol puts on the wire) and [`mock`] (the fake Company the Personal
//! Agent is tested against) are also compiled where the mock MCP servers
//! are — debug builds, or `internal-mocks` — with `oauth` and `tls`.

#[cfg(feature = "companies")]
pub mod account;
#[cfg(feature = "companies")]
pub mod api;
#[cfg(feature = "companies")]
pub mod carry;
#[cfg(all(feature = "companies", feature = "a2a"))]
pub mod cli;
#[cfg(feature = "companies")]
pub mod conversation;
#[cfg(feature = "companies")]
pub mod direct;
#[cfg(feature = "companies")]
pub mod discover;
#[cfg(feature = "companies")]
pub mod dpop;
#[cfg(feature = "companies")]
pub mod http;
#[cfg(feature = "companies")]
pub mod identity;
#[cfg(feature = "companies")]
pub mod ledger;
#[cfg(feature = "companies")]
pub mod mcp;
#[cfg(all(
    feature = "oauth",
    feature = "tls",
    any(debug_assertions, feature = "internal-mocks")
))]
pub mod mock;
#[cfg(feature = "companies")]
pub mod operations;
#[cfg(feature = "companies")]
pub mod privacy;
#[cfg(feature = "companies")]
pub mod request;
#[cfg(feature = "companies")]
pub mod session;
#[cfg(feature = "companies")]
pub mod signin;
#[cfg(feature = "companies")]
pub mod token;
#[cfg(feature = "companies")]
pub mod user_id;
pub mod wire;

#[cfg(feature = "companies")]
pub use supervisor::{Company, Notice, Notify, Supervisor};

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
    identity::Identity::load(
        client,
        crate::config::settings::companies::signs_in_directly(settings),
    )?
    .print(dir)
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
    use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
    use std::sync::{Arc, Condvar, Mutex};
    use std::time::Duration;

    use serde_json::json;

    use super::account::{Accounts, DEFAULT_LABEL, is_label};
    use super::discover::{Discovery, Entry, Note, Verified};
    use super::http::Pace;
    use super::identity::Identity;
    use super::ledger::Ledger;
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

        /// The issuer the Company was last verified with.
        pub fn issuer(&self) -> Option<String> {
            self.0.issuer()
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

        /// Calls in flight at the Company now.
        pub fn in_flight(&self) -> u32 {
            self.in_flight.load(Ordering::SeqCst)
        }

        /// The Company's not-before instant.
        pub fn pace(&self) -> &Pace {
            &self.discovery.entry.pace
        }

        /// The issuer the Company was last verified with — read without
        /// fetching anything.
        pub fn issuer(&self) -> Option<String> {
            self.discovery.last().map(|v| v.issuer.clone())
        }

        /// The Company as a gate names it: its own name quoted, beside the
        /// domain agentd verified (the entry's endpoint host before that).
        pub fn named(&self) -> String {
            match self.discovery.last() {
                Some(v) => super::carry::named(&v.name, &v.domain),
                None => {
                    let host = cfg::host_of(&self.discovery.entry.endpoint).unwrap_or_default();
                    super::carry::named(&host, &host)
                }
            }
        }

        /// The host globs every URL of the Company is held to: those it was
        /// last verified with, else the entry's own, else the endpoint's host
        /// and its subdomains.
        pub fn hosts(&self) -> Vec<String> {
            if let Some(v) = self.discovery.last() {
                return v.hosts.clone();
            }
            let e = &self.discovery.entry;
            e.hosts.clone().unwrap_or_else(|| {
                let home = cfg::host_of(&e.endpoint).unwrap_or_default();
                vec![home.clone(), format!("*.{home}")]
            })
        }
    }

    /// What the supervisor tells the runtime, off the loop.
    #[derive(Debug)]
    pub enum Notice {
        /// An operation reached a final state the runtime has not been told
        /// of: for the stream `company.<name>` and the audit stream.
        Final(super::operations::Final),
        /// A Company asks to talk with the User directly in one of its
        /// conversations (draft §7.10).
        DirectAsked(super::direct::Asked),
        /// A fetch of the OpenAPI description at this URL found it changed: the
        /// operation tools made from it are listed again.
        DescriptionChanged(String),
        /// A discovery refetch found the OpenAPI APIs the company entry's
        /// Company lists changed: the operation tools of each are listed
        /// again, so they follow an API that moved and are withdrawn from one
        /// no longer listed.
        ApisChanged(String),
    }

    /// Where notices go: the runtime's loop.
    pub type Notify = Arc<dyn Fn(Notice) + Send + Sync>;

    /// What the checks before the first Session came to.
    enum Ready {
        Unchecked,
        Passed,
        /// The last failure, said once until it changes.
        Failed(String),
    }

    /// The Sessions, by issuer and the label of the account each is signed
    /// in to — none for the signed-out Session.
    type Sessions = BTreeMap<(String, Option<String>), Arc<Session>>;

    /// The supervisor's Personal Agent: the identity, the User IDs, the
    /// Companies and their Sessions.
    pub struct Supervisor {
        pub identity: Identity,
        users: UserIds,
        /// Where the User's Account Tokens are kept, sealed; none without a
        /// seal key, which load requires of any `sign_in`.
        pub(super) accounts: Option<Accounts>,
        /// The Direct Sign-Ins waiting for their callback, by `state`.
        pub(super) direct: Mutex<BTreeMap<String, super::signin::Direct>>,
        /// The callbacks that named none, as the log has been told of them.
        pub(super) callbacks_refused: Mutex<crate::obs::quiet::Health>,
        principal: String,
        companies: BTreeMap<String, Arc<Company>>,
        /// Each Company MCP server's entry.
        servers: BTreeMap<String, String>,
        /// The Sessions, by issuer and the label of the account each is
        /// signed in to — none for the signed-out Session: several domains
        /// naming one issuer are one Company (draft §3.2), and a Session is
        /// signed in to at most one account (§4.4).
        sessions: Mutex<Sessions>,
        /// Whether the seal key and the published identity have checked out.
        ready: Mutex<Ready>,
        /// The Companies that started over with a seal key that opens none
        /// of the store's records ([`UserIds::start_over_at`]), checked out
        /// while the others wait for their own reset.
        started_over: Mutex<BTreeSet<String>>,
        /// What agentd keeps of its conversations and calls at each Company.
        pub(super) ledger: Ledger,
        /// The readers of the open conversations.
        pub(super) readers: Mutex<super::conversation::Readers>,
        /// Where the readers' events go: the runtime.
        pub(super) sink: Mutex<Option<super::conversation::Sink>>,
        /// `security.companies.limits.conversations`.
        pub(super) conversation_limit: u32,
        /// The OpenAPI descriptions fetched, by URL.
        pub(super) descriptions: Mutex<BTreeMap<String, Arc<super::api::Description>>>,
        /// The operations `api_allow` lists that have no tool, and why, by
        /// server: as the log was last told.
        pub(super) unoffered: Mutex<BTreeMap<String, BTreeMap<String, String>>>,
        /// Why each server's operation tools could not be made, as the log
        /// was last told.
        pub(super) api_failed: Mutex<BTreeMap<String, String>>,
        /// Whether the client metadata document lists the Operations
        /// extension.
        pub(super) operations: bool,
        /// The standing permissions, as configured.
        pub(super) standing: Vec<super::operations::Rule>,
        /// The Companies and account labels whose standing permissions are
        /// suspended until a restart.
        pub(super) standing_suspended: Mutex<BTreeSet<(String, String)>>,
        /// Wakes each Company's operations tracker.
        pub(super) track_wake: (Mutex<u64>, Condvar),
        /// This process's own: a confirm's lock names it, so a lock another
        /// life left is known as one.
        pub(super) life: String,
        /// Where notices for the runtime go.
        pub(super) notify: Mutex<Option<Notify>>,
        /// Whether the User can be asked to join a Direct Conversation a
        /// Company asks for: a listener serves, and unowned work may gate.
        pub(super) direct_askable: AtomicBool,
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
            let identity = Identity::load(client, cfg::signs_in_directly(settings))?;
            let seal = match &c.seal_key {
                Some(s) => {
                    let text = crate::sec::secret::resolve(&s.0, &|k| std::env::var(k).ok())
                        .map_err(|e| format!("security.companies.seal_key: {e}"))?;
                    Some(user_id::seal_bytes(&text)?)
                }
                None => None,
            };
            let accounts = match seal {
                Some(raw) => Some(Accounts::new(Arc::clone(&durable), &client.id, raw)?),
                None => None,
            };
            let ledger = Ledger::new(Arc::clone(&durable), &client.id, seal)?;
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
                let entry = Entry {
                    pace: Arc::new(Pace::new(name, Some(log.clone()))),
                    ..Entry::from_settings(name, svc, timeout)
                };
                companies.insert(
                    name.clone(),
                    Arc::new(Company {
                        name: name.clone(),
                        discovery: Discovery::new(entry),
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
                accounts,
                direct: Mutex::default(),
                callbacks_refused: Mutex::new(crate::obs::quiet::Health::new()),
                principal: c.user().to_string(),
                companies,
                servers,
                sessions: Mutex::default(),
                ready: Mutex::new(Ready::Unchecked),
                started_over: Mutex::default(),
                ledger,
                readers: Mutex::default(),
                sink: Mutex::default(),
                conversation_limit: c.conversations(),
                descriptions: Mutex::default(),
                unoffered: Mutex::default(),
                api_failed: Mutex::default(),
                operations: cfg::lists_operations(settings),
                standing: c
                    .standing
                    .iter()
                    .filter_map(super::operations::Rule::of)
                    .collect(),
                standing_suspended: Mutex::default(),
                track_wake: (Mutex::new(0), Condvar::new()),
                life: crate::sec::random::b64url_token(12)
                    .map_err(|e| format!("no OS randomness: {e}"))?,
                notify: Mutex::default(),
                direct_askable: AtomicBool::new(false),
                timeout,
                log: log.clone(),
            })))
        }

        /// Where notices for the runtime go: an operation's final state, a
        /// Company's request to talk with the User.
        pub fn notify_to(&self, sink: Notify) {
            *self.notify.lock().unwrap_or_else(|e| e.into_inner()) = Some(sink);
        }

        /// Whether the User can be asked to join a Direct Conversation a
        /// Company asks for; when not, agentd says in the conversation that
        /// they are not available.
        pub fn set_direct_askable(&self, askable: bool) {
            self.direct_askable.store(askable, Ordering::SeqCst);
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

        /// Every declared Company, by entry name.
        pub fn companies(&self) -> impl Iterator<Item = &Arc<Company>> {
            self.companies.values()
        }

        /// How a gate names the Company whose issuer is `issuer`: as
        /// [`Company::named`] when one was last verified with it, else the
        /// issuer itself, quoted.
        pub fn named_by_issuer(&self, issuer: &str) -> String {
            self.companies
                .values()
                .find(|c| c.issuer().as_deref() == Some(issuer))
                .map_or_else(|| super::quoted(issuer, 128), |c| c.named())
        }

        /// The (User, Company) pair's own secret, which the privacy profile
        /// derives from: from the Session's record, read or minted the first
        /// time. May fetch the Company's documents once they expire, so it is
        /// called off the loop.
        pub fn secret(&self, company: &Company) -> Result<String, String> {
            let verified = self.verify(company)?;
            self.session(&verified.issuer, None)?
                .secret(&self.users, &self.principal)
        }

        /// The `_meta` a call carries under the strict privacy profile (RFC
        /// 0047 §5.10), and the span it names: the call's idempotency key
        /// `key` keyed under the (User, Company) pair's secret, and — when
        /// the call is made for a unit of work — a span of the trace the
        /// unit has at this Company. Nothing else of agentd's. A secret that
        /// cannot be read fails the call, unsent.
        pub fn private_meta(
            &self,
            slot: &Slot,
            key: &str,
            unit: Option<&crate::obs::trace::TraceContext>,
        ) -> Result<(serde_json::Value, Option<crate::obs::trace::TraceContext>), String> {
            let secret = self
                .secret(&slot.0)
                .map_err(|e| format!("{e}; the call was not sent"))?;
            let mut meta = serde_json::Map::new();
            meta.insert(
                "agent/idempotency_key".into(),
                json!(super::privacy::idempotency_key(&secret, key)),
            );
            let span = unit.map(|u| super::privacy::reroot(&secret, u));
            if let Some(s) = &span {
                meta.extend(s.meta());
            }
            Ok((serde_json::Value::Object(meta), span))
        }

        /// The span a conversation or API request made for the unit of work
        /// `unit` names in its `traceparent` header, and the one agentd's
        /// span for the request, `call`, links to (RFC 0047 §5.10, §7):
        /// under the strict profile the trace the unit has at this Company,
        /// which only that link ties to the unit's; under `standard`, `call`
        /// itself, a span of the unit's trace. A secret that cannot be read
        /// fails the request, unsent.
        pub fn request_trace(
            &self,
            company: &Company,
            unit: Option<&crate::obs::trace::TraceContext>,
            call: Option<&crate::obs::trace::TraceContext>,
        ) -> Result<
            (
                Option<crate::obs::trace::TraceContext>,
                Option<crate::obs::trace::TraceContext>,
            ),
            String,
        > {
            if company.privacy == Privacy::Standard {
                return Ok((call.cloned(), None));
            }
            let Some(unit) = unit else {
                return Ok((None, None));
            };
            let secret = self
                .secret(company)
                .map_err(|e| format!("{e}; the request was not sent"))?;
            let sent = super::privacy::reroot(&secret, unit);
            Ok((Some(sent.clone()), Some(sent)))
        }

        /// The span a request that belongs to no unit of work names in its
        /// `traceparent` header — a reader's, a decline's: none under the
        /// strict profile, the process's own under `standard`.
        pub fn connection_trace(company: &Company) -> Option<crate::obs::trace::TraceContext> {
            (company.privacy == Privacy::Standard)
                .then(crate::obs::trace::process)
                .flatten()
        }

        /// Whether the Session of the account `account` names at `company` —
        /// or the signed-out Session — is signed in, once one is open.
        pub fn signed_in(&self, company: &Company, account: Option<&str>) -> Option<bool> {
            let issuer = company.issuer()?;
            let sessions = self.sessions.lock().unwrap_or_else(|e| e.into_inner());
            sessions
                .get(&(issuer, account.map(str::to_string)))
                .and_then(|s| s.is_signed_in())
        }

        /// The labels of the accounts the User holds at the Company `issuer`
        /// names, as the store keeps them now.
        fn labels_at(&self, issuer: &str) -> Result<Vec<String>, String> {
            match &self.accounts {
                Some(a) => a.labels(&self.principal, issuer),
                None => Ok(Vec::new()),
            }
        }

        /// The labels of the accounts the User holds at `company`, under the
        /// issuer it was last verified with; none before it was.
        pub fn accounts_at(&self, company: &Company) -> Vec<String> {
            company
                .issuer()
                .and_then(|i| self.labels_at(&i).ok())
                .unwrap_or_default()
        }

        /// The account the Company's MCP servers act with, whose schemas are
        /// the Company's and take no account: the one labelled
        /// [`DEFAULT_LABEL`] when it is held, else the only one held, else
        /// none — the signed-out Session.
        pub fn mcp_account(&self, issuer: &str) -> Option<String> {
            let labels = self.labels_at(issuer).ok()?;
            if labels.iter().any(|l| l == DEFAULT_LABEL) {
                return Some(DEFAULT_LABEL.to_string());
            }
            match labels.as_slice() {
                [only] => Some(only.clone()),
                _ => None,
            }
        }

        /// Whether the Session `company`'s MCP servers act with holds an
        /// Account Token — under the issuer it was last verified with.
        pub fn holds_account(&self, company: &Company) -> bool {
            company
                .issuer()
                .is_some_and(|i| self.mcp_account(&i).is_some())
        }

        /// Which of the User's accounts at `company` a call that acts with
        /// one goes in (RFC 0047 §9, P5b): `named`, when it names one that is
        /// held; else the only one held; else, with none held, the signed-out
        /// Session (`None`). A label not held, or none named while several
        /// are, is refused — nothing is sent.
        pub fn account_for(
            &self,
            company: &Company,
            named: Option<&str>,
        ) -> Result<Option<String>, String> {
            let name = &company.name;
            if let Some(l) = named.filter(|l| !is_label(l)) {
                return Err(format!(
                    "company {name}: {}; nothing was sent",
                    super::account::label_refused(l)
                ));
            }
            let issuer = self
                .verify(company)
                .map_err(|e| format!("company {name} is unavailable: {e}"))?
                .issuer
                .clone();
            let held = self.labels_at(&issuer)?;
            pick(name, &held, named)
        }

        /// The account `kept` at the Company `issuer` names, held: a call
        /// kept in it goes on in it — refused unsent once its Account Token is
        /// no longer held — and one that names another account is refused.
        /// Kept in none, the one `named`, as [`Self::account_for`] picks it.
        /// Nothing is fetched.
        pub(super) fn kept_account(
            &self,
            company: &Company,
            issuer: &str,
            kept: Option<&str>,
            named: Option<&str>,
            what: &str,
        ) -> Result<Option<String>, String> {
            let name = &company.name;
            match (kept, named) {
                (Some(k), Some(n)) if k != n => Err(format!(
                    "company {name}: {what} belongs to the account {k:?}, not {n:?}; nothing was sent"
                )),
                (Some(k), _) => {
                    if !self.labels_at(issuer)?.iter().any(|l| l == k) {
                        return Err(format!(
                            "company {name}: {what} belongs to the account {k:?}, which is not signed in here — company.sign_in with account {k:?} signs the User in to it again; nothing was sent"
                        ));
                    }
                    Ok(Some(k.to_string()))
                }
                (None, named) => {
                    if let Some(l) = named.filter(|l| !is_label(l)) {
                        return Err(format!(
                            "company {name}: {}; nothing was sent",
                            super::account::label_refused(l)
                        ));
                    }
                    pick(name, &self.labels_at(issuer)?, named)
                }
            }
        }

        /// Why `company` is unavailable, when the checks before the first
        /// Session failed — every Company's, unless it started over.
        pub fn not_ready(&self, company: &Company) -> Option<String> {
            match &*self.ready.lock().unwrap_or_else(|e| e.into_inner()) {
                Ready::Failed(_) if self.has_started_over(company) => None,
                Ready::Failed(e) => Some(e.clone()),
                _ => None,
            }
        }

        fn has_started_over(&self, company: &Company) -> bool {
            self.started_over
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .contains(&company.name)
        }

        /// `company.status` for one Company, as the supervisor knows it now —
        /// nothing is fetched. `joined` names the Company's MCP servers the
        /// runtime has connected. `detail` adds why a failed Company failed,
        /// in agentd's words with the values its documents gave: the
        /// operator's view, not the model's.
        pub fn status_of(
            &self,
            company: &Company,
            joined: &BTreeSet<String>,
            detail: bool,
        ) -> serde_json::Value {
            let last = company.discovery.last();
            let failed = company.discovery.failed();
            let (state, error) = if let Some(why) = company.discovery.suspended() {
                ("suspended", Some(why))
            } else if let Some(why) = self.not_ready(company) {
                ("unavailable", Some(why))
            } else if last.is_some() {
                ("verified", None)
            } else if let Some((_, line)) = &failed {
                ("unavailable", Some(line.clone()))
            } else {
                ("unverified", None)
            };
            let servers: Vec<serde_json::Value> = self
                .servers
                .iter()
                .filter(|(_, entry)| **entry == company.name)
                .map(|(server, _)| {
                    let unusable = last
                        .as_ref()
                        .and_then(|v| v.mcp.get(server))
                        .is_some_and(|url| company.is_unusable(url));
                    json!({"server": server, "joined": joined.contains(server), "unusable": unusable})
                })
                .collect();
            let mcp = company.issuer().and_then(|i| self.mcp_account(&i));
            let accounts: Vec<serde_json::Value> = self
                .accounts_at(company)
                .into_iter()
                .map(|label| self.account_status(company, &label))
                .collect();
            let mut doc = json!({
                "company": company.name,
                "state": state,
                "reason": failed.filter(|_| last.is_none()).map(|(r, _)| r),
                "domain": last.as_ref().map(|v| v.domain.clone()),
                "issuer": last.as_ref().map(|v| v.issuer.clone()),
                "privacy": match company.privacy {
                    Privacy::Strict => "strict",
                    Privacy::Standard => "standard",
                },
                "signed_in": self.signed_in(company, mcp.as_deref()),
                "accounts": accounts,
                "rate_limited_until": company
                    .pace()
                    .until()
                    .map(crate::obs::log::rfc3339_millis),
                "in_flight": company.in_flight(),
                "limit": company.limit(),
                "servers": servers,
            });
            if detail {
                doc["error"] = json!(error);
            }
            doc
        }

        /// One account `company.status` lists: its label, the scopes its
        /// Account Token grants and when it stops being good, how the User
        /// signed in and whether its Session is signed in — never a token or
        /// a Session's id.
        fn account_status(&self, company: &Company, label: &str) -> serde_json::Value {
            let held = match (&self.accounts, company.issuer()) {
                (Some(a), Some(i)) => a.load(&self.principal, &i, label).ok().flatten(),
                _ => None,
            };
            json!({
                "account": label,
                "scope": held.as_ref().map(|a| a.scope.clone()).unwrap_or_default(),
                "expires_at": held
                    .as_ref()
                    .and_then(|a| a.expires_at_ms)
                    .and_then(|ms| std::time::UNIX_EPOCH.checked_add(Duration::from_millis(ms)))
                    .map(crate::obs::log::rfc3339_millis),
                "kind": held.as_ref().map(|a| a.kind.clone()),
                "signed_in": self.signed_in(company, Some(label)),
            })
        }

        /// `company.user_id.reset`: forget the User ID agentd keeps for the
        /// User at `company`, with its secret and its `session_id`, and drop
        /// the Session held there — the Company sees a new User from the
        /// next call on, and loses whatever it kept for the old one. The
        /// issuer is the one the Company was last verified with; nothing is
        /// fetched.
        ///
        /// With a seal key that opens none of the store's records, every
        /// Company is unavailable and none is verified: the reset starts
        /// this one over with the key, as a new User, and the others stay
        /// unavailable until each is reset too (RFC 0047 §5.5.3). Once every
        /// Company has started over, the store takes the key.
        pub fn reset_user(&self, company: &Company) -> Result<serde_json::Value, String> {
            if self.users.check_seal().is_err() && !self.has_started_over(company) {
                self.users.start_over_at(&company.name)?;
                let all: Vec<&str> = self.companies.keys().map(String::as_str).collect();
                if all.iter().all(|e| self.users.started_over(e)) {
                    self.users.start_over(&all)?;
                    *self.ready.lock().unwrap_or_else(|e| e.into_inner()) = Ready::Unchecked;
                    self.started_over
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .clear();
                }
                return Ok(self.said_reset(company, true));
            }
            let issuer = company.issuer().ok_or_else(|| {
                format!(
                    "company {} has not been verified since agentd started, so the issuer its User ID is kept under is not known",
                    company.name
                )
            })?;
            self.users.forget(&self.principal, &issuer)?;
            // The conversations were the old User's (draft §7.2): their
            // readers stop, and what was kept of them goes.
            for cid in self
                .ledger
                .load(&self.principal, &issuer)
                .map(|b| b.conversations.into_keys().collect::<Vec<_>>())
                .unwrap_or_default()
            {
                self.stop_reader(&issuer, &cid);
            }
            self.ledger.forget(&self.principal, &issuer)?;
            self.sessions
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .retain(|(i, _), _| *i != issuer);
            Ok(self.said_reset(company, false))
        }

        /// Take every Account Token the User holds at `company` out of the
        /// store, by its account's label, for the caller to revoke: what
        /// `company.user_id.reset` does first, so no request after it is
        /// signed in as the old User. Each is taken under its Session's lock
        /// ([`Session::take_account`]); the issuer is the one the Company was
        /// last verified with, and nothing is fetched.
        pub fn take_accounts(
            &self,
            company: &Company,
        ) -> Result<Vec<(String, super::account::Account)>, String> {
            let (Some(store), Some(issuer)) = (&self.accounts, company.issuer()) else {
                return Ok(Vec::new());
            };
            let mut taken = Vec::new();
            for label in self.labels_at(&issuer)? {
                if let Some(a) = self.session(&issuer, Some(&label))?.take_account(
                    store,
                    &self.principal,
                    &company.name,
                )? {
                    taken.push((label, a));
                }
            }
            Ok(taken)
        }

        fn said_reset(&self, company: &Company, started_over: bool) -> serde_json::Value {
            self.log.info(
                "company.user_id.reset",
                json!({"company": company.name, "started_over": started_over}),
            );
            json!({"company": company.name, "reset": true, "started_over": started_over})
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
        ///
        /// A Company that started over with a seal key the store's records
        /// do not open ([`Self::reset_user`]) passes on its own; the others
        /// stay unavailable.
        fn ready(&self, company: &Company) -> Result<(), String> {
            let mut ready = self.ready.lock().unwrap_or_else(|e| e.into_inner());
            if matches!(*ready, Ready::Passed) || self.has_started_over(company) {
                return Ok(());
            }
            let outcome = match self.users.check_seal() {
                Ok(()) => Ok(true),
                Err(_) if self.users.started_over(&company.name) => Ok(false),
                Err(e) => Err(("company.seal_key.mismatch", e)),
            }
            .and_then(|all| {
                self.identity
                    .self_check(self.timeout.min(Duration::from_secs(10)))
                    .map(|()| all)
                    .map_err(|e| ("company.identity.unpublished", e))
            });
            match outcome {
                Ok(true) => {
                    *ready = Ready::Passed;
                    Ok(())
                }
                Ok(false) => {
                    self.started_over
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .insert(company.name.clone());
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
            self.ready(company).and_then(|()| {
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
                Note::OpenapiChanged => {
                    let sink = self
                        .notify
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .clone();
                    if let Some(sink) = sink {
                        sink(Notice::ApisChanged(company.to_string()));
                    }
                }
            }
        }

        /// The Session at `issuer` of the account `account` names — or the
        /// signed-out Session — made with a new DPoP key the first time.
        fn session(&self, issuer: &str, account: Option<&str>) -> Result<Arc<Session>, String> {
            let mut s = self.sessions.lock().unwrap_or_else(|e| e.into_inner());
            let key = (issuer.to_string(), account.map(str::to_string));
            if let Some(x) = s.get(&key) {
                return Ok(Arc::clone(x));
            }
            let x = Arc::new(Session::new(issuer, account)?);
            s.insert(key, Arc::clone(&x));
            Ok(x)
        }

        /// The Session of the account `account` names at `company` is
        /// forgotten: its account was signed out of, and a sign-in to it
        /// again starts a Session of its own, with a DPoP key of its own.
        pub(super) fn drop_session(&self, company: &Company, account: &str) {
            if let Some(issuer) = company.issuer() {
                self.sessions
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .remove(&(issuer, Some(account.to_string())));
            }
        }

        /// The Session `session_id` names at the Company `issuer` names —
        /// the last account's, signed out of — is the signed-out Session from
        /// now on, kept with the User ID: it goes on signed out, renewed with
        /// the assertion (draft §4.9: "Usually those Sessions continue signed
        /// out, so a cart or an open conversation carries on").
        pub(super) fn go_on_signed_out(
            &self,
            issuer: &str,
            session_id: &str,
        ) -> Result<(), String> {
            let mut rec = self.users.load_or_mint(&self.principal, issuer)?;
            rec.session_id = Some(session_id.to_string());
            self.users.save(&self.principal, issuer, &rec)?;
            self.sessions
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&(issuer.to_string(), None));
            Ok(())
        }

        /// `f`, with the Session at `company` of the account `account`
        /// names — or the signed-out Session — and what its requests are made
        /// with: the documents verified — fetched again once they expire —
        /// and the Session made the first time. Unreported, for the caller
        /// to report as part of its own attempt.
        pub fn with_session<T>(
            &self,
            company: &Company,
            account: Option<&str>,
            f: impl FnOnce(&Session, &Env<'_>) -> Result<T, String>,
        ) -> Result<T, String> {
            let verified = self.verify(company)?;
            let session = self.session(&verified.issuer, account)?;
            let dialer = verified.dialer(&company.discovery.entry);
            let env = Env {
                identity: &self.identity,
                verified: &verified,
                dialer: &dialer,
                users: &self.users,
                accounts: self.accounts.as_ref(),
                principal: &self.principal,
                ceiling: &company.discovery.entry.scopes,
                log: &self.log,
            };
            f(&session, &env)
        }

        /// A Bearer token for `company`'s MCP server at `url`, of the Session
        /// its MCP servers act with ([`Self::mcp_account`]): discovery, the
        /// Session and the token, reported as one outcome for target
        /// `company:<name>`.
        pub fn bearer(&self, company: &Company, url: &str) -> Result<String, String> {
            let attempt = self.verify(company).and_then(|v| {
                let account = self.mcp_account(&v.issuer);
                self.with_session(company, account.as_deref(), |session, env| {
                    session.open(env)?;
                    session.bearer(env, url)
                })
            });
            self.report(company, attempt)
        }

        /// `url` refused `token`: the next request gets a new one, whichever
        /// Session it was of.
        pub fn session_refused(&self, company: &Company, url: &str, token: &str) {
            let Some(issuer) = company.issuer() else {
                return;
            };
            let sessions: Vec<Arc<Session>> = self
                .sessions
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .iter()
                .filter(|((i, _), _)| *i == issuer)
                .map(|(_, s)| Arc::clone(s))
                .collect();
            for s in sessions {
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

    /// The account a call that names `named` goes in at `company`, where the
    /// User holds the accounts `held`: the one it names, when held; with none
    /// named, the only one held, or — with none held — the signed-out Session.
    /// Otherwise why none, naming what is held.
    fn pick(company: &str, held: &[String], named: Option<&str>) -> Result<Option<String>, String> {
        match (named, held) {
            (Some(n), _) if held.iter().any(|l| l == n) => Ok(Some(n.to_string())),
            (Some(n), []) => Err(format!(
                "company {company}: the User holds no account {n:?} here — none is signed in; company.sign_in with account {n:?} signs them in to it; nothing was sent"
            )),
            (Some(n), _) => Err(format!(
                "company {company}: the User holds no account {n:?} here (the accounts signed in are {}); company.sign_in with account {n:?} signs them in to it; nothing was sent",
                held.join(", ")
            )),
            (None, []) => Ok(None),
            (None, [only]) => Ok(Some(only.clone())),
            (None, several) => Err(format!(
                "company {company}: the User holds several accounts here ({}): name the one to act with as account; nothing was sent",
                several.join(", ")
            )),
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        /// A call goes in the account it names when that is held; naming
        /// none, in the only one held, or the signed-out Session while none
        /// is — and naming none while several are, or one not held, is
        /// refused, naming what is held.
        #[test]
        fn a_call_goes_in_the_account_it_names_or_the_only_one() {
            let held = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
            assert_eq!(pick("shop", &[], None), Ok(None), "signed out");
            assert_eq!(
                pick("shop", &held(&["work"]), None),
                Ok(Some("work".into()))
            );
            assert_eq!(
                pick("shop", &held(&["default", "work"]), Some("work")),
                Ok(Some("work".into()))
            );
            let e = pick("shop", &held(&["default", "work"]), None).unwrap_err();
            assert!(
                e.contains("several accounts here (default, work)")
                    && e.ends_with("nothing was sent"),
                "{e}"
            );
            let e = pick("shop", &held(&["default"]), Some("home")).unwrap_err();
            assert!(
                e.contains("no account \"home\" here (the accounts signed in are default)"),
                "{e}"
            );
            let e = pick("shop", &[], Some("home")).unwrap_err();
            assert!(e.contains("none is signed in"), "{e}");
        }

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
