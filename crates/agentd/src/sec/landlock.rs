// SPDX-License-Identifier: AGPL-3.0-only
//! Workers that cannot open the vault or a key file (RFC 0044 P2a,
//! invariant 1, T4): a Landlock ruleset every child is started under, where
//! the configuration holds a supervisor-only key. Raw syscalls through
//! `libc` — no crate.
//!
//! **What is denied.** The protected paths ([`set_protected`]): the file
//! store's root, where the grant vault and the tombstone register are kept;
//! every `{{secret-file:…}}` a supervisor-only secret is read from (the IdP
//! and resource client keys, the seal keys, the companies preview's seal
//! key); and the audit chain with its closed segments. A child can read,
//! write, run, truncate, create in, remove from, link or rename into none of
//! them — and neither can anything it starts, since a Landlock domain is
//! inherited and never lifted.
//!
//! **How.** Landlock grants, it does not deny: a ruleset handles a set of
//! rights, and only paths beneath a rule get any of them. So the ruleset,
//! built in the supervisor before each spawn, grants every right it handles
//! beneath each entry of every directory on the way to a protected path —
//! except that path itself and the directories on the way, and nothing
//! beneath a protected directory, even where another protected path (a key
//! file, the chain) lies inside it. The effect is
//! the whole filesystem, less the protected paths and less creating or
//! removing an entry directly in a directory on the way to one (such an
//! entry made after the spawn is outside every rule, and so denied too).
//! Listing a directory is not handled, so names stay visible; contents do
//! not. The child enters the domain between `fork` and `exec`
//! (`PR_SET_NO_NEW_PRIVS`, then `landlock_restrict_self`), before it runs
//! any code of its own; a child that cannot enter it is not started.
//!
//! **What each ABI handles.** ABI 1 every file right but listing (and
//! renames across directories are then refused outright); ABI 2 adds
//! linking and renaming across directories (`REFER`); ABI 3 truncation by
//! path (`TRUNCATE` — before it a child can still `truncate(2)` a protected
//! file it cannot open, which the audit chain detects and the vault's
//! sealing turns into a record that does not open); ABI 5 device ioctls.
//! Unix-socket `connect(2)` is no file right in any of them: the external
//! signer's socket stays reachable, and its own peer check is what guards
//! it.
//!
//! **Where the kernel has no Landlock** — older than 5.13, built without it,
//! or a seccomp profile that refuses the syscalls — [`abi`] is `None`,
//! children start unconfined, and the runtime says so at start
//! (`proc.landlock.unavailable`): the RFC's P1 residual, that a
//! same-uid worker can read a mounted key file and the vault, holds there.

use std::path::{Path, PathBuf};
use std::sync::RwLock;

/// The syscall numbers: the same on every architecture, as the calls came
/// after the tables were unified.
#[cfg(target_os = "linux")]
mod sys {
    pub const CREATE_RULESET: libc::c_long = 444;
    pub const ADD_RULE: libc::c_long = 445;
    pub const RESTRICT_SELF: libc::c_long = 446;
    pub const CREATE_RULESET_VERSION: u32 = 1;
    pub const RULE_PATH_BENEATH: u32 = 1;

    pub const EXECUTE: u64 = 1 << 0;
    pub const WRITE_FILE: u64 = 1 << 1;
    pub const READ_FILE: u64 = 1 << 2;
    pub const REFER: u64 = 1 << 13;
    pub const TRUNCATE: u64 = 1 << 14;
    pub const IOCTL_DEV: u64 = 1 << 15;
    /// Every ABI 1 right: bits 0–12, less `READ_DIR` (bit 3), which is never
    /// handled.
    pub const ABI1: u64 = ((1 << 13) - 1) & !(1 << 3);
    /// The rights a rule on a file (not a directory) may carry.
    pub const FILE: u64 = EXECUTE | WRITE_FILE | READ_FILE | TRUNCATE | IOCTL_DEV;

    #[repr(C)]
    pub struct RulesetAttr {
        pub handled_access_fs: u64,
    }

    #[repr(C, packed)]
    pub struct PathBeneath {
        pub allowed_access: u64,
        pub parent_fd: i32,
    }
}

/// A protected path, and whether its siblings named `<path>.*` are too
/// (the audit chain's closed segments).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Protected {
    pub path: PathBuf,
    pub with_suffixes: bool,
}

static PROTECTED: RwLock<Vec<Protected>> = RwLock::new(Vec::new());

/// Set what every child is denied; empty, children are started as they are.
/// Called once at start: every protected path is restart-only.
pub fn set_protected(paths: Vec<Protected>) {
    *PROTECTED.write().unwrap_or_else(|e| e.into_inner()) = paths;
}

