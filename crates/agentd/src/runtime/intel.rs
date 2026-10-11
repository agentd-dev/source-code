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

use serde_json::json;

use super::Runtime;
use crate::config::settings::{AuthKind, Connection, Intelligence, ModelRole, Settings};
use crate::subagent::protocol::IntelConfig;

/// A refreshing credential: the current bearer, from the login cache.
type Bearer = Arc<dyn Fn() -> Option<String> + Send + Sync>;

/// One connection, resolved: what its settings say at load, ready to dial.
pub(crate) struct Conn {
    /// The comma-joined endpoint list.
    uri: String,
    /// The resolved static `token`.
    token: Option<String>,
    /// `token_file`, read at each dial (a rotated file needs no reload). Only
    /// when no `token` is set: the inline token wins, as validation says.
    token_file: Option<String>,
    /// The resolved `headers`; a header whose secret does not resolve is
    /// dropped rather than sent with a placeholder in it.
    headers: Vec<(String, String)>,
    /// A refreshing credential (`auth:` oauth2, static, spiffe), overriding
    /// `token`.
    bearer: Option<Bearer>,
    /// `auth: {kind: aws}`: the dial signs itself.
    aws: Option<crate::config::AuthSpec>,
    dialect: Option<String>,
    timeout_ms: Option<u64>,
}

/// Every connection: the top-level one and each provider by name.
pub(crate) struct Conns {
    top: Conn,
    providers: BTreeMap<String, Conn>,
}

impl Conns {
    /// Resolve every connection's credential. A token that does not resolve,
    /// or a token file that cannot be read, refuses — the start, or the
    /// reload that would have gone live with it.
    pub(crate) fn build(
        settings: &Settings,
        env: &dyn Fn(&str) -> Option<String>,
    ) -> Result<Conns, String> {
        let intel = &settings.intelligence;
        let top = Conn::build(&intel.top_connection(), env)?;
        let mut providers = BTreeMap::new();
        for c in intel.connections() {
            if let Some(name) = c.name {
                providers.insert(name.to_string(), Conn::build(&c, env)?);
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
    fn build(c: &Connection<'_>, env: &dyn Fn(&str) -> Option<String>) -> Result<Conn, String> {
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
            .filter_map(|(k, v)| {
                crate::sec::secret::resolve(v, env)
                    .ok()
                    .map(|r| (k.clone(), r))
            })
            .collect();
        Ok(Conn {
            uri: c.endpoint_list().unwrap_or_default(),
            token,
            token_file,
            headers,
            bearer: bearer_provider(c),
            aws: c
                .auth
                .filter(|a| a.kind == AuthKind::Aws)
                .map(|a| a.to_spec()),
            dialect: c.dialect.map(str::to_string),
            timeout_ms: c.timeout.map(|t| t.as_millis() as u64),
        })
    }
}

/// The refreshing credential for a connection's `auth:` block, from the
/// cache its `agentd login` filled (`intelligence`, `intelligence:<name>`).
///
/// `None` when no bearer-style `auth:` is configured (and always without
/// `--features oauth`), which leaves the static `token` path untouched.
fn bearer_provider(c: &Connection<'_>) -> Option<Bearer> {
    #[cfg(feature = "oauth")]
    {
        let spec = c.auth?.to_spec();
        // SigV4 (`kind: aws`) signs each request over its own method, path
        // and body, so there is no reusable bearer to hand back: the dial
        // carries the spec and signs itself.
        if spec.kind == "aws" {
            return None;
        }
        // Build the signer once (preserving the oauth2 in-memory refresh) and
        // extract the bearer per dial. Covers static / oauth2 device-login /
        // spiffe jwt — all bearer-style for intelligence.
        let signer = crate::auth::device::signer_for(
            &spec,
            &c.login_target(),
            std::time::Duration::from_secs(30),
        )
        .ok()??;
        Some(Arc::new(move || {
            signer
                .sign("POST", "", "", &[])
                .into_iter()
                .find(|(k, _)| k.eq_ignore_ascii_case("authorization"))
                .map(|(_, v)| v.strip_prefix("Bearer ").unwrap_or(&v).to_string())
        }))
    }
    #[cfg(not(feature = "oauth"))]
    {
        let _ = c;
        None
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
        let (token, err) = super::bearer_now(
            conn.bearer.as_ref().and_then(|f| f()),
            conn.token_file.as_deref(),
            conn.token.as_deref(),
        );
        if let Some(e) = err {
            // Never fatal here: the dial that follows fails with the
            // provider's own 401, and a rotation caught mid-write should not
            // take a daemon down.
            self.log.warn(
                "intel.token_file.error",
                json!({"provider": provider, "err": e}),
            );
        }
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
        }
    }
}
