// SPDX-License-Identifier: AGPL-3.0-only
//! The agent card, read from the runtime: the well-known route and the
//! provider a2a-rs asks for the extended card.

use std::sync::Arc;

use axum::extract::State;
use axum::http::{StatusCode, header};
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
    async fn get_agent_card(&self) -> Result<a2a_rs::domain::AgentCard, a2a_rs::domain::A2AError> {
        self.card("GetAgentCard", Principal::anonymous()).await
    }

    /// The authenticated card, scoped to whoever is asking. The caller travels
    /// on the request's task-local, because the port takes none.
    async fn get_authenticated_extended_card(
        &self,
    ) -> Result<a2a_rs::domain::AgentCard, a2a_rs::domain::A2AError> {
        self.card("GetExtendedAgentCard", ports::caller()).await
    }
}

impl CardFromRuntime {
    async fn card(
        &self,
        method: &'static str,
        who: Principal,
    ) -> Result<a2a_rs::domain::AgentCard, a2a_rs::domain::A2AError> {
        let bridge = Arc::clone(&self.0);
        let v = tokio::task::spawn_blocking(move || bridge.call(method, json!({}), who))
            .await
            .map_err(|e| a2a_rs::domain::A2AError::Internal(e.to_string()))?;
        if let Some(e) = v.get("_error") {
            return Err(a2a_rs::domain::A2AError::UnsupportedOperation(
                e.get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("no extended card")
                    .to_string(),
            ));
        }
        serde_json::from_value(v).map_err(a2a_rs::domain::A2AError::JsonParse)
    }
}

/// GET on either well-known path: discovery is public, by design.
pub(super) async fn card(State(app): State<Arc<App>>) -> Response {
    let bridge = Arc::clone(&app.bridge);
    let v = tokio::task::spawn_blocking(move || {
        bridge.call("GetAgentCard", json!({}), Principal::anonymous())
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
