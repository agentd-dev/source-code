// SPDX-License-Identifier: AGPL-3.0-only
//! `agentd --logout <target>`: delete the login from the login cache, then
//! revoke its token at the authorization server that issued it (RFC 7009).
//!
//! The local delete comes first, before anything is dialled, so a logout cut
//! short — a server that does not answer, a Ctrl-C — has still removed the
//! credential here; the revocation is made from the record read under the
//! cache's lock. The revocation endpoint is the one the server's metadata
//! lists: the `issuer` the `auth:` block names, or the one RFC 9728
//! discovery finds for the resource, is asked for it. The refresh token is
//! revoked when there is one — which at most servers ends the grant and
//! every access token it issued (§2.1) — else the access token. A server
//! whose metadata lists no revocation endpoint, or a block with no issuer to
//! ask, leaves only the local delete. A revocation that fails is reported,
//! and the token stays good at the server until it expires.

use std::path::Path;
use std::time::Duration;

use crate::auth::cache::{self, CachedCred};
use crate::auth::login;
use crate::auth::oauth2::{self, OAuth2Params};
use crate::config::settings::{AuthKind, OAuthGrant, Settings};

/// What a logout did at the authorization server.
#[derive(Debug, PartialEq, Eq)]
pub enum Remote {
    /// Revoked there.
    Revoked,
    /// Nothing to do there: no cached credential, no OAuth login, or no
    /// revocation endpoint in the server's metadata.
    Nothing,
    /// The revocation was tried and failed, for this reason.
    Failed(String),
}

/// Log out of `target`: delete the cached credential under `dir`, then
/// revoke it at the server. `Ok` names what happened; `Err` is a local
/// delete that failed (nothing revoked), or a revocation that did (the local
/// delete done).
pub fn logout(
    settings: &Settings,
    target: &str,
    dir: &Path,
    timeout: Duration,
) -> Result<String, String> {
    let target = crate::auth::canonical_target(settings, target);
    let remote = match cache::take_file(dir, &target, timeout)? {
        Some(cred) => revoke_login(settings, &target, &cred, timeout),
        None => Remote::Nothing,
    };
    match remote {
        Remote::Revoked => Ok(format!(
            "logged out of {target} — revoked at the authorization server"
        )),
        Remote::Nothing => Ok(format!("logged out of {target}")),
        Remote::Failed(why) => Err(format!(
            "logged out of {target} here, but the revocation at the authorization server failed ({why}); the token stays good there until it expires"
        )),
    }
}

/// Revoke `cred`, the cached login of `target`, where its `auth:` block's
/// authorization server says to.
fn revoke_login(settings: &Settings, target: &str, cred: &CachedCred, timeout: Duration) -> Remote {
    let Ok(auth) = login::resolve_auth(settings, target) else {
        return Remote::Nothing;
    };
    if auth.kind != AuthKind::Oauth2 || auth.grant == Some(OAuthGrant::ClientCredentials) {
        return Remote::Nothing;
    }
    let resource = login::resource_of(settings, target).ok().flatten();
    let params = match revocation_params(auth, resource.as_deref(), timeout) {
        Ok(p) => p,
        Err(e) => return Remote::Failed(e),
    };
    match revoke(&params, cred, timeout) {
        Ok(true) => Remote::Revoked,
        Ok(false) => Remote::Nothing,
        Err(e) => Remote::Failed(e),
    }
}

/// The block's params with the revocation endpoint its issuer's metadata
/// lists, asked for even when every other endpoint is pinned — and asked
/// once: metadata fetched for the other endpoints already said.
fn revocation_params(
    auth: &crate::config::settings::Auth,
    resource: Option<&str>,
    timeout: Duration,
) -> Result<OAuth2Params, String> {
    let mut params = login::params_from_auth(auth, resource, timeout)?;
    if params.revocation_url.is_none()
        && login::pins_every_endpoint(auth)
        && let Some(issuer) = params.issuer.clone()
    {
        params.revocation_url = oauth2::discover(&issuer, timeout)?.revocation_endpoint;
    }
    Ok(params)
}

/// Revoke `cred` at `params`' revocation endpoint: its refresh token, else
/// its access token. `Ok(false)` when there is no endpoint to revoke at.
pub fn revoke(params: &OAuth2Params, cred: &CachedCred, timeout: Duration) -> Result<bool, String> {
    let Some(url) = params.revocation_url.as_deref() else {
        return Ok(false);
    };
    let (token, hint) = match &cred.refresh_token {
        Some(rt) => (rt.as_str(), "refresh_token"),
        None => (cred.access_token.as_str(), "access_token"),
    };
    oauth2::revoke(params, url, token, hint, timeout)?;
    Ok(true)
}
