// SPDX-License-Identifier: AGPL-3.0-only
//! **Agent Skills on disk** (RFC 0051 D5): a folder of skill directories,
//! each `<name>/SKILL.md`, held to the Agent Skills rules — the frontmatter
//! rules [`::mcp::skills::check_frontmatter`] holds a server's entries to,
//! plus a `name` equal to the directory's.
//!
//! A loose file in the folder (`README.md`, a `<name>.md`) is not a skill and
//! is not read, nor is a directory without a `SKILL.md` or one whose name
//! starts with a dot. A skill that breaks a rule is an error naming its file
//! and the rule: the operator wrote it, so it is refused at load rather than
//! skipped.

use serde_json::{Map, Value};
use std::path::{Path, PathBuf};

/// One skill read from a folder.
#[derive(Debug, Clone, PartialEq)]
pub struct LocalSkill {
    pub name: String,
    pub description: String,
    /// Its `SKILL.md`.
    pub skill_md: PathBuf,
    /// Its directory, the root its files are read under.
    pub dir: PathBuf,
    /// The text after the frontmatter, trimmed.
    pub body: String,
}

/// Split a `SKILL.md` into its frontmatter, as a JSON object, and its body:
/// the file begins with a `---` line, the frontmatter is the YAML up to the
/// next `---` line, and the body is everything after it, trimmed.
pub fn split(text: &str) -> Result<(Map<String, Value>, String), String> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let mut lines = text.split_inclusive('\n');
    let first = lines.next().unwrap_or_default();
    if first.trim_end_matches(['\r', '\n']) != "---" {
        return Err("it does not begin with YAML frontmatter (a `---` line)".into());
    }
    let mut head = String::new();
    let mut closed = false;
    let mut consumed = first.len();
    for line in lines {
        consumed += line.len();
        if line.trim_end_matches(['\r', '\n']) == "---" {
            closed = true;
            break;
        }
        head.push_str(line);
    }
    if !closed {
        return Err("its frontmatter is not closed with a `---` line".into());
    }
    let frontmatter = match crate::config::yaml::parse(&head) {
        Ok(Value::Object(m)) => m,
        Ok(Value::Null) => return Err("its frontmatter is empty".into()),
        Ok(_) => return Err("its frontmatter is not a YAML mapping".into()),
        Err(e) => return Err(format!("its frontmatter is not YAML: {e}")),
    };
    Ok((frontmatter, text[consumed..].trim().to_string()))
}

/// Read one `SKILL.md` in the directory `dir`, held to every rule.
pub fn read_skill(dir: &Path) -> Result<LocalSkill, String> {
    let skill_md = dir.join(::mcp::skills::SKILL_FILE);
    let at = skill_md.display();
    let text = std::fs::read_to_string(&skill_md).map_err(|e| format!("{at}: {e}"))?;
    let (fm, body) = split(&text).map_err(|e| format!("{at}: {e}"))?;
    ::mcp::skills::check_frontmatter(&fm).map_err(|e| format!("{at}: {e} (agentskills.io)"))?;
    let name = fm["name"].as_str().unwrap_or_default().to_string();
    let dir_name = dir.file_name().and_then(|n| n.to_str()).unwrap_or_default();
    if name != dir_name {
        return Err(format!(
            "{at}: `name` {name:?} is not its directory's name {dir_name:?} (agentskills.io)"
        ));
    }
    Ok(LocalSkill {
        name,
        description: fm["description"].as_str().unwrap_or_default().to_string(),
        skill_md,
        dir: dir.to_path_buf(),
        body,
    })
}

/// Every skill in `folder`, sorted by directory name, or every rule they
/// break. A folder that cannot be read is an error too.
pub fn read_folder(folder: &Path) -> Result<Vec<LocalSkill>, Vec<String>> {
    let rd = std::fs::read_dir(folder)
        .map_err(|e| vec![format!("{}: cannot read the folder: {e}", folder.display())])?;
    let mut dirs: Vec<PathBuf> = rd
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| !n.starts_with('.'))
        })
        .filter(|p| p.join(::mcp::skills::SKILL_FILE).is_file())
        .collect();
    dirs.sort();
    let (mut skills, mut errs) = (Vec::new(), Vec::new());
    for d in dirs {
        match read_skill(&d) {
            Ok(s) => skills.push(s),
            Err(e) => errs.push(e),
        }
    }
    if errs.is_empty() {
        Ok(skills)
    } else {
        Err(errs)
    }
}

/// Does `folder` hold at least one `<name>/SKILL.md`?
pub fn holds_skills(folder: &Path) -> bool {
    std::fs::read_dir(folder).is_ok_and(|rd| {
        rd.flatten()
            .any(|e| e.path().join(::mcp::skills::SKILL_FILE).is_file())
    })
}

