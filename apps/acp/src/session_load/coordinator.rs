use std::sync::Arc;

use agent_client_protocol::schema::v1::{LoadSessionRequest, LoadSessionResponse};

use crate::connection::AcpConnection;
use crate::runtime::AcpRuntime;
use crate::session::{SessionId as AnureoSessionId, SessionLifecycle};
use crate::session_update_log::{SessionLoadPromptState, SessionReplayMode};

use super::{add_session_load_response_meta, parse_session_load_meta, session_event_notification};

/// Owns the state transition and replay ordering for the standard `session/load` request.
///
/// The wire-level `sessionRecovery` metadata only selects between the ordinary
/// full load and cursor-based delta catch-up; neither mode is a separate ACP
/// lifecycle.
pub(crate) async fn load_session(
    runtime: Arc<AcpRuntime>,
    connection: Arc<AcpConnection>,
    request: LoadSessionRequest,
) -> agent_client_protocol::Result<LoadSessionResponse> {
    let session_id = AnureoSessionId::new(request.session_id.to_string());
    let load_meta = parse_session_load_meta(&request)?;

    if let Some(load_meta) = load_meta.as_ref().filter(|value| value.cursor.is_some()) {
        return load_delta_session(&runtime, &connection, request, session_id, load_meta).await;
    }

    load_full_session(
        &runtime,
        &connection,
        request,
        session_id,
        load_meta.is_some(),
    )
    .await
}

async fn load_delta_session(
    runtime: &AcpRuntime,
    connection: &AcpConnection,
    request: LoadSessionRequest,
    session_id: AnureoSessionId,
    load_meta: &super::metadata::SessionLoadMeta,
) -> agent_client_protocol::Result<LoadSessionResponse> {
    let response = runtime
        .agent
        .load_session_for_owner(request, &connection.principal)
        .await?;
    let prompt_state = current_prompt_state(runtime, &session_id);
    let opened = runtime
        .session_update_log
        .read_after_cursor(
            session_id.clone(),
            connection.id.clone(),
            load_meta.cursor.clone(),
            prompt_state,
        )
        .await
        .map_err(|error| agent_client_protocol::Error::internal_error().data(error.to_string()))?;

    if opened.mode != SessionReplayMode::Delta {
        runtime
            .bindings
            .remove_connection_from_session(&session_id, &connection.id);
        return Err(agent_client_protocol::Error::new(
            -32012,
            "session_load_cursor_reset_required",
        )
        .data(serde_json::json!({
            "reason": opened.reset_reason,
            "streamId": opened.stream_id,
            "throughSeq": opened.through_seq,
            "minReplaySeq": opened.min_replay_seq,
        })));
    }

    if let Err(error) = runtime
        .question_handler
        .rebind_session(session_id.as_str(), &connection.id)
        .await
    {
        runtime
            .bindings
            .remove_connection_from_session(&session_id, &connection.id);
        return Err(agent_client_protocol::Error::internal_error().data(error.to_string()));
    }

    let notifications = opened
        .events
        .iter()
        .map(|event| session_event_notification(&session_id, event))
        .collect::<agent_client_protocol::Result<Vec<_>>>()?;
    runtime
        .notification_router
        .send_and_flush(notifications)
        .await
        .map_err(|error| {
            agent_client_protocol::Error::internal_error()
                .data(format!("failed to flush session recovery: {error}"))
        })?;

    add_session_load_response_meta(
        response,
        "delta",
        &opened.stream_id,
        opened.through_seq,
        opened.prompt_state,
    )
}

