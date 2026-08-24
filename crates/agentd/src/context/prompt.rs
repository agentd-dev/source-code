// SPDX-License-Identifier: AGPL-3.0-only
//! The **system-prompt template** — the one place the agent's standing context
//! is assembled, and the surface an operator overrides.
//!
//! The grammar is deliberately tiny: interpolation plus two block forms.
//!
//! ```text
//! {{ expr }}                      interpolate
//! {{#if expr}} … {{else}} … {{/if}}
//! {{#each expr}} … {{/each}}      `this` is the element, `@index` its position
//! {{! comment }}
//! ```
//!
//! Everything hard about *expressions* — field access, comparisons, macros,
//! functions — is delegated to CEL, which agentd already ships. Resolution is
//! **path first, CEL second**: `{{instance}}` and `{{#each services}}` are bare
//! lookups that work in any build, and only a real expression
//! (`take(services, 16)`, `size(peers) > 0`) needs `--features cel`. That
//! keeps the default template — and most custom ones — working without the
//! feature, and a CEL-needing template is refused at CONFIG LOAD with the
//! feature message rather than mis-rendering at turn time.
//!
//! ## Why the default template is ordered the way it is
//!
//! Providers cache on the literal prefix of a request (Anthropic's prompt
//! caching, OpenAI's automatic prefix caching). A block that changes between
//! turns invalidates the cache for **everything after it**, so the default
//! template is ordered from most stable to most volatile: persona and
//! instruction (change only on reload), then configuration-derived sections
//! (workflows, services, streams, templates), then live state (peers with
//! their instance children, parked signals, memory keys). Put volatile
//! content last in a custom template for the same reason.

use crate::engine::template::{Data, lookup};
use serde_json::Value;

/// Nesting cap — a prompt template is a document, not a program.
const MAX_DEPTH: usize = 8;
/// Rendered-output cap: a runaway `{{#each}}` must not be able to blow the
/// context window (or the bill) on its own.
const MAX_OUTPUT: usize = 256 * 1024;

/// One parsed node.
#[derive(Debug, Clone, PartialEq)]
enum Node {
    Text(String),
    /// `{{ expr }}`
    Interp(String),
    /// `{{#if expr}} … {{else}} … {{/if}}`
    If {
        expr: String,
        then: Vec<Node>,
        otherwise: Vec<Node>,
    },
    /// `{{#each expr}} … {{/each}}`
    Each {
        expr: String,
        body: Vec<Node>,
    },
}

/// A compiled template. Parsing is done once (at config load); rendering is
/// per turn.
#[derive(Debug, Clone, PartialEq)]
pub struct Template {
    nodes: Vec<Node>,
    /// Every root identifier the template reads — used to fail closed on a
    /// typo'd reference at load instead of rendering a silent empty hole.
    pub roots: Vec<String>,
    /// Whether any expression needs CEL (i.e. is not a bare path).
    pub needs_cel: bool,
}

impl Template {
    /// Parse + validate. Errors name the problem, never a byte offset alone.
    pub fn parse(src: &str) -> Result<Template, String> {
        let mut p = Parser {
            s: src,
            i: 0,
            depth: 0,
        };
        let nodes = p.block(None)?;
        if p.i < src.len() {
            return Err(format!(
                "unexpected {:?} — a closing tag with no opening block",
                &src[p.i..(p.i + 20).min(src.len())]
            ));
        }
        let mut t = Template {
            nodes,
            roots: Vec::new(),
            needs_cel: false,
        };
        let mut roots = Vec::new();
        let mut needs_cel = false;
        collect(&t.nodes, &mut roots, &mut needs_cel);
        roots.sort();
        roots.dedup();
        t.roots = roots;
        t.needs_cel = needs_cel;
        Ok(t)
    }

    /// Does the template read `name` anywhere? (`{{instruction}}` guard.)
    pub fn reads(&self, name: &str) -> bool {
        self.roots.iter().any(|r| r == name)
    }

