// SPDX-License-Identifier: AGPL-3.0-only
//! **An instance child's own channel to its parent** (RFC 0036, RFC 0044
//! §7.P2c): a connected pair of `SOCK_SEQPACKET` unix sockets the parent
//! makes at spawn, one end of which the child is handed as descriptor
//! [`CHANNEL_FD`] and nothing else is.
//!
//! The channel is a door, not a connection: to talk to its parent the child
//! makes a fresh stream socket pair, passes one end through the door
//! (`SCM_RIGHTS`) and speaks HTTP on the other, one request per connection
//! as every A2A dial does ([`connect`]). The parent takes each end it is
//! passed ([`accept`]) and serves it with its own A2A router as the child's
//! principal, `instance:<handle>`, which may send `_instance.result` and
//! `_instance.emit` for that handle and nothing else.
//!
//! Who is calling is settled by construction, not by a credential or a uid:
//! only the holder of the door can open a connection through it, the door
//! exists only in the parent and in the one child it was handed to, and it
//! is close-on-exec in both, so neither's own children inherit it. No path
//! names it, so no other process can reach it, and the child's composed
//! configuration carries no credential of its parent's. That is also what
//! lets a child run as a uid of its own (`spawn.instance_uid`).

#[cfg(unix)]
mod imp {
    use std::io;
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
    use std::os::unix::net::UnixStream;
    use std::sync::OnceLock;
    use std::time::Duration;

    /// The descriptor an instance child is handed its channel on.
    pub const CHANNEL_FD: RawFd = 3;
    /// Set in an instance child's environment when it is handed a channel.
    pub const CHANNEL_ENV: &str = "AGENTD_INSTANCE_CHANNEL";

    /// A connected `SOCK_SEQPACKET` pair, both ends close-on-exec from the
    /// moment they exist: the parent's end, and the one the child is handed.
    pub fn pair() -> io::Result<(OwnedFd, OwnedFd)> {
        socketpair(libc::SOCK_SEQPACKET)
    }

