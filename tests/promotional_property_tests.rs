//! Property-based test for promotional opt-out (Property 20, Task 10.7).
//!
//! Exercises the REAL `NotificationService::broadcast_promotional` path against
//! a live PostgreSQL database. For any set of targeted users with arbitrary
//! promotional preferences, issuing a promotional broadcast creates a
//! promotional notification for a user IF AND ONLY IF that user has NOT opted
//! out of promotional notifications (Requirements 25.1, 26.4).
//!
//! Like the other DB-backed suites in this repo (`preferences_property_tests`,
//! `promotional_broadcast_tests`), this test is `#[ignore]`d by default and
//! requires a live database. Bring one up with `docker-compose.test.yml` and
//! point `DATABASE_URL` at it, e.g.:
//!
//! ```text
//! docker compose -f docker-compose.test.yml up -d
//! DATABASE_URL=postgres://ursnip_test:ursnip_test@localhost:5433/ursnip_test \
//!   cargo test --test promotional_property_tests -- --ignored
//! ```
//!
//! Requirements covered: 25.1, 26.4

use proptest::prelude::*;
use proptest::test_runner::Config as ProptestConfig;
use sqlx::PgPool;
use tokio::runtime::Runtime;
use uuid::Uuid;

use ursnip_backend::notifications::preferences::{
    NotificationPreferencesService, UpdateNotificationPreferences,
};
use ursnip_backend::notifications::service::NotificationService;

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

/// How a targeted user's promotional preference is configured for a case.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PromoPref {
    /// No `notification_preferences` row at all → defaults to opted-in.
    AbsentDefaultOptedIn,
    /// A row with `promotional_enabled = true`.
    ExplicitOptedIn,
    /// A row with `promotional_enabled = false` → opted out.
    ExplicitOptedOut,
}

impl PromoPref {
    /// Whether a user with this preference should receive a promotional
    /// notification (created iff not opted out).
    fn expects_notification(self) -> bool {
        !matches!(self, PromoPref::ExplicitOptedOut)
    }
}

/// Seed a user plus the individual workspace they own (the workspace a
/// promotional broadcast attaches the notification to) and configure their
/// promotional preference. Returns the new user id.
async fn seed_user_with_pref(pool: &PgPool, pref: PromoPref) -> Uuid {
    let user_id = Uuid::new_v4();
    let short = &user_id.to_string()[..8];
    sqlx::query(
        r#"INSERT INTO users (id, email, password_hash, role, status, referral_code)
           VALUES ($1, $2, '$argon2id$v=19$m=65536,t=3,p=4$hash', 'user', 'active', $3)"#,
    )
    .bind(user_id)
    .bind(format!("promo_prop_{short}@example.com"))
    .bind(format!("REF{short}"))
    .execute(pool)
    .await
    .expect("Failed to seed test user");

    let workspace_id = Uuid::new_v4();
    sqlx::query(
        r#"INSERT INTO workspaces (id, type, owner_id, name)
           VALUES ($1, 'individual', $2, 'Promo Prop WS')"#,
    )
    .bind(workspace_id)
    .bind(user_id)
    .execute(pool)
    .await
    .expect("Failed to seed test workspace");

    match pref {
        PromoPref::AbsentDefaultOptedIn => {
            // Intentionally leave no preferences row → all-enabled defaults.
        }
        PromoPref::ExplicitOptedIn | PromoPref::ExplicitOptedOut => {
            let promotional_enabled = pref == PromoPref::ExplicitOptedIn;
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
    }

    user_id
}

/// Count promotional notifications persisted for a user.
async fn promotional_count(pool: &PgPool, user_id: Uuid) -> i64 {
    sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM notifications WHERE user_id = $1 AND classification = 'promotional'",
    )
    .bind(user_id)
    .fetch_one(pool)
    .await
    .expect("count query must succeed")
}

/// Remove a seeded user together with everything that references it, so a case
/// leaks nothing into subsequent iterations.
async fn cleanup_user(pool: &PgPool, user_id: Uuid) {
    let _ = sqlx::query("DELETE FROM notifications WHERE user_id = $1")
        .bind(user_id)
        .execute(pool)
        .await;
    let _ = sqlx::query("DELETE FROM notification_preferences WHERE user_id = $1")
        .bind(user_id)
        .execute(pool)
        .await;
    let _ = sqlx::query("DELETE FROM workspaces WHERE owner_id = $1")
        .bind(user_id)
        .execute(pool)
        .await;
    let _ = sqlx::query("DELETE FROM users WHERE id = $1")
        .bind(user_id)
        .execute(pool)
        .await;
}

