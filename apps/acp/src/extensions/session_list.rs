//! `_anureo.dev/session/*` — global session listing + archival.
//!
//! Replaces the legacy Express→anureo `/api/experimental/session`
//! passthrough: the sidebar pulls active and archived sessions from the
//! anureo-owned `acp_sessions` table via canonical `list` (with a compatibility
//! `list-global` projection), and archive/unarchive mutations are persisted
//! server-side and broadcast as `session.updated` global events so every
//! connected client stays in sync.
//!
//! Spec: docs/acp-spec/extensions/37-session-list.md

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock, Weak};
use std::time::{Duration, SystemTime};

use async_trait::async_trait;
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use serde::Deserialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::agent::AnureoAcpAgent;
use crate::extensions::{ExtensionContext, ExtensionError, ExtensionHandler};
use crate::global_events::GlobalEventBus;
#[cfg(test)]
use crate::session_repository::SessionMetadata;
use crate::session_repository::{SessionIndexRecord, SessionTombstone};

pub const DOMAIN: &str = "session";

const DEFAULT_PAGE_LIMIT: usize = 200;
const MAX_PAGE_LIMIT: usize = 1000;

/// Handler for the `session` domain. The agent reference is late-bound
/// (registry registration happens before agent construction).
pub struct SessionListHandler {
    agent: RwLock<Option<Weak<AnureoAcpAgent>>>,
    global_bus: Option<Arc<GlobalEventBus>>,
    snapshots: Arc<Mutex<HashMap<String, Snapshot>>>,
    cursor_secret: [u8; 32],
    clock: Arc<dyn Fn() -> SystemTime + Send + Sync>,
    legacy_alias_calls: Arc<AtomicU64>,
}

const SNAPSHOT_TTL: Duration = Duration::from_secs(5 * 60);
const MAX_SNAPSHOTS_PER_OWNER: usize = 4;
const MAX_SNAPSHOT_BYTES_PER_OWNER: usize = 64 * 1024 * 1024;
const MAX_SNAPSHOT_BYTES_PROCESS: usize = 256 * 1024 * 1024;

struct Snapshot {
    owner_principal: String,
    directory: Option<String>,
    archived: String,
    records: Vec<SessionIndexRecord>,
    snapshot_version: i64,
    created_at: SystemTime,
    last_access_at: SystemTime,
    estimated_bytes: usize,
}

#[derive(Debug, Deserialize, serde::Serialize)]
struct CursorPayload {
    version: u8,
    snapshot_id: String,
    offset: usize,
}

