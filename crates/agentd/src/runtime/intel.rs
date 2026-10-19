// SPDX-License-Identifier: AGPL-3.0-only
//! The intelligence connections the runtime dials, and the one place a model
//! reference becomes a connection, a wire model and a fallback chain
//! (RFC 0046 §5.2–5.4).
//!
//! Every model call the runtime makes or hands a worker — a turn, a node, a
//! subagent, compaction, preflight, the goal check, the ask judge — is built
//! by [`Runtime::intel_config`]. Nothing about a model is copied into a field
//! at startup: the reference is resolved against the live settings at the
//! moment of the call, so a reload that moves a role, a model or its
//! provider is what the next call dials.

use std::collections::BTreeMap;
use std::sync::Arc;

use super::Runtime;
use crate::config::settings::{
    AuthKind, Connection, Intelligence, ModelRole, Settings, StructuredOutput,
};
use crate::subagent::protocol::IntelConfig;

/// A refreshing credential: the current bearer, from the login cache, or why
/// it cannot be had right now.
type Bearer = Arc<dyn Fn() -> Result<String, String> + Send + Sync>;

/// One connection, resolved: what its settings say at load, ready to dial.
pub(crate) struct Conn {
    /// The comma-joined endpoint list.
    uri: String,
    /// The resolved static `token`.
    token: Option<String>,
    /// `token_file`, read at each dial (a rotated file needs no reload). Only
    /// when no `token` is set: the inline token wins, as validation says.
    token_file: Option<String>,
    /// The resolved `headers`.
    headers: Vec<(String, String)>,
    /// A refreshing credential (`auth:` oauth2, static, spiffe), overriding
    /// `token`.
    bearer: Option<Bearer>,
    /// `auth: {kind: aws}`: the dial signs itself.
    aws: Option<crate::config::AuthSpec>,
    dialect: Option<String>,
    timeout_ms: Option<u64>,
    /// `structured_output: prompt`: a schema-bearing call is sent no format.
    structured_prompt: bool,
    /// The login target its credential is reported under (`intelligence`,
    /// `intelligence:<name>`).
    target: String,
}

/// Every connection: the top-level one and each provider by name.
pub(crate) struct Conns {
    top: Conn,
    providers: BTreeMap<String, Conn>,
}

impl Conns {
    /// Resolve every connection's credential. A token or a header whose
    /// secret does not resolve, a token file that cannot be read, or an
    /// `auth:` block that cannot make a signer refuses — the start, or the
    /// reload that would have gone live with it.
    pub(crate) fn build(
        settings: &Settings,
        env: &dyn Fn(&str) -> Option<String>,
    ) -> Result<Conns, String> {
        let intel = &settings.intelligence;
        let top = Conn::build(settings, &intel.top_connection(), env)?;
        let mut providers = BTreeMap::new();
        for c in intel.connections() {
            if let Some(name) = c.name {
                providers.insert(name.to_string(), Conn::build(settings, &c, env)?);
            }
        }
        Ok(Conns { top, providers })
    }

    /// A connection by provider name; `None`, or a name validation would
    /// have refused, is the top-level one.
    fn get(&self, provider: Option<&str>) -> &Conn {
        provider
            .and_then(|p| self.providers.get(p))
            .unwrap_or(&self.top)
    }
}

impl Conn {
    fn build(
        settings: &Settings,
        c: &Connection<'_>,
        env: &dyn Fn(&str) -> Option<String>,
    ) -> Result<Conn, String> {
        let at = c.at();
        let token = match c.token {
            Some(t) => Some(
                crate::sec::secret::resolve(&t.0, env).map_err(|e| format!("{at}.token: {e}"))?,
            ),
            None => None,
        };
        let token_file = match (&token, c.token_file) {
            (None, Some(p)) => {
                // Read to PROVE it is readable — a mistyped path refuses, as
                // it always has — then drop the value: it is read again at
                // every dial, so a rotated token needs no reload.
                crate::sec::secret::read_token_file(p)
                    .map_err(|e| format!("{at}.token_file: {e}"))?;
                Some(p.to_string())
            }
            _ => None,
        };
        let headers = c
            .headers
            .iter()
            .map(|(k, v)| {
                crate::sec::secret::resolve(v, env)
                    .map(|r| (k.clone(), r))
                    .map_err(|e| format!("{at}.headers.{k}: {e}"))
            })
            .collect::<Result<_, _>>()?;
        Ok(Conn {
            uri: c.endpoint_list().unwrap_or_default(),
            token,
            token_file,
            headers,
            bearer: bearer_provider(settings, c).map_err(|e| format!("{at}.auth: {e}"))?,
            aws: c
                .auth
                .filter(|a| a.kind == AuthKind::Aws)
                .map(|a| a.to_spec()),
            dialect: c.dialect.map(str::to_string),
            timeout_ms: c.timeout.map(|t| t.as_millis() as u64),
            structured_prompt: c.structured_output == StructuredOutput::Prompt,
            target: c.login_target(),
        })
    }
}

