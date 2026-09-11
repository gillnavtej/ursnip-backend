//! Notification persistence service.
//!
//! Inserts notification rows and queries them for the REST API.

use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use uuid::Uuid;

use crate::errors::AppError;

// ─── Retry policy (mirrors scheduler::service, Requirement 20.3) ─────────────────

/// Retry backoff durations for transient notification-write failures.
/// Mirrors the scheduler's exponential backoff: 1s → 5s → 30s.
const RETRY_DELAYS: [Duration; 3] = [
    Duration::from_secs(1),
    Duration::from_secs(5),
    Duration::from_secs(30),
];

/// Maximum number of write attempts.
const MAX_RETRIES: usize = 3;

/// Classify whether an error is transient (eligible for retry).
///
/// Mirrors the scheduler's classification: DatabaseTimeout, ServiceUnavailable,
/// and InternalError are treated as transient (DB connection/timeout issues).
/// All other errors are non-transient and are surfaced immediately.
fn is_transient(err: &AppError) -> bool {
    matches!(
        err,
        AppError::DatabaseTimeout | AppError::ServiceUnavailable | AppError::InternalError
    )
}

/// Map a sqlx database error to an appropriate AppError.
///
/// Connection/timeout/IO errors → DatabaseTimeout (transient, will be retried).
/// Other errors → InternalError (transient, will be retried).
/// Mirrors `scheduler::service::SchedulerService::map_db_error`.
fn map_db_error(err: &sqlx::Error) -> AppError {
    match err {
        sqlx::Error::PoolTimedOut | sqlx::Error::PoolClosed => AppError::DatabaseTimeout,
        sqlx::Error::Io(_) => AppError::DatabaseTimeout,
        _ => AppError::InternalError,
    }
}

/// Execute a fallible async DB write with bounded retry on transient failures.
///
/// Retries up to `MAX_RETRIES` attempts with exponential backoff (1s → 5s → 30s),
/// mirroring the scheduler's policy. Transient errors are retried; non-transient
/// errors are surfaced immediately. After exhausting all attempts the final error
/// is logged and returned (never discarded silently).
async fn write_with_retry<F, Fut>(op_name: &str, mut op: F) -> Result<(), AppError>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<(), AppError>>,
{
    let mut last_err: Option<AppError> = None;

    for attempt in 0..MAX_RETRIES {
        match op().await {
            Ok(()) => return Ok(()),
            Err(err) if is_transient(&err) => {
                if attempt < MAX_RETRIES - 1 {
                    let delay = RETRY_DELAYS[attempt];
                    tracing::warn!(
                        op = op_name,
                        attempt = attempt + 1,
                        max_retries = MAX_RETRIES,
                        delay_secs = delay.as_secs(),
                        error = %err,
                        "Notification write failed with transient error, retrying"
                    );
                    last_err = Some(err);
                    tokio::time::sleep(delay).await;
                } else {
                    tracing::error!(
                        op = op_name,
                        attempts = MAX_RETRIES,
                        error = %err,
                        "Notification write failed after all retry attempts"
                    );
                    return Err(err);
                }
            }
            Err(err) => {
                // Non-transient failure — surface immediately, do not retry.
                tracing::error!(
                    op = op_name,
                    error = %err,
                    "Notification write failed with non-transient error (no retry)"
                );
                return Err(err);
            }
        }
    }

    // Unreachable in practice (loop returns on the final attempt), but surface
    // the last transient error rather than discarding it silently.
    Err(last_err.unwrap_or(AppError::InternalError))
}

// ─── Preference gating (Requirements 26.4, 26.5) ─────────────────────────────

/// Determine whether an `event_type` is an expiry or security-relevant device
/// notification.
///
/// These are the two categories that survive a plain System_Notification
/// opt-out (Req 26.5): plan/subscription-expiry reminders and security-relevant
/// device events (register/remove). They are suppressed only when the user has
/// *also* explicitly disabled security-relevant System_Notifications
/// (`security_system_enabled = false`).
///
/// Matching is done by substring so it stays robust across the concrete
/// `event_type` names the triggers emit (e.g. `plan_expiry`,
/// `subscription_expiry`, `device_registered`, `device_removed`). The trigger
/// tasks (10.x) had not landed when this gating was written, so we match on the
/// stable "expiry"/"device" tokens rather than an exhaustive enum.
fn is_expiry_or_security(event_type: &str) -> bool {
    let et = event_type.to_ascii_lowercase();
    et.contains("expiry") || et.contains("device")
}