async fn load_full_session(
    runtime: &AcpRuntime,
    connection: &AcpConnection,
    request: LoadSessionRequest,
    session_id: AnureoSessionId,
    load_requested: bool,
) -> agent_client_protocol::Result<LoadSessionResponse> {
    if load_requested {
        runtime
            .session_update_log
            .head(&session_id)
            .map_err(|error| {
                agent_client_protocol::Error::internal_error().data(error.to_string())
            })?;
    }

    // A session entry already resident in this process is attached, not
    // restored: nothing is reset and an in-flight prompt keeps running, so the
    // restore lease (and its -32010 busy rejection) only applies to cold
    // entries that would be rebuilt from the checkpoint.
    let live_entry = runtime.agent.sessions().get(&session_id).is_some();

    let previous_lifecycle = if live_entry {
        None
    } else {
        runtime
            .agent
            .sessions()
            .begin_restore(&session_id)
            .map_err(|()| {
                agent_client_protocol::Error::new(
                    -32010,
                    "a prompt is already in progress for this session",
                )
            })?
    };
    runtime
        .bindings
        .add_connection_to_session(&session_id, connection.id.clone());
    connection.note_session(session_id.as_str()).await;

    let result = load_and_flush_full(
        runtime,
        connection,
        request,
        session_id.clone(),
        load_requested,
        live_entry,
    )
    .await;
    if result.is_ok() {
        runtime.record_session_rebind();
    } else {
        if !live_entry {
            runtime.agent.sessions().finish_restore(
                &session_id,
                previous_lifecycle.unwrap_or(SessionLifecycle::Idle),
            );
        }
        runtime
            .bindings
            .remove_connection_from_session(&session_id, &connection.id);
    }
    result
}

async fn load_and_flush_full(
    runtime: &AcpRuntime,
    connection: &AcpConnection,
    request: LoadSessionRequest,
    session_id: AnureoSessionId,
    load_requested: bool,
    live_entry: bool,
) -> agent_client_protocol::Result<LoadSessionResponse> {
    let response = runtime
        .agent
        .load_session_for_owner(request, &connection.principal)
        .await?;
    runtime
        .question_handler
        .rebind_session(session_id.as_str(), &connection.id)
        .await
        .map_err(|error| agent_client_protocol::Error::internal_error().data(error.to_string()))?;
    runtime
        .flush_notifications(&session_id)
        .await
        .map_err(|error| {
            agent_client_protocol::Error::internal_error()
                .data(format!("failed to flush session history: {error}"))
        })?;

    if !load_requested {
        return Ok(response);
    }
    let prompt_state = current_prompt_state(runtime, &session_id);
    if live_entry {
        // Attach replay: stream the retained seq'd event window instead of the
        // checkpoint so mid-turn events are included and the reported cursor
        // stays continuous with the live update stream.
        let opened = runtime
            .session_update_log
            .read_full_stream(session_id.clone(), connection.id.clone(), prompt_state)
            .await
            .map_err(|error| {
                agent_client_protocol::Error::internal_error().data(error.to_string())
            })?;
        let notifications = opened
            .events
            .iter()
            .map(|event| session_event_notification(&session_id, event))
            .collect::<agent_client_protocol::Result<Vec<_>>>()?;
        runtime
            .notification_router
            .send_and_flush(notifications)
            .await
            .map_err(|error| {
                agent_client_protocol::Error::internal_error()
                    .data(format!("failed to flush session recovery: {error}"))
            })?;
        return add_session_load_response_meta(
            response,
            "full",
            &opened.stream_id,
            opened.through_seq,
            opened.prompt_state,
        );
    }
    let baseline = runtime
        .session_update_log
        .head(&session_id)
        .map_err(|error| agent_client_protocol::Error::internal_error().data(error.to_string()))?;
    add_session_load_response_meta(
        response,
        "full",
        &baseline.stream_id,
        baseline.seq,
        prompt_state,
    )
}

fn current_prompt_state(
    runtime: &AcpRuntime,
    session_id: &AnureoSessionId,
) -> SessionLoadPromptState {
    if runtime.agent.sessions().has_active_prompt(session_id) {
        SessionLoadPromptState::Running
    } else {
        SessionLoadPromptState::Idle
    }
}
