//! HTTP handlers for team workspace endpoints.
//!
//! Provides handlers for creating team workspaces, managing invites,
//! joining via invite codes, removing members, and listing workspace details.

use std::sync::Arc;

use axum::{
    extract::{Extension, Path, Query},
    http::StatusCode,
    response::IntoResponse,
    Json,
};
use serde::Deserialize;
use sqlx::PgPool;
use uuid::Uuid;

use crate::errors::AppError;
use crate::middleware::auth_extractor::AccessTokenClaims;
use crate::middleware::workspace_context::WorkspaceContext;
use crate::middleware::permissions::{
    check_permission, PERM_MEMBERS_INVITE, PERM_MEMBERS_REMOVE,
    PERM_SNIPPETS_READ, PERM_WORKSPACE_SETTINGS, PERM_WORKSPACE_DELETE,
};
use crate::sync::session_registry::SessionRegistry;
use crate::workspace::service::{TeamService, WorkspaceService};

// ─── Request Body DTOs ──────────────────────────────────────────────────────────

/// Request body for creating a team workspace.
#[derive(Debug, Deserialize)]
pub struct CreateTeamRequest {
    pub name: String,
}

/// Request body for creating an invite link.
#[derive(Debug, Deserialize)]
pub struct CreateInviteRequest {
    pub max_uses: Option<i32>,
    pub expires_in_days: Option<i64>,
    /// Role to assign to users who join via this invite (default: "editor").
    pub role: Option<String>,
    /// Custom permissions to assign. If None, role-based defaults are used.
    pub permissions: Option<Vec<String>>,
}

/// Request body for joining a workspace via invite code.
#[derive(Debug, Deserialize)]
pub struct JoinViaInviteRequest {
    pub invite_code: String,
}

// ─── Handlers ───────────────────────────────────────────────────────────────────

/// POST /teams
///
/// Create a new team workspace. The authenticated user becomes the owner.
pub async fn create_team_handler(
    Extension(service): Extension<Arc<WorkspaceService>>,
    Extension(claims): Extension<AccessTokenClaims>,
    Json(body): Json<CreateTeamRequest>,
) -> Result<impl IntoResponse, AppError> {
    let response = service.create_team_workspace(claims.sub, &body.name).await?;
    Ok((StatusCode::CREATED, Json(response)))
}

/// POST /teams/{workspace_id}/invites
///
/// Create an invite link for a team workspace. Requires ADMIN+ role.
pub async fn create_invite_handler(
    Extension(service): Extension<Arc<TeamService>>,
    Extension(registry): Extension<Arc<SessionRegistry>>,
    Extension(claims): Extension<AccessTokenClaims>,
    Extension(ws_ctx): Extension<WorkspaceContext>,
    Path(workspace_id): Path<Uuid>,
    Json(body): Json<CreateInviteRequest>,
) -> Result<impl IntoResponse, AppError> {
    check_permission(&ws_ctx, PERM_MEMBERS_INVITE)?;

    // Validate permissions if provided
    if let Some(ref perms) = body.permissions {
        crate::middleware::permissions::validate_permissions(perms)?;
    }

    let response = service
        .create_invite(
            workspace_id,
            claims.sub,
            body.max_uses,
            body.expires_in_days,
            body.role.as_deref(),
            body.permissions.as_deref(),
        )
        .await?;

    // Broadcast invite created notification
    registry.broadcast_to_workspace(
        workspace_id,
        serde_json::json!({
            "type": "notification",
            "event": "invite_created",
            "user_id": claims.sub.to_string(),
            "workspace_id": workspace_id.to_string(),
            "timestamp": chrono::Utc::now().to_rfc3339(),
        }),
        None,
    );

    Ok((StatusCode::CREATED, Json(response)))
}

/// POST /teams/{workspace_id}/join
///
/// Join a team workspace using an invite code.
pub async fn join_via_invite_handler(
    Extension(service): Extension<Arc<TeamService>>,
    Extension(registry): Extension<Arc<SessionRegistry>>,
    Extension(claims): Extension<AccessTokenClaims>,
    Path(_workspace_id): Path<Uuid>,
    Json(body): Json<JoinViaInviteRequest>,
) -> Result<impl IntoResponse, AppError> {
    let response = service.join_via_invite(body.invite_code, claims.sub).await?;

    // Broadcast member joined notification
    registry.broadcast_to_workspace(
        response.workspace_id,
        serde_json::json!({
            "type": "notification",
            "event": "member_joined",
            "user_id": claims.sub.to_string(),
            "workspace_id": response.workspace_id.to_string(),
            "timestamp": chrono::Utc::now().to_rfc3339(),
        }),
        None,
    );

    Ok((StatusCode::OK, Json(response)))
}