/// Decide whether a notification should be created for a user given the user's
/// preferences and the notification's classification/event_type.
///
/// Rules (Req 26.4, 26.5):
/// - `promotional`: create only when `promotional_enabled` is true (26.4).
/// - `system`:
///     - when `system_enabled` is true → always create.
///     - when `system_enabled` is false → create only expiry + security-relevant
///       device notifications, and even those only while
///       `security_system_enabled` is true (26.5).
///
/// Any unrecognized classification is treated as `system` (matches the
/// `CreateNotification` default).
fn should_create(
    classification: &str,
    event_type: &str,
    prefs: &crate::notifications::preferences::NotificationPreferences,
) -> bool {
    if classification == "promotional" {
        return prefs.promotional_enabled;
    }

    // Treat everything else (including the "system" default) as a system notification.
    if prefs.system_enabled {
        return true;
    }

    // System notifications are opted out: keep only expiry / security-relevant
    // device notifications, and drop even those if the user explicitly disabled
    // security-relevant system notifications.
    is_expiry_or_security(event_type) && prefs.security_system_enabled
}

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
    pub classification: String,
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
    /// Notification classification: `system` (default) or `promotional`.
    pub classification: Option<String>,
}

/// Query parameters for listing notifications.
#[derive(Debug, Deserialize)]
pub struct ListNotificationsQuery {
    pub page: Option<i64>,
    pub per_page: Option<i64>,
    pub unread_only: Option<bool>,
}

/// Summary of a promotional broadcast fan-out (Requirement 25.1).
#[derive(Debug, Clone, Serialize)]
pub struct BroadcastResult {
    /// Number of targeted users that could receive the notification (i.e. that
    /// resolved to an individual workspace).
    pub targeted: usize,
    /// Number of promotional notifications actually created (targeted users who
    /// had not opted out of promotional notifications).
    pub created: usize,
    /// Number of targeted users skipped because they opted out.
    pub skipped: usize,
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
    ///
    /// Before inserting, the target user's `notification_preferences` are
    /// consulted and the notification is silently skipped when the user has
    /// opted out (Requirements 26.4, 26.5). A skip returns `Ok(())` — the
    /// notification is intentionally not created, which is not an error.
    ///
    /// The insert itself is wrapped in a bounded retry loop (mirroring the
    /// scheduler's 3-attempt exponential backoff). Transient DB failures are
    /// retried; the failure is surfaced (returned as `Err`) and logged rather
    /// than discarded silently (Requirement 20.3). The preference check happens
    /// BEFORE entering the retry/insert path.
    ///
    /// Note: per-recipient preference gating is applied only here (the
    /// single-user path). The bulk `notify_workspace_members` fan-out does not
    /// apply per-recipient gating (out of scope for this path).
    pub async fn create(&self, params: CreateNotification) -> Result<(), AppError> {
        let pool = self.pool.clone();
        let metadata = params.metadata.unwrap_or(serde_json::json!({}));
        let classification = params
            .classification
            .unwrap_or_else(|| "system".to_string());

        // Preference gating (Req 26.4, 26.5). Absent row = all-enabled defaults.
        let prefs = crate::notifications::preferences::NotificationPreferencesService::new(
            pool.clone(),
        )
        .get(params.user_id)
        .await?;

        if !should_create(&classification, &params.event_type, &prefs) {
            tracing::debug!(
                user_id = %params.user_id,
                event_type = %params.event_type,
                classification = %classification,
                "Notification suppressed by user notification_preferences"
            );
            return Ok(());
        }

        write_with_retry("notifications.create", || {
            let pool = pool.clone();
            let event_type = params.event_type.clone();
            let title = params.title.clone();
            let description = params.description.clone();
            let metadata = metadata.clone();
            let classification = classification.clone();
            async move {
                sqlx::query(
                    r#"
                    INSERT INTO notifications (user_id, workspace_id, event_type, title, description, metadata, classification)
                    VALUES ($1, $2, $3, $4, $5, $6, $7)
                    "#,
                )
                .bind(params.user_id)
                .bind(params.workspace_id)
                .bind(&event_type)
                .bind(&title)
                .bind(&description)
                .bind(metadata)
                .bind(&classification)
                .execute(&pool)
                .await
                .map(|_| ())
                .map_err(|e| map_db_error(&e))
            }
        })
        .await
    }

