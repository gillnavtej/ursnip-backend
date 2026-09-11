//! Per-user notification preferences: model, service, and HTTP handlers.
//!
//! Preferences are persisted server-side (table `notification_preferences`) so
//! they follow the user across devices. An absent row is treated as all
//! defaults (everything enabled). Updates are performed via upsert
//! (`INSERT ... ON CONFLICT (user_id) DO UPDATE`).

use std::sync::Arc;

use axum::{extract::Extension, http::StatusCode, response::IntoResponse, Json};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use uuid::Uuid;

use crate::errors::AppError;
use crate::middleware::auth_extractor::AccessTokenClaims;

/// A persisted notification-preferences record.
///
/// Maps to the `notification_preferences` table. `created_at`/`updated_at` are
/// included for `FromRow` mapping against the table columns.
#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct NotificationPreferences {
    pub user_id: Uuid,
    pub system_enabled: bool,
    pub promotional_enabled: bool,
    pub security_system_enabled: bool,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl NotificationPreferences {
    /// Build the all-enabled defaults for a user that has no persisted row yet.
    fn defaults(user_id: Uuid) -> Self {
        let now = Utc::now();
        Self {
            user_id,
            system_enabled: true,
            promotional_enabled: true,
            security_system_enabled: true,
            created_at: now,
            updated_at: now,
        }
    }
}

/// Request body for updating notification preferences.
#[derive(Debug, Clone, Deserialize)]
pub struct UpdateNotificationPreferences {
    pub system_enabled: bool,
    pub promotional_enabled: bool,
    pub security_system_enabled: bool,
}

/// Service handling notification-preferences persistence.
#[derive(Clone)]
pub struct NotificationPreferencesService {
    pool: PgPool,
}

impl NotificationPreferencesService {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Get preferences for a user, returning all-enabled defaults when no row exists.
    pub async fn get(&self, user_id: Uuid) -> Result<NotificationPreferences, AppError> {
        let row = sqlx::query_as::<_, NotificationPreferences>(
            r#"
            SELECT user_id, system_enabled, promotional_enabled, security_system_enabled,
                   created_at, updated_at
            FROM notification_preferences
            WHERE user_id = $1
            "#,
        )
        .bind(user_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| {
            tracing::error!("Failed to fetch notification preferences: {e}");
            AppError::InternalError
        })?;

        Ok(row.unwrap_or_else(|| NotificationPreferences::defaults(user_id)))
    }

    /// Upsert preferences for a user and return the resulting row.
    pub async fn upsert(
        &self,
        user_id: Uuid,
        params: UpdateNotificationPreferences,
    ) -> Result<NotificationPreferences, AppError> {
        let row = sqlx::query_as::<_, NotificationPreferences>(
            r#"
            INSERT INTO notification_preferences
                (user_id, system_enabled, promotional_enabled, security_system_enabled, updated_at)
            VALUES ($1, $2, $3, $4, now())
            ON CONFLICT (user_id) DO UPDATE SET
                system_enabled = EXCLUDED.system_enabled,
                promotional_enabled = EXCLUDED.promotional_enabled,
                security_system_enabled = EXCLUDED.security_system_enabled,
                updated_at = now()
            RETURNING user_id, system_enabled, promotional_enabled, security_system_enabled,
                      created_at, updated_at
            "#,
        )
        .bind(user_id)
        .bind(params.system_enabled)
        .bind(params.promotional_enabled)
        .bind(params.security_system_enabled)
        .fetch_one(&self.pool)
        .await
        .map_err(|e| {
            tracing::error!("Failed to upsert notification preferences: {e}");
            AppError::InternalError
        })?;

        Ok(row)
    }
}

/// GET /notifications/preferences
///
/// Return the authenticated user's notification preferences, or all-enabled
/// defaults when no row exists.
pub async fn get_preferences_handler(
    Extension(service): Extension<Arc<NotificationPreferencesService>>,
    Extension(claims): Extension<AccessTokenClaims>,
) -> Result<impl IntoResponse, AppError> {
    let prefs = service.get(claims.sub).await?;
    Ok((StatusCode::OK, Json(prefs)))
}

/// PUT /notifications/preferences
///
/// Upsert the authenticated user's notification preferences and return the
/// resulting record.
pub async fn update_preferences_handler(
    Extension(service): Extension<Arc<NotificationPreferencesService>>,
    Extension(claims): Extension<AccessTokenClaims>,
    Json(body): Json<UpdateNotificationPreferences>,
) -> Result<impl IntoResponse, AppError> {
    let prefs = service.upsert(claims.sub, body).await?;
    Ok((StatusCode::OK, Json(prefs)))
}
