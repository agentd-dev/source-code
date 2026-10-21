// SPDX-License-Identifier: AGPL-3.0-only
//! The grant vault (RFC 0044 §5.5): a person's delegation grant, the one
//! durable user credential in the design, kept as a [`Kind::Cred`] record
//! under its `grant_id` — its metadata in the clear, its refresh token
//! sealed with AES-256-GCM, the associated data `("delegation-grant",
//! grant_id, idp_iss, sub, epoch, rt_version)` — so a sealed blob does not
//! open under another grant, epoch or refresh-token version.
//!
//! **Who holds the key** ([`Custody`]). Under `key_custody: local` the seal
//! key is `identity.delegation.seal_key`, read by the supervisor. Under
//! `key_custody: external` (RFC 0044 P2a) agentd holds no key at all: each
//! write draws a fresh 32-byte data key, seals the refresh token with it,
//! and keeps it only as the external signer wraps it under its
//! key-encryption key (`signer.seal_key`), bound to the record by the same
//! parts under `"delegation-grant-key"`. The record a rotated refresh token
//! is kept in has its data key drawn and wrapped before the IdP is asked
//! ([`Ahead`]), so keeping the token needs no signer. Opening asks the
//! signer to unwrap; the data key is wiped once the record is sealed or
//! opened. A record sealed under one custody does not open under the other:
//! changing it means enrolling again, as losing the seal key does.
//!
//! **The tombstone register** is a record of its own, written only upward:
//! per grant, the highest epoch revoked and the latest `rt_version` stored.
//! Every load checks both. A revoked epoch is refused; a record whose
//! `rt_version` is below the floor is an older blob holding a rotated-out
//! refresh token — a store restored from a backup, an instance respawned from
//! an old snapshot — and presenting it could trip the IdP's reuse detection
//! and revoke the whole token family, so it is refused for a new enrolment
//! instead. With `observability.audit.chain` set, every raise is anchored in
//! the hash-chained audit log before the store is written, and the floors
//! checked are the higher of the store's and the chain's: a store restored
//! with its register, or a register truncated by a writer of the store, no
//! longer brings a revoked grant back. The anchor adds to the store's
//! floor and never stands in front of it: an anchor the chain does not take
//! is said (`delegation.register.unanchored`), the store's register is
//! raised and a revoked record deleted all the same, and every floor the
//! chain lacks is anchored again at the next raise and at the next start.
//!
//! **Writes compare and swap on `rt_version`**: [`Vault::store`] writes only
//! over the version it read, inside the per-grant lock the caller holds, so
//! a rotation is persisted once and before the new token is used.
//! `seal_key_previous` opens records sealed before a rotation; every write
//! seals with `seal_key`.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};

use crate::a2a::principals::typed::PrincipalRef;
use crate::auth::seal::{SealKey, associated_data};
use crate::b64;
use crate::obs::chain::{Anchor, Chain};
use crate::sec::signer::{Signer, SignerError};
use crate::state::{Durable, Kind};
use crate::store::StoreError;

/// Where the grants are, by `grant_id`.
const GRANT_PREFIX: &str = "delegation/grant/";
/// The tombstone register's one record.
const REGISTER: &str = "delegation/register";

/// A grant's metadata — everything but the refresh token.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Grant {
    pub grant_id: String,
    pub principal: PrincipalRef,
    /// How the enrolling principal was authenticated: `unix-uid:<n>`.
    pub fingerprint: String,
    /// The `identity.providers` entry.
    pub idp: String,
    pub idp_iss: String,
    pub sub: String,
    /// SHA-256 of the `email` claim, hex: never bound by.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email_hash: Option<String>,
    pub template: String,
    /// The scope ceiling on each server the template allowed.
    pub ceilings: BTreeMap<String, Vec<String>>,
    pub created_ms: u64,
    pub expires_ms: u64,
    pub epoch: u64,
    pub rt_version: u64,
    /// Mints so far, per resource, in the day and the month they were made.
    #[serde(default)]
    pub quota: BTreeMap<String, Quota>,
    /// What bound a device name to this grant's `iss` + `sub` (RFC 0044
    /// §5.4): `pin`, or `first_binding`; none for any other principal.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bound_by: Option<String>,
    /// The `act_for` workflows the person consented to run for them with
    /// nobody there, by name, each at the definition digest it had when
    /// they signed in (RFC 0044 §5.12 E): another digest suspends its
    /// firings until they sign in again.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub unattended: BTreeMap<String, String>,
    /// The CIBA requests sent them today, against `ciba.per_day`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ciba: Option<Quota>,
}

/// Mints counted against a resource.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Quota {
    /// `YYYY-MM-DD` (UTC) of `day_count`.
    pub day: String,
    pub day_count: u32,
    /// `YYYY-MM` (UTC) of `month_count`.
    pub month: String,
    pub month_count: u32,
}

impl Quota {
    /// The counts as of `now_ms`: a new day or month starts at zero.
    pub fn at(&self, now_ms: u64) -> Quota {
        let (day, month) = days(now_ms);
        Quota {
            day_count: if self.day == day { self.day_count } else { 0 },
            month_count: if self.month == month {
                self.month_count
            } else {
                0
            },
            day,
            month,
        }
    }
}

/// `(YYYY-MM-DD, YYYY-MM)` of `ms` since the epoch, UTC.
pub fn days(ms: u64) -> (String, String) {
    let days = (ms / 86_400_000) as i64;
    // Civil-from-days (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    (format!("{y:04}-{m:02}-{d:02}"), format!("{y:04}-{m:02}"))
}

/// What the register keeps of one grant.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Floor {
    /// The highest epoch revoked; a record at or below it is refused.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revoked_epoch: Option<u64>,
    /// The latest `rt_version` stored.
    pub rt_version: u64,
}

/// The stored record: the metadata, the sealed secret, and — under an
/// external signer — the data key it is sealed with, wrapped.
#[derive(Serialize, Deserialize)]
struct Stored {
    grant: Grant,
    /// base64url of the sealed refresh token.
    sealed: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    dek: Option<Wrapped>,
}

/// Where each principal's personal key is kept, by the SHA-256 of its id.
const PERSONAL_PREFIX: &str = "delegation/personal/";

/// A principal's personal key (RFC 0044 §6.4): the key their records'
/// personal content is sealed with, itself kept as a grant's refresh token
/// is — `sealed` under a local seal key, or `wrapped` by the signer.
#[derive(Serialize, Deserialize)]
struct StoredPersonal {
    principal: String,
    /// Named by every record it seals.
    id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    sealed: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    wrapped: Option<Wrapped>,
}

/// Where the refresh token an IdP issued with a firing's CIBA approval is
/// kept while the firing's run lasts, by firing and grant.
const APPROVAL_PREFIX: &str = "delegation/approval/";

/// Whose a CIBA approval's refresh token is (RFC 0044 §7.P3c): the firing
/// its person approved, its run, the grant it was asked at, and the IdP it
/// is revoked at.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Approval {
    pub firing: String,
    pub run: String,
    pub grant_id: String,
    pub idp: String,
}

impl Approval {
    /// Its record's name under [`APPROVAL_PREFIX`].
    pub fn name(&self) -> String {
        format!("{}.{}", self.firing, self.grant_id)
    }

    /// What the refresh token (`kind` `delegation-ciba-refresh`), or its
    /// data key, is sealed under: a record moved to another firing, run,
    /// grant or IdP does not open.
    fn ad(&self, kind: &[u8]) -> Vec<u8> {
        associated_data(&[
            kind,
            self.firing.as_bytes(),
            self.run.as_bytes(),
            self.grant_id.as_bytes(),
            self.idp.as_bytes(),
        ])
    }
}