    /// Render against `data`. A missing path renders empty rather than
    /// failing the turn — the load-time root check is what catches typos, and
    /// an absent-at-runtime value (no peers yet) is normal, not an error.
    pub fn render(&self, data: &Data) -> Result<String, String> {
        let mut out = String::new();
        render_nodes(&self.nodes, data, &mut out, 0)?;
        Ok(out)
    }
}

struct Parser<'a> {
    s: &'a str,
    i: usize,
    depth: usize,
}

impl<'a> Parser<'a> {
    /// Parse until EOF or one of `stop` (`{{/if}}`, `{{/each}}`, `{{else}}`).
    fn block(&mut self, stop: Option<&[&str]>) -> Result<Vec<Node>, String> {
        if self.depth > MAX_DEPTH {
            return Err(format!("template nests deeper than {MAX_DEPTH} blocks"));
        }
        let mut out = Vec::new();
        loop {
            let Some(start) = self.s[self.i..].find("{{") else {
                if self.i < self.s.len() {
                    out.push(Node::Text(self.s[self.i..].to_string()));
                    self.i = self.s.len();
                }
                if stop.is_some() {
                    return Err("unclosed block: expected a closing tag".into());
                }
                return Ok(out);
            };
            let start = self.i + start;
            if start > self.i {
                out.push(Node::Text(self.s[self.i..start].to_string()));
            }
            let after = start + 2;
            let Some(rel) = self.s[after..].find("}}") else {
                return Err("unterminated `{{` — every tag needs a closing `}}`".into());
            };
            let raw = self.s[after..after + rel].trim().to_string();
            let next = after + rel + 2;
            // A closing/else tag ends this block — the caller consumes it.
            if let Some(stops) = stop
                && stops.iter().any(|s| *s == raw)
            {
                self.i = start;
                return Ok(out);
            }
            self.i = next;
            if raw.starts_with('!') {
                continue; // comment
            }
            if let Some(expr) = raw.strip_prefix("#if ") {
                let expr = expr.trim().to_string();
                self.depth += 1;
                let then = self.block(Some(&["else", "/if"]))?;
                let otherwise = if self.peek_tag() == Some("else".into()) {
                    self.consume_tag();
                    self.block(Some(&["/if"]))?
                } else {
                    Vec::new()
                };
                self.expect_tag("/if")?;
                self.depth -= 1;
                out.push(Node::If {
                    expr,
                    then,
                    otherwise,
                });
                continue;
            }
            if let Some(expr) = raw.strip_prefix("#each ") {
                let expr = expr.trim().to_string();
                self.depth += 1;
                let body = self.block(Some(&["/each"]))?;
                self.expect_tag("/each")?;
                self.depth -= 1;
                out.push(Node::Each { expr, body });
                continue;
            }
            if raw.starts_with('/') || raw == "else" {
                return Err(format!(
                    "{{{{{raw}}}}} is a closing tag with no opening block"
                ));
            }
            if raw.starts_with('#') {
                return Err(format!(
                    "unknown block tag {{{{{raw}}}}} — the template language has `#if` and `#each` only"
                ));
            }
            if raw.is_empty() {
                return Err("empty `{{}}` tag".into());
            }
            out.push(Node::Interp(raw));
        }
    }

    fn peek_tag(&self) -> Option<String> {
        let rest = &self.s[self.i..];
        let after = rest.strip_prefix("{{")?;
        let end = after.find("}}")?;
        Some(after[..end].trim().to_string())
    }

    fn consume_tag(&mut self) {
        if let Some(rest) = self.s[self.i..].strip_prefix("{{")
            && let Some(end) = rest.find("}}")
        {
            self.i += 2 + end + 2;
        }
    }

    fn expect_tag(&mut self, tag: &str) -> Result<(), String> {
        match self.peek_tag() {
            Some(t) if t == tag => {
                self.consume_tag();
                Ok(())
            }
            _ => Err(format!("expected {{{{{tag}}}}}")),
        }
    }
}

