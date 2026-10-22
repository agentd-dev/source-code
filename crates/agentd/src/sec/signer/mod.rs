// SPDX-License-Identifier: AGPL-3.0-only
//! Keys agentd uses and never holds (RFC 0044 §7 P2a, T4/T5): an
//! **external signer** on a unix socket the operator names, which signs a
//! client assertion with the IdP or a resource's client key, and wraps and
//! unwraps the per-record data key a grant is sealed with, so the private
//! keys and the vault's key-encryption key never enter agentd's memory,
//! environment or files. agentd is the client; the protocol is agentd's
//! own, small enough to put in front of a KMS, an HSM or a process of the
//! operator's (`docs/external-signer.md` is its normative page).
//!
//! **The wire.** One request per connection: a frame is a four-byte
//! big-endian length and that many bytes of a UTF-8 JSON object, at most
//! [`MAX_FRAME`]. A request is `{"op", …}`; the answer is `{"ok": true, …}`
//! or `{"ok": false, "error": {"code", "message"}}`.
//! Bytes travel as base64url without padding. The ops:
//!
//! - `public_key {key}` → `{alg, jwk}`: a signing key's public JWK;
//! - `sign {key, alg, data}` → `{sig}`: the JWS signature over `data`, the
//!   signing input `header.claims` — `ES256` as the 64-byte `R || S`,
//!   `RS256` as RSASSA-PKCS1-v1_5;
//! - `wrap {key, plaintext, aad}` → `{wrapped}`: a data key encrypted under
//!   the key-encryption key `key`, bound to `aad`, opaque to agentd;
//! - `unwrap {key, wrapped, aad}` → `{plaintext}`;
//! - `derive {key, data}` → `{mac}`: HMAC-SHA256 of `data` under the
//!   derivation key `key` — the record-id keys of the companies preview's
//!   stores (RFC 0047), each derived from a fixed label;
//! - `sign_typed_data {key, typed_data}` → `{sig}`: an EVM key's 65-byte
//!   `r ‖ s ‖ v` over the EIP-712 digest of `typed_data`, which the signer
//!   computes itself — an x402 payment's EIP-3009 authorization (RFC 0049
//!   D8); `public_key` answers such a key `ES256K`, a secp256k1 JWK;
//! - `sign_solana_transaction {key, message}` → `{sig}`: a Solana key's
//!   64-byte Ed25519 signature of `message`, a Solana transaction's message
//!   the signer reads whole before it signs — an x402 payment's SPL transfer
//!   (RFC 0049 §10); `public_key` answers such a key `EdDSA`, an Ed25519 JWK.
//!
//! **What a signer signs** is its own policy, and `docs/external-signer.md`
//! asks it to sign only what a client assertion is: agentd sends the JWS
//! signing input itself, so a signer reads every header and claim before it
//! signs, and needs no field beside it.
//!
//! **Who answers.** Before a byte is sent, the socket's peer must be the uid
//! the operator named (`SO_PEERCRED`; `getpeereid` off Linux): a socket some
//! other process bound at that path is refused with [`SignerError::Peer`]
//! and told nothing.
//!
//! **When it does not.** A request — the connect, the retry, the write and
//! the read together — ends within the configured timeout: the connect is
//! bounded too (`SO_SNDTIMEO` set before `connect(2)`, which a signer
//! whose backlog is full would otherwise hold forever), and each read and
//! write is given only what is left of it, so a signer that trickles its
//! answer is cut off. A connection that could not be made — no socket,
//! nobody listening — is tried once more after [`RETRY_DELAY`], time
//! allowing; a request the signer may have received is never sent again.
//! What fails is [`SignerError::Unavailable`], typed, and the caller fails
//! closed with it: no assertion is made, so nothing is sent to an
//! authorization server, and a sealed record stays shut.
//!
//! Nothing here logs. An error names the socket, the op and the key id, and
//! the signer's own error code and message (cut to 200 characters) — never
//! the data, a signature, a data key or a wrapped one. The buffers a data
//! key passes through here — the request's field and the frame sent, the
//! answer's frame and field — are wiped once used.

