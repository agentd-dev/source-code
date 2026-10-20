// SPDX-License-Identifier: AGPL-3.0-only
//! Who may reach a Company, and how (RFC 0047 §5.6).
//!
//! A Company's MCP servers are the supervisor's alone: no spawn payload
//! carries one ([`Runtime::spawn_spec`]), a turn plan routes every Company
//! tool back to the supervisor (`tool_plan`), and a subagent granted a
//! Company server is handed its tools' definitions and calls them through
//! the supervisor too (`subagent.run`), whether or not `security.policies`
//! has a rule for them.
//!
//! And they act for one User. Company tools are offered only in work the
//! configured User (`security.companies.user`) owns — or, when the User is
//! `operator` or an operator principal, in work nobody owns: a schedule, a
//! webhook, a stream. Anywhere else they are not in the plan, and a call by
//! name is refused, naming both.
//!
//! Those tool calls are the only way to a Company's server. A workflow
//! reaches one through `tool` steps naming its tools; an `mcp.tool` or
//! `mcp.resource` step, a resource wait or a `subscribe` start that names
//! one is refused with the definition, or when the step runs for a templated
//! name; and no instruction or workflow definition is read from one.
//!
//! What a call's result reaches is recorded (§6.2): the conversation, run or
//! subagent it is read into carries the Company's origin from then on, and a
//! call to another Company from it opens the User's gate first
//! (`company_cross_check`), whatever the policies say. A call that may have
//! been performed and got no answer is `outcome unknown`, and is not sent
//! again, by a step's `retry:` or by a restart that finds its step running
//! (§5.9).
//!
//! And the browsers `security.companies.browsers` declares are kept off every
//! Company's hosts (§5.5.9): their tools are served by the supervisor, and a
//! call with any string argument naming a Company's host — read as a browser
//! reads a URL (`browsed`) — or a host that is not ASCII is refused unsent.
//! *Limit:* a browser that follows a link onto a Company's site, or a browser
//! server not declared, is not caught.

use super::reactor::Runtime;
use serde_json::Value;

/// How an `outcome unknown` error begins: a Company call that failed after
/// it was sent, and may have been performed.
pub(crate) const OUTCOME_UNKNOWN: &str = "outcome unknown: ";

/// Whether a step failed with an `outcome unknown` — which its `retry:` does
/// not repeat. A tool step's error is its result, a JSON string, so the text
/// may arrive quoted.
pub(crate) fn is_outcome_unknown(error: &str) -> bool {
    error.trim_start_matches('"').starts_with(OUTCOME_UNKNOWN)
}

/// The context a Company call's result is read into: what gains the
/// Company's origin when it arrives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reads {
    Conversation(String),
    Run(String),
    Child(String),
}

/// What a browser would open for one string argument.
#[cfg(feature = "companies")]
#[derive(Debug, PartialEq, Eq)]
enum Browsed {
    /// This host, as a browser compares it: lowercase, its trailing dots
    /// and port gone, an IPv4 address in dotted decimal.
    Host(String),
    /// A host agentd cannot compare as a browser would — not ASCII once
    /// percent-decoded (a browser maps such names through IDNA) — refused
    /// whatever it names.
    Opaque(String),
}

/// Every host among the string values of `v`, at any depth, as a browser
/// would read each string: a browser server names its URL argument as it
/// likes, so none is singled out.
#[cfg(feature = "companies")]
fn url_hosts(v: &Value, out: &mut Vec<Browsed>) {
    match v {
        Value::String(s) => out.extend(browsed(s)),
        Value::Array(a) => a.iter().for_each(|x| url_hosts(x, out)),
        Value::Object(o) => o.values().for_each(|x| url_hosts(x, out)),
        _ => {}
    }
}

