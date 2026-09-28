// SPDX-License-Identifier: AGPL-3.0-only
//! Who is calling: the connection's evidence, and the principal it resolves to.

use std::net::SocketAddr;
use std::sync::Arc;

use super::{App, Auth};
use crate::a2a::Principal;

/// The verified identity of the client certificate, when one was presented.
#[derive(Clone, Default, Debug)]
pub struct PeerId {
    pub presented: bool,
    pub subject: Option<String>,
    pub sans: Vec<String>,
}

pub(super) fn peer_identity(conn: &tokio_rustls::rustls::ServerConnection) -> PeerId {
    let Some(chain) = conn.peer_certificates() else {
        return PeerId::default();
    };
    let Some(leaf) = chain.first() else {
        return PeerId {
            presented: true,
            ..Default::default()
        };
    };
    let id = crate::net::x509::parse(leaf.as_ref());
    PeerId {
        presented: true,
        subject: id.subject_cn,
        sans: id.sans,
    }
}

/// The remote address, for the loopback determination.
#[derive(Clone, Copy)]
pub(super) struct Peer(pub(super) SocketAddr);

/// Resolve the caller, or `None` for "present a credential".
///
/// The order is the order of trust: a verified certificate and the server
/// bearer are the operator; any other bearer may still match a configured
/// principal rule; and an uncredentialed request is refused unless the
/// listener has no credentials to require.
pub(super) fn resolve(
    app: &Arc<App>,
    peer_id: &PeerId,
    peer: SocketAddr,
    bearer: Option<&str>,
) -> Option<Principal> {
    let a = &app.auth;
    let loopback = peer.ip().is_loopback();
    let mgmt = (!a.require_auth && loopback) || peer_id.presented || is_server_bearer(a, bearer);
    if !a.require_auth {
        return Some(app.bridge.principal_of(
            true,
            bearer,
            peer_id.subject.clone(),
            peer_id.sans.clone(),
        ));
    }
    if !mgmt && bearer.is_none() {
        return None;
    }
    Some(
        app.bridge
            .principal_of(mgmt, bearer, peer_id.subject.clone(), peer_id.sans.clone()),
    )
}

fn is_server_bearer(a: &Auth, bearer: Option<&str>) -> bool {
    match (&a.server_bearer, bearer) {
        (Some(server), Some(got)) => crate::sha::ct_eq(server.as_bytes(), got.as_bytes()),
        _ => false,
    }
}
