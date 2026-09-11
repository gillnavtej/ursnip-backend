//! Property-based test for notification-preferences persistence (Property 22).
//!
//! Exercises the real `NotificationPreferencesService` against a live PostgreSQL
//! database: for any generated preferences payload, upserting it (PUT-equivalent)
//! and then reading it back (GET-equivalent) must return equal
//! `system_enabled`, `promotional_enabled`, and `security_system_enabled` values.
//!
//! Like the other DB-backed suites in this repo (`sync_integration_tests`,
//! `notifications_migration_tests`), this test is `#[ignore]`d by default and
//! requires a live database. Bring one up with `docker-compose.test.yml` and point
//! `DATABASE_URL` at it, e.g.:
//!
//! ```text
//! docker compose -f docker-compose.test.yml up -d
//! DATABASE_URL=postgres://ursnip_test:ursnip_test@localhost:5433/ursnip_test \
//!   cargo test --test preferences_property_tests -- --ignored
//! ```
//!
//! Requirements covered: 26.1, 26.2

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

/// Insert a throwaway user so the `notification_preferences.user_id` FK
/// (REFERENCES users(id)) is satisfied. Returns the new user id.
async fn seed_user(pool: &PgPool) -> Uuid {
    let user_id = Uuid::new_v4();
    let short = &user_id.to_string()[..8];
    sqlx::query(
        r#"INSERT INTO users (id, email, password_hash, role, status, referral_code)
           VALUES ($1, $2, '$argon2id$v=19$m=65536,t=3,p=4$hash', 'user', 'active', $3)"#,
    )
    .bind(user_id)
    .bind(format!("prefs_prop_{short}@example.com"))
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
           VALUES ($1, 'individual', $2, 'Prefs Prop WS')"#,
    )
    .bind(workspace_id)
    .bind(user_id)
    .execute(pool)
    .await
    .expect("Failed to seed test workspace");
    (user_id, workspace_id)
}

/// Remove a throwaway user + workspace seeded for the notification-gating test.
/// notifications/preferences cascade via their FKs; delete explicitly first to
/// be robust across environments, then remove workspace and user.
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

/// Remove the throwaway user (preferences cascade via the FK's ON DELETE CASCADE).
async fn cleanup_user(pool: &PgPool, user_id: Uuid) {
    // Delete preferences explicitly first in case the FK is not ON DELETE CASCADE
    // in every environment, then remove the user row itself.
    let _ = sqlx::query("DELETE FROM notification_preferences WHERE user_id = $1")
        .bind(user_id)
        .execute(pool)
        .await;
    let _ = sqlx::query("DELETE FROM users WHERE id = $1")
        .bind(user_id)
        .execute(pool)
        .await;
}

// ─── Property 22: Preferences persistence round-trip ───────────────────────────
//
// Feature: ursnip-feature-matrix, Property 22: For any notification-preferences
// payload, persisting it via the preferences endpoint and then retrieving it SHALL
// return equal system_enabled, promotional_enabled, and security_system_enabled
// values.
//
// **Validates: Requirements 26.1, 26.2**

#[test]
#[ignore]
fn prop_preferences_persistence_round_trip() {
    // A single shared runtime + pool is reused across all proptest iterations so we
    // don't reconnect / re-run migrations per case.
    let rt = Runtime::new().expect("failed to build tokio runtime");
    let pool = rt.block_on(setup_test_db());
    let service = NotificationPreferencesService::new(pool.clone());

    let config = ProptestConfig {
        cases: 100,
        ..ProptestConfig::default()
    };

    let mut runner = proptest::test_runner::TestRunner::new(config);

    runner
        .run(
            &(any::<bool>(), any::<bool>(), any::<bool>()),
            |(system_enabled, promotional_enabled, security_system_enabled)| {
                rt.block_on(async {
                    let user_id = seed_user(&pool).await;

                    let params = UpdateNotificationPreferences {
                        system_enabled,
                        promotional_enabled,
                        security_system_enabled,
                    };

                    // PUT-equivalent: upsert the generated preferences.
                    let written = service
                        .upsert(user_id, params)
                        .await
                        .expect("upsert (PUT) must succeed");

                    // GET-equivalent: read the preferences back.
                    let read_back = service.get(user_id).await.expect("get (GET) must succeed");

                    // Always clean up before asserting so a failing case does not
                    // leak rows into subsequent iterations.
                    cleanup_user(&pool, user_id).await;

                    // The upsert return value must reflect exactly what was written.
                    prop_assert_eq!(
                        written.system_enabled,
                        system_enabled,
                        "upsert returned system_enabled != written"
                    );
                    prop_assert_eq!(
                        written.promotional_enabled,
                        promotional_enabled,
                        "upsert returned promotional_enabled != written"
                    );
                    prop_assert_eq!(
                        written.security_system_enabled,
                        security_system_enabled,
                        "upsert returned security_system_enabled != written"
                    );

                    // The round-trip read must equal what was written.
                    prop_assert_eq!(
                        read_back.system_enabled,
                        system_enabled,
                        "round-trip system_enabled mismatch"
                    );
                    prop_assert_eq!(
                        read_back.promotional_enabled,
                        promotional_enabled,
                        "round-trip promotional_enabled mismatch"
                    );
                    prop_assert_eq!(
                        read_back.security_system_enabled,
                        security_system_enabled,
                        "round-trip security_system_enabled mismatch"
                    );

                    Ok(())
                })
            },
        )
        .expect("Property 22: preferences persistence round-trip failed");
}