/// DELETE /teams/{workspace_id}/members/{user_id}
///
/// Remove a member from a team workspace. Requires ADMIN+ role.
pub async fn remove_member_handler(
    Extension(service): Extension<Arc<TeamService>>,
    Extension(registry): Extension<Arc<SessionRegistry>>,
    Extension(claims): Extension<AccessTokenClaims>,
    Extension(ws_ctx): Extension<WorkspaceContext>,
    Path((workspace_id, user_id)): Path<(Uuid, Uuid)>,
) -> Result<impl IntoResponse, AppError> {
    check_permission(&ws_ctx, PERM_MEMBERS_REMOVE)?;
    service.remove_member(workspace_id, claims.sub, user_id).await?;

    // Broadcast member removed notification
    registry.broadcast_to_workspace(
        workspace_id,
        serde_json::json!({
            "type": "notification",
            "event": "member_removed",
            "removed_user_id": user_id.to_string(),
            "by_user_id": claims.sub.to_string(),
            "workspace_id": workspace_id.to_string(),
            "timestamp": chrono::Utc::now().to_rfc3339(),
        }),
        None,
    );

    Ok(StatusCode::NO_CONTENT)
}

/// GET /teams/{workspace_id}
///
/// Get team workspace details. Requires VIEWER+ role.
pub async fn get_team_handler(
    Extension(service): Extension<Arc<WorkspaceService>>,
    Extension(ws_ctx): Extension<WorkspaceContext>,
    Path(workspace_id): Path<Uuid>,
) -> Result<impl IntoResponse, AppError> {
    check_permission(&ws_ctx, PERM_SNIPPETS_READ)?;
    let response = service.get_workspace(workspace_id).await?;
    Ok((StatusCode::OK, Json(response)))
}

/// GET /teams/{workspace_id}/members
///
/// List all members of a team workspace. Requires VIEWER+ role.
pub async fn list_members_handler(
    Extension(service): Extension<Arc<TeamService>>,
    Extension(ws_ctx): Extension<WorkspaceContext>,
    Path(workspace_id): Path<Uuid>,
) -> Result<impl IntoResponse, AppError> {
    check_permission(&ws_ctx, PERM_SNIPPETS_READ)?;
    let response = service.list_members(workspace_id).await?;
    Ok((StatusCode::OK, Json(response)))
}

// ─── Workspace CRUD Handlers ────────────────────────────────────────────────────

/// Request body for updating a workspace.
#[derive(Debug, Deserialize)]
pub struct UpdateWorkspaceRequest {
    pub name: Option<String>,
}

/// GET /workspaces
///
/// List all workspaces the authenticated user belongs to.
pub async fn list_workspaces_handler(
    Extension(service): Extension<Arc<WorkspaceService>>,
    Extension(claims): Extension<AccessTokenClaims>,
) -> Result<impl IntoResponse, AppError> {
    let response = service.list_user_workspaces(claims.sub).await?;
    Ok((StatusCode::OK, Json(response)))
}

/// GET /workspaces/:id
///
/// Get workspace details. Requires VIEWER+ role.
pub async fn get_workspace_detail_handler(
    Extension(service): Extension<Arc<WorkspaceService>>,
    Extension(ws_ctx): Extension<WorkspaceContext>,
    Path(workspace_id): Path<Uuid>,
) -> Result<impl IntoResponse, AppError> {
    check_permission(&ws_ctx, PERM_SNIPPETS_READ)?;
    let response = service.get_workspace(workspace_id).await?;
    Ok((StatusCode::OK, Json(response)))
}

/// PATCH /workspaces/:id
///
/// Update workspace settings. Requires ADMIN+ role.
pub async fn update_workspace_handler(
    Extension(service): Extension<Arc<WorkspaceService>>,
    Extension(registry): Extension<Arc<SessionRegistry>>,
    Extension(ws_ctx): Extension<WorkspaceContext>,
    Extension(claims): Extension<AccessTokenClaims>,
    Path(workspace_id): Path<Uuid>,
    Json(body): Json<UpdateWorkspaceRequest>,
) -> Result<impl IntoResponse, AppError> {
    check_permission(&ws_ctx, PERM_WORKSPACE_SETTINGS)?;
    let response = service.update_workspace(workspace_id, body.name).await?;

    // Broadcast workspace updated to all members
    registry.broadcast_to_workspace(
        workspace_id,
        serde_json::json!({
            "type": "notification",
            "event": "workspace_updated",
            "workspace_id": workspace_id.to_string(),
            "workspace_name": response.name,
            "user_id": claims.sub.to_string(),
            "timestamp": chrono::Utc::now().to_rfc3339(),
        }),
        None,
    );

    Ok((StatusCode::OK, Json(response)))
}