/// What every child is denied.
pub fn protected() -> Vec<Protected> {
    PROTECTED.read().unwrap_or_else(|e| e.into_inner()).clone()
}

/// The kernel's Landlock ABI version, or `None` when it has none.
pub fn abi() -> Option<u32> {
    #[cfg(target_os = "linux")]
    {
        // SAFETY: the version query takes no attribute.
        let v = unsafe {
            libc::syscall(
                sys::CREATE_RULESET,
                std::ptr::null::<sys::RulesetAttr>(),
                0usize,
                sys::CREATE_RULESET_VERSION,
            )
        };
        (v > 0).then_some(v as u32)
    }
    #[cfg(not(target_os = "linux"))]
    {
        None
    }
}

/// The rights a ruleset under `abi` handles.
#[cfg(target_os = "linux")]
fn handled(abi: u32) -> u64 {
    let mut h = sys::ABI1;
    if abi >= 2 {
        h |= sys::REFER;
    }
    if abi >= 3 {
        h |= sys::TRUNCATE;
    }
    if abi >= 5 {
        h |= sys::IOCTL_DEV;
    }
    h
}

/// `p` absolute and with its symlinks resolved as far as it exists; the
/// rest as written.
fn resolved(p: &Path) -> PathBuf {
    let mut existing = p.to_path_buf();
    let mut rest = Vec::new();
    loop {
        if let Ok(c) = std::fs::canonicalize(&existing) {
            let mut out = c;
            for r in rest.iter().rev() {
                out.push(r);
            }
            return out;
        }
        match (
            existing.file_name().map(|n| n.to_os_string()),
            existing.parent(),
        ) {
            (Some(name), Some(parent)) => {
                rest.push(name);
                existing = parent.to_path_buf();
            }
            _ => return p.to_path_buf(),
        }
    }
}

/// Each directory on the way to a protected path, and what to grant
/// beneath: every entry of those directories that is neither protected nor
/// on the way to a protected path. A protected path inside another — a key
/// file or the audit chain under the file store's root — puts no directory
/// on the way that the outer one denies: nothing beneath a protected
/// directory is granted.
fn grants(protected: &[Protected]) -> Vec<PathBuf> {
    let denied: Vec<Protected> = protected
        .iter()
        .map(|p| Protected {
            path: resolved(&p.path),
            with_suffixes: p.with_suffixes,
        })
        .filter(|p| p.path.parent().is_some())
        .collect();
    let mut ways: Vec<PathBuf> = Vec::new();
    for p in &denied {
        let mut a = p.path.parent();
        while let Some(dir) = a {
            if !ways.iter().any(|w| w == dir) {
                ways.push(dir.to_path_buf());
            }
            a = dir.parent();
        }
    }
    ways.retain(|w| !denied.iter().any(|d| w.starts_with(&d.path)));
    let is_denied = |e: &Path| {
        denied.iter().any(|p| {
            e == p.path
                || (p.with_suffixes
                    && e.parent() == p.path.parent()
                    && e.file_name()
                        .and_then(|n| n.to_str())
                        .zip(p.path.file_name().and_then(|n| n.to_str()))
                        .is_some_and(|(n, base)| {
                            n.strip_prefix(base).is_some_and(|r| r.starts_with('.'))
                        }))
        })
    };
    let mut out = Vec::new();
    for dir in &ways {
        let Ok(entries) = std::fs::read_dir(dir) else {
            continue;
        };
        for e in entries.flatten() {
            let path = e.path();
            if is_denied(&path) || ways.contains(&path) {
                continue;
            }
            out.push(path);
        }
    }
    out
}

/// Start `cmd` in the Landlock domain that denies the protected paths, when
/// there are any and the kernel has Landlock. What was applied is returned
/// — `{abi, denied}` — or `null` when nothing was; an error when the
/// ruleset could not be built, and the child is then not started.
pub fn confine(cmd: &mut std::process::Command) -> std::io::Result<serde_json::Value> {
    confine_beside(cmd, &[])
}