#[cfg(any(feature = "internal-mocks", debug_assertions))]
pub mod mock;

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::{Value, json};

use crate::b64;

/// The largest frame either side sends.
pub const MAX_FRAME: usize = 64 * 1024;
/// How long before a connection that could not be made is tried again.
pub const RETRY_DELAY: Duration = Duration::from_millis(100);
/// The default bound on one request.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(5);

/// The signer's socket and who must be behind it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Signer {
    pub socket: PathBuf,
    /// The uid the signer process runs as.
    pub uid: u32,
    pub timeout: Duration,
}

/// Why a request got no answer to use.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SignerError {
    /// No socket, nobody listening, a timeout, or an answer that is not the
    /// protocol: nothing was signed, wrapped or unwrapped.
    Unavailable(String),
    /// The socket's peer is not the uid configured.
    Peer { want: u32, got: u32 },
    /// The signer answered with an error.
    Refused { code: String, message: String },
}

impl std::fmt::Display for SignerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SignerError::Unavailable(e) => write!(f, "signer unavailable: {e}"),
            SignerError::Peer { want, got } => write!(
                f,
                "signer refused: the socket's peer runs as uid {got}, and the signer is configured as uid {want} — nothing was sent"
            ),
            SignerError::Refused { code, message } => {
                write!(f, "the signer refused ({code}): {message}")
            }
        }
    }
}

/// A signing key's public half, as the signer describes it.
#[derive(Debug, Clone, PartialEq)]
pub struct PublicKey {
    pub alg: String,
    pub jwk: Value,
}

impl Signer {
    pub fn new(socket: impl Into<PathBuf>, uid: u32, timeout: Option<Duration>) -> Signer {
        Signer {
            socket: socket.into(),
            uid,
            timeout: timeout.unwrap_or(DEFAULT_TIMEOUT),
        }
    }

    /// `key`'s public JWK and algorithm.
    pub fn public_key(&self, key: &str) -> Result<PublicKey, SignerError> {
        let a = self.call(json!({"op": "public_key", "key": key}))?;
        match (a["alg"].as_str(), a.get("jwk")) {
            (Some(alg), Some(jwk)) if jwk.is_object() => Ok(PublicKey {
                alg: alg.to_string(),
                jwk: jwk.clone(),
            }),
            _ => Err(self.malformed("public_key", key, "no alg and jwk")),
        }
    }

    /// The JWS signature over `data` with `key` under `alg`.
    pub fn sign(&self, key: &str, alg: &str, data: &[u8]) -> Result<Vec<u8>, SignerError> {
        let a = self.call(json!({"op": "sign", "key": key, "alg": alg,
                                 "data": b64::url_nopad(data)}))?;
        let sig = self.bytes(&a, "sig", "sign", key)?;
        // ES256 is the fixed-width R || S a JWS carries; a DER signature, as
        // a KMS returns one, is the signer's to convert.
        if alg == "ES256" && sig.len() != 64 {
            return Err(self.malformed(
                "sign",
                key,
                &format!("an ES256 signature of {} bytes, not 64", sig.len()),
            ));
        }
        Ok(sig)
    }

    /// `plaintext` wrapped under the key-encryption key `key`, bound to `aad`.
    pub fn wrap(&self, key: &str, plaintext: &[u8], aad: &[u8]) -> Result<Vec<u8>, SignerError> {
        let a = self.call_secret(
            json!({"op": "wrap", "key": key,
                   "plaintext": b64::url_nopad(plaintext),
                   "aad": b64::url_nopad(aad)}),
            Some("plaintext"),
            false,
        )?;
        self.bytes(&a, "wrapped", "wrap", key)
    }

