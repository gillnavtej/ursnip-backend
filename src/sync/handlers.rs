//! REST handlers for the sync module.
//!
//! Provides HTTP handlers for snippet and folder CRUD operations,
//! batch operations, snapshot retrieval, delta polling, and WebSocket upgrade.
//!
//! Role requirements:
//! - GET snapshot, GET deltas: VIEWER+
//! - POST snippets, POST folders, POST batch: EDITOR+
//! - PATCH snippets/:id, PATCH folders/:id: EDITOR+ (ownership checked in service)
//! - DELETE snippets/:id, DELETE folders/:id: EDITOR+ (ownership checked in service)

use std::sync::Arc;

use axum::{
    extract::{Extension, Path, Query},
    http::StatusCode,
    response::IntoResponse,
    Json,
};
use serde::Deserialize;
use uuid::Uuid;

use crate::errors::AppError;
use crate::middleware::auth_extractor::AccessTokenClaims;
use crate::middleware::workspace_context::WorkspaceContext;
use crate::middleware::permissions::{
    self, check_permission, check_entity_permission,
    PERM_SNIPPETS_READ, PERM_SNIPPETS_CREATE, PERM_SNIPPETS_UPDATE_OWN,
    PERM_SNIPPETS_UPDATE_ANY, PERM_SNIPPETS_DELETE_OWN, PERM_SNIPPETS_DELETE_ANY,
    PERM_FOLDERS_CREATE, PERM_FOLDERS_UPDATE, PERM_FOLDERS_DELETE,
    PERM_ANALYTICS_VIEW,
};
use crate::notifications::service::{NotificationService, persist_notification_async};
use crate::sync::service::{
    BatchOperationsPayload, CreateFolderPayload, CreateSnippetPayload, SyncService,
    UpdateFolderPayload, UpdateSnippetPayload,
};
use crate::sync::session_registry::SessionRegistry;

// ─── Query Parameter DTOs ───────────────────────────────────────────────────────

/// Query parameters for the GET /sync/snapshot endpoint.
#[derive(Debug, Deserialize)]
pub struct SnapshotQueryParams {
    pub workspace_id: Uuid,
}

/// Query parameters for the GET /sync/deltas endpoint.
#[derive(Debug, Deserialize)]
pub struct DeltasQueryParams {
    pub workspace_id: Uuid,
    pub since_version: Option<i64>,
    pub limit: Option<i64>,
}

// ─── Snippet Handlers ───────────────────────────────────────────────────────────

/// POST /sync/snippets
///
/// Create a new snippet in a workspace.
pub async fn create_snippet_handler(
    Extension(service): Extension<Arc<SyncService>>,
    Extension(registry): Extension<Arc<SessionRegistry>>,
    Extension(claims): Extension<AccessTokenClaims>,
    Extension(ws_ctx): Extension<WorkspaceContext>,
    Json(payload): Json<CreateSnippetPayload>,
) -> Result<impl IntoResponse, AppError> {
    check_permission(&ws_ctx, PERM_SNIPPETS_CREATE)?;
    let workspace_id = payload.workspace_id;
    let response = service.create_snippet(claims.sub, payload).await?;

    // Broadcast delta to workspace via WebSocket
    registry.broadcast_to_workspace(
        workspace_id,
        serde_json::json!({
            "type": "delta",
            "workspace_id": workspace_id.to_string(),
            "version": response.version,
            "timestamp": chrono::Utc::now().to_rfc3339(),
            "payload": serde_json::to_value(&response).unwrap_or_default()
        }),
        None,
    );

    Ok((StatusCode::CREATED, Json(response)))
}

/// PATCH /sync/snippets/:id
///
/// Update an existing snippet.
pub async fn update_snippet_handler(
    Extension(service): Extension<Arc<SyncService>>,
    Extension(registry): Extension<Arc<SessionRegistry>>,
    Extension(claims): Extension<AccessTokenClaims>,
    Extension(ws_ctx): Extension<WorkspaceContext>,
    Path(snippet_id): Path<Uuid>,
    Json(payload): Json<UpdateSnippetPayload>,
) -> Result<impl IntoResponse, AppError> {
    check_permission(&ws_ctx, PERM_SNIPPETS_UPDATE_OWN)?;
    let response = service.update_snippet(claims.sub, snippet_id, payload, ws_ctx.role).await?;

    // Broadcast delta to workspace via WebSocket
    registry.broadcast_to_workspace(
        response.workspace_id,
        serde_json::json!({
            "type": "delta",
            "workspace_id": response.workspace_id.to_string(),
            "version": response.version,
            "timestamp": chrono::Utc::now().to_rfc3339(),
            "payload": serde_json::to_value(&response).unwrap_or_default()
        }),
        None,
    );

    Ok((StatusCode::OK, Json(response)))
}

