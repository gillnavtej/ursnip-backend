//! Property-based test for notification classification (Property 21).
//!
//! Exercises the real `NotificationService::create` path against a live
//! PostgreSQL database: for any notification created by the service (across
//! varying creation inputs — event types, titles, descriptions, metadata, and
//! the classification input including the defaulted `None` case), the persisted
//! notification's `classification` column SHALL be exactly one of `system` or
//! `promotional`.
//!
//! Like the other DB-backed suites in this repo (`preferences_property_tests`,
//! `notifications_migration_tests`), this test is `#[ignore]`d by default and
//! requires a live database. Bring one up and point `DATABASE_URL` at it, e.g.:
//!
//! ```text
//! docker compose -f docker-compose.test.yml up -d
//! DATABASE_URL=postgres://ursnip_test:ursnip_test@localhost:5433/ursnip_test \
//!   cargo test --test notification_classification_property_tests -- --ignored
//! ```
//!
//! Requirements covered: 25.2

use proptest::prelude::*;
use proptest::test_runner::Config as ProptestConfig;
use sqlx::PgPool;
use tokio::runtime::Runtime;
use uuid::Uuid;

use ursnip_backend::notifications::preferences::{
    NotificationPreferencesService, UpdateNotificationPreferences,
};
use ursnip_backend::notifications::service::{CreateNotification, NotificationService};

// ─── Harness ─────────────────────────────────────────────────────────────────

/// Connect to the test database and apply migrations (idempotent).
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

/// Insert a throwaway user so the `notifications.user_id` FK (REFERENCES
/// users(id)) is satisfied. Returns the new user id.
async fn seed_user(pool: &PgPool) -> Uuid {
    let user_id = Uuid::new_v4();
    let short = &user_id.to_string()[..8];
    sqlx::query(
        r#"INSERT INTO users (id, email, password_hash, role, status, referral_code)
           VALUES ($1, $2, '$argon2id$v=19$m=65536,t=3,p=4$hash', 'user', 'active', $3)"#,
    )
    .bind(user_id)
    .bind(format!("notif_class_prop_{short}@example.com"))
    .bind(format!("REF{short}"))
    .execute(pool)
    .await
    .expect("Failed to seed test user");
    user_id
}

/// Insert a throwaway user plus an individual workspace they own so the
/// `notifications` table's FKs (`user_id` → users, `workspace_id` → workspaces)
/// are satisfied. Returns `(user_id, workspace_id)`.
async fn seed_user_and_workspace(pool: &PgPool) -> (Uuid, Uuid) {
    let user_id = seed_user(pool).await;
    let workspace_id = Uuid::new_v4();
    sqlx::query(
        r#"INSERT INTO workspaces (id, type, owner_id, name)
           VALUES ($1, 'individual', $2, 'Notif Class Prop WS')"#,
    )
    .bind(workspace_id)
    .bind(user_id)
    .execute(pool)
    .await
    .expect("Failed to seed test workspace");
    (user_id, workspace_id)
}

