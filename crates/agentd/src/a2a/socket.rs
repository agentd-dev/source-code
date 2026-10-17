// SPDX-License-Identifier: AGPL-3.0-only
//! Where a unix-socket listener binds. The listener binds a socket inside a
//! staging directory beside its path and renames it into place
//! ([`crate::a2a::serve`]), so the address it binds is longer than the path it
//! publishes; whatever chooses a socket path for a listener to bind sizes it
//! here.

use std::path::{Path, PathBuf};

/// The bytes a socket address holds for its path, the terminating NUL
/// included: `sun_path`, 108 on Linux and 104 on macOS and the BSDs.
#[cfg(unix)]
const SUN_PATH: usize =
    std::mem::size_of::<libc::sockaddr_un>() - std::mem::offset_of!(libc::sockaddr_un, sun_path);
#[cfg(not(unix))]
const SUN_PATH: usize = 108;

/// The widest pid: Linux's `PID_MAX_LIMIT`, 2^22, has seven digits, and no
/// other unix's pid is wider.
const WIDEST_PID: u32 = 9_999_999;

/// The 0700 directory a listener in process `pid` binds the socket for
/// `sock` in, beside it, before renaming it to `sock`.
pub fn stage_dir(sock: &Path, pid: u32) -> PathBuf {
    sock.parent()
        .unwrap_or(Path::new("."))
        .join(format!(".agentd-sock-{pid}"))
}

/// The address a listener in process `pid` binds for `sock`, in its
/// [`stage_dir`].
pub fn staged(sock: &Path, pid: u32) -> PathBuf {
    stage_dir(sock, pid).join("s")
}

/// Whether a listener in any process can bind `sock` and a client can
/// connect to it: the address it is staged at and `sock` itself both fit in
/// `sun_path`.
pub fn fits(sock: &Path) -> bool {
    let longest = sock
        .as_os_str()
        .len()
        .max(staged(sock, WIDEST_PID).as_os_str().len());
    longest < SUN_PATH
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A path fits only when the address the listener stages it at does,
    /// for a pid of any width: the staged address is what the bind is
    /// refused for, a path beside it is not.
    #[test]
    fn a_socket_fits_when_its_staged_address_does() {
        // `a2a.sock` in a directory of `len` bytes.
        let at = |len: usize| PathBuf::from(format!("/{}/a2a.sock", "d".repeat(len - 1)));
        let deepest = SUN_PATH - 1 - "/.agentd-sock-9999999/s".len();
        assert!(fits(&at(deepest)));
        assert!(!fits(&at(deepest + 1)), "a directory one byte too deep");
        // That socket's own path would fit; the address it is staged at
        // does not.
        assert!(at(deepest + 1).as_os_str().len() < SUN_PATH);
        // A name longer than the staging is measured itself.
        let named = |len: usize| PathBuf::from(format!("/d/{}", "n".repeat(len - 3)));
        assert!(fits(&named(SUN_PATH - 1)));
        assert!(!fits(&named(SUN_PATH)));
        assert_eq!(
            staged(Path::new("/run/a/a2a.sock"), 42),
            Path::new("/run/a/.agentd-sock-42/s")
        );
    }
}
