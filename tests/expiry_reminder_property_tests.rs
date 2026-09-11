//! Property-based test for expiry-reminder idempotence (Property 19).
//!
//! Exercises the real `SchedulerService::expiry_reminder_check()` against a live
//! PostgreSQL database. For any (subscription, period_end, threshold) whose
//! whole-day distance to `period_end` sits exactly on a reminder threshold,
//! running the scheduler check any number of times must create *at most one*
//! plan-expiry notification for that combination — the idempotence guarantee
//! backed by the `expiry_reminders_sent` marker table (migration 0018) and its
//! `INSERT ... ON CONFLICT DO NOTHING` guard.
//!
//! Like the other DB-backed suites in this repo (`notifications_property_tests`,
//! `preferences_property_tests`), this test is `#[ignore]`d by default and
//! requires a live database. Bring one up with `docker-compose.test.yml` and
//! point `DATABASE_URL` at it, e.g.:
//!
//! ```text
//! docker compose -f docker-compose.test.yml up -d
//! DATABASE_URL=postgres://ursnip_test:ursnip_test@localhost:5433/ursnip_test \
//!   cargo test --test expiry_reminder_property_tests -- --ignored
//! ```
//!
//! Requirements covered: 23.3

use proptest::prelude::*;
use proptest::test_runner::Config as ProptestConfig;
use sqlx::PgPool;
use std::sync::Arc;
use tokio::runtime::Runtime;
use uuid::Uuid;

use ursnip_backend::scheduler::service::SchedulerService;

// ─── Harness ─────────────────────────────────────────────────────────────────

/// The reminder day thresholds (Req 23.2) — a reminder fires iff the whole-day
/// distance to `period_end` equals one of these.
const THRESHOLDS: [i64; 5] = [30, 15, 7, 3, 1];

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

/// Insert a throwaway user so the workspace/subscription FKs are satisfied.
async fn seed_user(pool: &PgPool) -> Uuid {
    let user_id = Uuid::new_v4();
    let short = &user_id.to_string()[..8];
    sqlx::query(
        r#"INSERT INTO users (id, email, password_hash, role, status, referral_code)
           VALUES ($1, $2, '$argon2id$v=19$m=65536,t=3,p=4$hash', 'user', 'active', $3)"#,
    )
    .bind(user_id)
    .bind(format!("expiry_prop_{short}@example.com"))
    .bind(format!("REF{short}"))
    .execute(pool)
    .await
    .expect("Failed to seed test user");
    user_id
}

/// Seed a user, an individual workspace they own, and an active paid-tier
/// subscription whose `period_end` lands exactly on the given whole-day
/// `threshold` from now. Returns `(user_id, workspace_id, subscription_id,
/// period_end)`.
///
/// `period_end` is set to `now() + threshold days + 12h`; the scheduler computes
/// the whole-day distance via `FLOOR(EPOCH(period_end - now())/86400)`, so the
/// extra 12h buffer keeps the floor equal to `threshold` for the full duration
/// of the (fast, in-process) test run — never rounding down to `threshold - 1`.
async fn seed_expiring_subscription(
    pool: &PgPool,
    threshold: i64,
) -> (Uuid, Uuid, Uuid, chrono::DateTime<chrono::Utc>) {
    let user_id = seed_user(pool).await;
    let workspace_id = Uuid::new_v4();
    sqlx::query(
        r#"INSERT INTO workspaces (id, type, owner_id, name)
           VALUES ($1, 'individual', $2, 'Expiry Prop WS')"#,
    )
    .bind(workspace_id)
    .bind(user_id)
    .execute(pool)
    .await
    .expect("Failed to seed test workspace");

    let subscription_id = Uuid::new_v4();
    let period_end: chrono::DateTime<chrono::Utc> = sqlx::query_scalar(
        r#"INSERT INTO subscriptions (id, workspace_id, tier, status, period_end)
           VALUES ($1, $2, 'pro', 'active', now() + ($3 || ' days')::interval + interval '12 hours')
           RETURNING period_end"#,
    )
    .bind(subscription_id)
    .bind(workspace_id)
    .bind(threshold.to_string())
    .fetch_one(pool)
    .await
    .expect("Failed to seed test subscription");

    (user_id, workspace_id, subscription_id, period_end)
}

/// Count plan-expiry notifications for a given user + subscription + threshold.
/// Matches on `event_type = 'plan_expiry'` plus the `threshold_days` recorded in
/// the notification metadata, mirroring the (subscription, period_end, threshold)
/// key the idempotence guarantee is stated over.
async fn count_expiry_notifications(
    pool: &PgPool,
    user_id: Uuid,
    subscription_id: Uuid,
    threshold: i64,
) -> i64 {
    sqlx::query_scalar(
        r#"SELECT COUNT(*)
           FROM notifications
           WHERE user_id = $1
             AND event_type = 'plan_expiry'
             AND metadata->>'subscription_id' = $2::text
             AND (metadata->>'threshold_days')::bigint = $3"#,
    )
    .bind(user_id)
    .bind(subscription_id.to_string())
    .bind(threshold)
    .fetch_one(pool)
    .await
    .expect("Failed to count expiry notifications")
}

