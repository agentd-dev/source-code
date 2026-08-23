// SPDX-License-Identifier: AGPL-3.0-only
//! Endpoint authentication — the interactive & workload credential providers and
//! the durable token cache they share (RFC 0031).
//!
//! The existing static-header path (`mcp::auth`) and the AAuth request-signer
//! (`aauth`) are the `static`/`aauth` providers. This module owns the *new*
//! surfaces: the durable [`Kind::Cred`](crate::state::Kind::Cred) [`cache`], and
//! the OAuth 2.1 / OIDC [`oauth2`] flows (device grant + refresh + discovery)
//! that power `agentd login`. AWS SigV4/SSO and SPIFFE providers land behind the
//! `aws`/`spiffe` features in later phases.

pub mod cache;

#[cfg(feature = "oauth")]
pub mod oauth2;

#[cfg(feature = "oauth")]
pub mod challenge;

#[cfg(feature = "oauth")]
pub mod login;

#[cfg(feature = "oauth")]
pub mod device;

#[cfg(feature = "oauth")]
pub mod aws;

#[cfg(feature = "oauth")]
pub mod aws_sso;

#[cfg(feature = "oauth")]
pub mod browser;

/// Canonicalize a login/logout target (RFC 0037): `mcp:<name>` on a server that
/// references a catalog entry becomes `service:<entry>` — the key the daemon's
/// connect path reads — so a login (or logout) lands where every consumer of
/// the entry shares it. Feature-free: logout works without `oauth`.
pub fn canonical_target(settings: &crate::config::v2::Settings, target: &str) -> String {
    if let Some(name) = target.strip_prefix("mcp:")
        && let Some(svc) = settings
            .mcp
            .servers
            .iter()
            .find(|s| s.name == name)
            .and_then(|s| s.service.clone())
    {
        return format!("service:{svc}");
    }
    target.to_string()
}
