// SPDX-License-Identifier: AGPL-3.0-only
//! **The instruction document** — agentd as an `instruction/1` host.
//!
//! The document is read by the official instruction.md SDK
//! ([`instruction_md`]): the reader, its checks and the agent text
//! (`instruction-text/1`) are the SDK's, byte for byte. What is agentd's is
//! what a document's blocks *become* here — the host side the specification
//! leaves to each runtime (§9.1, §10, §17):
//!
//! | Block | Becomes |
//! |---|---|
//! | `:::mcp{id src tools allow deny}` | an `mcp.servers` entry: `id` → `name`, `src` → `endpoint`, `tools`/`allow` → `allow`, `deny` → `exclude` (the `agentd/1` export profile's mapping) |
//! | `::agent{id protocol=a2a endpoint\|src}` | an `a2a.peers` entry named `id`, under the [`IDENTITY`] grant |
//! | `::::agent{id}` with a body and no `protocol`, `src` or `endpoint` | an agent agentd runs itself: a `subagents.templates` entry named `id` whose instruction is the body, under the [`AGENTS`] grant ([`DocumentAgent`]) |
//! | `:::workflow{dialect="instruction/workflow"}` | a `workflows[]` entry (the body is the definition, its `name` required) |
//! | `:::config{dialect="agentd/config"}` | the document's configuration fragment, held to the document boundary downstream |
//! | `:::skill{id trigger}` with a body and no `src` | an inline skill in the catalogue |
//! | `::person`, `::role` | a person a workflow's `human` step may be addressed to (`to: <id>`) |
//!
//! Everything else is guidance and reaches the model only as text. A
//! connected object agentd will not activate — one outside its grants, one
//! that arrived over the wire under the §7.8 floor, a dialect it does not
//! speak, a target it cannot dial — stays in the agent text, unresolved, and
//! is reported as a host-policy diagnostic (§17): never removed, never
//! silently dropped.
//!
//! Configuration is taken from the document's top level and from inside its
//! layout containers (sections, tabs, columns…), never from inside an
//! example, a policy or any other block whose body is guidance.

use std::collections::{BTreeMap, BTreeSet};

use instruction_md::{
    ConfigurationBlock, ConnectedObject, Diagnostic, ReadOptions, Reading, Severity,
};
use serde_json::{Map, Value};

pub use instruction_md::Document;

/// The grant a document needs for a workflow that opens an inbound route on
/// the operator's webhook listener.
pub const INTERFACE: &str = "interface";
/// The grant a document needs for its A2A agents to become `a2a.peers`.
pub const IDENTITY: &str = "identity";
/// The grant a document needs for the agents it defines to become agentd's
/// subagents (RFC 0056).
pub const AGENTS: &str = "agents";
/// agentd's document grants (`agent.document_capabilities`). instruction/1
/// grants nothing by itself (§19): these are the host's.
pub const FAMILIES: &[&str] = &[INTERFACE, IDENTITY, AGENTS];
/// The grants a document that arrived over the wire never uses, whatever
/// the operator granted or its author signed (§7.8). `agents` is not one:
/// over the wire it needs a verified pin whose signature attests it
/// ([`Admission`]).
pub const WIRE_FLOOR: &[&str] = &[IDENTITY];

/// Every grant agentd knows.
pub fn all_families() -> BTreeSet<String> {
    FAMILIES.iter().map(|f| (*f).to_string()).collect()
}

/// The specification commit the SDK's registry was vendored from.
pub const SPEC_COMMIT: &str = instruction_md::SPEC_COMMIT;

/// The registry the SDK reads documents by — the specification's
/// `registry.json`, verbatim.
pub fn registry_json() -> &'static str {
    instruction_md::registry::REGISTRY_JSON
}

/// The workflow dialects agentd runs.
pub const WORKFLOW_DIALECTS: &[&str] = &["instruction/workflow", "agentd/workflow"];
/// The configuration dialect agentd reads.
pub const CONFIG_DIALECT: &str = "agentd/config";

/// One problem with a document: an SDK diagnostic of `error` severity, or a
/// host refusal, with its stable code and the 1-based line when there is one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refusal {
    pub line: Option<usize>,
    pub code: String,
    pub message: String,
}

impl Refusal {
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Refusal {
        Refusal {
            line: None,
            code: code.into(),
            message: message.into(),
        }
    }

    pub fn at(line: usize, code: impl Into<String>, message: impl Into<String>) -> Refusal {
        Refusal {
            line: Some(line),
            ..Refusal::new(code, message)
        }
    }

    fn of(d: &Diagnostic) -> Refusal {
        Refusal {
            line: d.line().map(|l| l as usize),
            code: d.code.to_string(),
            message: d.message.clone(),
        }
    }
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.line {
            Some(n) => write!(f, "line {n}: {}", self.message),
            None => f.write_str(&self.message),
        }
    }
}

/// A diagnostic as agentd reports it: `line N: message (code)`.
fn said(d: &Diagnostic) -> String {
    match d.line() {
        Some(n) => format!("line {n}: {} ({})", d.message, d.code),
        None => format!("{} ({})", d.message, d.code),
    }
}

/// A document the SDK read without an error.
#[derive(Debug, Clone)]
pub struct Parsed {
    pub reading: Reading,
    /// The SDK's diagnostics — the reader's, the inline checks', the
    /// workflow bodies' and the 1.1 lines' — none of them an error.
    pub diagnostics: Vec<Diagnostic>,
}

impl Parsed {
    /// The agent text (`instruction-text/1`), declared defaults substituted.
    pub fn text(&self) -> String {
        self.reading.text(None)
    }
}

/// Read a document with the SDK: the reader's diagnostics, the inline
/// checks', each `instruction/workflow` body's (`check_reading`), and the
/// lines written in the 1.1 syntax (`legacy_diagnostics`, an error: a 1.1
/// `:::!kind` block is prose to this reader, and delivering it would deliver
/// what its author did not mean). An error refuses the whole document;
/// warnings are kept on the result.
pub fn parse(text: &str) -> Result<Parsed, Vec<Refusal>> {
    let reading = instruction_md::read(text, &ReadOptions::default());
    let mut diagnostics = instruction_md::check_reading(&reading);
    diagnostics.extend(instruction_md::legacy_diagnostics(text));
    diagnostics.sort_by_key(|d| d.line().unwrap_or(0));
    let errors: Vec<Refusal> = diagnostics
        .iter()
        .filter(|d| d.severity == Severity::Error)
        .map(Refusal::of)
        .collect();
    if errors.is_empty() {
        Ok(Parsed {
            reading,
            diagnostics,
        })
    } else {
        Err(errors)
    }
}

/// Whether delivery would change anything of `text` — any block, keyword
/// line, comment, metadata, reference or parameter — or has anything to say
/// about it. Plain prose is used as it is, byte for byte.
pub fn needs_delivery(text: &str) -> bool {
    let reading = instruction_md::read(text, &ReadOptions::default());
    reading.changes_text()
        || !instruction_md::check_reading(&reading).is_empty()
        || !instruction_md::legacy_diagnostics(text).is_empty()
}

/// A document split at its backmatter (§3.2).
#[derive(Debug, Clone, PartialEq)]
pub struct SplitEndMatter<'a> {
    /// The text before the backmatter's `---` separator, front matter
    /// included; the whole text when there is none.
    pub before: &'a str,
    /// The backmatter's keys, when the text ends with backmatter.
    pub end_matter: Option<Map<String, Value>>,
    /// The 1-based line of the backmatter's `---` separator.
    pub opener_line: Option<usize>,
}