/// The bytes of the file at `relative` under a local skill's `dir`, which it
/// may not leave — by `..`, an absolute path, or a link that points out.
pub fn read_file(dir: &Path, relative: &str) -> Result<Vec<u8>, String> {
    if relative.starts_with('/') || relative.split('/').any(|s| s == "..") {
        return Err(format!("{relative:?} is not a path inside the skill"));
    }
    let root = dir
        .canonicalize()
        .map_err(|e| format!("{}: {e}", dir.display()))?;
    let path = root
        .join(relative)
        .canonicalize()
        .map_err(|e| format!("{relative}: {e}"))?;
    if !path.starts_with(&root) || !path.is_file() {
        return Err(format!("{relative:?} is not a file inside the skill"));
    }
    std::fs::read(&path).map_err(|e| format!("{relative}: {e}"))
}

/// The files under a local skill's `dir`, relative to it, `SKILL.md` left
/// out: at most `cap`, in path order, dot-entries skipped.
pub fn list_files(dir: &Path, cap: usize) -> Vec<String> {
    fn walk(base: &Path, at: &Path, out: &mut Vec<String>, cap: usize, depth: usize) {
        let Ok(rd) = std::fs::read_dir(at) else {
            return;
        };
        let mut entries: Vec<PathBuf> = rd.flatten().map(|e| e.path()).collect();
        entries.sort();
        for p in entries {
            if out.len() >= cap {
                return;
            }
            if p.file_name()
                .and_then(|n| n.to_str())
                .is_none_or(|n| n.starts_with('.'))
            {
                continue;
            }
            if p.is_dir() {
                if depth < 6 {
                    walk(base, &p, out, cap, depth + 1);
                }
            } else if let Ok(rel) = p.strip_prefix(base) {
                let rel = rel.to_string_lossy().replace('\\', "/");
                if rel != ::mcp::skills::SKILL_FILE {
                    out.push(rel);
                }
            }
        }
    }
    let mut out = Vec::new();
    walk(dir, dir, &mut out, cap, 0);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> PathBuf {
        let d =
            std::env::temp_dir().join(format!("agentd-localskills-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn skill(folder: &Path, dir: &str, text: &str) {
        std::fs::create_dir_all(folder.join(dir)).unwrap();
        std::fs::write(folder.join(dir).join("SKILL.md"), text).unwrap();
    }

    /// The refusal for a folder holding one skill directory `dir` with `text`.
    fn refusal(tag: &str, dir: &str, text: &str) -> String {
        let f = scratch(tag);
        skill(&f, dir, text);
        let errs = read_folder(&f).expect_err("refused");
        let _ = std::fs::remove_dir_all(&f);
        assert_eq!(errs.len(), 1, "{errs:?}");
        errs[0].clone()
    }

    #[test]
    fn a_skill_md_splits_into_frontmatter_and_body() {
        let (fm, body) = split("---\nname: a\ndescription: >-\n  folded\n  text\nmetadata:\n  version: \"1.0\"\n---\n\n# A\n\nDo it.\n").unwrap();
        assert_eq!(fm["name"], "a");
        assert_eq!(fm["description"], "folded text");
        assert_eq!(fm["metadata"]["version"], "1.0");
        assert_eq!(body, "# A\n\nDo it.");
        let (fm, body) = split("\u{feff}---\r\nname: a\r\ndescription: d\r\n---\r\n").unwrap();
        assert_eq!(fm["description"], "d");
        assert_eq!(body, "", "a skill may be frontmatter alone");
        assert!(
            split("# no frontmatter\n")
                .unwrap_err()
                .contains("does not begin")
        );
        assert!(split("---\nname: a\n").unwrap_err().contains("not closed"));
        assert!(split("---\n---\nbody").unwrap_err().contains("empty"));
        assert!(
            split("---\n- a\n---\n")
                .unwrap_err()
                .contains("not a YAML mapping")
        );
        assert!(
            split("---\nname: &x a\n---\n")
                .unwrap_err()
                .contains("not YAML")
        );
    }

    #[test]
    fn a_folder_reads_its_skill_directories_and_nothing_else() {
        let f = scratch("folder");
        skill(
            &f,
            "triage",
            "---\nname: triage\ndescription: Triage an issue.\nallowed-tools: Bash(rm:*)\nx-client: 1\n---\nLabel it.\n",
        );
        skill(
            &f,
            "deploy",
            "---\nname: deploy\ndescription: Deploy safely.\nlicense: MIT\ncompatibility: Needs nothing\n---\nBehind a flag.\n",
        );
        std::fs::create_dir_all(f.join("deploy/references")).unwrap();
        std::fs::write(f.join("deploy/references/flags.md"), "flags").unwrap();
        // Not skills: a loose file, a directory with no SKILL.md, a hidden one.
        std::fs::write(
            f.join("README.md"),
            "---\nname: readme\ndescription: x\n---\n",
        )
        .unwrap();
        std::fs::write(
            f.join("loose.md"),
            "---\nname: loose\ndescription: x\n---\n",
        )
        .unwrap();
        std::fs::create_dir_all(f.join("notes")).unwrap();
        skill(&f, ".hidden", "not even read");
        let skills = read_folder(&f).unwrap();
        let names: Vec<&str> = skills.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["deploy", "triage"]);
        assert_eq!(skills[0].body, "Behind a flag.");
        assert_eq!(skills[0].description, "Deploy safely.");
        assert_eq!(list_files(&skills[0].dir, 10), ["references/flags.md"]);
        assert_eq!(
            read_file(&skills[0].dir, "references/flags.md").unwrap(),
            b"flags"
        );
        assert!(read_file(&skills[0].dir, "../triage/SKILL.md").is_err());
        assert!(read_file(&skills[0].dir, "/etc/hostname").is_err());
        assert!(
            read_file(&skills[0].dir, "references").is_err(),
            "a directory is no file"
        );
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(f.join("triage/SKILL.md"), f.join("deploy/out.md")).unwrap();
            assert!(
                read_file(&skills[0].dir, "out.md")
                    .unwrap_err()
                    .contains("inside")
            );
        }
        assert!(holds_skills(&f));
        assert!(!holds_skills(&f.join("notes")));
        let _ = std::fs::remove_dir_all(&f);
        assert!(read_folder(&f).unwrap_err()[0].contains("cannot read the folder"));
    }

    /// Every Agent Skills rule a local `SKILL.md` is held to, each refusal
    /// naming the file and the rule.
    #[test]
    fn a_local_skill_that_breaks_a_rule_is_refused_naming_file_and_rule() {
        let e = refusal(
            "dirname",
            "runbook",
            "---\nname: incident\ndescription: d\n---\n",
        );
        assert!(
            e.contains("runbook/SKILL.md") && e.contains("not its directory's name"),
            "{e}"
        );
        let e = refusal("upper", "PDF", "---\nname: PDF\ndescription: d\n---\n");
        assert!(e.contains("PDF/SKILL.md") && e.contains("'P'"), "{e}");
        let e = refusal(
            "long",
            &"a".repeat(65),
            &format!("---\nname: {}\ndescription: d\n---\n", "a".repeat(65)),
        );
        assert!(e.contains("65 characters"), "{e}");
        let e = refusal("lead", "-a", "---\nname: -a\ndescription: d\n---\n");
        assert!(e.contains("starts or ends with a hyphen"), "{e}");
        let e = refusal("trail", "a-", "---\nname: a-\ndescription: d\n---\n");
        assert!(e.contains("starts or ends with a hyphen"), "{e}");
        let e = refusal("double", "a--b", "---\nname: a--b\ndescription: d\n---\n");
        assert!(e.contains("two hyphens"), "{e}");
        let e = refusal("noname", "a", "---\ndescription: d\n---\n");
        assert!(e.contains("`name` is required"), "{e}");
        let e = refusal("nodesc", "a", "---\nname: a\n---\n");
        assert!(e.contains("`description` is required"), "{e}");
        let e = refusal("emptydesc", "a", "---\nname: a\ndescription: \"\"\n---\n");
        assert!(e.contains("`description` is empty"), "{e}");
        let e = refusal(
            "longdesc",
            "a",
            &format!("---\nname: a\ndescription: {}\n---\n", "d".repeat(1025)),
        );
        assert!(e.contains("1025 characters"), "{e}");
        let e = refusal(
            "compat",
            "a",
            &format!(
                "---\nname: a\ndescription: d\ncompatibility: {}\n---\n",
                "c".repeat(501)
            ),
        );
        assert!(e.contains("`compatibility` is 501"), "{e}");
        let e = refusal(
            "meta",
            "a",
            "---\nname: a\ndescription: d\nmetadata:\n  version: 1.0\n---\n",
        );
        assert!(e.contains("`metadata` \"version\" is not a string"), "{e}");
        let e = refusal(
            "metamap",
            "a",
            "---\nname: a\ndescription: d\nmetadata: [x]\n---\n",
        );
        assert!(e.contains("`metadata` is not a map"), "{e}");
        let e = refusal(
            "license",
            "a",
            "---\nname: a\ndescription: d\nlicense: [MIT]\n---\n",
        );
        assert!(e.contains("`license` is not a string"), "{e}");
        let e = refusal(
            "tools",
            "a",
            "---\nname: a\ndescription: d\nallowed-tools: [Read]\n---\n",
        );
        assert!(e.contains("`allowed-tools` is not a string"), "{e}");
        let e = refusal("nofm", "a", "# A\n\nNo frontmatter.\n");
        assert!(
            e.contains("a/SKILL.md") && e.contains("does not begin with YAML frontmatter"),
            "{e}"
        );
        let e = refusal("notclosed", "a", "---\nname: a\ndescription: d\n");
        assert!(e.contains("not closed"), "{e}");
    }
}