/// DELETE /sync/snippets/:id
///
/// Soft-delete a snippet.
pub async fn delete_snippet_handler(
    Extension(service): Extension<Arc<SyncService>>,
    Extension(registry): Extension<Arc<SessionRegistry>>,
    Extension(notif_service): Extension<Arc<NotificationService>>,
    Extension(claims): Extension<AccessTokenClaims>,
    Extension(ws_ctx): Extension<WorkspaceContext>,
    Path(snippet_id): Path<Uuid>,
) -> Result<impl IntoResponse, AppError> {
    check_permission(&ws_ctx, PERM_SNIPPETS_DELETE_OWN)?;
    service.delete_snippet(claims.sub, snippet_id, ws_ctx.role).await?;

    // Broadcast snippet deleted notification
    registry.broadcast_to_workspace(
        ws_ctx.workspace_id,
        serde_json::json!({
            "type": "notification",
            "event": "snippet_deleted",
            "snippet_id": snippet_id.to_string(),
            "user_id": claims.sub.to_string(),
            "workspace_id": ws_ctx.workspace_id.to_string(),
            "timestamp": chrono::Utc::now().to_rfc3339(),
        }),
        None,
    );

    // Persist notification
    persist_notification_async(
        notif_service.clone(),
        ws_ctx.workspace_id,
        claims.sub,
        "snippet_deleted".to_string(),
        "Snippet Deleted".to_string(),
        Some("A snippet was moved to trash".to_string()),
        Some(serde_json::json!({ "snippet_id": snippet_id.to_string() })),
    );

    Ok(StatusCode::NO_CONTENT)
}

// ─── Batch Handler ──────────────────────────────────────────────────────────────

/// POST /sync/snippets/batch
///
/// Execute batch snippet operations (create, update, delete) atomically.
pub async fn batch_operations_handler(
    Extension(service): Extension<Arc<SyncService>>,
    Extension(registry): Extension<Arc<SessionRegistry>>,
    Extension(claims): Extension<AccessTokenClaims>,
    Extension(ws_ctx): Extension<WorkspaceContext>,
    Json(payload): Json<BatchOperationsPayload>,
) -> Result<impl IntoResponse, AppError> {
    check_permission(&ws_ctx, PERM_SNIPPETS_CREATE)?;
    let workspace_id = payload.workspace_id;
    let response = service.batch_operations(claims.sub, payload).await?;

    // Broadcast delta to workspace via WebSocket
    registry.broadcast_to_workspace(
        workspace_id,
        serde_json::json!({
            "type": "delta",
            "workspace_id": workspace_id.to_string(),
            "version": response.workspace_version,
            "timestamp": chrono::Utc::now().to_rfc3339(),
            "payload": serde_json::to_value(&response).unwrap_or_default()
        }),
        None,
    );

    Ok((StatusCode::OK, Json(response)))
}

// ─── Folder Handlers ────────────────────────────────────────────────────────────

/// POST /sync/folders
///
/// Create a new folder in a workspace.
pub async fn create_folder_handler(
    Extension(service): Extension<Arc<SyncService>>,
    Extension(registry): Extension<Arc<SessionRegistry>>,
    Extension(claims): Extension<AccessTokenClaims>,
    Extension(ws_ctx): Extension<WorkspaceContext>,
    Json(payload): Json<CreateFolderPayload>,
) -> Result<impl IntoResponse, AppError> {
    check_permission(&ws_ctx, PERM_FOLDERS_CREATE)?;
    let workspace_id = payload.workspace_id;
    let response = service.create_folder(claims.sub, payload).await?;

    // Broadcast delta to workspace via WebSocket
    registry.broadcast_to_workspace(
        workspace_id,
        serde_json::json!({
            "type": "delta",
            "workspace_id": workspace_id.to_string(),
            "version": response.version,
            "timestamp": chrono::Utc::now().to_rfc3339(),
            "payload": serde_json::to_value(&response).unwrap_or_default()
        }),
        None,
    );

    Ok((StatusCode::CREATED, Json(response)))
}

