//! HTTP handlers for device management endpoints.

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
use crate::middleware::workspace_context::{check_role_viewer, WorkspaceContext};
use crate::sync::session_registry::SessionRegistry;
use crate::devices::service::{DeviceService, RegisterDeviceRequest, UpdateDeviceRequest};

/// Query params for listing devices.
#[derive(Debug, Deserialize)]
pub struct ListDevicesParams {
    pub workspace_id: Uuid,
}

/// GET /devices?workspace_id=...
///
/// List all devices for the authenticated user in a workspace.
pub async fn list_devices_handler(
    Extension(service): Extension<Arc<DeviceService>>,
    Extension(claims): Extension<AccessTokenClaims>,
    Extension(ws_ctx): Extension<WorkspaceContext>,
    Query(params): Query<ListDevicesParams>,
) -> Result<impl IntoResponse, AppError> {
    check_role_viewer(&ws_ctx)?;
    let devices = service.list_devices(params.workspace_id, claims.sub).await?;
    Ok((StatusCode::OK, Json(devices)))
}

/// POST /devices
///
/// Register a new device. Plan device limit enforced.
pub async fn register_device_handler(
    Extension(service): Extension<Arc<DeviceService>>,
    Extension(registry): Extension<Arc<SessionRegistry>>,
    Extension(claims): Extension<AccessTokenClaims>,
    Json(body): Json<RegisterDeviceRequest>,
) -> Result<impl IntoResponse, AppError> {
    let workspace_id = body.workspace_id;
    let device = service.register_device(claims.sub, body).await?;

    // Broadcast device registered notification
    registry.broadcast_to_workspace(
        workspace_id,
        serde_json::json!({
            "type": "notification",
            "event": "device_registered",
            "device_name": device.device_name,
            "device_id": device.id.to_string(),
            "user_id": claims.sub.to_string(),
            "workspace_id": workspace_id.to_string(),
            "timestamp": chrono::Utc::now().to_rfc3339(),
        }),
        None,
    );

    Ok((StatusCode::CREATED, Json(device)))
}

/// PATCH /devices/:id
///
/// Update a device (name, sync version, active status).
pub async fn update_device_handler(
    Extension(service): Extension<Arc<DeviceService>>,
    Extension(claims): Extension<AccessTokenClaims>,
    Path(device_id): Path<Uuid>,
    Json(body): Json<UpdateDeviceRequest>,
) -> Result<impl IntoResponse, AppError> {
    let device = service.update_device(device_id, claims.sub, body).await?;
    Ok((StatusCode::OK, Json(device)))
}

/// DELETE /devices/:id
///
/// Disconnect (delete) a device.
pub async fn delete_device_handler(
    Extension(service): Extension<Arc<DeviceService>>,
    Extension(registry): Extension<Arc<SessionRegistry>>,
    Extension(claims): Extension<AccessTokenClaims>,
    Path(device_id): Path<Uuid>,
) -> Result<impl IntoResponse, AppError> {
    service.delete_device(device_id, claims.sub).await?;

    // Broadcast device disconnected notification (to all user's workspaces via any connected WS)
    // Note: We don't have workspace_id here, so we skip workspace-specific broadcast
    // The user will see it on their next page load

    Ok(StatusCode::NO_CONTENT)
}
