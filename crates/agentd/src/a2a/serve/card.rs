// SPDX-License-Identifier: AGPL-3.0-only
//! The agent card, read from the runtime: the well-known route and the
//! provider a2a-rs asks for the extended card.

use std::sync::Arc;

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde_json::{Value, json};

use super::App;
use crate::a2a::Principal;
use crate::a2a::ports;
use crate::runtime::a2a_server::A2aBridge;

/// The agent card, read from the runtime so its skills reflect the workflows
/// that are actually loaded rather than a snapshot taken at boot.
pub(super) struct CardFromRuntime(pub(super) Arc<A2aBridge>);

#[async_trait::async_trait]
impl a2a_rs::services::AgentInfoProvider for CardFromRuntime {
    /// The public card. No wire method reads it — the spec publishes it at
    /// `.well-known` — but the port is a2a-rs's to call, so it answers the
    /// same document the route does.
    async fn get_agent_card(&self) -> Result<a2a_rs::domain::AgentCard, a2a_rs::domain::A2AError> {
        self.card("PublicCard", Principal::anonymous()).await
    }

    /// The authenticated card, scoped to whoever is asking. The caller travels
    /// on the request's scope, because the port takes none.
    async fn get_authenticated_extended_card(
        &self,
    ) -> Result<a2a_rs::domain::AgentCard, a2a_rs::domain::A2AError> {
        self.card("GetExtendedAgentCard", ports::caller()).await
    }
}

impl CardFromRuntime {
    async fn card(
        &self,
        verb: &'static str,
        who: Principal,
    ) -> Result<a2a_rs::domain::AgentCard, a2a_rs::domain::A2AError> {
        let bridge = Arc::clone(&self.0);
        let v = tokio::task::spawn_blocking(move || bridge.call(verb, json!({}), who))
            .await
            .map_err(|e| a2a_rs::domain::A2AError::Internal(e.to_string()))?;
        // The runtime's refusal — no extended card for an anonymous caller,
        // say — is recorded whole, like every port's, so the listener answers
        // with the runtime's code and words rather than a2a-rs's rendering.
        if let Some(e) = ports::error_of(&v) {
            ports::record_error(e);
            return Err(ports::from_error_object(e));
        }
        serde_json::from_value(v).map_err(a2a_rs::domain::A2AError::JsonParse)
    }
}

/// GET on the well-known path: discovery is public, by design.
pub(super) async fn card(State(app): State<Arc<App>>) -> Response {
    let bridge = Arc::clone(&app.bridge);
    let v = tokio::task::spawn_blocking(move || {
        bridge.call("PublicCard", json!({}), Principal::anonymous())
    })
    .await
    .unwrap_or(Value::Null);
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "application/json")],
        serde_json::to_vec(&v).unwrap_or_default(),
    )
        .into_response()
}

/// The browser preflight for the card route. Until the card has a CORS
/// policy of its own it answers as the JSON-RPC endpoint does, from the same
/// origin allowlist.
pub(super) async fn card_preflight(State(app): State<Arc<App>>, headers: HeaderMap) -> Response {
    super::cors::preflight(State(app), headers).await
}
