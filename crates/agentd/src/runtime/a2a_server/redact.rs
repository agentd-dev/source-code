// SPDX-License-Identifier: AGPL-3.0-only
//! The effective-configuration view the `config` command answers with.

use serde_json::{Value, json};

/// The redaction marker a [`crate::config::v2::Secret`]'s `Debug` writes — the
/// one spelling this codebase uses for "a credential was here".
const REDACTED: &str = "***";

/// The effective settings document with every credential it carries replaced by
/// [`REDACTED`].
///
/// `settings_doc` is the merged files←env←flags layer. A FILE may only carry
/// `{{secret:…}}` references — an inline credential in a file is refused at
/// load — but an env- or flag-supplied credential sits INLINE in that
/// document. Answering the `config` command with the raw doc would therefore
/// echo live credentials back over a remote protocol surface, so every
/// credential is replaced before the document leaves the process.
///
/// Which values are credentials is not guessed from key names: the walk is
/// driven by the config JSON Schema, where every `Secret`-typed field is
/// declared with the one shared `secret` node and every header map with the one
/// shared `string_map` node (`config::v2::schema`). The schema/struct drift test
/// keeps a new field from being silently missing here, so the one act that
/// redacts a `Secret` added tomorrow is declaring it as a secret in the schema —
/// the same act that already makes it a credential everywhere else — rather than
/// a separate list of key names someone has to remember to extend.
pub(super) fn redact_settings(doc: &Value) -> Value {
    let schema = crate::config::v2::schema::schema();
    let defs = schema.get("$defs").cloned().unwrap_or(Value::Null);
    redact_by_schema(doc, &schema, &defs)
}

/// One node of [`redact_settings`]'s schema-guided walk.
fn redact_by_schema(v: &Value, node: &Value, defs: &Value) -> Value {
    let node = match node
        .get("$ref")
        .and_then(Value::as_str)
        .and_then(|r| r.strip_prefix("#/$defs/"))
    {
        Some(name) => defs.get(name).unwrap_or(node),
        None => node,
    };
    // A `oneOf` describes the same value several ways; a value is only as public
    // as its least public reading, so every branch gets to redact — and then the
    // node's OWN siblings still apply (McpServer's endpoint-xor-service `oneOf`
    // only constrains `required`; the secrets live in the sibling `properties`).
    if let Some(alts) = node.get("oneOf").and_then(Value::as_array) {
        let folded = alts
            .iter()
            .fold(v.clone(), |acc, alt| redact_by_schema(&acc, alt, defs));
        let mut rest = node.clone();
        if let Some(o) = rest.as_object_mut() {
            o.remove("oneOf");
        }
        return redact_by_schema(&folded, &rest, defs);
    }
    if is_secret_node(node) {
        return redact_value(v);
    }
    match v {
        Value::Object(o) => {
            let props = node.get("properties");
            // `additionalProperties` is `false` (closed object) or `true` (the
            // open `WorkflowRef`) far more often than it is a schema.
            let extra = node.get("additionalProperties").filter(|a| a.is_object());
            let headers = is_header_map_node(node);
            Value::Object(
                o.iter()
                    .map(|(k, x)| {
                        let out = match props.and_then(|p| p.get(k)).or(extra) {
                            // Header NAMES survive, values never do (see
                            // `is_header_map_node`).
                            _ if headers => redact_value(x),
                            Some(child) => redact_by_schema(x, child, defs),
                            // A key the schema does not describe. The only open
                            // node in the document is `WorkflowRef` — an inline
                            // dialect-3 workflow, whose own credentials are
                            // `{{secret:…}}` references — so this passes through
                            // rather than blanking a whole workflow definition.
                            None => x.clone(),
                        };
                        (k.clone(), out)
                    })
                    .collect(),
            )
        }
        Value::Array(a) => match node.get("items") {
            Some(items) => {
                Value::Array(a.iter().map(|x| redact_by_schema(x, items, defs)).collect())
            }
            None => v.clone(),
        },
        other => other.clone(),
    }
}

/// Is this schema node the shared `secret` node — the one every `Secret`-typed
/// field is declared with (`config::v2::schema`)?
fn is_secret_node(node: &Value) -> bool {
    node["type"] == "string"
        && node["description"]
            .as_str()
            .is_some_and(|d| d.starts_with("a secret"))
}

/// Is this schema node the shared `string_map` node — an object of plain string
/// values? Every one of them in the settings schema is a HEADER map, and a
/// header value is credential-bearing by nature (an `Authorization: Bearer …`
/// from env is inline), so this view exposes header NAMES only.
fn is_header_map_node(node: &Value) -> bool {
    node.get("properties").is_none() && node["additionalProperties"]["type"] == "string"
}

