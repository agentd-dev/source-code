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
//! | `:::workflow{dialect="instruction/workflow"}` | a `workflows[]` entry (the body is the definition; `name` defaults to the `id`) |
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

use instruction_md::{Diagnostic, Node, ReadOptions, Reading, Severity};
use serde_json::{Map, Value};

pub use instruction_md::Document;

/// The grant a document needs for a workflow that opens an inbound route on
/// the operator's webhook listener.
pub const INTERFACE: &str = "interface";
/// The grant a document needs for its A2A agents to become `a2a.peers`.
pub const IDENTITY: &str = "identity";
/// agentd's document grants (`agent.document_capabilities`). instruction/1
/// grants nothing by itself (§19): these are the host's.
pub const FAMILIES: &[&str] = &[INTERFACE, IDENTITY];
/// The grants a document that arrived over the wire never uses, whatever
/// the operator granted or its author signed (§7.8).
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
    /// The SDK's diagnostics — the reader's and the inline checks' — none of
    /// them an error.
    pub diagnostics: Vec<Diagnostic>,
}

impl Parsed {
    /// The agent text (`instruction-text/1`), declared defaults substituted.
    pub fn text(&self) -> String {
        self.reading.text(None)
    }
}

/// Read a document with the SDK. An error among its diagnostics refuses the
/// whole document; warnings are kept on the result.
pub fn parse(text: &str) -> Result<Parsed, Vec<Refusal>> {
    let reading = instruction_md::read(text, &ReadOptions::default());
    let diagnostics = instruction_md::check_reading(&reading);
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
/// line, comment, metadata, reference or parameter. Plain prose is used as it
/// is, byte for byte; the SDK's text ends in one newline, which alone is not
/// a change.
pub fn needs_delivery(text: &str) -> bool {
    let (agent, diagnostics) = instruction_md::deliver(text, None);
    !diagnostics.is_empty() || agent.trim_end_matches('\n') != text.trim_end_matches('\n')
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

/// Split a whole document at its backmatter, as the SDK recognises it: for a
/// reader that needs the text without its record (a folder of documents, a
/// prompt). Backmatter that is not a YAML mapping is refused.
pub fn split_end_matter(text: &str) -> Result<SplitEndMatter<'_>, Refusal> {
    let r = instruction_md::read(text, &ReadOptions::default());
    let Some(range) = r.back_range else {
        return Ok(SplitEndMatter {
            before: text,
            end_matter: None,
            opener_line: None,
        });
    };
    if let Some(d) = r.diagnostics.iter().find(|d| {
        d.severity == Severity::Error
            && d.code == "invalid-metadata"
            && d.line().is_some_and(|l| l >= range.start)
    }) {
        return Err(Refusal::of(d));
    }
    // The range starts at the `---` separator.
    let opener = range.start.max(1) as usize;
    let cut = text
        .split_inclusive('\n')
        .take(opener - 1)
        .map(str::len)
        .sum::<usize>();
    Ok(SplitEndMatter {
        before: &text[..cut],
        end_matter: Some(r.document.metadata.back.clone()),
        opener_line: Some(opener),
    })
}

/// A skill lifted from the document into the catalogue.
#[derive(Debug, Clone, PartialEq)]
pub struct InlineSkill {
    pub name: String,
    pub description: String,
    pub when_to_use: Option<String>,
    pub body: String,
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
    /// Connected objects recorded by kind (visible in `--capabilities`),
    /// activated by nothing.
    pub declarations: BTreeMap<String, Vec<Value>>,
    /// The ids of the document's `person` and `role` objects.
    pub people: BTreeSet<String>,
    /// The SDK's warnings and agentd's host-policy diagnostics, each
    /// `line N: message (code)`.
    pub warnings: Vec<String>,
}

/// The host-policy diagnostic code (§17, §18: a host's policy, distinct from
/// format invalidity).
pub const HOST_POLICY: &str = "host-policy";

/// How a document came: the grants it may use, and whether it arrived over
/// the wire (a URL, an OCI registry, an MCP server), where the §7.8 floor
/// holds.
#[derive(Debug, Clone, Copy)]
pub struct Admission<'a> {
    pub granted: &'a BTreeSet<String>,
    pub over_the_wire: bool,
}

impl Admission<'_> {
    fn admits(&self, grant: &str) -> Result<(), String> {
        if self.over_the_wire && WIRE_FLOOR.contains(&grant) {
            return Err(format!(
                "a document that arrived over the wire never uses the `{grant}` grant (§7.8); \
                 declare it in the operator's configuration"
            ));
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
        },
    )
}

