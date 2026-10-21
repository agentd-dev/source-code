// SPDX-License-Identifier: AGPL-3.0-only
//! Workers under a uid of their own (RFC 0044 §7.P2b, T4): with `spawn.uid`
//! and `spawn.gid` every turn worker, subagent and think child, every
//! instance child's workers and every `exec` command is started as that uid
//! and gid instead of agentd's, with no supplementary group. A process of
//! another uid cannot read agentd's memory or `/proc/<pid>/environ`, open a
//! file only agentd's uid may, or pass the external signer's peer check;
//! Landlock (`sec::landlock`) still denies the vault and the key files on
//! top.
//!
//! **How.** Between `fork` and `exec`, before any other code of the child
//! and before it enters its Landlock domain: `setgroups(0)`, `setgid`,
//! `setuid`, each checked; then every capability dropped — the ambient set
//! cleared, the permitted, effective and inheritable sets emptied and read
//! back empty — and `PR_SET_NO_NEW_PRIVS`, so neither a capability agentd
//! held nor a binary with file capabilities gives the child one: a worker
//! can never `setuid` back to agentd's uid. A child that cannot drop is not
//! started. agentd needs root, or `CAP_SETUID`, `CAP_SETGID` and `CAP_KILL`
//! — the last to signal a worker of another uid at its deadline and its
//! drain — to do it ([`refusal`]); a configuration it cannot honour is
//! refused at load, naming why.
//!
//! **What a worker reads** is the operator's to give the worker uid: the
//! binary it runs, its connections' `{{secret-file:…}}` files, the OAuth
//! cache under `AGENTD_CRED_DIR`, a TLS trust file, and an `exec` command's
//! working directory. No worker reads the supervisor's store, so nothing of
//! it is given; agentd changes the ownership of no file.
//!
//! **An instance child** is a supervisor of its own: it runs as agentd's
//! uid, since it answers its parent over the parent's unix listener as the
//! operator — a worker uid that could do that would make every turn worker
//! the operator — and its composed configuration carries `spawn.uid` and
//! `spawn.gid`, so its own workers drop as these do.

use std::sync::RwLock;

/// The uid and gid every worker is started as, when set.
static WORKER: RwLock<Option<(u32, u32)>> = RwLock::new(None);

/// Set the uid and gid every worker is started as. Called once at start:
/// `spawn.uid` and `spawn.gid` are restart-only.
pub fn set(w: Option<(u32, u32)>) {
    *WORKER.write().unwrap_or_else(|e| e.into_inner()) = w;
}

/// The uid and gid every worker is started as, when set.
pub fn get() -> Option<(u32, u32)> {
    *WORKER.read().unwrap_or_else(|e| e.into_inner())
}

/// The capability bits this process holds in effect, from
/// `/proc/self/status`; `None` where there is no such file.
fn effective_caps() -> Option<u64> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    let hex = status.lines().find_map(|l| l.strip_prefix("CapEff:"))?;
    u64::from_str_radix(hex.trim(), 16).ok()
}

const CAP_KILL: u64 = 1 << 5;
const CAP_SETGID: u64 = 1 << 6;
const CAP_SETUID: u64 = 1 << 7;