/// The host a browser would open for `s`, read as the WHATWG URL parser
/// reads it: tabs and newlines dropped, a special scheme (`http`, `https`,
/// `ws`, `wss`, `ftp`) followed by any number of `/` or `\`, the authority
/// ending at the first `/`, `\`, `?` or `#`, the user part before its last
/// `@`, the host percent-decoded. A string with no scheme — `//host`, or a
/// bare `host/path` a browser tool may take for one — is read as an
/// `https` URL. `None` when `s` names no host.
#[cfg(feature = "companies")]
fn browsed(s: &str) -> Option<Browsed> {
    let s: String = s
        .trim_matches(|c: char| c <= ' ')
        .chars()
        .filter(|c| !matches!(c, '\t' | '\n' | '\r'))
        .collect();
    let scheme_end = s.find(':').filter(|&i| {
        i > 0
            && s[..i].starts_with(|c: char| c.is_ascii_alphabetic())
            && s[..i]
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
    });
    let rest = match scheme_end {
        Some(i) => {
            let scheme = s[..i].to_ascii_lowercase();
            let rest = &s[i + 1..];
            if matches!(scheme.as_str(), "http" | "https" | "ws" | "wss" | "ftp") {
                rest.trim_start_matches(['/', '\\'])
            } else {
                // Another scheme names a host only after `//`; `host:port`
                // with no slashes is a bare host.
                match rest.strip_prefix("//") {
                    Some(r) => r,
                    None if rest.starts_with(|c: char| c.is_ascii_digit()) => s.as_str(),
                    None => return None,
                }
            }
        }
        None if s.starts_with("//") || s.starts_with("\\\\") => s.trim_start_matches(['/', '\\']),
        None => {
            // A bare `host/path`: one word with a dot in its host.
            let host = s.split(['/', '\\', '?', '#']).next().unwrap_or_default();
            if host.is_empty() || !host.contains('.') || host.chars().any(char::is_whitespace) {
                return None;
            }
            s.as_str()
        }
    };
    let authority = rest.split(['/', '\\', '?', '#']).next().unwrap_or_default();
    let host = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
    let host = if host.starts_with('[') {
        host.split_inclusive(']').next().unwrap_or(host)
    } else {
        host.rsplit_once(':')
            .filter(|(_, p)| p.bytes().all(|b| b.is_ascii_digit()))
            .map_or(host, |(h, _)| h)
    };
    if host.is_empty() {
        return None;
    }
    let Some(decoded) = percent_decoded(host).filter(|h| h.is_ascii()) else {
        return Some(Browsed::Opaque(host.to_string()));
    };
    let host = decoded.trim_end_matches('.').to_ascii_lowercase();
    if host.is_empty() {
        return None;
    }
    Some(Browsed::Host(ipv4(&host).unwrap_or(host)))
}

/// `s` with its `%XX` escapes decoded; `None` when the bytes are not UTF-8.
#[cfg(feature = "companies")]
pub(crate) fn percent_decoded(s: &str) -> Option<String> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        let hex = |c: u8| (c as char).to_digit(16);
        match (
            b[i],
            b.get(i + 1).copied().and_then(hex),
            b.get(i + 2).copied().and_then(hex),
        ) {
            (b'%', Some(h), Some(l)) => {
                out.push((h * 16 + l) as u8);
                i += 3;
            }
            (c, _, _) => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8(out).ok()
}

/// `host` as the IPv4 address a browser reads it as, in dotted decimal,
/// when its last label is a number (WHATWG's IPv4 parser: one to four
/// parts, each decimal, `0x` hex or `0` octal, the last filling what is
/// left).
#[cfg(feature = "companies")]
fn ipv4(host: &str) -> Option<String> {
    let mut parts: Vec<&str> = host.split('.').collect();
    if parts.last() == Some(&"") {
        parts.pop();
    }
    let number = |p: &str| -> Option<u64> {
        let (digits, radix) = match p {
            _ if p.starts_with("0x") || p.starts_with("0X") => (&p[2..], 16),
            _ if p.len() > 1 && p.starts_with('0') => (&p[1..], 8),
            _ => (p, 10),
        };
        if digits.is_empty() {
            return (radix == 16).then_some(0);
        }
        u64::from_str_radix(digits, radix).ok()
    };
    let last = number(parts.last()?)?;
    if parts.len() > 4 {
        return None;
    }
    let mut n: u64 = 0;
    for p in &parts[..parts.len() - 1] {
        let v = number(p).filter(|v| *v <= 255)?;
        n = n * 256 + v;
    }
    let left = 4 - (parts.len() - 1) as u32;
    if last >= 256u64.pow(left) {
        return None;
    }
    let ip = n * 256u64.pow(left) + last;
    Some(std::net::Ipv4Addr::from(u32::try_from(ip).ok()?).to_string())
}