/// An approval's refresh token as the store keeps it: sealed as a grant's
/// is, under `seal_key` or a data key the signer wrapped.
#[derive(Serialize, Deserialize)]
struct StoredApproval {
    approval: Approval,
    /// base64url of the sealed refresh token.
    sealed: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    dek: Option<Wrapped>,
}

/// What a personal key is sealed or wrapped under.
fn personal_ad(principal: &str, id: &str) -> Vec<u8> {
    associated_data(&[
        b"delegation-personal-key",
        principal.as_bytes(),
        id.as_bytes(),
    ])
}

/// The register's entry of a personal key: forgotten once its revoked
/// epoch is set.
fn personal_floor(id: &str) -> String {
    format!("personal:{id}")
}

/// A record's data key as the signer wrapped it.
#[derive(Serialize, Deserialize)]
struct Wrapped {
    /// The key-encryption key's id at the signer.
    key: String,
    /// base64url of the signer's answer, opaque to agentd.
    wrapped: String,
}

/// Why a grant did not load.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refused {
    /// Its epoch was revoked.
    Revoked,
    /// Older than the latest refresh token stored: enrol again.
    Stale,
    /// It does not open with the seal keys, or the store failed.
    Unreadable(String),
    /// The external signer did not unwrap its data key: nothing was opened.
    SignerUnavailable(String),
}

/// Why an approval's refresh token the store keeps was not opened.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Unopened {
    /// The external signer did not unwrap its data key: it stays, for a
    /// later pass.
    Signer(String),
    /// The store did not answer: it stays, for a later pass.
    Store(String),
    /// It never will — not an approval record, or sealed under keys no
    /// longer held: there is nothing to revoke with.
    Never(String),
}

/// Why a grant was not stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NotStored {
    /// The external signer did not wrap its data key: nothing was written.
    Signer(String),
    /// The store, the register, or a rotation made meanwhile.
    Failed(String),
}

impl std::fmt::Display for NotStored {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            NotStored::Signer(e) | NotStored::Failed(e) => f.write_str(e),
        }
    }
}

/// Who holds the key a grant's refresh token is sealed under.
pub enum Custody {
    /// `seal_key` and `seal_key_previous`: keys the supervisor holds.
    Local {
        key: Box<SealKey>,
        previous: Option<Box<SealKey>>,
    },
    /// The signer's key-encryption key `key` (and `previous`, which only
    /// unwraps): agentd holds neither.
    External {
        signer: Signer,
        key: String,
        previous: Option<String>,
    },
}

impl Custody {
    /// `key` is `seal_key`'s bytes, `previous` `seal_key_previous`'s.
    pub fn local(key: [u8; 32], previous: Option<[u8; 32]>) -> Result<Custody, String> {
        Ok(Custody::Local {
            key: Box::new(
                SealKey::new(&key).map_err(|e| format!("identity.delegation.seal_key: {e}"))?,
            ),
            previous: previous
                .map(|p| SealKey::new(&p).map(Box::new))
                .transpose()
                .map_err(|e| format!("identity.delegation.seal_key_previous: {e}"))?,
        })
    }
}

/// Where grants are kept.
pub struct Vault {
    durable: Arc<Durable>,
    custody: Custody,
    /// The audit chain the register is anchored in, when there is one.
    chain: Option<Arc<Chain>>,
    /// Held while the register is read and written.
    register: Mutex<()>,
    log: Option<crate::obs::log::Logger>,
}

/// A data key drawn and wrapped before the IdP is asked for anything that
/// may rotate the refresh token, for the record that would keep the new
/// one (`rt_version` one up): keeping a rotated token then needs no signer,
/// so a signer that fails after the IdP has spent the old token loses
/// nothing. Under an external signer only; wiped when dropped.
pub struct Ahead {
    epoch: u64,
    rt_version: u64,
    dek: [u8; 32],
    wrapped: Wrapped,
}

impl Ahead {
    /// Whether this is the data key for `next`'s record.
    pub fn is_for(&self, next: &Grant) -> bool {
        self.epoch == next.epoch && self.rt_version == next.rt_version
    }
}

impl Drop for Ahead {
    fn drop(&mut self) {
        crate::sec::harden::wipe(&mut self.dek);
    }
}

fn parts(g: &Grant, kind: &[u8]) -> Vec<u8> {
    associated_data(&[
        kind,
        g.grant_id.as_bytes(),
        g.idp_iss.as_bytes(),
        g.sub.as_bytes(),
        g.epoch.to_string().as_bytes(),
        g.rt_version.to_string().as_bytes(),
    ])
}

/// What the refresh token is sealed under.
fn ad(g: &Grant) -> Vec<u8> {
    parts(g, b"delegation-grant")
}

/// What the data key is wrapped under.
fn wrap_ad(g: &Grant) -> Vec<u8> {
    parts(g, b"delegation-grant-key")
}

/// A signer's answer as a load refusal: a wrapped key that does not open
/// is a record that does not, anything else an outage.
fn unwrap_refused(e: SignerError) -> Refused {
    match &e {
        SignerError::Refused { code, .. } if code == "unwrap_failed" => {
            Refused::Unreadable(e.to_string())
        }
        _ => Refused::SignerUnavailable(e.to_string()),
    }
}

impl Vault {
    /// The vault in `durable`; every floor of the store's register the
    /// `chain` lacks — an anchor that failed before — is anchored now.
    pub fn new(
        durable: Arc<Durable>,
        custody: Custody,
        chain: Option<Arc<Chain>>,
        log: Option<crate::obs::log::Logger>,
    ) -> Vault {
        let v = Vault {
            durable,
            custody,
            chain,
            register: Mutex::new(()),
            log,
        };
        if let Some(c) = &v.chain
            && let Err(e) = v.anchor_lacking(c, None)
        {
            v.unanchored(None, &e);
        }
        v
    }

    /// Whether the external signer holds the key-encryption key: opening a
    /// record then asks it.
    pub fn external(&self) -> bool {
        matches!(self.custody, Custody::External { .. })
    }

    /// Anchor in `c` every floor of the store's register it lacks, and
    /// `this` grant's floor.
    fn anchor_lacking(&self, c: &Chain, this: Option<(&str, Floor)>) -> Result<(), String> {
        let mut want = self.stored_floors();
        if let Some((g, f)) = this {
            let e = want.entry(g.to_string()).or_default();
            e.rt_version = e.rt_version.max(f.rt_version);
            e.revoked_epoch = e.revoked_epoch.max(f.revoked_epoch);
        }
        let have = c.anchors();
        for (g, f) in want {
            let held = have.get(&g).copied().unwrap_or_default();
            let mut raised = held;
            raised.merge(&Anchor {
                revoked_epoch: f.revoked_epoch,
                rt_version: f.rt_version,
            });
            if raised != held {
                c.anchor(&g, raised)
                    .map_err(|e| format!("the tombstone register's anchor: {e}"))?;
            }
        }
        Ok(())
    }

    fn unanchored(&self, grant_id: Option<&str>, err: &str) {
        if let Some(log) = &self.log {
            log.warn(
                "delegation.register.unanchored",
                serde_json::json!({"grant_id": grant_id, "err": err,
                                   "msg": "the store's register is raised; the anchor is tried again at the next raise and the next start"}),
            );
        }
    }

    fn id(grant_id: &str) -> String {
        format!("{GRANT_PREFIX}{grant_id}")
    }

