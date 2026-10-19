// SPDX-License-Identifier: AGPL-3.0-only
//! A client metadata document and the keys behind it: the one implementation
//! of every client identity agentd publishes — `security.client`, its Client
//! ID Metadata Document (RFC 0052,
//! draft-ietf-oauth-client-id-metadata-document-02), and
//! `security.companies.client`, the identity every Company sees (RFC 0047).
//! Each is a JSON document at its `client_id` URL, a key set at its
//! `jwks_uri`, and the ES256 key that signs its `private_key_jwt`
//! assertions.
//!
//! The key is a PKCS#8 file created `0600` when absent; its `kid` is its RFC
//! 7638 thumbprint. The key set lists it and every published key — the next
//! one before a rotation, the previous one after — by their public halves
//! only. The two documents are printed for static hosting, or served by the
//! A2A listener at their URLs' paths; either way, each is first held to what
//! the draft lets a document carry ([`check`]).

use std::path::{Path, PathBuf};

use serde_json::{Map, Value, json};

use crate::auth::client_assertion::ClientKey;
use crate::config::settings::companies::path_of;

/// The file names a print writes.
pub const METADATA_FILE: &str = "client.json";
pub const JWKS_FILE: &str = "jwks.json";

/// `token_endpoint_auth_method` values built on a shared secret, which a
/// document must not name (draft §4.1).
const SECRET_METHODS: &[&str] = &[
    "client_secret_post",
    "client_secret_basic",
    "client_secret_jwt",
];

/// JWK members that hold private key material (RFC 7518 §6.2.2, §6.3.2,
/// §6.4; RFC 8037 §2).
const PRIVATE_MEMBERS: &[&str] = &["d", "p", "q", "dp", "dq", "qi", "oth", "k"];

/// A client identity: its document, its signing key and the keys it
/// publishes.
pub struct Identity {
    pub client_id: String,
    pub jwks_uri: String,
    key: ClientKey,
    published: Vec<ClientKey>,
    metadata: Value,
}