/// Read and map a document that arrived over the wire (§7.8).
pub fn extract_from_wire(
    text: &str,
    granted: &BTreeSet<String>,
) -> Result<Extraction, Vec<Refusal>> {
    fold(
        &parse(text)?,
        Admission {
            granted,
            over_the_wire: true,
        },
    )
}

/// Map a read document onto agentd's configuration. A block that cannot be
/// mapped refuses the whole document — no partial load; a connected object
/// agentd will not activate is a warning and stays text.
pub fn fold(parsed: &Parsed, admission: Admission<'_>) -> Result<Extraction, Vec<Refusal>> {
    let lines: Vec<&str> = parsed.reading.lines.iter().map(String::as_str).collect();
    let mut f = Folder {
        out: Extraction {
            cleaned: parsed.text(),
            warnings: parsed.diagnostics.iter().map(said).collect(),
            ..Extraction::default()
        },
        errs: Vec::new(),
        admission,
        lines: &lines,
    };
    for n in &parsed.reading.document.body {
        f.node(n);
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
    lines: &'a [&'a str],
}

fn line_of(n: &Node) -> usize {
    n.range.map_or(0, |r| r.start as usize)
}

/// A string attribute, trimmed, when it is not empty.
fn attr<'n>(n: &'n Node, key: &str) -> Option<&'n str> {
    n.attr_str(key).map(str::trim).filter(|s| !s.is_empty())
}

/// A list attribute (the reader splits `tools="a,b"` into a list).
fn list(n: &Node, key: &str) -> Vec<Value> {
    match n.attrs.get(key) {
        Some(Value::Array(a)) => a.clone(),
        Some(Value::String(s)) => s
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|s| Value::String(s.to_string()))
            .collect(),
        _ => Vec::new(),
    }
}