/// Replace one credential-bearing value.
///
/// A value that is EXACTLY one `{{secret:NAME}}` / `{{secret-file:PATH}}`
/// reference survives: it NAMES a credential instead of being one, and that
/// name is what makes the effective document useful to the operator reading it.
/// Anything else — an inline env/flag credential, or a template that merely
/// embeds a reference next to inline material — is replaced whole.
fn redact_value(v: &Value) -> Value {
    match v {
        Value::String(s) if is_bare_secret_ref(s) => v.clone(),
        Value::String(_) => json!(REDACTED),
        other => other.clone(),
    }
}

/// Whether `s` is a lone `{{secret:NAME}}` / `{{secret-file:PATH}}` reference
/// (nothing before it, nothing after it, no second ref inside).
fn is_bare_secret_ref(s: &str) -> bool {
    let Some(inner) = s
        .strip_prefix("{{secret:")
        .or_else(|| s.strip_prefix("{{secret-file:"))
    else {
        return false;
    };
    match inner.strip_suffix("}}") {
        Some(name) => !name.is_empty() && !name.contains('{') && !name.contains('}'),
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_config_view_redacts_every_credential_the_schema_declares() {
        // Env/flag values land INLINE in the effective document, which is what
        // the `config` command answers with — so each of these is a live
        // credential on a remote surface until it is redacted.
        let doc = json!({
            "intelligence": {
                "model": "gpt-5",
                "token": "sk-inline-from-env",
                "headers": {"Authorization": "Bearer sk-inline-header", "X-Tenant": "acme"},
                "auth": {"kind": "static", "token": "sk-inline-auth", "value": "sk-inline-value"}
            },
            "a2a": {"listen": "https://0.0.0.0:8443", "bearer": "{{secret:A2A_BEARER}}",
                    "peers": [{"name": "p", "endpoint": "https://p", "headers": {"X-Key": "k-inline"}}]},
            "mcp": {"servers": [{"name": "s", "endpoint": "https://s",
                    "oauth": {"token_url": "u", "client_id": "c", "client_secret": "cs-inline"}}]},
            "security": {"aauth": {"provider": "p", "enroll_token": "et-inline"}},
            "webhooks": {"default_auth": {"hmac": {"secret": "hs-inline"}}}
        });
        let r = redact_settings(&doc);
        assert!(
            !r.to_string().contains("inline"),
            "no credential survives the view: {r}"
        );
        assert_eq!(r["intelligence"]["token"], REDACTED);
        assert_eq!(r["intelligence"]["auth"]["token"], REDACTED);
        assert_eq!(r["intelligence"]["auth"]["value"], REDACTED);
        assert_eq!(r["intelligence"]["headers"]["Authorization"], REDACTED);
        assert_eq!(r["a2a"]["peers"][0]["headers"]["X-Key"], REDACTED);
        assert_eq!(r["mcp"]["servers"][0]["oauth"]["client_secret"], REDACTED);
        assert_eq!(r["security"]["aauth"]["enroll_token"], REDACTED);
        assert_eq!(r["webhooks"]["default_auth"]["hmac"]["secret"], REDACTED);
        // The view still has to be worth reading: structure, non-secret values
        // and header NAMES stay, and a bare reference names its secret.
        assert_eq!(r["intelligence"]["model"], "gpt-5");
        assert_eq!(r["a2a"]["listen"], "https://0.0.0.0:8443");
        assert_eq!(r["mcp"]["servers"][0]["oauth"]["client_id"], "c");
        assert_eq!(r["a2a"]["bearer"], "{{secret:A2A_BEARER}}");
        assert!(r["intelligence"]["headers"].get("X-Tenant").is_some());
    }

    #[test]
    fn only_a_lone_secret_reference_survives_redaction() {
        // A reference NAMES a credential; a template that merely embeds one
        // carries inline material beside it, so it goes whole.
        assert!(is_bare_secret_ref("{{secret:TOKEN}}"));
        assert!(is_bare_secret_ref("{{secret-file:/run/secrets/tok}}"));
        assert!(!is_bare_secret_ref("Bearer {{secret:TOKEN}}"));
        assert!(!is_bare_secret_ref("{{secret:TOKEN}}-sk-tail"));
        assert!(!is_bare_secret_ref("{{secret:}}"));
        assert!(!is_bare_secret_ref("sk-plain"));
        let r = redact_settings(&json!({"intelligence": {"token": "Bearer {{secret:T}} sk-tail"}}));
        assert_eq!(r["intelligence"]["token"], REDACTED);
    }
}
