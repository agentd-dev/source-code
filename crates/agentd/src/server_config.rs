//! Server-level HTTP config — TLS termination, client auth (mTLS).
//!
//! The `[server]` block in the workflow TOML is the source of truth.
//! Plain HTTP is the default (no block / `[server]` without `tls`);
//! adding `[server.tls]` turns on HTTPS; adding
//! `[server.tls.client_auth]` with `mode = "required"` adds mTLS.
//!
//! ```toml
//! [server.tls]
//! cert_file = "/etc/ssl/server.pem"
//! key_file  = "/etc/ssl/server.key"
//!
//! [server.tls.client_auth]
//! mode    = "required"          # only `required` is wired today
//! ca_file = "/etc/ssl/client-ca.pem"
//! ```
//!
//! The actual rustls [`rustls::ServerConfig`] build happens in
//! [`triggers::http::tls`](crate::triggers::http) under the
//! `server-tls` feature — this module stays unconditional so the
//! config types parse cleanly even in builds that don't ship TLS.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// `[server]` block. Extensible; today only `tls` is defined.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ServerConfig {
    #[serde(default)]
    pub tls: Option<TlsConfig>,
}

/// `[server.tls]`. Absence of this block means plain HTTP.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct TlsConfig {
    /// PEM-encoded server cert chain. Can hold multiple certs
    /// (leaf first, intermediates after).
    pub cert_file: PathBuf,
    /// PEM-encoded private key matching the first cert in
    /// `cert_file`. `PKCS8`, `RSA`, and `EC` keys are supported.
    pub key_file: PathBuf,
    /// Optional `[server.tls.client_auth]` sub-block. Present =
    /// mTLS is required.
    #[serde(default)]
    pub client_auth: Option<ClientAuthConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ClientAuthConfig {
    pub mode: ClientAuthMode,
    pub ca_file: PathBuf,
}

/// mTLS handshake policy.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum ClientAuthMode {
    /// No client cert demanded.
    #[default]
    None,
    /// (Deferred for R3b MVP.) Connections may present a cert;
    /// accepted either way.
    Optional,
    /// All connections must present a valid cert signed by the
    /// configured CA. rustls handshakes without a cert → aborted.
    Required,
}

impl ClientAuthMode {
    pub fn is_required(&self) -> bool {
        matches!(self, ClientAuthMode::Required)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_server_block_parses() {
        let cfg: ServerConfig = toml::from_str("").unwrap();
        assert!(cfg.tls.is_none());
    }

    #[test]
    fn tls_block_parses() {
        let src = r#"
            [tls]
            cert_file = "/a/cert.pem"
            key_file  = "/a/key.pem"
        "#;
        let cfg: ServerConfig = toml::from_str(src).unwrap();
        let tls = cfg.tls.unwrap();
        assert_eq!(tls.cert_file, PathBuf::from("/a/cert.pem"));
        assert_eq!(tls.key_file, PathBuf::from("/a/key.pem"));
        assert!(tls.client_auth.is_none());
    }

    #[test]
    fn mtls_block_parses() {
        let src = r#"
            [tls]
            cert_file = "/a/cert.pem"
            key_file  = "/a/key.pem"

            [tls.client_auth]
            mode    = "required"
            ca_file = "/a/ca.pem"
        "#;
        let cfg: ServerConfig = toml::from_str(src).unwrap();
        let auth = cfg.tls.unwrap().client_auth.unwrap();
        assert_eq!(auth.mode, ClientAuthMode::Required);
        assert_eq!(auth.ca_file, PathBuf::from("/a/ca.pem"));
        assert!(auth.mode.is_required());
    }

    #[test]
    fn unknown_field_rejected() {
        let src = r#"
            [tls]
            cert_file = "/a/cert.pem"
            key_file  = "/a/key.pem"
            surprise  = 42
        "#;
        assert!(toml::from_str::<ServerConfig>(src).is_err());
    }

    #[test]
    fn unknown_client_auth_mode_rejected() {
        let src = r#"
            [tls]
            cert_file = "/a/cert.pem"
            key_file  = "/a/key.pem"
            [tls.client_auth]
            mode    = "handshakes-of-vengeance"
            ca_file = "/a/ca.pem"
        "#;
        assert!(toml::from_str::<ServerConfig>(src).is_err());
    }
}