impl Runtime {
    /// Whether `server` is a browser `security.companies.browsers` declares.
    pub(crate) fn is_browser_server(&self, server: &str) -> bool {
        self.settings
            .security
            .companies
            .browsers
            .iter()
            .any(|b| b == server)
    }

    /// Why a call on the browser `server` with `args` is refused, when one of
    /// its string arguments is a URL whose host is one of a Company's: agentd
    /// cannot join a browser to a Session (draft §5), so it keeps declared
    /// browsers off Companies' sites, sign-in pages included. Logged
    /// `company.browser.refused`.
    pub(crate) fn browser_refused(&self, server: &str, args: &Value) -> Option<String> {
        if !self.is_browser_server(server) {
            return None;
        }
        #[cfg(feature = "companies")]
        {
            let sup = self.companies.as_ref()?;
            let mut hosts = Vec::new();
            url_hosts(args, &mut hosts);
            for host in hosts {
                let host = match host {
                    Browsed::Host(h) => h,
                    Browsed::Opaque(h) => {
                        self.log.warn(
                            "company.browser.refused",
                            serde_json::json!({"server": server, "host": h, "company": null}),
                        );
                        return Some(format!(
                            "{server} is a browser (security.companies.browsers), and the host {h:?} is not one agentd can compare with the Companies' hosts: give it in ASCII (an IDNA name as its xn-- form)"
                        ));
                    }
                };
                let Some(c) = sup.companies().find(|c| {
                    c.hosts()
                        .iter()
                        .any(|p| crate::config::settings::companies::host_matches(p, &host))
                }) else {
                    continue;
                };
                self.log.warn(
                    "company.browser.refused",
                    serde_json::json!({"server": server, "host": host, "company": c.name}),
                );
                return Some(format!(
                    "{server} is a browser (security.companies.browsers), and {host} is a host of company {}: agentd keeps its browsers off every Company's site — use the Company's own tools",
                    c.name
                ));
            }
            None
        }
        #[cfg(not(feature = "companies"))]
        {
            let _ = args;
            None
        }
    }

    /// The context `caller`'s result is read into, when a model reads it.
    #[cfg(feature = "companies")]
    pub(crate) fn reads_of(&self, caller: &super::tools::ToolCaller) -> Option<Reads> {
        if let Some(h) = &caller.subagent {
            return Some(Reads::Child(h.clone()));
        }
        if let Some(r) = &caller.run {
            return Some(Reads::Run(r.clone()));
        }
        (caller.node.is_some() || caller.ctx.is_some())
            .then(|| Reads::Conversation(caller.context_id()))
    }

    /// A result reached `into`: its record gains what the result `carries`.
    pub(crate) fn read_into(&mut self, into: Reads, carries: &crate::engine::run::Carries) {
        match into {
            Reads::Run(id) => self.hand_run(&id, carries),
            Reads::Child(h) => self.steer_carries(&h, carries),
            Reads::Conversation(ctx) => self.hand_conversation(&ctx, carries),
        }
    }

    /// Whether `server` is one of a Company's.
    pub(crate) fn is_company_server(&self, server: &str) -> bool {
        self.mcp_specs
            .get(server)
            .is_some_and(|s| s.company.is_some())
    }

    /// Whether `tool` acts at a Company: one of a Company's MCP tools, or a
    /// `company.*` tool.
    pub(crate) fn is_company_tool(&self, tool: &str) -> bool {
        tool.starts_with("company.") || self.company_server_of(tool).is_some()
    }

    /// The Company server a registered tool is served by, when it is one.
    pub(crate) fn company_server_of(&self, tool: &str) -> Option<String> {
        match self.registry.get(tool).map(|t| &t.imp) {
            Some(crate::registry::Impl::Mcp { server, .. }) if self.is_company_server(server) => {
                Some(server.clone())
            }
            _ => None,
        }
    }