/// PATCH /sync/folders/:id
///
/// Update an existing folder.
pub async fn update_folder_handler(
    Extension(service): Extension<Arc<SyncService>>,
    Extension(registry): Extension<Arc<SessionRegistry>>,
    Extension(claims): Extension<AccessTokenClaims>,
    Extension(ws_ctx): Extension<WorkspaceContext>,
    Path(folder_id): Path<Uuid>,
    Json(payload): Json<UpdateFolderPayload>,
) -> Result<impl IntoResponse, AppError> {
    check_permission(&ws_ctx, PERM_FOLDERS_UPDATE)?;
    let response = service.update_folder(claims.sub, folder_id, payload, ws_ctx.role).await?;

    // Broadcast delta to workspace via WebSocket
    registry.broadcast_to_workspace(
        response.workspace_id,
        serde_json::json!({
            "type": "delta",
            "workspace_id": response.workspace_id.to_string(),
            "version": response.version,
            "timestamp": chrono::Utc::now().to_rfc3339(),
            "payload": serde_json::to_value(&response).unwrap_or_default()
        }),
        None,
    );

    Ok((StatusCode::OK, Json(response)))
}

/// DELETE /sync/folders/:id
///
/// Soft-delete a folder.
pub async fn delete_folder_handler(
    Extension(service): Extension<Arc<SyncService>>,
    Extension(registry): Extension<Arc<SessionRegistry>>,
    Extension(claims): Extension<AccessTokenClaims>,
    Extension(ws_ctx): Extension<WorkspaceContext>,
    Path(folder_id): Path<Uuid>,
) -> Result<impl IntoResponse, AppError> {
    check_permission(&ws_ctx, PERM_FOLDERS_DELETE)?;
    service.delete_folder(claims.sub, folder_id, ws_ctx.role).await?;

    // Broadcast folder deleted notification
    registry.broadcast_to_workspace(
        ws_ctx.workspace_id,
        serde_json::json!({
            "type": "notification",
            "event": "folder_deleted",
            "folder_id": folder_id.to_string(),
            "user_id": claims.sub.to_string(),
            "workspace_id": ws_ctx.workspace_id.to_string(),
            "timestamp": chrono::Utc::now().to_rfc3339(),
        }),
        None,
    );

    Ok(StatusCode::NO_CONTENT)
}

// ─── Snapshot & Delta Handlers ──────────────────────────────────────────────────

/// GET /sync/snapshot
///
/// Retrieve a full workspace snapshot (all active snippets and folders).
pub async fn get_snapshot_handler(
    Extension(service): Extension<Arc<SyncService>>,
    Extension(claims): Extension<AccessTokenClaims>,
    Extension(ws_ctx): Extension<WorkspaceContext>,
    Query(params): Query<SnapshotQueryParams>,
) -> Result<impl IntoResponse, AppError> {
    check_permission(&ws_ctx, PERM_SNIPPETS_READ)?;
    let response = service.get_snapshot(claims.sub, params.workspace_id).await?;
    Ok((StatusCode::OK, Json(response)))
}

/// GET /sync/deltas
///
/// Retrieve deltas since a given version for incremental sync.
pub async fn get_deltas_handler(
    Extension(service): Extension<Arc<SyncService>>,
    Extension(claims): Extension<AccessTokenClaims>,
    Extension(ws_ctx): Extension<WorkspaceContext>,
    Query(params): Query<DeltasQueryParams>,
) -> Result<impl IntoResponse, AppError> {
    check_permission(&ws_ctx, PERM_SNIPPETS_READ)?;
    let since_version = params.since_version.unwrap_or(0);
    let response = service
        .get_deltas(claims.sub, params.workspace_id, since_version, params.limit)
        .await?;
    Ok((StatusCode::OK, Json(response)))
}

// ─── WebSocket Handler ──────────────────────────────────────────────────────────

/// GET /sync/ws
///
/// WebSocket upgrade endpoint. Returns 501 Not Implemented as a placeholder
/// until the full WebSocket handler is wired (see sync/websocket.rs).
pub async fn ws_handler() -> StatusCode {
    StatusCode::NOT_IMPLEMENTED
}

// ─── Snippet Analytics Handlers ─────────────────────────────────────────────────