/// [`confine`], the child also granted every right beneath each directory of
/// `own` — one of its own inside a protected one, as an instance's directory
/// is inside the file store's root. A directory of `own` that is a
/// protected path, or holds one, is granted nothing.
pub fn confine_beside(
    cmd: &mut std::process::Command,
    own: &[PathBuf],
) -> std::io::Result<serde_json::Value> {
    let protected = protected();
    if protected.is_empty() {
        return Ok(serde_json::Value::Null);
    }
    #[cfg(target_os = "linux")]
    {
        use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
        use std::os::unix::process::CommandExt;
        let Some(abi) = abi() else {
            return Ok(serde_json::Value::Null);
        };
        let handled = handled(abi);
        let attr = sys::RulesetAttr {
            handled_access_fs: handled,
        };
        // SAFETY: a valid attribute of the size given.
        let fd = unsafe {
            libc::syscall(
                sys::CREATE_RULESET,
                &attr as *const sys::RulesetAttr,
                std::mem::size_of::<sys::RulesetAttr>(),
                0u32,
            )
        };
        if fd < 0 {
            return Err(std::io::Error::other(format!(
                "landlock_create_ruleset: {}",
                std::io::Error::last_os_error()
            )));
        }
        // SAFETY: a descriptor the kernel just returned, owned here alone.
        let ruleset = unsafe { OwnedFd::from_raw_fd(fd as i32) };
        let denied: Vec<PathBuf> = protected.iter().map(|p| resolved(&p.path)).collect();
        let own = own
            .iter()
            .map(|o| resolved(o))
            .filter(|o| o.exists() && !denied.iter().any(|d| d.starts_with(o)));
        for path in grants(&protected).into_iter().chain(own) {
            let Ok(c) = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()) else {
                continue;
            };
            // SAFETY: a NUL-terminated path; the descriptor is owned below.
            let raw = unsafe {
                libc::open(
                    c.as_ptr(),
                    libc::O_PATH | libc::O_CLOEXEC | libc::O_NOFOLLOW,
                )
            };
            if raw < 0 {
                continue;
            }
            // SAFETY: as above.
            let at = unsafe { OwnedFd::from_raw_fd(raw) };
            // SAFETY: an all-zero stat is a valid out-parameter.
            let mut st: libc::stat = unsafe { std::mem::zeroed() };
            // SAFETY: a valid descriptor and a pointer to a stat.
            if unsafe { libc::fstat(at.as_raw_fd(), &mut st) } != 0 {
                continue;
            }
            let kind = st.st_mode & libc::S_IFMT;
            if kind == libc::S_IFLNK {
                // What a link names is held to its own place.
                continue;
            }
            let allowed = if kind == libc::S_IFDIR {
                handled
            } else {
                handled & sys::FILE
            };
            let rule = sys::PathBeneath {
                allowed_access: allowed,
                parent_fd: at.as_raw_fd(),
            };
            // SAFETY: a valid ruleset, rule type and rule.
            let r = unsafe {
                libc::syscall(
                    sys::ADD_RULE,
                    ruleset.as_raw_fd(),
                    sys::RULE_PATH_BENEATH,
                    &rule as *const sys::PathBeneath,
                    0u32,
                )
            };
            if r != 0 {
                return Err(std::io::Error::other(format!(
                    "landlock_add_rule {}: {}",
                    path.display(),
                    std::io::Error::last_os_error()
                )));
            }
        }
        // SAFETY: between fork and exec the closure makes two raw syscalls
        // on a descriptor it owns and touches no heap; the ruleset closes in
        // the supervisor when `cmd` is dropped.
        unsafe {
            cmd.pre_exec(move || {
                if libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                if libc::syscall(sys::RESTRICT_SELF, ruleset.as_raw_fd(), 0u32) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        Ok(serde_json::json!({
            "abi": abi,
            "denied": protected.iter().map(|p| p.path.display().to_string()).collect::<Vec<_>>(),
        }))
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (cmd, own);
        Ok(serde_json::Value::Null)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The protected set is the process's: one test sets it at a time.
    static ONE: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Run `sh -c script` under a domain denying `protected`, with
    /// [`confine`]'s own ruleset; `None` when this kernel has no Landlock.
    fn under(protected: Vec<Protected>, script: &str) -> Option<std::process::Output> {
        abi()?;
        let _held = ONE.lock().unwrap_or_else(|e| e.into_inner());
        set_protected(protected);
        let mut cmd = std::process::Command::new("sh");
        cmd.args(["-c", script]);
        let applied = confine(&mut cmd).unwrap();
        set_protected(Vec::new());
        assert!(applied["abi"].as_u64().is_some(), "{applied}");
        Some(cmd.output().unwrap())
    }

    /// A child under the domain cannot read, write or replace a protected
    /// directory's files or a protected key file, and reads its siblings
    /// as before.
    #[test]
    fn a_child_is_denied_the_vault_and_the_key_files() {
        let dir = tempfile::tempdir().unwrap();
        let vault = dir.path().join("state");
        std::fs::create_dir_all(vault.join("cred")).unwrap();
        std::fs::write(vault.join("cred").join("grant"), "sealed").unwrap();
        let key = dir.path().join("idp-key.pem");
        std::fs::write(&key, "PRIVATE").unwrap();
        let chain = dir.path().join("audit.log");
        std::fs::write(&chain, "{}\n").unwrap();
        std::fs::write(dir.path().join("audit.log.00000000000000000001"), "{}\n").unwrap();
        let open = dir.path().join("public.txt");
        std::fs::write(&open, "public\n").unwrap();
        let script = format!(
            "cat {g} && echo READ_GRANT; cat {k} && echo READ_KEY; \
             echo x >> {g} && echo WROTE_GRANT; rm -f {g} && echo REMOVED_GRANT; \
             cat {c} && echo READ_CHAIN; cat {c}.00000000000000000001 && echo READ_SEGMENT; \
             cat {o} && echo READ_OPEN; ls / >/dev/null && echo LISTED_ROOT",
            g = vault.join("cred").join("grant").display(),
            k = key.display(),
            c = chain.display(),
            o = open.display(),
        );
        let Some(out) = under(
            vec![
                Protected {
                    path: vault.clone(),
                    with_suffixes: false,
                },
                Protected {
                    path: key.clone(),
                    with_suffixes: false,
                },
                Protected {
                    path: chain.clone(),
                    with_suffixes: true,
                },
            ],
            &script,
        ) else {
            eprintln!("skipped: this kernel has no Landlock");
            return;
        };
        let said = String::from_utf8_lossy(&out.stdout);
        for denied in [
            "READ_GRANT",
            "READ_KEY",
            "WROTE_GRANT",
            "REMOVED_GRANT",
            "READ_CHAIN",
            "READ_SEGMENT",
        ] {
            assert!(!said.contains(denied), "{denied}: {said}");
        }
        assert!(said.contains("public\nREAD_OPEN"), "{said}");
        assert!(said.contains("LISTED_ROOT"), "{said}");
        assert_eq!(
            std::fs::read_to_string(vault.join("cred").join("grant")).unwrap(),
            "sealed",
            "untouched"
        );
    }

    /// A key file and the audit chain inside the protected store root — the
    /// layout the deployment docs show — leave the rest of the root denied:
    /// the inner paths put none of it on the way.
    #[test]
    fn a_protected_path_inside_another_grants_nothing_of_it() {
        let dir = tempfile::tempdir().unwrap();
        let store = dir.path().join("state");
        std::fs::create_dir_all(store.join("cred")).unwrap();
        std::fs::create_dir_all(store.join("audit")).unwrap();
        std::fs::write(store.join("cred").join("grant"), "sealed").unwrap();
        std::fs::write(store.join("probe"), "x").unwrap();
        let seal = store.join("seal");
        std::fs::write(&seal, "KEY").unwrap();
        let chain = store.join("audit").join("audit.log");
        std::fs::write(&chain, "{}\n").unwrap();
        std::fs::write(store.join("audit").join("chain-head.json"), "{}").unwrap();
        let open = dir.path().join("public.txt");
        std::fs::write(&open, "public\n").unwrap();
        let script = format!(
            "cat {p} && echo READ_PROBE; cat {g} && echo READ_GRANT; \
             cat {h} && echo READ_HEAD; echo x >> {p} && echo WROTE_PROBE; \
             cat {s} && echo READ_SEAL; cat {c} && echo READ_CHAIN; \
             cat {o} && echo READ_OPEN",
            p = store.join("probe").display(),
            g = store.join("cred").join("grant").display(),
            h = store.join("audit").join("chain-head.json").display(),
            s = seal.display(),
            c = chain.display(),
            o = open.display(),
        );
        let Some(out) = under(
            vec![
                Protected {
                    path: store.clone(),
                    with_suffixes: false,
                },
                Protected {
                    path: seal.clone(),
                    with_suffixes: false,
                },
                Protected {
                    path: chain.clone(),
                    with_suffixes: true,
                },
            ],
            &script,
        ) else {
            eprintln!("skipped: this kernel has no Landlock");
            return;
        };
        let said = String::from_utf8_lossy(&out.stdout);
        for denied in [
            "READ_PROBE",
            "READ_GRANT",
            "READ_HEAD",
            "WROTE_PROBE",
            "READ_SEAL",
            "READ_CHAIN",
        ] {
            assert!(!said.contains(denied), "{denied}: {said}");
        }
        assert!(said.contains("public\nREAD_OPEN"), "{said}");
    }

    /// Nothing protected, nothing applied.
    #[test]
    fn no_protected_path_applies_nothing() {
        let _held = ONE.lock().unwrap_or_else(|e| e.into_inner());
        let mut cmd = std::process::Command::new("true");
        assert!(confine(&mut cmd).unwrap().is_null());
    }
}