/// Split a whole document at its backmatter, as the SDK recognises it
/// (`split_backmatter`): for a reader that needs the text without its record
/// (a folder of documents, a prompt). Backmatter that is not a YAML mapping
/// is refused.
pub fn split_end_matter(text: &str) -> Result<SplitEndMatter<'_>, Refusal> {
    let split = instruction_md::split_backmatter(text);
    if let Some(d) = split
        .diagnostics
        .iter()
        .find(|d| d.severity == Severity::Error)
    {
        return Err(Refusal::of(d));
    }
    Ok(SplitEndMatter {
        before: split.before,
        opener_line: split.range.map(|r| r.start as usize),
        end_matter: split.backmatter.map(|_| split.data.clone()),
    })
}

/// A skill lifted from the document into the catalogue.
#[derive(Debug, Clone, PartialEq)]
pub struct InlineSkill {
    pub name: String,
    pub description: String,
    pub when_to_use: Option<String>,
    pub body: String,
    /// The URL a fetched skill's text came from (a `:::skill{src}` naming a
    /// `SKILL.md`): outside text, which reaches the model marked untrusted.
    /// `None` for a body the document carries.
    pub fetched_from: Option<String>,
}

/// A `:::skill{src}` naming a `SKILL.md` agentd fetches at load, before the
/// fetch ([`skill_url`]).
#[derive(Debug, Clone, PartialEq)]
pub struct RemoteSkill {
    /// The document's id for it, which names the skill.
    pub name: String,
    /// Its title or description, the fallback when the `SKILL.md` has none.
    pub description: Option<String>,
    pub when_to_use: Option<String>,
    /// The `src` as written.
    pub src: String,
    /// What is fetched: `src` normalised to the raw file.
    pub url: String,
}

/// What a document yields: the agent text, a configuration fragment to merge
/// under the operator's, the workflows to splice, the inline skills, the
/// connected objects agentd records but does not activate, the people its
/// gates may address, and every diagnostic worth saying.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Extraction {
    /// The agent text (`instruction-text/1`), the SDK's byte for byte.
    pub cleaned: String,
    pub config: Map<String, Value>,
    /// Root-level `workflows[]` entries — spliced into the document array,
    /// not folded under a section.
    pub workflows: Vec<Value>,
    pub skills: Vec<InlineSkill>,
    /// Skills whose `SKILL.md` is fetched at load, not yet fetched: the
    /// configuration loader fetches them (`config::settings`).
    pub remote_skills: Vec<RemoteSkill>,
    /// Connected objects recorded by kind (visible in `--capabilities`),
    /// activated by nothing.
    pub declarations: BTreeMap<String, Vec<Value>>,
    /// The ids of the document's `person` and `role` objects.
    pub people: BTreeSet<String>,
    /// The agents the document defines for agentd to run (RFC 0056), under
    /// the [`AGENTS`] grant.
    pub agents: Vec<DocumentAgent>,
    /// The `(id, kind)` of every person, role and agent the document
    /// declares, wherever it stands: the names its agents' text may refer to
    /// ([`extract_knowing`]).
    pub known: Vec<(String, String)>,
    /// The SDK's warnings and agentd's host-policy diagnostics, each
    /// `line N: message (code)`.
    pub warnings: Vec<String>,
}

/// The host-policy diagnostic code (§17, §18: a host's policy, distinct from
/// format invalidity).
pub const HOST_POLICY: &str = "host-policy";

/// An agent a document defines for agentd to run (RFC 0056): an `agent`
/// with a body and no `protocol`, `src` or `endpoint`. Its body is its own
/// instruction document.
#[derive(Debug, Clone, PartialEq)]
pub struct DocumentAgent {
    /// Its `id`: the name of the template it becomes.
    pub id: String,
    pub title: Option<String>,
    /// Its instruction document: the body as written, under the parent
    /// document's declared parameters (front matter) when it has any.
    pub instruction: String,
    /// The parent document's line the agent opens on.
    pub line: usize,
    /// The agent it is nested in — its reporting line, a label and nothing
    /// more — or `None` at the top level.
    pub reports_to: Option<String>,
    /// How many lines [`DocumentAgent::instruction`] carries before the body.
    pub header_lines: usize,
    /// The parent document's line of each body line of the instruction, in
    /// order: the agents nested in it are cut out, so the two run apart.
    pub lines: Vec<usize>,
}

impl DocumentAgent {
    /// A diagnostic about the agent's instruction (`line N: …`), named by the
    /// agent and its line moved to the parent document's.
    pub fn in_parent(&self, said: &str) -> String {
        let moved = said
            .strip_prefix("line ")
            .and_then(|rest| rest.split_once(':'))
            .and_then(|(n, rest)| Some((n.parse::<usize>().ok()?, rest)))
            .map(|(n, rest)| {
                let at = n
                    .checked_sub(self.header_lines + 1)
                    .and_then(|i| self.lines.get(i).copied())
                    .unwrap_or(self.line);
                format!("line {at}:{rest}")
            });
        format!("agent {:?}: {}", self.id, moved.as_deref().unwrap_or(said))
    }
}

/// An `agent` the host runs itself: no `protocol`, `src` or `endpoint`.
fn runs_itself(attrs: &instruction_md::types::Attrs) -> bool {
    ["protocol", "src", "endpoint"].iter().all(|k| {
        attrs
            .get(*k)
            .and_then(Value::as_str)
            .is_none_or(|v| v.trim().is_empty())
    })
}

/// `body`, whose first line is the parent's line `first`, with the parent's
/// line ranges `cuts` taken out — and with them the blank lines a cut leaves
/// doubled. With cuts or without, blank lines at the start and the end go
/// and the text ends in one newline, so it is the same however many nested
/// blocks stood where, none included. Returns the text and the parent's line
/// of each line kept.
fn cut_lines(body: &str, first: usize, cuts: &[(usize, usize)]) -> (String, Vec<usize>) {
    let mut kept: Vec<(usize, &str)> = Vec::new();
    let mut after_cut = false;
    for (i, line) in body.lines().enumerate() {
        let at = first + i;
        if cuts.iter().any(|(s, e)| (*s..=*e).contains(&at)) {
            after_cut = true;
            continue;
        }
        let blank = line.trim().is_empty();
        if blank
            && (kept.is_empty()
                || (after_cut && kept.last().is_some_and(|(_, l)| l.trim().is_empty())))
        {
            continue;
        }
        after_cut = after_cut && blank;
        kept.push((at, line));
    }
    while kept.last().is_some_and(|(_, l)| l.trim().is_empty()) {
        kept.pop();
    }
    let mut text: String = kept.iter().map(|(_, l)| *l).collect::<Vec<_>>().join("\n");
    text.push('\n');
    (text, kept.into_iter().map(|(at, _)| at).collect())
}

/// How a document came: the grants it may use, whether it arrived over the
/// wire (a URL, an OCI registry, an MCP server), where the §7.8 floor holds,
/// and what its author attested when a pinned publisher verified it.
#[derive(Debug, Clone, Copy)]
pub struct Admission<'a> {
    pub granted: &'a BTreeSet<String>,
    pub over_the_wire: bool,
    /// The families its author's signature attests, when it verified against
    /// a pinned publisher (`agent.instruction.trust`); `None` when nothing
    /// verified it.
    pub attested: Option<&'a [String]>,
}