impl SessionListHandler {
    pub fn new() -> Self {
        let mut cursor_secret = [0_u8; 32];
        cursor_secret[..16].copy_from_slice(Uuid::new_v4().as_bytes());
        cursor_secret[16..].copy_from_slice(Uuid::new_v4().as_bytes());
        Self {
            agent: RwLock::new(None),
            global_bus: None,
            snapshots: Arc::new(Mutex::new(HashMap::new())),
            cursor_secret,
            clock: Arc::new(SystemTime::now),
            legacy_alias_calls: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Number of legacy `list-global` calls handled by this runtime.
    /// Canonical `_anureo.dev/session/list` calls are intentionally not
    /// included, so release telemetry can measure alias usage directly.
    pub fn legacy_alias_call_count(&self) -> u64 {
        self.legacy_alias_calls.load(Ordering::Relaxed)
    }

    /// Override the wall clock for deterministic snapshot TTL/quota tests.
    pub fn with_clock<F>(mut self, clock: F) -> Self
    where
        F: Fn() -> SystemTime + Send + Sync + 'static,
    {
        self.clock = Arc::new(clock);
        self
    }

    pub fn with_global_bus(mut self, bus: Arc<GlobalEventBus>) -> Self {
        self.global_bus = Some(bus);
        self
    }

    pub fn bind(&self, agent: &Arc<AnureoAcpAgent>) {
        *self.agent.write().expect("session-list bind poisoned") = Some(Arc::downgrade(agent));
    }

    fn agent(&self) -> Result<Arc<AnureoAcpAgent>, ExtensionError> {
        self.agent
            .read()
            .expect("session-list agent poisoned")
            .as_ref()
            .and_then(Weak::upgrade)
            .ok_or_else(|| ExtensionError {
                code: -32603,
                message: "internal_error".into(),
                data: Some(Value::String("agent not bound".into())),
            })
    }

    async fn handle_list_global(
        &self,
        params: Value,
        ctx: &ExtensionContext,
    ) -> Result<Value, ExtensionError> {
        self.legacy_alias_calls.fetch_add(1, Ordering::Relaxed);
        let parsed: ListGlobalParams = serde_json::from_value(params)
            .map_err(|error| ExtensionError::invalid_params(format!("{error}")))?;
        let canonical = self
            .handle_list(
                json!({
                    "archived": if parsed.archived { "archived" } else { "active" },
                    "directory": parsed.directory,
                    "limit": parsed.limit,
                    "cursor": parsed.cursor,
                }),
                ctx,
            )
            .await?;
        let sessions = canonical["sessions"]
            .as_array()
            .into_iter()
            .flatten()
            .map(to_legacy_list_item)
            .collect::<Vec<_>>();
        Ok(json!({
            "sessions": sessions,
            "nextCursor": canonical["nextCursor"],
            "hasMore": canonical["hasMore"],
        }))
    }

    async fn handle_list(
        &self,
        params: Value,
        ctx: &ExtensionContext,
    ) -> Result<Value, ExtensionError> {
        let parsed: ListParams = serde_json::from_value(params)
            .map_err(|error| ExtensionError::invalid_params(format!("{error}")))?;
        let archived = parsed.archived.as_deref().unwrap_or("all");
        if !matches!(archived, "all" | "active" | "archived") {
            return Err(ExtensionError::invalid_params(
                "archived must be all, active, or archived",
            ));
        }
        let limit = parsed.limit.unwrap_or(DEFAULT_PAGE_LIMIT);
        if !(1..=MAX_PAGE_LIMIT).contains(&limit) {
            return Err(ExtensionError::invalid_params(
                "limit must be between 1 and 1000",
            ));
        }
        let now = (self.clock)();
        let directory = parsed
            .directory
            .map(canonicalize_directory_filter);
        let (snapshot_id, offset, records, snapshot_version) = if let Some(cursor) = parsed.cursor {
            let payload = self.decode_cursor(&cursor)?;
            let mut snapshots = self.snapshots.lock().expect("session snapshots poisoned");
            let snapshot = snapshots
                .get_mut(&payload.snapshot_id)
                .ok_or_else(snapshot_expired)?;
            if now.duration_since(snapshot.created_at).unwrap_or_default() >= SNAPSHOT_TTL
                || snapshot.owner_principal != ctx.principal
                // Subsequent pages are allowed to send only `cursor` and
                // `limit`; omitted filters inherit the immutable snapshot.
                // If a caller explicitly supplies a filter, it must match.
                || directory
                    .as_ref()
                    .is_some_and(|directory| snapshot.directory.as_ref() != Some(directory))
                || parsed
                    .archived
                    .as_ref()
                    .is_some_and(|requested| requested != &snapshot.archived)
                || payload.offset > snapshot.records.len()
            {
                return Err(snapshot_expired());
            }
            snapshot.last_access_at = now;
            (
                payload.snapshot_id,
                payload.offset,
                snapshot.records.clone(),
                snapshot.snapshot_version,
            )
        } else {
            let agent = self.agent()?;
            let (records, snapshot_version) = agent
                .list_index_records_for_owner(&ctx.principal, directory.as_deref(), archived)
                .await
                .map_err(|error| internal_error(error.message))?;
            let snapshot_id = Uuid::new_v4().simple().to_string();
            let snapshot = Snapshot {
                owner_principal: ctx.principal.clone(),
                directory: directory.clone(),
                archived: archived.to_string(),
                records: records.clone(),
                snapshot_version,
                created_at: now,
                last_access_at: now,
                estimated_bytes: estimate_snapshot_bytes(&records),
            };
            let mut snapshots = self.snapshots.lock().expect("session snapshots poisoned");
            snapshots.retain(|_, item| {
                now.duration_since(item.created_at).unwrap_or_default() < SNAPSHOT_TTL
            });
            // Make room for the new immutable snapshot before evaluating byte
            // quotas. Otherwise a full four-snapshot owner can be rejected
            // based on bytes that would immediately be evicted.
            evict_oldest_snapshot_for_owner(&mut snapshots, &ctx.principal);
            let owner_bytes: usize = snapshots
                .values()
                .filter(|item| item.owner_principal == ctx.principal)
                .map(|item| item.estimated_bytes)
                .sum();
            let process_bytes: usize = snapshots.values().map(|item| item.estimated_bytes).sum();
            if owner_bytes.saturating_add(snapshot.estimated_bytes) > MAX_SNAPSHOT_BYTES_PER_OWNER
                || process_bytes.saturating_add(snapshot.estimated_bytes)
                    > MAX_SNAPSHOT_BYTES_PROCESS
            {
                return Err(snapshot_capacity_exceeded());
            }
            snapshots.insert(snapshot_id.clone(), snapshot);
            (snapshot_id, 0, records, snapshot_version)
        };

        let end = (offset + limit).min(records.len());
        let page = &records[offset..end];
        let next_offset = (end < records.len()).then_some(end);
        let next_cursor = next_offset.map(|next| {
            self.encode_cursor(CursorPayload {
                version: 1,
                snapshot_id: snapshot_id.clone(),
                offset: next,
            })
        });
        Ok(json!({
            "sessions": page.iter().map(to_index_item).collect::<Vec<_>>(),
            "nextCursor": next_cursor,
            "hasMore": next_offset.is_some(),
            "snapshotVersion": snapshot_version,
        }))
    }

    fn encode_cursor(&self, payload: CursorPayload) -> String {
        let body = serde_json::to_vec(&payload).expect("cursor payload serializes");
        let signature = hmac_sha256(&self.cursor_secret, &body);
        format!(
            "{}.{}",
            URL_SAFE_NO_PAD.encode(body),
            URL_SAFE_NO_PAD.encode(signature)
        )
    }

    fn decode_cursor(&self, cursor: &str) -> Result<CursorPayload, ExtensionError> {
        let Some((body, signature)) = cursor.split_once('.') else {
            return Err(ExtensionError::invalid_params("invalid_cursor"));
        };
        let body = URL_SAFE_NO_PAD
            .decode(body)
            .map_err(|_| ExtensionError::invalid_params("invalid_cursor"))?;
        let signature = URL_SAFE_NO_PAD
            .decode(signature)
            .map_err(|_| ExtensionError::invalid_params("invalid_cursor"))?;
        if signature.as_slice() != hmac_sha256(&self.cursor_secret, &body) {
            return Err(ExtensionError::invalid_params("invalid_cursor"));
        }
        let payload: CursorPayload = serde_json::from_slice(&body)
            .map_err(|_| ExtensionError::invalid_params("invalid_cursor"))?;
        if payload.version != 1 {
            return Err(ExtensionError::invalid_params("invalid_cursor"));
        }
        Ok(payload)
    }

    async fn handle_archive(
        &self,
        params: Value,
        ctx: &ExtensionContext,
    ) -> Result<Value, ExtensionError> {
        let parsed: ArchiveParams = serde_json::from_value(params)
            .map_err(|error| ExtensionError::invalid_params(format!("{error}")))?;
        if parsed.session_id.is_empty() {
            return Err(ExtensionError::invalid_params("sessionId is required"));
        }

        let agent = self.agent()?;
        let changed = agent
            .archive_session_index_for_owner(&ctx.principal, &parsed.session_id, parsed.archived)
            .await
            .map_err(|error| internal_error(error.message))?
            .ok_or_else(|| ExtensionError::not_found("session not found"))?;
        let canonical = changed
            .iter()
            .find(|record| record.session_id == parsed.session_id)
            .cloned()
            .ok_or_else(|| ExtensionError::not_found("session not found"))?;
        let affected: Vec<_> = changed
            .iter()
            .filter(|record| record.session_id != canonical.session_id)
            .map(to_index_item)
            .collect();

        if let Some(bus) = &self.global_bus {
            bus.publish(
                "session",
                "session.updated",
                json!({
                    "info": to_event_info_from_index(&canonical),
                    "affectedSessions": affected.clone(),
                }),
            );
            for ancestor in changed
                .iter()
                .filter(|record| record.session_id != canonical.session_id)
            {
                bus.publish(
                    "session",
                    "session.updated",
                    json!({ "info": to_event_info_from_index(ancestor) }),
                );
            }
        }

        Ok(json!({
            "session": to_index_item(&canonical),
            "affectedSessions": affected,
            "indexVersion": canonical.index_version,
        }))
    }

    async fn handle_update(
        &self,
        params: Value,
        ctx: &ExtensionContext,
    ) -> Result<Value, ExtensionError> {
        let parsed: UpdateParams = serde_json::from_value(params)
            .map_err(|error| ExtensionError::invalid_params(format!("{error}")))?;
        if parsed.session_id.is_empty() {
            return Err(ExtensionError::invalid_params("sessionId is required"));
        }
        if parsed.metadata.is_none() && parsed.title.is_none() {
            return Err(ExtensionError::invalid_params(
                "metadata or title is required",
            ));
        }

        let agent = self.agent()?;
        if let Some(metadata) = parsed.metadata.as_ref() {
            if !metadata.is_object() {
                return Err(ExtensionError::invalid_params("metadata must be an object"));
            }
        }
        if let Some(ref title) = parsed.title {
            if title.trim().is_empty() {
                return Err(ExtensionError::invalid_params("title cannot be empty"));
            }
        }
        let canonical = agent
            .update_session_index_fields_for_owner(
                &ctx.principal,
                &parsed.session_id,
                parsed.title.as_deref(),
                parsed.metadata,
            )
            .await
            .map_err(|error| internal_error(error.message))?
            .ok_or_else(|| ExtensionError::not_found("session not found"))?;
        let metadata = canonical.metadata.clone();
        if let Some(bus) = &self.global_bus {
            bus.publish(
                "session",
                "session.updated",
                json!({
                    "info": to_event_info_from_index(&canonical),
                    "metadata": metadata,
                }),
            );
        }
        Ok(json!({
            "session": to_index_item(&canonical),
            "metadata": metadata,
            "affectedSessions": [],
            "indexVersion": canonical.index_version,
        }))
    }

    async fn handle_delete(
        &self,
        params: Value,
        ctx: &ExtensionContext,
    ) -> Result<Value, ExtensionError> {
        let parsed: DeleteParams = serde_json::from_value(params)
            .map_err(|error| ExtensionError::invalid_params(format!("{error}")))?;
        if parsed.session_id.is_empty() {
            return Err(ExtensionError::invalid_params("sessionId is required"));
        }
        let agent = self.agent()?;
        let existed = agent
            .session_index_record_for_owner(&ctx.principal, &parsed.session_id)
            .await
            .map_err(|error| internal_error(error.message))?
            .is_some();
        let delete_response = agent
            .delete_session_for_owner(
                agent_client_protocol::schema::v1::DeleteSessionRequest::new(
                    agent_client_protocol::schema::v1::SessionId::new(parsed.session_id.clone()),
                ),
                &ctx.principal,
            )
            .await
            .map_err(|error| internal_error(error.message))?;
        let delete_response_json = serde_json::to_value(&delete_response)
            .map_err(|error| internal_error(error.to_string()))?;
        let tombstone = agent
            .session_tombstone_for_owner(&ctx.principal, &parsed.session_id)
            .await
            .map_err(|error| internal_error(error.message))?
            .ok_or_else(|| ExtensionError::not_found("session not found"))?;
        let affected_sessions = delete_response_json
            .get("_meta")
            .and_then(|meta| meta.get("anureo.dev"))
            .and_then(|meta| meta.get("affectedSessions"))
            .cloned()
            .unwrap_or_else(|| json!([]));
        if existed {
            if let Some(bus) = &self.global_bus {
                bus.publish(
                    "session",
                    "session.deleted",
                    json!({
                        "info": to_tombstone_event_info(&tombstone),
                        "sessionID": tombstone.session_id,
                        "tombstone": tombstone_event_payload(&tombstone),
                    }),
                );
                if let Some(ancestors) = delete_response_json
                    .get("_meta")
                    .and_then(|meta| meta.get("anureo.dev"))
                    .and_then(|meta| meta.get("affectedSessions"))
                    .and_then(Value::as_array)
                {
                    for ancestor in ancestors {
                        if let Some(info) = to_event_info_from_wire(ancestor) {
                            bus.publish("session", "session.updated", json!({ "info": info }));
                        }
                    }
                }
            }
        }
        Ok(json!({
            "tombstone": tombstone_event_payload(&tombstone),
            "affectedSessions": affected_sessions,
            "indexVersion": tombstone.index_version,
        }))
    }
}

impl Default for SessionListHandler {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Deserialize)]
struct ListGlobalParams {
    #[serde(default)]
    archived: bool,
    #[serde(default)]
    directory: Option<String>,
    #[serde(default)]
    cursor: Option<String>,
    #[serde(default)]
    limit: Option<usize>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ListParams {
    /// Optional on continuation pages: omitted values inherit the snapshot.
    #[serde(default)]
    archived: Option<String>,
    #[serde(default)]
    directory: Option<String>,
    #[serde(default)]
    limit: Option<usize>,
    #[serde(default)]
    cursor: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ArchiveParams {
    session_id: String,
    #[serde(default = "default_archived")]
    archived: bool,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct UpdateParams {
    session_id: String,
    #[serde(default)]
    metadata: Option<Value>,
    #[serde(default)]
    title: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DeleteParams {
    session_id: String,
}

fn default_archived() -> bool {
    true
}

/// Spec 37: `directory` must match the server-canonicalized cwd stored by
/// `session/new`, so caller paths that resolve differently (macOS `/var` →
/// `/private/var`, Windows `\\?\` verbatim prefix, `.` segments) still match
/// the stored records — same normalization the standard `session/list` uses.
/// A filter pointing at a missing directory keeps verbatim matching: the SQL
/// layer still strips verbatim prefixes and unifies separators.
fn canonicalize_directory_filter(directory: String) -> String {
    std::fs::canonicalize(&directory)
        .map(|path| path.to_string_lossy().into_owned())
        .unwrap_or(directory)
}

fn internal_error(message: impl Into<String>) -> ExtensionError {
    ExtensionError {
        code: -32603,
        message: "internal_error".into(),
        data: Some(Value::String(message.into())),
    }
}

fn snapshot_expired() -> ExtensionError {
    ExtensionError {
        code: -32004,
        message: "snapshot_expired".into(),
        data: None,
    }
}

fn snapshot_capacity_exceeded() -> ExtensionError {
    ExtensionError {
        code: -32005,
        message: "snapshot_capacity_exceeded".into(),
        data: None,
    }
}

fn evict_oldest_snapshot_for_owner(
    snapshots: &mut HashMap<String, Snapshot>,
    owner_principal: &str,
) -> bool {
    if snapshots
        .values()
        .filter(|item| item.owner_principal == owner_principal)
        .count()
        < MAX_SNAPSHOTS_PER_OWNER
    {
        return false;
    }
    let oldest = snapshots
        .iter()
        .filter(|(_, item)| item.owner_principal == owner_principal)
        .min_by_key(|(_, item)| item.last_access_at)
        .map(|(id, _)| id.clone());
    oldest.is_some_and(|id| snapshots.remove(&id).is_some())
}

fn estimate_snapshot_bytes(records: &[SessionIndexRecord]) -> usize {
    // The quota contract counts the compact canonical wire projection, a
    // fixed 64-byte per-record ownership/index overhead, and 256 bytes for
    // the snapshot container itself. `to_index_item` is the same camel-case
    // projection emitted on the wire, so metadata is charged in full.
    records.iter().fold(256usize, |total, record| {
        let serialized = serde_json::to_vec(&to_index_item(record)).map_or(0, |bytes| bytes.len());
        total.saturating_add(serialized).saturating_add(64)
    })
}

fn hmac_sha256(secret: &[u8; 32], payload: &[u8]) -> [u8; 32] {
    let mut inner_key = [0x36_u8; 64];
    let mut outer_key = [0x5c_u8; 64];
    for index in 0..32 {
        inner_key[index] ^= secret[index];
        outer_key[index] ^= secret[index];
    }
    let mut inner = Sha256::new();
    inner.update(inner_key);
    inner.update(payload);
    let inner_digest = inner.finalize();
    let mut outer = Sha256::new();
    outer.update(outer_key);
    outer.update(inner_digest);
    outer.finalize().into()
}

/// Legacy projection adapter. The source is the canonical immutable snapshot;
/// this function must not read the repository or fetch metadata per item.
fn to_legacy_list_item(item: &Value) -> Value {
    json!({
        "sessionId": item["sessionId"],
        "cwd": item["cwd"],
        "title": item["title"].as_str().unwrap_or_default(),
        "createdAt": item["createdAt"],
        "updatedAt": item["activityAt"],
        "archivedAt": item["archivedAt"],
        "metadata": item["metadata"],
    })
}

fn to_index_item(record: &SessionIndexRecord) -> Value {
    json!({
        "sessionId": record.session_id,
        "parentSessionId": record.parent_session_id,
        "cwd": record.cwd.to_string_lossy(),
        "title": record.title.clone().unwrap_or_else(|| {
            format!("Session {}", &record.session_id[..8.min(record.session_id.len())])
        }),
        "metadata": record.metadata,
        "createdAt": record.created_at,
        "activityAt": record.activity_at,
        "treeActivityAt": record.tree_activity_at,
        "stateChangedAt": record.state_changed_at,
        "metadataUpdatedAt": record.metadata_updated_at,
        "archivedAt": record.archived_at,
        "closedAt": record.closed_at,
        "lifecycle": record.lifecycle,
        "revision": record.revision,
        "indexVersion": record.index_version,
    })
}

/// anureo-shaped session info for `session.updated` global events; the FE
/// event reducer requires `id` plus `time` (with `time.archived` for
/// archival transitions).
#[cfg(test)]
fn to_event_info(metadata: &SessionMetadata) -> Value {
    let cwd = metadata.cwd.to_string_lossy();
    let directory = strip_verbatim_prefix(&cwd);
    json!({
        "id": metadata.session_id,
        "title": metadata.title.clone().unwrap_or_default(),
        "parentID": "",
        "project": directory,
        "directory": directory,
        "time": {
            "created": metadata.created_at.as_deref().and_then(rfc3339_to_ms).unwrap_or(0),
            "updated": metadata
                .updated_at
                .as_deref()
                .and_then(rfc3339_to_ms)
                .or_else(|| metadata.created_at.as_deref().and_then(rfc3339_to_ms))
                .unwrap_or(0),
            "archived": metadata.archived_at.as_deref().and_then(rfc3339_to_ms),
        },
        "share": Value::Null,
        "version": "",
        "replay": Value::Null,
    })
}

/// Canonical global event projection. It retains the legacy `id`/`time`
/// fields required by anureo Desk while carrying the complete SessionIndex
/// fields used for freshness, hierarchy, and metadata reconciliation.
pub(crate) fn to_event_info_from_index(record: &SessionIndexRecord) -> Value {
    let cwd = record.cwd.to_string_lossy();
    let directory = strip_verbatim_prefix(&cwd);
    let created = rfc3339_to_ms(&record.created_at).unwrap_or(0);
    let activity = rfc3339_to_ms(&record.activity_at).unwrap_or(created);
    let state_changed = record
        .state_changed_at
        .as_deref()
        .and_then(rfc3339_to_ms)
        .unwrap_or(created);
    let metadata_updated = record
        .metadata_updated_at
        .as_deref()
        .and_then(rfc3339_to_ms)
        .unwrap_or(created);
    let updated = activity.max(state_changed).max(metadata_updated);
    json!({
        "id": record.session_id,
        "sessionId": record.session_id,
        "title": record.title.clone().unwrap_or_else(|| {
            format!("Session {}", &record.session_id[..8.min(record.session_id.len())])
        }),
        "parentID": record.parent_session_id.clone().unwrap_or_default(),
        "parentSessionId": record.parent_session_id,
        "cwd": record.cwd,
        "project": directory,
        "directory": directory,
        "metadata": record.metadata,
        "lifecycle": record.lifecycle,
        "createdAt": record.created_at,
        "archivedAt": record.archived_at,
        "revision": record.revision,
        "indexVersion": record.index_version,
        "activityAt": record.activity_at,
        "treeActivityAt": record.tree_activity_at,
        "stateChangedAt": record.state_changed_at,
        "metadataUpdatedAt": record.metadata_updated_at,
        "closedAt": record.closed_at,
        "time": {
            "created": created,
            "updated": updated,
            "activity": activity,
            "archived": record.archived_at.as_deref().and_then(rfc3339_to_ms),
        },
        "share": Value::Null,
        "version": "",
        "replay": Value::Null,
    })
}

/// Convert a canonical response projection back into the legacy event shape
/// without re-reading SQLite after the mutation transaction.
pub(crate) fn to_event_info_from_wire(record: &Value) -> Option<Value> {
    let session_id = record.get("sessionId")?.as_str()?;
    let cwd = record
        .get("cwd")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let created = record
        .get("createdAt")
        .and_then(Value::as_str)
        .and_then(rfc3339_to_ms)
        .unwrap_or(0);
    let activity = record
        .get("activityAt")
        .and_then(Value::as_str)
        .and_then(rfc3339_to_ms)
        .unwrap_or(created);
    let state_changed = record
        .get("stateChangedAt")
        .and_then(Value::as_str)
        .and_then(rfc3339_to_ms)
        .unwrap_or(created);
    let metadata_updated = record
        .get("metadataUpdatedAt")
        .and_then(Value::as_str)
        .and_then(rfc3339_to_ms)
        .unwrap_or(created);
    Some(json!({
        "id": session_id,
        "sessionId": session_id,
        "title": record.get("title").cloned().unwrap_or(Value::Null),
        "parentID": record.get("parentSessionId").cloned().unwrap_or(Value::Null),
        "parentSessionId": record.get("parentSessionId").cloned().unwrap_or(Value::Null),
        "cwd": record.get("cwd").cloned().unwrap_or(Value::String(cwd.into())),
        "project": strip_verbatim_prefix(cwd),
        "directory": strip_verbatim_prefix(cwd),
        "metadata": record.get("metadata").cloned().unwrap_or_else(|| json!({})),
        "lifecycle": record.get("lifecycle").cloned().unwrap_or(Value::Null),
        "createdAt": record.get("createdAt").cloned().unwrap_or(Value::Null),
        "archivedAt": record.get("archivedAt").cloned().unwrap_or(Value::Null),
        "revision": record.get("revision").cloned().unwrap_or(Value::Null),
        "indexVersion": record.get("indexVersion").cloned().unwrap_or(Value::Null),
        "activityAt": record.get("activityAt").cloned().unwrap_or(Value::Null),
        "treeActivityAt": record.get("treeActivityAt").cloned().unwrap_or(Value::Null),
        "stateChangedAt": record.get("stateChangedAt").cloned().unwrap_or(Value::Null),
        "metadataUpdatedAt": record.get("metadataUpdatedAt").cloned().unwrap_or(Value::Null),
        "closedAt": record.get("closedAt").cloned().unwrap_or(Value::Null),
        "time": {
            "created": created,
            "updated": activity.max(state_changed).max(metadata_updated),
            "activity": activity,
            "archived": record.get("archivedAt").and_then(Value::as_str).and_then(rfc3339_to_ms),
        },
        "share": Value::Null,
        "version": "",
        "replay": Value::Null,
    }))
}

pub(crate) fn tombstone_event_payload(tombstone: &SessionTombstone) -> Value {
    json!({
        "sessionId": tombstone.session_id,
        "cwd": tombstone.cwd.to_string_lossy(),
        "parentSessionId": tombstone.parent_session_id,
        "revision": tombstone.revision,
        "indexVersion": tombstone.index_version,
        "deletedAt": tombstone.deleted_at,
        "deleted": true,
    })
}

pub(crate) fn to_tombstone_event_info(tombstone: &SessionTombstone) -> Value {
    let directory = strip_verbatim_prefix(&tombstone.cwd.to_string_lossy());
    let deleted_at = rfc3339_to_ms(&tombstone.deleted_at).unwrap_or(0);
    json!({
        "id": tombstone.session_id,
        "sessionId": tombstone.session_id,
        "parentID": tombstone.parent_session_id.clone().unwrap_or_default(),
        "parentSessionId": tombstone.parent_session_id,
        "directory": directory,
        "project": directory,
        "revision": tombstone.revision,
        "indexVersion": tombstone.index_version,
        "deleted": true,
        "time": { "created": deleted_at, "updated": deleted_at, "archived": Value::Null },
    })
}

fn strip_verbatim_prefix(path: &str) -> String {
    path.strip_prefix(r"\\?\")
        .unwrap_or(path)
        .replace('\\', "/")
}

fn rfc3339_to_ms(value: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(value)
        .ok()
        .map(|dt| dt.timestamp_millis())
}

#[async_trait]
impl ExtensionHandler for SessionListHandler {
    async fn handle(
        &self,
        method: &str,
        params: Value,
        ctx: &ExtensionContext,
    ) -> Result<Value, ExtensionError> {
        match method {
            "list" => self.handle_list(params, ctx).await,
            "list-global" => self.handle_list_global(params, ctx).await,
            "archive" => self.handle_archive(params, ctx).await,
            "update" => self.handle_update(params, ctx).await,
            "delete" => self.handle_delete(params, ctx).await,
            _ => Err(ExtensionError::method_not_found()),
        }
    }

    fn capabilities(&self) -> Value {
        json!({ "methods": ["list", "list-global", "archive", "update", "delete"] })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_client_protocol::schema::v1::NewSessionRequest;
    use std::sync::Arc;

    use crate::agent::AnureoAcpAgent;
    use crate::client_capabilities::ClientCapabilitiesInfo;
    use crate::session_repository::SessionMetadata;

    #[test]
    fn event_info_maps_anureo_shape() {
        let metadata = SessionMetadata {
            session_id: "s-1".into(),
            thread_id: "t-1".into(),
            owner_principal: "local-anonymous".into(),
            cwd: r"C:\repo".into(),
            lifecycle: "idle".into(),
            title: Some("Title".into()),
            updated_at: Some("2026-08-18T10:00:00+00:00".into()),
            created_at: Some("2026-08-18T09:00:00+00:00".into()),
            archived_at: Some("2026-08-18T11:00:00+00:00".into()),
        };
        let info = to_event_info(&metadata);
        assert_eq!(info["id"], "s-1");
        assert_eq!(info["directory"], "C:/repo");
        assert_eq!(info["time"]["created"], 1_787_043_600_000i64);
        assert_eq!(info["time"]["updated"], 1_787_047_200_000i64);
        assert_eq!(info["time"]["archived"], 1_787_050_800_000i64);
    }

    #[test]
    fn unarchived_info_has_null_archived() {
        let metadata = SessionMetadata {
            archived_at: None,
            created_at: None,
            updated_at: None,
            ..base_metadata()
        };
        let info = to_event_info(&metadata);
        assert!(info["time"]["archived"].is_null());
    }

    #[test]
    fn cursor_signature_round_trip_and_tamper_rejection() {
        let handler = SessionListHandler::new();
        let cursor = handler.encode_cursor(CursorPayload {
            version: 1,
            snapshot_id: "snapshot-1".into(),
            offset: 200,
        });
        let decoded = handler.decode_cursor(&cursor).unwrap();
        assert_eq!(decoded.snapshot_id, "snapshot-1");
        assert_eq!(decoded.offset, 200);

        let mut tampered = cursor.into_bytes();
        let last = tampered.len() - 1;
        tampered[last] = if tampered[last] == b'A' { b'B' } else { b'A' };
        let tampered = String::from_utf8(tampered).unwrap();
        assert!(handler.decode_cursor(&tampered).is_err());
    }

    #[test]
    fn canonical_event_projection_retains_legacy_and_index_fields() {
        let record = SessionIndexRecord {
            session_id: "s-1".into(),
            parent_session_id: Some("root".into()),
            owner_principal: "owner".into(),
            cwd: r"C:\repo".into(),
            lifecycle: "idle".into(),
            title: Some("Title".into()),
            metadata: json!({"kind":"review"}),
            created_at: "2026-08-18T09:00:00+00:00".into(),
            activity_at: "2026-08-18T10:00:00+00:00".into(),
            tree_activity_at: "2026-08-18T10:00:00+00:00".into(),
            state_changed_at: None,
            metadata_updated_at: None,
            archived_at: None,
            closed_at: None,
            revision: 3,
            index_version: 7,
        };
        let info = to_event_info_from_index(&record);
        assert_eq!(info["id"], "s-1");
        assert_eq!(info["parentID"], "root");
        assert_eq!(info["revision"], 3);
        assert_eq!(info["indexVersion"], 7);
        assert_eq!(info["metadata"]["kind"], "review");
    }

    #[test]
    fn snapshot_eviction_happens_before_new_quota_admission() {
        let mut snapshots = HashMap::new();
        for index in 0..MAX_SNAPSHOTS_PER_OWNER {
            snapshots.insert(
                format!("owner-{index}"),
                Snapshot {
                    owner_principal: "owner".into(),
                    directory: None,
                    archived: "all".into(),
                    records: Vec::new(),
                    snapshot_version: 1,
                    created_at: SystemTime::UNIX_EPOCH,
                    last_access_at: SystemTime::UNIX_EPOCH + Duration::from_secs(index as u64),
                    estimated_bytes: 1,
                },
            );
        }
        snapshots.insert(
            "other".into(),
            Snapshot {
                owner_principal: "other".into(),
                directory: None,
                archived: "all".into(),
                records: Vec::new(),
                snapshot_version: 1,
                created_at: SystemTime::UNIX_EPOCH,
                last_access_at: SystemTime::UNIX_EPOCH,
                estimated_bytes: 1,
            },
        );

        assert!(evict_oldest_snapshot_for_owner(&mut snapshots, "owner"));
        assert!(!snapshots.contains_key("owner-0"));
        assert_eq!(snapshots.len(), MAX_SNAPSHOTS_PER_OWNER);
        assert!(snapshots.contains_key("other"));
    }

    #[test]
    fn snapshot_byte_accounting_matches_wire_projection_contract() {
        let mut record = SessionIndexRecord {
            session_id: "session".into(),
            parent_session_id: None,
            owner_principal: "owner".into(),
            cwd: std::path::PathBuf::from("C:/repo"),
            lifecycle: "idle".into(),
            title: Some("Title".into()),
            metadata: json!({"payload": "x"}),
            created_at: "2026-08-22T00:00:00.000000Z".into(),
            activity_at: "2026-08-22T00:00:00.000000Z".into(),
            tree_activity_at: "2026-08-22T00:00:00.000000Z".into(),
            state_changed_at: None,
            metadata_updated_at: None,
            archived_at: None,
            closed_at: None,
            revision: 1,
            index_version: 1,
        };
        let wire_bytes = serde_json::to_vec(&to_index_item(&record)).unwrap().len();
        assert_eq!(estimate_snapshot_bytes(&[]), 256);
        assert_eq!(
            estimate_snapshot_bytes(&[record.clone()]),
            256 + wire_bytes + 64
        );

        record.metadata = json!({"payload": "x".repeat(4096)});
        assert!(estimate_snapshot_bytes(&[record]) > 256 + wire_bytes + 64);
    }

    #[test]
    fn capabilities_advertise_canonical_list_and_legacy_alias_during_migration() {
        let capabilities = SessionListHandler::new().capabilities();
        let methods = capabilities["methods"].as_array().expect("methods array");
        let methods: Vec<_> = methods.iter().filter_map(Value::as_str).collect();
        assert!(methods.contains(&"list"));
        assert!(methods.contains(&"list-global"));
        assert!(methods.contains(&"archive"));
        assert!(methods.contains(&"update"));
        assert!(methods.contains(&"delete"));
    }

    #[tokio::test]
    async fn legacy_list_global_returns_legacy_projection_from_shared_store() {
        let temp = tempfile::tempdir().unwrap();
        let agent =
            Arc::new(AnureoAcpAgent::new_with_db_path(temp.path().join("memory.db")).expect("agent"));
        let response = agent
            .new_session(
                NewSessionRequest::new(temp.path()).meta(
                    json!({
                        "anureo.dev": {
                            "title": "Legacy visible",
                            "metadata": { "source": "shared-index" }
                        }
                    })
                    .as_object()
                    .cloned()
                    .expect("session meta object"),
                ),
            )
            .await
            .expect("session");
        let session_id = response.session_id.to_string();

        let handler = SessionListHandler::new();
        handler.bind(&agent);
        let ctx = ExtensionContext {
            session_id: None,
            principal: "local-anonymous".into(),
            connection_id: "test-connection".into(),
            working_directory: Some(temp.path().to_path_buf()),
            client_capabilities: ClientCapabilitiesInfo::default(),
        };
        let listed = handler
            .handle(
                "list-global",
                json!({ "archived": false, "directory": temp.path(), "limit": 10 }),
                &ctx,
            )
            .await
            .expect("legacy list");
        let item = listed["sessions"]
            .as_array()
            .and_then(|items| items.iter().find(|item| item["sessionId"] == session_id))
            .expect("legacy session item");
        assert_eq!(item["title"], "Legacy visible");
        assert_eq!(item["metadata"]["source"], "shared-index");
        assert!(
            item.get("revision").is_none(),
            "legacy projection must not leak index fields"
        );
        assert_eq!(listed["hasMore"], false);
        assert_eq!(handler.legacy_alias_call_count(), 1);
    }

    #[tokio::test]
    async fn legacy_alias_paginates_with_canonical_snapshot_cursor() {
        let temp = tempfile::tempdir().unwrap();
        let agent =
            Arc::new(AnureoAcpAgent::new_with_db_path(temp.path().join("memory.db")).expect("agent"));
        for title in ["First", "Second"] {
            agent
                .new_session(
                    NewSessionRequest::new(temp.path()).meta(
                        json!({
                            "anureo.dev": {
                                "title": title,
                                "metadata": { "source": title }
                            }
                        })
                        .as_object()
                        .cloned()
                        .expect("session meta object"),
                    ),
                )
                .await
                .expect("session");
        }

        let handler = SessionListHandler::new();
        handler.bind(&agent);
        let ctx = ExtensionContext {
            session_id: None,
            principal: "local-anonymous".into(),
            connection_id: "test-connection".into(),
            working_directory: Some(temp.path().to_path_buf()),
            client_capabilities: ClientCapabilitiesInfo::default(),
        };
        let first = handler
            .handle(
                "list-global",
                json!({ "archived": false, "directory": temp.path(), "limit": 1 }),
                &ctx,
            )
            .await
            .expect("first legacy page");
        assert_eq!(first["sessions"].as_array().unwrap().len(), 1);
        let cursor = first["nextCursor"].as_str().expect("opaque cursor");
        assert!(
            cursor.contains('.'),
            "alias must return canonical opaque cursor"
        );

        let second = handler
            .handle(
                "list-global",
                json!({
                    "archived": false,
                    "directory": temp.path(),
                    "limit": 1,
                    "cursor": cursor,
                }),
                &ctx,
            )
            .await
            .expect("second legacy page");
        assert_eq!(handler.legacy_alias_call_count(), 2);
        assert_eq!(second["sessions"].as_array().unwrap().len(), 1);
        assert_ne!(
            first["sessions"][0]["sessionId"],
            second["sessions"][0]["sessionId"]
        );
        assert!(second["sessions"][0].get("revision").is_none());
        assert_eq!(second["hasMore"], false);
    }

    #[tokio::test]
    async fn archive_handler_returns_canonical_target_and_changed_ancestor() {
        let temp = tempfile::tempdir().unwrap();
        let agent =
            Arc::new(AnureoAcpAgent::new_with_db_path(temp.path().join("memory.db")).expect("agent"));
        let root = agent
            .new_session(NewSessionRequest::new(temp.path()))
            .await
            .expect("root session")
            .session_id
            .to_string();
        let child_response = agent
            .new_session(
                NewSessionRequest::new(temp.path()).meta(
                    json!({
                        "anureo.dev": {
                            "parentSessionId": root,
                            "title": "Child",
                            "metadata": { "kind": "child" }
                        }
                    })
                    .as_object()
                    .cloned()
                    .expect("session meta object"),
                ),
            )
            .await
            .expect("child session");
        let child = child_response.session_id.to_string();
        let child_json = serde_json::to_value(&child_response).expect("child response json");
        assert_eq!(
            child_json["_meta"]["anureo.dev"]["affectedSessions"][0]["sessionId"],
            root
        );
        assert_eq!(
            child_json["_meta"]["anureo.dev"]["affectedSessions"][0]["indexVersion"],
            child_json["_meta"]["anureo.dev"]["indexVersion"]
        );
        assert_eq!(
            child_json["_meta"]["anureo.dev"]["session"]["title"],
            "Child"
        );
        assert_eq!(
            child_json["_meta"]["anureo.dev"]["session"]["metadata"]["kind"],
            "child"
        );

        let handler = SessionListHandler::new();
        handler.bind(&agent);
        let ctx = ExtensionContext {
            session_id: None,
            principal: "local-anonymous".into(),
            connection_id: "test-connection".into(),
            working_directory: Some(temp.path().to_path_buf()),
            client_capabilities: ClientCapabilitiesInfo::default(),
        };
        let response = handler
            .handle(
                "archive",
                json!({ "sessionId": child, "archived": true }),
                &ctx,
            )
            .await
            .expect("archive response");
        assert_eq!(response["session"]["sessionId"], child);
        assert!(response["session"]["archivedAt"].is_string());
        assert_eq!(response["affectedSessions"][0]["sessionId"], root);
        assert_eq!(
            response["affectedSessions"][0]["indexVersion"],
            response["indexVersion"]
        );
    }

    #[tokio::test]
    async fn update_handler_is_target_only_and_explicitly_has_no_ancestor_effects() {
        let temp = tempfile::tempdir().unwrap();
        let agent =
            Arc::new(AnureoAcpAgent::new_with_db_path(temp.path().join("memory.db")).expect("agent"));
        let root = agent
            .new_session(NewSessionRequest::new(temp.path()))
            .await
            .expect("root session")
            .session_id
            .to_string();
        let child = agent
            .new_session(
                NewSessionRequest::new(temp.path()).meta(
                    json!({
                        "anureo.dev": {
                            "parentSessionId": root,
                            "title": "Child",
                            "metadata": { "kind": "child" }
                        }
                    })
                    .as_object()
                    .cloned()
                    .expect("session meta object"),
                ),
            )
            .await
            .expect("child session")
            .session_id
            .to_string();
        let root_before = agent
            .session_index_record_for_owner("local-anonymous", &root)
            .await
            .expect("root record")
            .expect("root exists");

        let handler = SessionListHandler::new();
        handler.bind(&agent);
        let ctx = ExtensionContext {
            session_id: None,
            principal: "local-anonymous".into(),
            connection_id: "test-connection".into(),
            working_directory: Some(temp.path().to_path_buf()),
            client_capabilities: ClientCapabilitiesInfo::default(),
        };
        let response = handler
            .handle(
                "update",
                json!({
                    "sessionId": child,
                    "title": "Renamed child",
                    "metadata": { "kind": "updated" }
                }),
                &ctx,
            )
            .await
            .expect("update response");

        assert_eq!(response["session"]["sessionId"], child);
        assert_eq!(response["session"]["title"], "Renamed child");
        assert_eq!(response["metadata"]["kind"], "updated");
        assert_eq!(response["affectedSessions"], json!([]));
        let root_after = agent
            .session_index_record_for_owner("local-anonymous", &root)
            .await
            .expect("root record")
            .expect("root exists");
        assert_eq!(root_after.revision, root_before.revision);
        assert_eq!(root_after.index_version, root_before.index_version);
    }

    #[tokio::test]
    async fn delete_response_contains_changed_ancestor_projection() {
        let temp = tempfile::tempdir().unwrap();
        let agent =
            Arc::new(AnureoAcpAgent::new_with_db_path(temp.path().join("memory.db")).expect("agent"));
        let root = agent
            .new_session(NewSessionRequest::new(temp.path()))
            .await
            .expect("root")
            .session_id
            .to_string();
        let child = agent
            .new_session(
                NewSessionRequest::new(temp.path()).meta(
                    json!({
                        "anureo.dev": { "parentSessionId": root, "title": "Child" }
                    })
                    .as_object()
                    .cloned()
                    .expect("meta"),
                ),
            )
            .await
            .expect("child")
            .session_id
            .to_string();

        let handler = SessionListHandler::new();
        handler.bind(&agent);
        let ctx = ExtensionContext {
            session_id: None,
            principal: "local-anonymous".into(),
            connection_id: "test-connection".into(),
            working_directory: Some(temp.path().to_path_buf()),
            client_capabilities: ClientCapabilitiesInfo::default(),
        };
        let response = handler
            .handle("delete", json!({ "sessionId": child }), &ctx)
            .await
            .expect("delete response");
        assert_eq!(response["tombstone"]["sessionId"], child);
        assert_eq!(response["affectedSessions"][0]["sessionId"], root);
        assert!(response["affectedSessions"][0]["revision"]
            .as_i64()
            .is_some());
    }

    #[tokio::test]
    async fn list_snapshot_pages_have_stable_version_without_duplicates() {
        let temp = tempfile::tempdir().unwrap();
        let agent =
            Arc::new(AnureoAcpAgent::new_with_db_path(temp.path().join("memory.db")).expect("agent"));
        for _ in 0..5 {
            agent
                .new_session(NewSessionRequest::new(temp.path()))
                .await
                .expect("session");
        }

        let handler = SessionListHandler::new();
        handler.bind(&agent);
        let ctx = ExtensionContext {
            session_id: None,
            principal: "local-anonymous".into(),
            connection_id: "test-connection".into(),
            working_directory: Some(temp.path().to_path_buf()),
            client_capabilities: ClientCapabilitiesInfo::default(),
        };
        let directory = temp.path().to_string_lossy().to_string();
        let mut cursor = None;
        let mut snapshot_version = None;
        let mut ids = Vec::new();
        for _ in 0..10 {
            let params = if let Some(next) = cursor.take() {
                // ACP clients commonly send only the opaque cursor and page
                // size after page one; omitted filters must inherit the
                // immutable snapshot rather than expire it.
                json!({ "cursor": next, "limit": 2 })
            } else {
                json!({
                    "archived": "active",
                    "directory": directory,
                    "limit": 2,
                })
            };
            let response = handler
                .handle("list", params, &ctx)
                .await
                .expect("list page");
            let version = response["snapshotVersion"]
                .as_i64()
                .expect("snapshot version");
            if let Some(previous) = snapshot_version {
                assert_eq!(version, previous);
            } else {
                snapshot_version = Some(version);
            }
            ids.extend(
                response["sessions"]
                    .as_array()
                    .expect("sessions array")
                    .iter()
                    .map(|item| item["sessionId"].as_str().unwrap().to_string()),
            );
            if !response["hasMore"].as_bool().unwrap_or(false) {
                break;
            }
            cursor = response["nextCursor"].as_str().map(str::to_string);
            assert!(cursor.is_some());
        }
        assert_eq!(ids.len(), 5);
        let unique = ids.iter().collect::<std::collections::HashSet<_>>();
        assert_eq!(unique.len(), ids.len());
    }

    /// Regression (GH run 33637237811, macos-14): `session/new` stores the
    /// canonicalized cwd, but a client listing with a non-canonical path
    /// (macOS `/var/folders/...` vs stored `/private/var/folders/...`)
    /// previously matched zero rows. The `directory` filter must be
    /// canonicalized the same way before matching. `join(".")` is a portable
    /// stand-in for a symlinked caller path: it exists but differs lexically.
    #[tokio::test]
    async fn list_directory_filter_canonicalizes_non_canonical_caller_path() {
        let temp = tempfile::tempdir().unwrap();
        let agent =
            Arc::new(AnureoAcpAgent::new_with_db_path(temp.path().join("memory.db")).expect("agent"));
        let session_id = agent
            .new_session(NewSessionRequest::new(temp.path()))
            .await
            .expect("session")
            .session_id
            .to_string();

        let handler = SessionListHandler::new();
        handler.bind(&agent);
        let ctx = ExtensionContext {
            session_id: None,
            principal: "local-anonymous".into(),
            connection_id: "test-connection".into(),
            working_directory: Some(temp.path().to_path_buf()),
            client_capabilities: ClientCapabilitiesInfo::default(),
        };
        let non_canonical = temp.path().join(".").to_string_lossy().to_string();
        assert_ne!(
            non_canonical,
            std::fs::canonicalize(temp.path())
                .expect("canonicalize")
                .to_string_lossy(),
            "precondition: caller path must differ lexically from the canonical path"
        );
        let response = handler
            .handle(
                "list",
                json!({ "directory": non_canonical, "limit": 10 }),
                &ctx,
            )
            .await
            .expect("list page");
        let ids: Vec<_> = response["sessions"]
            .as_array()
            .expect("sessions array")
            .iter()
            .filter(|item| item["sessionId"] == session_id)
            .collect();
        assert_eq!(ids.len(), 1, "non-canonical directory must still match");
    }

    fn base_metadata() -> SessionMetadata {
        SessionMetadata {
            session_id: "s".into(),
            thread_id: "t".into(),
            owner_principal: "local-anonymous".into(),
            cwd: r"C:\repo".into(),
            lifecycle: "idle".into(),
            title: None,
            updated_at: None,
            created_at: None,
            archived_at: None,
        }
    }
}
