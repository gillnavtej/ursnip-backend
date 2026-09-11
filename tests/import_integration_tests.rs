//! Integration test for import ingestion via the batch endpoint (task 5.4).
//!
//! Exercises the full end-to-end import path required by Requirement 4.2:
//! a raw JSON import payload is parsed by [`ImportService::parse`] into
//! candidate snippets, those candidates are mapped onto `create_snippet`
//! batch operations and POSTed to the existing `POST /sync/snippets/batch`
//! endpoint through the Axum router, and the resulting rows are asserted to
//! appear in the target workspace (both in the endpoint response and directly
//! in the `snippets` table).
//!
//! This mirrors the frontend/import wiring of task 5.2: candidates are turned
//! into `{ "type": "create_snippet", ... }` batch operations, exactly the shape
//! the `Batch_Sync_Endpoint` deserializes.
//!
//! Requires a running PostgreSQL instance. Set the DATABASE_URL environment
//! variable. The suite applies migrations fresh and seeds test data per test,
//! matching `sync_integration_tests.rs`.
//!
//! Run with: `cargo test --test import_integration_tests -- --ignored`
//!
//! Requirements covered: 4.2

use std::sync::Arc;

use axum::{
    body::Body,
    extract::Extension,
    http::{Request, StatusCode},
    routing::post,
    Router,
};
use chrono::Utc;
use serde_json::{json, Value};
use sqlx::PgPool;
use tower::ServiceExt;
use uuid::Uuid;

use ursnip_backend::middleware::auth_extractor::AccessTokenClaims;
use ursnip_backend::middleware::workspace_context::{WorkspaceContext, WorkspaceRole};
use ursnip_backend::models::common::{ClientType, Role, Tier};
use ursnip_backend::sync::handlers::batch_operations_handler;
use ursnip_backend::sync::import::{CandidateSnippet, ImportService};
use ursnip_backend::sync::service::SyncService;
use ursnip_backend::sync::session_registry::SessionRegistry;

// ─── Test Helpers ───────────────────────────────────────────────────────────────

/// Build a minimal test router exposing just the batch endpoint under test,
/// injecting the SyncService and SessionRegistry as the real router does.
fn build_test_router(pool: PgPool) -> Router {
    let sync_service = Arc::new(SyncService::new(pool));
    let session_registry = Arc::new(SessionRegistry::new(10_000));

    Router::new()
        .route("/sync/snippets/batch", post(batch_operations_handler))
        .layer(Extension(sync_service))
        .layer(Extension(session_registry))
}

/// Create default test claims for a native Pro-tier user. The batch endpoint's
/// tier gate reads the subscription from the database, but claims are kept
/// consistent with the seeded Pro subscription.
fn test_claims(user_id: Uuid) -> AccessTokenClaims {
    AccessTokenClaims {
        sub: user_id,
        client_type: ClientType::Native,
        role: Role::User,
        permissions: vec![],
        subscription_tier: Tier::Pro,
        status: "active".to_string(),
        must_reset_password: false,
        exp: Utc::now().timestamp() + 3600,
    }
}

/// Owner-level workspace context, as `auth_middleware` + `workspace_context_guard`
/// would inject for the workspace owner. The batch handler requires this
/// extension and checks `PERM_SNIPPETS_CREATE`, which Owner satisfies.
fn owner_ctx(user_id: Uuid, workspace_id: Uuid) -> WorkspaceContext {
    WorkspaceContext {
        workspace_id,
        user_id,
        role: WorkspaceRole::Owner,
        permissions: None,
    }
}

/// Build a JSON POST request with claims and workspace context injected into
/// extensions, mirroring the middleware chain the real router runs.
fn post_json(
    uri: &str,
    body: Value,
    claims: &AccessTokenClaims,
    ws_ctx: &WorkspaceContext,
) -> Request<Body> {
    let mut req = Request::builder()
        .method("POST")
        .uri(uri)
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();
    req.extensions_mut().insert(claims.clone());
    req.extensions_mut().insert(ws_ctx.clone());
    req
}

/// Extract JSON body from a response.
async fn body_json(response: axum::response::Response) -> Value {
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    serde_json::from_slice(&bytes).unwrap_or(Value::Null)
}

/// Map parsed import candidates onto a `POST /sync/snippets/batch` request body,
/// each becoming a `create_snippet` operation (task 5.2 wiring). This is the
/// exact shape `BatchOperation` deserializes (`#[serde(tag = "type", rename_all
/// = "snake_case")]`).
fn candidates_to_batch_body(workspace_id: Uuid, candidates: &[CandidateSnippet]) -> Value {
    let operations: Vec<Value> = candidates
        .iter()
        .map(|c| {
            json!({
                "type": "create_snippet",
                "workspace_id": workspace_id,
                "trigger": c.trigger,
                "content": c.content,
                "snippet_type": c.snippet_type,
                "ai_prompt": c.ai_prompt,
            })
        })
        .collect();

    json!({
        "workspace_id": workspace_id,
        "operations": operations,
    })
}

/// Initialize a test database pool and run migrations.
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

