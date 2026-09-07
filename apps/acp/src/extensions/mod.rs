//! `_anureo.dev/*` extension framework.
//!
//! Provides registry, dispatch, and shared utilities for all custom
//! JSON-RPC extension domains.

pub mod auth;
pub mod boundary;
pub mod capability;
pub mod pagination;
pub mod progress;
pub mod register;

// ── Domain modules (stubs — implementation added in later phases) ──────
pub mod agent_profile;
pub mod auto_review;
pub mod client_auth;
pub mod command;
pub mod config_entity;
pub mod config_store;
pub mod connection;
pub mod diagnostics;
pub mod dictation;
pub mod files;
pub mod git;
pub mod github;
pub mod global;
pub mod goal;
pub mod mcp;
pub mod model;
pub mod multi_run;
pub mod notification;
pub mod pairing;
pub mod plugin;
pub mod preview;
pub mod project;
pub mod provider;
pub mod question;
pub mod quota_provider;
pub mod relay;
pub mod scheduled_task;
pub mod session_assist;
pub mod session_auth;
pub mod session_folder;
pub mod session_history;
pub mod session_list;
pub mod session_metrics;
pub mod settings;
pub mod skills;
pub mod small_model;
pub mod snippet;
pub mod terminal_ext;
pub mod tts;
pub mod tunnel;
pub mod worktree;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;
use tokio::sync::RwLock;

use crate::client_capabilities::ClientCapabilitiesInfo;

// ---------------------------------------------------------------------------
// Extension prefix
// ---------------------------------------------------------------------------

pub const EXTENSION_PREFIX: &str = "_anureo.dev/";

// ---------------------------------------------------------------------------
// Error
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub struct ExtensionError {
    pub code: i32,
    pub message: String,
    pub data: Option<Value>,
}

impl ExtensionError {
    pub fn method_not_found() -> Self {
        Self {
            code: -32601,
            message: "method_not_found".into(),
            data: None,
        }
    }

    pub fn invalid_params(msg: impl Into<String>) -> Self {
        Self {
            code: -32602,
            message: "invalid_params".into(),
            data: Some(Value::String(msg.into())),
        }
    }

    pub fn capability_not_supported(domain: &str) -> Self {
        Self {
            code: -32001,
            message: "capability_not_supported".into(),
            data: Some(Value::String(format!("domain '{domain}' is not available"))),
        }
    }

    pub fn forbidden(msg: impl Into<String>) -> Self {
        Self {
            code: -32002,
            message: "forbidden".into(),
            data: Some(Value::String(msg.into())),
        }
    }

    pub fn not_found(msg: impl Into<String>) -> Self {
        Self {
            code: -32003,
            message: "not_found".into(),
            data: Some(Value::String(msg.into())),
        }
    }

    pub fn conflict(msg: impl Into<String>) -> Self {
        Self {
            code: -32005,
            message: "conflict".into(),
            data: Some(Value::String(msg.into())),
        }
    }

    pub fn directory_boundary_violation(path: &str) -> Self {
        Self {
            code: -32007,
            message: "directory_boundary_violation".into(),
            data: Some(Value::String(format!(
                "path '{path}' is outside the working directory"
            ))),
        }
    }

    pub fn to_json(&self, id: &Value) -> Value {
        let mut error = serde_json::json!({
            "code": self.code,
            "message": self.message,
        });
        if let Some(data) = &self.data {
            error["data"] = data.clone();
        }
        serde_json::json!({ "jsonrpc": "2.0", "id": id, "error": error })
    }
}

impl std::fmt::Display for ExtensionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "[{}] {}", self.code, self.message)
    }
}

impl std::error::Error for ExtensionError {}

// ---------------------------------------------------------------------------
// Context
// ---------------------------------------------------------------------------

pub struct ExtensionContext {
    pub session_id: Option<String>,
    pub principal: String,
    pub connection_id: String,
    pub working_directory: Option<PathBuf>,
    pub client_capabilities: ClientCapabilitiesInfo,
}

// ---------------------------------------------------------------------------
// Handler trait
// ---------------------------------------------------------------------------

