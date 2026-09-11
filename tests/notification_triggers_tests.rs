//! Integration tests for triggered / non-blocking notification behavior (task 10.8).
//!
//! These assert the two guarantees the notification triggers must uphold:
//!
//!   (a) Each event trigger *creates* the expected System_Notification(s):
//!       - Workspace invite → sender + receiver notifications (Req 21.1, 21.2)
//!       - Subscription activation → owner notification            (Req 22.1)
//!       - Device register / remove → owner notification           (Req 24.1)
//!
//!   (b) The subscription-activation and device register/remove operations still
//!       *succeed* even when the notification write fails — a notification
//!       failure must never fail or roll back the parent operation
//!       (Req 22.2, 24.2).
//!
//! ## How the triggers work (and why the tests poll)
//!
//! Every trigger emits its notification through a fire-and-forget `tokio::spawn`
//! that runs *after* the primary operation has committed. This is the structural
//! source of the non-blocking guarantee: the parent function returns `Ok`
//! immediately and the notification write happens on a detached task. Because the
//! write is asynchronous, the creation assertions poll the `notifications` table
//! for a bounded window rather than reading it once.
//!
//! ## How failure is injected (Req 22.2, 24.2)
//!
//! To prove the operation survives a notification-write failure we install a
//! `BEFORE INSERT` trigger on the `notifications` table that raises an exception.
//! This fails *only* the notification INSERT — the `devices` / `subscriptions`
//! writes are untouched — so we can assert the primary operation still returns
//! `Ok` and its row persisted, while no notification row was created. The trigger
//! is dropped again before the test returns.
//!
//! Like the other DB-backed suites in this repo these tests are `#[ignore]`d by
//! default and require a live PostgreSQL instance with migrations applied.
//!
//! Run with:
//!   cargo test --test notification_triggers_tests -- --ignored
//! (requires DATABASE_URL pointing to a test database).
//!
//! Requirements covered: 21.1, 21.2, 22.1, 22.2, 24.1, 24.2

use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use sqlx::PgPool;
use uuid::Uuid;

use ursnip_backend::collaborators::CollaboratorService;
use ursnip_backend::config::{AppConfig, EmailProviderType};
use ursnip_backend::devices::service::{DeviceService, RegisterDeviceRequest};
use ursnip_backend::subscription::{process_webhook, WebhookPayload};

// ─── Harness ─────────────────────────────────────────────────────────────────

async fn setup_db() -> PgPool {
    dotenvy::dotenv().ok();
    let database_url =
        std::env::var("DATABASE_URL").expect("DATABASE_URL must be set for integration tests");
    let pool = PgPool::connect(&database_url)
        .await
        .expect("Failed to connect to test database");
    sqlx::migrate!("./migrations")
        .run(&pool)
        .await
        .expect("Failed to run migrations");
    pool
}

fn test_config() -> Arc<AppConfig> {
    Arc::new(AppConfig {
        database_url: std::env::var("DATABASE_URL").unwrap_or_default(),
        jwt_secret: "test-jwt-secret-for-integration-tests-min-32-chars".to_string(),
        google_client_id: "x".to_string(),
        google_client_secret: "x".to_string(),
        github_client_id: "x".to_string(),
        github_client_secret: "x".to_string(),
        oauth_redirect_base_url: "http://localhost:3000".to_string(),
        billing_webhook_secret: "x".to_string(),
        email_from_address: "test@example.com".to_string(),
        seed_admin_email: "admin@example.com".to_string(),
        seed_admin_password: "adminpass123".to_string(),
        email_provider: EmailProviderType::Smtp,
        email_smtp_host: Some("localhost".to_string()),
        email_smtp_port: Some(587),
        email_smtp_user: Some("user".to_string()),
        email_smtp_password: Some("pass".to_string()),
        email_api_key: None,
        email_api_url: None,
        email_from_name: "Ursnip Test".to_string(),
        port: 8080,
        log_level: "info".to_string(),
        database_max_connections: 5,
        database_min_connections: 1,
        database_connect_timeout_secs: 5,
        database_idle_timeout_secs: 300,
        database_statement_timeout_secs: 30,
        cors_allowed_origins: vec![],
        trusted_proxy_cidrs: vec![],
        ws_max_connections: 100,
        shutdown_timeout_secs: 5,
        billing_success_url: None,
        billing_cancel_url: None,
        paddle_api_key: None,
        paddle_environment: "sandbox".to_string(),
        frontend_url: "http://localhost:3000".to_string(),
    })
}

