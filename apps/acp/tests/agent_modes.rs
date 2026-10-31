use agent_client_protocol::schema::v1::{
    LoadSessionRequest, NewSessionRequest, SessionNotification, SessionUpdate,
    SetSessionConfigOptionRequest, SetSessionModeRequest,
};
use anureo_acp::stream_bridge::SessionUpdateEnvelope;
use anureo_acp::{AnureoAcpAgent, ModelOption, ModelProvider};
use std::path::PathBuf;
use std::sync::Arc;

fn make_new_session_request() -> NewSessionRequest {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    NewSessionRequest::new(cwd)
}

struct MockModelProvider;

#[async_trait::async_trait]
impl ModelProvider for MockModelProvider {
    async fn fetch_models(&self) -> Vec<ModelOption> {
        vec![
            ModelOption {
                id: "default".to_string(),
                name: "(default)".to_string(),
                provider: String::new(),
            },
            ModelOption {
                id: "test-model".to_string(),
                name: "Test Model".to_string(),
                provider: "test".to_string(),
            },
        ]
    }
}

fn create_test_agent() -> AnureoAcpAgent {
    AnureoAcpAgent::new()
        .unwrap()
        .with_model_provider(Arc::new(MockModelProvider))
}

fn extract_mode_ids_from_json(json: &serde_json::Value) -> Vec<String> {
    let modes_field = json.get("modes");
    if modes_field.is_none() {
        eprintln!(
            "JSON has no 'modes' field. Keys: {:?}",
            json.as_object().map(|o| o.keys().collect::<Vec<_>>())
        );
        return vec![];
    }
    let modes_obj = modes_field.unwrap();
    if let Some(arr) = modes_obj.get("availableModes").and_then(|m| m.as_array()) {
        return arr
            .iter()
            .filter_map(|entry| entry.get("id").and_then(|id| id.as_str().map(String::from)))
            .collect();
    }
    if let Some(arr) = modes_obj.as_array() {
        return arr
            .iter()
            .filter_map(|entry| entry.get("id").and_then(|id| id.as_str().map(String::from)))
            .collect();
    }
    eprintln!(
        "Unexpected modes structure: {}",
        serde_json::to_string_pretty(modes_obj).unwrap()
    );
    vec![]
}

fn extract_current_mode_id(json: &serde_json::Value) -> Option<String> {
    json.get("modes")
        .and_then(|m| m.get("currentModeId"))
        .and_then(|id| id.as_str().map(String::from))
}

#[tokio::test]
async fn test_new_session_returns_modes_with_ask_and_default() {
    let agent = create_test_agent();
    let response = agent.new_session(make_new_session_request()).await.unwrap();
    let json = serde_json::to_value(&response).unwrap();

    let modes = extract_mode_ids_from_json(&json);
    assert!(
        modes.contains(&"ask".to_string()),
        "modes should contain 'ask', got: {:?}",
        modes
    );
    assert!(
        modes.contains(&"dev".to_string()),
        "modes should contain 'dev', got: {:?}",
        modes
    );
}

#[tokio::test]
async fn test_new_session_default_mode_is_dev() {
    let agent = create_test_agent();
    let response = agent.new_session(make_new_session_request()).await.unwrap();
    let json = serde_json::to_value(&response).unwrap();

    let current = extract_current_mode_id(&json);
    assert_eq!(current, Some("dev".to_string()));
}

#[tokio::test]
async fn test_set_session_mode_and_load_preserves_mode() {
    let agent = create_test_agent();
    let ns_response = agent.new_session(make_new_session_request()).await.unwrap();
    let session_id = ns_response.session_id.clone();

    let set_request: SetSessionModeRequest = serde_json::from_value(serde_json::json!({
        "sessionId": session_id.to_string(),
        "modeId": "dev"
    }))
    .unwrap();
    agent.set_session_mode(set_request).await.unwrap();

    let load_request: LoadSessionRequest = serde_json::from_value(serde_json::json!({
        "sessionId": session_id.to_string(),
        "cwd": std::env::current_dir().unwrap().to_str().unwrap(),
        "mcpServers": []
    }))
    .unwrap();

    let load_response = agent.load_session(load_request).await.unwrap();
    let json = serde_json::to_value(&load_response).unwrap();
    let current = extract_current_mode_id(&json);
    assert_eq!(
        current,
        Some("dev".to_string()),
        "load_session should preserve the mode set via set_session_mode"
    );
}