    /// What [`Self::wrap`] wrapped under `key` and `aad`.
    pub fn unwrap(&self, key: &str, wrapped: &[u8], aad: &[u8]) -> Result<Vec<u8>, SignerError> {
        let mut a = self.call_secret(
            json!({"op": "unwrap", "key": key,
                   "wrapped": b64::url_nopad(wrapped),
                   "aad": b64::url_nopad(aad)}),
            None,
            true,
        )?;
        let out = self.bytes(&a, "plaintext", "unwrap", key);
        wipe_field(&mut a, "plaintext");
        out
    }

    /// HMAC-SHA256 of `data` under the derivation key `key`: 32 bytes.
    pub fn derive(&self, key: &str, data: &[u8]) -> Result<[u8; 32], SignerError> {
        let mut a = self.call_secret(
            json!({"op": "derive", "key": key, "data": b64::url_nopad(data)}),
            None,
            true,
        )?;
        let mut mac = self.bytes(&a, "mac", "derive", key)?;
        wipe_field(&mut a, "mac");
        let out = <[u8; 32]>::try_from(mac.as_slice()).map_err(|_| {
            self.malformed(
                "derive",
                key,
                &format!("a mac of {} bytes, not 32", mac.len()),
            )
        });
        crate::sec::harden::wipe(&mut mac);
        out
    }

    /// The 65-byte `r ‖ s ‖ v` signature, by the EVM key `key`, of the
    /// EIP-712 typed data `typed_data` (RFC 0049 D8): sent whole, not as a
    /// digest, so the signer computes the digest itself from what it read.
    #[cfg(feature = "x402")]
    pub fn sign_typed_data(&self, key: &str, typed_data: &Value) -> Result<[u8; 65], SignerError> {
        let a =
            self.call(json!({"op": "sign_typed_data", "key": key, "typed_data": typed_data}))?;
        let sig = self.bytes(&a, "sig", "sign_typed_data", key)?;
        <[u8; 65]>::try_from(sig.as_slice()).map_err(|_| {
            self.malformed(
                "sign_typed_data",
                key,
                &format!("a signature of {} bytes, not 65", sig.len()),
            )
        })
    }

    /// The 64-byte Ed25519 signature, by the Solana key `key`, of a
    /// transaction's `message` (RFC 0049 §10): the bytes signed, sent whole,
    /// so the signer reads the transfer before it signs.
    #[cfg(feature = "x402")]
    pub fn sign_solana_transaction(
        &self,
        key: &str,
        message: &[u8],
    ) -> Result<[u8; 64], SignerError> {
        let a = self.call(json!({"op": "sign_solana_transaction", "key": key,
                                 "message": b64::url_nopad(message)}))?;
        let sig = self.bytes(&a, "sig", "sign_solana_transaction", key)?;
        <[u8; 64]>::try_from(sig.as_slice()).map_err(|_| {
            self.malformed(
                "sign_solana_transaction",
                key,
                &format!("a signature of {} bytes, not 64", sig.len()),
            )
        })
    }

    fn malformed(&self, op: &str, key: &str, why: &str) -> SignerError {
        SignerError::Unavailable(format!(
            "{}: {op} {key:?}: the answer is not the protocol ({why})",
            self.socket.display()
        ))
    }

    fn bytes(&self, a: &Value, field: &str, op: &str, key: &str) -> Result<Vec<u8>, SignerError> {
        a[field]
            .as_str()
            .and_then(|s| b64::url_decode(s).ok())
            .ok_or_else(|| self.malformed(op, key, &format!("no base64url {field}")))
    }

    /// One request and its answer: the peer checked first, a connection
    /// that could not be made tried once more, the whole within the timeout.
    fn call(&self, req: Value) -> Result<Value, SignerError> {
        self.call_secret(req, None, false)
    }