// A tag prefix unique to this suite, keeping seeded emails/referral codes from
// colliding with other integration suites that share the test database.
const TAG: &str = "notiftrig";

async fn mk_user(pool: &PgPool, label: &str) -> (Uuid, String) {
    let id = Uuid::new_v4();
    let email = format!("{}_{}_{}@example.com", TAG, label, &id.to_string()[..8]);
    let referral = format!("R{}{}", TAG.chars().next().unwrap(), &id.to_string()[..8]);
    sqlx::query(
        r#"INSERT INTO users (id, email, password_hash, role, status, referral_code)
           VALUES ($1, $2, '$argon2id$v=19$m=65536,t=3,p=4$hash', 'user', 'active', $3)"#,
    )
    .bind(id)
    .bind(&email)
    .bind(&referral)
    .execute(pool)
    .await
    .expect("create user");
    (id, email)
}

/// Create an individual workspace owned by `owner` plus a subscription row.
async fn mk_workspace(pool: &PgPool, owner: Uuid, tier: &str, status: &str) -> Uuid {
    let ws = Uuid::new_v4();
    sqlx::query(
        r#"INSERT INTO workspaces (id, type, owner_id, name)
           VALUES ($1, 'individual', $2, 'WS')"#,
    )
    .bind(ws)
    .bind(owner)
    .execute(pool)
    .await
    .expect("create workspace");
    sqlx::query("INSERT INTO subscriptions (workspace_id, tier, status) VALUES ($1, $2, $3)")
        .bind(ws)
        .bind(tier)
        .bind(status)
        .execute(pool)
        .await
        .expect("create subscription");
    ws
}

fn collab_service(pool: &PgPool) -> CollaboratorService {
    CollaboratorService::new(pool.clone(), test_config())
}

// ─── Notification polling / assertions ────────────────────────────────────────

