//! HTTP handlers for the notifications REST API.

use std::sync::Arc;

use axum::{
    extract::{Extension, Path, Query},
    http::StatusCode,
    response::IntoResponse,
    Json,
};
use uuid::Uuid;

use serde::Deserialize;

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

/// Request body for the promotional broadcast endpoint.
///
/// `title` is required and must be non-empty. `description` and `metadata` are
/// optional. When `user_ids` is present the broadcast is limited to those
/// users; when omitted (or `null`) it targets every active, non-deleted user.
#[derive(Debug, Deserialize)]
pub struct BroadcastPromotionalRequest {
    pub title: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub metadata: Option<serde_json::Value>,
    #[serde(default)]
    pub user_ids: Option<Vec<Uuid>>,
}

/// POST /admin/notifications/promotional
///
/// Admin-only. Fan out a promotional notification to the targeted users, one
/// per targeted user who has not opted out of promotional notifications
/// (Requirement 25.1). Users who opted out are skipped. The response reports
/// how many notifications were created versus skipped.
///
/// This route lives inside the admin route group, which is protected by
/// `admin_guard`, so no additional role check is required here.
pub async fn broadcast_promotional_handler(
    Extension(service): Extension<Arc<NotificationService>>,
    Extension(_claims): Extension<AccessTokenClaims>,
    Json(body): Json<BroadcastPromotionalRequest>,
) -> Result<impl IntoResponse, AppError> {
    let title = body.title.trim();
    if title.is_empty() {
        return Err(AppError::InvalidRequestBody);
    }

    let result = service
        .broadcast_promotional(
            title,
            body.description.as_deref(),
            body.metadata.clone(),
            body.user_ids.as_deref(),
        )
        .await?;

    Ok((StatusCode::OK, Json(result)))
}
