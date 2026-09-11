//! Property-based test for snippet sharing (Property 1).
//!
//! Exercises the real `SharingService` against a live PostgreSQL database: for
//! any source snippet and any resolvable recipient with a conflict-free target
//! workspace, `share_snippet` creates a recipient-owned copy whose `trigger` and
//! `content` equal the source's, AND a subsequent edit to the source snippet
//! does NOT change the recipient's copy (the copy is an independent value copy
//! with no foreign key back to the source).
//!
//! Like the other DB-backed suites in this repo (`preferences_property_tests`,
//! `notifications_property_tests`, `sync_integration_tests`), this test is
//! `#[ignore]`d by default and requires a live database. Bring one up with
//! `docker-compose.test.yml` and point `DATABASE_URL` at it, e.g.:
//!
//! ```text
//! docker compose -f docker-compose.test.yml up -d
//! DATABASE_URL=postgres://ursnip_test:ursnip_test@localhost:5433/ursnip_test \
//!   cargo test --test sharing_property_tests -- --ignored
//! ```
//!
//! Requirements covered: 1.1, 1.2, 1.4, 2.1, 2.2

use std::sync::Arc;

use proptest::prelude::*;
use proptest::test_runner::Config as ProptestConfig;
use sqlx::PgPool;
use tokio::runtime::Runtime;
use uuid::Uuid;

use ursnip_backend::errors::AppError;
use ursnip_backend::sharing::service::{ShareRequest, SharingService};
use ursnip_backend::sync::service::SyncService;

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

/// Insert a throwaway user. Returns the new user id.
async fn seed_user(pool: &PgPool) -> Uuid {
    let user_id = Uuid::new_v4();
    let short = &user_id.to_string()[..8];
    sqlx::query(
        r#"INSERT INTO users (id, email, password_hash, role, status, referral_code)
           VALUES ($1, $2, '$argon2id$v=19$m=65536,t=3,p=4$hash', 'user', 'active', $3)"#,
    )
    .bind(user_id)
    .bind(format!("share_prop_{short}@example.com"))
    .bind(format!("REF{short}"))
    .execute(pool)
    .await
    .expect("Failed to seed test user");
    user_id
}

/// Insert a user + an individual workspace they own (with owner membership and an
/// active free subscription so `SyncService::create_snippet` can load a tier).
/// Returns `(user_id, workspace_id)`.
async fn seed_user_and_workspace(pool: &PgPool) -> (Uuid, Uuid) {
    let user_id = seed_user(pool).await;
    let workspace_id = Uuid::new_v4();
    sqlx::query(
        r#"INSERT INTO workspaces (id, type, owner_id, name)
           VALUES ($1, 'individual', $2, 'Share Prop WS')"#,
    )
    .bind(workspace_id)
    .bind(user_id)
    .execute(pool)
    .await
    .expect("Failed to seed test workspace");
    sqlx::query(
        r#"INSERT INTO workspace_members (workspace_id, user_id, role)
           VALUES ($1, $2, 'owner')"#,
    )
    .bind(workspace_id)
    .bind(user_id)
    .execute(pool)
    .await
    .expect("Failed to seed workspace membership");
    sqlx::query(
        r#"INSERT INTO subscriptions (workspace_id, tier, status)
           VALUES ($1, 'free', 'active')"#,
    )
    .bind(workspace_id)
    .execute(pool)
    .await
    .expect("Failed to seed subscription");
    (user_id, workspace_id)
}

/// Insert a source snippet owned by `owner_id` in `workspace_id`. Returns the
/// new snippet id.
async fn seed_snippet(
    pool: &PgPool,
    workspace_id: Uuid,
    owner_id: Uuid,
    trigger: &str,
    content: &str,
) -> Uuid {
    let snippet_id = Uuid::new_v4();
    sqlx::query(
        r#"INSERT INTO snippets (id, workspace_id, created_by, trigger, content, snippet_type, version)
           VALUES ($1, $2, $3, $4, $5, 'text', 1)"#,
    )
    .bind(snippet_id)
    .bind(workspace_id)
    .bind(owner_id)
    .bind(trigger)
    .bind(content)
    .execute(pool)
    .await
    .expect("Failed to seed source snippet");
    snippet_id
}