/// Poll for a notification matching `user_id` + `event_type`, up to a bounded
/// number of short intervals. Returns the number of matching rows once it
/// becomes non-zero, or 0 if none appears within the window.
///
/// The triggers emit on a spawned task after the primary op commits, so the row
/// is not guaranteed to exist the instant the call returns. Polling gives the
/// detached write time to land without making the test flaky-sensitive to exact
/// timing. The window (~5s) comfortably covers a local INSERT.
async fn await_notification_count(pool: &PgPool, user_id: Uuid, event_type: &str) -> i64 {
    for _ in 0..50 {
        let count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM notifications WHERE user_id = $1 AND event_type = $2",
        )
        .bind(user_id)
        .bind(event_type)
        .fetch_one(pool)
        .await
        .expect("count notifications");
        if count > 0 {
            return count;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    0
}

/// Count notifications for a user + event_type without waiting (point-in-time).
async fn notification_count(pool: &PgPool, user_id: Uuid, event_type: &str) -> i64 {
    sqlx::query_scalar("SELECT COUNT(*) FROM notifications WHERE user_id = $1 AND event_type = $2")
        .bind(user_id)
        .bind(event_type)
        .fetch_one(pool)
        .await
        .expect("count notifications")
}

/// Install a `BEFORE INSERT` trigger on `notifications` that always raises, so
/// that every notification INSERT fails while leaving all other tables writable.
/// Used to prove the parent operation survives a notification-write failure.
async fn install_failing_notification_trigger(pool: &PgPool) {
    sqlx::query(
        r#"
        CREATE OR REPLACE FUNCTION notiftrig_fail_notifications()
        RETURNS trigger AS $$
        BEGIN
            RAISE EXCEPTION 'notiftrig: forced notification write failure';
        END;
        $$ LANGUAGE plpgsql;
        "#,
    )
    .execute(pool)
    .await
    .expect("create failing trigger fn");

    sqlx::query(
        r#"
        CREATE TRIGGER notiftrig_before_insert
        BEFORE INSERT ON notifications
        FOR EACH ROW EXECUTE FUNCTION notiftrig_fail_notifications();
        "#,
    )
    .execute(pool)
    .await
    .expect("create failing trigger");
}

/// Remove the failing trigger and its function, restoring normal writes.
async fn remove_failing_notification_trigger(pool: &PgPool) {
    let _ = sqlx::query("DROP TRIGGER IF EXISTS notiftrig_before_insert ON notifications")
        .execute(pool)
        .await;
    let _ = sqlx::query("DROP FUNCTION IF EXISTS notiftrig_fail_notifications()")
        .execute(pool)
        .await;
}

/// Best-effort teardown of everything a test seeded.
async fn cleanup(pool: &PgPool, user_ids: &[Uuid]) {
    // Ensure the failure trigger is never left behind even if a test aborts.
    remove_failing_notification_trigger(pool).await;

    let _ = sqlx::query("DELETE FROM notifications WHERE user_id = ANY($1)")
        .bind(user_ids)
        .execute(pool)
        .await;
    for tbl in ["collaborations", "collaborator_invites"] {
        for id in user_ids {
            let _ = sqlx::query(&format!(
                "DELETE FROM {tbl} WHERE owner_id = $1 OR collaborator_id = $1"
            ))
            .bind(id)
            .execute(pool)
            .await;
        }
    }
    let _ = sqlx::query(
        "DELETE FROM devices WHERE workspace_id IN (SELECT id FROM workspaces WHERE owner_id = ANY($1))",
    )
    .bind(user_ids)
    .execute(pool)
    .await;
    let _ = sqlx::query(
        "DELETE FROM billing_events WHERE workspace_id IN (SELECT id FROM workspaces WHERE owner_id = ANY($1))",
    )
    .bind(user_ids)
    .execute(pool)
    .await;
    let _ = sqlx::query(
        "DELETE FROM transactions WHERE workspace_id IN (SELECT id FROM workspaces WHERE owner_id = ANY($1))",
    )
    .bind(user_ids)
    .execute(pool)
    .await;
    let _ = sqlx::query(
        "DELETE FROM subscriptions WHERE workspace_id IN (SELECT id FROM workspaces WHERE owner_id = ANY($1))",
    )
    .bind(user_ids)
    .execute(pool)
    .await;
    let _ = sqlx::query("DELETE FROM workspaces WHERE owner_id = ANY($1)")
        .bind(user_ids)
        .execute(pool)
        .await;
    let _ = sqlx::query("DELETE FROM users WHERE id = ANY($1)")
        .bind(user_ids)
        .execute(pool)
        .await;
}

// ═══════════════════════════════════════════════════════════════════════════
// (a) Triggers create the expected notification(s)
// ═══════════════════════════════════════════════════════════════════════════

/// Req 21.1 / 21.2: sending a workspace invite to an *existing* user creates a
/// System_Notification for both the sender and the receiver.
#[tokio::test]
#[ignore]
async fn invite_creates_sender_and_receiver_notifications() {
    let pool = setup_db().await;
    let (owner, _) = mk_user(&pool, "inviteowner").await;
    // Teams tier so the invite is allowed; owner needs an individual workspace
    // to anchor the notifications (see CollaboratorService::emit_invite_notifications).
    let _ws = mk_workspace(&pool, owner, "teams", "active").await;
    let (receiver, receiver_email) = mk_user(&pool, "invitee").await;

    let svc = collab_service(&pool);
    svc.invite(owner, &receiver_email).await.expect("invite ok");

    // Sender notification (Req 21.1).
    let sender_count = await_notification_count(&pool, owner, "workspace_invite_sent").await;
    assert!(
        sender_count >= 1,
        "sender should get a workspace_invite_sent notification (Req 21.1)"
    );

    // Receiver notification (Req 21.2) — the invited email resolves to a user.
    let receiver_count =
        await_notification_count(&pool, receiver, "workspace_invite_received").await;
    assert!(
        receiver_count >= 1,
        "existing invitee should get a workspace_invite_received notification (Req 21.2)"
    );

    cleanup(&pool, &[owner, receiver]).await;
}

/// Req 22.1: activating a paid-tier subscription creates a System_Notification
/// for the workspace owner. Driven through the real `process_webhook`
/// `subscription.activated` path, which emits the notification on a spawned task.
#[tokio::test]
#[ignore]
async fn subscription_activation_creates_owner_notification() {
    let pool = setup_db().await;
    let (owner, _) = mk_user(&pool, "subowner").await;
    // Start pending_payment on a pro tier; the webhook transitions it to active.
    let ws = mk_workspace(&pool, owner, "pro", "pending_payment").await;

    let payload = WebhookPayload {
        event_id: format!("{}_evt_{}", TAG, Uuid::new_v4()),
        event_type: "subscription.activated".to_string(),
        workspace_id: Some(ws),
        external_subscription_id: Some(format!("{}_ext_{}", TAG, Uuid::new_v4())),
        timestamp: Utc::now(),
        data: serde_json::json!({}),
    };

    process_webhook(&pool, payload)
        .await
        .expect("webhook activation should succeed");

    // The subscription was actually activated.
    let status: String =
        sqlx::query_scalar("SELECT status FROM subscriptions WHERE workspace_id = $1")
            .bind(ws)
            .fetch_one(&pool)
            .await
            .expect("read subscription status");
    assert_eq!(status, "active", "subscription should be active after webhook");

    // Owner received the activation notification (Req 22.1).
    let count = await_notification_count(&pool, owner, "subscription_activated").await;
    assert!(
        count >= 1,
        "owner should get a subscription_activated notification (Req 22.1)"
    );

    cleanup(&pool, &[owner]).await;
}

/// Req 24.1: registering and then removing a device each create a
/// System_Notification for the owning user.
#[tokio::test]
#[ignore]
async fn device_register_and_remove_create_notifications() {
    let pool = setup_db().await;
    let (owner, _) = mk_user(&pool, "devowner").await;
    let ws = mk_workspace(&pool, owner, "pro", "active").await;
    let svc = DeviceService::new(pool.clone());

    let device = svc
        .register_device(
            owner,
            RegisterDeviceRequest {
                workspace_id: ws,
                device_name: "Laptop".to_string(),
                device_type: Some("desktop".to_string()),
                platform: Some("macos".to_string()),
            },
        )
        .await
        .expect("register device ok");

    let registered = await_notification_count(&pool, owner, "device_registered").await;
    assert!(
        registered >= 1,
        "registering a device should create a device_registered notification (Req 24.1)"
    );

    svc.delete_device(device.id, owner)
        .await
        .expect("delete device ok");

    let removed = await_notification_count(&pool, owner, "device_removed").await;
    assert!(
        removed >= 1,
        "removing a device should create a device_removed notification (Req 24.1)"
    );

    cleanup(&pool, &[owner]).await;
}

// ═══════════════════════════════════════════════════════════════════════════
// (b) Primary operations succeed even when the notification write fails
// ═══════════════════════════════════════════════════════════════════════════

/// Req 22.2: if the notification write fails during subscription activation, the
/// activation still completes and is not rolled back.
#[tokio::test]
#[ignore]
async fn subscription_activation_succeeds_when_notification_write_fails() {
    let pool = setup_db().await;
    let (owner, _) = mk_user(&pool, "subfail").await;
    let ws = mk_workspace(&pool, owner, "pro", "pending_payment").await;

    // Force every notification INSERT to fail for the duration of the op.
    install_failing_notification_trigger(&pool).await;

    let payload = WebhookPayload {
        event_id: format!("{}_evtfail_{}", TAG, Uuid::new_v4()),
        event_type: "subscription.activated".to_string(),
        workspace_id: Some(ws),
        external_subscription_id: Some(format!("{}_extfail_{}", TAG, Uuid::new_v4())),
        timestamp: Utc::now(),
        data: serde_json::json!({}),
    };

    // The activation must still succeed despite the notification failure.
    let result = process_webhook(&pool, payload).await;
    assert!(
        result.is_ok(),
        "activation must not fail when the notification write fails (Req 22.2): {result:?}"
    );

    // And it must be durably committed (not rolled back).
    let status: String =
        sqlx::query_scalar("SELECT status FROM subscriptions WHERE workspace_id = $1")
            .bind(ws)
            .fetch_one(&pool)
            .await
            .expect("read subscription status");
    assert_eq!(
        status, "active",
        "subscription activation must be committed even though the notification write failed"
    );

    // Give the detached notification task time to run and fail; no row persisted.
    tokio::time::sleep(Duration::from_millis(300)).await;
    remove_failing_notification_trigger(&pool).await;
    let count = notification_count(&pool, owner, "subscription_activated").await;
    assert_eq!(
        count, 0,
        "no notification should exist when its write was forced to fail"
    );

    cleanup(&pool, &[owner]).await;
}

/// Req 24.2: if the notification write fails during device registration, the
/// device is still registered (and likewise a device removal still completes).
#[tokio::test]
#[ignore]
async fn device_operations_succeed_when_notification_write_fails() {
    let pool = setup_db().await;
    let (owner, _) = mk_user(&pool, "devfail").await;
    let ws = mk_workspace(&pool, owner, "pro", "active").await;
    let svc = DeviceService::new(pool.clone());

    // Force every notification INSERT to fail.
    install_failing_notification_trigger(&pool).await;

    // Register must succeed despite the notification write failing (Req 24.2).
    let device = svc
        .register_device(
            owner,
            RegisterDeviceRequest {
                workspace_id: ws,
                device_name: "Phone".to_string(),
                device_type: Some("mobile".to_string()),
                platform: Some("android".to_string()),
            },
        )
        .await
        .expect("device registration must succeed even when notification write fails (Req 24.2)");

    // The device row is durably persisted.
    let exists: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM devices WHERE id = $1")
        .bind(device.id)
        .fetch_one(&pool)
        .await
        .expect("count devices");
    assert_eq!(exists, 1, "the registered device must persist");

    // Removal must also succeed despite the notification write failing.
    svc.delete_device(device.id, owner)
        .await
        .expect("device removal must succeed even when notification write fails (Req 24.2)");

    let after_delete: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM devices WHERE id = $1")
        .bind(device.id)
        .fetch_one(&pool)
        .await
        .expect("count devices after delete");
    assert_eq!(after_delete, 0, "the device must be removed");

    // Let the detached notification tasks run and fail; assert none persisted.
    tokio::time::sleep(Duration::from_millis(300)).await;
    remove_failing_notification_trigger(&pool).await;
    assert_eq!(
        notification_count(&pool, owner, "device_registered").await,
        0,
        "no device_registered notification should exist when its write was forced to fail"
    );
    assert_eq!(
        notification_count(&pool, owner, "device_removed").await,
        0,
        "no device_removed notification should exist when its write was forced to fail"
    );

    cleanup(&pool, &[owner]).await;
}