/// The refreshing credential for a connection's `auth:` block, from the
/// cache its `agentd --login` filled (`intelligence`, `intelligence:<name>`).
///
/// `None` when no bearer-style `auth:` is configured (and always without
/// `--features oauth`), which leaves the static `token` path untouched. An
/// `Err` is a block no signer can be made from; building one dials nothing,
/// so an issuer that is down is the calls' failure, not the start's.
fn bearer_provider(settings: &Settings, c: &Connection<'_>) -> Result<Option<Bearer>, String> {
    #[cfg(feature = "oauth")]
    {
        let Some(auth) = c.auth else {
            return Ok(None);
        };
        let spec = settings.auth_spec(auth);
        // SigV4 (`kind: aws`) signs each request over its own method, path
        // and body, so there is no reusable bearer to hand back: the dial
        // carries the spec and signs itself.
        if spec.kind == "aws" {
            return Ok(None);
        }
        // Build the signer once (preserving the oauth2 in-memory refresh) and
        // extract the bearer per dial. Covers static / oauth2 device-login /
        // spiffe jwt — all bearer-style for intelligence.
        let target = c.login_target();
        let Some(signer) =
            crate::auth::device::signer_for(&spec, &target, std::time::Duration::from_secs(30))?
        else {
            return Ok(None);
        };
        Ok(Some(Arc::new(move || {
            signer
                .sign("POST", "", "", &[])?
                .into_iter()
                .find(|(k, _)| k.eq_ignore_ascii_case("authorization"))
                .map(|(_, v)| v.strip_prefix("Bearer ").unwrap_or(&v).to_string())
                .ok_or_else(|| format!("{target}: auth yields no bearer"))
        })))
    }
    #[cfg(not(feature = "oauth"))]
    {
        let _ = (settings, c);
        Ok(None)
    }
}

impl Runtime {
    /// The model `reference` names — or, for `None`, the `turn` role's — as a
    /// worker dials it: its provider's connection with a credential current
    /// at this instant, the wire model, and its fallback chain, each link on
    /// its own provider. The config keeps `reference` as given, so a stored
    /// one resolves the same way again: a named model, or whatever `turn`
    /// names by then.
    pub(crate) fn intel_config(&self, reference: Option<&str>) -> IntelConfig {
        self.resolve_in(&self.settings.intelligence, reference)
    }

    /// The model `role` runs on (RFC 0046 §5.4), as [`Self::intel_config`].
    ///
    /// `approve` resolves in the operator's own `intelligence` section when
    /// a served document merged into it ([`Intelligence::operator`]): the
    /// operator's `roles.approve`, else the operator's turn model, with the
    /// operator's models and fallbacks — never a name a document declared
    /// or re-bound. Only when the operator names no model at all, and so
    /// left the choice of every model to the document, does the judge run on
    /// the agent's.
    pub(crate) fn role_config(&self, role: ModelRole) -> IntelConfig {
        let section = self.role_section(role);
        self.resolve_in(section, section.reference(role).as_deref())
    }

    /// The `intelligence` section `role` resolves in: the operator's own for
    /// `approve` when a served document merged into it and the operator
    /// names a model, else the live one ([`Self::role_config`]).
    pub(crate) fn role_section(&self, role: ModelRole) -> &Intelligence {
        let intel = &self.settings.intelligence;
        match intel.operator.as_deref() {
            Some(op) if role == ModelRole::Approve && op.reference(role).is_some() => op,
            _ => intel,
        }
    }

    /// [`Self::intel_config`] against a given `intelligence` section.
    pub(crate) fn resolve_in(&self, intel: &Intelligence, reference: Option<&str>) -> IntelConfig {
        let resolved = reference
            .map(str::to_string)
            .or_else(|| intel.reference(ModelRole::Turn));
        let mut cfg = self.intel_link(intel, resolved.as_deref());
        if let Some(r) = &resolved {
            cfg.fallbacks = intel
                .fallback_chain(r)
                .iter()
                .map(|f| self.intel_link(intel, Some(f)))
                .collect();
        }
        cfg.reference = reference.map(str::to_string);
        cfg
    }

