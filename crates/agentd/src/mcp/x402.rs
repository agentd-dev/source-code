// SPDX-License-Identifier: AGPL-3.0-only
//! Where x402 rides on MCP (the x402 v2 MCP transport; RFC 0049 §7.1): a
//! `tools/call` result with `isError: true` whose `structuredContent` — or,
//! for a server that sends none, `content[0].text` parsed as JSON — is a
//! `PaymentRequired`; the paid call's `PaymentPayload` in
//! `params._meta["x402/payment"]`; and the settlement in the result's
//! `_meta["x402/payment-response"]`.
//!
//! Only where things are, read as JSON: always compiled, so a build without
//! the `x402` feature still tells a step that a tool asks to be paid. What
//! the objects say, and whether to pay, is `crate::payments`' and
//! `runtime::payments`'.

use serde_json::Value;

/// The `_meta` key a paid call carries its `PaymentPayload` under.
pub const PAYMENT: &str = "x402/payment";
/// The `_meta` key a result carries its `SettlementResponse` under.
pub const PAYMENT_RESPONSE: &str = "x402/payment-response";

/// The resource a tool's payment request names: `mcp://tool/<name>`, as the
/// transport's examples and the x402 SDKs' MCP servers write it.
pub fn tool_resource(tool: &str) -> String {
    format!("mcp://tool/{tool}")
}

/// Whether `v` looks like a `PaymentRequired`: an object with `x402Version`
/// and `accepts` (the transport's client requirements).
fn looks_required(v: &Value) -> bool {
    v.get("x402Version").is_some() && v.get("accepts").is_some()
}

/// The `PaymentRequired` an error result carries, as sent: its
/// `structuredContent` when that looks like one, else its first content
/// part's text parsed as JSON when that does. `None` for a result that is
/// not an error, or whose error is anything else.
pub fn required_of(res: &::mcp::wire::CallToolResult) -> Option<Value> {
    if !res.is_error() {
        return None;
    }
    if let Some(sc) = res
        .structured_content
        .as_ref()
        .filter(|v| looks_required(v))
    {
        return Some(sc.clone());
    }
    res.content
        .first()
        .and_then(|p| p.get("text"))
        .and_then(Value::as_str)
        .and_then(|t| serde_json::from_str::<Value>(t).ok())
        .filter(looks_required)
}

/// The settlement a result's `_meta` carries, as sent.
pub fn response_of(res: &::mcp::wire::CallToolResult) -> Option<&Value> {
    res.meta.as_ref()?.get(PAYMENT_RESPONSE)
}

/// What a step that may not pay fails with when a tool asks to be paid.
pub fn unpaid_error(tool: &str) -> String {
    format!(
        "{tool}: the tool asks to be paid (x402), and this step has no payment field — an mcp.tool step pays only with one, within security.payments (RFC 0049)"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn result(v: Value) -> ::mcp::wire::CallToolResult {
        serde_json::from_value(v).unwrap()
    }

    /// The transport's own payment-required example reads from
    /// `structuredContent`; a server that sends only text is read from its
    /// first part; anything else — a success, another error — is not one.
    #[test]
    fn a_payment_request_is_read_where_the_transport_puts_it() {
        let required = json!({"x402Version": 2, "error": "Payment required to access this resource",
            "resource": {"url": "mcp://tool/financial_analysis"},
            "accepts": [{"scheme": "exact", "network": "eip155:84532", "amount": "10000"}]});
        let both = result(json!({"isError": true, "structuredContent": required,
            "content": [{"type": "text", "text": required.to_string()}]}));
        assert_eq!(required_of(&both), Some(required.clone()));
        let text_only = result(json!({"isError": true,
            "content": [{"type": "text", "text": required.to_string()}]}));
        assert_eq!(required_of(&text_only), Some(required.clone()));
        for not_one in [
            json!({"content": [{"type": "text", "text": required.to_string()}]}),
            json!({"isError": true, "content": [{"type": "text", "text": "no such row"}]}),
            json!({"isError": true, "structuredContent": {"error": "x"}}),
            json!({"isError": true, "content": [{"type": "text", "text": "{\"x402Version\": 2}"}]}),
        ] {
            assert_eq!(required_of(&result(not_one.clone())), None, "{not_one}");
        }
    }

    /// The settlement is the result's `_meta`, kept on the wire type: a
    /// result read and written back still carries it.
    #[test]
    fn a_settlement_rides_the_results_meta() {
        let res = result(json!({"content": [{"type": "text", "text": "done"}],
            "_meta": {"x402/payment-response": {"success": true, "transaction": "0xab",
                      "network": "eip155:84532"}}}));
        assert_eq!(response_of(&res).unwrap()["transaction"], "0xab");
        let back: ::mcp::wire::CallToolResult =
            serde_json::from_value(serde_json::to_value(&res).unwrap()).unwrap();
        assert!(response_of(&back).is_some());
        assert!(response_of(&result(json!({"content": []}))).is_none());
        assert_eq!(tool_resource("get_weather"), "mcp://tool/get_weather");
    }
}