/// Fetch a snippet's `(trigger, content)` by id.
async fn fetch_snippet(pool: &PgPool, snippet_id: Uuid) -> (String, String) {
    sqlx::query_as::<_, (String, String)>(
        "SELECT trigger, content FROM snippets WHERE id = $1",
    )
    .bind(snippet_id)
    .fetch_one(pool)
    .await
    .expect("Failed to fetch snippet")
}

/// Remove the two throwaway users + workspaces seeded for a case. Deletes in
/// reverse dependency order and is robust across environments.
async fn cleanup(
    pool: &PgPool,
    source_user: Uuid,
    source_ws: Uuid,
    recipient_user: Uuid,
    recipient_ws: Uuid,
) {
    for ws in [source_ws, recipient_ws] {
        let _ = sqlx::query("DELETE FROM sync_deltas WHERE workspace_id = $1")
            .bind(ws)
            .execute(pool)
            .await;
        let _ = sqlx::query(
            "DELETE FROM snippet_folders WHERE snippet_id IN (SELECT id FROM snippets WHERE workspace_id = $1)",
        )
        .bind(ws)
        .execute(pool)
        .await;
        let _ = sqlx::query("DELETE FROM snippets WHERE workspace_id = $1")
            .bind(ws)
            .execute(pool)
            .await;
        let _ = sqlx::query("DELETE FROM subscriptions WHERE workspace_id = $1")
            .bind(ws)
            .execute(pool)
            .await;
        let _ = sqlx::query("DELETE FROM workspace_members WHERE workspace_id = $1")
            .bind(ws)
            .execute(pool)
            .await;
    }
    for u in [source_user, recipient_user] {
        let _ = sqlx::query("DELETE FROM notifications WHERE user_id = $1")
            .bind(u)
            .execute(pool)
            .await;
        let _ = sqlx::query("DELETE FROM notification_preferences WHERE user_id = $1")
            .bind(u)
            .execute(pool)
            .await;
    }
    for ws in [source_ws, recipient_ws] {
        let _ = sqlx::query("DELETE FROM workspaces WHERE id = $1")
            .bind(ws)
            .execute(pool)
            .await;
    }
    for u in [source_user, recipient_user] {
        let _ = sqlx::query("DELETE FROM users WHERE id = $1")
            .bind(u)
            .execute(pool)
            .await;
    }
}

// ─── Strategies ────────────────────────────────────────────────────────────────

/// A snippet trigger: non-empty, printable, no whitespace, bounded — matches the
/// input space the sharing/sync path actually accepts and round-trips cleanly
/// through the unique `(workspace_id, trigger)` index.
fn trigger_strategy() -> impl Strategy<Value = String> {
    "[a-zA-Z0-9_.:-]{1,32}"
}

/// Snippet content: printable, bounded well under the free-tier 2000-char limit
/// so the copy is always accepted by `SyncService`'s tier content-size check
/// (this property is about preservation + independence, not the size limit).
fn content_strategy() -> impl Strategy<Value = String> {
    "[a-zA-Z0-9 _.:\\-\\n]{0,300}"
}

/// A distinct, non-conflicting new content value used to edit the source after
/// the share, so we can prove the recipient's copy is unaffected. Prefixed so it
/// can never coincide with the generated original content.
fn edit_content_strategy() -> impl Strategy<Value = String> {
    "[a-zA-Z0-9 _.:\\-\\n]{0,300}".prop_map(|s| format!("EDITED::{s}"))
}

