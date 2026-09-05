//! Notification persistence service.
//!
//! Inserts notification rows and queries them for the REST API.

use std::sync::Arc;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use uuid::Uuid;

use crate::errors::AppError;

/// A persisted notification record.
#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct Notification {
    pub id: Uuid,
    pub user_id: Uuid,
    pub workspace_id: Uuid,
    pub event_type: String,
    pub title: String,
    pub description: Option<String>,
    pub metadata: serde_json::Value,
    pub is_read: bool,
    pub created_at: DateTime<Utc>,
}

/// Parameters for creating a notification.
#[derive(Debug, Clone)]
pub struct CreateNotification {
    pub user_id: Uuid,
    pub workspace_id: Uuid,
    pub event_type: String,
    pub title: String,
    pub description: Option<String>,
    pub metadata: Option<serde_json::Value>,
}

/// Query parameters for listing notifications.
#[derive(Debug, Deserialize)]
pub struct ListNotificationsQuery {
    pub page: Option<i64>,
    pub per_page: Option<i64>,
    pub unread_only: Option<bool>,
}

/// Paginated response.
#[derive(Debug, Serialize)]
pub struct NotificationListResponse {
    pub data: Vec<Notification>,
    pub total: i64,
    pub unread_count: i64,
    pub page: i64,
    pub per_page: i64,
}

#[derive(Clone)]
pub struct NotificationService {
    pool: PgPool,
}

impl NotificationService {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Insert a single notification.
    pub async fn create(&self, params: CreateNotification) -> Result<(), AppError> {
        sqlx::query(
            r#"
            INSERT INTO notifications (user_id, workspace_id, event_type, title, description, metadata)
            VALUES ($1, $2, $3, $4, $5, $6)
            "#,
        )
        .bind(params.user_id)
        .bind(params.workspace_id)
        .bind(&params.event_type)
        .bind(&params.title)
        .bind(&params.description)
        .bind(params.metadata.unwrap_or(serde_json::json!({})))
        .execute(&self.pool)
        .await
        .map_err(|e| {
            tracing::error!("Failed to insert notification: {e}");
            AppError::InternalError
        })?;

        Ok(())
    }

    /// Insert notifications for all members of a workspace (except the actor).
    pub async fn notify_workspace_members(
        &self,
        workspace_id: Uuid,
        actor_id: Uuid,
        event_type: &str,
        title: &str,
        description: Option<&str>,
        metadata: Option<serde_json::Value>,
    ) -> Result<(), AppError> {
        let meta = metadata.unwrap_or(serde_json::json!({}));

        sqlx::query(
            r#"
            INSERT INTO notifications (user_id, workspace_id, event_type, title, description, metadata)
            SELECT wm.user_id, $1, $2, $3, $4, $5
            FROM workspace_members wm
            WHERE wm.workspace_id = $1 AND wm.user_id != $6
            "#,
        )
        .bind(workspace_id)
        .bind(event_type)
        .bind(title)
        .bind(description)
        .bind(&meta)
        .bind(actor_id)
        .execute(&self.pool)
        .await
        .map_err(|e| {
            tracing::error!("Failed to notify workspace members: {e}");
            AppError::InternalError
        })?;

        Ok(())
    }

    /// List notifications for a user (paginated, optionally filtered to unread).
    pub async fn list(
        &self,
        user_id: Uuid,
        query: &ListNotificationsQuery,
    ) -> Result<NotificationListResponse, AppError> {
        let page = query.page.unwrap_or(1).max(1);
        let per_page = query.per_page.unwrap_or(30).min(100);
        let offset = (page - 1) * per_page;
        let unread_only = query.unread_only.unwrap_or(false);

        let (data, total) = if unread_only {
            let rows = sqlx::query_as::<_, Notification>(
                r#"
                SELECT * FROM notifications
                WHERE user_id = $1 AND is_read = false
                ORDER BY created_at DESC
                LIMIT $2 OFFSET $3
                "#,
            )
            .bind(user_id)
            .bind(per_page)
            .bind(offset)
            .fetch_all(&self.pool)
            .await
            .map_err(|_| AppError::InternalError)?;

            let count: Option<i64> = sqlx::query_scalar(
                "SELECT COUNT(*) FROM notifications WHERE user_id = $1 AND is_read = false",
            )
            .bind(user_id)
            .fetch_one(&self.pool)
            .await
            .map_err(|_| AppError::InternalError)?;

            (rows, count.unwrap_or(0))
        } else {
            let rows = sqlx::query_as::<_, Notification>(
                r#"
                SELECT * FROM notifications
                WHERE user_id = $1
                ORDER BY created_at DESC
                LIMIT $2 OFFSET $3
                "#,
            )
            .bind(user_id)
            .bind(per_page)
            .bind(offset)
            .fetch_all(&self.pool)
            .await
            .map_err(|_| AppError::InternalError)?;

            let count: Option<i64> = sqlx::query_scalar(
                "SELECT COUNT(*) FROM notifications WHERE user_id = $1",
            )
            .bind(user_id)
            .fetch_one(&self.pool)
            .await
            .map_err(|_| AppError::InternalError)?;

            (rows, count.unwrap_or(0))
        };

        let unread_count: Option<i64> = sqlx::query_scalar(
            "SELECT COUNT(*) FROM notifications WHERE user_id = $1 AND is_read = false",
        )
        .bind(user_id)
        .fetch_one(&self.pool)
        .await
        .map_err(|_| AppError::InternalError)?;

        Ok(NotificationListResponse {
            data,
            total,
            unread_count: unread_count.unwrap_or(0),
            page,
            per_page,
        })
    }

    /// Mark a single notification as read.
    pub async fn mark_read(&self, user_id: Uuid, notification_id: Uuid) -> Result<(), AppError> {
        let result = sqlx::query(
            "UPDATE notifications SET is_read = true WHERE id = $1 AND user_id = $2",
        )
        .bind(notification_id)
        .bind(user_id)
        .execute(&self.pool)
        .await
        .map_err(|_| AppError::InternalError)?;

        if result.rows_affected() == 0 {
            return Err(AppError::UserNotFound); // reuse 404 variant
        }
        Ok(())
    }

    /// Mark all notifications as read for a user.
    pub async fn mark_all_read(&self, user_id: Uuid) -> Result<u64, AppError> {
        let result = sqlx::query(
            "UPDATE notifications SET is_read = true WHERE user_id = $1 AND is_read = false",
        )
        .bind(user_id)
        .execute(&self.pool)
        .await
        .map_err(|_| AppError::InternalError)?;

        Ok(result.rows_affected())
    }
}

/// Helper: persist a notification for all workspace members (except actor) in a background task.
/// This is fire-and-forget — does not block the request.
pub fn persist_notification_async(
    service: Arc<NotificationService>,
    workspace_id: Uuid,
    actor_id: Uuid,
    event_type: String,
    title: String,
    description: Option<String>,
    metadata: Option<serde_json::Value>,
) {
    tokio::spawn(async move {
        if let Err(e) = service
            .notify_workspace_members(
                workspace_id,
                actor_id,
                &event_type,
                &title,
                description.as_deref(),
                metadata,
            )
            .await
        {
            tracing::error!("Failed to persist notification: {:?}", e);
        }
    });
}