    /// Fan a promotional notification out to a set of targeted users
    /// (Requirement 25.1).
    ///
    /// For each targeted user the notification is classified `promotional` and
    /// routed through the single-user [`create`](Self::create) path, so the
    /// per-user preference gating applies automatically: a user who has opted
    /// out of promotional notifications (`promotional_enabled = false`) is
    /// skipped, while a user with no persisted preferences row is treated as
    /// opted-in (all-enabled defaults). This satisfies "create a
    /// Promotional_Notification for each targeted User who has not opted out".
    ///
    /// Each notification is attached to the recipient's own `individual`
    /// workspace (the `notifications.workspace_id` column is `NOT NULL` with an
    /// FK to `workspaces`). Users without an individual workspace, and users
    /// who have opted out, contribute nothing — they are counted as skipped.
    ///
    /// Targeting:
    /// - When `user_ids` is `Some`, only those users are targeted.
    /// - When `user_ids` is `None`, every active, non-deleted user is targeted.
    ///
    /// Returns a [`BroadcastResult`] summarizing how many notifications were
    /// created versus skipped. A failure creating any single notification is
    /// surfaced (the fan-out is not silently partial).
    pub async fn broadcast_promotional(
        &self,
        title: &str,
        description: Option<&str>,
        metadata: Option<serde_json::Value>,
        user_ids: Option<&[Uuid]>,
    ) -> Result<BroadcastResult, AppError> {
        // Resolve (user_id, individual workspace_id) pairs for the target set.
        // Only users that own an `individual` workspace can receive a
        // notification, since workspace_id is NOT NULL.
        let targets: Vec<(Uuid, Uuid)> = match user_ids {
            Some(ids) => {
                if ids.is_empty() {
                    return Ok(BroadcastResult {
                        targeted: 0,
                        created: 0,
                        skipped: 0,
                    });
                }
                sqlx::query_as::<_, (Uuid, Uuid)>(
                    r#"
                    SELECT u.id, w.id
                    FROM users u
                    JOIN workspaces w ON w.owner_id = u.id AND w.type = 'individual'
                    WHERE u.id = ANY($1) AND u.deleted_at IS NULL
                    "#,
                )
                .bind(ids)
                .fetch_all(&self.pool)
                .await
                .map_err(|e| {
                    tracing::error!("Failed to resolve broadcast targets: {e}");
                    AppError::InternalError
                })?
            }
            None => sqlx::query_as::<_, (Uuid, Uuid)>(
                r#"
                SELECT u.id, w.id
                FROM users u
                JOIN workspaces w ON w.owner_id = u.id AND w.type = 'individual'
                WHERE u.deleted_at IS NULL AND u.status = 'active'
                "#,
            )
            .fetch_all(&self.pool)
            .await
            .map_err(|e| {
                tracing::error!("Failed to resolve broadcast targets: {e}");
                AppError::InternalError
            })?,
        };

        let targeted = targets.len();
        let mut created = 0usize;

        let prefs_service =
            crate::notifications::preferences::NotificationPreferencesService::new(
                self.pool.clone(),
            );

        for (user_id, workspace_id) in targets {
            // Determine opt-out up front so the count reflects exactly which
            // users had a notification created (created iff not opted out).
            // Absent preferences row = all-enabled defaults (opted in).
            let opted_in = prefs_service.get(user_id).await?.promotional_enabled;

            // Route through the single-user `create` path, which re-applies the
            // same preference gating before inserting. Opted-out users are
            // skipped (Ok with no insert); this keeps a single source of truth
            // for the gating rule.
            self.create(CreateNotification {
                user_id,
                workspace_id,
                event_type: "promotional_broadcast".to_string(),
                title: title.to_string(),
                description: description.map(str::to_string),
                metadata: metadata.clone(),
                classification: Some("promotional".to_string()),
            })
            .await?;

            if opted_in {
                created += 1;
            }
        }

        Ok(BroadcastResult {
            targeted,
            created,
            skipped: targeted - created,
        })
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
        let pool = self.pool.clone();

        write_with_retry("notifications.notify_workspace_members", || {
            let pool = pool.clone();
            let meta = meta.clone();
            async move {
                sqlx::query(
                    r#"
                    INSERT INTO notifications (user_id, workspace_id, event_type, title, description, metadata, classification)
                    SELECT wm.user_id, $1, $2, $3, $4, $5, 'system'
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
                .execute(&pool)
                .await
                .map(|_| ())
                .map_err(|e| map_db_error(&e))
            }
        })
        .await
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

/// Helper: persist a single-user notification in a background task.
///
/// Fire-and-forget — spawns the write so the caller is never blocked and a
/// notification failure never fails the parent operation (Requirements 22.2,
/// 24.2). Routing through `NotificationService::create` means per-recipient
/// preference gating (Req 26.4, 26.5) and the bounded write-retry (Req 20.3)
/// are applied; a final failure is logged rather than surfaced.
pub fn persist_user_notification_async(
    service: Arc<NotificationService>,
    user_id: Uuid,
    workspace_id: Uuid,
    event_type: String,
    title: String,
    description: Option<String>,
    metadata: Option<serde_json::Value>,
) {
    tokio::spawn(async move {
        let params = CreateNotification {
            user_id,
            workspace_id,
            event_type,
            title,
            description,
            metadata,
            classification: Some("system".to_string()),
        };
        if let Err(e) = service.create(params).await {
            tracing::error!(error = %e, "Failed to persist user notification");
        }
    });
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

#[cfg(test)]
mod retry_tests {
    //! Unit tests for the bounded retry-then-surface behavior (Requirement 20.3).
    //!
    //! These exercise `write_with_retry` directly with in-memory closures (no
    //! live database), covering the two required scenarios:
    //!   1. transient failure then success — the retry loop retries and succeeds.
    //!   2. persistent transient failure — after the bound is exhausted the
    //!      error is surfaced (returned as `Err`) rather than silently discarded.
    //!
    //! `start_paused = true` makes tokio auto-advance the timer, so the real
    //! backoff sleeps (1s → 5s → 30s) complete instantly under test.

    use super::{write_with_retry, MAX_RETRIES};
    use crate::errors::AppError;
    use std::cell::Cell;

    /// Transient failure then success: the operation fails transiently on the
    /// first attempt(s) then succeeds. The retry loop must retry and return Ok,
    /// having invoked the operation exactly `fail_times + 1` times.
    #[tokio::test(start_paused = true)]
    async fn transient_failure_then_success_retries_and_succeeds() {
        // Fail once (transient) then succeed on the second attempt.
        let fail_times = 1usize;
        let attempts = Cell::new(0usize);

        let result = write_with_retry("test.transient_then_success", || {
            let n = attempts.get() + 1;
            attempts.set(n);
            async move {
                if n <= fail_times {
                    // DatabaseTimeout is classified transient → eligible for retry.
                    Err(AppError::DatabaseTimeout)
                } else {
                    Ok(())
                }
            }
        })
        .await;

        assert!(
            result.is_ok(),
            "retry loop should eventually succeed after a transient failure, got {result:?}"
        );
        assert_eq!(
            attempts.get(),
            fail_times + 1,
            "operation should be retried exactly once then succeed (2 total invocations)"
        );
    }

    /// Transient failure on the first two attempts then success on the third
    /// (the last allowed attempt). Confirms the loop uses its full budget when
    /// needed and still succeeds without exceeding the bound.
    #[tokio::test(start_paused = true)]
    async fn transient_failures_up_to_bound_then_success() {
        let fail_times = MAX_RETRIES - 1; // 2 failures, succeed on the 3rd attempt
        let attempts = Cell::new(0usize);

        let result = write_with_retry("test.transient_up_to_bound", || {
            let n = attempts.get() + 1;
            attempts.set(n);
            async move {
                if n <= fail_times {
                    Err(AppError::ServiceUnavailable)
                } else {
                    Ok(())
                }
            }
        })
        .await;

        assert!(result.is_ok(), "should succeed on the final allowed attempt");
        assert_eq!(
            attempts.get(),
            MAX_RETRIES,
            "operation should be invoked exactly MAX_RETRIES times"
        );
    }

    /// Persistent transient failure: the operation always fails with a transient
    /// error. The retry loop must exhaust exactly `MAX_RETRIES` attempts (not
    /// loop forever) and surface the error as `Err` rather than discarding it.
    #[tokio::test(start_paused = true)]
    async fn persistent_failure_surfaces_after_bound() {
        let attempts = Cell::new(0usize);

        let result = write_with_retry("test.persistent_failure", || {
            attempts.set(attempts.get() + 1);
            async move { Err::<(), AppError>(AppError::DatabaseTimeout) }
        })
        .await;

        // Failure is surfaced, not silently discarded.
        assert!(
            result.is_err(),
            "persistent transient failure must be surfaced as Err"
        );
        assert!(
            matches!(result, Err(AppError::DatabaseTimeout)),
            "the surfaced error should be the last transient error, got {result:?}"
        );
        // Bounded: retried exactly MAX_RETRIES times, never infinitely.
        assert_eq!(
            attempts.get(),
            MAX_RETRIES,
            "operation should be attempted exactly MAX_RETRIES times before surfacing"
        );
    }

    /// Non-transient failure: a non-transient error must be surfaced immediately
    /// on the first attempt without consuming the retry budget.
    #[tokio::test(start_paused = true)]
    async fn non_transient_failure_surfaces_immediately_without_retry() {
        let attempts = Cell::new(0usize);

        let result = write_with_retry("test.non_transient", || {
            attempts.set(attempts.get() + 1);
            // RecipientNotFound is not classified transient.
            async move { Err::<(), AppError>(AppError::RecipientNotFound) }
        })
        .await;

        assert!(matches!(result, Err(AppError::RecipientNotFound)));
        assert_eq!(
            attempts.get(),
            1,
            "non-transient error should not be retried (single invocation)"
        );
    }
}

#[cfg(test)]
mod gating_tests {
    use super::{is_expiry_or_security, should_create};
    use crate::notifications::preferences::NotificationPreferences;
    use chrono::Utc;
    use uuid::Uuid;

    /// Build a preferences record with explicit toggles.
    fn prefs(system: bool, promotional: bool, security: bool) -> NotificationPreferences {
        let now = Utc::now();
        NotificationPreferences {
            user_id: Uuid::new_v4(),
            system_enabled: system,
            promotional_enabled: promotional,
            security_system_enabled: security,
            created_at: now,
            updated_at: now,
        }
    }

    #[test]
    fn expiry_and_device_events_are_expiry_or_security() {
        assert!(is_expiry_or_security("plan_expiry"));
        assert!(is_expiry_or_security("subscription_expiry"));
        assert!(is_expiry_or_security("device_registered"));
        assert!(is_expiry_or_security("device_removed"));
        // Case-insensitive.
        assert!(is_expiry_or_security("Plan_Expiry"));
        // Unrelated system events are not expiry/security.
        assert!(!is_expiry_or_security("workspace_invite"));
        assert!(!is_expiry_or_security("snippet_created"));
    }

    #[test]
    fn promotional_respects_opt_out() {
        // Req 26.4: promotional created only when promotional_enabled.
        assert!(should_create(
            "promotional",
            "promo_broadcast",
            &prefs(true, true, true)
        ));
        assert!(!should_create(
            "promotional",
            "promo_broadcast",
            &prefs(true, false, true)
        ));
        // system_enabled state is irrelevant for promotional.
        assert!(should_create(
            "promotional",
            "promo_broadcast",
            &prefs(false, true, false)
        ));
    }

    #[test]
    fn system_enabled_always_creates_system() {
        assert!(should_create(
            "system",
            "workspace_invite",
            &prefs(true, false, true)
        ));
        assert!(should_create(
            "system",
            "plan_expiry",
            &prefs(true, true, false)
        ));
    }

    #[test]
    fn system_opt_out_preserves_expiry_and_security() {
        // Req 26.5: system opted out but security still on → expiry/device kept.
        assert!(should_create(
            "system",
            "plan_expiry",
            &prefs(false, true, true)
        ));
        assert!(should_create(
            "system",
            "device_removed",
            &prefs(false, true, true)
        ));
        // Non-expiry/non-device system notifications are suppressed.
        assert!(!should_create(
            "system",
            "workspace_invite",
            &prefs(false, true, true)
        ));
    }

    #[test]
    fn system_opt_out_with_security_disabled_suppresses_everything_system() {
        // Req 26.5 tail: security disabled → even expiry/device suppressed.
        assert!(!should_create(
            "system",
            "plan_expiry",
            &prefs(false, true, false)
        ));
        assert!(!should_create(
            "system",
            "device_registered",
            &prefs(false, true, false)
        ));
    }

    #[test]
    fn unknown_classification_treated_as_system() {
        // Defaults to system semantics.
        assert!(should_create("", "workspace_invite", &prefs(true, false, false)));
        assert!(!should_create(
            "",
            "workspace_invite",
            &prefs(false, false, false)
        ));
    }
}