// ─── Property 1: Shared copy preserves data and is independent of the source ────
//
// Feature: ursnip-feature-matrix, Property 1: Shared copy preserves data and is independent of the source
//
// For any source snippet and any resolvable recipient with a conflict-free target
// workspace, sharing SHALL create a recipient-owned snippet whose trigger and
// content equal the source's, AND subsequent edits to the source snippet SHALL
// NOT change the recipient's copy.
//
// This drives the REAL `SharingService::share_snippet` path against a live
// database: seed a source snippet (owned by the requester, so read access is
// granted), share it to a distinct recipient's workspace, assert the copy equals
// the source's trigger/content, then mutate the source row and assert the copy is
// unchanged (independence — a value copy with no FK to the source, Req 1.2).
//
// **Validates: Requirements 1.1, 1.2**

#[test]
#[ignore]
fn prop_shared_copy_preserves_and_is_independent() {
    // A single shared runtime + pool is reused across all proptest iterations so we
    // don't reconnect / re-run migrations per case.
    let rt = Runtime::new().expect("failed to build tokio runtime");
    let pool = rt.block_on(setup_test_db());

    let sync = Arc::new(SyncService::new(pool.clone()));
    let sharing = SharingService::new(pool.clone(), sync);

    let config = ProptestConfig {
        cases: 100,
        ..ProptestConfig::default()
    };

    let mut runner = proptest::test_runner::TestRunner::new(config);

    let strategy = (
        trigger_strategy(),      // source trigger
        content_strategy(),      // source content
        edit_content_strategy(), // content the source is edited to afterwards
    );

    runner
        .run(&strategy, |(trigger, content, edited_content)| {
            rt.block_on(async {
                // Distinct source owner (the requester) and recipient, each with
                // their own individual workspace + active free subscription.
                let (source_user, source_ws) = seed_user_and_workspace(&pool).await;
                let (recipient_user, recipient_ws) = seed_user_and_workspace(&pool).await;

                // Seed the source snippet owned by the requester so the read-access
                // check (owner bypass) passes.
                let source_snippet_id =
                    seed_snippet(&pool, source_ws, source_user, &trigger, &content).await;

                // ── Share the snippet to the recipient's workspace. ──────────
                // Recipient is addressed by user id; the target workspace is
                // conflict-free (freshly seeded, empty).
                let share_result = sharing
                    .share_snippet(
                        source_user,
                        ShareRequest {
                            source_snippet_id,
                            recipient: recipient_user.to_string(),
                            target_workspace_id: recipient_ws,
                        },
                    )
                    .await;

                // Read back the copy (if the share succeeded) before mutating the
                // source, then edit the source and read the copy again to check
                // independence.
                let outcome = match &share_result {
                    Ok(copy) => {
                        let copy_id = copy.snippet_id;
                        let recipient_id = copy.recipient_id;
                        let returned_trigger = copy.trigger.clone();

                        // Copy as persisted right after the share.
                        let (copy_trigger_before, copy_content_before) =
                            fetch_snippet(&pool, copy_id).await;

                        // ── Edit the SOURCE snippet (value change on the source
                        // row only). A correct value copy shares no FK/state with
                        // the source, so this must not touch the copy. ──────────
                        sqlx::query(
                            "UPDATE snippets SET content = $1, version = version + 1 WHERE id = $2",
                        )
                        .bind(&edited_content)
                        .bind(source_snippet_id)
                        .execute(&pool)
                        .await
                        .expect("editing the source snippet must succeed");

                        // Copy as persisted after the source edit.
                        let (copy_trigger_after, copy_content_after) =
                            fetch_snippet(&pool, copy_id).await;

                        Some((
                            recipient_id,
                            returned_trigger,
                            copy_trigger_before,
                            copy_content_before,
                            copy_trigger_after,
                            copy_content_after,
                        ))
                    }
                    Err(_) => None,
                };

                // Always clean up before asserting so a failing case does not leak
                // rows into subsequent iterations.
                cleanup(&pool, source_user, source_ws, recipient_user, recipient_ws).await;

                // The share of a conflict-free copy to a resolvable recipient must
                // succeed (Req 1.1).
                prop_assert!(
                    share_result.is_ok(),
                    "share_snippet must succeed for a resolvable recipient with a \
                     conflict-free workspace, got {:?}",
                    share_result
                );

                let (
                    recipient_id,
                    returned_trigger,
                    copy_trigger_before,
                    copy_content_before,
                    copy_trigger_after,
                    copy_content_after,
                ) = outcome.expect("share succeeded so an outcome must be present");

                // The copy is owned by the resolved recipient.
                prop_assert_eq!(
                    recipient_id,
                    recipient_user,
                    "the copy must be owned by the resolved recipient"
                );

                // Req 1.1: the copy's trigger and content equal the source's.
                prop_assert_eq!(
                    &copy_trigger_before,
                    &trigger,
                    "copy trigger must equal the source trigger"
                );
                prop_assert_eq!(
                    &copy_content_before,
                    &content,
                    "copy content must equal the source content"
                );
                // The service-reported trigger is consistent with the persisted copy.
                prop_assert_eq!(
                    &returned_trigger,
                    &trigger,
                    "returned trigger must equal the source trigger"
                );

                // Req 1.2: editing the source did not change the recipient's copy.
                prop_assert_eq!(
                    &copy_trigger_after,
                    &copy_trigger_before,
                    "copy trigger must be unchanged after editing the source"
                );
                prop_assert_eq!(
                    &copy_content_after,
                    &copy_content_before,
                    "copy content must be unchanged after editing the source (independence)"
                );
                // And specifically: the copy did NOT pick up the source's new value.
                prop_assert_ne!(
                    &copy_content_after,
                    &edited_content,
                    "copy content must not reflect the edited source content"
                );

                Ok(())
            })
        })
        .expect("Property 1: shared copy preservation and independence failed");
}