/// Remove a throwaway user + workspace seeded for this test, cleaning up any
/// notifications/preferences first to be robust across environments.
async fn cleanup_user_and_workspace(pool: &PgPool, user_id: Uuid, workspace_id: Uuid) {
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

// ─── Strategies ────────────────────────────────────────────────────────────────

/// The classification inputs the service accepts as a valid classification:
///   - `None`           → the service defaults it to `system`.
///   - `Some("system")` → an explicit system notification.
///   - `Some("promotional")` → an explicit promotional notification.
///
/// Every one of these must persist a row whose `classification` column is
/// exactly one of `system` or `promotional` (Property 21).
fn classification_input_strategy() -> impl Strategy<Value = Option<String>> {
    prop_oneof![
        Just(None),
        Just(Some("system".to_string())),
        Just(Some("promotional".to_string())),
    ]
}

/// A varied set of event types spanning the concrete triggers the service emits
/// (invite/subscription/device/expiry/promotional), so the property is checked
/// across many creation paths rather than a single event.
fn event_type_strategy() -> impl Strategy<Value = String> {
    prop_oneof![
        Just("workspace_invite".to_string()),
        Just("subscription_activated".to_string()),
        Just("device_registered".to_string()),
        Just("device_removed".to_string()),
        Just("plan_expiry".to_string()),
        Just("snippet_shared".to_string()),
        Just("promo_broadcast".to_string()),
    ]
}

/// Arbitrary non-empty title/description text.
fn text_strategy() -> impl Strategy<Value = String> {
    "[a-zA-Z0-9 ]{1,40}"
}

// ─── Property 21: Every notification is classified ───────────────────────────
//
// Feature: ursnip-feature-matrix, Property 21: Every notification is classified
//
// For any notification created by the Notification_Service, its classification
// SHALL be exactly one of `system` or `promotional`.
//
// This drives the REAL `NotificationService::create` path against a live
// database: create a notification with varying inputs, read the persisted
// `classification` column back, and assert it is exactly one of the two allowed
// values. Preferences are set to all-enabled so no case is suppressed by the
// gating logic (that is Property 20/23's concern; here we assert the
// classification of every notification that IS created).
//
// **Validates: Requirements 25.2**

#[test]
#[ignore]
fn prop_every_notification_is_classified() {
    // A single shared runtime + pool is reused across all proptest iterations so
    // we don't reconnect / re-run migrations per case.
    let rt = Runtime::new().expect("failed to build tokio runtime");
    let pool = rt.block_on(setup_test_db());
    let prefs_service = NotificationPreferencesService::new(pool.clone());
    let notif_service = NotificationService::new(pool.clone());

    let config = ProptestConfig {
        cases: 100,
        ..ProptestConfig::default()
    };

    let mut runner = proptest::test_runner::TestRunner::new(config);

    let strategy = (
        classification_input_strategy(),
        event_type_strategy(),
        text_strategy(),
        prop::option::of(text_strategy()),
        any::<bool>(),
    );

    runner
        .run(
            &strategy,
            |(classification_input, event_type, title, description, with_metadata)| {
                rt.block_on(async {
                    let (user_id, workspace_id) = seed_user_and_workspace(&pool).await;

                    // All-enabled preferences so nothing is suppressed by the
                    // gating logic — we want the notification to actually persist
                    // so we can inspect its classification.
                    prefs_service
                        .upsert(
                            user_id,
                            UpdateNotificationPreferences {
                                system_enabled: true,
                                promotional_enabled: true,
                                security_system_enabled: true,
                            },
                        )
                        .await
                        .expect("upserting preferences must succeed");

                    let metadata = if with_metadata {
                        Some(serde_json::json!({ "k": "v", "n": 1 }))
                    } else {
                        None
                    };

                    // Create via the real service across a varied creation path.
                    notif_service
                        .create(CreateNotification {
                            user_id,
                            workspace_id,
                            event_type: event_type.clone(),
                            title: title.clone(),
                            description: description.clone(),
                            metadata,
                            classification: classification_input.clone(),
                        })
                        .await
                        .expect("create must succeed");

                    // Read back every persisted classification for this user.
                    let persisted: Vec<String> = sqlx::query_scalar(
                        "SELECT classification FROM notifications WHERE user_id = $1",
                    )
                    .bind(user_id)
                    .fetch_all(&pool)
                    .await
                    .expect("querying persisted notifications must succeed");

                    // Clean up before asserting so a failing case leaks nothing.
                    cleanup_user_and_workspace(&pool, user_id, workspace_id).await;

                    // Exactly one notification should have been created.
                    prop_assert_eq!(
                        persisted.len(),
                        1,
                        "exactly one notification should be created for input {:?} (persisted: {:?})",
                        classification_input,
                        persisted
                    );

                    // Every persisted classification must be EXACTLY one of the two
                    // allowed values — never empty, null-like, or anything else.
                    for c in &persisted {
                        prop_assert!(
                            c == "system" || c == "promotional",
                            "persisted classification must be exactly `system` or `promotional`, \
                             got `{}` for classification input {:?} / event `{}`",
                            c,
                            classification_input,
                            event_type
                        );
                    }

                    Ok(())
                })
            },
        )
        .expect("Property 21: every notification is classified failed");
}