/// DELETE /workspaces/:id
///
/// Delete a workspace. Requires OWNER role. Cannot delete individual workspaces.
pub async fn delete_workspace_detail_handler(
    Extension(service): Extension<Arc<WorkspaceService>>,
    Extension(registry): Extension<Arc<SessionRegistry>>,
    Extension(ws_ctx): Extension<WorkspaceContext>,
    Extension(claims): Extension<AccessTokenClaims>,
    Path(workspace_id): Path<Uuid>,
) -> Result<impl IntoResponse, AppError> {
    check_permission(&ws_ctx, PERM_WORKSPACE_DELETE)?;

    // Broadcast before delete (so members are still connected)
    registry.broadcast_to_workspace(
        workspace_id,
        serde_json::json!({
            "type": "notification",
            "event": "workspace_deleted",
            "workspace_id": workspace_id.to_string(),
            "user_id": claims.sub.to_string(),
            "timestamp": chrono::Utc::now().to_rfc3339(),
        }),
        None,
    );

    service.delete_workspace(workspace_id).await?;
    Ok(StatusCode::NO_CONTENT)
}

// ─── User Lookup Handler ────────────────────────────────────────────────────────

/// Query params for user lookup.
#[derive(Debug, Deserialize)]
pub struct LookupUsersQuery {
    pub ids: String, // comma-separated UUIDs
}

/// Minimal user info for display purposes.
#[derive(Debug, serde::Serialize, sqlx::FromRow)]
pub struct UserLookupItem {
    pub id: Uuid,
    pub email: String,
    pub first_name: Option<String>,
    pub last_name: Option<String>,
}

/// GET /users/lookup?ids=uuid1,uuid2,...
///
/// Batch lookup users by IDs. Returns minimal info (email, name).
/// Used by the frontend to display owner names in folders/snippets.
pub async fn lookup_users_handler(
    Extension(pool): Extension<PgPool>,
    Extension(_claims): Extension<AccessTokenClaims>,
    Query(query): Query<LookupUsersQuery>,
) -> Result<impl IntoResponse, AppError> {
    let ids: Vec<Uuid> = query.ids.split(',')
        .filter_map(|s| Uuid::parse_str(s.trim()).ok())
        .collect();

    if ids.is_empty() {
        return Ok((StatusCode::OK, Json(Vec::<UserLookupItem>::new())));
    }

    // Limit to 50 IDs per request
    let ids = &ids[..ids.len().min(50)];

    let users = sqlx::query_as::<_, UserLookupItem>(
        "SELECT id, email, first_name, last_name FROM users WHERE id = ANY($1)"
    )
    .bind(ids)
    .fetch_all(&pool)
    .await
    .map_err(|_| AppError::InternalError)?;

    Ok((StatusCode::OK, Json(users)))
}

// ─── Permission Management Handlers ─────────────────────────────────────────────

/// Request body for updating a member's permissions.
#[derive(Debug, Deserialize)]
pub struct UpdatePermissionsRequest {
    pub permissions: Vec<String>,
}

/// Response for permission operations.
#[derive(Debug, serde::Serialize)]
pub struct MemberPermissionsResponse {
    pub workspace_id: Uuid,
    pub user_id: Uuid,
    pub role: String,
    pub permissions: Option<Vec<String>>,
}

/// GET /teams/:workspace_id/members/:user_id/permissions
///
/// Get a member's current permissions (custom or role-based defaults).
pub async fn get_member_permissions_handler(
    Extension(pool): Extension<PgPool>,
    Extension(ws_ctx): Extension<WorkspaceContext>,
    Path((workspace_id, user_id)): Path<(Uuid, Uuid)>,
) -> Result<impl IntoResponse, AppError> {
    // Any member can view permissions (needed for UI)
    check_permission(&ws_ctx, PERM_SNIPPETS_READ)?;

    let row: Option<(String, Option<serde_json::Value>)> = sqlx::query_as(
        "SELECT role, permissions FROM workspace_members WHERE workspace_id = $1 AND user_id = $2",
    )
    .bind(workspace_id)
    .bind(user_id)
    .fetch_optional(&pool)
    .await
    .map_err(|_| AppError::InternalError)?;

    let (role, perms_json) = row.ok_or(AppError::UserNotFound)?;

    let permissions: Option<Vec<String>> = perms_json
        .and_then(|v| serde_json::from_value(v).ok());

    Ok((StatusCode::OK, Json(MemberPermissionsResponse {
        workspace_id,
        user_id,
        role,
        permissions,
    })))
}