impl Folder<'_> {
    fn host_policy(&mut self, n: &Node, message: String) {
        let id = attr(n, "id").map_or_else(String::new, |i| format!(" {i:?}"));
        self.out.warnings.push(format!(
            "line {}: {}{id} is not activated: {message} ({HOST_POLICY})",
            line_of(n),
            n.kind
        ));
    }

    fn declare(&mut self, n: &Node) {
        let mut rec = n.attrs.clone();
        rec.insert("line".into(), Value::from(line_of(n)));
        self.out
            .declarations
            .entry(n.kind.clone())
            .or_default()
            .push(Value::Object(rec));
    }

    /// A container's body as written, between its fences.
    fn body(&self, n: &Node) -> String {
        let Some(r) = n.range else {
            return String::new();
        };
        let (from, to) = (r.start as usize, r.end as usize);
        if to <= from + 1 {
            return String::new();
        }
        self.lines
            .get(from..to - 1)
            .map(|ls| ls.join("\n"))
            .unwrap_or_default()
            .trim()
            .to_string()
    }

    fn node(&mut self, n: &Node) {
        if !n.is_directive() {
            return;
        }
        match n.kind.as_str() {
            "mcp" => self.mcp(n),
            "agent" => self.agent(n),
            "workflow" => self.workflow(n),
            "config" => self.config(n),
            "skill" => self.skill(n),
            "person" | "role" => {
                if let Some(id) = attr(n, "id") {
                    self.out.people.insert(id.to_string());
                }
            }
            "tool" | "service" | "source" => self.declare(n),
            kind if instruction_md::registry::kind(kind).is_some_and(|k| k.family == "layout") => {
                for c in n.children() {
                    self.node(c);
                }
            }
            _ => {}
        }
    }

    fn mcp(&mut self, n: &Node) {
        let Some(id) = attr(n, "id") else { return };
        let Some(src) = attr(n, "src") else {
            self.declare(n);
            return self.host_policy(
                n,
                "agentd dials an MCP server by its URL (`src`); a registry entry or server \
                 reference is not resolved"
                    .into(),
            );
        };
        let mut server = Map::new();
        server.insert("name".into(), id.into());
        server.insert("endpoint".into(), src.into());
        let mut allow = list(n, "tools");
        for a in list(n, "allow") {
            if !allow.contains(&a) {
                allow.push(a);
            }
        }
        if !allow.is_empty() {
            server.insert("allow".into(), Value::Array(allow));
        }
        let deny = list(n, "deny");
        if !deny.is_empty() {
            server.insert("exclude".into(), Value::Array(deny));
        }
        push(&mut self.out.config, "mcp", "servers").push(Value::Object(server));
    }

    fn agent(&mut self, n: &Node) {
        let Some(id) = attr(n, "id") else { return };
        if attr(n, "protocol") != Some("a2a") {
            self.declare(n);
            return self.host_policy(n, "agentd reaches agents over A2A (`protocol=a2a`)".into());
        }
        if let Err(why) = self.admission.admits(IDENTITY) {
            self.declare(n);
            return self.host_policy(n, why);
        }
        if let Some(b) = attr(n, "binding").filter(|b| *b != "JSONRPC") {
            self.declare(n);
            return self.host_policy(n, format!("agentd speaks A2A's JSONRPC binding, not {b}"));
        }
        if attr(n, "tenant").is_some() {
            self.declare(n);
            return self.host_policy(
                n,
                "agentd takes an agent's tenant from its Agent Card, not from the document".into(),
            );
        }
        // agentd dials the configured URL and reads the card on its origin
        // (`/.well-known/agent-card.json`), so an endpoint is used as it is,
        // and a card at the well-known path names its origin.
        let endpoint = match (attr(n, "endpoint"), attr(n, "src")) {
            (Some(e), _) => e.to_string(),
            (None, Some(card)) => match card.strip_suffix("/.well-known/agent-card.json") {
                Some(origin) if origin.contains("://") => origin.to_string(),
                _ => {
                    self.declare(n);
                    return self.host_policy(
                        n,
                        "agentd finds an agent by its endpoint, or by an Agent Card at \
                         /.well-known/agent-card.json on its origin; this `src` is neither"
                            .into(),
                    );
                }
            },
            // The reader refuses an A2A agent with neither.
            (None, None) => return,
        };
        let mut peer = Map::new();
        peer.insert("name".into(), id.into());
        peer.insert("endpoint".into(), endpoint.into());
        push(&mut self.out.config, "a2a", "peers").push(Value::Object(peer));
    }

    /// A structured body that parsed to a mapping, or a refusal.
    fn mapping(&mut self, n: &Node) -> Option<Map<String, Value>> {
        match n.value.as_ref().and_then(|v| v.data.as_ref()) {
            Some(Value::Object(m)) => Some(m.clone()),
            _ => {
                self.errs.push(Refusal::at(
                    line_of(n),
                    "invalid-structured-body",
                    format!("the {} body must be a mapping", n.kind),
                ));
                None
            }
        }
    }

    fn workflow(&mut self, n: &Node) {
        let dialect = attr(n, "dialect").unwrap_or_default();
        if !WORKFLOW_DIALECTS.contains(&dialect) {
            self.declare(n);
            return self.host_policy(
                n,
                format!(
                    "agentd runs the {} dialects, not {dialect:?}",
                    WORKFLOW_DIALECTS.join(" and ")
                ),
            );
        }
        let Some(mut m) = self.mapping(n) else { return };
        if !m.contains_key("name")
            && let Some(id) = attr(n, "id")
        {
            m.insert("name".into(), id.into());
        }
        self.out.workflows.push(Value::Object(m));
    }

    fn config(&mut self, n: &Node) {
        let dialect = attr(n, "dialect").unwrap_or_default();
        if dialect != CONFIG_DIALECT {
            self.declare(n);
            return self.host_policy(n, format!("agentd reads {CONFIG_DIALECT}, not {dialect:?}"));
        }
        let Some(m) = self.mapping(n) else { return };
        // The document cannot grant itself trust: its grants and its own
        // source are the operator's.
        for key in [
            "document_capabilities",
            "instruction_sources",
            "instruction",
        ] {
            if m.contains_key(key) {
                self.errs.push(Refusal::at(
                    line_of(n),
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

    fn skill(&mut self, n: &Node) {
        let Some(id) = attr(n, "id") else { return };
        if attr(n, "src").is_some() {
            self.declare(n);
            return self.host_policy(
                n,
                "agentd loads packaged skills from its configuration (`skills:`), not from a \
                 document's `src`"
                    .into(),
            );
        }
        let body = self.body(n);
        if body.is_empty() {
            return self.declare(n);
        }
        self.out.skills.push(InlineSkill {
            name: id.to_string(),
            description: attr(n, "description")
                .or_else(|| attr(n, "title"))
                .unwrap_or_default()
                .to_string(),
            when_to_use: attr(n, "trigger").map(str::to_string),
            body,
        });
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
            }]
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
}
