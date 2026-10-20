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

#[cfg(test)]
mod tests {
    use super::*;

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
