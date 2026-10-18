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

use super::reactor::Runtime;

impl Runtime {
    /// Whether `server` is one of a Company's.
    pub(crate) fn is_company_server(&self, server: &str) -> bool {
        self.mcp_specs
            .get(server)
            .is_some_and(|s| s.company.is_some())
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
    /// is down does.
    pub(crate) fn company_tool_to_come(
        &self,
        tool: &str,
        registry: &crate::registry::Registry,
    ) -> bool {
        registry.get(tool).is_none()
            && tool.split_once('.').is_some_and(|(server, _)| {
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