impl Admission<'_> {
    fn admits(&self, grant: &str) -> Result<(), String> {
        if self.over_the_wire && WIRE_FLOOR.contains(&grant) {
            return Err(format!(
                "a document that arrived over the wire never uses the `{grant}` grant (§7.8); \
                 declare it in the operator's configuration"
            ));
        }
        // Over the wire, the agents a document defines run only when the
        // publisher the operator pinned stands behind them.
        if self.over_the_wire && grant == AGENTS {
            match self.attested {
                None => {
                    return Err(format!(
                        "a document that arrived over the wire uses the `{grant}` grant only \
                         when it verified against a pinned publisher (`agent.instruction.trust`) \
                         and its author's signature attests `{grant}`; this one is not verified"
                    ));
                }
                Some(a) if !a.iter().any(|c| c == grant) => {
                    return Err(format!(
                        "a document that arrived over the wire uses the `{grant}` grant only \
                         when its author's signature attests `{grant}`; this one attests {a:?}"
                    ));
                }
                Some(_) => {}
            }
        }
        if !self.granted.contains(grant) {
            return Err(format!(
                "needs the `{grant}` grant (`agent.document_capabilities: [{grant}]`)"
            ));
        }
        Ok(())
    }
}

/// Read and map a document the operator supplied.
pub fn extract(text: &str, granted: &BTreeSet<String>) -> Result<Extraction, Vec<Refusal>> {
    fold(
        &parse(text)?,
        Admission {
            granted,
            over_the_wire: false,
            attested: None,
        },
    )
}

/// [`extract`], for a document agent's instruction: `known` are the
/// `(id, kind)`s its parent document declares (its people and agents), which
/// its typed references name as the parent does — in text only.
pub fn extract_knowing(
    text: &str,
    granted: &BTreeSet<String>,
    known: &[(String, String)],
) -> Result<Extraction, Vec<Refusal>> {
    fold_knowing(
        &parse(text)?,
        Admission {
            granted,
            over_the_wire: false,
            attested: None,
        },
        known,
    )
}

/// Read and map a document that arrived over the wire (§7.8), with the
/// families its author attested when a pinned publisher verified it.
pub fn extract_from_wire(
    text: &str,
    granted: &BTreeSet<String>,
    attested: Option<&[String]>,
) -> Result<Extraction, Vec<Refusal>> {
    fold(
        &parse(text)?,
        Admission {
            granted,
            over_the_wire: true,
            attested,
        },
    )
}

/// Map a read document onto agentd's configuration: the SDK's connected
/// objects and configuration blocks that stand at the top level or inside
/// layout blocks only (`Location::in_layout`) — never inside an example, a
/// policy or any other block whose body is guidance. A block that cannot be
/// mapped refuses the whole document — no partial load; a connected object
/// agentd will not activate is a warning and stays text.
///
/// The blocks inside an `agent` are that agent's, never the document's:
/// `agent` is no layout container, so `in_layout` leaves them out here, and
/// an agent agentd runs carries them in its own instruction
/// ([`DocumentAgent`]).
pub fn fold(parsed: &Parsed, admission: Admission<'_>) -> Result<Extraction, Vec<Refusal>> {
    fold_knowing(parsed, admission, &[])
}

/// [`fold`], with `known`: the `(id, kind)`s of objects another document
/// declares — an agent's parent's people and agents — which a typed reference
/// to `#id` names as it would in that document: resolved in the agent text,
/// not `[unresolved]`, and not said as missing. Names in text, nothing more:
/// a known id is no person a gate may address and no agent to dial.
fn fold_knowing(
    parsed: &Parsed,
    admission: Admission<'_>,
    known: &[(String, String)],
) -> Result<Extraction, Vec<Refusal>> {
    let missing_known = |d: &Diagnostic| {
        d.code == "reference-optional"
            && known
                .iter()
                .any(|(id, _)| d.message.ends_with(&format!("has the id \"{id}\"")))
    };
    let cleaned = if known.is_empty() {
        parsed.text()
    } else {
        let r = &parsed.reading;
        let mut identities = r.identities.clone();
        identities.extend(
            known
                .iter()
                .map(|(id, kind)| instruction_md::types::Identity {
                    id: id.clone(),
                    kind: kind.clone(),
                    line: 0,
                }),
        );
        instruction_md::text::to_text(
            &r.document,
            &instruction_md::text::TextOptions {
                params: None,
                parameters: r.parameters.clone(),
                use_prose: true,
                identities: Some(identities),
                known: None,
            },
        )
    };
    let body = &parsed.reading.document.body;
    // The agents nested in an agent, each with the agent it is nested in:
    // its reporting line. Only through agents and layout — an agent inside
    // an example is the example's — and only agents the host runs itself
    // are cut out of the one they report to; any other stays its text.
    let mut reports: std::collections::HashMap<*const instruction_md::types::Node, String> =
        Default::default();
    let mut cuts: BTreeMap<String, Vec<(usize, usize)>> = BTreeMap::new();
    instruction_md::walk(body, &mut |n, parents| {
        if n.kind == "agent"
            && parents
                .iter()
                .all(|p| p.kind == "agent" || instruction_md::is_layout(&p.kind))
            && let Some(manager) = parents
                .iter()
                .rev()
                .find(|p| p.kind == "agent")
                .and_then(|p| p.attrs.get("id"))
                .and_then(Value::as_str)
        {
            reports.insert(n as *const _, manager.to_string());
            if runs_itself(&n.attrs)
                && let Some(r) = n.range
            {
                cuts.entry(manager.to_string())
                    .or_default()
                    .push((r.start as usize, r.end as usize));
            }
        }
        true
    });
    let mut f = Folder {
        out: Extraction {
            cleaned,
            warnings: parsed
                .diagnostics
                .iter()
                .filter(|d| !missing_known(d))
                .map(said)
                .collect(),
            ..Extraction::default()
        },
        errs: Vec::new(),
        admission,
        parameters: parsed.reading.document.metadata.front.get("parameters"),
        cuts,
    };
    for o in instruction_md::connected_objects(body) {
        if matches!(o.kind, "person" | "role" | "agent")
            && let Some(id) = o.id
        {
            f.out.known.push((id.to_string(), o.kind.to_string()));
        }
        if o.location.in_layout() {
            f.object(&o);
        } else if o.kind == "agent"
            && let Some(manager) = reports.get(&(o.node as *const _))
            && runs_itself(o.attrs)
            && let Some(id) = o.id
        {
            f.own_agent(&o, id, Some(manager));
        }
    }
    for b in instruction_md::configuration_blocks(body) {
        if b.location.in_layout() {
            f.block(&b);
        }
    }
    if f.errs.is_empty() {
        Ok(f.out)
    } else {
        Err(f.errs)
    }
}

struct Folder<'a> {
    out: Extraction,
    errs: Vec<Refusal>,
    admission: Admission<'a>,
    /// The document's declared parameters (its front matter's), which the
    /// agents it defines are written under too.
    parameters: Option<&'a Value>,
    /// The line ranges of the agents nested in each agent, by the id of the
    /// one they are nested in: cut out of its instruction.
    cuts: BTreeMap<String, Vec<(usize, usize)>>,
}

