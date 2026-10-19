// SPDX-License-Identifier: AGPL-3.0-only
//! AGENTS.md (<https://agents.md>, RFC 0053): the plain-Markdown notes a
//! workspace keeps for the agents that work in it. agentd reads those of the
//! workspace the operator names (`agent.workspace`): the `AGENTS.md` of its
//! root, then of each directory on the way down to agentd's working
//! directory when that lies inside it, so the nearest comes last and wins
//! where two disagree ("the closest AGENTS.md to the edited file wins").
//! Outside the workspace, the root's alone.
//!
//! The files are text and nothing else: they reach the system prompt as
//! data the template renders below the instruction, and are never parsed as
//! an instruction document, so a `:::!` line in one is a line of text and
//! configures nothing (RFC 0042). They are read at load, which a reload
//! runs again: UTF-8 only, at most [`MAX_BYTES`] together, and a symlink is
//! followed only to a file inside the workspace.

use std::io::Read;
use std::path::{Path, PathBuf};

/// The file each directory may hold.
pub const FILE: &str = "AGENTS.md";

/// The most the files may hold together, in bytes as they are on disk.
pub const MAX_BYTES: usize = 32 * 1024;

/// One file's text, by its path from the workspace root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentsMd {
    /// `AGENTS.md`, `services/billing/AGENTS.md`.
    pub path: String,
    pub text: String,
}

/// The directories whose `AGENTS.md` is read, in order: the workspace's
/// root, then each directory below it down to `cwd` when `cwd` lies inside
/// it. Both are taken with their symlinks resolved, so every directory is
/// one of the workspace's own.
pub fn dirs(root: &Path, cwd: Option<&Path>) -> Result<Vec<PathBuf>, String> {
    let root = root
        .canonicalize()
        .map_err(|e| format!("{}: {e}", root.display()))?;
    if !root.is_dir() {
        return Err(format!("{} is not a directory", root.display()));
    }
    let mut out = vec![root.clone()];
    if let Some(below) = cwd
        .and_then(|c| c.canonicalize().ok())
        .and_then(|c| c.strip_prefix(&root).map(Path::to_path_buf).ok())
    {
        let mut d = root;
        for part in below.components() {
            d.push(part);
            out.push(d.clone());
        }
    }
    Ok(out)
}

