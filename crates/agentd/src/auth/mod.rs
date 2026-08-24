// SPDX-License-Identifier: AGPL-3.0-only
//! Endpoint authentication — the interactive and workload credential providers
//! and the token cache they share.
//!
//! Static headers (`mcp::auth`) and the AAuth request-signer (`aauth`) supply
//! the `static` and `aauth` providers. This module owns the rest: the
//! [`Kind::Cred`](crate::state::Kind::Cred)-backed [`cache`], the OAuth 2.1 /
//! OIDC [`oauth2`] flows (device grant, browser + PKCE, refresh, discovery)
//! behind `agentd login`, and the AWS SigV4 / IAM Identity Center providers.
//! Everything but the cache is gated on the `oauth` cargo feature, so a build
//! without it carries no interactive-login code at all.

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

/// Canonicalize a login/logout target: `mcp:<name>` on a server that references
/// a service-catalog entry becomes `service:<entry>`, the key the daemon's
/// connect path actually reads. Every server pointing at that entry shares one
/// credential, so a login must land where all of them look and a logout must
/// revoke it for all of them at once. Deliberately outside the `oauth` feature
/// gate, so logout still resolves in a build without interactive login.
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