/// PATCH /teams/:workspace_id/members/:user_id/permissions
///
/// Update a member's custom permissions. Only owner/admin (or users with
/// `members:update_permissions` permission) can modify another member's permissions.
///
/// Setting permissions to an empty array removes all permissions (locked out).
/// To revert to role-based defaults, use DELETE /teams/:workspace_id/members/:user_id/permissions.
pub async fn update_member_permissions_handler(
    Extension(pool): Extension<PgPool>,
    Extension(ws_ctx): Extension<WorkspaceContext>,
    Extension(claims): Extension<AccessTokenClaims>,
    Path((workspace_id, user_id)): Path<(Uuid, Uuid)>,
    Json(body): Json<UpdatePermissionsRequest>,
) -> Result<impl IntoResponse, AppError> {
    use crate::middleware::permissions::{validate_permissions, PERM_MEMBERS_UPDATE_PERMISSIONS};

    // Must have permission to update others' permissions
    check_permission(&ws_ctx, PERM_MEMBERS_UPDATE_PERMISSIONS)?;

    // Cannot modify own permissions (prevent self-escalation)
    if user_id == claims.sub {
        return Err(AppError::Forbidden);
    }

    // Validate all permission strings are known
    validate_permissions(&body.permissions)?;

    // Verify target user is a member
    let exists: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM workspace_members WHERE workspace_id = $1 AND user_id = $2)",
    )
    .bind(workspace_id)
    .bind(user_id)
    .fetch_one(&pool)
    .await
    .map_err(|_| AppError::InternalError)?;

    if !exists {
        return Err(AppError::UserNotFound);
    }

    // Cannot modify owner's permissions
    let target_role: String = sqlx::query_scalar(
        "SELECT role FROM workspace_members WHERE workspace_id = $1 AND user_id = $2",
    )
    .bind(workspace_id)
    .bind(user_id)
    .fetch_one(&pool)
    .await
    .map_err(|_| AppError::InternalError)?;

    if target_role == "owner" {
        return Err(AppError::Forbidden);
    }

    // Update permissions
    let perms_json = serde_json::to_value(&body.permissions)
        .map_err(|_| AppError::InternalError)?;

    sqlx::query(
        "UPDATE workspace_members SET permissions = $1 WHERE workspace_id = $2 AND user_id = $3",
    )
    .bind(&perms_json)
    .bind(workspace_id)
    .bind(user_id)
    .execute(&pool)
    .await
    .map_err(|_| AppError::InternalError)?;

    Ok((StatusCode::OK, Json(MemberPermissionsResponse {
        workspace_id,
        user_id,
        role: target_role,
        permissions: Some(body.permissions),
    })))
}

/// DELETE /teams/:workspace_id/members/:user_id/permissions
///
/// Reset a member's permissions to role-based defaults (removes custom permissions).
pub async fn reset_member_permissions_handler(
    Extension(pool): Extension<PgPool>,
    Extension(ws_ctx): Extension<WorkspaceContext>,
    Extension(claims): Extension<AccessTokenClaims>,
    Path((workspace_id, user_id)): Path<(Uuid, Uuid)>,
) -> Result<impl IntoResponse, AppError> {
    use crate::middleware::permissions::PERM_MEMBERS_UPDATE_PERMISSIONS;

    check_permission(&ws_ctx, PERM_MEMBERS_UPDATE_PERMISSIONS)?;

    if user_id == claims.sub {
        return Err(AppError::Forbidden);
    }

    sqlx::query(
        "UPDATE workspace_members SET permissions = NULL WHERE workspace_id = $1 AND user_id = $2",
    )
    .bind(workspace_id)
    .bind(user_id)
    .execute(&pool)
    .await
    .map_err(|_| AppError::InternalError)?;

    Ok((StatusCode::OK, Json(serde_json::json!({
        "status": "reset",
        "message": "Permissions reset to role-based defaults"
    }))))
}