use sqlx::PgPool;
use crate::sync::analytics::{self, RecordUsageRequest};

/// POST /sync/snippets/:id/usage
///
/// Record a snippet expansion event. Called by the Tauri desktop app
/// each time a snippet trigger is expanded.
/// Also broadcasts a live analytics event to all workspace WebSocket connections.
pub async fn record_snippet_usage_handler(
    Extension(pool): Extension<PgPool>,
    Extension(registry): Extension<Arc<SessionRegistry>>,
    Extension(claims): Extension<AccessTokenClaims>,
    Path(snippet_id): Path<Uuid>,
    Json(body): Json<RecordUsageRequest>,
) -> Result<impl IntoResponse, AppError> {
    analytics::record_usage(
        &pool,
        snippet_id,
        body.workspace_id,
        claims.sub,
        body.device_id,
    )
    .await?;

    // Broadcast live analytics event to workspace
    registry.broadcast_to_workspace(
        body.workspace_id,
        serde_json::json!({
            "type": "analytics",
            "event": "snippet_expanded",
            "snippet_id": snippet_id.to_string(),
            "user_id": claims.sub.to_string(),
            "workspace_id": body.workspace_id.to_string(),
            "timestamp": chrono::Utc::now().to_rfc3339(),
        }),
        None,
    );

    Ok(StatusCode::CREATED)
}

/// GET /sync/snippets/:id/analytics
///
/// Get usage analytics for a specific snippet (last 30 days).
/// Requires VIEWER+ role (workspace membership checked by middleware).
pub async fn get_snippet_analytics_handler(
    Extension(pool): Extension<PgPool>,
    Extension(ws_ctx): Extension<WorkspaceContext>,
    Path(snippet_id): Path<Uuid>,
) -> Result<impl IntoResponse, AppError> {
    check_permission(&ws_ctx, PERM_ANALYTICS_VIEW)?;
    let response = analytics::get_snippet_analytics(&pool, snippet_id).await?;
    Ok((StatusCode::OK, Json(response)))
}

/// GET /sync/analytics
///
/// Get analytics for the authenticated user across all their workspaces.
/// Scoped to the user's own snippet usage only.
pub async fn get_platform_analytics_handler(
    Extension(pool): Extension<PgPool>,
    Extension(claims): Extension<AccessTokenClaims>,
) -> Result<impl IntoResponse, AppError> {
    eprintln!("[ANALYTICS] Handler called for user: {}", claims.sub);
    
    // Test: simple count query
    let count: Option<i64> = sqlx::query_scalar("SELECT COUNT(*) FROM snippet_usages WHERE user_id = $1")
        .bind(claims.sub)
        .fetch_one(&pool)
        .await
        .map_err(|e| { eprintln!("[ANALYTICS] count query failed: {e:?}"); AppError::InternalError })?;
    
    eprintln!("[ANALYTICS] count = {:?}", count);

    // Test: daily query with date_trunc  
    let daily_rows = sqlx::query_as::<_, analytics::DailyExpansionsByType>(
        r#"
        SELECT
            date_trunc('day', su.expanded_at) AS date,
            COUNT(*) FILTER (WHERE s.snippet_type = 'static') AS static_count,
            COUNT(*) FILTER (WHERE s.snippet_type = 'dynamic') AS dynamic_count,
            COUNT(*) FILTER (WHERE s.snippet_type = 'ai') AS ai_count
        FROM snippet_usages su
        JOIN snippets s ON s.id = su.snippet_id
        WHERE su.user_id = $1
        GROUP BY date_trunc('day', su.expanded_at)
        ORDER BY date ASC
        "#,
    )
    .bind(claims.sub)
    .fetch_all(&pool)
    .await
    .map_err(|e| { eprintln!("[ANALYTICS] daily query failed: {e:?}"); AppError::InternalError })?;

    eprintln!("[ANALYTICS] daily rows = {}", daily_rows.len());

    // If we get here, run the full analytics
    let response = analytics::get_user_analytics(&pool, claims.sub).await?;
    eprintln!("[ANALYTICS] Full response success!");
    Ok((StatusCode::OK, Json(response)))
}

