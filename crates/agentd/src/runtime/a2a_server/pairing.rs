// SPDX-License-Identifier: AGPL-3.0-only
//! Pairing-code login: the rotating code, the session tokens it mints, and
//! the two calls that read and redeem it.

use super::FeedVis;
use super::{UNSUPPORTED_OPERATION, err_obj};
use crate::a2a::Principal;
use crate::runtime::reactor::Runtime;
use serde_json::{Value, json};
use std::sync::Mutex;
use std::time::Duration;

/// The pairing window (how often the code rotates).
const PAIR_WINDOW_SECS: u64 = 60;
/// Failed attempts allowed per window before pairing locks out.
const PAIR_MAX_FAILS: usize = 5;

/// Pairing-code login state: a per-process random seed derives a 6-digit code
/// per 60-second window (`HMAC(seed, window)` — no timer thread needed); a
/// correct code (current or previous window, constant-time, rate-limited)
/// mints a high-entropy **session token** that rides `Authorization: Bearer`
/// like any other credential. Sessions live in memory: a restart revokes all.
pub struct PairingState {
    seed: [u8; 32],
    role: crate::config::v2::Role,
    ttl_ms: u64,
    sessions: Mutex<std::collections::HashMap<String, (crate::config::v2::Role, u64)>>,
    /// Recent failed-attempt timestamps (ms) — the rate limiter.
    fails: Mutex<Vec<u64>>,
}

impl PairingState {
    /// Build with fresh randomness. Fails without an OS entropy source.
    pub fn new(role: crate::config::v2::Role, ttl: Duration) -> Result<PairingState, String> {
        Ok(PairingState {
            seed: os_random_32()?,
            role,
            ttl_ms: ttl.as_millis() as u64,
            sessions: Mutex::new(std::collections::HashMap::new()),
            fails: Mutex::new(Vec::new()),
        })
    }

    fn code_for(&self, window: u64) -> String {
        let mac = crate::sha::hmac_sha256(&self.seed, &window.to_be_bytes());
        let n = u32::from_be_bytes([mac[0], mac[1], mac[2], mac[3]]) % 1_000_000;
        format!("{n:06}")
    }

    /// The current code and how long it stays valid (ms).
    pub fn current_code(&self) -> (String, u64) {
        let now = crate::state::now_ms();
        let window = now / 1000 / PAIR_WINDOW_SECS;
        let expires_in = (window + 1) * PAIR_WINDOW_SECS * 1000 - now;
        (self.code_for(window), expires_in)
    }

    /// Verify a presented code (current or previous window — clock/typing
    /// grace), constant-time, rate-limited. `Ok(token, expires_ms)` mints a
    /// session; `Err` is the client-facing message.
    pub fn pair(&self, code: &str) -> Result<(String, u64), String> {
        let now = crate::state::now_ms();
        {
            let mut fails = self.fails.lock().unwrap_or_else(|e| e.into_inner());
            fails.retain(|t| now.saturating_sub(*t) < PAIR_WINDOW_SECS * 1000);
            if fails.len() >= PAIR_MAX_FAILS {
                return Err("too many pairing attempts; wait a minute".into());
            }
        }
        let window = now / 1000 / PAIR_WINDOW_SECS;
        let code = code.trim().replace([' ', '-'], "");
        let hit = crate::sha::ct_eq(self.code_for(window).as_bytes(), code.as_bytes())
            | crate::sha::ct_eq(
                self.code_for(window.saturating_sub(1)).as_bytes(),
                code.as_bytes(),
            );
        if !hit {
            self.fails
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(now);
            return Err("wrong pairing code".into());
        }
        let token = format!("pat-{}", crate::sha::to_hex(&os_random_32()?));
        let expires = now + self.ttl_ms;
        let mut sessions = self.sessions.lock().unwrap_or_else(|e| e.into_inner());
        sessions.retain(|_, (_, exp)| *exp > now);
        sessions.insert(token.clone(), (self.role, expires));
        Ok((token, expires))
    }

    /// Resolve a presented bearer against the live sessions.
    pub fn check_bearer(&self, bearer: &str) -> Option<crate::config::v2::Role> {
        if !bearer.starts_with("pat-") {
            return None;
        }
        let now = crate::state::now_ms();
        let sessions = self.sessions.lock().unwrap_or_else(|e| e.into_inner());
        sessions
            .get(bearer)
            .filter(|(_, exp)| *exp > now)
            .map(|(role, _)| *role)
    }

    pub fn role(&self) -> crate::config::v2::Role {
        self.role
    }
    pub fn session_count(&self) -> usize {
        let now = crate::state::now_ms();
        self.sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .filter(|(_, exp)| *exp > now)
            .count()
    }
}

/// 32 bytes of OS randomness (`/dev/urandom`) — dependency-free.
fn os_random_32() -> Result<[u8; 32], String> {
    use std::io::Read;
    let mut buf = [0u8; 32];
    #[cfg(unix)]
    {
        std::fs::File::open("/dev/urandom")
            .and_then(|mut f| f.read_exact(&mut buf))
            .map_err(|e| format!("/dev/urandom: {e}"))?;
        Ok(buf)
    }
    #[cfg(not(unix))]
    {
        let _ = &mut buf;
        Err("pairing needs an OS entropy source (unix /dev/urandom)".into())
    }
}