    /// Whether `tool` is named for a Company server that has not joined: its
    /// tools are listed only once it does (`runtime::join`, every boot
    /// included), so a `tool` step naming one is not refused for it — the
    /// step fails if it runs before the join, as a call to any server that
    /// is down does. An operation tool's name is known before then, and only
    /// one the configuration makes is to come.
    pub(crate) fn company_tool_to_come(
        &self,
        tool: &str,
        registry: &crate::registry::Registry,
    ) -> bool {
        registry.get(tool).is_none()
            && tool.split_once('.').is_some_and(|(server, _)| {
                use crate::config::settings::companies as co;
                if co::is_api_server(&self.settings, server) {
                    return co::api_tool_names(&self.settings, server)
                        .iter()
                        .any(|t| t == tool);
                }
                self.is_company_server(server) && !self.mcp.contains_key(server)
            })
    }

    /// Whether work owned by `owner` (`None`: nobody's) may use a Company's
    /// tools: the configured User's work, or — when the User is `operator`
    /// or an operator principal — work nobody owns.
    pub(crate) fn company_work_admitted(&self, owner: Option<&str>) -> Result<(), String> {
        let user = self.settings.security.companies.user();
        let acting = self.acting_principal(owner);
        let admitted = match (&acting, user) {
            (None, crate::config::settings::companies::OPERATOR) => true,
            (None, u) => self
                .acting_principal(Some(u))
                .is_some_and(|p| p.is_operator()),
            (Some(p), crate::config::settings::companies::OPERATOR) => p.is_operator(),
            (Some(p), u) => p.id == u,
        };
        if admitted {
            return Ok(());
        }
        Err(format!(
            "company tools act for {user}; this work is owned by {}",
            acting.map_or_else(|| "nobody".to_string(), |p| p.id)
        ))
    }
}

#[cfg(feature = "companies")]
impl Runtime {
    /// The cross-Company rule (RFC 0047 §6.2): a call to a Company's tool
    /// `name` — one of its MCP servers', or a `company.*` tool reaching its
    /// agent or its APIs — from work that holds another Company's content
    /// opens the User's gate, holding the call, unless the User already
    /// approved it. `None` when the work holds no other Company's content.
    pub(crate) fn company_cross_check(
        &mut self,
        caller: &super::tools::ToolCaller,
        name: &str,
        args: &Value,
        passed: super::reactor::Passed,
    ) -> Option<super::tools::ToolOutcome> {
        if passed.company {
            return None;
        }
        let sup = self.companies.clone()?;
        // A Company's MCP tool, or a tool that reaches its agent or its
        // APIs, which names the Company it acts at.
        let company = match self.company_server_of(name) {
            Some(server) => sup.company_of_server(&server)?,
            None if crate::config::settings::companies::is_interface_tool(name) => {
                sup.company(args["company"].as_str()?)?
            }
            None => return None,
        };
        // A Company's MCP tools are listed only once it is verified; its
        // `company.*` tools exist before, and a Company not yet verified
        // could be any of those the work holds content from.
        let carries = self.caller_carries(caller);
        let from = match company.issuer() {
            Some(issuer) => crate::companies::carry::foreign(&carries, &issuer),
            None => crate::companies::carry::issuers(&carries)
                .map(str::to_string)
                .collect(),
        };
        if from.is_empty() {
            return None;
        }
        let named: Vec<String> = from.iter().map(|i| sup.named_by_issuer(i)).collect();
        let question = crate::companies::carry::question(&named, &company.named(), name, args);
        let ask = super::reactor::CompanyAsk {
            company: company.name.clone(),
            kind: "cross_company".into(),
            from,
        };
        Some(self.company_gate(caller, name, args, ask, question, passed))
    }

    /// The User at `company` was reset: its MCP connections — the old User's
    /// sessions, which the Company tied to the old User's tokens — are
    /// dropped, and each joins again on a fresh connection
    /// (`runtime::join`); a call meanwhile is refused unsent.
    #[cfg(feature = "a2a")]
    pub(crate) fn company_redial(&mut self, company: &str) {
        let Some(sup) = self.companies.clone() else {
            return;
        };
        let servers: Vec<String> = self
            .mcp
            .keys()
            .filter(|s| sup.company_of_server(s).is_some_and(|c| c.name == company))
            .cloned()
            .collect();
        for server in servers {
            self.mcp.remove(&server);
            self.log.info(
                "mcp.disconnect",
                serde_json::json!({"server": server, "reason": "user_id_reset"}),
            );
        }
    }

