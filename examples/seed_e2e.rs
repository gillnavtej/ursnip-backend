//! Idempotent E2E test-fixture seeder.
//!
//! Creates the deterministic user + workspace the live-backend Playwright specs
//! (`ursnip-app/e2e/{full-ui-test,admin-and-features,snippet-toolbar}.spec.ts`)
//! expect:
//!
//!   - user  `testuser@example.com` / `password123`  (role=user, email_verified)
//!   - its individual workspace, id = `687af4e9-48a7-44a5-bf93-18d939276881`
//!   - an owner `workspace_members` row + a `pro` subscription on that workspace
//!
//! The admin account (`admin@ursnip.com` / configured `SEED_ADMIN_PASSWORD`) is
//! seeded by the backend itself on boot (see `main.rs::seed_admin_password`), so
//! it is intentionally not created here.
//!
//! Idempotent: safe to run repeatedly. Re-running refreshes the password hash and
//! verified flag but never duplicates rows.
//!
//! Usage (from `ursnip-backend/`):
//!   DATABASE_URL=postgres://<user>@localhost:5432/ursnip_test \
//!     cargo run --example seed_e2e

use sqlx::postgres::PgPoolOptions;
use ursnip_backend::auth::password::hash_password;
use uuid::Uuid;

const TEST_EMAIL: &str = "testuser@example.com";
const TEST_PASSWORD: &str = "password123";
const WORKSPACE_ID: &str = "687af4e9-48a7-44a5-bf93-18d939276881";

#[tokio::main]
async fn main() {
    dotenvy::dotenv().ok();

    let database_url = std::env::var("DATABASE_URL")
        .expect("DATABASE_URL must be set (point it at the E2E test database)");

    let pool = PgPoolOptions::new()
        .max_connections(2)
        .connect(&database_url)
        .await
        .expect("failed to connect to the E2E database");

    let workspace_id = Uuid::parse_str(WORKSPACE_ID).expect("valid workspace UUID");
    let password_hash = hash_password(TEST_PASSWORD).expect("hash the test password");

    // ── 1. Upsert the test user (verified, active, role=user) ────────────────
    // referral_code is NOT NULL + unique; only set it on first insert.
    let user_id: Uuid = sqlx::query_scalar(
        r#"
        INSERT INTO users (email, password_hash, first_name, last_name, role, status,
                           referral_code, must_reset_password, email_verified)
        VALUES ($1, $2, 'Test', 'User', 'user', 'active', $3, FALSE, TRUE)
        ON CONFLICT (email) DO UPDATE
            SET password_hash = EXCLUDED.password_hash,
                status        = 'active',
                email_verified = TRUE,
                deleted_at    = NULL
        RETURNING id
        "#,
    )
    .bind(TEST_EMAIL)
    .bind(&password_hash)
    .bind(format!("e2e-{}", &WORKSPACE_ID[..8]))
    .fetch_one(&pool)
    .await
    .expect("upsert test user");

    // ── 2. Upsert the individual workspace at the fixed id ───────────────────
    sqlx::query(
        r#"
        INSERT INTO workspaces (id, type, owner_id, name)
        VALUES ($1, 'individual', $2, 'Test Workspace')
        ON CONFLICT (id) DO UPDATE
            SET owner_id = EXCLUDED.owner_id,
                type     = 'individual'
        "#,
    )
    .bind(workspace_id)
    .bind(user_id)
    .execute(&pool)
    .await
    .expect("upsert workspace");

    // ── 3. Owner membership ──────────────────────────────────────────────────
    sqlx::query(
        r#"
        INSERT INTO workspace_members (workspace_id, user_id, role)
        VALUES ($1, $2, 'owner')
        ON CONFLICT (workspace_id, user_id) DO UPDATE SET role = 'owner'
        "#,
    )
    .bind(workspace_id)
    .bind(user_id)
    .execute(&pool)
    .await
    .expect("upsert workspace member");

    // ── 4. Free subscription on the workspace (idempotent) ───────────────────
    // The live-backend Playwright specs assume testuser is on the FREE tier: the
    // billing page only renders the "Choose Your Plan" cards for free/non-active
    // subscriptions (paid users see a subscription-summary view instead). Seeding
    // `free` keeps those billing assertions valid while still exercising real
    // tier-limit behavior on snippet creation.
    let has_sub: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM subscriptions WHERE workspace_id = $1)",
    )
    .bind(workspace_id)
    .fetch_one(&pool)
    .await
    .expect("check subscription");

    if has_sub {
        sqlx::query("UPDATE subscriptions SET tier = 'free', status = 'active' WHERE workspace_id = $1")
            .bind(workspace_id)
            .execute(&pool)
            .await
            .expect("update subscription");
    } else {
        sqlx::query(
            "INSERT INTO subscriptions (workspace_id, tier, status) VALUES ($1, 'free', 'active')",
        )
        .bind(workspace_id)
        .execute(&pool)
        .await
        .expect("insert subscription");
    }

    // ── 5. Reset workspace content for a clean, repeatable run ───────────────
    // The live-backend specs create folders/snippets; on the free tier those hit
    // hard caps (e.g. 3 folders). Because the E2E database persists between runs,
    // purge prior test content so every run starts from an empty workspace.
    // Hard-delete (not soft) so the unique (workspace, trigger) index is freed too.
    let del_snippets = sqlx::query("DELETE FROM snippets WHERE workspace_id = $1")
        .bind(workspace_id)
        .execute(&pool)
        .await
        .expect("purge snippets")
        .rows_affected();
    let del_folders = sqlx::query("DELETE FROM folders WHERE workspace_id = $1")
        .bind(workspace_id)
        .execute(&pool)
        .await
        .expect("purge folders")
        .rows_affected();
    // Clear the sync delta log so a fresh client snapshot/delta cycle starts clean.
    sqlx::query("DELETE FROM sync_deltas WHERE workspace_id = $1")
        .bind(workspace_id)
        .execute(&pool)
        .await
        .expect("purge sync_deltas");

    println!("✓ E2E seed complete");
    println!("  user:      {TEST_EMAIL} / {TEST_PASSWORD}  (id={user_id})");
    println!("  workspace: {WORKSPACE_ID} (individual, free)");
    println!("  reset:     deleted {del_snippets} snippet(s), {del_folders} folder(s)");
}