    /// The register as the store holds it.
    fn stored_floors(&self) -> BTreeMap<String, Floor> {
        self.durable
            .get(Kind::Cred, REGISTER)
            .ok()
            .flatten()
            .and_then(|e| serde_json::from_value(e.state).ok())
            .unwrap_or_default()
    }

    /// The register: the store's, raised to what the audit chain anchors.
    pub fn floors(&self) -> BTreeMap<String, Floor> {
        let mut all = self.stored_floors();
        if let Some(c) = &self.chain {
            for (g, a) in c.anchors() {
                let f = all.entry(g).or_default();
                f.rt_version = f.rt_version.max(a.rt_version);
                f.revoked_epoch = f.revoked_epoch.max(a.revoked_epoch);
            }
        }
        all
    }

    /// Raise `grant_id`'s floor: never lowered, anchored in the chain —
    /// with any floor the chain lacks — before the store's register is
    /// written. An anchor that fails is said, and the store's register is
    /// written all the same: only the store failing fails the raise.
    fn raise(&self, grant_id: &str, f: impl FnOnce(&mut Floor)) -> Result<(), String> {
        let _held = self.register.lock().unwrap_or_else(|e| e.into_inner());
        let before = self.floors().get(grant_id).copied().unwrap_or_default();
        let mut after = before;
        f(&mut after);
        after.rt_version = after.rt_version.max(before.rt_version);
        after.revoked_epoch = after.revoked_epoch.max(before.revoked_epoch);
        if let Some(c) = &self.chain
            && let Err(e) = self.anchor_lacking(c, Some((grant_id, after)))
        {
            self.unanchored(Some(grant_id), &e);
        }
        let mut all = self.stored_floors();
        if all.get(grant_id) == Some(&after) {
            return Ok(());
        }
        all.insert(grant_id.to_string(), after);
        let v = serde_json::to_value(&all).map_err(|e| e.to_string())?;
        self.durable
            .put(Kind::Cred, REGISTER, v, None)
            .map(|_| ())
            .map_err(|e| format!("the tombstone register: {e}"))
    }

    /// The record of `grant_id`, held to the register.
    fn stored(&self, grant_id: &str) -> Result<Option<Stored>, Refused> {
        let Some(env) = self
            .durable
            .get(Kind::Cred, &Self::id(grant_id))
            .map_err(|e| Refused::Unreadable(e.to_string()))?
        else {
            return Ok(None);
        };
        let stored: Stored = serde_json::from_value(env.state)
            .map_err(|e| Refused::Unreadable(format!("not a grant record: {e}")))?;
        let floor = self.floors().get(grant_id).copied().unwrap_or_default();
        if floor.revoked_epoch.is_some_and(|r| stored.grant.epoch <= r) {
            return Err(Refused::Revoked);
        }
        if stored.grant.rt_version < floor.rt_version {
            return Err(Refused::Stale);
        }
        Ok(Some(stored))
    }

    /// The grant `grant_id` and its refresh token, held to the register.
    pub fn load(&self, grant_id: &str) -> Result<Option<(Grant, String)>, Refused> {
        let Some(stored) = self.stored(grant_id)? else {
            return Ok(None);
        };
        let rt = self.open(&stored)?;
        Ok(Some((stored.grant, rt)))
    }

    /// The grant `grant_id`, held to the register: under a local key, only
    /// once its refresh token opens; under an external signer, unopened, so
    /// a signer that is down when agentd starts loses no grant — the open
    /// is checked at the grant's first use.
    pub fn meta(&self, grant_id: &str) -> Result<Option<Grant>, Refused> {
        match &self.custody {
            Custody::Local { .. } => self.load(grant_id).map(|g| g.map(|(g, _)| g)),
            Custody::External { .. } => self.stored(grant_id).map(|s| s.map(|s| s.grant)),
        }
    }

    fn open(&self, stored: &Stored) -> Result<String, Refused> {
        let sealed = b64::url_decode(&stored.sealed)
            .map_err(|_| Refused::Unreadable("the sealed refresh token is not base64url".into()))?;
        let aad = ad(&stored.grant);
        let plain = match (&self.custody, &stored.dek) {
            (Custody::Local { key, previous }, None) => key
                .open(&aad, &sealed)
                .or_else(|e| match previous {
                    Some(p) => p.open(&aad, &sealed),
                    None => Err(e),
                })
                .map_err(Refused::Unreadable)?,
            (Custody::Local { .. }, Some(_)) => {
                return Err(Refused::Unreadable(
                    "sealed under an external signer's key, and identity.delegation.key_custody is local: enrol again".into(),
                ));
            }
            (Custody::External { .. }, None) => {
                return Err(Refused::Unreadable(
                    "sealed under a local seal key, and identity.delegation.key_custody is external: enrol again".into(),
                ));
            }
            (
                Custody::External {
                    signer,
                    key,
                    previous,
                },
                Some(w),
            ) => {
                if &w.key != key && previous.as_ref() != Some(&w.key) {
                    return Err(Refused::Unreadable(format!(
                        "its data key is wrapped under {:?}, which is neither identity.delegation.signer.seal_key nor seal_key_previous",
                        w.key
                    )));
                }
                let wrapped = b64::url_decode(&w.wrapped).map_err(|_| {
                    Refused::Unreadable("the wrapped data key is not base64url".into())
                })?;
                let mut dek = signer
                    .unwrap(&w.key, &wrapped, &wrap_ad(&stored.grant))
                    .map_err(unwrap_refused)?;
                let opened = SealKey::new(&dek).and_then(|k| k.open(&aad, &sealed));
                crate::sec::harden::wipe(&mut dek);
                opened.map_err(Refused::Unreadable)?
            }
        };
        String::from_utf8(plain)
            .map_err(|_| Refused::Unreadable("the refresh token is not UTF-8".into()))
    }

    /// A fresh data key for `grant`'s record, wrapped by the signer; `None`
    /// under a local key, which needs none.
    fn draw(&self, grant: &Grant) -> Result<Option<Ahead>, NotStored> {
        let Custody::External { signer, key, .. } = &self.custody else {
            return Ok(None);
        };
        let mut a = Ahead {
            epoch: grant.epoch,
            rt_version: grant.rt_version,
            dek: [0u8; 32],
            wrapped: Wrapped {
                key: key.clone(),
                wrapped: String::new(),
            },
        };
        crate::sec::random::fill(&mut a.dek)
            .map_err(|e| NotStored::Failed(format!("no OS randomness for a data key: {e}")))?;
        let wrapped = signer
            .wrap(key, &a.dek, &wrap_ad(grant))
            .map_err(|e| NotStored::Signer(e.to_string()))?;
        a.wrapped.wrapped = b64::url_nopad(&wrapped);
        Ok(Some(a))
    }

    /// The data key the record after `grant`'s — the one a rotated refresh
    /// token is kept in — will be sealed with, wrapped now ([`Ahead`]).
    pub fn ahead(&self, grant: &Grant) -> Result<Option<Ahead>, NotStored> {
        let mut next = grant.clone();
        next.rt_version += 1;
        self.draw(&next)
    }

    /// `rt` sealed for `grant`, and the wrapped data key it is sealed with
    /// under an external signer: `ahead`'s when it is for this record, else
    /// one drawn now.
    fn seal(
        &self,
        grant: &Grant,
        rt: &str,
        ahead: Option<Ahead>,
    ) -> Result<(Vec<u8>, Option<Wrapped>), NotStored> {
        match &self.custody {
            Custody::Local { key, .. } => key
                .seal(&ad(grant), rt.as_bytes())
                .map(|s| (s, None))
                .map_err(NotStored::Failed),
            Custody::External { key, .. } => {
                let a = match ahead.filter(|a| a.is_for(grant) && &a.wrapped.key == key) {
                    Some(a) => a,
                    None => self.draw(grant)?.ok_or_else(|| {
                        NotStored::Failed("no data key under an external signer".into())
                    })?,
                };
                let sealed = SealKey::new(&a.dek)
                    .and_then(|k| k.seal(&ad(grant), rt.as_bytes()))
                    .map_err(NotStored::Failed)?;
                Ok((
                    sealed,
                    Some(Wrapped {
                        key: a.wrapped.key.clone(),
                        wrapped: a.wrapped.wrapped.clone(),
                    }),
                ))
            }
        }
    }