#[tokio::test]
async fn test_set_session_mode_rejects_unknown_mode() {
    let agent = create_test_agent();
    let ns_response = agent.new_session(make_new_session_request()).await.unwrap();
    let session_id = ns_response.session_id.clone();

    let set_request: SetSessionModeRequest = serde_json::from_value(serde_json::json!({
        "sessionId": session_id.to_string(),
        "modeId": "nonexistent-mode"
    }))
    .unwrap();

    let result = agent.set_session_mode(set_request).await;
    assert!(result.is_err(), "expected error for unknown mode");
}

#[tokio::test]
async fn test_set_session_mode_rejects_unknown_session() {
    let agent = create_test_agent();

    let set_request: SetSessionModeRequest = serde_json::from_value(serde_json::json!({
        "sessionId": "nonexistent-session",
        "modeId": "ask"
    }))
    .unwrap();

    let result = agent.set_session_mode(set_request).await;
    assert!(result.is_err(), "expected error for unknown session");
}

#[tokio::test]
async fn test_load_session_new_entry_defaults_to_dev() {
    let agent = create_test_agent();
    let ns_response = agent.new_session(make_new_session_request()).await.unwrap();
    let session_id = ns_response.session_id.clone();

    let load_request: LoadSessionRequest = serde_json::from_value(serde_json::json!({
        "sessionId": session_id.to_string(),
        "cwd": std::env::current_dir().unwrap().to_str().unwrap(),
        "mcpServers": []
    }))
    .unwrap();
    let load_response = agent.load_session(load_request).await.unwrap();
    let json = serde_json::to_value(&load_response).unwrap();
    let current = extract_current_mode_id(&json);
    assert_eq!(
        current,
        Some("dev".to_string()),
        "load_session new entry should default to dev mode"
    );
}

#[tokio::test]
async fn test_load_session_modes_list_contains_builtins() {
    let agent = create_test_agent();
    let session_id = agent
        .new_session(make_new_session_request())
        .await
        .unwrap()
        .session_id;

    let load_request: LoadSessionRequest = serde_json::from_value(serde_json::json!({
        "sessionId": session_id.to_string(),
        "cwd": std::env::current_dir().unwrap().to_str().unwrap(),
        "mcpServers": []
    }))
    .unwrap();

    let load_response = agent.load_session(load_request).await.unwrap();
    let json = serde_json::to_value(&load_response).unwrap();
    let modes = extract_mode_ids_from_json(&json);

    assert!(
        modes.contains(&"ask".to_string()),
        "load_session modes should contain 'ask', got: {:?}",
        modes
    );
    assert!(
        modes.contains(&"dev".to_string()),
        "load_session modes should contain 'dev', got: {:?}",
        modes
    );
}

#[tokio::test]
async fn test_set_session_config_option_mode_switches_mode_and_returns_mode_first() {
    let agent = create_test_agent();
    let ns_response = agent.new_session(make_new_session_request()).await.unwrap();
    let session_id = ns_response.session_id.clone();

    let set_config_request: SetSessionConfigOptionRequest =
        serde_json::from_value(serde_json::json!({
            "sessionId": session_id.to_string(),
            "configId": "mode",
            "value": "dev"
        }))
        .unwrap();

    let set_response = agent
        .set_session_config_option(set_config_request)
        .await
        .unwrap();
    let response_json = serde_json::to_value(&set_response).unwrap();
    let options = response_json["configOptions"]
        .as_array()
        .expect("configOptions should be an array");
    assert!(
        options.len() >= 2,
        "configOptions should include mode and model, got: {}",
        response_json
    );
    assert_eq!(options[0]["id"], "mode");
    assert_eq!(options[0]["currentValue"], "dev");
    assert_eq!(options[1]["id"], "model");

    let load_request: LoadSessionRequest = serde_json::from_value(serde_json::json!({
        "sessionId": session_id.to_string(),
        "cwd": std::env::current_dir().unwrap().to_str().unwrap(),
        "mcpServers": []
    }))
    .unwrap();
    let load_response = agent.load_session(load_request).await.unwrap();
    let load_json = serde_json::to_value(&load_response).unwrap();
    assert_eq!(extract_current_mode_id(&load_json), Some("dev".to_string()));
}

