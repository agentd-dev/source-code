// SPDX-License-Identifier: AGPL-3.0-only
//! The grant vault (RFC 0044 §5.5): a person's delegation grant, the one
//! durable user credential in the design, kept as a [`Kind::Cred`] record
//! under its `grant_id` — its metadata in the clear, its refresh token
//! sealed with AES-256-GCM under `identity.delegation.seal_key`, the
//! associated data `("delegation-grant", grant_id, idp_iss, sub, epoch,
//! rt_version)` — so a sealed blob does not open under another grant, epoch
//! or refresh-token version.
//!
//! **The tombstone register** is a record of its own, written only upward:
//! per grant, the highest epoch revoked and the latest `rt_version` stored.
//! Every load checks both. A revoked epoch is refused; a record whose
//! `rt_version` is below the floor is an older blob holding a rotated-out
//! refresh token — a store restored from a backup, an instance respawned from
//! an old snapshot — and presenting it could trip the IdP's reuse detection
//! and revoke the whole token family, so it is refused for a new enrolment
//! instead. A writer of the store itself can truncate the register too; from
//! P2 the register is anchored in the hash-chained audit sink.
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
use crate::state::{Durable, Kind};

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

/// The stored record: the metadata, and the sealed secret.
#[derive(Serialize, Deserialize)]
struct Stored {
    grant: Grant,
    /// base64url of the sealed refresh token.
    sealed: String,
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
}

/// Where grants are kept.
pub struct Vault {
    durable: Arc<Durable>,
    key: SealKey,
    previous: Option<SealKey>,
    /// Held while the register is read and written.
    register: Mutex<()>,
}

fn ad(g: &Grant) -> Vec<u8> {
    associated_data(&[
        b"delegation-grant",
        g.grant_id.as_bytes(),
        g.idp_iss.as_bytes(),
        g.sub.as_bytes(),
        g.epoch.to_string().as_bytes(),
        g.rt_version.to_string().as_bytes(),
    ])
}

impl Vault {
    /// `key` is `seal_key`'s bytes, `previous` `seal_key_previous`'s.
    pub fn new(
        durable: Arc<Durable>,
        key: [u8; 32],
        previous: Option<[u8; 32]>,
    ) -> Result<Vault, String> {
        Ok(Vault {
            durable,
            key: SealKey::new(&key).map_err(|e| format!("identity.delegation.seal_key: {e}"))?,
            previous: previous
                .map(|p| SealKey::new(&p))
                .transpose()
                .map_err(|e| format!("identity.delegation.seal_key_previous: {e}"))?,
            register: Mutex::new(()),
        })
    }

    fn id(grant_id: &str) -> String {
        format!("{GRANT_PREFIX}{grant_id}")
    }

    /// The register as stored.
    pub fn floors(&self) -> BTreeMap<String, Floor> {
        self.durable
            .get(Kind::Cred, REGISTER)
            .ok()
            .flatten()
            .and_then(|e| serde_json::from_value(e.state).ok())
            .unwrap_or_default()
    }

    /// Raise `grant_id`'s floor: never lowered.
    fn raise(&self, grant_id: &str, f: impl FnOnce(&mut Floor)) -> Result<(), String> {
        let _held = self.register.lock().unwrap_or_else(|e| e.into_inner());
        let mut all = self.floors();
        let before = all.get(grant_id).copied().unwrap_or_default();
        let mut after = before;
        f(&mut after);
        after.rt_version = after.rt_version.max(before.rt_version);
        after.revoked_epoch = after.revoked_epoch.max(before.revoked_epoch);
        if after == before && all.contains_key(grant_id) {
            return Ok(());
        }
        all.insert(grant_id.to_string(), after);
        let v = serde_json::to_value(&all).map_err(|e| e.to_string())?;
        self.durable
            .put(Kind::Cred, REGISTER, v, None)
            .map(|_| ())
            .map_err(|e| format!("the tombstone register: {e}"))
    }

    /// The grant `grant_id` and its refresh token, held to the register.
    pub fn load(&self, grant_id: &str) -> Result<Option<(Grant, String)>, Refused> {
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
        let sealed = b64::url_decode(&stored.sealed)
            .map_err(|_| Refused::Unreadable("the sealed refresh token is not base64url".into()))?;
        let aad = ad(&stored.grant);
        let plain = self
            .key
            .open(&aad, &sealed)
            .or_else(|e| match &self.previous {
                Some(p) => p.open(&aad, &sealed),
                None => Err(e),
            })
            .map_err(Refused::Unreadable)?;
        let rt = String::from_utf8(plain)
            .map_err(|_| Refused::Unreadable("the refresh token is not UTF-8".into()))?;
        Ok(Some((stored.grant, rt)))
    }

    /// Write `grant` with `rt`, over the record of `rt_version` `over` (`None`:
    /// a new enrolment, over whatever is there). Refused when the stored
    /// record's version is not `over`: someone else rotated first.
    pub fn store(&self, grant: &Grant, rt: &str, over: Option<u64>) -> Result<(), String> {
        if let Some(want) = over {
            let now = self
                .durable
                .get(Kind::Cred, &Self::id(&grant.grant_id))
                .map_err(|e| e.to_string())?
                .and_then(|e| serde_json::from_value::<Stored>(e.state).ok())
                .map(|s| s.grant.rt_version);
            if now != Some(want) {
                return Err(format!(
                    "grant {}: its refresh token was rotated meanwhile (rt_version {now:?}, not {want})",
                    grant.grant_id
                ));
            }
        }
        let sealed = self.key.seal(&ad(grant), rt.as_bytes())?;
        let v = serde_json::to_value(Stored {
            grant: grant.clone(),
            sealed: b64::url_nopad(&sealed),
        })
        .map_err(|e| e.to_string())?;
        self.durable
            .put(Kind::Cred, &Self::id(&grant.grant_id), v, None)
            .map_err(|e| format!("grant {}: {e}", grant.grant_id))?;
        let version = grant.rt_version;
        self.raise(&grant.grant_id, |f| f.rt_version = version)
    }

    /// Revoke `grant_id` at `epoch`: the register first, then the record.
    pub fn tombstone(&self, grant_id: &str, epoch: u64) -> Result<(), String> {
        self.raise(grant_id, |f| f.revoked_epoch = Some(epoch))?;
        self.durable
            .delete(Kind::Cred, &Self::id(grant_id))
            .map_err(|e| format!("grant {grant_id}: {e}"))
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
            [key; 32],
            previous.map(|p| [p; 32]),
        )
        .unwrap()
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
}