    /// `company.status`: each declared Company as the supervisor knows it
    /// now, nothing fetched — the model's view without the operator's
    /// detail.
    pub(crate) fn company_status(&self, detail: bool) -> Value {
        let Some(sup) = &self.companies else {
            return serde_json::json!({"user": self.settings.security.companies.user(), "companies": []});
        };
        // Its MCP servers connected, and its OpenAPI APIs whose operation
        // tools are offered.
        let joined: std::collections::BTreeSet<String> = self
            .mcp
            .keys()
            .filter(|s| self.is_company_server(s))
            .chain(self.joining.offered().map(|(s, _)| s))
            .cloned()
            .collect();
        let companies: Vec<Value> = sup
            .companies()
            .map(|c| sup.status_of(c, &joined, detail))
            .collect();
        serde_json::json!({"user": sup.principal(), "companies": companies})
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(feature = "companies")]
    use serde_json::json;

    #[cfg(feature = "companies")]
    #[test]
    fn every_url_among_the_arguments_is_found() {
        let mut hosts = Vec::new();
        url_hosts(
            &json!({"url": "https://Shop.Example/x", "steps": [{"go": " http://a.b:8080/ "}],
                    "text": "see https://c.d", "n": 3, "path": "/local", "bad": "://x"}),
            &mut hosts,
        );
        let mut hosts: Vec<String> = hosts
            .into_iter()
            .map(|h| match h {
                Browsed::Host(h) => h,
                Browsed::Opaque(h) => panic!("{h}"),
            })
            .collect();
        hosts.sort();
        assert_eq!(
            hosts,
            ["a.b", "shop.example"],
            "a string that IS a URL, at any depth"
        );
    }

    /// Each string is read as a browser reads it, so no spelling of a
    /// Company's host gets past the guard.
    #[cfg(feature = "companies")]
    #[test]
    fn a_url_is_read_as_a_browser_reads_it() {
        let host = |s: &str| Browsed::Host(s.to_string());
        for (s, want) in [
            ("https://shop.example./account", host("shop.example")),
            ("https://shop.example\\@evil.test/", host("shop.example")),
            ("https://user:pw@shop.example:8443/", host("shop.example")),
            ("https://shop%2Eexample/", host("shop.example")),
            ("https:shop.example/", host("shop.example")),
            ("https:\\\\shop.example", host("shop.example")),
            ("HTTPS://SHOP.EXAMPLE", host("shop.example")),
            ("ht\ttps://shop.exa\nmple/", host("shop.example")),
            ("//shop.example/x", host("shop.example")),
            ("shop.example/products", host("shop.example")),
            ("shop.example", host("shop.example")),
            ("https://127.1/", host("127.0.0.1")),
            ("https://0x7f.0.0.1/", host("127.0.0.1")),
            ("https://2130706433/", host("127.0.0.1")),
            ("https://0177.0.0.1/", host("127.0.0.1")),
            ("https://[::1]:8080/", host("[::1]")),
            ("https://xn--caf-dma.example/", host("xn--caf-dma.example")),
            (
                "https://café.example/",
                Browsed::Opaque("café.example".into()),
            ),
            (
                "https://%C3%A9.example/",
                Browsed::Opaque("%C3%A9.example".into()),
            ),
            (
                "https://ｓｈｏｐ.example/",
                Browsed::Opaque("ｓｈｏｐ.example".into()),
            ),
        ] {
            assert_eq!(browsed(s), Some(want), "{s:?}");
        }
        for s in [
            "see the shop",
            "/local/path",
            "://x",
            "mailto:dana@shop.example",
            "javascript:alert(1)",
            "README",
            "",
        ] {
            assert_eq!(browsed(s), None, "{s:?}");
        }
    }

    #[test]
    fn an_outcome_unknown_is_recognised_quoted_or_not() {
        assert!(is_outcome_unknown(
            "outcome unknown: company shop may have performed shop.pay"
        ));
        assert!(is_outcome_unknown(
            "\"outcome unknown: company shop may have performed shop.pay\""
        ));
        assert!(!is_outcome_unknown("company shop refused the call"));
    }
}