impl Folder<'_> {
    fn host_policy(&mut self, kind: &str, id: Option<&str>, line: usize, message: String) {
        let id = id.map_or_else(String::new, |i| format!(" {i:?}"));
        self.out.warnings.push(format!(
            "line {line}: {kind}{id} is not activated: {message} ({HOST_POLICY})"
        ));
    }

    fn declare(&mut self, kind: &str, attrs: &Map<String, Value>, line: usize) {
        let mut rec = attrs.clone();
        rec.insert("line".into(), Value::from(line));
        self.out
            .declarations
            .entry(kind.to_string())
            .or_default()
            .push(Value::Object(rec));
    }

    /// Record an object agentd does not activate, and say why.
    fn hold(&mut self, o: &ConnectedObject<'_>, why: String) {
        let line = o.location.line().unwrap_or(0) as usize;
        self.declare(o.kind, o.attrs, line);
        self.host_policy(o.kind, o.id, line, why);
    }

    fn object(&mut self, o: &ConnectedObject<'_>) {
        match o.kind {
            "mcp" => self.mcp(o),
            "agent" => self.agent(o),
            "skill" => self.skill(o),
            "person" | "role" => {
                if let Some(id) = o.id {
                    self.out.people.insert(id.to_string());
                }
            }
            "tool" | "service" | "source" => {
                self.declare(o.kind, o.attrs, o.location.line().unwrap_or(0) as usize);
            }
            _ => {}
        }
    }

    fn mcp(&mut self, o: &ConnectedObject<'_>) {
        let Some(id) = o.id else { return };
        let line = o.location.line().unwrap_or(0) as usize;
        // A credential the operator bound, and a resolver of the document's
        // choosing, are not agentd's to take from a document.
        for (attr, why) in [
            (
                "account",
                "an account is a credential the operator binds to a server",
            ),
            (
                "resolver",
                "agentd resolves a registry server through the operator's mcp.registries alone",
            ),
        ] {
            if o.attr(attr).is_some() {
                self.out.warnings.push(format!(
                    "line {line}: mcp {id:?}: its `{attr}` is not activated: {why} ({HOST_POLICY})"
                ));
            }
        }
        let mut server = Map::new();
        server.insert("name".into(), id.into());
        match (o.attr("registry"), o.attr("server"), o.src) {
            // By registry and name (RFC 0055 §4.2): the load holds it to the
            // operator's mcp.registries; a `src` beside them must be one of
            // the record's remotes.
            (Some(registry), Some(name), src) => {
                server.insert("registry".into(), registry.into());
                server.insert("server".into(), name.into());
                if let Some(src) = src {
                    server.insert("endpoint".into(), src.into());
                }
            }
            (Some(_), None, _) | (None, Some(_), None) => {
                return self.hold(
                    o,
                    "a server named by registry needs both `registry` (the registry's URL) and \
                     `server` (its name there)"
                        .into(),
                );
            }
            (None, _, Some(src)) => {
                server.insert("endpoint".into(), src.into());
            }
            (None, None, None) => {
                return self.hold(
                    o,
                    "agentd dials an MCP server by its URL (`src`) or by a registry and the \
                     server's name there (`registry`, `server`)"
                        .into(),
                );
            }
        }
        let mut allow = o.list("tools");
        for a in o.list("allow") {
            if !allow.contains(&a) {
                allow.push(a);
            }
        }
        if !allow.is_empty() {
            server.insert("allow".into(), allow.into());
        }
        let deny = o.list("deny");
        if !deny.is_empty() {
            server.insert("exclude".into(), deny.into());
        }
        push(&mut self.out.config, "mcp", "servers").push(Value::Object(server));
    }

    fn agent(&mut self, o: &ConnectedObject<'_>) {
        // The reader refuses an agent with no `id` (`invalid-attributes`):
        // the registry requires one.
        let Some(id) = o.id else { return };
        if runs_itself(o.attrs) {
            return self.own_agent(o, id, None);
        }
        if o.attr("protocol") != Some("a2a") {
            return self.hold(o, "agentd reaches agents over A2A (`protocol=a2a`)".into());
        }
        if let Err(why) = self.admission.admits(IDENTITY) {
            return self.hold(o, why);
        }
        if let Some(b) = o.attr("binding").filter(|b| *b != "JSONRPC") {
            return self.hold(o, format!("agentd speaks A2A's JSONRPC binding, not {b}"));
        }
        if o.attr("tenant").is_some() {
            return self.hold(
                o,
                "agentd takes an agent's tenant from its Agent Card, not from the document".into(),
            );
        }
        // agentd dials the configured URL and reads the card on its origin
        // (`/.well-known/agent-card.json`), so an endpoint is used as it is,
        // and a card at the well-known path names its origin.
        let endpoint = match (o.endpoint, o.src) {
            (Some(e), _) => e.to_string(),
            (None, Some(card)) => match card.strip_suffix("/.well-known/agent-card.json") {
                Some(origin) if origin.contains("://") => origin.to_string(),
                _ => {
                    return self.hold(
                        o,
                        "agentd finds an agent by its endpoint, or by an Agent Card at \
                         /.well-known/agent-card.json on its origin; this `src` is neither"
                            .into(),
                    );
                }
            },
            (None, None) => {
                return self.hold(
                    o,
                    "agentd dials an A2A agent at its `endpoint`, or finds it by its Agent Card \
                     (`src`); this one names neither"
                        .into(),
                );
            }
        };
        let mut peer = Map::new();
        peer.insert("name".into(), id.into());
        peer.insert("endpoint".into(), endpoint.into());
        push(&mut self.out.config, "a2a", "peers").push(Value::Object(peer));
    }

    /// An agent the document defines for agentd to run (RFC 0056): no
    /// protocol, no `src`, no `endpoint` — its body is its instruction.
    fn own_agent(&mut self, o: &ConnectedObject<'_>, id: &str, reports_to: Option<&str>) {
        if let Err(why) = self.admission.admits(AGENTS) {
            return self.hold(o, format!("an agent agentd runs itself {why}"));
        }
        let line = o.location.line().unwrap_or(0) as usize;
        // Its text is its own: the agents that report to it are cut out.
        let (body, lines) = cut_lines(
            o.body.unwrap_or_default(),
            line + 1,
            self.cuts.get(id).map_or(&[][..], Vec::as_slice),
        );
        if body.trim().is_empty() {
            return self.hold(
                o,
                "an agent agentd runs itself is a container whose body is its instruction \
                 (`::::agent{id=…} … ::::`); this one has no body"
                    .into(),
            );
        }
        // Written under the document's parameters, so a `${name}` in its
        // body reads as it does in the document.
        let (instruction, header_lines) = match self.parameters {
            Some(p) if p.as_object().is_some_and(|m| !m.is_empty()) => {
                (format!("---\nspec: \"1\"\nparameters: {p}\n---\n{body}"), 4)
            }
            _ => (body, 0),
        };
        self.out.agents.push(DocumentAgent {
            id: id.to_string(),
            title: o.title.map(str::to_string),
            instruction,
            line,
            reports_to: reports_to.map(str::to_string),
            header_lines,
            lines,
        });
    }

    fn skill(&mut self, o: &ConnectedObject<'_>) {
        let Some(id) = o.id else { return };
        if let Some(src) = o.src {
            let Some(url) = skill_url(src) else {
                return self.hold(
                    o,
                    "agentd fetches a skill's `src` only when it names a SKILL.md on GitHub \
                     (a github.com blob or tree URL, or raw.githubusercontent.com); a packaged \
                     skill is the configuration's (`skills:`)"
                        .into(),
                );
            };
            self.out.remote_skills.push(RemoteSkill {
                name: id.to_string(),
                description: o.description.or(o.title).map(str::to_string),
                when_to_use: o.attr("trigger").map(str::to_string),
                src: src.to_string(),
                url,
            });
            return;
        }
        let body = o.body.map(str::trim).unwrap_or_default();
        if body.is_empty() {
            return self.declare(o.kind, o.attrs, o.location.line().unwrap_or(0) as usize);
        }
        self.out.skills.push(InlineSkill {
            name: id.to_string(),
            description: o.description.or(o.title).unwrap_or_default().to_string(),
            when_to_use: o.attr("trigger").map(str::to_string),
            body: body.to_string(),
            fetched_from: None,
        });
    }

    /// A workflow or a configuration block. Its body's shape is the reader's
    /// to judge (a structured body that does not parse, or is no mapping,
    /// is its error), and an `instruction/workflow` body's is the SDK's
    /// (`invalid-workflow`, `limit-exceeded`, `extension-unavailable`);
    /// what the body asks of this host — a start on an event agentd raises,
    /// a server it declares — is the engine's, at load.
    fn block(&mut self, b: &ConfigurationBlock<'_>) {
        let line = b.location.line().unwrap_or(0) as usize;
        let dialect = b.dialect.unwrap_or_default();
        let (wanted, what) = match b.kind {
            "workflow" => (WORKFLOW_DIALECTS.contains(&dialect), "workflow"),
            "config" => (dialect == CONFIG_DIALECT, "config"),
            _ => return,
        };
        if !wanted {
            self.declare(b.kind, b.attrs, line);
            let runs = if what == "workflow" {
                format!(
                    "agentd runs the {} dialects",
                    WORKFLOW_DIALECTS.join(" and ")
                )
            } else {
                format!("agentd reads {CONFIG_DIALECT}")
            };
            return self.host_policy(b.kind, b.id, line, format!("{runs}, not {dialect:?}"));
        }
        let Some(Value::Object(m)) = b.data else {
            return;
        };
        let m = m.clone();
        // A workflow's `name` is the language's to require, and the SDK's
        // check does; the block's `id` is its instruction identity.
        if what == "workflow" {
            self.out.workflows.push(Value::Object(m));
            return;
        }
        // The document cannot grant itself trust: its grants and its own
        // source are the operator's.
        for key in [
            "document_capabilities",
            "instruction_sources",
            "instruction",
        ] {
            if m.contains_key(key) {
                self.errs.push(Refusal::at(
                    line,
                    "self-grant",
                    format!(
                        "a config block may not write `{key}` — it is operator configuration, \
                         not a document's to set"
                    ),
                ));
            }
        }
        merge_missing(&mut self.out.config, m);
    }
}

