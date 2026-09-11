//! Device management service.
//!
//! Handles CRUD operations for registered devices within a workspace.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use uuid::Uuid;

use crate::errors::AppError;
use crate::notifications::service::{CreateNotification, NotificationService};

// ─── DTOs ───────────────────────────────────────────────────────────────────────

/// Request to register a new device.
#[derive(Debug, Deserialize)]
pub struct RegisterDeviceRequest {
    pub workspace_id: Uuid,
    pub device_name: String,
    pub device_type: Option<String>,
    pub platform: Option<String>,
}

/// Request to update a device.
#[derive(Debug, Deserialize)]
pub struct UpdateDeviceRequest {
    pub device_name: Option<String>,
    pub last_sync_version: Option<i64>,
    pub is_active: Option<bool>,
}

/// Device response.
#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct DeviceResponse {
    pub id: Uuid,
    pub workspace_id: Uuid,
    pub user_id: Uuid,
    pub device_name: String,
    pub device_type: String,
    pub platform: Option<String>,
    pub last_sync_version: i64,
    pub last_sync_at: Option<DateTime<Utc>>,
    pub is_active: bool,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

// ─── Service ────────────────────────────────────────────────────────────────────

/// Maximum devices per user per workspace (free tier).
const FREE_TIER_MAX_DEVICES: i64 = 2;
/// Maximum devices per user per workspace (pro tier).
const PRO_TIER_MAX_DEVICES: i64 = 5;
/// Maximum devices per user per workspace (teams tier — effectively unlimited).
const TEAMS_TIER_MAX_DEVICES: i64 = 100;

pub struct DeviceService {
    pool: PgPool,
}

impl DeviceService {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Emit a System_Notification for a device management event (Requirement 24.1).
    ///
    /// Non-blocking (Requirement 24.2): the notification write is spawned as a
    /// fire-and-forget background task so that a notification failure never
    /// blocks or rolls back the device operation. A fresh `NotificationService`
    /// is built from the pool — its `create` path applies the user's
    /// notification preferences and bounded retry internally. Device events are
    /// treated as security-relevant, so they survive a plain System opt-out.
    fn emit_device_notification(
        &self,
        user_id: Uuid,
        workspace_id: Uuid,
        event_type: &'static str,
        title: &'static str,
        device_id: Uuid,
        device_name: String,
    ) {
        let notifications = NotificationService::new(self.pool.clone());
        let notif = CreateNotification {
            user_id,
            workspace_id,
            event_type: event_type.to_string(),
            title: title.to_string(),
            description: Some(format!("Device \"{device_name}\".")),
            metadata: Some(serde_json::json!({
                "device_id": device_id,
                "device_name": device_name,
            })),
            classification: Some("system".to_string()),
        };
        tokio::spawn(async move {
            if let Err(e) = notifications.create(notif).await {
                tracing::error!(error = %e, event = event_type, "Failed to emit device notification");
            }
        });
    }

    /// List all devices for a user in a workspace.
    pub async fn list_devices(
        &self,
        workspace_id: Uuid,
        user_id: Uuid,
    ) -> Result<Vec<DeviceResponse>, AppError> {
        let devices = sqlx::query_as::<_, DeviceResponse>(
            r#"
            SELECT id, workspace_id, user_id, device_name, device_type, platform,
                   last_sync_version, last_sync_at, is_active, created_at, updated_at
            FROM devices
            WHERE workspace_id = $1 AND user_id = $2
            ORDER BY created_at ASC
            "#,
        )
        .bind(workspace_id)
        .bind(user_id)
        .fetch_all(&self.pool)
        .await
        .map_err(|_| AppError::InternalError)?;

        Ok(devices)
    }

    /// Register a new device.
    ///
    /// Enforces per-workspace device limit based on subscription tier.
    pub async fn register_device(
        &self,
        user_id: Uuid,
        request: RegisterDeviceRequest,
    ) -> Result<DeviceResponse, AppError> {
        // Check device count limit
        let count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM devices WHERE workspace_id = $1 AND user_id = $2",
        )
        .bind(request.workspace_id)
        .bind(user_id)
        .fetch_one(&self.pool)
        .await
        .map_err(|_| AppError::InternalError)?;

