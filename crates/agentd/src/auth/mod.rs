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
