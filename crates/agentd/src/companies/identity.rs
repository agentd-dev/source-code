// SPDX-License-Identifier: AGPL-3.0-only
//! The client identity every Company sees (RFC 0047 §5.5.2, draft §4.1): the
//! client metadata document at the `client_id` URL, the key set at its
//! `jwks_uri`, and the ES256 key that signs every client assertion and
//! Session assertion.
//!
//! The key lives in `security.companies.client.key_file`, a PKCS#8 file
//! created `0600` when absent; its `kid` is its RFC 7638 thumbprint. Only
//! the supervisor reads it, and no child is handed it. The key set lists it
//! and every `published_key_files` key — the next one before a rotation,
//! the previous one after — by their public halves only.
//!
//! The documents are published one of two ways: printed for static hosting
//! (`agentd --print-company-client <dir>`), or served by the A2A listener at
//! their URLs' paths (`client.serve`). The document's `redirect_uris` lists
//! `client.redirect_uri` — where Direct Sign-In returns the User, which only
//! the listener serves — when some Company may be signed in to directly, and
//! is `[]` otherwise. Before its first Session the
//! supervisor fetches both from where they are published and requires what
//! it finds to be what it would print, with the signing key in the set —
//! the self-check; a Company would refuse its assertions otherwise, and this
//! names the cause.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::{Value, json};

use crate::auth::client_assertion::{ClientKey, LIFETIME_S};
use crate::config::settings::companies::{Client, path_of};

/// The file names `--print-company-client` writes.
pub const METADATA_FILE: &str = "client.json";
pub const JWKS_FILE: &str = "jwks.json";

/// How long a Company may cache the served documents.
pub const SERVED_MAX_AGE_S: u64 = 300;

/// The client identity.
pub struct Identity {
    pub client_id: String,
    pub jwks_uri: String,
    /// Where Direct Sign-In returns the User, when some Company may be
    /// signed in to directly.
    pub redirect_uri: Option<String>,
    key: ClientKey,
    published: Vec<ClientKey>,
    metadata: Value,
}

impl Identity {
    /// The identity `client` configures: its key loaded, or created `0600`,
    /// and each published key likewise. With `direct`, its `redirect_uri` is
    /// listed for Direct Sign-In.
    pub fn load(client: &Client, direct: bool) -> Result<Identity, String> {
        let key = ClientKey::load_or_create(Path::new(client.key_file()))
            .map_err(|e| format!("security.companies.client.key_file: {e}"))?;
        let mut published = Vec::new();
        for p in &client.published_key_files {
            published.push(
                ClientKey::load_or_create(Path::new(p))
                    .map_err(|e| format!("security.companies.client.published_key_files: {e}"))?,
            );
        }
        let jwks_uri = client.jwks_uri();
        let redirect_uri = direct.then(|| client.redirect_uri());
        let mut metadata = json!({
            "client_id": client.id,
            "client_name": client.name,
            "jwks_uri": jwks_uri,
            // Always present: the kit's schema requires the member, and a
            // Company running its validator refuses a document without it.
            // Nothing is listed until a Company may be signed in to directly.
            "redirect_uris": redirect_uri.iter().collect::<Vec<_>>(),
            "token_endpoint_auth_method": "private_key_jwt",
        });
        if let Some(l) = &client.logo_uri {
            metadata["logo_uri"] = json!(l);
        }
        // The extensions it supports, each at the version agentd speaks: a
        // Company returns operations only to an agent that lists them
        // (Operations §2).
        if !client.extensions.is_empty() {
            let mut ext = serde_json::Map::new();
            for x in &client.extensions {
                match x {
                    crate::config::settings::companies::Extension::Operations => {
                        ext.insert(
                            super::wire::operations::EXTENSION.into(),
                            json!({"version": super::wire::operations::VERSION}),
                        );
                    }
                }
            }
            metadata[super::wire::EXTENSIONS_MEMBER] = Value::Object(ext);
        }
        Ok(Identity {
            client_id: client.id.clone(),
            jwks_uri,
            redirect_uri,
            key,
            published,
            metadata,
        })
    }

    /// The client metadata document.
    pub fn metadata(&self) -> &Value {
        &self.metadata
    }

    /// The key set: the signing key and every published one, public halves
    /// only, each once.
    pub fn jwks(&self) -> Value {
        let mut keys: Vec<Value> = Vec::new();
        for k in std::iter::once(&self.key).chain(self.published.iter()) {
            let j = k.public_jwk();
            if !keys.iter().any(|x| x["kid"] == j["kid"]) {
                keys.push(j);
            }
        }
        json!({"keys": keys})
    }

    /// The `kid` assertions are signed under.
    pub fn kid(&self) -> &str {
        self.key.kid()
    }

    /// The two documents and the paths of their URLs, for the listener to
    /// serve.
    pub fn documents(&self) -> Vec<(String, Vec<u8>)> {
        vec![
            (path_of(&self.client_id), pretty(&self.metadata)),
            (path_of(&self.jwks_uri), pretty(&self.jwks())),
        ]
    }

    /// Write `client.json` and `jwks.json` into `dir`, for static hosting at
    /// the `client_id` and `jwks_uri`.
    pub fn print(&self, dir: &Path) -> Result<Vec<PathBuf>, String> {
        std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        let mut out = Vec::new();
        for (name, doc) in [
            (METADATA_FILE, self.metadata.clone()),
            (JWKS_FILE, self.jwks()),
        ] {
            let p = dir.join(name);
            std::fs::write(&p, pretty(&doc)).map_err(|e| format!("{}: {e}", p.display()))?;
            out.push(p);
        }
        Ok(out)
    }

