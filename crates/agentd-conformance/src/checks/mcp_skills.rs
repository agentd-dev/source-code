// SPDX-License-Identifier: AGPL-3.0-only
//! agentd as a client of the MCP Skills extension (SEP-2640,
//! `io.modelcontextprotocol/skills`), read from what it logs and from what
//! reached the server: a listed skill loads only once its `SKILL.md` matches
//! the digest and size its entry lists; one whose bytes do not is declined,
//! after the entry is fetched again with `skills/get`; and a server's prompt
//! is not a skill. The server is the built-in mock at MCP 2026-07-28, which
//! declares the extension, lists its skills with `skills/list`, serves one
//! `SKILL.md` with other bytes than its entry lists, and writes one line per
//! request it answers.

use crate::checks::util::write_file;
use crate::{Category, Check, Harness, Outcome};
use serde_json::Value;

pub fn checks() -> Vec<Check> {
    vec![
        Check {
            id: "mcp-skills/a-listed-skill-loads-once-its-digest-matches",
            category: Category::McpSkills,
            desc: "a skill skills/list names loads once its SKILL.md matches its entry's digest and size",
            run: a_listed_skill_loads,
        },
        Check {
            id: "mcp-skills/a-skill-whose-bytes-differ-is-declined",
            category: Category::McpSkills,
            desc: "a SKILL.md whose bytes do not match its entry is declined, after a skills/get refresh",
            run: a_tampered_skill_is_declined,
        },
        Check {
            id: "mcp-skills/a-prompt-is-not-a-skill",
            category: Category::McpSkills,
            desc: "a server's prompt is never listed, fetched or loaded as a skill",
            run: a_prompt_is_not_a_skill,
        },
    ]
}

/// A job whose instruction is `instruction`, its skills from the server
/// `mock`.
fn config(endpoint: &str, instruction: &str) -> String {
    format!(
        "agent:\n  name: skills\n  instruction: \"{instruction}\"\nstore:\n  kind: memory\n\
         mcp:\n  servers:\n    - name: mock\n      endpoint: {endpoint}/mcp\n\
         skills:\n  sources:\n    - {{server: mock}}\n\
         workflows:\n  - name: job\n    steps:\n      s: {{kind: once}}\n\
         \x20     f: {{kind: finish, depends_on: [s], status: completed}}\n\
         lifecycle:\n  run_until: idle\n  idle_grace: 200ms\n\
         observability:\n  log_level: info\n"
    )
}

/// What one run shows: agentd's exit code and events, and the mock's record
/// of what reached it (`MOCK_SKILLS <method> <params>`, `MOCK_READ <uri>`,
/// `MOCK_META {"method": …}`).
struct Seen {
    code: Option<i32>,
    events: Vec<Value>,
    stderr: String,
    server: String,
}

impl Seen {
    fn named(&self, name: &str) -> Vec<&Value> {
        self.events.iter().filter(|e| e["event"] == name).collect()
    }

    /// The server's lines that begin with `marker`.
    fn served(&self, marker: &str) -> Vec<&str> {
        self.server
            .lines()
            .filter(|l| l.starts_with(marker))
            .collect()
    }
}

/// Start agentd with `instruction` against a 2026-07-28 mock.
fn run(h: &Harness, instruction: &str) -> Seen {
    let tmp = h.tempdir();
    let log = tmp.path().join("mcp.log");
    let mock = h.spawn_mock_mcp_at(
        &tmp.path().join("mcp.addr"),
        "mock://res",
        "2026-07-28",
        &log,
    );
    let cfg = write_file(&tmp, "agentd.yaml", &config(&mock.endpoint(), instruction));
    let r = h.run(&["--config", &cfg]);
    drop(mock);
    Seen {
        code: r.code,
        events: r.events(),
        stderr: r.stderr,
        server: std::fs::read_to_string(&log).unwrap_or_default(),
    }
}

fn a_listed_skill_loads(h: &Harness) -> Outcome {
    let s = run(h, "Follow @skill:pdf-processing.");
    let loaded = s.named("skills.loaded");
    Outcome::require(
        s.code == Some(0),
        format!("exit {:?}; stderr:\n{}", s.code, s.stderr),
    )
    .and(|| {
        Outcome::require(
            s.served("MOCK_SKILLS skills/list").len() > 1,
            format!(
                "the listing was not read page by page; server:\n{}",
                s.server
            ),
        )
    })
    .and(|| {
        Outcome::require(
            loaded.iter().any(|e| {
                e["server"] == "mock"
                    && e["uri"] == "skill://pdf-processing/SKILL.md"
                    && e["verified"] == true
            }),
            format!("the skill was not loaded as (mock, its uri), verified: {loaded:?}"),
        )
    })
    .and(|| {
        Outcome::require(
            s.served("MOCK_READ skill://pdf-processing/templates/")
                .is_empty(),
            "a supporting file was read before anything asked for it",
        )
    })
}

fn a_tampered_skill_is_declined(h: &Harness) -> Outcome {
    let s = run(h, "Follow @skill:tampered.");
    let declined = s.named("skills.declined");
    Outcome::require(
        s.named("skills.loaded").is_empty(),
        format!(
            "a skill whose bytes differ was loaded; stderr:\n{}",
            s.stderr
        ),
    )
    .and(|| {
        Outcome::require(
            declined
                .iter()
                .any(|e| e["server"] == "mock" && e["uri"] == "skill://tampered/SKILL.md"),
            format!("the decline does not name the server and the uri: {declined:?}"),
        )
    })
    .and(|| {
        Outcome::require(
            s.served("MOCK_SKILLS skills/get").len() == 1,
            format!(
                "the entry was not fetched again before the decline; server:\n{}",
                s.server
            ),
        )
    })
}

fn a_prompt_is_not_a_skill(h: &Harness) -> Outcome {
    let s = run(h, "Follow @skill:review-pr.");
    let unknown = s.named("skills.unknown");
    Outcome::require(
        s.named("skills.loaded").is_empty(),
        format!("a prompt loaded as a skill; stderr:\n{}", s.stderr),
    )
    .and(|| {
        Outcome::require(
            unknown.iter().any(|e| e["skill"] == "review-pr"),
            format!("the reference to a prompt was not said to be unknown: {unknown:?}"),
        )
    })
    .and(|| {
        Outcome::require(
            !s.server.contains("\"method\":\"prompts/"),
            format!(
                "a prompt was listed or fetched for skills; server:\n{}",
                s.server
            ),
        )
    })
}