    /// The wire model the agent's turns send — what status reports.
    pub(crate) fn turn_model(&self) -> String {
        self.settings
            .intelligence
            .role_wire_model(ModelRole::Turn)
            .unwrap_or_default()
    }

    /// One link of a chain: a model on its connection, without fallbacks.
    fn intel_link(&self, intel: &Intelligence, reference: Option<&str>) -> IntelConfig {
        let provider = reference.and_then(|r| intel.provider_of(r));
        let conn = self.intel.get(provider);
        // An `auth:` credential overrides `token`. One that cannot be had
        // right now — no login, a token file that cannot be read — fails this
        // model's calls, unsent: never the static token or no credential in
        // its place. Asked again at the next resolution, and never fatal, so a
        // rotation caught mid-write does not take a daemon down.
        let credential = match conn.bearer.as_ref() {
            Some(bearer) => bearer().map(Some),
            None => super::bearer_now(
                &conn.target,
                conn.token_file.as_deref(),
                conn.token.as_deref(),
            ),
        };
        let (token, credential_error) = match credential {
            Ok(token) => (token, None),
            Err(e) => (None, Some(e)),
        };
        // A reasoning model's defaults; an effort a node or a call names is
        // put over them where the call is made.
        let reasoning = reference
            .and_then(|r| intel.tier(r))
            .filter(|t| t.kind == crate::config::settings::ModelKind::Reasoning)
            .map(|t| {
                let d = t.reasoning.clone().unwrap_or_default();
                crate::wire::intel::ReasoningAsk {
                    effort: d.effort.unwrap_or(crate::wire::intel::Effort::Medium),
                    budget: d.budget,
                }
            });
        IntelConfig {
            uri: conn.uri.clone(),
            token,
            model: reference.map(|r| intel.wire_model(r)),
            headers: conn.headers.clone(),
            aws_auth: conn.aws.clone(),
            dialect: conn.dialect.clone(),
            provider: provider.map(str::to_string),
            timeout_ms: conn.timeout_ms,
            reference: None,
            fallbacks: Vec::new(),
            reasoning,
            structured_prompt: conn.structured_prompt,
            credential_error,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings(yaml: &str) -> Settings {
        Settings::from_document(crate::config::yaml::parse(yaml).unwrap(), "t").unwrap()
    }

    /// A header whose secret does not resolve refuses the connection — the
    /// start, or the reload — rather than dialing without it.
    #[test]
    fn a_header_whose_secret_does_not_resolve_refuses_the_connection() {
        let s = settings(
            "intelligence:\n  endpoints: https://intel.example/v1\n  headers: {X-Api-Key: \"{{secret:INTEL_TEST_UNSET_KEY}}\"}\n",
        );
        let none = |_: &str| None;
        let e = Conns::build(&s, &none).err().expect("refused");
        assert!(
            e.contains("headers.X-Api-Key") && e.contains("INTEL_TEST_UNSET_KEY"),
            "{e}"
        );
        let set = |k: &str| (k == "INTEL_TEST_UNSET_KEY").then(|| "k".to_string());
        let conns = Conns::build(&s, &set).expect("resolves");
        assert_eq!(
            conns.get(None).headers,
            vec![("X-Api-Key".to_string(), "k".to_string())]
        );
    }

    /// A device login that is not cached is no start failure: the connection
    /// builds, dialing nothing, and its bearer says what is missing.
    #[cfg(feature = "oauth")]
    #[test]
    fn a_missing_login_is_the_bearers_error_not_the_starts() {
        let s = settings(
            "intelligence:\n  endpoints: https://intel.example/v1\n  providers:\n    dev-missing:\n      endpoints: https://intel.example/v1\n      auth: {kind: oauth2, grant: device, client_id: c, issuer: \"http://127.0.0.1:9\"}\n",
        );
        let none = |_: &str| None;
        let conns = Conns::build(&s, &none).expect("builds without a login");
        let bearer = conns
            .get(Some("dev-missing"))
            .bearer
            .as_ref()
            .expect("an auth bearer");
        let e = bearer().unwrap_err();
        assert!(
            e.starts_with("intelligence:dev-missing: no cached credential")
                && e.contains("run `agentd --login intelligence:dev-missing`"),
            "{e}"
        );
    }
}