    /// [`Self::call`], wiping the request's field `sent` once it is sent,
    /// and the answer's frame once it is read when the answer is `secret`.
    fn call_secret(
        &self,
        mut req: Value,
        sent: Option<&str>,
        secret: bool,
    ) -> Result<Value, SignerError> {
        let op = req["op"].as_str().unwrap_or_default().to_string();
        let key = req["key"].as_str().unwrap_or_default().to_string();
        let at = || format!("{}: {op} {key:?}", self.socket.display());
        let until = std::time::Instant::now() + self.timeout;
        let stream = match self.connect(until) {
            Ok(s) => s,
            Err(e) => {
                if until.saturating_duration_since(std::time::Instant::now()) <= RETRY_DELAY {
                    return Err(SignerError::Unavailable(format!("{}: {e}", at())));
                }
                std::thread::sleep(RETRY_DELAY);
                self.connect(until)
                    .map_err(|e| SignerError::Unavailable(format!("{}: {e}", at())))?
            }
        };
        let got = peer_uid(&stream)
            .map_err(|e| SignerError::Unavailable(format!("{}: the peer: {e}", at())))?;
        if got != self.uid {
            return Err(SignerError::Peer {
                want: self.uid,
                got,
            });
        }
        // Serialized into a frame that never grows, so no copy of what it
        // carries is left behind by a reallocation.
        let mut frame = Vec::with_capacity(4 + MAX_FRAME + 1);
        frame.extend_from_slice(&[0; 4]);
        let serialized = serde_json::to_writer(&mut frame, &req);
        if let Some(f) = sent {
            wipe_field(&mut req, f);
        }
        let written = serialized.map_err(std::io::Error::other).and_then(|()| {
            let n = frame.len() - 4;
            if n > MAX_FRAME {
                return Err(std::io::Error::other(format!(
                    "a frame of {n} bytes (1 to {MAX_FRAME})"
                )));
            }
            frame[..4].copy_from_slice(&(n as u32).to_be_bytes());
            Deadline::new(&stream, until).write_all(&frame)
        });
        crate::sec::harden::wipe(&mut frame);
        written.map_err(|e| SignerError::Unavailable(format!("{}: {e}", at())))?;
        let mut answer = read_frame(&mut Deadline::new(&stream, until))
            .map_err(|e| SignerError::Unavailable(format!("{}: {e}", at())))?;
        let parsed = serde_json::from_slice::<Value>(&answer);
        if secret {
            crate::sec::harden::wipe(&mut answer);
        }
        let a: Value = parsed
            .map_err(|_| SignerError::Unavailable(format!("{}: the answer is not JSON", at())))?;
        match a["ok"].as_bool() {
            Some(true) => Ok(a),
            Some(false) => Err(SignerError::Refused {
                code: cut(a["error"]["code"].as_str().unwrap_or("unknown")),
                message: cut(a["error"]["message"].as_str().unwrap_or_default()),
            }),
            None => Err(SignerError::Unavailable(format!(
                "{}: the answer has no ok",
                at()
            ))),
        }
    }

    /// Connect by `until`: a signer whose listen backlog is full holds a
    /// plain `connect(2)` for as long as it stays full.
    fn connect(
        &self,
        until: std::time::Instant,
    ) -> std::io::Result<std::os::unix::net::UnixStream> {
        let left = left(until)?;
        #[cfg(any(target_os = "linux", target_os = "android"))]
        {
            connect_within(&self.socket, left)
        }
        #[cfg(not(any(target_os = "linux", target_os = "android")))]
        {
            let _ = left;
            std::os::unix::net::UnixStream::connect(&self.socket)
        }
    }
}

/// What is left until `until`, or a timeout when nothing is.
fn left(until: std::time::Instant) -> std::io::Result<Duration> {
    let left = until.saturating_duration_since(std::time::Instant::now());
    if left.is_zero() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            "the signer did not answer in time",
        ));
    }
    Ok(left)
}