// ─── Property 20: Promotional notifications respect opt-out ────────────────────
//
// Feature: ursnip-feature-matrix, Property 20: Promotional notifications respect opt-out
//
// For any set of targeted users with arbitrary promotional preferences, issuing
// a promotional broadcast SHALL create a promotional notification for a user if
// and only if that user has not opted out of promotional notifications.
//
// **Validates: Requirements 25.1, 26.4**

/// Map a small integer draw to a `PromoPref` variant so proptest covers all
/// three configurations (absent default, explicit opt-in, explicit opt-out)
/// across iterations.
fn pref_from_draw(n: u8) -> PromoPref {
    match n % 3 {
        0 => PromoPref::AbsentDefaultOptedIn,
        1 => PromoPref::ExplicitOptedIn,
        _ => PromoPref::ExplicitOptedOut,
    }
}

#[test]
#[ignore]
fn prop_promotional_notifications_respect_opt_out() {
    // A single shared runtime + pool is reused across all proptest iterations so
    // we don't reconnect / re-run migrations per case.
    let rt = Runtime::new().expect("failed to build tokio runtime");
    let pool = rt.block_on(setup_test_db());
    let service = NotificationService::new(pool.clone());

    let config = ProptestConfig {
        cases: 100,
        ..ProptestConfig::default()
    };

    let mut runner = proptest::test_runner::TestRunner::new(config);

    // Generate a non-empty set of targeted users, each with an arbitrary
    // promotional preference (absent-default / explicit opt-in / explicit
    // opt-out). The vec length varies so we exercise mixed opt-in/opt-out sets.
    let strategy = prop::collection::vec(any::<u8>().prop_map(pref_from_draw), 1..6);

    runner
        .run(&strategy, |prefs| {
            rt.block_on(async {
                // Seed one user per generated preference and remember expectations.
                let mut users: Vec<(Uuid, PromoPref)> = Vec::with_capacity(prefs.len());
                for pref in &prefs {
                    let user_id = seed_user_with_pref(&pool, *pref).await;
                    users.push((user_id, *pref));
                }

                let target_ids: Vec<Uuid> = users.iter().map(|(id, _)| *id).collect();
                let expected_created =
                    users.iter().filter(|(_, p)| p.expects_notification()).count();

                // Issue the real promotional broadcast to the exact target set.
                let broadcast = service
                    .broadcast_promotional(
                        "New feature available",
                        Some("Check out our latest update"),
                        None,
                        Some(&target_ids),
                    )
                    .await
                    .expect("broadcast must succeed");

                // Record the per-user persisted promotional counts before cleanup.
                let mut observed: Vec<(PromoPref, i64)> = Vec::with_capacity(users.len());
                for (user_id, pref) in &users {
                    observed.push((*pref, promotional_count(&pool, *user_id).await));
                }

                // Always clean up before asserting so a failing case leaks nothing.
                for (user_id, _) in &users {
                    cleanup_user(&pool, *user_id).await;
                }

                // Fan-out summary: every targeted user resolved to a workspace, and
                // exactly the not-opted-out users had a notification created.
                prop_assert_eq!(
                    broadcast.targeted,
                    users.len(),
                    "every seeded target should resolve to an individual workspace"
                );
                prop_assert_eq!(
                    broadcast.created,
                    expected_created,
                    "created count must equal the number of not-opted-out users"
                );
                prop_assert_eq!(
                    broadcast.skipped,
                    users.len() - expected_created,
                    "skipped count must equal the number of opted-out users"
                );

                // The core iff: a promotional notification exists for a user
                // exactly when that user has not opted out.
                for (pref, count) in observed {
                    if pref.expects_notification() {
                        prop_assert_eq!(
                            count,
                            1,
                            "not-opted-out user ({:?}) must have exactly one promotional notification",
                            pref
                        );
                    } else {
                        prop_assert_eq!(
                            count,
                            0,
                            "opted-out user ({:?}) must have no promotional notification",
                            pref
                        );
                    }
                }

                Ok(())
            })
        })
        .expect("Property 20: promotional notifications respect opt-out failed");
}