#[tokio::test]
async fn test_set_session_config_option_mode_accepts_typed_value_payload() {
    let agent = create_test_agent();
    let ns_response = agent.new_session(make_new_session_request()).await.unwrap();
    let session_id = ns_response.session_id.clone();

    let set_config_request: SetSessionConfigOptionRequest =
        serde_json::from_value(serde_json::json!({
            "sessionId": session_id.to_string(),
            "configId": "mode",
            "type": "value_id",
            "value": "dev"
        }))
        .unwrap();

    let set_response = agent
        .set_session_config_option(set_config_request)
        .await
        .unwrap();
    let response_json = serde_json::to_value(&set_response).unwrap();
    let options = response_json["configOptions"]
        .as_array()
        .expect("configOptions should be an array");
    assert!(
        options.len() >= 2,
        "configOptions should include mode and model, got: {}",
        response_json
    );
    assert_eq!(options[0]["id"], "mode");
    assert_eq!(options[0]["currentValue"], "dev");
    assert_eq!(options[1]["id"], "model");

    let load_request: LoadSessionRequest = serde_json::from_value(serde_json::json!({
        "sessionId": session_id.to_string(),
        "cwd": std::env::current_dir().unwrap().to_str().unwrap(),
        "mcpServers": []
    }))
    .unwrap();
    let load_response = agent.load_session(load_request).await.unwrap();
    let load_json = serde_json::to_value(&load_response).unwrap();
    assert_eq!(extract_current_mode_id(&load_json), Some("dev".to_string()));
}

#[tokio::test]
async fn test_set_session_config_option_broadcasts_config_option_update() {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<SessionUpdateEnvelope>();
    let agent = AnureoAcpAgent::with_session_update_tx(tx)
        .unwrap()
        .with_model_provider(Arc::new(MockModelProvider));
    let ns_response = agent.new_session(make_new_session_request()).await.unwrap();
    let session_id = ns_response.session_id.clone();

    let set_config_request: SetSessionConfigOptionRequest =
        serde_json::from_value(serde_json::json!({
            "sessionId": session_id.to_string(),
            "configId": "effort",
            "value": "high"
        }))
        .unwrap();
    agent
        .set_session_config_option(set_config_request)
        .await
        .unwrap();

    // set 响应之外，还应向连接广播 ACP 标准的 session/update → config_option_update，
    // 让同会话的其他端（anureo app / cowork）无需轮询即可刷新档位与当前值。
    let deadline = tokio::time::Duration::from_secs(2);
    let mut broadcast: Option<SessionNotification> = None;
    while broadcast.is_none() {
        let envelope = tokio::time::timeout(deadline, rx.recv())
            .await
            .expect("timed out waiting for config_option_update")
            .expect("update channel closed");
        match envelope {
            SessionUpdateEnvelope::Session(notification) => {
                if matches!(notification.update, SessionUpdate::ConfigOptionUpdate(_)) {
                    broadcast = Some(notification);
                }
            }
            SessionUpdateEnvelope::SessionReplay { .. } => {}
            SessionUpdateEnvelope::HistoryBatch { .. } => {}
        }
    }
    let notification = broadcast.unwrap();
    assert_eq!(notification.session_id, session_id);
    let SessionUpdate::ConfigOptionUpdate(update) = notification.update else {
        unreachable!("filtered to ConfigOptionUpdate above");
    };
    let update_json = serde_json::to_value(&update).unwrap();
    let options = update_json["configOptions"]
        .as_array()
        .expect("configOptions should be an array");
    let effort = options
        .iter()
        .find(|option| option["id"] == "effort")
        .expect("effort option should be part of the broadcast");
    assert_eq!(effort["currentValue"], "high");
}