#[async_trait]
pub trait ExtensionHandler: Send + Sync {
    async fn handle(
        &self,
        method: &str,
        params: Value,
        ctx: &ExtensionContext,
    ) -> Result<Value, ExtensionError>;

    fn capabilities(&self) -> Value;
}

// ---------------------------------------------------------------------------
// Registry
// ---------------------------------------------------------------------------

pub struct ExtensionRegistry {
    handlers: HashMap<String, Arc<dyn ExtensionHandler>>,
    /// 中立方法全名 → 规范扩展方法全名（如 `_session/goal` →
    /// `_anureo.dev/goal/session_control`），见 [`Self::register_alias`]。
    aliases: HashMap<String, String>,
    capabilities: RwLock<Value>,
}

impl ExtensionRegistry {
    pub fn new() -> Self {
        Self {
            handlers: HashMap::new(),
            aliases: HashMap::new(),
            capabilities: RwLock::new(Value::Object(Default::default())),
        }
    }

    pub fn register(&mut self, domain: &str, handler: Arc<dyn ExtensionHandler>) {
        let _caps = handler.capabilities();
        tracing::info!(domain, "Registered extension handler");
        self.handlers.insert(domain.to_string(), handler);
    }

    /// Register a neutral (non-`_anureo.dev/`) method alias that routes to a
    /// canonical extension method (P8：中立 goal 扩展 `_session/goal` →
    /// `_anureo.dev/goal/session_control`）。Alias 不出现在能力快照中
    /// （中立面经 initialize `_meta.goal` 单独广播）。
    pub fn register_alias(&mut self, alias: &str, canonical: &str) {
        tracing::info!(alias, canonical, "Registered extension method alias");
        self.aliases.insert(alias.to_string(), canonical.to_string());
    }

    pub fn build_capability_snapshot(&self) -> Value {
        let mut snapshot = serde_json::Map::new();
        for (domain, handler) in &self.handlers {
            snapshot.insert(domain.clone(), handler.capabilities());
        }
        Value::Object(snapshot)
    }

    pub async fn refresh_capabilities(&self) {
        let snapshot = self.build_capability_snapshot();
        *self.capabilities.write().await = snapshot;
    }

    pub async fn capability_snapshot(&self) -> Value {
        self.capabilities.read().await.clone()
    }

    pub fn has_domain(&self, domain: &str) -> bool {
        self.handlers.contains_key(domain)
    }

    pub async fn dispatch(
        &self,
        method: &str,
        params: Value,
        ctx: &ExtensionContext,
    ) -> Result<Value, ExtensionError> {
        // 中立别名（如 `_session/goal`）先解析为规范扩展方法再走前缀路由。
        let canonical;
        let method = match self.aliases.get(method) {
            Some(target) => {
                canonical = target.clone();
                canonical.as_str()
            }
            None => method,
        };
        let stripped = method
            .strip_prefix(EXTENSION_PREFIX)
            .ok_or_else(|| ExtensionError {
                code: -32601,
                message: "method_not_found".into(),
                data: Some(Value::String(format!(
                    "method '{method}' does not start with '{EXTENSION_PREFIX}'"
                ))),
            })?;

        let (domain, sub_method) = stripped.split_once('/').unwrap_or((stripped, ""));

        if domain.is_empty() || sub_method.is_empty() {
            return Err(ExtensionError::method_not_found());
        }

        let handler = self
            .handlers
            .get(domain)
            .ok_or_else(|| ExtensionError::capability_not_supported(domain))?;

        handler.handle(sub_method, params, ctx).await
    }
}

impl Default for ExtensionRegistry {
    fn default() -> Self {
        Self::new()
    }
}

pub fn is_extension_message(line: &str) -> bool {
    line.contains(EXTENSION_PREFIX)
}

pub fn extract_method(line: &str) -> Option<String> {
    let value: Value = serde_json::from_str(line).ok()?;
    value
        .get("method")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
}

pub fn extract_id(line: &str) -> Option<Value> {
    let value: Value = serde_json::from_str(line).ok()?;
    value.get("id").cloned()
}
