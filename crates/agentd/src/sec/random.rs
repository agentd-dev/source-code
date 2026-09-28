// SPDX-License-Identifier: AGPL-3.0-only
//! OS randomness for secrets: session tokens, launch codes, internal keys a
//! caller must not be able to guess.
//!
//! Read straight from the kernel's CSPRNG (`/dev/urandom`) — no dependency,
//! and nothing seeded in-process that a fork or a snapshot could duplicate.
//! Unlike the id generators elsewhere (a ULID only needs to be unique), there
//! is deliberately **no fallback**: a token derived from the clock and the pid
//! is one an attacker can enumerate, so a host without an entropy source gets
//! an error, never a weaker token.

use std::io;

/// The kernel's entropy device, opened once and kept. A request that mints a
/// token must not need a fresh file descriptor: a peer that holds enough idle
/// connections to exhaust the fd table would otherwise turn every mint into an
/// error, where a held descriptor keeps reading.
#[cfg(unix)]
static SOURCE: std::sync::OnceLock<std::fs::File> = std::sync::OnceLock::new();

#[cfg(unix)]
fn source() -> io::Result<&'static std::fs::File> {
    if let Some(f) = SOURCE.get() {
        return Ok(f);
    }
    // Two threads racing here each open the device; one handle wins and the
    // other is dropped, which costs a descriptor for an instant and nothing
    // else. (`OnceLock::get_or_try_init` would say this directly, but is not
    // stable.) std opens with O_CLOEXEC, so a re-exec'd child never inherits it.
    let f = std::fs::File::open("/dev/urandom")?;
    Ok(SOURCE.get_or_init(|| f))
}

/// Open the entropy source now. The daemon calls this at startup, so a host
/// without one refuses to start instead of failing on the first request that
/// needs a secret, and so no later mint depends on a free file descriptor.
pub fn open() -> io::Result<()> {
    #[cfg(unix)]
    {
        source().map(|_| ())
    }
    #[cfg(not(unix))]
    {
        Err(unsupported())
    }
}

#[cfg(not(unix))]
fn unsupported() -> io::Error {
    io::Error::new(
        io::ErrorKind::Unsupported,
        "no OS entropy source (agentd reads /dev/urandom)",
    )
}

/// Fill `buf` from the OS CSPRNG.
pub fn fill(buf: &mut [u8]) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::io::Read;
        // `Read` is implemented for `&File`, so the shared handle reads
        // without a lock of ours: each read(2) on /dev/urandom is independent.
        source()?.read_exact(buf)
    }
    #[cfg(not(unix))]
    {
        let _ = buf;
        Err(unsupported())
    }
}

/// `n_bytes` of OS randomness as lowercase hex (`2 * n_bytes` characters).
///
/// An error when the entropy source cannot be read. A secret is either
/// unguessable or not issued: every caller mints something a stranger must
/// not guess, so there is no weaker token to fall back to — the caller
/// answers "temporarily unavailable" instead. Never a panic: the release
/// profile aborts on panic, and a mint sits on request paths a stranger can
/// reach, so a panic here would let one take the daemon (and every session it
/// holds) down.
pub fn hex_token(n_bytes: usize) -> io::Result<String> {
    let mut buf = vec![0u8; n_bytes];
    fill(&mut buf)?;
    Ok(crate::sha::to_hex(&buf))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_tokens_are_distinct_and_well_formed() {
        let mut seen = std::collections::HashSet::new();
        for _ in 0..10_000 {
            let t = hex_token(32).expect("OS randomness");
            assert_eq!(t.len(), 64, "{t}");
            assert!(
                t.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')),
                "{t}"
            );
            assert!(seen.insert(t.clone()), "duplicate token {t}");
        }
    }

    #[test]
    fn fill_writes_every_byte() {
        // Random data has about 16 zero bytes in 4 KiB; a stub that returns
        // Ok without reading leaves all 4096.
        let mut buf = vec![0u8; 4096];
        fill(&mut buf).expect("OS randomness");
        assert!(buf.iter().filter(|&&b| b == 0).count() < 256, "unfilled");
        let mut empty = [];
        fill(&mut empty).expect("an empty fill is a no-op");
    }

    /// Set in the child [`a_full_fd_table_does_not_stop_a_mint`] spawns.
    #[cfg(unix)]
    const FD_CHILD: &str = "AGENTD_TEST_RANDOM_FD_CHILD";

    /// A mint after the process has run out of file descriptors still
    /// succeeds, because the source was opened at startup and is kept. Run in
    /// a child process: exhausting the fd table here would starve every other
    /// test in this binary.
    #[cfg(unix)]
    #[test]
    fn a_full_fd_table_does_not_stop_a_mint() {
        if std::env::var_os(FD_CHILD).is_some() {
            return exhaust_fds_then_mint();
        }
        let out = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "sec::random::tests::a_full_fd_table_does_not_stop_a_mint",
                "--nocapture",
                "--test-threads=1",
            ])
            .env(FD_CHILD, "1")
            .output()
            .expect("spawn the test binary");
        let log = String::from_utf8_lossy(&out.stdout).into_owned()
            + &String::from_utf8_lossy(&out.stderr);
        assert!(out.status.success(), "the child failed:\n{log}");
        // The filter matched the test, so the child really ran it.
        assert!(log.contains("1 passed"), "the child ran nothing:\n{log}");
    }

    #[cfg(unix)]
    fn exhaust_fds_then_mint() {
        // What the daemon does at startup.
        open().expect("OS randomness");
        // A small ceiling, so exhausting it is quick.
        let lim = libc::rlimit {
            rlim_cur: 64,
            rlim_max: 64,
        };
        // SAFETY: setrlimit reads a valid, initialised struct.
        assert_eq!(unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &lim) }, 0);
        let mut held = Vec::new();
        let err = loop {
            match std::fs::File::open("/dev/null") {
                Ok(f) => held.push(f),
                Err(e) => break e,
            }
        };
        assert_eq!(err.raw_os_error(), Some(libc::EMFILE), "{err}");
        let t = hex_token(32).expect("a mint with no free descriptor");
        assert_eq!(t.len(), 64);
        drop(held);
    }
}