fn push<'a>(cfg: &'a mut Map<String, Value>, section: &str, key: &str) -> &'a mut Vec<Value> {
    let s = cfg
        .entry(section)
        .or_insert_with(|| Value::Object(Map::new()));
    if !s.is_object() {
        *s = Value::Object(Map::new());
    }
    let list = s
        .as_object_mut()
        .expect("an object")
        .entry(key)
        .or_insert_with(|| Value::Array(Vec::new()));
    if !list.is_array() {
        *list = Value::Array(Vec::new());
    }
    list.as_array_mut().expect("an array")
}

/// Merge a fragment UNDER a document: a key already present in `into` wins,
/// so an explicit config key always beats what a block contributed. Arrays
/// of the same key concatenate (fragment first) so a document's servers add
/// to, rather than replace, any `mcp.servers` written explicitly.
pub fn merge_missing(into: &mut Map<String, Value>, add: Map<String, Value>) {
    for (k, v) in add {
        match (into.get_mut(&k), v) {
            (Some(Value::Object(d)), Value::Object(s)) => merge_missing(d, s),
            (Some(Value::Array(have)), Value::Array(mut more)) => {
                more.extend(std::mem::take(have));
                *have = more;
            }
            (Some(_), _) => {}
            (None, v) => {
                into.insert(k, v);
            }
        }
    }
}

/// The raw `SKILL.md` a skill's `src` names, or `None` for a `src` agentd
/// does not fetch. Three spellings of one file on GitHub, all normalised to
/// the raw URL:
///
/// - `https://github.com/{owner}/{repo}/blob/{ref}/{path}/SKILL.md`
/// - `https://github.com/{owner}/{repo}/tree/{ref}/{dir}` (its `SKILL.md`)
/// - `https://raw.githubusercontent.com/{owner}/{repo}/{ref}/{path}/SKILL.md`
///
/// A test build also takes a plain `http://` loopback URL ending in
/// `/SKILL.md`, the one plaintext fetch agentd's URL reader makes, so an e2e
/// can serve one without the internet.
pub fn skill_url(src: &str) -> Option<String> {
    let clean = |segments: &[&str]| {
        segments
            .iter()
            .all(|s| !s.is_empty() && *s != "." && *s != ".." && !s.contains(['?', '#', '\\']))
    };
    if let Some(rest) = src.strip_prefix("https://github.com/") {
        let parts: Vec<&str> = rest.trim_end_matches('/').split('/').collect();
        let [owner, repo, kind, git_ref, path @ ..] = parts.as_slice() else {
            return None;
        };
        if path.is_empty() || !clean(&parts) {
            return None;
        }
        let file = match *kind {
            "blob" if path.last() == Some(&"SKILL.md") => path.join("/"),
            "tree" if path.last() != Some(&"SKILL.md") => format!("{}/SKILL.md", path.join("/")),
            _ => return None,
        };
        return Some(format!(
            "https://raw.githubusercontent.com/{owner}/{repo}/{git_ref}/{file}"
        ));
    }
    if let Some(rest) = src.strip_prefix("https://raw.githubusercontent.com/") {
        let parts: Vec<&str> = rest.split('/').collect();
        return (parts.len() >= 4 && parts.last() == Some(&"SKILL.md") && clean(&parts))
            .then(|| src.to_string());
    }
    #[cfg(any(debug_assertions, feature = "internal-mocks"))]
    if let Some(rest) = src.strip_prefix("http://") {
        let (host, path) = rest.split_once('/')?;
        let host = host.rsplit_once(':').map_or(host, |(h, _)| h);
        if (host == "127.0.0.1" || host == "localhost")
            && path.ends_with("SKILL.md")
            && clean(&path.split('/').collect::<Vec<_>>())
        {
            return Some(src.to_string());
        }
    }
    None
}

/// A digest of what a document's machinery maps onto — everything an
/// [`Extraction`] carries but the agent text and the diagnostics — or `None`
/// when it maps onto nothing. Two versions whose machinery is the same digest
/// the same however their prose moved, so a text-only edit is told apart from
/// one that adds a server. Canonical: object keys serialize sorted.
pub fn machinery_digest(ex: &Extraction) -> Option<String> {
    if ex.config.is_empty()
        && ex.workflows.is_empty()
        && ex.skills.is_empty()
        && ex.remote_skills.is_empty()
        && ex.declarations.is_empty()
        && ex.people.is_empty()
        && ex.agents.is_empty()
    {
        return None;
    }
    let skills: Vec<Value> = ex
        .skills
        .iter()
        .map(|s| {
            serde_json::json!({"name": s.name, "description": s.description,
                               "when_to_use": s.when_to_use, "body": s.body})
        })
        .chain(ex.remote_skills.iter().map(|s| {
            serde_json::json!({"name": s.name, "description": s.description,
                               "when_to_use": s.when_to_use, "src": s.src})
        }))
        .collect();
    let all = serde_json::json!({
        "config": ex.config,
        "workflows": ex.workflows,
        "skills": skills,
        "declarations": ex.declarations,
        "people": ex.people,
        "agents": ex.agents.iter().map(|a| {
            serde_json::json!({"id": a.id, "title": a.title, "instruction": a.instruction,
                               "reports_to": a.reports_to})
        }).collect::<Vec<_>>(),
    });
    Some(instruction_md::digest(all.to_string().as_bytes()))
}