    /// A client assertion (§4.1, RFC 7523): `iss` = `sub` = the `client_id`,
    /// `aud` the Company's issuer as one string, a minute's life, a fresh
    /// `jti`.
    pub fn client_assertion(&self, issuer: &str) -> Result<String, String> {
        self.key.client_assertion(&self.client_id, issuer)
    }

    /// A Session assertion (§4.2): `iss` the `client_id`, `sub` the User ID,
    /// `aud` the issuer as one string, a minute's life, a fresh `jti`.
    pub fn session_assertion(&self, user_id: &str, issuer: &str) -> Result<String, String> {
        let iat = crate::auth::cache::now_ms() / 1000;
        let jti = crate::sec::random::b64url_token(16)
            .map_err(|e| format!("session assertion: no OS randomness for its jti: {e}"))?;
        self.key.sign_jwt(
            json!({"typ": "JWT"}),
            &json!({
                "iss": self.client_id,
                "sub": user_id,
                "aud": issuer,
                "iat": iat,
                "exp": iat + LIFETIME_S,
                "jti": jti,
            }),
        )
    }

    /// The self-check: the document at the `client_id` is the one this
    /// identity prints, and the key set at the `jwks_uri` holds the signing
    /// key. Both URLs are the operator's, dialled by name.
    pub fn self_check(&self, timeout: Duration) -> Result<(), String> {
        let fetch = |url: &str| -> Result<Value, String> {
            let r = crate::auth::http::get(
                url,
                &[("Accept", "application/json")],
                timeout,
                crate::auth::http::Dial::Named,
            )?;
            if r.status != 200 {
                return Err(format!("{url}: HTTP {}", r.status));
            }
            super::http::parse_unique(&r.body).map_err(|e| format!("{url}: {e}"))
        };
        let published = fetch(&self.client_id)?;
        if published != self.metadata {
            return Err(format!(
                "the document at {} is not the client metadata this configuration prints (re-run --print-company-client and publish it)",
                self.client_id
            ));
        }
        let jwks = fetch(&self.jwks_uri)?;
        let listed = jwks["keys"]
            .as_array()
            .is_some_and(|ks| ks.iter().any(|k| k["kid"] == self.kid()));
        if !listed {
            return Err(format!(
                "the key set at {} does not hold the signing key {}",
                self.jwks_uri,
                self.kid()
            ));
        }
        Ok(())
    }
}

fn pretty(v: &Value) -> Vec<u8> {
    let mut out = serde_json::to_vec_pretty(v).unwrap_or_default();
    out.push(b'\n');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn client(dir: &Path) -> Client {
        Client {
            id: "https://agent.example/companies/client.json".into(),
            name: "Dana's agent".into(),
            logo_uri: Some("https://agent.example/logo.png".into()),
            jwks_uri: None,
            key_file: Some(dir.join("key").display().to_string()),
            published_key_files: vec![dir.join("next").display().to_string()],
            redirect_uri: None,
            serve: false,
            extensions: Vec::new(),
        }
    }

    /// The key is created `0600` and kept, the key set holds it and the
    /// published key by their public halves, and the documents are served at
    /// their URLs' paths.
    #[test]
    fn the_identity_is_its_key_file_and_publishes_public_halves() {
        let dir = tempfile::tempdir().unwrap();
        let id = Identity::load(&client(dir.path()), false).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(dir.path().join("key"))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        let again = Identity::load(&client(dir.path()), false).unwrap();
        assert_eq!(again.kid(), id.kid(), "the same key across loads");
        let jwks = id.jwks();
        let keys = jwks["keys"].as_array().unwrap();
        assert_eq!(keys.len(), 2);
        assert_eq!(keys[0]["kid"], id.kid());
        for k in keys {
            assert!(k.get("d").is_none(), "public halves only");
        }
        assert_eq!(
            id.metadata()["jwks_uri"],
            "https://agent.example/companies/jwks.json"
        );
        assert_eq!(id.metadata()["redirect_uris"], json!([]));
        let paths: Vec<String> = id.documents().into_iter().map(|(p, _)| p).collect();
        assert_eq!(paths, ["/companies/client.json", "/companies/jwks.json"]);
        let printed = id.print(&dir.path().join("out")).unwrap();
        let doc: Value = serde_json::from_slice(&std::fs::read(&printed[0]).unwrap()).unwrap();
        assert_eq!(&doc, id.metadata());
        // A Company that may be signed in to directly: the callback is
        // listed, and only then.
        let direct = Identity::load(&client(dir.path()), true).unwrap();
        assert_eq!(
            direct.metadata()["redirect_uris"],
            json!(["https://agent.example/companies/callback"])
        );
        assert_eq!(
            direct.redirect_uri.as_deref(),
            Some("https://agent.example/companies/callback")
        );
        assert!(id.redirect_uri.is_none());
        assert!(
            id.metadata().get("extensions").is_none(),
            "nothing listed until configured"
        );
        // The Operations extension, listed at version 1 (Operations §2).
        let listing = Identity::load(
            &Client {
                extensions: vec![crate::config::settings::companies::Extension::Operations],
                ..client(dir.path())
            },
            false,
        )
        .unwrap();
        assert_eq!(
            listing.metadata()["extensions"],
            json!({"operations": {"version": "1"}})
        );
    }
}
