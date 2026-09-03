use serde_json::Value;

use super::*;
use super::{ExtensionContext, ExtensionError};

pub async fn handle(
    params: Value,
    _method: &str,
    ctx: &ExtensionContext,
) -> Result<Value, ExtensionError> {
    match _method {
        "stash_list" => handle_stash_list(params, ctx).await,
        "stash_create" => handle_stash_create(params, ctx).await,
        "stash_pop" => handle_stash_pop(params, ctx).await,
        "stash_apply" => handle_stash_apply(params, ctx).await,
        "stash_drop" => handle_stash_drop(params, ctx).await,
        "stash_count" => handle_stash_count(params, ctx).await,
        _ => Err(ExtensionError::method_not_found()),
    }
}

pub async fn handle_stash_list(
    params: Value,
    ctx: &ExtensionContext,
) -> Result<Value, ExtensionError> {
    let cursor: Option<String> = params
        .get("cursor")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let limit: usize = params
        .get("limit")
        .and_then(|v| v.as_u64())
        .map(|v| v as usize)
        .unwrap_or(50);
    let skip = decode_cursor_offset(&cursor);

    let repo_dir = ctx
        .working_directory
        .clone()
        .unwrap_or_else(|| std::path::PathBuf::from("."));
    let items = anureo_git::facade::stash_list(&repo_dir)
        .await
        .map_err(ext_err_from_git)?;

    let total = items.len();
    let end = (skip + limit).min(total);
    let page = items[skip..end].to_vec();
    let has_more = end < total;
    let next_cursor = if has_more {
        encode_cursor_offset(end)
    } else {
        None
    };

    Ok(serde_json::json!({
        "items": page,
        "nextCursor": next_cursor,
        "hasMore": has_more,
    }))
}

pub async fn handle_stash_create(
    params: Value,
    ctx: &ExtensionContext,
) -> Result<Value, ExtensionError> {
    require_git_scope(ctx, "git:stage")?;
    let message: Option<String> = optional_param_str(&params, "message");
    let include_untracked: bool = params
        .get("includeUntracked")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let keep_index: bool = params
        .get("keepIndex")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    let repo_dir = ctx
        .working_directory
        .clone()
        .unwrap_or_else(|| std::path::PathBuf::from("."));
    if !anureo_git::facade::is_dirty(&repo_dir)
        .await
        .unwrap_or(false)
    {
        return Err(ExtensionError::invalid_params("no local changes to save"));
    }

    let opts = anureo_git::types::StashPushOptions {
        message: message.clone(),
        include_untracked,
        keep_index,
    };
    anureo_git::facade::stash_push(&repo_dir, opts)
        .await
        .map_err(ext_err_from_git)?;
    let list = anureo_git::facade::stash_list(&repo_dir)
        .await
        .map_err(ext_err_from_git)?;
    let (index, msg_out) = list
        .first()
        .map(|e| (e.index, e.message.clone()))
        .unwrap_or((0u32, String::new()));

    Ok(serde_json::json!({
        "index": index,
        "message": msg_out,
        "created": true,
    }))
}

pub async fn handle_stash_pop(
    params: Value,
    ctx: &ExtensionContext,
) -> Result<Value, ExtensionError> {
    require_git_scope(ctx, "git:stage")?;
    let index: u32 = require_param(&params, "index")?;
    let repo_dir = ctx
        .working_directory
        .clone()
        .unwrap_or_else(|| std::path::PathBuf::from("."));
    match anureo_git::facade::stash_pop(&repo_dir, index as usize).await {
        Ok(_) => Ok(serde_json::json!({"index": index, "popped": true})),
        Err(e) => match e.kind() {
            anureo_git::GitErrorKind::Conflict => Err(ExtensionError::invalid_params(
                "stash pop resulted in merge conflict",
            )),
            anureo_git::GitErrorKind::NotFound => Err(ExtensionError::not_found(format!(
                "stash entry {index} not found"
            ))),
            _ => Err(ext_err_from_git(e)),
        },
    }
}

pub async fn handle_stash_apply(
    params: Value,
    ctx: &ExtensionContext,
) -> Result<Value, ExtensionError> {
    require_git_scope(ctx, "git:stage")?;
    let index: u32 = require_param(&params, "index")?;
    let repo_dir = ctx
        .working_directory
        .clone()
        .unwrap_or_else(|| std::path::PathBuf::from("."));
    match anureo_git::facade::stash_apply(&repo_dir, index as usize).await {
        Ok(_) => Ok(serde_json::json!({"index": index, "applied": true})),
        Err(e) => match e.kind() {
            anureo_git::GitErrorKind::Conflict => Err(ExtensionError::invalid_params(
                "stash apply resulted in merge conflict",
            )),
            anureo_git::GitErrorKind::NotFound => Err(ExtensionError::not_found(format!(
                "stash entry {index} not found"
            ))),
            _ => Err(ext_err_from_git(e)),
        },
    }
}

pub async fn handle_stash_drop(
    params: Value,
    ctx: &ExtensionContext,
) -> Result<Value, ExtensionError> {
    require_git_scope(ctx, "git:stage")?;
    let index: u32 = require_param(&params, "index")?;
    let repo_dir = ctx
        .working_directory
        .clone()
        .unwrap_or_else(|| std::path::PathBuf::from("."));
    anureo_git::facade::stash_drop(&repo_dir, index as usize)
        .await
        .map_err(ext_err_from_git)?;
    Ok(serde_json::json!({"index": index, "dropped": true}))
}

pub async fn handle_stash_count(
    params: Value,
    ctx: &ExtensionContext,
) -> Result<Value, ExtensionError> {
    let _ = params;
    let repo_dir = ctx
        .working_directory
        .clone()
        .unwrap_or_else(|| std::path::PathBuf::from("."));
    let result = anureo_git::facade::stash_count(&repo_dir)
        .await
        .map_err(ext_err_from_git)?;

    let files: Vec<_> = result
        .files
        .iter()
        .map(|f| {
            serde_json::json!({
                "path": f.path,
                "insertions": f.insertions,
                "deletions": f.deletions,
            })
        })
        .collect();

    Ok(serde_json::json!({
        "count": result.count,
        "files": files,
    }))
}