    /// Write `grant` with `rt`, over the record of `rt_version` `over` (`None`:
    /// a new enrolment, over whatever is there). Refused when the stored
    /// record's version is not `over`: someone else rotated first.
    pub fn store(&self, grant: &Grant, rt: &str, over: Option<u64>) -> Result<(), NotStored> {
        self.store_ahead(grant, rt, over, None)
    }

    /// [`Self::store`], sealed with the data key `ahead` drew for this
    /// record, when it is for it: no signer is asked.
    pub fn store_ahead(
        &self,
        grant: &Grant,
        rt: &str,
        over: Option<u64>,
        ahead: Option<Ahead>,
    ) -> Result<(), NotStored> {
        if let Some(want) = over {
            let now = self
                .durable
                .get(Kind::Cred, &Self::id(&grant.grant_id))
                .map_err(|e| NotStored::Failed(e.to_string()))?
                .and_then(|e| serde_json::from_value::<Stored>(e.state).ok())
                .map(|s| s.grant.rt_version);
            if now != Some(want) {
                return Err(NotStored::Failed(format!(
                    "grant {}: its refresh token was rotated meanwhile (rt_version {now:?}, not {want})",
                    grant.grant_id
                )));
            }
        }
        let (sealed, dek) = self.seal(grant, rt, ahead)?;
        let v = serde_json::to_value(Stored {
            grant: grant.clone(),
            sealed: b64::url_nopad(&sealed),
            dek,
        })
        .map_err(|e| NotStored::Failed(e.to_string()))?;
        self.durable
            .put(Kind::Cred, &Self::id(&grant.grant_id), v, None)
            .map_err(|e| NotStored::Failed(format!("grant {}: {e}", grant.grant_id)))?;
        let version = grant.rt_version;
        self.raise(&grant.grant_id, |f| f.rt_version = version)
            .map_err(NotStored::Failed)
    }

    /// Revoke `grant_id` at `epoch`: the register first, then the record —
    /// deleted even when the register could not be raised, since a revoked
    /// record left in place is a grant the next start loads.
    pub fn tombstone(&self, grant_id: &str, epoch: u64) -> Result<(), String> {
        let raised = self.raise(grant_id, |f| f.revoked_epoch = Some(epoch));
        let deleted = self
            .durable
            .delete(Kind::Cred, &Self::id(grant_id))
            .map_err(|e| format!("grant {grant_id}: {e}"));
        raised.and(deleted)
    }

    /// Where `principal`'s personal key is kept.
    fn personal_id(principal: &str) -> String {
        format!(
            "{PERSONAL_PREFIX}{}",
            crate::sha::sha256_hex(principal.as_bytes())
        )
    }

    /// `principal`'s personal key (RFC 0044 §6.4): the one kept, opened —
    /// or, with `create`, a new one made and kept when there is none. Kept
    /// sealed as a grant's refresh token is: under `seal_key`, or wrapped by
    /// the external signer, bound to the principal and the key's id. A key
    /// the register says was forgotten never opens again, even from a store
    /// restored from before it was.
    pub fn personal_key(
        &self,
        principal: &str,
        create: bool,
    ) -> Result<Option<(String, [u8; 32])>, Refused> {
        let at = Self::personal_id(principal);
        let stored = self
            .durable
            .get(Kind::Cred, &at)
            .map_err(|e| Refused::Unreadable(e.to_string()))?
            .and_then(|e| serde_json::from_value::<StoredPersonal>(e.state).ok())
            .filter(|s| s.principal == principal);
        let Some(s) = stored else {
            if !create {
                return Ok(None);
            }
            let mut key = [0u8; 32];
            crate::sec::random::fill(&mut key)
                .map_err(|e| Refused::Unreadable(format!("no OS randomness for a key: {e}")))?;
            let id = crate::sec::random::b64url_token(12)
                .map_err(|e| Refused::Unreadable(format!("no OS randomness for a key id: {e}")))?;
            let ad = personal_ad(principal, &id);
            let (sealed, wrapped) = match &self.custody {
                Custody::Local { key: k, .. } => (
                    Some(b64::url_nopad(
                        &k.seal(&ad, &key).map_err(Refused::Unreadable)?,
                    )),
                    None,
                ),
                Custody::External { signer, key: k, .. } => {
                    let w = signer.wrap(k, &key, &ad).map_err(|e| match e {
                        SignerError::Unavailable(_) => Refused::SignerUnavailable(e.to_string()),
                        e => Refused::Unreadable(e.to_string()),
                    })?;
                    (
                        None,
                        Some(Wrapped {
                            key: k.clone(),
                            wrapped: b64::url_nopad(&w),
                        }),
                    )
                }
            };
            let v = serde_json::to_value(StoredPersonal {
                principal: principal.to_string(),
                id: id.clone(),
                sealed,
                wrapped,
            })
            .map_err(|e| Refused::Unreadable(e.to_string()))?;
            let put = self.durable.put(Kind::Cred, &at, v, None);
            if let Err(e) = put {
                crate::sec::harden::wipe(&mut key);
                return Err(Refused::Unreadable(format!(
                    "{principal}'s personal key: {e}"
                )));
            }
            return Ok(Some((id, key)));
        };
        if self
            .floors()
            .get(&personal_floor(&s.id))
            .is_some_and(|f| f.revoked_epoch.is_some())
        {
            return Err(Refused::Revoked);
        }
        let ad = personal_ad(principal, &s.id);
        let mut raw = match (&self.custody, &s.sealed, &s.wrapped) {
            (Custody::Local { key, previous }, Some(sealed), None) => {
                let b = b64::url_decode(sealed)
                    .map_err(|_| Refused::Unreadable("a personal key is not base64url".into()))?;
                key.open(&ad, &b)
                    .or_else(|e| match previous {
                        Some(p) => p.open(&ad, &b),
                        None => Err(e),
                    })
                    .map_err(Refused::Unreadable)?
            }
            (
                Custody::External {
                    signer,
                    key,
                    previous,
                },
                None,
                Some(w),
            ) if &w.key == key || previous.as_ref() == Some(&w.key) => {
                let b = b64::url_decode(&w.wrapped).map_err(|_| {
                    Refused::Unreadable("a wrapped personal key is not base64url".into())
                })?;
                signer.unwrap(&w.key, &b, &ad).map_err(unwrap_refused)?
            }
            _ => {
                return Err(Refused::Unreadable(
                    "a personal key kept under another custody or key-encryption key".into(),
                ));
            }
        };
        let key = <[u8; 32]>::try_from(raw.as_slice())
            .map_err(|_| Refused::Unreadable("a personal key is not 32 bytes".into()));
        crate::sec::harden::wipe(&mut raw);
        Ok(Some((s.id, key?)))
    }