/// Remove all rows seeded for a case so a failure does not leak state into the
/// next iteration. Order respects FKs: notifications + markers, then the
/// subscription, workspace, and user.
async fn cleanup(pool: &PgPool, user_id: Uuid, workspace_id: Uuid, subscription_id: Uuid) {
    let _ = sqlx::query("DELETE FROM expiry_reminders_sent WHERE subscription_id = $1")
        .bind(subscription_id)
        .execute(pool)
        .await;
    let _ = sqlx::query("DELETE FROM notifications WHERE user_id = $1")
        .bind(user_id)
        .execute(pool)
        .await;
    let _ = sqlx::query("DELETE FROM notification_preferences WHERE user_id = $1")
        .bind(user_id)
        .execute(pool)
        .await;
    let _ = sqlx::query("DELETE FROM subscriptions WHERE id = $1")
        .bind(subscription_id)
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

/// Pick one of the reminder day thresholds so the seeded subscription actually
/// triggers a reminder (the "fires at threshold" behaviour is Property 18; here
/// we assume firing and pin down idempotence of repeated runs).
fn threshold_strategy() -> impl Strategy<Value = i64> {
    prop::sample::select(THRESHOLDS.to_vec())
}

/// Number of times to run the scheduler check for a case. At least 2 so we
/// genuinely exercise repetition; the property must hold for *any* count.
fn run_count_strategy() -> impl Strategy<Value = u32> {
    2u32..=6
}

// ─── Property 19: Expiry reminders are idempotent per threshold ─────────────────
//
// Feature: ursnip-feature-matrix, Property 19: Expiry reminders are idempotent per threshold
//
// For any subscription, paid period, and day threshold, running the
// Expiry_Reminder_Scheduler any number of times SHALL result in at most one
// plan-expiry notification for that (subscription, period_end, threshold)
// combination.
//
// This drives the REAL `SchedulerService::expiry_reminder_check` path against a
// live database: a subscription is seeded exactly on a reminder threshold, the
// check is run `runs` times, and the number of matching plan-expiry
// notifications must be exactly one (fires once, then idempotent no-ops).
//
// **Validates: Requirements 23.3**

#[test]
#[ignore]
fn prop_expiry_reminder_idempotent_per_threshold() {
    // A single shared runtime + pool is reused across all proptest iterations so
    // we don't reconnect / re-run migrations per case.
    let rt = Runtime::new().expect("failed to build tokio runtime");
    let pool = rt.block_on(setup_test_db());
    let scheduler = Arc::new(SchedulerService::new(pool.clone(), 5));

    let config = ProptestConfig {
        cases: 100,
        ..ProptestConfig::default()
    };

    let mut runner = proptest::test_runner::TestRunner::new(config);

    let strategy = (threshold_strategy(), run_count_strategy());

    runner
        .run(&strategy, |(threshold, runs)| {
            rt.block_on(async {
                let (user_id, workspace_id, subscription_id, _period_end) =
                    seed_expiring_subscription(&pool, threshold).await;

                // Run the real scheduler check `runs` times. Only the first run
                // should claim the (subscription, period_end, threshold) marker
                // and create a notification; every later run must be a no-op.
                let mut run_results = Vec::with_capacity(runs as usize);
                for _ in 0..runs {
                    run_results.push(scheduler.expiry_reminder_check().await);
                }

                let count =
                    count_expiry_notifications(&pool, user_id, subscription_id, threshold).await;

                // Clean up before asserting so a failing case does not leak rows
                // into subsequent iterations.
                cleanup(&pool, user_id, workspace_id, subscription_id).await;

                // Every run must succeed (no transient/DB errors during the test).
                for (i, r) in run_results.iter().enumerate() {
                    prop_assert!(
                        r.is_ok(),
                        "expiry_reminder_check run {} failed: {:?}",
                        i,
                        r
                    );
                }

                // The core idempotence guarantee: at most one notification for
                // this (subscription, period_end, threshold) regardless of how
                // many times the check ran. Because the seeded subscription sits
                // exactly on a threshold, the count is exactly one.
                prop_assert_eq!(
                    count,
                    1,
                    "threshold={} runs={}: expected exactly one plan-expiry notification, got {}",
                    threshold,
                    runs,
                    count
                );

                Ok(())
            })
        })
        .expect("Property 19: expiry-reminder idempotence failed");
}