/// Why workers cannot run as `uid`:`gid` here, or `None` when they can:
/// agentd is `euid` with the capability bits `caps` in effect, the
/// external signers run as `signers`, and `instances` says whether an
/// instance-tier template is configured. Pure, so every refusal is
/// testable as any uid.
pub fn refusal(
    uid: u32,
    gid: u32,
    euid: u32,
    caps: Option<u64>,
    signers: &[u32],
    instances: bool,
) -> Option<String> {
    if uid == 0 || gid == 0 {
        return Some("workers never run as root (uid 0 or gid 0)".into());
    }
    if uid == euid {
        return Some(format!(
            "{uid} is agentd's own uid: workers would not be separated from it"
        ));
    }
    if signers.contains(&uid) {
        return Some(format!(
            "{uid} is an external signer's uid: a worker would pass the signer's peer check"
        ));
    }
    if euid == 0 {
        return None;
    }
    let held = caps.unwrap_or(0);
    if held & CAP_SETUID == 0 || held & CAP_SETGID == 0 {
        return Some(format!(
            "agentd runs as uid {euid} without CAP_SETUID and CAP_SETGID, so it cannot start a worker as another uid — run it as root, or grant it CAP_SETUID, CAP_SETGID and CAP_KILL"
        ));
    }
    if held & CAP_KILL == 0 {
        return Some(format!(
            "agentd runs as uid {euid} without CAP_KILL, so it could not signal a worker of another uid at its deadline or its drain — run it as root, or grant it CAP_KILL too"
        ));
    }
    if instances {
        return Some(format!(
            "agentd runs as uid {euid}, and an instance child it starts holds none of its capabilities (no_new_privs), so it could not start its own workers as uid {uid} — run agentd as root to use instance-tier templates with spawn.uid"
        ));
    }
    None
}

/// Why instance children cannot run as `uid`:`gid` here, or `None` when
/// they can: agentd is `euid`, the external signers run as `signers`, and
/// workers as `workers` (`spawn.uid`, `spawn.gid`). Root alone can, since
/// the child's directory is made its own as well as the child started as
/// another uid; and never as a worker's uid, whose processes could then
/// read the child's store and reach its unix listener as its operator.
pub fn instance_refusal(
    uid: u32,
    gid: u32,
    euid: u32,
    signers: &[u32],
    workers: Option<(u32, u32)>,
) -> Option<String> {
    if uid == 0 || gid == 0 {
        return Some("instance children never run as root (uid 0 or gid 0)".into());
    }
    if uid == euid {
        return Some(format!(
            "{uid} is agentd's own uid: the child would not be separated from it"
        ));
    }
    if signers.contains(&uid) {
        return Some(format!(
            "{uid} is an external signer's uid: the child would pass the signer's peer check"
        ));
    }
    if workers.is_some_and(|(w, _)| w == uid) {
        return Some(format!(
            "{uid} is spawn.uid, the workers' uid: a worker could read the child's store and reach its listener as its operator"
        ));
    }
    if euid != 0 {
        return Some(format!(
            "agentd runs as uid {euid}: starting a child as another uid and making its directory its own needs root"
        ));
    }
    None
}

/// [`instance_refusal`] for this process.
pub fn instance_refusal_here(
    uid: u32,
    gid: u32,
    signers: &[u32],
    workers: Option<(u32, u32)>,
) -> Option<String> {
    // SAFETY: geteuid has no failure mode.
    let euid = unsafe { libc::geteuid() };
    instance_refusal(uid, gid, euid, signers, workers)
}

/// Start an instance child's `cmd` as `uid` and `gid`, holding no
/// capability, before its Landlock domain — as a worker is started.
pub fn start_instance_as(cmd: &mut std::process::Command, ids: (u32, u32)) {
    start_as(cmd, ids);
}

/// [`refusal`] for this process.
pub fn refusal_here(uid: u32, gid: u32, signers: &[u32], instances: bool) -> Option<String> {
    // SAFETY: geteuid has no failure mode.
    let euid = unsafe { libc::geteuid() };
    refusal(uid, gid, euid, effective_caps(), signers, instances)
}

/// Start `cmd` as the worker uid and gid, when they are set: registered
/// before the Landlock domain's own step, so the child drops first.
pub fn confine(cmd: &mut std::process::Command) -> Option<(u32, u32)> {
    let w = get()?;
    start_as(cmd, w);
    Some(w)
}

