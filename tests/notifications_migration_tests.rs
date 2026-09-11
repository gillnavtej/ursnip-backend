//! Smoke test for the `notifications` table migration (`0015_notifications.sql`).
//!
//! This confirms the CRITICAL latent-defect fix: `NotificationService` writes to a
//! `notifications` table that previously had no migration, so every notification
//! write failed. This test asserts the table and its expected columns exist after
//! migrations run.
//!
//! Like the other DB-backed integration suites in this repo, it is `#[ignore]`d by
//! default and requires a live PostgreSQL instance.
//!
//! Run with: `cargo test --test notifications_migration_tests -- --ignored`
//! (requires DATABASE_URL pointing to a test database).
//!
//! Requirements covered: 20.1

use sqlx::PgPool;

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

// ─── Tests ───────────────────────────────────────────────────────────────────

/// The `notifications` table and all columns `NotificationService` relies on must
/// exist after migrations are applied.
#[tokio::test]
#[ignore]
async fn notifications_table_and_columns_exist_after_migration() {
    let pool = setup_test_db().await;

    // The table itself should be present in the public schema.
    let table_exists: Option<String> = sqlx::query_scalar(
        r#"
        SELECT table_name
        FROM information_schema.tables
        WHERE table_name = 'notifications'
        "#,
    )
    .fetch_optional(&pool)
    .await
    .expect("Failed to query information_schema.tables");

    assert_eq!(
        table_exists.as_deref(),
        Some("notifications"),
        "the `notifications` table should exist after migrations run"
    );

    // Collect every column defined on the table.
    let columns: Vec<String> = sqlx::query_scalar(
        r#"
        SELECT column_name
        FROM information_schema.columns
        WHERE table_name = 'notifications'
        "#,
    )
    .fetch_all(&pool)
    .await
    .expect("Failed to query information_schema.columns");

    // Every column that NotificationService / the FromRow struct depends on.
    let expected_columns = [
        "id",
        "user_id",
        "workspace_id",
        "event_type",
        "title",
        "description",
        "metadata",
        "classification",
        "is_read",
        "created_at",
    ];

    for expected in expected_columns {
        assert!(
            columns.iter().any(|c| c == expected),
            "expected column `{expected}` to exist on `notifications`; found columns: {columns:?}"
        );
    }
}
