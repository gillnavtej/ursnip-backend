//! HTTP handlers for the notifications REST API.

use std::sync::Arc;

use axum::{
    extract::{Extension, Path, Query},
    http::StatusCode,
    response::IntoResponse,
    Json,
};
use uuid::Uuid;

use crate::errors::AppError;
use crate::middleware::auth_extractor::AccessTokenClaims;
use crate::notifications::service::{ListNotificationsQuery, NotificationService};

/// GET /notifications
///
/// List notifications for the authenticated user.
/// Query params: ?page=1&per_page=30&unread_only=true
pub async fn list_notifications_handler(
    Extension(service): Extension<Arc<NotificationService>>,
    Extension(claims): Extension<AccessTokenClaims>,
    Query(query): Query<ListNotificationsQuery>,
) -> Result<impl IntoResponse, AppError> {
    let response = service.list(claims.sub, &query).await?;
    Ok((StatusCode::OK, Json(response)))
}

/// PATCH /notifications/:id/read
///
/// Mark a single notification as read.
pub async fn mark_read_handler(
    Extension(service): Extension<Arc<NotificationService>>,
    Extension(claims): Extension<AccessTokenClaims>,
    Path(notification_id): Path<Uuid>,
) -> Result<impl IntoResponse, AppError> {
    service.mark_read(claims.sub, notification_id).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// PATCH /notifications/read-all
///
/// Mark all notifications as read for the authenticated user.
pub async fn mark_all_read_handler(
    Extension(service): Extension<Arc<NotificationService>>,
    Extension(claims): Extension<AccessTokenClaims>,
) -> Result<impl IntoResponse, AppError> {
    let count = service.mark_all_read(claims.sub).await?;
    Ok((StatusCode::OK, Json(serde_json::json!({ "marked": count }))))
}