impl Identity {
    /// The identity at `client_id` whose document holds `members` and the
    /// members every one of agentd's has — `client_id`, `jwks_uri` and
    /// `token_endpoint_auth_method: private_key_jwt` — with its key loaded
    /// from `key_file`, or created `0600`, and each of `published_key_files`
    /// likewise. `at` names the configuration section in an error.
    pub fn load(
        client_id: &str,
        jwks_uri: &str,
        members: Map<String, Value>,
        key_file: &str,
        published_key_files: &[String],
        at: &str,
    ) -> Result<Identity, String> {
        let key = ClientKey::load_or_create(Path::new(key_file))
            .map_err(|e| format!("{at}.key_file: {e}"))?;
        let mut published = Vec::new();
        for p in published_key_files {
            published.push(
                ClientKey::load_or_create(Path::new(p))
                    .map_err(|e| format!("{at}.published_key_files: {e}"))?,
            );
        }
        let mut metadata = Value::Object(members);
        metadata["client_id"] = json!(client_id);
        metadata["jwks_uri"] = json!(jwks_uri);
        metadata["token_endpoint_auth_method"] = json!("private_key_jwt");
        let id = Identity {
            client_id: client_id.to_string(),
            jwks_uri: jwks_uri.to_string(),
            key,
            published,
            metadata,
        };
        check(&id.metadata, &id.jwks()).map_err(|e| format!("{at}: {e}"))?;
        Ok(id)
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

    /// The signing key, for an assertion of another kind than a client's.
    pub fn key(&self) -> &ClientKey {
        &self.key
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

    /// A client assertion (RFC 7523 §2.2): `iss` = `sub` = the `client_id`,
    /// `aud` as given, a minute's life, a fresh `jti`.
    pub fn client_assertion(&self, aud: &str) -> Result<String, String> {
        self.key.client_assertion(&self.client_id, aud)
    }
}

/// `security.client`'s identity (RFC 0052): a Client ID Metadata Document
/// with `client_name`, `logo_uri`, its loopback `redirect_uris`, and the
/// `grant_types` and `response_types` its grants give, and the key behind it.
pub fn of_client(c: &crate::config::settings::client::Client) -> Result<Identity, String> {
    let mut members = Map::new();
    members.insert("client_name".into(), json!(c.name));
    if let Some(l) = &c.logo_uri {
        members.insert("logo_uri".into(), json!(l));
    }
    // Always present: MCP's client registration names it among the members a
    // document must carry, even where no grant returns to one.
    members.insert("redirect_uris".into(), json!(c.redirect_uris));
    members.insert("grant_types".into(), json!(c.grant_types()));
    members.insert("response_types".into(), json!(c.response_types()));
    Identity::load(
        &c.id,
        &c.jwks_uri(),
        members,
        c.key_file(),
        &c.published_key_files,
        "security.client",
    )
}

/// `--print-client-metadata <dir>`: write `security.client`'s document and
/// key set into `dir`, for static hosting at the `client_id` and `jwks_uri`,
/// creating the signing key `0600` when absent. Dials nothing.
pub fn print_client(
    settings: &crate::config::settings::Settings,
    dir: &Path,
) -> Result<Vec<PathBuf>, String> {
    let c = settings
        .security
        .client
        .as_ref()
        .ok_or("security.client is not configured")?;
    of_client(c)?.print(dir)
}

/// What the draft lets a client metadata document carry (§4, §4.1), held to
/// agentd's own before it is printed or served: `client_id` a Client
/// Identifier URL, no `client_secret` or `client_secret_expires_at`, no
/// authentication method built on a shared secret, a key set for a
/// `private_key_jwt` client, and no private key member in any key it
/// publishes — the document's `jwks` or the separate key set `jwks`.
pub fn check(doc: &Value, jwks: &Value) -> Result<(), String> {
    let Some(obj) = doc.as_object() else {
        return Err("a client metadata document is a JSON object".into());
    };
    let id = obj
        .get("client_id")
        .and_then(Value::as_str)
        .ok_or("the document has no client_id")?;
    if let Some(why) = crate::config::settings::client::client_id_url_error(id) {
        return Err(format!("client_id {id:?} {why} (draft §3)"));
    }
    for m in ["client_secret", "client_secret_expires_at"] {
        if obj.contains_key(m) {
            return Err(format!(
                "the document carries {m}, which the draft forbids (§4.1)"
            ));
        }
    }
    let method = obj
        .get("token_endpoint_auth_method")
        .and_then(Value::as_str);
    if let Some(m) = method
        && SECRET_METHODS.contains(&m)
    {
        return Err(format!(
            "token_endpoint_auth_method {m} is built on a shared secret, which the draft forbids (§4.1)"
        ));
    }
    if method == Some("private_key_jwt")
        && !obj.contains_key("jwks_uri")
        && !obj.contains_key("jwks")
    {
        return Err("a private_key_jwt client publishes its keys: jwks_uri or jwks".into());
    }
    for set in [obj.get("jwks"), Some(jwks)].into_iter().flatten() {
        for k in set["keys"].as_array().into_iter().flatten() {
            if let Some(m) = PRIVATE_MEMBERS.iter().find(|m| k.get(**m).is_some()) {
                return Err(format!(
                    "a published key carries the private member {m:?}; only public keys are published (§4.1)"
                ));
            }
        }
    }
    Ok(())
}

fn pretty(v: &Value) -> Vec<u8> {
    let mut out = serde_json::to_vec_pretty(v).unwrap_or_default();
    out.push(b'\n');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn members() -> Map<String, Value> {
        let mut m = Map::new();
        m.insert("client_name".into(), json!("Dana's agent"));
        m.insert("redirect_uris".into(), json!([]));
        m
    }

    /// The key is created `0600` and kept; the key set holds it and the
    /// published key by their public halves; the document carries the
    /// members every identity has; and both are served at their URLs' paths
    /// and printed under their file names.
    #[test]
    fn an_identity_is_its_key_file_and_publishes_public_halves() {
        let dir = tempfile::tempdir().unwrap();
        let key = dir.path().join("key").display().to_string();
        let next = dir.path().join("next").display().to_string();
        let load = || {
            Identity::load(
                "https://agent.example/oauth/client.json",
                "https://agent.example/oauth/jwks.json",
                members(),
                &key,
                std::slice::from_ref(&next),
                "security.client",
            )
            .unwrap()
        };
        let id = load();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&key).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        assert_eq!(load().kid(), id.kid(), "the same key across loads");
        let jwks = id.jwks();
        let keys = jwks["keys"].as_array().unwrap();
        assert_eq!(keys.len(), 2);
        assert_eq!(keys[0]["kid"], id.kid());
        assert!(
            keys.iter().all(|k| k.get("d").is_none()),
            "public halves only"
        );
        assert_eq!(
            id.metadata(),
            &json!({
                "client_id": "https://agent.example/oauth/client.json",
                "client_name": "Dana's agent",
                "jwks_uri": "https://agent.example/oauth/jwks.json",
                "redirect_uris": [],
                "token_endpoint_auth_method": "private_key_jwt",
            })
        );
        let paths: Vec<String> = id.documents().into_iter().map(|(p, _)| p).collect();
        assert_eq!(paths, ["/oauth/client.json", "/oauth/jwks.json"]);
        let printed = id.print(&dir.path().join("out")).unwrap();
        assert!(printed[0].ends_with(METADATA_FILE) && printed[1].ends_with(JWKS_FILE));
        let doc: Value = serde_json::from_slice(&std::fs::read(&printed[0]).unwrap()).unwrap();
        assert_eq!(&doc, id.metadata());
        let set: Value = serde_json::from_slice(&std::fs::read(&printed[1]).unwrap()).unwrap();
        assert_eq!(set, id.jwks());
        // A client_id the draft refuses never reaches a document.
        let e = Identity::load(
            "https://agent.example/a/../client.json",
            "https://agent.example/jwks.json",
            members(),
            &key,
            &[],
            "security.client",
        )
        .err()
        .unwrap();
        assert!(e.starts_with("security.client: client_id"), "{e}");
    }

    /// A document with a secret, a shared-secret method, no key set for a
    /// private_key_jwt client, or a private key member is refused.
    #[test]
    fn a_document_carries_no_secret_and_no_private_key() {
        let doc = json!({
            "client_id": "https://agent.example/client.json",
            "jwks_uri": "https://agent.example/jwks.json",
            "token_endpoint_auth_method": "private_key_jwt",
        });
        let public = json!({"keys": [{"kty": "EC", "crv": "P-256", "x": "a", "y": "b"}]});
        assert_eq!(check(&doc, &public), Ok(()));
        let with = |k: &str, v: Value| {
            let mut d = doc.clone();
            d[k] = v;
            d
        };
        for (bad, want) in [
            (with("client_secret", json!("s")), "client_secret"),
            (
                with("client_secret_expires_at", json!(0)),
                "client_secret_expires_at",
            ),
            (
                with("token_endpoint_auth_method", json!("client_secret_basic")),
                "shared secret",
            ),
            (
                with("token_endpoint_auth_method", json!("client_secret_jwt")),
                "shared secret",
            ),
            (
                with("client_id", json!("https://agent.example/client.json?v=2")),
                "query",
            ),
            (
                with("jwks", json!({"keys": [{"kty": "oct", "k": "c2VjcmV0"}]})),
                "\"k\"",
            ),
        ] {
            let e = check(&bad, &public).unwrap_err();
            assert!(e.contains(want), "{bad} → {e}");
        }
        let mut no_keys = doc.clone();
        no_keys.as_object_mut().unwrap().remove("jwks_uri");
        assert!(
            check(&no_keys, &json!({}))
                .unwrap_err()
                .contains("publishes its keys")
        );
        let private =
            json!({"keys": [{"kty": "EC", "crv": "P-256", "x": "a", "y": "b", "d": "c"}]});
        assert!(check(&doc, &private).unwrap_err().contains("\"d\""));
        assert!(check(&json!([]), &public).is_err());
    }
}