/// Start `cmd` as `uid` and `gid`, holding no capability.
fn start_as(cmd: &mut std::process::Command, (uid, gid): (u32, u32)) {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // SAFETY: only async-signal-safe calls between fork and exec
        // (setgroups, setgid, setuid, getuid, getgid, prctl, capset, capget).
        unsafe {
            cmd.pre_exec(move || {
                if libc::setgroups(0, std::ptr::null()) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                if libc::setgid(gid as libc::gid_t) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                if libc::setuid(uid as libc::uid_t) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                if libc::getuid() != uid
                    || libc::geteuid() != uid
                    || libc::getgid() != gid
                    || libc::getegid() != gid
                {
                    return Err(std::io::Error::from_raw_os_error(libc::EPERM));
                }
                #[cfg(target_os = "linux")]
                {
                    caps::drop_all()?;
                    if libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                }
                Ok(())
            });
        }
    }
}

/// The calling thread's capability sets, by `capget(2)` and `capset(2)`
/// at version 3 (64 bits each).
#[cfg(target_os = "linux")]
mod caps {
    const VERSION_3: u32 = 0x2008_0522;

    #[repr(C)]
    struct Header {
        version: u32,
        pid: libc::c_int,
    }

    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    struct Data {
        effective: u32,
        permitted: u32,
        inheritable: u32,
    }

    /// The effective, permitted and inheritable sets.
    pub(super) fn get() -> std::io::Result<[u64; 3]> {
        let mut h = Header {
            version: VERSION_3,
            pid: 0,
        };
        let mut d = [Data::default(); 2];
        // SAFETY: a version-3 header and two data words, as capget(2) asks.
        let r = unsafe { libc::syscall(libc::SYS_capget, &mut h as *mut Header, d.as_mut_ptr()) };
        if r != 0 {
            return Err(std::io::Error::last_os_error());
        }
        let wide = |lo: u32, hi: u32| u64::from(lo) | (u64::from(hi) << 32);
        Ok([
            wide(d[0].effective, d[1].effective),
            wide(d[0].permitted, d[1].permitted),
            wide(d[0].inheritable, d[1].inheritable),
        ])
    }

    /// Set the effective, permitted and inheritable sets to `sets`.
    pub(super) fn set(sets: [u64; 3]) -> std::io::Result<()> {
        let mut h = Header {
            version: VERSION_3,
            pid: 0,
        };
        let word = |i: usize| Data {
            effective: (sets[0] >> (32 * i)) as u32,
            permitted: (sets[1] >> (32 * i)) as u32,
            inheritable: (sets[2] >> (32 * i)) as u32,
        };
        let d = [word(0), word(1)];
        // SAFETY: a version-3 header and two data words, as capset(2) asks.
        let r = unsafe { libc::syscall(libc::SYS_capset, &mut h as *mut Header, d.as_ptr()) };
        if r != 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(())
    }

    /// Hold no capability: the ambient set cleared, the other three emptied,
    /// and read back empty. Async-signal-safe: system calls only.
    pub(super) fn drop_all() -> std::io::Result<()> {
        // A kernel before 4.3 has no ambient set, so nothing to clear.
        // SAFETY: prctl with integer arguments.
        if unsafe {
            libc::prctl(
                libc::PR_CAP_AMBIENT,
                libc::PR_CAP_AMBIENT_CLEAR_ALL as libc::c_ulong,
                0,
                0,
                0,
            )
        } != 0
        {
            let e = std::io::Error::last_os_error();
            if e.raw_os_error() != Some(libc::EINVAL) {
                return Err(e);
            }
        }
        // Lowering a set needs no privilege.
        set([0; 3])?;
        if get()? != [0; 3] {
            return Err(std::io::Error::from_raw_os_error(libc::EPERM));
        }
        Ok(())
    }
}

/// Who makes a supervisor-only path, as [`unopenable`] judges it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Made {
    /// The operator: it is judged as it is, and must exist (a key file,
    /// the external signer's socket).
    Operator,
    /// agentd, owner-only, when it is absent (the audit chain, `0600` in a
    /// `0700` directory).
    WhenAbsent,
    /// agentd, which restricts it to its owner at every start (the file
    /// store's root, `0700`) — where it can: one agentd's own uid does not
    /// own is judged as it is, since that restriction may fail unsaid.
    Restricted,
}