// ─── Property 23: System opt-out preserves expiry and security notifications ───
//
// Feature: ursnip-feature-matrix, Property 23: System opt-out preserves expiry and security notifications
//
// For any user who has opted out of System_Notifications (`system_enabled = false`)
// but has NOT explicitly disabled security-relevant System_Notifications
// (`security_system_enabled = true`), the Notification_Service SHALL still create
// plan-expiry and security-relevant device notifications for that user, while
// generic (non-expiry, non-device) system notifications are suppressed.
//
// This drives the REAL `NotificationService::create` path against a live database,
// exercising its preference gating end-to-end (create → persisted row visible via
// `list`), not just the pure `should_create` predicate.
//
// **Validates: Requirements 26.5**

/// Event types that MUST survive a plain system opt-out (expiry + security-relevant
/// device events), paired with their expected persisted `classification`.
const PRESERVED_EVENT_TYPES: &[&str] = &[
    "plan_expiry",
    "subscription_expiry",
    "device_registered",
    "device_removed",
];

/// Generic system event types that MUST be suppressed once the user opts out of
/// system notifications (they are neither expiry nor security-relevant device events).
const SUPPRESSED_EVENT_TYPES: &[&str] = &[
    "workspace_invite",
    "snippet_created",
    "member_joined",
    "comment_added",
];

#[test]
#[ignore]
fn prop_system_opt_out_preserves_expiry_and_security() {
    let rt = Runtime::new().expect("failed to build tokio runtime");
    let pool = rt.block_on(setup_test_db());
    let prefs_service = NotificationPreferencesService::new(pool.clone());
    let notif_service = NotificationService::new(pool.clone());

    let config = ProptestConfig {
        cases: 100,
        ..ProptestConfig::default()
    };

    let mut runner = proptest::test_runner::TestRunner::new(config);

    // Generators:
    //   - promotional_enabled: free to vary (must not affect system gating).
    //   - preserved_idx / suppressed_idx: pick one event of each kind per case so
    //     we cover every preserved and suppressed event type across iterations.
    let strategy = (
        any::<bool>(),
        0usize..PRESERVED_EVENT_TYPES.len(),
        0usize..SUPPRESSED_EVENT_TYPES.len(),
    );

    runner
        .run(
            &strategy,
            |(promotional_enabled, preserved_idx, suppressed_idx)| {
                rt.block_on(async {
                    let (user_id, workspace_id) = seed_user_and_workspace(&pool).await;

                    // The invariant premise: system opted OUT, security-relevant
                    // system notifications NOT explicitly disabled.
                    prefs_service
                        .upsert(
                            user_id,
                            UpdateNotificationPreferences {
                                system_enabled: false,
                                promotional_enabled,
                                security_system_enabled: true,
                            },
                        )
                        .await
                        .expect("upserting preferences must succeed");

                    let preserved_event = PRESERVED_EVENT_TYPES[preserved_idx];
                    let suppressed_event = SUPPRESSED_EVENT_TYPES[suppressed_idx];

                    // Create a preserved (expiry / security-relevant device) system notification.
                    notif_service
                        .create(CreateNotification {
                            user_id,
                            workspace_id,
                            event_type: preserved_event.to_string(),
                            title: format!("preserved: {preserved_event}"),
                            description: None,
                            metadata: None,
                            classification: Some("system".to_string()),
                        })
                        .await
                        .expect("create (preserved) must succeed");

                    // Create a generic system notification that should be suppressed.
                    notif_service
                        .create(CreateNotification {
                            user_id,
                            workspace_id,
                            event_type: suppressed_event.to_string(),
                            title: format!("suppressed: {suppressed_event}"),
                            description: None,
                            metadata: None,
                            classification: Some("system".to_string()),
                        })
                        .await
                        .expect("create (suppressed) must succeed");

                    // Read back what was actually persisted for this user.
                    let persisted_events: Vec<String> = sqlx::query_scalar(
                        "SELECT event_type FROM notifications WHERE user_id = $1",
                    )
                    .bind(user_id)
                    .fetch_all(&pool)
                    .await
                    .expect("querying persisted notifications must succeed");

                    // Clean up before asserting so a failing case leaks nothing.
                    cleanup_user_and_workspace(&pool, user_id, workspace_id).await;

                    // The expiry / security-relevant device notification survived the opt-out.
                    prop_assert!(
                        persisted_events.iter().any(|e| e == preserved_event),
                        "expiry/security event `{}` must still be created when system_enabled=false \
                         and security_system_enabled=true (persisted: {:?})",
                        preserved_event,
                        persisted_events
                    );

                    // The generic system notification was suppressed by the opt-out.
                    prop_assert!(
                        !persisted_events.iter().any(|e| e == suppressed_event),
                        "generic system event `{}` must be suppressed when system_enabled=false \
                         (persisted: {:?})",
                        suppressed_event,
                        persisted_events
                    );

                    Ok(())
                })
            },
        )
        .expect("Property 23: system opt-out preserves expiry and security notifications failed");
}