/// Seed a test user and workspace with a free subscription.
/// Returns (user_id, workspace_id).
async fn seed_user_and_workspace(pool: &PgPool) -> (Uuid, Uuid) {
    let user_id = Uuid::new_v4();
    let workspace_id = Uuid::new_v4();
    let referral_code = format!("REF{}", &user_id.to_string()[..8]);

    sqlx::query(
        r#"INSERT INTO users (id, email, password_hash, role, status, referral_code)
           VALUES ($1, $2, $3, 'user', 'active', $4)"#,
    )
    .bind(user_id)
    .bind(format!("test_{}@example.com", &user_id.to_string()[..8]))
    .bind("$argon2id$v=19$m=65536,t=3,p=4$hash")
    .bind(&referral_code)
    .execute(pool)
    .await
    .expect("Failed to create test user");

    sqlx::query(
        r#"INSERT INTO workspaces (id, type, owner_id, name)
           VALUES ($1, 'individual', $2, 'Test Workspace')"#,
    )
    .bind(workspace_id)
    .bind(user_id)
    .execute(pool)
    .await
    .expect("Failed to create test workspace");

    sqlx::query(
        r#"INSERT INTO workspace_members (workspace_id, user_id, role)
           VALUES ($1, $2, 'owner')"#,
    )
    .bind(workspace_id)
    .bind(user_id)
    .execute(pool)
    .await
    .expect("Failed to create workspace membership");

    // The batch endpoint (`POST /sync/snippets/batch`) requires a Pro or Teams
    // tier — free-tier workspaces are rejected with 402. Import ingestion reuses
    // this endpoint, so the workspace is seeded on a paid tier.
    sqlx::query(
        r#"INSERT INTO subscriptions (workspace_id, tier, status)
           VALUES ($1, 'pro', 'active')"#,
    )
    .bind(workspace_id)
    .execute(pool)
    .await
    .expect("Failed to create test subscription");

    (user_id, workspace_id)
}

/// Clean up test data after each test.
async fn cleanup(pool: &PgPool, user_id: Uuid, workspace_id: Uuid) {
    let _ = sqlx::query("DELETE FROM sync_deltas WHERE workspace_id = $1")
        .bind(workspace_id)
        .execute(pool)
        .await;
    let _ = sqlx::query("DELETE FROM snippet_folders WHERE snippet_id IN (SELECT id FROM snippets WHERE workspace_id = $1)")
        .bind(workspace_id)
        .execute(pool)
        .await;
    let _ = sqlx::query("DELETE FROM snippets WHERE workspace_id = $1")
        .bind(workspace_id)
        .execute(pool)
        .await;
    let _ = sqlx::query("DELETE FROM subscriptions WHERE workspace_id = $1")
        .bind(workspace_id)
        .execute(pool)
        .await;
    let _ = sqlx::query("DELETE FROM workspace_members WHERE workspace_id = $1")
        .bind(workspace_id)
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

// ─── Test: Import ingestion via the batch endpoint ──────────────────────────────
// Requirements: 4.2 — a parsed import payload posted to POST /sync/snippets/batch
// results in the snippet rows appearing in the target workspace.

#[tokio::test]
#[ignore]
async fn test_import_ingests_snippets_via_batch_endpoint() {
    let pool = setup_test_db().await;
    let (user_id, workspace_id) = seed_user_and_workspace(&pool).await;
    let app = build_test_router(pool.clone());
    let claims = test_claims(user_id);

    // A canonical import payload, as would arrive from an exported JSON file.
    let import_payload = r#"{
        "version": 1,
        "workspace_name": "Imported Set",
        "snippets": [
            {"trigger": "/hello", "content": "Hello, world!", "snippet_type": "static", "ai_prompt": null},
            {"trigger": "/sig", "content": "Best regards", "snippet_type": "static", "ai_prompt": null},
            {"trigger": "/thx", "content": "Thanks so much!", "snippet_type": "static", "ai_prompt": null}
        ]
    }"#;

    // 1. Parse the import payload into candidate snippets (Req 4.1).
    let candidates = ImportService::parse(import_payload).expect("valid import payload should parse");
    assert_eq!(candidates.len(), 3);

    // 2. Map candidates to create_snippet ops and POST to the batch endpoint (Req 4.2).
    let batch_body = candidates_to_batch_body(workspace_id, &candidates);
    let ws_ctx = owner_ctx(user_id, workspace_id);
    let resp = app
        .oneshot(post_json("/sync/snippets/batch", batch_body, &claims, &ws_ctx))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let json = body_json(resp).await;
    let results = json["results"].as_array().unwrap();
    assert_eq!(results.len(), 3, "all three imported snippets should be ingested");
    // Sequential version assignment across the batch.
    assert_eq!(results[0]["version"], 1);
    assert_eq!(results[1]["version"], 2);
    assert_eq!(results[2]["version"], 3);
    assert_eq!(json["workspace_version"], 3);

    // 3. Assert the rows actually appear in the workspace's snippets table
    //    (the columns the batch create path persists: trigger, content, type).
    let rows: Vec<(String, String, String)> = sqlx::query_as(
        r#"SELECT trigger, content, snippet_type
           FROM snippets
           WHERE workspace_id = $1 AND deleted_at IS NULL
           ORDER BY trigger"#,
    )
    .bind(workspace_id)
    .fetch_all(&pool)
    .await
    .expect("query imported snippets");

    assert_eq!(rows.len(), 3, "three snippet rows should exist in the workspace");
    assert_eq!(
        rows,
        vec![
            ("/hello".to_string(), "Hello, world!".to_string(), "static".to_string()),
            ("/sig".to_string(), "Best regards".to_string(), "static".to_string()),
            ("/thx".to_string(), "Thanks so much!".to_string(), "static".to_string()),
        ]
    );

    // Every ingested row is owned by the importing user in the target workspace.
    let owned_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM snippets WHERE workspace_id = $1 AND created_by = $2 AND deleted_at IS NULL",
    )
    .bind(workspace_id)
    .bind(user_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(owned_count, 3);

    cleanup(&pool, user_id, workspace_id).await;
}