/// What a document's mapping would give it of the floor's grants through
/// blocks every document may use: `a2a.peers` written by a config block, and
/// a secret named anywhere in its configuration or workflows
/// (`{{secret:…}}`, `{{secret-file:…}}`) — which a header or a step would
/// then send wherever the document points it. Empty when it reaches neither.
pub fn wire_floor_effects(ex: &Extraction) -> Vec<String> {
    let mut out = Vec::new();
    if ex
        .config
        .get("a2a")
        .and_then(|a| a.get("peers"))
        .is_some_and(|p| !p.is_null())
    {
        out.push("writes a2a.peers".to_string());
    }
    let names_a_secret = |v: &Value| v.to_string().contains("{{secret");
    if names_a_secret(&Value::Object(ex.config.clone())) || ex.workflows.iter().any(names_a_secret)
    {
        out.push("names a secret".to_string());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOME: &str = "---\nspec: \"1\"\n---\n\
        You keep the flat warm with the :mcp[thermostat]{ref=#thermostat}.\n\n\
        ::person{id=nataliia title=\"Nataliia\"}\n\n\
        :::mcp{id=thermostat title=\"Thermostat\" src=\"https://home.example/mcp/thermostat\" tools=\"rooms.read,target.set\" deny=\"target.set\"}\n\
        Change one room at a time.\n\
        :::\n\n\
        ::agent{id=home title=\"Home agent\" protocol=a2a src=\"https://home.example/.well-known/agent-card.json\"}\n\n\
        :::workflow{id=warm dialect=\"instruction/workflow\"}\n\
        name: warm\n\
        steps:\n  start: {kind: manual}\n  done: {kind: finish, depends_on: [start]}\n\
        :::\n";

    #[test]
    fn connected_objects_become_servers_peers_and_workflows() {
        let granted = BTreeSet::from([IDENTITY.to_string()]);
        let ex = extract(HOME, &granted).unwrap();
        assert_eq!(
            ex.config["mcp"]["servers"],
            serde_json::json!([{"name": "thermostat", "endpoint": "https://home.example/mcp/thermostat",
                                "allow": ["rooms.read", "target.set"], "exclude": ["target.set"]}])
        );
        assert_eq!(
            ex.config["a2a"]["peers"],
            serde_json::json!([{"name": "home", "endpoint": "https://home.example"}])
        );
        assert_eq!(ex.workflows[0]["name"], "warm");
        assert!(ex.people.contains("nataliia"));
        assert!(ex.warnings.is_empty(), "{:?}", ex.warnings);
        // The agent text is the SDK's, byte for byte.
        assert_eq!(ex.cleaned, instruction_md::deliver(HOME, None).0);
    }

    /// An agent outside the grants, or from the wire, stays text and is
    /// said — never activated, never removed (§17).
    #[test]
    fn an_ungranted_agent_stays_text_with_a_host_policy_diagnostic() {
        for (granted, wire) in [
            (BTreeSet::new(), false),
            (BTreeSet::from([IDENTITY.to_string()]), true),
        ] {
            let parsed = parse(HOME).unwrap();
            let ex = fold(
                &parsed,
                Admission {
                    granted: &granted,
                    over_the_wire: wire,
                    attested: None,
                },
            )
            .unwrap();
            assert!(ex.config.get("a2a").is_none(), "{:?}", ex.config);
            assert_eq!(ex.warnings.len(), 1, "{:?}", ex.warnings);
            assert!(
                ex.warnings[0].contains("(host-policy)"),
                "{:?}",
                ex.warnings
            );
            assert!(ex.cleaned.contains("Home agent"), "{}", ex.cleaned);
            assert_eq!(ex.declarations["agent"].len(), 1);
        }
    }

    #[test]
    fn a_reader_error_refuses_the_document_naming_its_line() {
        let e = extract(":::mcp{id=x}\nunclosed\n", &all_families()).unwrap_err();
        assert!(e.iter().any(|r| r.code == "unclosed-directive"), "{e:?}");
        assert!(e[0].to_string().starts_with("line 1: "), "{e:?}");
    }

    /// A 1.1 `:::!kind` line refuses the document with the SDK's code.
    #[test]
    fn a_legacy_line_refuses_the_document() {
        let e = extract(
            ":::!mcp{name=x}\nendpoint: https://x.example/mcp\n:::\n",
            &all_families(),
        )
        .unwrap_err();
        assert!(
            e.iter()
                .any(|r| r.code == instruction_md::LEGACY_SYNTAX && r.line == Some(1)),
            "{e:?}"
        );
    }

    #[test]
    fn a_config_block_cannot_grant_itself_trust() {
        let e = extract(
            ":::config{id=c dialect=\"agentd/config\"}\ndocument_capabilities: [identity]\n:::\n",
            &all_families(),
        )
        .unwrap_err();
        assert_eq!(e[0].code, "self-grant", "{e:?}");
    }

    #[test]
    fn an_unknown_dialect_is_a_host_policy_diagnostic() {
        let ex = extract(
            ":::workflow{id=w dialect=\"other/workflow\"}\nname: w\n:::\n",
            &all_families(),
        )
        .unwrap();
        assert!(ex.workflows.is_empty());
        assert!(
            ex.warnings[0].contains("other/workflow"),
            "{:?}",
            ex.warnings
        );
    }

    #[test]
    fn an_inline_skill_carries_its_body() {
        let ex = extract(
            ":::skill{id=refunds title=\"Refunds\" trigger=\"a customer asks for a refund\"}\nRefund within 30 days.\n:::\n",
            &all_families(),
        )
        .unwrap();
        assert_eq!(
            ex.skills,
            [InlineSkill {
                name: "refunds".into(),
                description: "Refunds".into(),
                when_to_use: Some("a customer asks for a refund".into()),
                body: "Refund within 30 days.".into(),
                fetched_from: None,
            }]
        );
    }

    #[test]
    fn a_skill_src_on_github_names_its_raw_skill_md() {
        let raw = "https://raw.githubusercontent.com/anthropics/skills/main/skills/frontend-design/SKILL.md";
        for src in [
            "https://github.com/anthropics/skills/blob/main/skills/frontend-design/SKILL.md",
            "https://github.com/anthropics/skills/tree/main/skills/frontend-design",
            "https://github.com/anthropics/skills/tree/main/skills/frontend-design/",
            raw,
        ] {
            assert_eq!(skill_url(src).as_deref(), Some(raw), "{src}");
        }
        for src in [
            // Not a SKILL.md, or not on GitHub, or not https.
            "https://github.com/anthropics/skills/blob/main/skills/frontend-design/README.md",
            "https://github.com/anthropics/skills/tree/main/skills/x/SKILL.md",
            "https://github.com/anthropics/skills",
            "https://github.com/anthropics/skills/blob/main",
            "https://example.com/skills/x/SKILL.md",
            "http://github.com/anthropics/skills/blob/main/x/SKILL.md",
            "http://raw.githubusercontent.com/a/b/main/x/SKILL.md",
            "https://raw.githubusercontent.com/a/b/SKILL.md",
            "https://github.com/a/b/blob/main/../x/SKILL.md",
            "https://raw.githubusercontent.com/a/b/main/x/SKILL.md?token=1",
            "http://10.0.0.1/x/SKILL.md",
            "mcp://server/skill",
        ] {
            assert_eq!(skill_url(src), None, "{src}");
        }
    }

    #[test]
    fn a_skill_with_a_github_src_is_fetched_and_another_src_is_held() {
        let ex = extract(
            "::skill{id=frontend-design title=\"frontend-design\" src=\"https://github.com/anthropics/skills/blob/main/skills/frontend-design/SKILL.md\"}\n\n\
             ::skill{id=other title=\"Other\" src=\"https://example.com/x.zip\"}\n",
            &all_families(),
        )
        .unwrap();
        assert_eq!(ex.remote_skills.len(), 1);
        assert_eq!(ex.remote_skills[0].name, "frontend-design");
        assert!(
            ex.remote_skills[0]
                .url
                .starts_with("https://raw.githubusercontent.com/")
        );
        assert!(ex.skills.is_empty());
        assert_eq!(ex.declarations["skill"].len(), 1);
        assert!(
            ex.warnings
                .iter()
                .any(|w| w.contains("\"other\"") && w.contains(HOST_POLICY)),
            "{:?}",
            ex.warnings
        );
    }

    /// Blocks inside an example are an example: nothing folds from them.
    #[test]
    fn nothing_folds_from_inside_an_example() {
        let ex = extract(
            "::::example{title=\"A server\"}\n:::mcp{id=x src=\"https://x.example/mcp\"}\nx\n:::\n::::\n",
            &all_families(),
        )
        .unwrap();
        assert!(ex.config.is_empty(), "{:?}", ex.config);
    }

    #[test]
    fn plain_prose_needs_no_delivery_and_a_block_does() {
        assert!(!needs_delivery("be terse\n"));
        assert!(!needs_delivery("be terse"));
        // A rule already in its canonical form is its own agent text.
        assert!(!needs_delivery("MUST: be terse.\n"));
        assert!(needs_delivery("ALWAYS: be terse.\n"));
        assert!(needs_delivery("<!-- a note -->\nbe terse\n"));
        assert!(needs_delivery("---\nspec: \"1\"\n---\nbe terse\n"));
    }

    #[test]
    fn backmatter_splits_off() {
        let text = "Be terse.\n\n---\nauthors: [Alex]\n";
        let s = split_end_matter(text).unwrap();
        assert_eq!(s.before, "Be terse.\n\n");
        assert_eq!(s.opener_line, Some(3));
        assert_eq!(
            s.end_matter.unwrap()["authors"],
            serde_json::json!(["Alex"])
        );
        assert_eq!(
            split_end_matter("Be terse.\n").unwrap().before,
            "Be terse.\n"
        );
    }

    #[test]
    fn the_wire_floor_sees_peers_and_secrets_a_config_block_writes() {
        let all = all_families();
        let peers = extract(
            ":::config{id=c dialect=\"agentd/config\"}\na2a:\n  peers:\n    - {name: p, endpoint: \"https://p.example\"}\n:::\n",
            &all,
        )
        .unwrap();
        assert_eq!(wire_floor_effects(&peers), ["writes a2a.peers"]);
        let secret = extract(
            ":::config{id=c dialect=\"agentd/config\"}\nmcp:\n  servers:\n    - {name: s, endpoint: \"https://s.example/mcp\", headers: {Authorization: \"Bearer {{secret:TOKEN}}\"}}\n:::\n",
            &all,
        )
        .unwrap();
        assert_eq!(wire_floor_effects(&secret), ["names a secret"]);
    }

    /// A document with one agent agentd runs, whose body carries a server,
    /// a skill and a workflow of its own.
    const TEAM: &str = "---\nspec: \"1\"\n---\n# The company\n\n\
        ::::agent{id=support title=\"Support lead\"}\n\
        You answer every ticket.\n\n\
        :::mcp{id=desk title=\"Helpdesk\" src=\"https://desk.example/mcp\"}\nThe tickets.\n:::\n\n\
        :::skill{id=triage title=\"Triage\" trigger=\"a new ticket\"}\nRead it all.\n:::\n\n\
        :::workflow{id=queue dialect=\"instruction/workflow\"}\n\
        name: queue\nsteps:\n  start: {kind: once}\n  done: {kind: finish, depends_on: [start]}\n:::\n\
        ::::\n";

    fn agents() -> BTreeSet<String> {
        BTreeSet::from([AGENTS.to_string()])
    }

    /// Under the grant, the agent is the document's to define: its body is
    /// its instruction, and nothing inside it lands on the parent.
    #[test]
    fn a_granted_agent_is_defined_and_its_blocks_stay_its_own() {
        let ex = extract(TEAM, &agents()).unwrap();
        assert_eq!(ex.agents.len(), 1, "{:?}", ex.warnings);
        let a = &ex.agents[0];
        assert_eq!(a.id, "support");
        assert_eq!(a.title.as_deref(), Some("Support lead"));
        assert_eq!(a.line, 6);
        assert!(a.instruction.starts_with("You answer every ticket."));
        assert!(
            a.instruction.contains(":::mcp{id=desk"),
            "{}",
            a.instruction
        );
        assert!(a.instruction.contains("name: queue"), "{}", a.instruction);
        assert!(ex.config.is_empty(), "{:?}", ex.config);
        assert!(ex.workflows.is_empty(), "{:?}", ex.workflows);
        assert!(ex.skills.is_empty(), "{:?}", ex.skills);
        assert!(ex.warnings.is_empty(), "{:?}", ex.warnings);
        // The body is the agent's own document, and reads as one.
        let child = extract(&a.instruction, &BTreeSet::new()).unwrap();
        assert_eq!(child.config["mcp"]["servers"][0]["name"], "desk");
        assert_eq!(child.workflows[0]["name"], "queue");
        assert_eq!(child.skills[0].name, "triage");
    }

    /// Without the grant the agent stays text, said with the grant it
    /// needs, and its blocks are not the parent's either.
    #[test]
    fn an_agent_without_the_grant_is_held_naming_the_grant() {
        let ex = extract(TEAM, &BTreeSet::from([IDENTITY.to_string()])).unwrap();
        assert!(ex.agents.is_empty());
        assert!(ex.config.is_empty() && ex.workflows.is_empty() && ex.skills.is_empty());
        assert_eq!(ex.declarations["agent"].len(), 1);
        assert_eq!(ex.warnings.len(), 1, "{:?}", ex.warnings);
        assert!(
            ex.warnings[0].contains("\"support\"")
                && ex.warnings[0].contains("agent.document_capabilities: [agents]")
                && ex.warnings[0].contains(HOST_POLICY),
            "{:?}",
            ex.warnings
        );
        assert!(
            ex.cleaned.contains("You answer every ticket."),
            "{}",
            ex.cleaned
        );
    }

    /// An agent with no id refuses the document (the reader requires one);
    /// one with no body has no instruction to run, and says so.
    #[test]
    fn an_agent_without_an_id_refuses_and_one_without_a_body_is_said() {
        let e = extract("::agent{title=\"Nobody\"}\n", &agents()).unwrap_err();
        assert!(
            e.iter()
                .any(|r| r.code == "invalid-attributes" && r.message.contains("id")),
            "{e:?}"
        );
        let ex = extract("::agent{id=empty}\n", &agents()).unwrap();
        assert!(ex.agents.is_empty());
        assert_eq!(ex.warnings.len(), 1, "{:?}", ex.warnings);
        assert!(
            ex.warnings[0].contains("\"empty\"") && ex.warnings[0].contains("no body"),
            "{:?}",
            ex.warnings
        );
    }

    /// Over the wire, `agents` needs a pinned publisher's signature that
    /// attests it: unpinned is refused, attested without it is refused.
    #[test]
    fn over_the_wire_an_agent_needs_a_pin_that_attests_agents() {
        let refused = |attested: Option<&[String]>, why: &str| {
            let ex = extract_from_wire(TEAM, &agents(), attested).unwrap();
            assert!(ex.agents.is_empty());
            assert!(
                ex.warnings.iter().any(|w| w.contains(why)),
                "{why}: {:?}",
                ex.warnings
            );
        };
        refused(None, "is not verified");
        refused(Some(&["compute".to_string()]), "attests [\"compute\"]");
        let ex = extract_from_wire(TEAM, &agents(), Some(&[AGENTS.to_string()])).unwrap();
        assert_eq!(ex.agents.len(), 1, "{:?}", ex.warnings);
    }

    /// An agent is written under its document's parameters, and what is said
    /// about its instruction names it, at the parent's line.
    #[test]
    fn an_agent_reads_its_documents_parameters_and_reports_at_the_parents_line() {
        let doc = "---\nspec: \"1\"\nparameters:\n  voice: { type: string, default: warm }\n---\n\
            ::::agent{id=cmo}\nWrite in a ${voice} voice.\n${missing}\n::::\n";
        let ex = extract(doc, &agents()).unwrap();
        let a = &ex.agents[0];
        assert_eq!(a.line, 6);
        let child = extract(&a.instruction, &BTreeSet::new()).unwrap();
        assert!(
            child.cleaned.contains("Write in a warm voice."),
            "{}",
            child.cleaned
        );
        let w = child
            .warnings
            .iter()
            .find(|w| w.contains("missing"))
            .expect("an undeclared parameter is said");
        assert!(
            a.in_parent(w).starts_with("agent \"cmo\": line 8:"),
            "{}",
            a.in_parent(w)
        );
    }

    /// A changed agent moves the machinery digest, so a served document whose
    /// agents moved is applied.
    #[test]
    fn an_agent_moves_the_machinery_digest() {
        let one = extract(TEAM, &agents()).unwrap();
        let two = extract(&TEAM.replace("every ticket", "each ticket"), &agents()).unwrap();
        assert!(machinery_digest(&one).is_some());
        assert_ne!(machinery_digest(&one), machinery_digest(&two));
    }

    /// A manager with one report, and the report's own report (depth 2).
    fn org(extra: &str) -> String {
        format!(
            "---\nspec: \"1\"\n---\n# Org\n\n\
             :::::agent{{id=cmo title=\"Chief marketing officer\"}}\n\
             You run marketing.\n\n\
             ::::agent{{id=community title=\"Community lead\"}}\n\
             You run the forum.\n\n\
             :::agent{{id=moderator}}\nYou keep the forum civil.\n:::\n\
             ::::\n{extra}\
             :::::\n"
        )
    }

    /// An agent nested in an agent is its own agent, labelled with the one
    /// it reports to, and its text is cut out of that one's — at any depth.
    #[test]
    fn a_nested_agent_reports_to_its_manager_and_its_text_is_its_own() {
        let ex = extract(&org(""), &agents()).unwrap();
        let by = |id: &str| ex.agents.iter().find(|a| a.id == id).unwrap();
        assert_eq!(ex.agents.len(), 3, "{:?}", ex.warnings);
        assert_eq!(by("cmo").reports_to, None);
        assert_eq!(by("community").reports_to.as_deref(), Some("cmo"));
        assert_eq!(by("moderator").reports_to.as_deref(), Some("community"));
        assert_eq!(by("cmo").instruction, "You run marketing.\n");
        assert_eq!(by("community").instruction, "You run the forum.\n");
        assert_eq!(by("moderator").instruction, "You keep the forum civil.\n");
        assert!(ex.warnings.is_empty(), "{:?}", ex.warnings);
        // A report's lines are its own, at the parent's.
        assert_eq!(by("moderator").line, 12);
        assert_eq!(
            by("moderator").in_parent("line 1: x"),
            "agent \"moderator\": line 13: x"
        );
    }

    /// A manager's FIRST report, added, leaves its instruction byte for byte
    /// as it was with none: blank lines are normalized with cuts or without.
    #[test]
    fn a_first_report_leaves_its_managers_text_byte_identical() {
        let alone = "---\nspec: \"1\"\n---\n:::::agent{id=cmo}\nYou run marketing.\n\n:::::\n";
        let with = "---\nspec: \"1\"\n---\n:::::agent{id=cmo}\nYou run marketing.\n\n\
            ::::agent{id=writer}\nYou write.\n::::\n\n:::::\n";
        let cmo = |doc: &str| {
            extract(doc, &agents())
                .unwrap()
                .agents
                .into_iter()
                .find(|a| a.id == "cmo")
                .unwrap()
                .instruction
        };
        assert_eq!(cmo(alone).as_bytes(), cmo(with).as_bytes());
        assert_eq!(cmo(alone), "You run marketing.\n");
    }

    /// A report added under a manager leaves the manager's instruction as it
    /// was: the new agent is cut out, and with it the blank line before it.
    #[test]
    fn a_report_added_leaves_its_managers_text_as_it_was() {
        let one = extract(&org(""), &agents()).unwrap();
        let two = extract(
            &org("\n::::agent{id=writer title=\"Writer\"}\nYou write.\n::::\n"),
            &agents(),
        )
        .unwrap();
        let cmo = |ex: &Extraction| ex.agents.iter().find(|a| a.id == "cmo").unwrap().clone();
        assert_eq!(cmo(&one).instruction, cmo(&two).instruction);
        let writer = two.agents.iter().find(|a| a.id == "writer").unwrap();
        assert_eq!(writer.reports_to.as_deref(), Some("cmo"));
    }

    /// One id twice in a document is the reader's error, nested or not.
    #[test]
    fn a_duplicate_agent_id_refuses_the_document() {
        let e = extract(&org("\n::::agent{id=community}\nAgain.\n::::\n"), &agents()).unwrap_err();
        assert!(e.iter().any(|r| r.code == "duplicate-id"), "{e:?}");
    }

    /// An agent's references to its document's people and agents resolve in
    /// its text — names only — and are not said as missing.
    #[test]
    fn an_agents_references_to_its_documents_people_and_agents_resolve() {
        let doc = "---\nspec: \"1\"\n---\n::person{id=andrii title=\"Andrii\"}\n\n\
            ::::agent{id=cfo}\nAsk :person[Andrii]{ref=#andrii}; tell the :agent[CTO]{ref=#cto}.\n::::\n\n\
            ::::agent{id=cto}\nShip it.\n::::\n";
        let ex = extract(doc, &agents()).unwrap();
        let cfo = &ex
            .agents
            .iter()
            .find(|a| a.id == "cfo")
            .unwrap()
            .instruction;
        let plain = extract(cfo, &BTreeSet::new()).unwrap();
        assert!(plain.cleaned.contains("[unresolved]"), "{}", plain.cleaned);
        assert_eq!(plain.warnings.len(), 2, "{:?}", plain.warnings);
        let knowing = extract_knowing(cfo, &BTreeSet::new(), &ex.known).unwrap();
        assert!(
            !knowing.cleaned.contains("[unresolved]"),
            "{}",
            knowing.cleaned
        );
        assert!(
            knowing.cleaned.contains("Andrii (person; #andrii)"),
            "{}",
            knowing.cleaned
        );
        assert!(knowing.warnings.is_empty(), "{:?}", knowing.warnings);
        // Names, not people: no gate may address a known id.
        assert!(knowing.people.is_empty());
    }

    /// A workflow whose steps are one YAML flow mapping on a line reads as
    /// the block form does.
    #[test]
    fn a_workflow_written_as_one_flow_mapping_reads() {
        let ex = extract(
            ":::workflow{id=week dialect=\"instruction/workflow\"}\nname: week\n\
             steps: { start: { kind: once }, draft: { kind: agent, depends_on: [start], instruction: \"Draft it.\" }, done: { kind: finish, depends_on: [draft] } }\n:::\n",
            &BTreeSet::new(),
        )
        .unwrap();
        assert!(ex.warnings.is_empty(), "{:?}", ex.warnings);
        assert_eq!(
            ex.workflows[0]["steps"]["draft"]["depends_on"],
            serde_json::json!(["start"])
        );
        crate::engine::model::parse_workflow(&ex.workflows[0]).unwrap();
    }
}
