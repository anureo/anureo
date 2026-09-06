//! Injectable prompt execution boundary used by black-box ACP tests.

use std::sync::Arc;

use agent_client_protocol::schema::v1::{PromptRequest, PromptResponse};

use crate::client_capabilities::ClientCapabilitiesInfo;
use crate::notification_router::NotificationRouter;
use crate::tools::ClientBridgeTrait;
use crate::AnureoAcpAgent;

#[async_trait::async_trait]
pub trait AcpPromptExecutor: Send + Sync {
    async fn execute(
        &self,
        agent: &AnureoAcpAgent,
        router: &NotificationRouter,
        request: PromptRequest,
        capabilities: ClientCapabilitiesInfo,
        bridge: Arc<dyn ClientBridgeTrait>,
    ) -> agent_client_protocol::Result<PromptResponse>;
}

#[derive(Debug, Default)]
pub struct AnureoPromptExecutor;

#[async_trait::async_trait]
impl AcpPromptExecutor for AnureoPromptExecutor {
    async fn execute(
        &self,
        agent: &AnureoAcpAgent,
        _router: &NotificationRouter,
        request: PromptRequest,
        capabilities: ClientCapabilitiesInfo,
        bridge: Arc<dyn ClientBridgeTrait>,
    ) -> agent_client_protocol::Result<PromptResponse> {
        agent
            .prompt_with_capabilities(request, capabilities, bridge)
            .await
    }
}

#[cfg(feature = "test-support")]
#[derive(Debug, Default)]
pub struct DeterministicPromptExecutor;

#[cfg(feature = "test-support")]
#[async_trait::async_trait]
impl AcpPromptExecutor for DeterministicPromptExecutor {
    async fn execute(
        &self,
        agent: &AnureoAcpAgent,
        router: &NotificationRouter,
        request: PromptRequest,
        _capabilities: ClientCapabilitiesInfo,
        _bridge: Arc<dyn ClientBridgeTrait>,
    ) -> agent_client_protocol::Result<PromptResponse> {
        use agent_client_protocol::schema::v1::{
            ContentBlock, ContentChunk, SessionNotification, SessionUpdate, StopReason, TextContent,
        };

        let session_id = crate::session::SessionId::new(request.session_id.to_string());
        let cancellation = agent.sessions().begin_prompt(&session_id).ok_or_else(|| {
            agent_client_protocol::Error::new(
                -32010,
                "a prompt is already in progress for this session",
            )
        })?;
        let _guard = crate::session::PromptGuard::new(
            agent.sessions(),
            &session_id,
            cancellation.generation(),
        );

        let serialized = serde_json::to_string(&request.prompt).unwrap_or_default();
        if serialized.contains("SLOW_E2E") {
            tokio::time::sleep(std::time::Duration::from_millis(1_500)).await;
        } else if serialized.contains("SLOW") {
            tokio::time::sleep(std::time::Duration::from_millis(250)).await;
        }
        if cancellation.token().is_cancelled() {
            return Ok(PromptResponse::new(StopReason::Cancelled));
        }

        let text = format!("deterministic:{}", session_id.as_str());
        let notification = SessionNotification::new(
            request.session_id,
            SessionUpdate::AgentMessageChunk(ContentChunk::new(ContentBlock::Text(
                TextContent::new(text),
            ))),
        );
        if let Some(sender) = &agent.session_update_tx {
            sender
                .send(crate::stream_bridge::SessionUpdateEnvelope::Session(
                    notification,
                ))
                .map_err(|error| {
                    agent_client_protocol::Error::internal_error().data(error.to_string())
                })?;
        } else {
            router.send(notification).await.map_err(|error| {
                agent_client_protocol::Error::internal_error().data(error.to_string())
            })?;
        }
        Ok(PromptResponse::new(StopReason::EndTurn))
    }
}