/// What a process of `uid`:`gid`, in no other group, may do to an entry of
/// mode `mode` owned by `owner`:`group`: read, write, and search or run.
pub fn access(mode: u32, owner: u32, group: u32, uid: u32, gid: u32) -> (bool, bool, bool) {
    let bits = if owner == uid {
        (mode >> 6) & 7
    } else if group == gid {
        (mode >> 3) & 7
    } else {
        mode & 7
    };
    (bits & 4 != 0, bits & 2 != 0, bits & 1 != 0)
}

/// Whether `path` carries a POSIX access ACL, which a mode does not say.
#[cfg(target_os = "linux")]
fn has_acl(path: &std::path::Path) -> bool {
    use std::os::unix::ffi::OsStrExt;
    let Ok(p) = std::ffi::CString::new(path.as_os_str().as_bytes()) else {
        return true;
    };
    // SAFETY: a NUL-terminated path and attribute name, and no buffer: the
    // size alone is asked.
    let n = unsafe {
        libc::getxattr(
            p.as_ptr(),
            c"system.posix_acl_access".as_ptr(),
            std::ptr::null_mut(),
            0,
        )
    };
    n >= 0
}

#[cfg(not(target_os = "linux"))]
fn has_acl(_: &std::path::Path) -> bool {
    false
}