/// Root identifiers + whether any expression is more than a bare path.
fn collect(nodes: &[Node], roots: &mut Vec<String>, needs_cel: &mut bool) {
    for n in nodes {
        match n {
            Node::Text(_) => {}
            Node::Interp(e) => note(e, roots, needs_cel),
            Node::If {
                expr,
                then,
                otherwise,
            } => {
                note(expr, roots, needs_cel);
                collect(then, roots, needs_cel);
                collect(otherwise, roots, needs_cel);
            }
            Node::Each { expr, body } => {
                note(expr, roots, needs_cel);
                collect(body, roots, needs_cel);
            }
        }
    }
}

fn note(expr: &str, roots: &mut Vec<String>, needs_cel: &mut bool) {
    match bare_path(expr) {
        Some(p) => {
            let root = p.split(['.', '[']).next().unwrap_or(p).to_string();
            // `this` / `@index` are block-scoped, not template roots.
            if root != "this" && !root.starts_with('@') {
                roots.push(root);
            }
        }
        None => *needs_cel = true,
    }
}

/// A bare path (`a`, `a.b.c`, `this.name`) — no operators, calls or literals.
fn bare_path(expr: &str) -> Option<&str> {
    let e = expr.trim();
    if e.is_empty() {
        return None;
    }
    let ok = e
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.' || c == '@');
    if ok && !e.starts_with('.') && !e.ends_with('.') {
        Some(e)
    } else {
        None
    }
}

fn render_nodes(nodes: &[Node], data: &Data, out: &mut String, depth: usize) -> Result<(), String> {
    if depth > MAX_DEPTH {
        return Err("template recursion exceeded".into());
    }
    for n in nodes {
        if out.len() > MAX_OUTPUT {
            return Err(format!(
                "rendered prompt exceeds {MAX_OUTPUT} bytes — narrow an `{{{{#each}}}}`"
            ));
        }
        match n {
            Node::Text(t) => out.push_str(t),
            Node::Interp(e) => {
                let v = eval(e, data)?;
                out.push_str(&stringify(&v));
            }
            Node::If {
                expr,
                then,
                otherwise,
            } => {
                let v = eval(expr, data)?;
                let branch = if truthy(&v) { then } else { otherwise };
                render_nodes(branch, data, out, depth + 1)?;
            }
            Node::Each { expr, body } => {
                let v = eval(expr, data)?;
                let items: Vec<Value> = match v {
                    Value::Array(a) => a,
                    Value::Null => Vec::new(),
                    // Iterating an object yields its values, which is what an
                    // author means by "each server in mcp.servers".
                    Value::Object(o) => o.into_values().collect(),
                    other => vec![other],
                };
                for (i, item) in items.iter().enumerate() {
                    let mut scoped = data.clone();
                    scoped.insert("this".into(), item.clone());
                    scoped.insert("@index".into(), Value::from(i as u64));
                    render_nodes(body, &scoped, out, depth + 1)?;
                }
            }
        }
    }
    Ok(())
}

/// Path first, CEL second. A missing path is empty (see `Template::render`).
fn eval(expr: &str, data: &Data) -> Result<Value, String> {
    if let Some(p) = bare_path(expr) {
        return Ok(lookup(p, data).unwrap_or(Value::Null));
    }
    let vars: Vec<(&str, &Value)> = data.iter().map(|(k, v)| (k.as_str(), v)).collect();
    crate::cel::eval_value(expr, &vars).map_err(|e| format!("{expr:?}: {e}"))
}