/// PATCH /sync/snippets/:id/restore
///
/// Restore a soft-deleted snippet (set deleted_at = NULL).
pub async fn restore_snippet_handler(
    Extension(service): Extension<Arc<SyncService>>,
    Extension(registry): Extension<Arc<SessionRegistry>>,
    Extension(pool): Extension<PgPool>,
    Extension(claims): Extension<AccessTokenClaims>,
    Extension(ws_ctx): Extension<WorkspaceContext>,
    Path(snippet_id): Path<Uuid>,
) -> Result<impl IntoResponse, AppError> {
    check_permission(&ws_ctx, PERM_SNIPPETS_DELETE_ANY)?;

    // Restore: clear deleted_at, bump version
    let row = sqlx::query_as::<_, (Uuid, String, bool, i64)>(
        r#"
        UPDATE snippets SET deleted_at = NULL, is_enabled = true, version = version + 1, updated_at = now()
        WHERE id = $1 AND deleted_at IS NOT NULL
        RETURNING workspace_id, trigger, is_enabled, version
        "#,
    )
    .bind(snippet_id)
    .fetch_optional(&pool)
    .await
    .map_err(|_| AppError::InternalError)?;

    let (workspace_id, trigger, is_enabled, version) = row.ok_or(AppError::WorkspaceNotFound)?;

    // Broadcast restore notification
    registry.broadcast_to_workspace(
        workspace_id,
        serde_json::json!({
            "type": "delta",
            "workspace_id": workspace_id.to_string(),
            "version": version,
            "timestamp": chrono::Utc::now().to_rfc3339(),
            "payload": {
                "trigger": trigger,
                "is_enabled": is_enabled,
                "version": version,
            }
        }),
        None,
    );

    Ok((StatusCode::OK, Json(serde_json::json!({ "status": "restored", "trigger": trigger }))))
}

/// DELETE /sync/snippets/:id/hard
///
/// Permanently delete a snippet (hard delete — removes from DB entirely).
/// Only works on already soft-deleted snippets. Requires ADMIN+ role.
pub async fn hard_delete_snippet_handler(
    Extension(registry): Extension<Arc<SessionRegistry>>,
    Extension(notif_service): Extension<Arc<NotificationService>>,
    Extension(pool): Extension<PgPool>,
    Extension(claims): Extension<AccessTokenClaims>,
    Extension(ws_ctx): Extension<WorkspaceContext>,
    Path(snippet_id): Path<Uuid>,
) -> Result<impl IntoResponse, AppError> {
    check_permission(&ws_ctx, PERM_SNIPPETS_DELETE_ANY)?;

    // Only hard-delete snippets that are already soft-deleted
    let result = sqlx::query(
        "DELETE FROM snippets WHERE id = $1 AND deleted_at IS NOT NULL RETURNING workspace_id",
    )
    .bind(snippet_id)
    .execute(&pool)
    .await
    .map_err(|_| AppError::InternalError)?;

    if result.rows_affected() == 0 {
        return Err(AppError::WorkspaceNotFound);
    }

    // Broadcast hard delete notification
    registry.broadcast_to_workspace(
        ws_ctx.workspace_id,
        serde_json::json!({
            "type": "notification",
            "event": "snippet_hard_deleted",
            "snippet_id": snippet_id.to_string(),
            "user_id": claims.sub.to_string(),
            "workspace_id": ws_ctx.workspace_id.to_string(),
            "timestamp": chrono::Utc::now().to_rfc3339(),
        }),
        None,
    );

    // Persist notification
    persist_notification_async(
        notif_service.clone(),
        ws_ctx.workspace_id,
        claims.sub,
        "snippet_hard_deleted".to_string(),
        "Snippet Permanently Deleted".to_string(),
        Some("A snippet was permanently removed".to_string()),
        Some(serde_json::json!({ "snippet_id": snippet_id.to_string() })),
    );

    Ok(StatusCode::NO_CONTENT)
}