/// Why a process of `uid`:`gid` — a worker, in no supplementary group —
/// could open supervisor-only `path`, judged by the mode and ownership of
/// it and of every directory above it (the user's decision of 2026-10-20,
/// RFC 0044 T4); `Ok` when it could not. A directory above that the uid
/// cannot search closes everything below; one it could write, and so empty
/// and refill, without the sticky bit, opens it; anything the uid owns, or
/// that carries a POSIX ACL, it is not judged safe — an owner may change a
/// mode, and an ACL is not read. A directory is open when the uid may
/// search it, a socket when it may write it, anything else when it may read
/// or write it. `made` says who makes it.
pub fn unopenable(path: &std::path::Path, uid: u32, gid: u32, made: Made) -> Result<(), String> {
    use std::os::unix::fs::{FileTypeExt, MetadataExt};
    let mut nearest = path.to_path_buf();
    while std::fs::symlink_metadata(&nearest).is_err() {
        if !nearest.pop() {
            return Err(format!("{} names nothing on this host", path.display()));
        }
    }
    let exists = nearest.as_path() == path;
    if !exists && made == Made::Operator {
        return Err(format!(
            "{} does not exist, so its mode cannot be judged",
            path.display()
        ));
    }
    let real = nearest
        .canonicalize()
        .map_err(|e| format!("{}: {e}", nearest.display()))?;
    let mut dirs: Vec<&std::path::Path> = real.ancestors().collect();
    dirs.reverse();
    for (i, d) in dirs.iter().enumerate() {
        let m = std::fs::metadata(d).map_err(|e| format!("{}: {e}", d.display()))?;
        if m.uid() == uid {
            return Err(format!(
                "{} is owned by the worker uid {uid}, which may change its mode",
                d.display()
            ));
        }
        if has_acl(d) {
            return Err(format!(
                "{} carries a POSIX ACL, which this check does not read",
                d.display()
            ));
        }
        // SAFETY: geteuid has no failure mode.
        let restricted = made == Made::Restricted && m.uid() == unsafe { libc::geteuid() };
        let mode = match (exists && i + 1 == dirs.len(), restricted) {
            (true, true) => m.mode() & 0o700,
            _ => m.mode(),
        };
        let (r, w, x) = access(mode, m.uid(), m.gid(), uid, gid);
        if exists && i + 1 == dirs.len() {
            let t = m.file_type();
            let open = if t.is_dir() {
                x.then_some("enter")
            } else if t.is_socket() {
                w.then_some("connect to")
            } else {
                (r || w).then_some("read or write")
            };
            return match open {
                Some(how) => Err(format!(
                    "the worker uid {uid} can {how} {} (mode {:o})",
                    d.display(),
                    m.mode() & 0o7777
                )),
                None => Ok(()),
            };
        }
        if w && m.mode() & 0o1000 == 0 {
            return Err(format!(
                "the worker uid {uid} can write {}, and so replace what is in it",
                d.display()
            ));
        }
        if !x {
            return Ok(());
        }
    }
    // Absent, beneath directories the uid can search: agentd makes it,
    // owner-only.
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What a uid may do follows the owner bits for its owner, the group
    /// bits for its group alone, and the other bits for anyone else.
    #[test]
    fn access_follows_the_bits_that_apply() {
        assert_eq!(access(0o740, 10, 20, 10, 99), (true, true, true));
        assert_eq!(access(0o640, 10, 20, 10, 99), (true, true, false));
        assert_eq!(access(0o740, 10, 20, 11, 20), (true, false, false));
        assert_eq!(access(0o741, 10, 20, 11, 21), (false, false, true));
        assert_eq!(access(0o700, 10, 20, 11, 21), (false, false, false));
    }

    /// A supervisor-only path is closed to the worker uid by a directory
    /// above it the uid cannot search, or by its own mode; open where the
    /// uid can read it, enter it, or write a directory above it; a path the
    /// operator makes must exist, one agentd makes need not; a store root
    /// is judged owner-only, as agentd makes it at every start, where its
    /// own uid owns it, and as it is where not.
    #[test]
    fn a_path_is_closed_to_the_worker_uid_by_its_mode_and_its_directories() {
        use std::os::unix::fs::PermissionsExt;
        let set = |p: &std::path::Path, m: u32| {
            std::fs::set_permissions(p, std::fs::Permissions::from_mode(m)).unwrap()
        };
        let base = std::env::temp_dir().join(format!(
            "agentd-unopenable-{}-{}",
            std::process::id(),
            crate::state::now_ms()
        ));
        std::fs::create_dir_all(&base).unwrap();
        set(&base, 0o755);
        // SAFETY: getuid has no failure mode.
        let me = unsafe { libc::getuid() };
        // Never the uid running the test, nor root.
        let (uid, gid) = if me == 65534 {
            (65533, 65533)
        } else {
            (65534, 65534)
        };
        let key = base.join("key.pem");
        std::fs::write(&key, "k").unwrap();
        set(&key, 0o644);
        let err = unopenable(&key, uid, gid, Made::Operator).unwrap_err();
        assert!(err.contains("can read or write"), "{err}");
        set(&key, 0o600);
        assert_eq!(unopenable(&key, uid, gid, Made::Operator), Ok(()));
        // A closed directory above closes what is beneath it.
        let closed = base.join("closed");
        std::fs::create_dir(&closed).unwrap();
        let inside = closed.join("seal");
        std::fs::write(&inside, "s").unwrap();
        set(&inside, 0o644);
        set(&closed, 0o700);
        assert_eq!(unopenable(&inside, uid, gid, Made::Operator), Ok(()));
        // A directory the uid can write lets it swap what is in it.
        set(&closed, 0o777);
        let err = unopenable(&inside, uid, gid, Made::Operator).unwrap_err();
        assert!(err.contains("and so replace"), "{err}");
        set(&closed, 0o711);
        let err = unopenable(&closed, uid, gid, Made::Operator).unwrap_err();
        assert!(err.contains("can enter"), "{err}");
        // agentd makes the store root owner-only at every start…
        assert_eq!(unopenable(&closed, uid, gid, Made::Restricted), Ok(()));
        // …where it can: a root its own uid does not own is judged as it
        // is, for the restriction could fail unsaid.
        if me == 0 {
            let other = if uid == 65533 { 65532 } else { 65533 };
            std::os::unix::fs::chown(&closed, Some(other), Some(other)).unwrap();
            let err = unopenable(&closed, uid, gid, Made::Restricted).unwrap_err();
            assert!(err.contains("can enter"), "{err}");
            std::os::unix::fs::chown(&closed, Some(0), Some(0)).unwrap();
        } else {
            use std::os::unix::fs::MetadataExt;
            let root = std::path::Path::new("/");
            if std::fs::metadata(root).unwrap().mode() & 0o001 != 0 {
                let err = unopenable(root, uid, gid, Made::Restricted).unwrap_err();
                assert!(err.contains("can enter"), "{err}");
            }
        }
        // Absent: the operator's must exist; agentd's is made owner-only.
        let absent = base.join("absent").join("audit.log");
        let err = unopenable(&absent, uid, gid, Made::Operator).unwrap_err();
        assert!(err.contains("does not exist"), "{err}");
        assert_eq!(unopenable(&absent, uid, gid, Made::WhenAbsent), Ok(()));
        // Anything the worker uid owns is not judged safe.
        let err = unopenable(&key, me, me, Made::Operator).unwrap_err();
        assert!(err.contains("owned by the worker uid"), "{err}");
        set(&closed, 0o700);
        let _ = std::fs::remove_dir_all(&base);
    }

    /// An instance child's own uid is never root's, agentd's own, a
    /// signer's or the workers', and needs agentd to run as root to make the
    /// child's directory its own: each refusal names why.
    #[test]
    fn an_instance_uid_is_held_to_its_rules() {
        assert_eq!(
            instance_refusal(1500, 1500, 0, &[991], Some((1600, 1600))),
            None
        );
        for (r, says) in [
            (instance_refusal(0, 1500, 0, &[], None), "never run as root"),
            (instance_refusal(1500, 0, 0, &[], None), "never run as root"),
            (
                instance_refusal(1000, 1000, 1000, &[], None),
                "agentd's own uid",
            ),
            (
                instance_refusal(991, 991, 0, &[991], None),
                "external signer's uid",
            ),
            (
                instance_refusal(1600, 1600, 0, &[], Some((1600, 1600))),
                "spawn.uid",
            ),
            (instance_refusal(1500, 1500, 1000, &[], None), "needs root"),
        ] {
            assert!(
                r.as_deref().is_some_and(|r| r.contains(says)),
                "{says}: {r:?}"
            );
        }
    }

    /// Each reason a configuration cannot be honoured is named, as any uid:
    /// root and a uid holding `CAP_SETUID`, `CAP_SETGID` and `CAP_KILL`
    /// can; an instance-tier
    /// template needs root; no worker is root, agentd's own uid or a
    /// signer's.
    #[test]
    fn workers_run_as_another_uid_only_where_agentd_can_drop_to_it() {
        let both = Some(CAP_SETUID | CAP_SETGID | CAP_KILL);
        assert_eq!(refusal(65534, 65534, 0, Some(0), &[], true), None);
        assert_eq!(refusal(65534, 65534, 1000, both, &[], false), None);
        for (r, says) in [
            (
                refusal(65534, 65534, 1000, Some(0), &[], false),
                "without CAP_SETUID and CAP_SETGID",
            ),
            (
                refusal(65534, 65534, 1000, Some(CAP_SETUID), &[], false),
                "without CAP_SETUID and CAP_SETGID",
            ),
            (
                refusal(65534, 65534, 1000, None, &[], false),
                "without CAP_SETUID and CAP_SETGID",
            ),
            (
                refusal(
                    65534,
                    65534,
                    1000,
                    Some(CAP_SETUID | CAP_SETGID),
                    &[],
                    false,
                ),
                "without CAP_KILL",
            ),
            (
                refusal(65534, 65534, 1000, both, &[], true),
                "instance-tier templates",
            ),
            (
                refusal(0, 65534, 0, Some(0), &[], false),
                "never run as root",
            ),
            (
                refusal(65534, 0, 0, Some(0), &[], false),
                "never run as root",
            ),
            (
                refusal(1000, 1000, 1000, both, &[], false),
                "agentd's own uid",
            ),
            (
                refusal(991, 991, 0, Some(0), &[991], false),
                "an external signer's uid",
            ),
        ] {
            assert!(
                r.as_deref().is_some_and(|r| r.contains(says)),
                "{r:?} for {says}"
            );
        }
    }

    /// The capability lines of `/proc/self/status` a child prints, and its
    /// uid, gid and groups first.
    #[cfg(target_os = "linux")]
    const SHOW: &str = "id -u; id -g; id -G; grep -E '^Cap(Inh|Prm|Eff|Amb):' /proc/self/status";

    /// What a dropped child must print of [`SHOW`]: the worker uid and gid,
    /// no other group, and no capability in any set.
    #[cfg(target_os = "linux")]
    fn dropped(text: &str) {
        let lines: Vec<String> = text
            .lines()
            .map(|l| l.split_whitespace().collect::<Vec<_>>().join(" "))
            .collect();
        assert_eq!(
            lines,
            [
                "65534",
                "65534",
                "65534",
                "CapInh: 0000000000000000",
                "CapPrm: 0000000000000000",
                "CapEff: 0000000000000000",
                "CapAmb: 0000000000000000",
            ],
            "{text}"
        );
    }

    /// A child started through [`confine`] runs as the uid and gid set,
    /// with no supplementary group and no capability — run only as root,
    /// which can drop.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_child_runs_as_the_worker_uid() {
        // SAFETY: geteuid has no failure mode.
        if unsafe { libc::geteuid() } != 0 {
            eprintln!("skipped: dropping to another uid needs root");
            return;
        }
        let mut cmd = std::process::Command::new("sh");
        cmd.args(["-c", SHOW]);
        start_as(&mut cmd, (65534, 65534));
        let out = cmd.output().expect("run sh");
        dropped(&String::from_utf8_lossy(&out.stdout));
    }

    /// Where the kernel keeps capabilities across `setuid` — a non-root
    /// agentd holding `CAP_SETUID` and `CAP_SETGID`, ambient or by file
    /// capabilities, which the change of uid never clears — the child still
    /// holds none, so it can never `setuid` back. Shown as root by keeping
    /// them as such an agentd does: `SECBIT_NO_SETUID_FIXUP`, and both in
    /// the ambient set, which survives `exec`.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_child_keeps_no_capability_the_supervisor_held() {
        // SAFETY: geteuid has no failure mode.
        if unsafe { libc::geteuid() } != 0 {
            eprintln!("skipped: dropping to another uid needs root");
            return;
        }
        const SECBIT_NO_SETUID_FIXUP: libc::c_ulong = 1 << 2;
        use std::os::unix::process::CommandExt;
        let mut cmd = std::process::Command::new("sh");
        cmd.args(["-c", SHOW]);
        // SAFETY: system calls only, between fork and exec.
        unsafe {
            cmd.pre_exec(|| {
                if libc::prctl(libc::PR_SET_SECUREBITS, SECBIT_NO_SETUID_FIXUP, 0, 0, 0) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                let [e, p, i] = caps::get()?;
                caps::set([e, p, i | CAP_SETUID | CAP_SETGID])?;
                for cap in [7, 6] {
                    if libc::prctl(
                        libc::PR_CAP_AMBIENT,
                        libc::PR_CAP_AMBIENT_RAISE as libc::c_ulong,
                        cap,
                        0,
                        0,
                    ) != 0
                    {
                        return Err(std::io::Error::last_os_error());
                    }
                }
                Ok(())
            });
        }
        start_as(&mut cmd, (65534, 65534));
        let out = cmd.output().expect("run sh");
        dropped(&String::from_utf8_lossy(&out.stdout));
    }
}