/// The `AGENTS.md` files of the workspace at `root` for an agent working in
/// `cwd`, root first. A directory without one is passed over, and so is a
/// file holding only whitespace. Refused, naming the file: one that is not a
/// regular file, a symlink that leads out of the workspace, files that
/// together pass [`MAX_BYTES`] — counted before anything is decoded, so a
/// file past it is refused whole, never cut — and text that is not UTF-8.
pub fn read(root: &Path, cwd: Option<&Path>) -> Result<Vec<AgentsMd>, String> {
    let dirs = dirs(root, cwd)?;
    let top = dirs[0].clone();
    let mut out = Vec::new();
    let mut total = 0usize;
    for dir in dirs {
        let file = dir.join(FILE);
        let rel = file
            .strip_prefix(&top)
            .unwrap_or(&file)
            .to_string_lossy()
            .into_owned();
        match std::fs::symlink_metadata(&file) {
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(format!("{rel}: {e}")),
        }
        let real = file.canonicalize().map_err(|e| format!("{rel}: {e}"))?;
        if !real.starts_with(&top) {
            return Err(format!(
                "{rel} leads to {}, outside the workspace; a symlink is followed only to a file inside it",
                real.display()
            ));
        }
        if !std::fs::metadata(&real).is_ok_and(|m| m.is_file()) {
            return Err(format!("{rel} is not a regular file"));
        }
        // Read no further than one byte past what the cap leaves.
        let mut bytes = Vec::new();
        std::fs::File::open(&real)
            .and_then(|f| {
                f.take((MAX_BYTES - total) as u64 + 1)
                    .read_to_end(&mut bytes)
            })
            .map_err(|e| format!("{rel}: {e}"))?;
        total += bytes.len();
        if total > MAX_BYTES {
            return Err(format!(
                "the AGENTS.md files up to {rel} hold more than {MAX_BYTES} bytes, the most agentd puts into the prompt; shorten them"
            ));
        }
        let text = String::from_utf8(bytes).map_err(|_| format!("{rel} is not UTF-8 text"))?;
        let text = text.strip_prefix('\u{feff}').unwrap_or(&text).trim_end();
        if !text.trim().is_empty() {
            out.push(AgentsMd {
                path: rel,
                text: text.to_string(),
            });
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(p: &Path, text: &str) {
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, text).unwrap();
    }

    /// Root first, then each directory down to the working directory, the
    /// nearest last; a directory without the file, or one past the working
    /// directory, adds nothing; and a working directory outside the
    /// workspace reads the root's alone.
    #[test]
    fn the_files_come_root_first_and_nearest_last() {
        let ws = tempfile::tempdir().unwrap();
        let root = ws.path();
        write(&root.join(FILE), "root rules\n");
        write(&root.join("services").join(FILE), "services rules");
        write(&root.join("services/billing").join(FILE), "billing rules");
        write(
            &root.join("services/billing/api/deeper").join(FILE),
            "below the cwd",
        );
        write(&root.join("docs").join(FILE), "a sibling");
        let cwd = root.join("services/billing/api");
        std::fs::create_dir_all(&cwd).unwrap();
        let got = read(root, Some(&cwd)).unwrap();
        let paths: Vec<&str> = got.iter().map(|f| f.path.as_str()).collect();
        assert_eq!(
            paths,
            [
                "AGENTS.md",
                "services/AGENTS.md",
                "services/billing/AGENTS.md"
            ]
        );
        assert_eq!(got[0].text, "root rules", "trailing whitespace trimmed");
        assert_eq!(got[2].text, "billing rules");
        let elsewhere = tempfile::tempdir().unwrap();
        let alone = read(root, Some(elsewhere.path())).unwrap();
        assert_eq!(alone.len(), 1);
        assert_eq!(alone[0].path, "AGENTS.md");
        assert_eq!(read(root, None).unwrap(), alone);
        // No file anywhere: nothing, and no error.
        let empty = tempfile::tempdir().unwrap();
        assert!(read(empty.path(), Some(empty.path())).unwrap().is_empty());
    }

    /// A working directory reached through a symlink is the directory it
    /// leads to: inside the workspace it walks there.
    #[cfg(unix)]
    #[test]
    fn a_symlinked_working_directory_is_where_it_leads() {
        let ws = tempfile::tempdir().unwrap();
        let root = ws.path();
        write(&root.join("pkg").join(FILE), "pkg rules");
        let link = tempfile::tempdir().unwrap();
        let via = link.path().join("into-pkg");
        std::os::unix::fs::symlink(root.join("pkg"), &via).unwrap();
        let got = read(root, Some(&via)).unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].path, "pkg/AGENTS.md");
    }

    /// The cap is the files together, a leading BOM is not text, and what
    /// is not UTF-8, not a file, or a symlink out of the workspace is
    /// refused, named.
    #[test]
    fn what_is_read_is_capped_and_held_to_the_workspace() {
        let ws = tempfile::tempdir().unwrap();
        let root = ws.path();
        write(&root.join(FILE), &"a".repeat(MAX_BYTES - 10));
        let sub = root.join("sub");
        write(&sub.join(FILE), "0123456789");
        assert_eq!(read(root, Some(&sub)).unwrap().len(), 2, "exactly the cap");
        write(&sub.join(FILE), "0123456789x");
        let e = read(root, Some(&sub)).unwrap_err();
        assert!(e.contains("sub/AGENTS.md") && e.contains("32768"), "{e}");
        // One file alone past the cap is read no further than it.
        write(&root.join(FILE), &"b".repeat(4 * MAX_BYTES));
        assert!(read(root, None).unwrap_err().contains("more than 32768"));
        // Past the cap is too large whatever the text: a multibyte character
        // the cap cuts through is not taken for bad UTF-8, and text past
        // trailing whitespace the cap falls in is not dropped unseen.
        write(&root.join(FILE), &"я".repeat(20_000));
        let e = read(root, None).unwrap_err();
        assert!(e.contains("more than 32768"), "{e}");
        write(
            &root.join(FILE),
            &format!("{}{}b", "a".repeat(MAX_BYTES - 1), " ".repeat(100)),
        );
        let e = read(root, None).unwrap_err();
        assert!(e.contains("more than 32768"), "{e}");

        write(&root.join(FILE), "\u{feff}# Rules");
        assert_eq!(read(root, None).unwrap()[0].text, "# Rules");
        std::fs::write(root.join(FILE), [0x66, 0x6f, 0xff, 0x6f]).unwrap();
        assert!(
            read(root, None)
                .unwrap_err()
                .contains("AGENTS.md is not UTF-8")
        );
        std::fs::remove_file(root.join(FILE)).unwrap();
        std::fs::create_dir(root.join(FILE)).unwrap();
        assert!(
            read(root, None)
                .unwrap_err()
                .contains("is not a regular file")
        );
        std::fs::remove_dir(root.join(FILE)).unwrap();
        #[cfg(unix)]
        {
            let outside = tempfile::tempdir().unwrap();
            write(&outside.path().join("notes.md"), "secret");
            std::os::unix::fs::symlink(outside.path().join("notes.md"), root.join(FILE)).unwrap();
            let e = read(root, None).unwrap_err();
            assert!(e.contains("outside the workspace"), "{e}");
            std::fs::remove_file(root.join(FILE)).unwrap();
            write(&root.join("shared.md"), "shared rules");
            std::os::unix::fs::symlink(root.join("shared.md"), root.join(FILE)).unwrap();
            assert_eq!(
                read(root, None).unwrap()[0].text,
                "shared rules",
                "inside it is followed"
            );
        }
        let e = read(&root.join("missing"), None).unwrap_err();
        assert!(e.contains("missing"), "{e}");
        write(&root.join("plain"), "x");
        assert!(
            read(&root.join("plain"), None)
                .unwrap_err()
                .contains("is not a directory")
        );
    }
}