/// PATCH /sync/folders/:id/restore
///
/// Restore a soft-deleted folder.
pub async fn restore_folder_handler(
    Extension(registry): Extension<Arc<SessionRegistry>>,
    Extension(pool): Extension<PgPool>,
    Extension(claims): Extension<AccessTokenClaims>,
    Extension(ws_ctx): Extension<WorkspaceContext>,
    Path(folder_id): Path<Uuid>,
) -> Result<impl IntoResponse, AppError> {
    check_permission(&ws_ctx, PERM_FOLDERS_DELETE)?;

    let row = sqlx::query_as::<_, (Uuid, String, i64)>(
        r#"
        UPDATE folders SET deleted_at = NULL, version = version + 1, updated_at = now()
        WHERE id = $1 AND deleted_at IS NOT NULL
        RETURNING workspace_id, name, version
        "#,
    )
    .bind(folder_id)
    .fetch_optional(&pool)
    .await
    .map_err(|_| AppError::InternalError)?;

    let (workspace_id, name, version) = row.ok_or(AppError::WorkspaceNotFound)?;

    registry.broadcast_to_workspace(
        workspace_id,
        serde_json::json!({
            "type": "delta",
            "workspace_id": workspace_id.to_string(),
            "version": version,
            "timestamp": chrono::Utc::now().to_rfc3339(),
            "payload": { "name": name, "version": version }
        }),
        None,
    );

    Ok((StatusCode::OK, Json(serde_json::json!({ "status": "restored", "name": name }))))
}

/// DELETE /sync/folders/:id/hard
///
/// Permanently delete a folder (hard delete).
pub async fn hard_delete_folder_handler(
    Extension(registry): Extension<Arc<SessionRegistry>>,
    Extension(pool): Extension<PgPool>,
    Extension(claims): Extension<AccessTokenClaims>,
    Extension(ws_ctx): Extension<WorkspaceContext>,
    Path(folder_id): Path<Uuid>,
) -> Result<impl IntoResponse, AppError> {
    check_permission(&ws_ctx, PERM_FOLDERS_DELETE)?;

    let result = sqlx::query(
        "DELETE FROM folders WHERE id = $1 AND deleted_at IS NOT NULL",
    )
    .bind(folder_id)
    .execute(&pool)
    .await
    .map_err(|_| AppError::InternalError)?;

    if result.rows_affected() == 0 {
        return Err(AppError::WorkspaceNotFound);
    }

    registry.broadcast_to_workspace(
        ws_ctx.workspace_id,
        serde_json::json!({
            "type": "notification",
            "event": "folder_hard_deleted",
            "folder_id": folder_id.to_string(),
            "user_id": claims.sub.to_string(),
            "workspace_id": ws_ctx.workspace_id.to_string(),
            "timestamp": chrono::Utc::now().to_rfc3339(),
        }),
        None,
    );

    Ok(StatusCode::NO_CONTENT)
}

// ─── Data Sources ────────────────────────────────────────────────────────────────

/// GET /sync/data-sources
///
/// Get the current user's data source configurations.
/// Returns the JSONB array stored in user_data_sources.
pub async fn get_data_sources_handler(
    Extension(pool): Extension<PgPool>,
    Extension(claims): Extension<AccessTokenClaims>,
) -> Result<impl IntoResponse, AppError> {
    let row: Option<(serde_json::Value,)> = sqlx::query_as(
        "SELECT configs FROM user_data_sources WHERE user_id = $1",
    )
    .bind(claims.sub)
    .fetch_optional(&pool)
    .await
    .map_err(|_| AppError::InternalError)?;

    let configs = row.map(|(c,)| c).unwrap_or(serde_json::json!([]));
    Ok((StatusCode::OK, Json(configs)))
}

/// PUT /sync/data-sources
///
/// Save (replace) the current user's data source configurations.
/// Accepts a JSON array of data source config objects.
pub async fn save_data_sources_handler(
    Extension(pool): Extension<PgPool>,
    Extension(claims): Extension<AccessTokenClaims>,
    Json(configs): Json<serde_json::Value>,
) -> Result<impl IntoResponse, AppError> {
    // Validate it's an array
    if !configs.is_array() {
        return Err(AppError::ValidationError {
            details: vec![crate::errors::FieldError {
                field: "configs".into(),
                message: "Must be a JSON array".into(),
            }],
        });
    }

    sqlx::query(
        r#"
        INSERT INTO user_data_sources (user_id, configs, updated_at)
        VALUES ($1, $2, now())
        ON CONFLICT (user_id) DO UPDATE
        SET configs = $2, updated_at = now()
        "#,
    )
    .bind(claims.sub)
    .bind(&configs)
    .execute(&pool)
    .await
    .map_err(|_| AppError::InternalError)?;

    Ok((StatusCode::OK, Json(serde_json::json!({
        "status": "saved",
        "count": configs.as_array().map(|a| a.len()).unwrap_or(0),
    }))))
}