fn stringify(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

/// CEL-ish truthiness, extended so `{{#if services}}` means "non-empty".
fn truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().is_some_and(|f| f != 0.0),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn data(pairs: &[(&str, Value)]) -> Data {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), v.clone()))
            .collect()
    }

    #[test]
    fn interpolation_and_paths() {
        let t = Template::parse("You are {{instance}} ({{agent.mode}}).").unwrap();
        let d = data(&[
            ("instance", json!("beacon")),
            ("agent", json!({"mode": "daemon"})),
        ]);
        assert_eq!(t.render(&d).unwrap(), "You are beacon (daemon).");
        assert!(!t.needs_cel, "bare paths need no CEL");
        assert_eq!(t.roots, vec!["agent".to_string(), "instance".to_string()]);
    }

    #[test]
    fn each_iterates_with_this_and_index() {
        let t = Template::parse("{{#each xs}}{{@index}}:{{this.n}} {{/each}}").unwrap();
        let d = data(&[("xs", json!([{"n": "a"}, {"n": "b"}]))]);
        assert_eq!(t.render(&d).unwrap(), "0:a 1:b ");
    }

    #[test]
    fn each_over_empty_and_absent_renders_nothing() {
        let t = Template::parse("[{{#each xs}}x{{/each}}]").unwrap();
        assert_eq!(t.render(&data(&[("xs", json!([]))])).unwrap(), "[]");
        assert_eq!(t.render(&data(&[])).unwrap(), "[]");
    }

    #[test]
    fn if_else_uses_emptiness_as_falsy() {
        let t = Template::parse("{{#if xs}}some{{else}}none{{/if}}").unwrap();
        assert_eq!(t.render(&data(&[("xs", json!([1]))])).unwrap(), "some");
        assert_eq!(t.render(&data(&[("xs", json!([]))])).unwrap(), "none");
        assert_eq!(t.render(&data(&[("xs", json!(""))])).unwrap(), "none");
        assert_eq!(t.render(&data(&[])).unwrap(), "none");
    }

    #[test]
    fn nested_blocks_compose() {
        let t = Template::parse(
            "{{#each svc}}- {{this.name}}{{#if this.tags}} [{{this.tags}}]{{/if}}\n{{/each}}",
        )
        .unwrap();
        let d = data(&[(
            "svc",
            json!([{"name":"billing","tags":["sensitive"]},{"name":"docs"}]),
        )]);
        assert_eq!(
            t.render(&d).unwrap(),
            "- billing [[\"sensitive\"]]\n- docs\n"
        );
    }

    #[test]
    fn comments_are_dropped() {
        let t = Template::parse("a{{! not rendered }}b").unwrap();
        assert_eq!(t.render(&data(&[])).unwrap(), "ab");
    }

    #[test]
    fn malformed_templates_are_refused_at_parse() {
        for (src, want) in [
            ("{{#each xs}}oops", "unclosed block"),
            ("{{ unterminated", "unterminated"),
            ("{{/each}}", "closing tag with no opening block"),
            ("{{#while x}}{{/while}}", "unknown block tag"),
            ("{{}}", "empty"),
        ] {
            let e = Template::parse(src).unwrap_err();
            assert!(e.contains(want), "{src:?} → {e:?} (wanted {want:?})");
        }
    }

    #[test]
    fn expressions_are_flagged_as_needing_cel() {
        let bare = Template::parse("{{#each services}}{{this.name}}{{/each}}").unwrap();
        assert!(!bare.needs_cel);
        let expr = Template::parse("{{#each take(services, 3)}}x{{/each}}").unwrap();
        assert!(expr.needs_cel, "a call is not a bare path");
        assert!(
            Template::parse("{{#if size(peers) > 0}}y{{/if}}")
                .unwrap()
                .needs_cel
        );
    }

    #[test]
    fn the_instruction_guard_can_see_the_reference() {
        assert!(
            Template::parse("## I\n{{instruction}}")
                .unwrap()
                .reads("instruction")
        );
        assert!(
            !Template::parse("nothing here")
                .unwrap()
                .reads("instruction")
        );
    }

    #[test]
    fn a_runaway_each_is_capped_not_unbounded() {
        let t = Template::parse("{{#each xs}}{{this}}{{/each}}").unwrap();
        let big: Vec<Value> = (0..60_000).map(|_| json!("0123456789")).collect();
        let e = t.render(&data(&[("xs", Value::Array(big))])).unwrap_err();
        assert!(e.contains("exceeds"), "{e}");
    }

    #[test]
    fn nesting_beyond_the_cap_is_refused() {
        let src = "{{#if a}}".repeat(MAX_DEPTH + 2) + &"{{/if}}".repeat(MAX_DEPTH + 2);
        assert!(Template::parse(&src).unwrap_err().contains("nests deeper"));
    }
}