// ─── Property 2 helpers ─────────────────────────────────────────────────────

/// Count every snippet row across ALL workspaces (the global snippet total).
/// Property 2 asserts this total is unchanged by a share to an unresolvable
/// recipient, so it must be a global count — not scoped to a single workspace.
async fn count_all_snippets(pool: &PgPool) -> i64 {
    sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM snippets")
        .fetch_one(pool)
        .await
        .expect("Failed to count snippets")
}

/// A recipient identifier that does NOT resolve to an existing user.
///
/// Two shapes are generated so the property covers both resolution paths in
/// `resolve_recipient` (UUID id lookup and case-insensitive email lookup):
///   * a fresh random UUID string — parses as a UUID but matches no `users.id`;
///   * a syntactically-email-like string using a random, non-seeded local part
///     at a reserved example domain — matches no `users.email`.
/// The email local part is drawn from a wide charset so it is exceedingly
/// unlikely to collide with any seeded `share_prop_<hex8>@example.com` address,
/// and even a hypothetical collision would only make the recipient resolvable
/// (the test would then observe a successful/other outcome, which is asserted
/// against explicitly).
fn unresolvable_recipient_strategy() -> impl Strategy<Value = String> {
    let random_uuid = Just(()).prop_map(|_| Uuid::new_v4().to_string());
    let random_email =
        "[a-zA-Z0-9_.+-]{1,24}".prop_map(|local| format!("{local}@nonexistent.invalid"));
    prop_oneof![random_uuid, random_email]
}

// ─── Property 2: Unresolvable recipient creates no snippet ──────────────────────
//
// Feature: ursnip-feature-matrix, Property 2: Unresolvable recipient creates no snippet
//
// For any recipient identifier that does not resolve to an existing user, a
// share request SHALL be rejected with a recipient-not-found error AND the
// number of snippets in every workspace SHALL be unchanged.
//
// This drives the REAL `SharingService::share_snippet` path against a live
// database: seed a source snippet owned by the requester (so the read-access
// check passes and we reach recipient resolution), snapshot the global snippet
// count, attempt to share to an UNRESOLVABLE recipient, and assert the call
// fails with `RecipientNotFound` and the global snippet count is identical
// before and after (no copy — and nothing else — was written).
//
// **Validates: Requirements 1.4**