/// A unix-socket connect that gives up after `within`: the send timeout,
/// which Linux's `connect(2)` waits a full backlog out by, set first.
#[cfg(any(target_os = "linux", target_os = "android"))]
fn connect_within(
    path: &Path,
    within: Duration,
) -> std::io::Result<std::os::unix::net::UnixStream> {
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use std::os::unix::ffi::OsStrExt;
    // SAFETY: an all-zero sockaddr_un is valid.
    let mut addr: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    addr.sun_family = libc::AF_UNIX as libc::sa_family_t;
    let bytes = path.as_os_str().as_bytes();
    if bytes.len() >= addr.sun_path.len() {
        return Err(std::io::Error::other(format!(
            "{}: longer than a unix socket path may be",
            path.display()
        )));
    }
    for (d, b) in addr.sun_path.iter_mut().zip(bytes) {
        *d = *b as libc::c_char;
    }
    // SAFETY: socket(2) with constant arguments.
    let fd = unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_STREAM | libc::SOCK_CLOEXEC, 0) };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: a descriptor just made, owned here alone.
    let fd = unsafe { OwnedFd::from_raw_fd(fd) };
    let tv = libc::timeval {
        tv_sec: within.as_secs() as libc::time_t,
        tv_usec: within.subsec_micros().max(1) as libc::suseconds_t,
    };
    // SAFETY: a valid descriptor and a timeval of the size given.
    if unsafe {
        libc::setsockopt(
            fd.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_SNDTIMEO,
            (&tv as *const libc::timeval).cast(),
            std::mem::size_of::<libc::timeval>() as libc::socklen_t,
        )
    } != 0
    {
        return Err(std::io::Error::last_os_error());
    }
    let len = (std::mem::size_of::<libc::sa_family_t>() + bytes.len() + 1) as libc::socklen_t;
    loop {
        // SAFETY: a valid descriptor and an address of the length given.
        let r = unsafe {
            libc::connect(
                fd.as_raw_fd(),
                (&addr as *const libc::sockaddr_un).cast(),
                len,
            )
        };
        if r == 0 {
            break;
        }
        let e = std::io::Error::last_os_error();
        match e.raw_os_error() {
            Some(libc::EINTR) => continue,
            Some(libc::EAGAIN) => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "the signer accepted no connection in time (its backlog is full)",
                ));
            }
            _ => return Err(e),
        }
    }
    Ok(std::os::unix::net::UnixStream::from(fd))
}

/// A stream whose every read and write is given only what is left until
/// `until`.
struct Deadline<'a> {
    s: &'a std::os::unix::net::UnixStream,
    until: std::time::Instant,
}

impl<'a> Deadline<'a> {
    fn new(s: &'a std::os::unix::net::UnixStream, until: std::time::Instant) -> Self {
        Deadline { s, until }
    }
}

impl Read for Deadline<'_> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.s.set_read_timeout(Some(left(self.until)?))?;
        (&mut &*self.s).read(buf)
    }
}

impl Write for Deadline<'_> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.s.set_write_timeout(Some(left(self.until)?))?;
        (&mut &*self.s).write(buf)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Overwrite the string `v[field]` holds with zeros, and drop it.
fn wipe_field(v: &mut Value, field: &str) {
    if let Some(Value::String(s)) = v.get_mut(field) {
        let mut b = std::mem::take(s).into_bytes();
        crate::sec::harden::wipe(&mut b);
    }
}

/// `s` cut to 200 characters: a signer's words are kept short in an error.
fn cut(s: &str) -> String {
    s.chars().take(200).collect()
}

/// The uid of the process at the other end of `s`.
pub fn peer_uid(s: &std::os::unix::net::UnixStream) -> std::io::Result<u32> {
    use std::os::fd::AsRawFd;
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        let mut cred = libc::ucred {
            pid: 0,
            uid: 0,
            gid: 0,
        };
        let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
        // SAFETY: a valid fd, and a pointer to a ucred of the length given.
        let r = unsafe {
            libc::getsockopt(
                s.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_PEERCRED,
                (&mut cred as *mut libc::ucred).cast(),
                &mut len,
            )
        };
        if r != 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(cred.uid)
    }
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    {
        let (mut uid, mut gid) = (0, 0);
        // SAFETY: a valid fd, and two pointers to stack values.
        if unsafe { libc::getpeereid(s.as_raw_fd(), &mut uid, &mut gid) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(uid)
    }
}