    /// Destroy `principal`'s personal key (crypto-shredding, RFC 0044 §6.4):
    /// its id is raised in the register as forgotten — anchored in the audit
    /// chain first, as a revocation is — and the record deleted, so what it
    /// sealed is unreadable from here on and a store restored from before
    /// still opens none of it. The id forgotten, or `None` when there was no
    /// key.
    pub fn forget_personal(&self, principal: &str) -> Result<Option<String>, String> {
        let at = Self::personal_id(principal);
        let Some(s) = self
            .durable
            .get(Kind::Cred, &at)
            .map_err(|e| e.to_string())?
            .and_then(|e| serde_json::from_value::<StoredPersonal>(e.state).ok())
        else {
            return Ok(None);
        };
        let raised = self.raise(&personal_floor(&s.id), |f| f.revoked_epoch = Some(1));
        let deleted = self
            .durable
            .delete(Kind::Cred, &at)
            .map_err(|e| format!("{principal}'s personal key: {e}"));
        raised.and(deleted).map(|()| Some(s.id))
    }

    /// Every grant's id.
    pub fn ids(&self) -> Vec<String> {
        self.durable
            .list_prefix(Kind::Cred, GRANT_PREFIX)
            .unwrap_or_default()
            .into_iter()
            .filter_map(|k| k.key.rsplit('/').next().map(str::to_string))
            .filter(|k| !k.is_empty())
            .collect()
    }

    /// Keep `rt`, the refresh token an IdP issued with approval `a`, sealed
    /// until [`Self::forget_approval`] (RFC 0044 §7.P3c): under a local key,
    /// or under a fresh data key the external signer wraps now — a signer
    /// that wraps none leaves nothing written.
    pub fn keep_approval(&self, a: &Approval, rt: &str) -> Result<(), NotStored> {
        let ad = a.ad(b"delegation-ciba-refresh");
        let (sealed, dek) = match &self.custody {
            Custody::Local { key, .. } => (
                key.seal(&ad, rt.as_bytes()).map_err(NotStored::Failed)?,
                None,
            ),
            Custody::External { signer, key, .. } => {
                let mut dek = [0u8; 32];
                crate::sec::random::fill(&mut dek).map_err(|e| {
                    NotStored::Failed(format!("no OS randomness for a data key: {e}"))
                })?;
                let out = signer
                    .wrap(key, &dek, &a.ad(b"delegation-ciba-refresh-key"))
                    .map_err(|e| NotStored::Signer(e.to_string()))
                    .and_then(|w| {
                        SealKey::new(&dek)
                            .and_then(|k| k.seal(&ad, rt.as_bytes()))
                            .map(|s| (s, w))
                            .map_err(NotStored::Failed)
                    });
                crate::sec::harden::wipe(&mut dek);
                let (sealed, wrapped) = out?;
                (
                    sealed,
                    Some(Wrapped {
                        key: key.clone(),
                        wrapped: b64::url_nopad(&wrapped),
                    }),
                )
            }
        };
        let v = serde_json::to_value(StoredApproval {
            approval: a.clone(),
            sealed: b64::url_nopad(&sealed),
            dek,
        })
        .map_err(|e| NotStored::Failed(e.to_string()))?;
        self.durable
            .put(
                Kind::Cred,
                &format!("{APPROVAL_PREFIX}{}", a.name()),
                v,
                None,
            )
            .map(|_| ())
            .map_err(|e| NotStored::Failed(format!("approval {}: {e}", a.name())))
    }

    /// The names of every approval's refresh token kept; `Err` when the
    /// store failed to list them. A store that cannot list
    /// (`Unsupported`) finds none, as it finds no grant at a start.
    pub fn approvals(&self) -> Result<Vec<String>, String> {
        let listed = match self.durable.list_prefix(Kind::Cred, APPROVAL_PREFIX) {
            Ok(l) => l,
            Err(StoreError::Unsupported(_)) => Vec::new(),
            Err(e) => return Err(format!("the approvals kept: {e}")),
        };
        Ok(listed
            .into_iter()
            .filter_map(|k| k.key.rsplit('/').next().map(str::to_string))
            .filter(|k| !k.is_empty())
            .collect())
    }

    /// The approval kept as `name`, and its refresh token, opened.
    pub fn open_approval(&self, name: &str) -> Result<Option<(Approval, String)>, Unopened> {
        let Some(env) = self
            .durable
            .get(Kind::Cred, &format!("{APPROVAL_PREFIX}{name}"))
            .map_err(|e| match e {
                StoreError::Corrupt(e) => Unopened::Never(e),
                e => Unopened::Store(format!("approval {name}: {e}")),
            })?
        else {
            return Ok(None);
        };
        let s: StoredApproval = serde_json::from_value(env.state)
            .map_err(|e| Unopened::Never(format!("not an approval record: {e}")))?;
        let sealed = b64::url_decode(&s.sealed)
            .map_err(|_| Unopened::Never("the sealed refresh token is not base64url".into()))?;
        let ad = s.approval.ad(b"delegation-ciba-refresh");
        let plain = match (&self.custody, &s.dek) {
            (Custody::Local { key, previous }, None) => key
                .open(&ad, &sealed)
                .or_else(|e| match previous {
                    Some(p) => p.open(&ad, &sealed),
                    None => Err(e),
                })
                .map_err(Unopened::Never)?,
            (
                Custody::External {
                    signer,
                    key,
                    previous,
                },
                Some(w),
            ) if &w.key == key || previous.as_ref() == Some(&w.key) => {
                let wrapped = b64::url_decode(&w.wrapped)
                    .map_err(|_| Unopened::Never("the wrapped data key is not base64url".into()))?;
                let mut dek = signer
                    .unwrap(
                        &w.key,
                        &wrapped,
                        &s.approval.ad(b"delegation-ciba-refresh-key"),
                    )
                    .map_err(|e| match &e {
                        SignerError::Refused { code, .. } if code == "unwrap_failed" => {
                            Unopened::Never(e.to_string())
                        }
                        _ => Unopened::Signer(e.to_string()),
                    })?;
                let opened = SealKey::new(&dek).and_then(|k| k.open(&ad, &sealed));
                crate::sec::harden::wipe(&mut dek);
                opened.map_err(Unopened::Never)?
            }
            _ => {
                return Err(Unopened::Never(
                    "sealed under another custody or key-encryption key".into(),
                ));
            }
        };
        let rt = String::from_utf8(plain)
            .map_err(|_| Unopened::Never("the refresh token is not UTF-8".into()))?;
        Ok(Some((s.approval, rt)))
    }