        // Get subscription tier to determine limit
        let tier: Option<String> = sqlx::query_scalar(
            "SELECT tier FROM subscriptions WHERE workspace_id = $1",
        )
        .bind(request.workspace_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(|_| AppError::InternalError)?;

        let max_devices = match tier.as_deref() {
            Some("teams") => TEAMS_TIER_MAX_DEVICES,
            Some("pro") => PRO_TIER_MAX_DEVICES,
            _ => FREE_TIER_MAX_DEVICES,
        };

        if count >= max_devices {
            return Err(AppError::SeatLimitReached);
        }

        let device_type = request.device_type.unwrap_or_else(|| "desktop".to_string());
        let platform = request.platform;

        let device = sqlx::query_as::<_, DeviceResponse>(
            r#"
            INSERT INTO devices (workspace_id, user_id, device_name, device_type, platform)
            VALUES ($1, $2, $3, $4, $5)
            RETURNING id, workspace_id, user_id, device_name, device_type, platform,
                      last_sync_version, last_sync_at, is_active, created_at, updated_at
            "#,
        )
        .bind(request.workspace_id)
        .bind(user_id)
        .bind(&request.device_name)
        .bind(&device_type)
        .bind(&platform)
        .fetch_one(&self.pool)
        .await
        .map_err(|_| AppError::InternalError)?;

        // Requirement 24.1/24.2: emit a non-blocking device-registered notification.
        self.emit_device_notification(
            user_id,
            device.workspace_id,
            "device_registered",
            "New device registered",
            device.id,
            device.device_name.clone(),
        );

        Ok(device)
    }

    /// Update a device (name, sync version, active status).
    pub async fn update_device(
        &self,
        device_id: Uuid,
        user_id: Uuid,
        request: UpdateDeviceRequest,
    ) -> Result<DeviceResponse, AppError> {
        // Verify device belongs to user
        let existing = sqlx::query_as::<_, DeviceResponse>(
            r#"
            SELECT id, workspace_id, user_id, device_name, device_type, platform,
                   last_sync_version, last_sync_at, is_active, created_at, updated_at
            FROM devices
            WHERE id = $1 AND user_id = $2
            "#,
        )
        .bind(device_id)
        .bind(user_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(|_| AppError::InternalError)?;

        let existing = existing.ok_or(AppError::WorkspaceNotFound)?;

        let new_name = request.device_name.unwrap_or(existing.device_name);
        let new_version = request.last_sync_version.unwrap_or(existing.last_sync_version);
        let new_active = request.is_active.unwrap_or(existing.is_active);

        // If sync version changed, also update last_sync_at
        let sync_at = if request.last_sync_version.is_some() {
            Some(Utc::now())
        } else {
            existing.last_sync_at
        };

        let device = sqlx::query_as::<_, DeviceResponse>(
            r#"
            UPDATE devices
            SET device_name = $1, last_sync_version = $2, last_sync_at = $3, is_active = $4, updated_at = now()
            WHERE id = $5
            RETURNING id, workspace_id, user_id, device_name, device_type, platform,
                      last_sync_version, last_sync_at, is_active, created_at, updated_at
            "#,
        )
        .bind(&new_name)
        .bind(new_version)
        .bind(sync_at)
        .bind(new_active)
        .bind(device_id)
        .fetch_one(&self.pool)
        .await
        .map_err(|_| AppError::InternalError)?;

        Ok(device)
    }

    /// Delete (disconnect) a device.
    pub async fn delete_device(
        &self,
        device_id: Uuid,
        user_id: Uuid,
    ) -> Result<(), AppError> {
        // Load the device first so we can emit a device-removed notification
        // (Requirement 24.1) with its workspace_id / name after a successful
        // delete. Absence → 404 (reuses WorkspaceNotFound like update_device).
        let existing = sqlx::query_as::<_, DeviceResponse>(
            r#"
            SELECT id, workspace_id, user_id, device_name, device_type, platform,
                   last_sync_version, last_sync_at, is_active, created_at, updated_at
            FROM devices
            WHERE id = $1 AND user_id = $2
            "#,
        )
        .bind(device_id)
        .bind(user_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(|_| AppError::InternalError)?;

        let existing = existing.ok_or(AppError::WorkspaceNotFound)?;

        let result = sqlx::query("DELETE FROM devices WHERE id = $1 AND user_id = $2")
            .bind(device_id)
            .bind(user_id)
            .execute(&self.pool)
            .await
            .map_err(|_| AppError::InternalError)?;

        if result.rows_affected() == 0 {
            return Err(AppError::WorkspaceNotFound);
        }

        // Requirement 24.1/24.2: emit a non-blocking device-removed notification.
        self.emit_device_notification(
            user_id,
            existing.workspace_id,
            "device_removed",
            "Device removed",
            existing.id,
            existing.device_name.clone(),
        );

        Ok(())
    }
}