#[test]
#[ignore]
fn prop_unresolvable_recipient_creates_no_snippet() {
    let rt = Runtime::new().expect("failed to build tokio runtime");
    let pool = rt.block_on(setup_test_db());

    let sync = Arc::new(SyncService::new(pool.clone()));
    let sharing = SharingService::new(pool.clone(), sync);

    let config = ProptestConfig {
        cases: 100,
        ..ProptestConfig::default()
    };

    let mut runner = proptest::test_runner::TestRunner::new(config);

    // The target workspace is arbitrary: recipient resolution fails before the
    // workspace is ever touched, so a random (non-existent) id is sufficient and
    // exercises that no write happens regardless of target.
    let strategy = (
        trigger_strategy(),                             // source trigger
        content_strategy(),                             // source content
        unresolvable_recipient_strategy(),              // a recipient that resolves to no user
        Just(()).prop_map(|_| Uuid::new_v4()),          // an arbitrary target workspace id
    );

    runner
        .run(&strategy, |(trigger, content, recipient, target_ws)| {
            rt.block_on(async {
                // Seed the requester + their workspace and a source snippet they
                // own, so the read-access check passes and resolution is reached.
                let (source_user, source_ws) = seed_user_and_workspace(&pool).await;
                let source_snippet_id =
                    seed_snippet(&pool, source_ws, source_user, &trigger, &content).await;

                // Snapshot the GLOBAL snippet total before the attempt.
                let count_before = count_all_snippets(&pool).await;

                // ── Attempt to share to an UNRESOLVABLE recipient. ───────────
                // Recipient resolution precedes any write, so this must fail
                // before a copy is created (Req 1.4).
                let share_result = sharing
                    .share_snippet(
                        source_user,
                        ShareRequest {
                            source_snippet_id,
                            recipient: recipient.clone(),
                            target_workspace_id: target_ws,
                        },
                    )
                    .await;

                // Global snippet total after the attempt.
                let count_after = count_all_snippets(&pool).await;

                // Clean up before asserting so a failing case never leaks rows.
                // (No recipient/target workspace was seeded — resolution failed —
                // so only the requester's rows exist to remove.)
                cleanup(&pool, source_user, source_ws, source_user, source_ws).await;

                // Req 1.4: the share is rejected with a recipient-not-found error.
                match &share_result {
                    Err(AppError::RecipientNotFound) => {}
                    other => prop_assert!(
                        false,
                        "share to an unresolvable recipient ({:?}) must fail with \
                         RecipientNotFound, got {:?}",
                        recipient,
                        other
                    ),
                }

                // Req 1.4: no snippet was created anywhere — the global snippet
                // count is unchanged across all workspaces.
                prop_assert_eq!(
                    count_after,
                    count_before,
                    "global snippet count must be unchanged after a failed share \
                     (before={}, after={})",
                    count_before,
                    count_after
                );

                Ok(())
            })
        })
        .expect("Property 2: unresolvable recipient creates no snippet failed");
}

// ─── Property 3 helpers ─────────────────────────────────────────────────────

/// Count live (non-deleted) snippet rows in a single workspace. Property 3
/// asserts the RECIPIENT workspace's count is unchanged by a share that hits a
/// trigger conflict, so the count must be scoped to that one workspace.
async fn count_workspace_snippets(pool: &PgPool, workspace_id: Uuid) -> i64 {
    sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM snippets WHERE workspace_id = $1 AND deleted_at IS NULL",
    )
    .bind(workspace_id)
    .fetch_one(pool)
    .await
    .expect("Failed to count workspace snippets")
}

// ─── Property 3: Trigger conflict rejects the copy ──────────────────────────────
//
// Feature: ursnip-feature-matrix, Property 3: Trigger conflict rejects the copy
//
// For any source snippet whose trigger T already exists in the recipient's
// target workspace, a share request SHALL be rejected with a trigger-conflict
// error identifying the conflicting trigger T AND no copy SHALL be created (the
// recipient workspace's snippet count is unchanged).
//
// This drives the REAL `SharingService::share_snippet` path against a live
// database: seed a source snippet owned by the requester (so the read-access
// check passes) with trigger T, seed a DISTINCT recipient whose target workspace
// ALREADY contains a snippet with the same trigger T, snapshot the recipient
// workspace's snippet count, attempt the share, then assert the call fails with
// `TriggerAlreadyExists` (the recipient workspace's trigger-uniqueness rule,
// reused from `SyncService`, identifying T) and the recipient workspace's
// snippet count is identical before and after (the conflicting copy was never
// written — `SyncService::create_snippet` is transactional and commits only on
// success).
//
// **Validates: Requirements 2.1, 2.2**