    /// Forget the approval's refresh token kept as `name`: revoked, or its
    /// run ended.
    pub fn forget_approval(&self, name: &str) -> Result<(), String> {
        self.durable
            .delete(Kind::Cred, &format!("{APPROVAL_PREFIX}{name}"))
            .map_err(|e| format!("approval {name}: {e}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::a2a::principals::typed::{PrincipalRef, Source};
    use crate::store::Store;
    use crate::store::memory::MemoryStore;

    fn durable(store: &Arc<MemoryStore>) -> Durable {
        Durable::new(
            Arc::clone(store) as Arc<dyn Store>,
            "t/",
            "t",
            crate::state::Policy::default(),
            None,
        )
    }

    fn vault(store: &Arc<MemoryStore>, key: u8, previous: Option<u8>) -> Vault {
        Vault::new(
            Arc::new(durable(store)),
            Custody::local([key; 32], previous.map(|p| [p; 32])).unwrap(),
            None,
            None,
        )
    }

    fn uid() -> u32 {
        // SAFETY: getuid has no failure mode.
        unsafe { libc::getuid() }
    }

    /// A vault whose key the signer at `sock` holds.
    fn external(store: &Arc<MemoryStore>, sock: &std::path::Path) -> Vault {
        Vault::new(
            Arc::new(durable(store)),
            Custody::External {
                signer: Signer::new(sock, uid(), Some(std::time::Duration::from_millis(500))),
                key: "seal".into(),
                previous: None,
            },
            None,
            None,
        )
    }

    /// The stored envelope of `id`.
    fn raw(store: &Arc<MemoryStore>, id: &str) -> serde_json::Value {
        let key = durable(store).key(Kind::Cred, id);
        store.get(&key, None).unwrap().unwrap()
    }

    fn grant() -> Grant {
        let p = PrincipalRef::local(Source::Operator, "alice");
        Grant {
            grant_id: p.grant_id("https://acme.okta.com"),
            principal: p,
            fingerprint: "unix-uid:1000".into(),
            idp: "okta".into(),
            idp_iss: "https://acme.okta.com".into(),
            sub: "00u1alice".into(),
            email_hash: None,
            template: "eng".into(),
            ceilings: [("linear".to_string(), vec!["issues:read".to_string()])].into(),
            created_ms: 1,
            expires_ms: u64::MAX,
            epoch: 1,
            rt_version: 1,
            quota: Default::default(),
            bound_by: None,
            unattended: Default::default(),
            ciba: None,
        }
    }

    /// The refresh token is sealed: the store holds no byte of it, and the
    /// record opens only as itself.
    #[test]
    fn a_grant_is_sealed_and_bound_to_its_epoch_and_version() {
        let store = Arc::new(MemoryStore::new());
        let v = vault(&store, 7, None);
        let g = grant();
        v.store(&g, "rt-secret-1", None).unwrap();
        let (back, rt) = v.load(&g.grant_id).unwrap().unwrap();
        assert_eq!((back, rt.as_str()), (g.clone(), "rt-secret-1"));
        let raw = raw(&store, &format!("{GRANT_PREFIX}{}", g.grant_id)).to_string();
        assert!(!raw.contains("rt-secret-1"), "{raw}");
        // Another key does not open it; the previous key does, after a
        // rotation of the seal key.
        assert!(matches!(
            vault(&store, 8, None).load(&g.grant_id),
            Err(Refused::Unreadable(_))
        ));
        assert_eq!(
            vault(&store, 8, Some(7))
                .load(&g.grant_id)
                .unwrap()
                .unwrap()
                .1,
            "rt-secret-1"
        );
        // A record whose metadata was edited does not open: the associated
        // data names the epoch.
        let mut stored: serde_json::Value = serde_json::from_str(&raw).unwrap();
        stored["state"]["grant"]["epoch"] = serde_json::json!(9);
        durable(&store)
            .put(
                Kind::Cred,
                &format!("{GRANT_PREFIX}{}", g.grant_id),
                stored["state"].clone(),
                None,
            )
            .unwrap();
        assert!(matches!(v.load(&g.grant_id), Err(Refused::Unreadable(_))));
    }

    /// A rotation is written only over the version it read; an older record
    /// restored over a newer one is refused, and a revoked epoch never loads
    /// again.
    #[test]
    fn the_register_refuses_a_revoked_epoch_and_a_rotated_out_token() {
        let store = Arc::new(MemoryStore::new());
        let v = vault(&store, 7, None);
        let mut g = grant();
        let id = format!("{GRANT_PREFIX}{}", g.grant_id);
        v.store(&g, "rt-1", None).unwrap();
        let old = raw(&store, &id);
        g.rt_version = 2;
        assert!(v.store(&g, "rt-2", Some(5)).is_err(), "not over version 5");
        v.store(&g, "rt-2", Some(1)).unwrap();
        assert_eq!(v.load(&g.grant_id).unwrap().unwrap().1, "rt-2");
        // A backup from before the rotation, restored beside the register.
        let restored = Arc::new(MemoryStore::new());
        let d = durable(&restored);
        restored.put(&d.key(Kind::Cred, &id), 1, &old).unwrap();
        restored
            .put(&d.key(Kind::Cred, REGISTER), 1, &raw(&store, REGISTER))
            .unwrap();
        assert_eq!(
            vault(&restored, 7, None).load(&g.grant_id),
            Err(Refused::Stale)
        );
        let fresh = vault(&store, 7, None);
        // Revoked: gone, and the epoch stays refused.
        fresh.store(&g, "rt-2", None).unwrap();
        fresh.tombstone(&g.grant_id, g.epoch).unwrap();
        assert_eq!(fresh.load(&g.grant_id), Ok(None));
        fresh.store(&g, "rt-2", None).unwrap();
        assert_eq!(fresh.load(&g.grant_id), Err(Refused::Revoked));
        g.epoch = 2;
        fresh.store(&g, "rt-3", None).unwrap();
        assert_eq!(fresh.load(&g.grant_id).unwrap().unwrap().1, "rt-3");
        assert_eq!(fresh.ids(), vec![g.grant_id.clone()]);
    }

    #[test]
    fn days_are_utc_calendar_days() {
        assert_eq!(days(0), ("1970-01-01".into(), "1970-01".into()));
        // 2026-10-19T12:00:00Z
        assert_eq!(
            days(1_792_411_200_000),
            ("2026-10-19".into(), "2026-10".into())
        );
        let q = Quota {
            day: "2026-10-18".into(),
            day_count: 9,
            month: "2026-10".into(),
            month_count: 40,
        };
        let now = q.at(1_792_411_200_000);
        assert_eq!((now.day_count, now.month_count), (0, 40));
    }

    /// Under an external signer the vault holds no key: a record is sealed
    /// under a data key of its own, kept only as the signer wrapped it; it
    /// opens through the signer, not without it, and not under a local key.
    #[test]
    fn a_grant_sealed_through_the_signer_opens_only_through_it() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("signer.sock");
        let _signer =
            crate::sec::signer::mock::serve(&sock, &crate::sec::signer::mock::default_knobs())
                .unwrap();
        let store = Arc::new(MemoryStore::new());
        let v = external(&store, &sock);
        let g = grant();
        v.store(&g, "rt-secret-ext", None).unwrap();
        assert_eq!(v.load(&g.grant_id).unwrap().unwrap().1, "rt-secret-ext");
        let id = format!("{GRANT_PREFIX}{}", g.grant_id);
        let record = raw(&store, &id);
        let text = record.to_string();
        assert!(!text.contains("rt-secret-ext"), "{text}");
        assert_eq!(record["state"]["dek"]["key"], "seal");
        assert!(record["state"]["dek"]["wrapped"].is_string(), "{text}");
        // What the store holds is no key: no local seal key opens it, and
        // its wrapped data key is not one either.
        assert!(matches!(
            vault(&store, 7, None).load(&g.grant_id),
            Err(Refused::Unreadable(_))
        ));
        let wrapped = b64::url_decode(record["state"]["dek"]["wrapped"].as_str().unwrap()).unwrap();
        let sealed = b64::url_decode(record["state"]["sealed"].as_str().unwrap()).unwrap();
        for w in wrapped.windows(32) {
            let k = SealKey::new(w).unwrap();
            assert!(k.open(&ad(&g), &sealed).is_err());
        }
        // A data key moved to another grant's record does not unwrap there.
        let mut other = g.clone();
        other.epoch = 2;
        let mut moved = record["state"].clone();
        moved["grant"] = serde_json::to_value(&other).unwrap();
        durable(&store).put(Kind::Cred, &id, moved, None).unwrap();
        assert!(matches!(v.load(&g.grant_id), Err(Refused::Unreadable(_))));
        // A record sealed under a local key does not open under the signer.
        let local = Arc::new(MemoryStore::new());
        vault(&local, 7, None).store(&g, "rt-local", None).unwrap();
        assert!(matches!(
            external(&local, &sock).load(&g.grant_id),
            Err(Refused::Unreadable(_))
        ));
    }

    /// With the signer down nothing is opened and nothing is written: the
    /// refusal is typed, and the grant's metadata still loads.
    #[test]
    fn a_signer_that_is_down_opens_and_writes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("signer.sock");
        let store = Arc::new(MemoryStore::new());
        let g = grant();
        {
            let _signer =
                crate::sec::signer::mock::serve(&sock, &crate::sec::signer::mock::default_knobs())
                    .unwrap();
            external(&store, &sock).store(&g, "rt-1", None).unwrap();
        }
        let gone = dir.path().join("gone.sock");
        let v = external(&store, &gone);
        assert!(matches!(
            v.load(&g.grant_id),
            Err(Refused::SignerUnavailable(_))
        ));
        assert_eq!(v.meta(&g.grant_id).unwrap(), Some(g.clone()));
        let fresh = Arc::new(MemoryStore::new());
        assert!(matches!(
            external(&fresh, &gone).store(&g, "rt-2", None),
            Err(NotStored::Signer(_))
        ));
        assert!(
            durable(&fresh)
                .get(Kind::Cred, &format!("{GRANT_PREFIX}{}", g.grant_id))
                .unwrap()
                .is_none(),
            "nothing written"
        );
    }

    /// A principal's personal key (§6.4) is made once, kept sealed — never
    /// in the clear — and the same key opens in every later life; forgotten,
    /// it is gone, and a store restored from before it was forgotten opens
    /// none of it again through the register the audit chain anchors. Under
    /// the external signer it is kept wrapped, and opens only through it.
    #[test]
    fn a_personal_key_is_kept_sealed_until_it_is_forgotten() {
        let dir = tempfile::tempdir().unwrap();
        let (chain, _) = Chain::open(
            crate::obs::chain::Options {
                path: dir.path().join("audit.log"),
                rotate_at: crate::obs::chain::DEFAULT_ROTATE_AT,
                retention: crate::obs::chain::DEFAULT_RETENTION,
            },
            None,
            None,
        )
        .unwrap();
        let chain = Arc::new(chain);
        let anchored = |store: &Arc<MemoryStore>| {
            Vault::new(
                Arc::new(durable(store)),
                Custody::local([7; 32], None).unwrap(),
                Some(Arc::clone(&chain)),
                None,
            )
        };
        let store = Arc::new(MemoryStore::new());
        let who = "operator:alice";
        assert_eq!(anchored(&store).personal_key(who, false), Ok(None));
        let (id, key) = anchored(&store).personal_key(who, true).unwrap().unwrap();
        assert_eq!(
            anchored(&store).personal_key(who, true).unwrap(),
            Some((id.clone(), key)),
            "the same key in a later life"
        );
        let at = Vault::personal_id(who);
        let kept = raw(&store, &at);
        assert!(kept["state"]["sealed"].is_string(), "{kept}");
        let text = serde_json::to_vec(&kept).unwrap();
        assert!(!text.windows(32).any(|w| w == key), "kept sealed only");
        // Another principal's key is another key.
        let (other, _) = anchored(&store)
            .personal_key("operator:bob", true)
            .unwrap()
            .unwrap();
        assert_ne!(other, id);
        // Forgotten: the record is gone and a new life finds no key.
        let backup = kept.clone();
        assert_eq!(anchored(&store).forget_personal(who), Ok(Some(id.clone())));
        assert_eq!(anchored(&store).personal_key(who, false), Ok(None));
        // Restored from before: refused, by the register the chain holds.
        let restored = Arc::new(MemoryStore::new());
        let d = durable(&restored);
        restored.put(&d.key(Kind::Cred, &at), 1, &backup).unwrap();
        assert_eq!(
            anchored(&restored).personal_key(who, false),
            Err(Refused::Revoked)
        );
        assert!(chain.anchors().contains_key(&personal_floor(&id)));

        // Under the external signer: wrapped, and only it opens the key.
        let sock = dir.path().join("signer.sock");
        let _signer =
            crate::sec::signer::mock::serve(&sock, &crate::sec::signer::mock::default_knobs())
                .unwrap();
        let ext = Arc::new(MemoryStore::new());
        let (eid, ekey) = external(&ext, &sock)
            .personal_key(who, true)
            .unwrap()
            .unwrap();
        let wrapped = raw(&ext, &at);
        assert!(
            wrapped["state"]["wrapped"]["wrapped"].is_string(),
            "{wrapped}"
        );
        assert!(wrapped["state"]["sealed"].is_null());
        assert_eq!(
            external(&ext, &sock).personal_key(who, false).unwrap(),
            Some((eid, ekey))
        );
        assert!(matches!(
            vault(&ext, 7, None).personal_key(who, false),
            Err(Refused::Unreadable(_))
        ));
        assert!(matches!(
            external(&ext, &dir.path().join("gone.sock")).personal_key(who, false),
            Err(Refused::SignerUnavailable(_))
        ));
    }

    /// A store restored from before a revocation — the grant and the
    /// register both — is refused through the register anchored in the
    /// audit chain; without the chain, it is not (the P1 residual).
    #[test]
    fn a_restored_pre_revocation_blob_is_refused_through_the_anchored_register() {
        let dir = tempfile::tempdir().unwrap();
        let (chain, _) = Chain::open(
            crate::obs::chain::Options {
                path: dir.path().join("audit.log"),
                rotate_at: crate::obs::chain::DEFAULT_ROTATE_AT,
                retention: crate::obs::chain::DEFAULT_RETENTION,
            },
            None,
            None,
        )
        .unwrap();
        let chain = Arc::new(chain);
        let anchored = |store: &Arc<MemoryStore>| {
            Vault::new(
                Arc::new(durable(store)),
                Custody::local([7; 32], None).unwrap(),
                Some(Arc::clone(&chain)),
                None,
            )
        };
        let store = Arc::new(MemoryStore::new());
        let mut g = grant();
        let id = format!("{GRANT_PREFIX}{}", g.grant_id);
        anchored(&store).store(&g, "rt-1", None).unwrap();
        let backup = (raw(&store, &id), raw(&store, REGISTER));
        let restore = |(grant, register): &(serde_json::Value, serde_json::Value)| {
            let s = Arc::new(MemoryStore::new());
            let d = durable(&s);
            s.put(&d.key(Kind::Cred, &id), 1, grant).unwrap();
            s.put(&d.key(Kind::Cred, REGISTER), 1, register).unwrap();
            s
        };
        // A rotation, then the backup from before it.
        g.rt_version = 2;
        anchored(&store).store(&g, "rt-2", Some(1)).unwrap();
        assert_eq!(
            anchored(&restore(&backup)).load(&g.grant_id),
            Err(Refused::Stale)
        );
        // The revocation, then the backup from before it.
        anchored(&store).tombstone(&g.grant_id, g.epoch).unwrap();
        let before_revocation = (
            {
                let s = Arc::new(MemoryStore::new());
                let v = vault(&s, 7, None);
                v.store(&g, "rt-2", None).unwrap();
                raw(&s, &id)
            },
            backup.1.clone(),
        );
        assert_eq!(
            anchored(&restore(&before_revocation)).load(&g.grant_id),
            Err(Refused::Revoked)
        );
        // The same restore with no chain loads: what P2a's anchor closes.
        assert!(matches!(
            vault(&restore(&before_revocation), 7, None).load(&g.grant_id),
            Ok(Some(_))
        ));
        // The anchor is in the chain file itself.
        let text = std::fs::read_to_string(dir.path().join("audit.log")).unwrap();
        assert!(text.contains(crate::obs::chain::REGISTER), "{text}");
        assert!(text.contains("\"revoked_epoch\":1"), "{text}");
    }

    /// A revocation whose anchor the chain does not take still raises the
    /// store's register and deletes the record; the floor the chain lacks
    /// is anchored at the next start, and then refuses a restored blob.
    #[test]
    fn a_revocation_the_chain_does_not_take_still_deletes_and_is_anchored_later() {
        let dir = tempfile::tempdir().unwrap();
        let opts = crate::obs::chain::Options {
            path: dir.path().join("audit.log"),
            rotate_at: crate::obs::chain::DEFAULT_ROTATE_AT,
            retention: crate::obs::chain::DEFAULT_RETENTION,
        };
        let chain = Arc::new(Chain::open(opts.clone(), None, None).unwrap().0);
        let anchored = |store: &Arc<MemoryStore>, chain: &Arc<Chain>| {
            Vault::new(
                Arc::new(durable(store)),
                Custody::local([7; 32], None).unwrap(),
                Some(Arc::clone(chain)),
                None,
            )
        };
        let store = Arc::new(MemoryStore::new());
        let g = grant();
        let id = format!("{GRANT_PREFIX}{}", g.grant_id);
        let v = anchored(&store, &chain);
        v.store(&g, "rt-1", None).unwrap();
        let backup = (raw(&store, &id), raw(&store, REGISTER));
        chain.refuse_writes();
        v.tombstone(&g.grant_id, g.epoch).unwrap();
        assert_eq!(v.load(&g.grant_id), Ok(None), "the record is deleted");
        assert_eq!(
            v.stored_floors()[&g.grant_id].revoked_epoch,
            Some(g.epoch),
            "the store's register is raised"
        );
        assert_eq!(chain.anchors()[&g.grant_id].revoked_epoch, None);
        drop((v, chain));
        // The next start: the chain verified again, the floor it lacks
        // anchored from the store's register.
        let chain = Arc::new(Chain::open(opts, None, None).unwrap().0);
        anchored(&store, &chain);
        assert_eq!(
            chain.anchors()[&g.grant_id].revoked_epoch,
            Some(g.epoch),
            "anchored at the start"
        );
        let restored = Arc::new(MemoryStore::new());
        let d = durable(&restored);
        restored.put(&d.key(Kind::Cred, &id), 1, &backup.0).unwrap();
        restored
            .put(&d.key(Kind::Cred, REGISTER), 1, &backup.1)
            .unwrap();
        assert_eq!(
            anchored(&restored, &chain).load(&g.grant_id),
            Err(Refused::Revoked)
        );
    }

    /// The data key a rotated refresh token is kept with is wrapped before
    /// the IdP is asked: with the signer gone by the time the IdP has
    /// rotated the token, the new token is still kept, and opens.
    #[test]
    fn a_rotated_token_is_kept_with_a_data_key_wrapped_ahead() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("signer.sock");
        let mut knobs = crate::sec::signer::mock::default_knobs();
        knobs["keys"]["seal"] = serde_json::json!({"use": "wrap", "hex": "ab".repeat(32)});
        let _signer = crate::sec::signer::mock::serve(&sock, &knobs).unwrap();
        let store = Arc::new(MemoryStore::new());
        let up = external(&store, &sock);
        let mut g = grant();
        up.store(&g, "rt-1", None).unwrap();
        let ahead = up.ahead(&g).unwrap();
        assert!(ahead.is_some(), "an external signer's key");
        // The signer is down by the time the rotated token comes back.
        let down = external(&store, &dir.path().join("gone.sock"));
        g.rt_version = 2;
        assert!(matches!(
            down.store(&g, "rt-2", Some(1)),
            Err(NotStored::Signer(_))
        ));
        down.store_ahead(&g, "rt-2", Some(1), ahead).unwrap();
        assert_eq!(up.load(&g.grant_id).unwrap().unwrap().1, "rt-2");
        // A key drawn for another record is not used for this one.
        let stale = up.ahead(&grant()).unwrap();
        g.rt_version = 3;
        assert!(matches!(
            down.store_ahead(&g, "rt-3", Some(2), stale),
            Err(NotStored::Signer(_))
        ));
        // Under a local key nothing is drawn.
        assert!(vault(&store, 7, None).ahead(&g).unwrap().is_none());
    }