    fn socketpair(kind: libc::c_int) -> io::Result<(OwnedFd, OwnedFd)> {
        let mut fds = [0 as libc::c_int; 2];
        #[cfg(any(target_os = "linux", target_os = "android"))]
        let kind = kind | libc::SOCK_CLOEXEC;
        // SAFETY: a two-element array for socketpair(2) to fill.
        let rc = unsafe { libc::socketpair(libc::AF_UNIX, kind, 0, fds.as_mut_ptr()) };
        if rc != 0 {
            return Err(io::Error::last_os_error());
        }
        #[cfg(not(any(target_os = "linux", target_os = "android")))]
        for fd in fds {
            // SAFETY: a descriptor socketpair(2) just made.
            unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) };
        }
        // SAFETY: two fresh descriptors this call owns.
        Ok(unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) })
    }

    /// Deliver `end` to the child as [`CHANNEL_FD`], and only to it: `end`
    /// is close-on-exec here, and between fork and exec `dup2` puts a copy
    /// at the channel's number without the flag — or, when it already is
    /// that number, the flag is cleared. Registered after
    /// [`crate::signals::pass_only_stdio`], whose marking it then undoes
    /// for this one descriptor.
    pub fn hand_to(cmd: &mut std::process::Command, end: &OwnedFd) {
        use std::os::unix::process::CommandExt;
        let src = end.as_raw_fd();
        cmd.env(CHANNEL_ENV, CHANNEL_FD.to_string());
        // SAFETY: dup2 and fcntl only, between fork and exec.
        unsafe {
            cmd.pre_exec(move || {
                let rc = if src == CHANNEL_FD {
                    libc::fcntl(CHANNEL_FD, libc::F_SETFD, 0)
                } else {
                    libc::dup2(src, CHANNEL_FD)
                };
                if rc < 0 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
    }

    /// The parent's side: the next connection the child opened through the
    /// door, or `None` once the child has closed it (it exited).
    pub fn accept(door: &OwnedFd) -> io::Result<Option<UnixStream>> {
        loop {
            let mut byte = [0u8; 1];
            let mut iov = libc::iovec {
                iov_base: byte.as_mut_ptr().cast(),
                iov_len: 1,
            };
            // Room for one descriptor's control message.
            let mut space = [0u64; 8];
            // SAFETY: a zeroed msghdr pointing at the buffers above.
            let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
            msg.msg_iov = &mut iov;
            msg.msg_iovlen = 1;
            msg.msg_control = space.as_mut_ptr().cast();
            msg.msg_controllen = std::mem::size_of_val(&space) as _;
            #[cfg(any(target_os = "linux", target_os = "android"))]
            let flags = libc::MSG_CMSG_CLOEXEC;
            #[cfg(not(any(target_os = "linux", target_os = "android")))]
            let flags = 0;
            // SAFETY: recvmsg(2) into the msghdr above.
            let n = unsafe { libc::recvmsg(door.as_raw_fd(), &mut msg, flags) };
            if n < 0 {
                let e = io::Error::last_os_error();
                if e.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(e);
            }
            if n == 0 {
                return Ok(None);
            }
            let mut got: Option<OwnedFd> = None;
            // SAFETY: walking the control messages recvmsg(2) filled in.
            unsafe {
                let mut c = libc::CMSG_FIRSTHDR(&msg);
                while !c.is_null() {
                    if (*c).cmsg_level == libc::SOL_SOCKET && (*c).cmsg_type == libc::SCM_RIGHTS {
                        let data = libc::CMSG_DATA(c).cast::<libc::c_int>();
                        let count = ((*c).cmsg_len as usize - libc::CMSG_LEN(0) as usize)
                            / std::mem::size_of::<libc::c_int>();
                        for i in 0..count {
                            let fd = std::ptr::read_unaligned(data.add(i));
                            let owned = OwnedFd::from_raw_fd(fd);
                            #[cfg(not(any(target_os = "linux", target_os = "android")))]
                            libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC);
                            // One descriptor per message; any more are closed.
                            if got.is_none() {
                                got = Some(owned);
                            }
                        }
                    }
                    c = libc::CMSG_NXTHDR(&msg, c);
                }
            }
            // A message that carried no descriptor, or one that is not a
            // stream socket, is not a connection: dropped, and the door
            // waits for the next.
            if let Some(fd) = got
                && socket_type(&fd) == Some(libc::SOCK_STREAM)
            {
                return Ok(Some(UnixStream::from(fd)));
            }
        }
    }

    fn socket_type(fd: &OwnedFd) -> Option<libc::c_int> {
        let mut t: libc::c_int = 0;
        let mut len = std::mem::size_of::<libc::c_int>() as libc::socklen_t;
        // SAFETY: getsockopt(2) of an int into `t`.
        let rc = unsafe {
            libc::getsockopt(
                fd.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_TYPE,
                (&mut t as *mut libc::c_int).cast(),
                &mut len,
            )
        };
        (rc == 0).then_some(t)
    }

    /// The door this process was handed, when it is an instance child that
    /// was: taken once, the first time it is asked for.
    fn door() -> Option<&'static OwnedFd> {
        static DOOR: OnceLock<Option<OwnedFd>> = OnceLock::new();
        DOOR.get_or_init(|| {
            let named = std::env::var(CHANNEL_ENV).ok()?;
            if named.trim() != CHANNEL_FD.to_string()
                || std::env::var_os(crate::supervisor::reap::INSTANCE_CHILD_ENV).is_none()
            {
                return None;
            }
            // SAFETY: the parent placed the channel at this number; it is
            // checked to be a SEQPACKET socket before it is used, and made
            // close-on-exec so no child of this process inherits it.
            let fd = unsafe { OwnedFd::from_raw_fd(CHANNEL_FD) };
            if socket_type(&fd) != Some(libc::SOCK_SEQPACKET) {
                // Not ours to close either: hand the number back untouched.
                std::mem::forget(fd);
                return None;
            }
            // SAFETY: fcntl on a descriptor this process holds.
            unsafe { libc::fcntl(CHANNEL_FD, libc::F_SETFD, libc::FD_CLOEXEC) };
            Some(fd)
        })
        .as_ref()
    }

    /// Take the door now, at start, before anything else could open a
    /// descriptor at its number: whether this process holds one.
    pub fn take_inherited() -> bool {
        door().is_some()
    }

    /// Whether this process holds a channel to its parent.
    pub fn held() -> bool {
        door().is_some()
    }

    /// The child's side: a new connection to its parent, through the door,
    /// with `timeout` on every read and write.
    pub fn connect(timeout: Duration) -> io::Result<UnixStream> {
        let door = door().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotConnected,
                "this process holds no channel to a parent",
            )
        })?;
        let (mine, theirs) = socketpair(libc::SOCK_STREAM)?;
        let byte = [1u8; 1];
        let mut iov = libc::iovec {
            iov_base: byte.as_ptr() as *mut _,
            iov_len: 1,
        };
        let mut space = [0u64; 8];
        // SAFETY: a zeroed msghdr carrying one descriptor in its control
        // message, built with the CMSG_* helpers.
        unsafe {
            let mut msg: libc::msghdr = std::mem::zeroed();
            msg.msg_iov = &mut iov;
            msg.msg_iovlen = 1;
            msg.msg_control = space.as_mut_ptr().cast();
            msg.msg_controllen = libc::CMSG_SPACE(std::mem::size_of::<libc::c_int>() as u32) as _;
            let c = libc::CMSG_FIRSTHDR(&msg);
            (*c).cmsg_level = libc::SOL_SOCKET;
            (*c).cmsg_type = libc::SCM_RIGHTS;
            (*c).cmsg_len = libc::CMSG_LEN(std::mem::size_of::<libc::c_int>() as u32) as _;
            std::ptr::write_unaligned(libc::CMSG_DATA(c).cast::<libc::c_int>(), theirs.as_raw_fd());
            loop {
                let n = libc::sendmsg(door.as_raw_fd(), &msg, libc::MSG_NOSIGNAL);
                if n >= 0 {
                    break;
                }
                let e = io::Error::last_os_error();
                if e.kind() != io::ErrorKind::Interrupted {
                    return Err(e);
                }
            }
        }
        drop(theirs);
        let s = UnixStream::from(mine);
        s.set_read_timeout(Some(timeout))?;
        s.set_write_timeout(Some(timeout))?;
        Ok(s)
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use std::io::{Read, Write};

        /// A connection made through the door arrives at its other end as a
        /// stream both sides can talk over; the door says when its other end
        /// is gone; a message with no descriptor is not a connection.
        #[test]
        fn a_connection_is_passed_through_the_door() {
            let (parent, child) = pair().unwrap();
            // As `connect` does, with `child` as the door.
            let (mine, theirs) = socketpair(libc::SOCK_STREAM).unwrap();
            let byte = [1u8];
            let mut iov = libc::iovec {
                iov_base: byte.as_ptr() as *mut _,
                iov_len: 1,
            };
            let mut space = [0u64; 8];
            // SAFETY: as in `connect`.
            unsafe {
                // First a message with no descriptor: not a connection.
                let mut bare: libc::msghdr = std::mem::zeroed();
                bare.msg_iov = &mut iov;
                bare.msg_iovlen = 1;
                assert_eq!(libc::sendmsg(child.as_raw_fd(), &bare, 0), 1);
                let mut msg: libc::msghdr = std::mem::zeroed();
                msg.msg_iov = &mut iov;
                msg.msg_iovlen = 1;
                msg.msg_control = space.as_mut_ptr().cast();
                msg.msg_controllen =
                    libc::CMSG_SPACE(std::mem::size_of::<libc::c_int>() as u32) as _;
                let c = libc::CMSG_FIRSTHDR(&msg);
                (*c).cmsg_level = libc::SOL_SOCKET;
                (*c).cmsg_type = libc::SCM_RIGHTS;
                (*c).cmsg_len = libc::CMSG_LEN(std::mem::size_of::<libc::c_int>() as u32) as _;
                std::ptr::write_unaligned(
                    libc::CMSG_DATA(c).cast::<libc::c_int>(),
                    theirs.as_raw_fd(),
                );
                assert_eq!(libc::sendmsg(child.as_raw_fd(), &msg, 0), 1);
            }
            drop(theirs);
            let mut served = accept(&parent).unwrap().expect("a connection");
            let mut mine = UnixStream::from(mine);
            mine.write_all(b"ping").unwrap();
            let mut got = [0u8; 4];
            served.read_exact(&mut got).unwrap();
            assert_eq!(&got, b"ping");
            served.write_all(b"pong").unwrap();
            mine.read_exact(&mut got).unwrap();
            assert_eq!(&got, b"pong");
            drop(child);
            assert!(accept(&parent).unwrap().is_none(), "the child is gone");
        }

        /// No channel is held where none was handed: the env names none.
        #[test]
        fn a_process_handed_no_channel_holds_none() {
            if std::env::var_os(CHANNEL_ENV).is_none() {
                assert!(!held());
                assert!(connect(Duration::from_secs(1)).is_err());
            }
        }
    }
}

#[cfg(unix)]
pub use imp::*;
