//! Integration tests for the promotional broadcast fan-out (Task 10.9,
//! Requirement 25.1).
//!
//! Exercises the real `NotificationService::broadcast_promotional` path against
//! a live PostgreSQL database. Requirement 25.1: issuing a promotional message
//! SHALL create a Promotional_Notification for each targeted User who has not
//! opted out of promotional notifications.
//!
//! Like the other DB-backed suites in this repo, this test is `#[ignore]`d by
//! default and requires a live database:
//!
//! ```text
//! docker compose -f docker-compose.test.yml up -d
//! DATABASE_URL=postgres://ursnip_test:ursnip_test@localhost:5433/ursnip_test \
//!   cargo test --test promotional_broadcast_tests -- --ignored
//! ```
//!
//! Requirements covered: 25.1

use sqlx::PgPool;
use uuid::Uuid;

use ursnip_backend::notifications::preferences::{
    NotificationPreferencesService, UpdateNotificationPreferences,
};
use ursnip_backend::notifications::service::NotificationService;

// ─── Harness ─────────────────────────────────────────────────────────────────

async fn setup_test_db() -> PgPool {
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

/// Seed a user plus the individual workspace they own (the workspace a
/// promotional broadcast attaches the notification to). Returns
/// `(user_id, workspace_id)`.
async fn seed_user_and_workspace(pool: &PgPool, active: bool) -> (Uuid, Uuid) {
    let user_id = Uuid::new_v4();
    let short = &user_id.to_string()[..8];
    let status = if active { "active" } else { "suspended" };
    sqlx::query(
        r#"INSERT INTO users (id, email, password_hash, role, status, referral_code)
           VALUES ($1, $2, '$argon2id$v=19$m=65536,t=3,p=4$hash', 'user', $3, $4)"#,
    )
    .bind(user_id)
    .bind(format!("promo_bcast_{short}@example.com"))
    .bind(status)
    .bind(format!("REF{short}"))
    .execute(pool)
    .await
    .expect("Failed to seed test user");

    let workspace_id = Uuid::new_v4();
    sqlx::query(
        r#"INSERT INTO workspaces (id, type, owner_id, name)
           VALUES ($1, 'individual', $2, 'Promo Broadcast WS')"#,
    )
    .bind(workspace_id)
    .bind(user_id)
    .execute(pool)
    .await
    .expect("Failed to seed test workspace");

    (user_id, workspace_id)
}

async fn set_promotional_pref(pool: &PgPool, user_id: Uuid, promotional_enabled: bool) {
    NotificationPreferencesService::new(pool.clone())
        .upsert(
            user_id,
            UpdateNotificationPreferences {
                system_enabled: true,
                promotional_enabled,
                security_system_enabled: true,
            },
        )
        .await
        .expect("upserting preferences must succeed");
}

async fn promotional_count(pool: &PgPool, user_id: Uuid) -> i64 {
    sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM notifications WHERE user_id = $1 AND classification = 'promotional'",
    )
    .bind(user_id)
    .fetch_one(pool)
    .await
    .expect("count query must succeed")
}

async fn cleanup(pool: &PgPool, user_id: Uuid, workspace_id: Uuid) {
    let _ = sqlx::query("DELETE FROM notifications WHERE user_id = $1")
        .bind(user_id)
        .execute(pool)
        .await;
    let _ = sqlx::query("DELETE FROM notification_preferences WHERE user_id = $1")
        .bind(user_id)
        .execute(pool)
        .await;
    let _ = sqlx::query("DELETE FROM workspaces WHERE id = $1")
        .bind(workspace_id)
        .execute(pool)
        .await;
    let _ = sqlx::query("DELETE FROM users WHERE id = $1")
        .bind(user_id)
        .execute(pool)
        .await;
}

// ─── Tests ───────────────────────────────────────────────────────────────────

/// Req 25.1: an explicit-target broadcast creates a promotional notification
/// for each targeted user who has NOT opted out, and skips those who have.
#[tokio::test]
#[ignore]
async fn broadcast_creates_for_opted_in_and_skips_opted_out() {
    let pool = setup_test_db().await;
    let service = NotificationService::new(pool.clone());

    // opted_in has default (all-enabled) prefs; opted_out explicitly disables.
    let (opted_in, ws_in) = seed_user_and_workspace(&pool, true).await;
    let (opted_out, ws_out) = seed_user_and_workspace(&pool, true).await;
    set_promotional_pref(&pool, opted_in, true).await;
    set_promotional_pref(&pool, opted_out, false).await;

    let result = service
        .broadcast_promotional(
            "New feature available",
            Some("Check out our latest update"),
            None,
            Some(&[opted_in, opted_out]),
        )
        .await
        .expect("broadcast must succeed");

    let created_in = promotional_count(&pool, opted_in).await;
    let created_out = promotional_count(&pool, opted_out).await;

    cleanup(&pool, opted_in, ws_in).await;
    cleanup(&pool, opted_out, ws_out).await;

    assert_eq!(result.targeted, 2, "both users resolve to a workspace");
    assert_eq!(result.created, 1, "only the opted-in user gets one");
    assert_eq!(result.skipped, 1, "the opted-out user is skipped");
    assert_eq!(created_in, 1, "opted-in user has exactly one promo notification");
    assert_eq!(created_out, 0, "opted-out user has none");
}

/// Req 25.1: a user with no persisted preferences row is treated as opted-in
/// (all-enabled defaults) and receives the promotional notification.
#[tokio::test]
#[ignore]
async fn broadcast_defaults_absent_prefs_to_opted_in() {
    let pool = setup_test_db().await;
    let service = NotificationService::new(pool.clone());

    // No preferences row is inserted for this user.
    let (user_id, ws) = seed_user_and_workspace(&pool, true).await;

    let result = service
        .broadcast_promotional("Sale offer", None, None, Some(&[user_id]))
        .await
        .expect("broadcast must succeed");

    let created = promotional_count(&pool, user_id).await;
    cleanup(&pool, user_id, ws).await;

    assert_eq!(result.created, 1, "absent prefs row defaults to opted-in");
    assert_eq!(created, 1);
}

/// An empty explicit target list creates nothing.
#[tokio::test]
#[ignore]
async fn broadcast_empty_target_list_creates_nothing() {
    let pool = setup_test_db().await;
    let service = NotificationService::new(pool.clone());

    let result = service
        .broadcast_promotional("Announcement", None, None, Some(&[]))
        .await
        .expect("broadcast must succeed");

    assert_eq!(result.targeted, 0);
    assert_eq!(result.created, 0);
    assert_eq!(result.skipped, 0);
}
