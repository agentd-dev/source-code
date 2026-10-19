// SPDX-License-Identifier: AGPL-3.0-only
//! `agentd --delegation-lint` (RFC 0044 §5.5): dials nothing. It reads the
//! configuration and prints the Resource connections each IdP must have for
//! the templates to work — exactly those, since the IdP's list, not the
//! template, is the real ceiling — and what in the configuration would
//! refuse or narrow a delegated call.

use crate::config::settings::Settings;
use crate::config::settings::delegation::{self as cfg, Profile, ScopeMode};

/// One lint finding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    /// `error` refuses a delegated call; `warn` narrows or costs.
    pub level: &'static str,
    pub text: String,
}

/// `--delegation-lint`: what each IdP's Resource connections must be, and
/// what would refuse or narrow a delegated call. No network.
pub fn lint(settings: &Settings) -> (String, Vec<Finding>) {
    let mut report = String::new();
    let mut findings = Vec::new();
    let mut find = |level, text: String| findings.push(Finding { level, text });
    for (name, p) in &settings.identity.providers {
        report.push_str(&format!(
            "identity provider {name} ({} profile, issuer {})\n",
            match p.profile {
                Profile::Okta => "okta",
                Profile::Generic => "generic",
            },
            p.issuer
        ));
        let mut any = false;
        for t in settings
            .identity
            .delegation
            .templates
            .iter()
            .filter(|t| &t.idp == name)
        {
            for (srv, ceiling) in &t.servers {
                let Some(m) = settings.mcp.servers.iter().find(|m| &m.name == srv) else {
                    continue;
                };
                let Some(a) = m.auth.as_ref() else { continue };
                any = true;
                report.push_str(&format!(
                    "  Resource connection for {srv}: authorization server {} — resource {} — client {} — scopes {} (template {})\n",
                    a.audience.as_deref().unwrap_or("?"),
                    a.resource_for(&m.endpoint, true).unwrap_or_default(),
                    a.client_id.as_deref().unwrap_or("?"),
                    ceiling.scopes.join(" "),
                    t.name
                ));
                let mut wanted: Vec<&String> = a.scopes.iter().collect();
                if a.scope_mode.unwrap_or_default() == ScopeMode::Server {
                    wanted.extend(a.tool_scopes.values().flatten());
                }
                for w in wanted {
                    if !ceiling.scopes.contains(w) {
                        find(
                            "warn",
                            format!(
                                "{srv}: the server asks for {w:?}, which template {} does not allow — every token for it is minted without it",
                                t.name
                            ),
                        );
                    }
                }
                if a.scope_mode == Some(ScopeMode::PerTool) {
                    find(
                        "warn",
                        format!(
                            "{srv}: scope_mode per_tool mints once per scope set; under plain Okta SSO two sets already exceed the monthly quota for an all-day user"
                        ),
                    );
                }
                if p.profile == Profile::Okta && t.budget.per_month() > 250 {
                    find(
                        "warn",
                        format!(
                            "template {}: budget.per_month {} is over Okta's 250 ID-JAGs per user, resource app and month",
                            t.name,
                            t.budget.per_month()
                        ),
                    );
                }
            }
        }
        if !any {
            find(
                "error",
                format!(
                    "identity provider {name}: no template allows a server there — nobody can enrol, and every delegated call fails"
                ),
            );
        }
        report.push_str(
            "  The IdP's Resource connections for agentd must be exactly these — the IdP, not the template, is the real ceiling — and the agent's refresh tokens should live days with an idle window, not Okta's standard 90 days.\n",
        );
    }
    for m in cfg::delegated_servers(settings) {
        let named = settings
            .identity
            .delegation
            .templates
            .iter()
            .any(|t| t.servers.contains_key(&m.name));
        if !named {
            find(
                "error",
                format!(
                    "{}: no template allows it — its tools are never offered",
                    m.name
                ),
            );
        }
    }
    if cfg::person(settings).is_none() && cfg::delegated_servers(settings).next().is_some() {
        find(
            "error",
            "no a2a.principals rule with match.uid and idp_subject: nobody can enrol".into(),
        );
    }
    for f in &findings {
        report.push_str(&format!("{}: {}\n", f.level, f.text));
    }
    (report, findings)
}