#[test]
#[ignore]
fn prop_trigger_conflict_rejects_the_copy() {
    let rt = Runtime::new().expect("failed to build tokio runtime");
    let pool = rt.block_on(setup_test_db());

    let sync = Arc::new(SyncService::new(pool.clone()));
    let sharing = SharingService::new(pool.clone(), sync);

    let config = ProptestConfig {
        cases: 100,
        ..ProptestConfig::default()
    };

    let mut runner = proptest::test_runner::TestRunner::new(config);

    let strategy = (
        trigger_strategy(), // the conflicting trigger T (used by BOTH the source and the pre-existing recipient snippet)
        content_strategy(), // source content
        content_strategy(), // content of the pre-existing recipient snippet that occupies T
    );

    runner
        .run(&strategy, |(trigger, source_content, existing_content)| {
            rt.block_on(async {
                // Distinct source owner (the requester) and recipient, each with
                // their own individual workspace + active free subscription.
                let (source_user, source_ws) = seed_user_and_workspace(&pool).await;
                let (recipient_user, recipient_ws) = seed_user_and_workspace(&pool).await;

                // Seed the source snippet owned by the requester (with trigger T)
                // so the read-access check (owner bypass) passes.
                let source_snippet_id =
                    seed_snippet(&pool, source_ws, source_user, &trigger, &source_content).await;

                // Seed a PRE-EXISTING snippet in the recipient's target workspace
                // occupying the SAME trigger T — this is the conflict the share
                // must be rejected against (Req 2.1/2.2).
                let _existing_recipient_snippet_id = seed_snippet(
                    &pool,
                    recipient_ws,
                    recipient_user,
                    &trigger,
                    &existing_content,
                )
                .await;

                // Snapshot the recipient workspace's snippet count before the
                // attempt (it already contains exactly the pre-existing T).
                let count_before = count_workspace_snippets(&pool, recipient_ws).await;

                // ── Attempt to share the source (trigger T) into the recipient's
                // workspace, which already holds T. The recipient workspace's
                // trigger-uniqueness rule (reused from SyncService) must reject
                // this before any copy row is committed. ────────────────────────
                let share_result = sharing
                    .share_snippet(
                        source_user,
                        ShareRequest {
                            source_snippet_id,
                            recipient: recipient_user.to_string(),
                            target_workspace_id: recipient_ws,
                        },
                    )
                    .await;

                // Recipient workspace snippet count after the attempt.
                let count_after = count_workspace_snippets(&pool, recipient_ws).await;

                // Clean up before asserting so a failing case never leaks rows.
                cleanup(&pool, source_user, source_ws, recipient_user, recipient_ws).await;

                // Req 2.1/2.2: the share is rejected with a trigger-conflict error.
                // An exact-trigger collision surfaces as `TriggerAlreadyExists`
                // (the prefix-conflict variant is reserved for prefix relations),
                // which identifies the conflicting trigger T — the same trigger
                // the caller submitted and knows.
                match &share_result {
                    Err(AppError::TriggerAlreadyExists) => {}
                    other => prop_assert!(
                        false,
                        "share into a workspace already holding trigger {:?} must be \
                         rejected with TriggerAlreadyExists (identifying T), got {:?}",
                        trigger,
                        other
                    ),
                }

                // Req 2.2: no copy was created — the recipient workspace's snippet
                // count is unchanged (still just the pre-existing T).
                prop_assert_eq!(
                    count_after,
                    count_before,
                    "recipient workspace snippet count must be unchanged after a \
                     trigger-conflict rejection (before={}, after={})",
                    count_before,
                    count_after
                );

                Ok(())
            })
        })
        .expect("Property 3: trigger conflict rejects the copy failed");
}

