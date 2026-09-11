//! Property-based test for notification persistence round-trip (Property 17).
//!
//! Exercises the real `NotificationService` against a live PostgreSQL database:
//! for any generated notification payload, writing it via `create` and then
//! listing that user's notifications via `list` must return a notification with
//! equal `user_id`, `workspace_id`, `event_type`, `title`, `description`, and
//! `metadata`.
//!
//! Like the other DB-backed suites in this repo (`preferences_property_tests`,
//! `notifications_migration_tests`), this test is `#[ignore]`d by default and
//! requires a live database. Bring one up with `docker-compose.test.yml` and
//! point `DATABASE_URL` at it, e.g.:
//!
//! ```text
//! docker compose -f docker-compose.test.yml up -d
//! DATABASE_URL=postgres://ursnip_test:ursnip_test@localhost:5433/ursnip_test \
//!   cargo test --test notifications_property_tests -- --ignored
//! ```
//!
//! Requirements covered: 20.2

use proptest::prelude::*;
use proptest::test_runner::Config as ProptestConfig;
use sqlx::PgPool;
use tokio::runtime::Runtime;
use uuid::Uuid;

use ursnip_backend::notifications::service::{
    CreateNotification, ListNotificationsQuery, NotificationService,
};

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
    .bind(format!("notif_prop_{short}@example.com"))
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
           VALUES ($1, 'individual', $2, 'Notif Prop WS')"#,
    )
    .bind(workspace_id)
    .bind(user_id)
    .execute(pool)
    .await
    .expect("Failed to seed test workspace");
    (user_id, workspace_id)
}

/// Remove the throwaway user + workspace seeded for a case. Delete notifications
/// explicitly first to be robust across environments, then the workspace and user.
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

/// Non-empty, reasonably-bounded text for `event_type` / `title`.
/// Constrained to printable characters so it round-trips cleanly through Postgres
/// text columns and stays within the input space the service actually sees.
fn text_strategy() -> impl Strategy<Value = String> {
    "[a-zA-Z0-9 _.:-]{1,80}"
}

/// Optional description: either `None` or a bounded text string (may be empty).
fn description_strategy() -> impl Strategy<Value = Option<String>> {
    prop_oneof![
        Just(None),
        "[a-zA-Z0-9 _.:-]{0,200}".prop_map(Some),
    ]
}

/// Arbitrary JSON metadata object with string values. `create` defaults an
/// absent metadata to `{}`, so we exercise both the `None` case and populated
/// objects to cover the round-trip of the `jsonb` column.
fn metadata_strategy() -> impl Strategy<Value = Option<serde_json::Value>> {
    let entries = prop::collection::vec(
        ("[a-z][a-z0-9_]{0,15}", "[a-zA-Z0-9 _.:-]{0,40}"),
        0..5,
    );
    prop_oneof![
        Just(None),
        entries.prop_map(|kvs| {
            let map: serde_json::Map<String, serde_json::Value> = kvs
                .into_iter()
                .map(|(k, v)| (k, serde_json::Value::String(v)))
                .collect();
            Some(serde_json::Value::Object(map))
        }),
    ]
}

// ─── Property 17: Notification persistence round-trip ───────────────────────────
//
// Feature: ursnip-feature-matrix, Property 17: Notification persistence round-trip
//
// For any valid notification written via the Notification_Service after the
// migration is applied, listing that user's notifications SHALL return a
// notification with equal user_id, workspace_id, event_type, title, description,
// and metadata.
//
// This drives the REAL `NotificationService::create` and `NotificationService::list`
// paths against a live database (write → persisted row visible via `list`).
//
// **Validates: Requirements 20.2**

#[test]
#[ignore]
fn prop_notification_persistence_round_trip() {
    // A single shared runtime + pool is reused across all proptest iterations so we
    // don't reconnect / re-run migrations per case.
    let rt = Runtime::new().expect("failed to build tokio runtime");
    let pool = rt.block_on(setup_test_db());
    let service = NotificationService::new(pool.clone());

    let config = ProptestConfig {
        cases: 100,
        ..ProptestConfig::default()
    };

    let mut runner = proptest::test_runner::TestRunner::new(config);

    let strategy = (
        text_strategy(),        // event_type
        text_strategy(),        // title
        description_strategy(), // description
        metadata_strategy(),    // metadata
    );

    runner
        .run(
            &strategy,
            |(event_type, title, description, metadata)| {
                rt.block_on(async {
                    let (user_id, workspace_id) = seed_user_and_workspace(&pool).await;

                    // The metadata actually persisted: `create` defaults `None` to `{}`.
                    let expected_metadata = metadata
                        .clone()
                        .unwrap_or_else(|| serde_json::json!({}));

                    // Write the notification via the real service (classification
                    // defaults to `system`; a fresh user has all-enabled default
                    // preferences, so the notification is not gated out).
                    let create_result = service
                        .create(CreateNotification {
                            user_id,
                            workspace_id,
                            event_type: event_type.clone(),
                            title: title.clone(),
                            description: description.clone(),
                            metadata: metadata.clone(),
                            classification: None,
                        })
                        .await;

                    // List that user's notifications back via the real service.
                    // Only meaningful when the write succeeded; if it failed the
                    // create assertion below surfaces the error.
                    let listed = if create_result.is_ok() {
                        Some(
                            service
                                .list(
                                    user_id,
                                    &ListNotificationsQuery {
                                        page: None,
                                        per_page: None,
                                        unread_only: None,
                                    },
                                )
                                .await,
                        )
                    } else {
                        None
                    };

                    // Always clean up before asserting so a failing case does not
                    // leak rows into subsequent iterations.
                    cleanup_user_and_workspace(&pool, user_id, workspace_id).await;

                    // The write itself must have succeeded (persisted durably).
                    prop_assert!(
                        create_result.is_ok(),
                        "create must succeed, got {:?}",
                        create_result
                    );

                    let response = listed
                        .expect("list should have been attempted")
                        .expect("list must succeed");

                    // Exactly one notification was written for this fresh user.
                    prop_assert_eq!(
                        response.data.len(),
                        1,
                        "listing a fresh user must return exactly the one written notification"
                    );

                    let n = &response.data[0];

                    prop_assert_eq!(n.user_id, user_id, "round-trip user_id mismatch");
                    prop_assert_eq!(
                        n.workspace_id,
                        workspace_id,
                        "round-trip workspace_id mismatch"
                    );
                    prop_assert_eq!(
                        &n.event_type,
                        &event_type,
                        "round-trip event_type mismatch"
                    );
                    prop_assert_eq!(&n.title, &title, "round-trip title mismatch");
                    prop_assert_eq!(
                        &n.description,
                        &description,
                        "round-trip description mismatch"
                    );
                    prop_assert_eq!(
                        &n.metadata,
                        &expected_metadata,
                        "round-trip metadata mismatch"
                    );

                    Ok(())
                })
            },
        )
        .expect("Property 17: notification persistence round-trip failed");
}