/// The principal a live pairing session resolves to.
pub fn paired_principal(role: crate::config::v2::Role) -> Principal {
    use crate::config::v2::Role;
    match role {
        Role::Operator => Principal {
            id: "operator".into(),
            role: Role::Operator,
            grants: vec!["*".into()],
            rate: None,
            budget: None,
            labels: Default::default(),
        },
        other => Principal {
            id: "user:paired".into(),
            role: other,
            grants: Vec::new(),
            rate: None,
            budget: None,
            labels: Default::default(),
        },
    }
}

impl Runtime {
    /// `pairing.code` (operator): the CURRENT rotating code + its remaining
    /// validity — what the operator reads out to whoever is connecting.
    pub(super) fn interface_pairing_code(&self) -> Value {
        if !self.settings.interface.enabled {
            return err_obj(
                UNSUPPORTED_OPERATION,
                "the interface surface is disabled (set interface.enabled: true)",
            );
        }
        let Some(p) = &self.a2a_pairing else {
            return err_obj(
                UNSUPPORTED_OPERATION,
                "pairing is disabled (set interface.pairing.enabled: true)",
            );
        };
        let (code, expires_in) = p.current_code();
        json!({"pairing": {
            "code": code,
            "expires_in_ms": expires_in,
            "window_ms": PAIR_WINDOW_SECS * 1000,
            "role": format!("{:?}", p.role()).to_lowercase(),
            "sessions": p.session_count(),
            "url": self.settings.a2a.listen,
        }})
    }

    /// `Pair {code}` — exchange the rotating code for a session token. This is
    /// the ONE method an anonymous caller may use, so it is rate-limited and
    /// locks out after too many failures within a window.
    pub(super) fn a2a_pair(&mut self, params: &Value) -> Value {
        let Some(p) = &self.a2a_pairing else {
            return err_obj(
                UNSUPPORTED_OPERATION,
                "pairing is disabled (set interface.pairing.enabled: true)",
            );
        };
        let code = params
            .get("code")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if code.is_empty() {
            return err_obj(::mcp::rpc::INVALID_PARAMS, "Pair needs a code");
        }
        match p.pair(code) {
            Ok((token, expires)) => {
                self.log
                    .info("interface.paired", json!({"role": format!("{:?}", p.role()).to_lowercase(), "sessions": p.session_count()}));
                self.feed_push(
                    "pairing",
                    FeedVis::Operator,
                    json!({"paired": true, "sessions": p.session_count()}),
                );
                json!({"token": token, "expiresAt": expires, "role": format!("{:?}", p.role()).to_lowercase(),
                       "agent": {"name": "agentd", "instance": self.instance, "version": crate::VERSION}})
            }
            Err(e) => err_obj(-32003, &e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pairing_codes_rotate_verify_rate_limit_and_mint_sessions() {
        use crate::config::v2::Role;
        let p = PairingState::new(Role::Operator, Duration::from_secs(60)).unwrap();
        // Deterministic per window; distinct across windows; 6 digits.
        let w = crate::state::now_ms() / 1000 / PAIR_WINDOW_SECS;
        let (code, expires_in) = p.current_code();
        assert_eq!(code, p.code_for(w));
        assert_eq!(code.len(), 6);
        assert!(code.chars().all(|c| c.is_ascii_digit()));
        assert!(expires_in <= PAIR_WINDOW_SECS * 1000);
        assert_ne!(p.code_for(w), p.code_for(w + 1));
        // Two instances have different seeds ⇒ different codes (unpredictable).
        let q = PairingState::new(Role::Operator, Duration::from_secs(60)).unwrap();
        assert_ne!(p.code_for(w), q.code_for(w), "seeded from OS randomness");
        // The current AND previous window verify (grace); formatting tolerated.
        let prev = p.code_for(w.saturating_sub(1));
        let spaced = format!("{} {}", &code[..3], &code[3..]);
        let (tok, exp) = p.pair(&spaced).unwrap();
        assert!(tok.starts_with("pat-") && tok.len() > 40, "{tok}");
        assert!(exp > crate::state::now_ms());
        let _ = p.pair(&prev).unwrap();
        assert_eq!(p.session_count(), 2);
        // The minted token resolves as a bearer; garbage does not.
        assert_eq!(p.check_bearer(&tok), Some(Role::Operator));
        assert_eq!(p.check_bearer("pat-nope"), None);
        assert_eq!(p.check_bearer("other"), None);
        // Rate limit: failures lock pairing out for the window.
        for _ in 0..PAIR_MAX_FAILS {
            assert!(p.pair("000000").is_err() || p.pair("999999").is_err());
        }
        let locked = p.pair(&p.current_code().0);
        assert!(
            locked.is_err() && locked.unwrap_err().contains("too many"),
            "even the right code is refused while locked out"
        );
        // Expired sessions stop resolving.
        let short = PairingState::new(Role::User, Duration::from_millis(1)).unwrap();
        let (t2, _) = short.pair(&short.current_code().0).unwrap();
        std::thread::sleep(Duration::from_millis(5));
        assert_eq!(short.check_bearer(&t2), None, "expired");
    }
}