    /// The refresh token an IdP issued with a CIBA approval is kept sealed
    /// — never a byte of it in the clear, under a local key or a data key
    /// the signer wraps — listed by name until it is forgotten; a record
    /// moved to another firing, run, grant or IdP does not open, nor one
    /// kept under the other custody, and a signer that wraps nothing
    /// writes nothing (RFC 0044 §7.P3c).
    #[test]
    fn an_approvals_refresh_token_is_kept_sealed_until_it_is_forgotten() {
        let a = Approval {
            firing: "ua_f1".into(),
            run: "r1".into(),
            grant_id: grant().grant_id,
            idp: "okta".into(),
        };
        let store = Arc::new(MemoryStore::new());
        let v = vault(&store, 7, None);
        v.keep_approval(&a, "ciba-rt-secret").unwrap();
        assert_eq!(v.approvals(), Ok(vec![a.name()]));
        let id = format!("{APPROVAL_PREFIX}{}", a.name());
        let record = raw(&store, &id);
        assert!(!record.to_string().contains("ciba-rt-secret"), "{record}");
        assert_eq!(
            v.open_approval(&a.name()).unwrap(),
            Some((a.clone(), "ciba-rt-secret".to_string()))
        );
        assert!(matches!(
            vault(&store, 8, None).open_approval(&a.name()),
            Err(Unopened::Never(_))
        ));
        // Revoked at another IdP: the record names the one it was kept for.
        let mut moved = record["state"].clone();
        moved["approval"]["idp"] = serde_json::json!("elsewhere");
        durable(&store).put(Kind::Cred, &id, moved, None).unwrap();
        assert!(matches!(
            v.open_approval(&a.name()),
            Err(Unopened::Never(_))
        ));
        v.forget_approval(&a.name()).unwrap();
        assert_eq!(v.approvals(), Ok(vec![]));
        assert_eq!(v.open_approval(&a.name()), Ok(None));

        // Under the external signer: wrapped, and only it opens the token.
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("signer.sock");
        let _signer =
            crate::sec::signer::mock::serve(&sock, &crate::sec::signer::mock::default_knobs())
                .unwrap();
        let ext = Arc::new(MemoryStore::new());
        external(&ext, &sock)
            .keep_approval(&a, "ciba-rt-ext")
            .unwrap();
        let record = raw(&ext, &id);
        assert!(!record.to_string().contains("ciba-rt-ext"), "{record}");
        assert_eq!(record["state"]["dek"]["key"], "seal");
        assert_eq!(
            external(&ext, &sock).open_approval(&a.name()).unwrap(),
            Some((a.clone(), "ciba-rt-ext".to_string()))
        );
        assert!(matches!(
            vault(&ext, 7, None).open_approval(&a.name()),
            Err(Unopened::Never(_))
        ));
        let gone = dir.path().join("gone.sock");
        assert!(matches!(
            external(&ext, &gone).open_approval(&a.name()),
            Err(Unopened::Signer(_))
        ));
        let fresh = Arc::new(MemoryStore::new());
        assert!(matches!(
            external(&fresh, &gone).keep_approval(&a, "ciba-rt-2"),
            Err(NotStored::Signer(_))
        ));
        assert_eq!(
            external(&fresh, &gone).approvals(),
            Ok(vec![]),
            "nothing written"
        );
    }
}