/// Write one frame: the length, then `body`.
pub fn write_frame(w: &mut impl Write, body: &[u8]) -> std::io::Result<()> {
    if body.is_empty() || body.len() > MAX_FRAME {
        return Err(std::io::Error::other(format!(
            "a frame of {} bytes (1 to {MAX_FRAME})",
            body.len()
        )));
    }
    let mut out = Vec::with_capacity(4 + body.len());
    out.extend_from_slice(&(body.len() as u32).to_be_bytes());
    out.extend_from_slice(body);
    w.write_all(&out)?;
    w.flush()
}

/// Read one frame's body.
pub fn read_frame(r: &mut impl Read) -> std::io::Result<Vec<u8>> {
    let mut len = [0u8; 4];
    r.read_exact(&mut len)?;
    let n = u32::from_be_bytes(len) as usize;
    if n == 0 || n > MAX_FRAME {
        return Err(std::io::Error::other(format!(
            "a frame of {n} bytes (1 to {MAX_FRAME})"
        )));
    }
    let mut body = vec![0u8; n];
    r.read_exact(&mut body)?;
    Ok(body)
}

/// Whether `p` is a path a signer socket may be named by: absolute.
pub fn valid_socket_path(p: &Path) -> bool {
    p.is_absolute()
}

#[cfg(test)]
mod tests {
    use super::*;
    use ring::signature::{ECDSA_P256_SHA256_FIXED, UnparsedPublicKey};

    fn uid() -> u32 {
        // SAFETY: getuid has no failure mode.
        unsafe { libc::getuid() }
    }

    /// The reference signer's knobs with `idp` signing for `aud`.
    fn knobs_for(aud: &str) -> Value {
        let mut k = mock::default_knobs();
        k["keys"]["idp"]["audiences"] = json!([aud]);
        k
    }

    const AUD: &str = "https://idp.example/token";

    /// The reference signer answers every op, and agentd's client reads
    /// each answer: a signature that verifies with the public JWK alone, a
    /// data key that unwraps only under the associated data it was wrapped
    /// with.
    #[test]
    fn every_op_round_trips_through_the_reference_signer() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("s.sock");
        let _m = mock::serve(&sock, &knobs_for(AUD)).unwrap();
        let s = Signer::new(&sock, uid(), None);

        let pk = s.public_key("idp").unwrap();
        assert_eq!(pk.alg, "ES256");
        assert!(pk.jwk.get("d").is_none(), "a public JWK: {}", pk.jwk);
        let input =
            crate::auth::client_assertion::assertion_input("agentd", AUD, "ES256", "idp").unwrap();
        let sig = s.sign("idp", "ES256", input.as_bytes()).unwrap();
        let mut point = vec![0x04];
        point.extend(b64::url_decode(pk.jwk["x"].as_str().unwrap()).unwrap());
        point.extend(b64::url_decode(pk.jwk["y"].as_str().unwrap()).unwrap());
        UnparsedPublicKey::new(&ECDSA_P256_SHA256_FIXED, &point)
            .verify(input.as_bytes(), &sig)
            .expect("the signature verifies with the public key");
        // A derivation is HMAC-SHA256 under the derivation key: the same
        // data the same 32 bytes, other data others.
        let a = s
            .derive("derive", b"agentd company-user record id")
            .unwrap();
        assert_eq!(
            a,
            s.derive("derive", b"agentd company-user record id")
                .unwrap()
        );
        assert_ne!(
            a,
            s.derive("derive", b"agentd company-account record id")
                .unwrap()
        );
        assert!(
            matches!(s.derive("seal", b"x"), Err(SignerError::Refused { code, .. }) if code == "wrong_use")
        );
        assert!(
            matches!(s.derive("derive", b""), Err(SignerError::Refused { code, .. }) if code == "bad_request")
        );