// ─── Unit test: read-access authorization rejection (Task 4.7) ──────────────────
//
// A requester who has NO read access to the source snippet must be rejected with
// an authorization error (`ResourceAccessDenied` → HTTP 403) when attempting to
// share it, and NO shared copy may be created.
//
// This drives the REAL `SharingService::share_snippet` read-access check
// (`access::effective_access`) against a live database:
//   * Seed a source snippet owned by user A in A's workspace.
//   * Seed a DISTINCT requester B with their own workspace who is NOT A's
//     workspace owner and has NO accepted collaboration / access grant on the
//     source — so `effective_access` resolves to `None`.
//   * Seed a recipient C with a conflict-free workspace, so that if the
//     read-access check were (incorrectly) skipped, resolution + copy would
//     otherwise succeed and a copy WOULD be created — making this a real guard
//     against the authorization path, not merely a side effect of a later
//     failure.
//   * Attempt the share as B and assert it fails with `ResourceAccessDenied`
//     and the global snippet count is unchanged (no copy written).
//
// Unlike the property tests above, this is a single deterministic example, but it
// uses the same live-DB harness and is `#[ignore]`d by default for the same
// reason. Run it with:
//
// ```text
// docker compose -f docker-compose.test.yml up -d
// DATABASE_URL=postgres://ursnip_test:ursnip_test@localhost:5433/ursnip_test \
//   cargo test --test sharing_property_tests -- --ignored
// ```
//
// Requirements covered: 1.5

#[test]
#[ignore]
fn share_without_read_access_is_rejected_and_creates_no_copy() {
    let rt = Runtime::new().expect("failed to build tokio runtime");
    let pool = rt.block_on(setup_test_db());

    let sync = Arc::new(SyncService::new(pool.clone()));
    let sharing = SharingService::new(pool.clone(), sync);

    rt.block_on(async {
        // Source owner A and their workspace, plus a source snippet A owns.
        let (source_user, source_ws) = seed_user_and_workspace(&pool).await;
        let source_snippet_id =
            seed_snippet(&pool, source_ws, source_user, "readaccess.trigger", "secret").await;

        // Requester B: a distinct user with their own workspace. B is neither the
        // source workspace owner nor an accepted collaborator with any grant on
        // the source, so B has NO read access to it.
        let (requester_user, requester_ws) = seed_user_and_workspace(&pool).await;

        // Recipient C with a conflict-free (empty) workspace. Their presence means
        // that, absent the read-access guard, recipient resolution and the copy
        // would otherwise succeed — so the rejection is genuinely due to
        // authorization, not a downstream failure.
        let (recipient_user, recipient_ws) = seed_user_and_workspace(&pool).await;

        // Snapshot the global snippet total before the attempt.
        let count_before = count_all_snippets(&pool).await;

        // ── B (no read access) attempts to share A's snippet to C. ───────────
        let share_result = sharing
            .share_snippet(
                requester_user,
                ShareRequest {
                    source_snippet_id,
                    recipient: recipient_user.to_string(),
                    target_workspace_id: recipient_ws,
                },
            )
            .await;

        // Global snippet total after the attempt.
        let count_after = count_all_snippets(&pool).await;

        // Clean up all three users' workspaces before asserting so a failing case
        // never leaks rows into other tests.
        cleanup(&pool, source_user, source_ws, requester_user, requester_ws).await;
        cleanup(&pool, recipient_user, recipient_ws, recipient_user, recipient_ws).await;

        // Req 1.5: a requester without read access is rejected with the
        // authorization error (ResourceAccessDenied → HTTP 403).
        assert!(
            matches!(share_result, Err(AppError::ResourceAccessDenied)),
            "share by a requester without read access must be rejected with \
             ResourceAccessDenied (403), got {share_result:?}"
        );

        // Req 1.5: no shared copy was created — the global snippet count is
        // unchanged.
        assert_eq!(
            count_after, count_before,
            "no snippet may be created when the share is rejected for lack of read \
             access (before={count_before}, after={count_after})"
        );
    });
}