        let wrapped = s.wrap("seal", &[7u8; 32], b"ad-1").unwrap();
        assert!(!wrapped.windows(32).any(|w| w == [7u8; 32]), "wrapped");
        assert_eq!(s.unwrap("seal", &wrapped, b"ad-1").unwrap(), vec![7u8; 32]);
        match s.unwrap("seal", &wrapped, b"ad-2") {
            Err(SignerError::Refused { code, .. }) => assert_eq!(code, "unwrap_failed"),
            other => panic!("{other:?}"),
        }
        // A key the signer does not hold, or one used for what it is not.
        for r in [
            s.sign("nope", "ES256", b"x").err(),
            s.sign("seal", "ES256", b"x").err(),
            s.wrap("idp", b"k", b"a").err(),
            s.sign("idp", "RS256", b"x").err(),
        ] {
            assert!(matches!(r, Some(SignerError::Refused { .. })), "{r:?}");
        }
    }

    /// The reference signer signs only a client assertion, and only for an
    /// audience its key is configured with: a payload of any other shape,
    /// and an assertion for another audience, are `denied` and recorded as
    /// refused — nothing is signed.
    #[test]
    fn the_signer_signs_only_an_assertion_for_a_configured_audience() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("s.sock");
        let record = dir.path().join("record.jsonl");
        let mut knobs = knobs_for(AUD);
        knobs["record"] = json!(record.display().to_string());
        let _m = mock::serve(&sock, &knobs).unwrap();
        let s = Signer::new(&sock, uid(), None);
        let denied = |r: Result<Vec<u8>, SignerError>| matches!(r, Err(SignerError::Refused { code, .. }) if code == "denied");
        assert!(
            denied(s.sign("idp", "ES256", b"anything at all")),
            "a non-assertion payload"
        );
        let elsewhere = crate::auth::client_assertion::assertion_input(
            "agentd",
            "https://evil.example/token",
            "ES256",
            "idp",
        )
        .unwrap();
        assert!(
            denied(s.sign("idp", "ES256", elsewhere.as_bytes())),
            "an unconfigured audience"
        );
        // `ras` has no audience at all: it signs nothing.
        let for_ras =
            crate::auth::client_assertion::assertion_input("agentd", AUD, "ES256", "ras").unwrap();
        assert!(denied(s.sign("ras", "ES256", for_ras.as_bytes())));
        let ok =
            crate::auth::client_assertion::assertion_input("agentd", AUD, "ES256", "idp").unwrap();
        assert!(s.sign("idp", "ES256", ok.as_bytes()).is_ok());
        let seen = std::fs::read_to_string(&record).unwrap();
        assert_eq!(seen.matches("\"denied\"").count(), 3, "{seen}");
        assert!(
            !seen.contains("evil.example"),
            "the record holds no data: {seen}"
        );
    }

    /// No socket is a typed outage after one retry; a socket whose peer is
    /// another uid is refused before a byte is sent.
    #[test]
    fn a_missing_signer_and_another_uids_socket_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("none.sock");
        let s = Signer::new(&sock, uid(), Some(Duration::from_millis(500)));
        let t = std::time::Instant::now();
        match s.sign("idp", "ES256", b"x") {
            Err(SignerError::Unavailable(e)) => assert!(e.contains("none.sock"), "{e}"),
            other => panic!("{other:?}"),
        }
        assert!(t.elapsed() >= RETRY_DELAY, "tried again once");

        let sock = dir.path().join("s.sock");
        let record = dir.path().join("record.jsonl");
        let mut knobs = mock::default_knobs();
        knobs["record"] = json!(record.display().to_string());
        let _m = mock::serve(&sock, &knobs).unwrap();
        let other = Signer::new(&sock, uid().wrapping_add(1), None);
        assert_eq!(
            other.sign("idp", "ES256", b"x"),
            Err(SignerError::Peer {
                want: uid().wrapping_add(1),
                got: uid()
            })
        );
        let seen = std::fs::read_to_string(&record).unwrap_or_default();
        assert!(!seen.contains("\"sign\""), "nothing was sent: {seen}");
    }

    /// A signer that stops answering mid-request is an outage within the
    /// timeout, not a hang.
    #[test]
    fn a_silent_signer_times_out() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("silent.sock");
        let l = std::os::unix::net::UnixListener::bind(&sock).unwrap();
        let held = std::thread::spawn(move || {
            let (c, _) = l.accept().unwrap();
            std::thread::sleep(Duration::from_millis(800));
            drop(c);
        });
        let s = Signer::new(&sock, uid(), Some(Duration::from_millis(200)));
        let t = std::time::Instant::now();
        assert!(matches!(
            s.public_key("idp"),
            Err(SignerError::Unavailable(_))
        ));
        assert!(
            t.elapsed() < Duration::from_millis(700),
            "{:?}",
            t.elapsed()
        );
        held.join().unwrap();
    }

    /// A request ends within the timeout however the signer stalls it: a
    /// listen backlog that is full holds the connect, and an answer sent a
    /// byte at a time holds the read.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_stalled_signer_is_cut_off_at_the_timeout() {
        use std::os::fd::AsRawFd;
        let dir = tempfile::tempdir().unwrap();
        // A listener that accepts nothing, its backlog of none taken.
        let full = dir.path().join("full.sock");
        let l = std::os::unix::net::UnixListener::bind(&full).unwrap();
        // SAFETY: a listening socket's descriptor; listen again sets its
        // backlog.
        assert_eq!(unsafe { libc::listen(l.as_raw_fd(), 0) }, 0);
        let _queued = std::os::unix::net::UnixStream::connect(&full).unwrap();
        let s = Signer::new(&full, uid(), Some(Duration::from_millis(300)));
        let t = std::time::Instant::now();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(s.public_key("idp"));
        });
        let r = rx
            .recv_timeout(Duration::from_secs(10))
            .expect("the connect to a full backlog was not cut off");
        assert!(matches!(r, Err(SignerError::Unavailable(_))), "{r:?}");
        assert!(t.elapsed() < Duration::from_secs(3), "{:?}", t.elapsed());
        // A signer that answers a byte every 100ms.
        let slow = dir.path().join("slow.sock");
        let l = std::os::unix::net::UnixListener::bind(&slow).unwrap();
        std::thread::spawn(move || {
            if let Ok((mut c, _)) = l.accept() {
                let _ = read_frame(&mut c);
                let _ = c.write_all(&100u32.to_be_bytes());
                for _ in 0..100 {
                    if c.write_all(b" ").is_err() {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(100));
                }
            }
        });
        let s = Signer::new(&slow, uid(), Some(Duration::from_millis(500)));
        let t = std::time::Instant::now();
        let r = s.public_key("idp");
        assert!(matches!(r, Err(SignerError::Unavailable(_))), "{r:?}");
        assert!(t.elapsed() < Duration::from_secs(3), "{:?}", t.elapsed());
    }

    #[test]
    fn frames_are_bounded() {
        let mut buf = Vec::new();
        write_frame(&mut buf, b"{}").unwrap();
        assert_eq!(&buf[..4], &[0, 0, 0, 2]);
        assert_eq!(read_frame(&mut buf.as_slice()).unwrap(), b"{}");
        assert!(write_frame(&mut Vec::new(), &vec![b'x'; MAX_FRAME + 1]).is_err());
        let huge = ((MAX_FRAME + 1) as u32).to_be_bytes();
        assert!(read_frame(&mut huge.as_slice()).is_err());
        assert!(read_frame(&mut [0u8, 0, 0, 0].as_slice()).is_err());
    }
}
