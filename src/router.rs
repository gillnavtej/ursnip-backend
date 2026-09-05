//! Application router with route definitions and middleware layering.
//!
//! Defines all route groups and applies middleware in the correct order:
//!
//! **Global middleware** (outermost, runs first):
//! 1. trace_id — generates UUID, injects into extensions
//! 2. security_headers — adds security response headers
//! 3. cors — origin allowlist enforcement
//! 4. body_limit — 1 MB default request body limit
//! 5. panic_recovery — catches panics, returns 500
//! 6. ip_rate_limit — per-IP sliding window (100 req/min)
//!
//! **Per-group middleware** (applied to authenticated groups):
//! 1. auth_middleware — JWT validation, injects AccessTokenClaims
//! 2. client_type_guard — enforces native/web access rules
//! 3. subscription_context — injects SubscriptionContext
//! 4. user_rate_limit — per-user sliding window (500 req/min)
//! 5. admin_guard — (only on /admin) requires Admin role

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use axum::{
    extract::Extension,
    http::StatusCode,
    response::IntoResponse,
    routing::{get, post, put, delete, patch},
    Json, Router,
};
use sqlx::PgPool;

use crate::config::AppConfig;
use crate::middleware::body_limit::{body_limit_layer, DEFAULT_BODY_LIMIT, SYNC_BODY_LIMIT};
use crate::middleware::cors::cors_middleware;
use crate::middleware::panic_recovery::panic_recovery_layer;
use crate::middleware::rate_limit::{ip_rate_limit, user_rate_limit, RateLimiter};
use crate::middleware::{
    admin_guard, auth_middleware, client_type_guard, security_headers,
    subscription_context_middleware, trace_id_layer, workspace_context_guard,
};
use crate::admin::handlers::{
    cancel_subscription_handler as admin_cancel_subscription_handler,
    create_coupon_handler, create_discount_handler, create_feature_flag_handler,
    create_tax_rate_handler, deactivate_workspace_handler, delete_feature_flag_handler,
    delete_user_handler, delete_workspace_handler, demote_admin_handler,
    extend_subscription_handler, force_password_reset_handler, get_audit_log_handler,
    get_coupon_handler, get_referral_stats_handler, get_subscription_handler,
    get_user_handler, get_workspace_handler, list_admins_handler, list_audit_logs_handler,
    list_billing_events_handler, list_coupons_handler, list_discounts_handler,
    list_feature_flags_handler, list_subscriptions_handler, list_tax_rates_handler,
    list_users_handler, list_workspaces_handler, overview_stats_handler,
    override_tier_handler, referral_analytics_handler, suspend_user_handler,
    unsuspend_user_handler, update_coupon_handler, update_discount_handler,
    update_feature_flag_handler, update_tax_rate_handler,
};
use crate::admin::service::AdminService;
use crate::auth::handlers::{
    create_admin_invite_handler, register_handler, login_handler, refresh_handler,
    logout_handler, forgot_password_handler, reset_password_handler, update_profile_handler,
    get_profile_handler, change_email_handler, verify_email_change_handler, change_password_handler,
    delete_account_handler, list_sessions_handler, revoke_session_handler,
    oauth_authorize_handler, oauth_callback_handler, register_via_invite_handler,
    get_my_referrals_handler, verify_otp_handler, resend_otp_handler,
};
use crate::auth::oauth::OAuthService;
use crate::auth::service::AuthService;
use crate::sync::handlers::{
    batch_operations_handler, create_folder_handler, create_snippet_handler,
    delete_folder_handler, delete_snippet_handler, get_deltas_handler, get_snapshot_handler,
    update_folder_handler, update_snippet_handler, ws_handler,
    record_snippet_usage_handler, get_snippet_analytics_handler,
    get_platform_analytics_handler, restore_snippet_handler, hard_delete_snippet_handler,
    restore_folder_handler, hard_delete_folder_handler,
    get_data_sources_handler, save_data_sources_handler,
};
use crate::sync::service::SyncService;
use crate::sync::session_registry::SessionRegistry;
use crate::workspace::handlers::{
    create_invite_handler, create_team_handler, get_team_handler, join_via_invite_handler,
    list_members_handler, remove_member_handler,
    list_workspaces_handler as list_user_workspaces_handler, get_workspace_detail_handler,
    update_workspace_handler, delete_workspace_detail_handler, lookup_users_handler,
    get_member_permissions_handler, update_member_permissions_handler, reset_member_permissions_handler,
};
use crate::workspace::service::{TeamService, WorkspaceService};
use crate::ai::AiService;
use crate::ai::handlers::{expand_handler, generate_handler, improve_handler};
use crate::devices::handlers::{
    list_devices_handler, register_device_handler, update_device_handler, delete_device_handler,
};
use crate::devices::service::DeviceService;
use crate::notifications::handlers::{
    list_notifications_handler, mark_read_handler, mark_all_read_handler,
};
use crate::notifications::service::NotificationService;
use crate::subscription::handlers::{
    activate_subscription_handler, add_seats_handler, billing_webhook_handler,
    cancel_subscription_handler, checkout_handler, current_subscription_handler,
    list_transactions_handler, upgrade_handler,
};
use crate::subscription::service::SubscriptionService;

use crate::collaborators::handlers::{
    accept_invite_handler, delete_grant_handler, invite_handler, list_collaborators_handler,
    list_grants_handler, remove_collaborator_handler, upsert_grant_handler,
};
use crate::collaborators::CollaboratorService;

// ─── Application State ─────────────────────────────────────────────────────────

/// Shared application state accessible to all handlers and middleware.
#[derive(Clone)]
pub struct AppState {
    /// PostgreSQL connection pool.
    pub pool: PgPool,
    /// Application configuration.
    pub config: Arc<AppConfig>,
    /// Application-wide rate limiter.
    pub rate_limiter: RateLimiter,
    /// Readiness flag — set to true after all initialization (scheduler, email, etc.) is complete.
    pub ready: Arc<AtomicBool>,
    /// Session registry for WebSocket connections (used during graceful shutdown).
    pub session_registry: Arc<SessionRegistry>,
}

// ─── Router Builder ────────────────────────────────────────────────────────────

/// Builds the complete application router with all route groups and middleware.
///
/// # Arguments
///
/// * `state` - Shared application state containing pool, config, and rate limiter.
///
/// # Returns
///
/// A fully-configured `Router` ready to be served.
pub fn build_router(state: AppState) -> Router {
    let jwt_secret = Arc::new(state.config.jwt_secret.clone());
    let allowed_origins = Arc::new(state.config.cors_allowed_origins.clone());
    let rate_limiter = state.rate_limiter.clone();

    // ── Public routes (no auth required) ────────────────────────────────────
    let health_routes = Router::new()
        .route("/health", get(health_handler))
        .route("/ready", get(ready_handler))
        .route("/plans", get(plans_handler))
        .layer(Extension(state.pool.clone()))
        .layer(Extension(state.ready.clone()));

    // ── Auth service setup ─────────────────────────────────────────────────
    let auth_service = Arc::new(AuthService::new(state.pool.clone(), state.config.clone()));
    let oauth_service = Arc::new(OAuthService::new(state.pool.clone(), state.config.clone()));

    // ── Auth routes (public — no auth required) ─────────────────────────────
    let auth_routes = Router::new()
        .route("/auth/register", post(register_handler))
        .route("/auth/verify-otp", post(verify_otp_handler))
        .route("/auth/resend-otp", post(resend_otp_handler))
        .route("/auth/login", post(login_handler))
        .route("/auth/refresh", post(refresh_handler))
        .route("/auth/forgot-password", post(forgot_password_handler))
        .route("/auth/reset-password", post(reset_password_handler))
        .route("/auth/verify-email-change", get(verify_email_change_handler))
        .route("/auth/register-invite", post(register_via_invite_handler))
        .route("/auth/oauth/:provider/authorize", get(oauth_authorize_handler))
        .route("/auth/oauth/:provider/callback", get(oauth_callback_handler))
        .layer(Extension(auth_service.clone()))
        .layer(Extension(oauth_service))
        .layer(Extension(state.config.clone()));

    // ── Auth routes (protected — require valid JWT) ─────────────────────────
    let auth_protected_routes = Router::new()
        .route("/auth/logout", post(logout_handler))
        .route("/auth/profile", patch(update_profile_handler).get(get_profile_handler))
        .route("/auth/change-email", post(change_email_handler))
        .route("/auth/change-password", post(change_password_handler))
        .route("/auth/account", delete(delete_account_handler))
        .route("/auth/sessions", get(list_sessions_handler))
        .route("/auth/sessions/:session_id", delete(revoke_session_handler))
        .route("/auth/referrals", get(get_my_referrals_handler))
        .layer(Extension(auth_service.clone()))
        .layer(Extension(state.pool.clone()));

    // ── Webhook routes (no auth required, verified by signature) ─────────────
    let webhook_routes = Router::new()
        .route("/webhooks/billing", post(billing_webhook_handler))
        .layer(Extension(state.config.clone()))
        .layer(Extension(state.pool.clone()));

    // ── Sync routes (native only, elevated body limit, workspace-scoped RBAC) ──
    // Guarded routes require X-Workspace-ID header (reads from header or query param):
    let pool_for_sync = Arc::new(state.pool.clone());
    let sync_guarded_routes = Router::new()
        .route("/sync/snippets", post(create_snippet_handler))
        .route("/sync/snippets/:id", patch(update_snippet_handler))
        .route("/sync/snippets/:id", delete(delete_snippet_handler))
        .route("/sync/snippets/batch", post(batch_operations_handler))
        .route("/sync/snippets/:id/usage", post(record_snippet_usage_handler))
        .route("/sync/snippets/:id/analytics", get(get_snippet_analytics_handler))
        .route("/sync/snippets/:id/restore", patch(restore_snippet_handler))
        .route("/sync/snippets/:id/hard", delete(hard_delete_snippet_handler))
        .route("/sync/folders", post(create_folder_handler))
        .route("/sync/folders/:id", patch(update_folder_handler))
        .route("/sync/folders/:id", delete(delete_folder_handler))
        .route("/sync/folders/:id/restore", patch(restore_folder_handler))
        .route("/sync/folders/:id/hard", delete(hard_delete_folder_handler))
        .route("/sync/snapshot", get(get_snapshot_handler))
        .route("/sync/deltas", get(get_deltas_handler))
        .layer(Extension(state.pool.clone()))
        .layer(axum::middleware::from_fn(move |req, next| {
            workspace_context_guard(req, next, pool_for_sync.clone())
        }));

    // WebSocket + platform analytics routes bypass workspace context:
    let ws_state = crate::sync::websocket::WsState {
        registry: state.session_registry.clone(),
        sync_service: Arc::new(SyncService::new(state.pool.clone())),
        jwt_secret: jwt_secret.clone(),
    };
    // WS route is separate — it handles its own JWT auth via query param
    let ws_route = Router::new()
        .route("/sync/ws", get(crate::sync::websocket::ws_upgrade_handler).with_state(ws_state));

    let sync_unguarded_routes = Router::new()
        .route("/sync/analytics", get(get_platform_analytics_handler))
        .route("/sync/data-sources", get(get_data_sources_handler).put(save_data_sources_handler))
        .layer(Extension(state.pool.clone()));

    let notification_service = Arc::new(NotificationService::new(state.pool.clone()));

    let sync_routes = Router::new()
        .merge(sync_guarded_routes)
        .merge(sync_unguarded_routes)
        .layer(Extension(Arc::new(SyncService::new(state.pool.clone()))))
        .layer(Extension(state.session_registry.clone()))
        .layer(Extension(notification_service.clone()))
        .layer(body_limit_layer(SYNC_BODY_LIMIT));

    // ── AI routes (native only) ─────────────────────────────────────────────
    let ai_routes = Router::new()
        .route("/ai/expand", post(expand_handler))
        .route("/ai/generate", post(generate_handler))
        .route("/ai/improve", post(improve_handler))
        .layer(Extension(Arc::new(AiService::new(state.pool.clone(), state.config.clone()))));


    // ── Admin routes (web only, admin role required) ─────────────────────────
    let admin_routes = Router::new()
        // Users
        .route("/admin/users", get(list_users_handler))
        .route("/admin/users/:user_id", get(get_user_handler).delete(delete_user_handler))
        .route("/admin/users/:user_id/suspend", post(suspend_user_handler))
        .route("/admin/users/:user_id/unsuspend", post(unsuspend_user_handler))
        .route("/admin/users/:user_id/force-password-reset", post(force_password_reset_handler))
        // Workspaces
        .route("/admin/workspaces", get(list_workspaces_handler))
        .route("/admin/workspaces/:workspace_id", get(get_workspace_handler).delete(delete_workspace_handler))
        .route("/admin/workspaces/:workspace_id/deactivate", post(deactivate_workspace_handler))
        // Discounts
        .route("/admin/discounts", get(list_discounts_handler).post(create_discount_handler))
        .route("/admin/discounts/:id", patch(update_discount_handler))
        // Coupons
        .route("/admin/coupons", get(list_coupons_handler).post(create_coupon_handler))
        .route("/admin/coupons/:id", get(get_coupon_handler).patch(update_coupon_handler))
        // Referrals
        .route("/admin/referrals", get(get_referral_stats_handler))
        // Subscriptions
        .route("/admin/subscriptions", get(list_subscriptions_handler))
        .route("/admin/subscriptions/:id", get(get_subscription_handler))
        .route("/admin/subscriptions/:id/extend", post(extend_subscription_handler))
        .route("/admin/subscriptions/:id/cancel", post(admin_cancel_subscription_handler))
        .route("/admin/subscriptions/:id/tier", patch(override_tier_handler))
        // Billing Events
        .route("/admin/billing-events", get(list_billing_events_handler))
        // Tax Rates
        .route("/admin/tax-rates", get(list_tax_rates_handler).post(create_tax_rate_handler))
        .route("/admin/tax-rates/:country_code", patch(update_tax_rate_handler))
        // Audit Logs
        .route("/admin/audit-logs", get(list_audit_logs_handler))
        .route("/admin/audit-logs/:id", get(get_audit_log_handler))
        // Feature Flags
        .route("/admin/feature-flags", get(list_feature_flags_handler).post(create_feature_flag_handler))
        .route("/admin/feature-flags/:name", put(update_feature_flag_handler).delete(delete_feature_flag_handler))
        // Admin Management
        .route("/admin/admins", get(list_admins_handler))
        .route("/admin/admins/:admin_id", delete(demote_admin_handler))
        // Admin Invites (calls auth service)
        .route("/admin/invites", post(create_admin_invite_handler))
        // Stats
        .route("/admin/stats/overview", get(overview_stats_handler))
        .route("/admin/stats/referrals", get(referral_analytics_handler))
        .layer(Extension(Arc::new(AdminService::new(state.pool.clone(), Some(state.session_registry.clone())))))
        .layer(Extension(Arc::new(AuthService::new(state.pool.clone(), state.config.clone()))))
        .layer(Extension(state.pool.clone()))
        .layer(axum::middleware::from_fn(admin_guard));

    // ── Subscription routes (web only, workspace-scoped RBAC) ─────────────────
    let pool_for_subs = Arc::new(state.pool.clone());
    let subscription_routes = Router::new()
        .route("/subscriptions/upgrade", post(upgrade_handler))
        .route("/subscriptions/checkout", post(checkout_handler))
        .route("/subscriptions/activate", post(activate_subscription_handler))
        .route("/subscriptions/cancel", post(cancel_subscription_handler))
        .route("/subscriptions/add-seats", post(add_seats_handler))
        .route("/subscriptions/current", get(current_subscription_handler))
        .route("/subscriptions/transactions", get(list_transactions_handler))
        .layer(Extension(Arc::new(SubscriptionService::new(state.pool.clone(), state.config.clone()))))
        .layer(Extension(state.pool.clone()))
        .layer(Extension(state.config.clone()))
        .layer(axum::middleware::from_fn(move |req, next| {
            workspace_context_guard(req, next, pool_for_subs.clone())
        }));

    // ── Team routes (web only, workspace-scoped RBAC) ─────────────────────────
    // Routes that require workspace membership:
    let pool_for_teams = Arc::new(state.pool.clone());
    let team_guarded_routes = Router::new()
        .route("/teams/:workspace_id", get(get_team_handler))
        .route("/teams/:workspace_id/invites", post(create_invite_handler))
        .route("/teams/:workspace_id/members", get(list_members_handler))
        .route(
            "/teams/:workspace_id/members/:user_id",
            delete(remove_member_handler),
        )
        .route(
            "/teams/:workspace_id/members/:user_id/permissions",
            get(get_member_permissions_handler)
                .patch(update_member_permissions_handler)
                .delete(reset_member_permissions_handler),
        )
        .layer(axum::middleware::from_fn(move |req, next| {
            workspace_context_guard(req, next, pool_for_teams.clone())
        }));

    // Routes that don't require existing membership (create workspace, join via invite):
    let team_unguarded_routes = Router::new()
        .route("/teams", post(create_team_handler))
        .route("/teams/:workspace_id/join", post(join_via_invite_handler));

    let team_routes = Router::new()
        .merge(team_guarded_routes)
        .merge(team_unguarded_routes)
        .layer(Extension(Arc::new(WorkspaceService::new(state.pool.clone()))))
        .layer(Extension(Arc::new(TeamService::new(state.pool.clone()))))
        .layer(Extension(state.session_registry.clone()));

    // ── Workspace CRUD routes (web, workspace-scoped RBAC where needed) ─────
    let pool_for_workspaces = Arc::new(state.pool.clone());
    // Guarded routes require workspace membership:
    let workspace_guarded_routes = Router::new()
        .route("/workspaces/:id", get(get_workspace_detail_handler))
        .route("/workspaces/:id", patch(update_workspace_handler))
        .route("/workspaces/:id", delete(delete_workspace_detail_handler))
        .layer(axum::middleware::from_fn(move |req, next| {
            workspace_context_guard(req, next, pool_for_workspaces.clone())
        }));

    // Unguarded routes (user-scoped, no workspace context needed):
    let workspace_unguarded_routes = Router::new()
        .route("/workspaces", get(list_user_workspaces_handler))
        .route("/users/lookup", get(lookup_users_handler))
        .layer(Extension(state.pool.clone()));

    let workspace_routes = Router::new()
        .merge(workspace_guarded_routes)
        .merge(workspace_unguarded_routes)
        .layer(Extension(Arc::new(WorkspaceService::new(state.pool.clone()))));

    // ── Device routes (workspace-scoped RBAC for list, unguarded for register/update/delete) ──
    let pool_for_devices = Arc::new(state.pool.clone());
    let device_guarded_routes = Router::new()
        .route("/devices", get(list_devices_handler))
        .layer(axum::middleware::from_fn(move |req, next| {
            workspace_context_guard(req, next, pool_for_devices.clone())
        }));

    let device_unguarded_routes = Router::new()
        .route("/devices", post(register_device_handler))
        .route("/devices/:id", patch(update_device_handler))
        .route("/devices/:id", delete(delete_device_handler));

    let device_routes = Router::new()
        .merge(device_guarded_routes)
        .merge(device_unguarded_routes)
        .layer(Extension(Arc::new(DeviceService::new(state.pool.clone()))))
        .layer(Extension(state.session_registry.clone()));

    // ── Notification routes ─────────────────────────────────────────────────
    let notification_routes = Router::new()
        .route("/notifications", get(list_notifications_handler))
        .route("/notifications/read-all", patch(mark_all_read_handler))
        .route("/notifications/:id/read", patch(mark_read_handler))
        .layer(Extension(notification_service.clone()));

    // ── Collaborator routes (account-level, NOT workspace-scoped) ────────────
    // Teams gating and account-owner checks are enforced inside the service;
    // `accept` is usable by any authenticated user (matched by their email).
    let collaborator_routes = Router::new()
        .route("/collaborators/invite", post(invite_handler))
        .route("/collaborators", get(list_collaborators_handler))
        .route("/collaborators/accept", post(accept_invite_handler))
        .route(
            "/collaborators/grants/:grant_id",
            delete(delete_grant_handler),
        )
        .route(
            "/collaborators/:user_id/grants",
            get(list_grants_handler).post(upsert_grant_handler),
        )
        .route(
            "/collaborators/:user_id",
            delete(remove_collaborator_handler),
        )
        .layer(Extension(Arc::new(CollaboratorService::new(
            state.pool.clone(),
            state.config.clone(),
        ))))
        .layer(Extension(state.pool.clone()));

    // ── Authenticated routes (with per-group middleware) ─────────────────────
    // These route groups require authentication and per-user rate limiting.
    let jwt_secret_clone = jwt_secret.clone();
    let authenticated_routes = Router::new()
        .merge(sync_routes)
        .merge(ai_routes)
        .merge(admin_routes)
        .merge(subscription_routes)
        .merge(team_routes)
        .merge(workspace_routes)
        .merge(device_routes)
        .merge(notification_routes)
        .merge(collaborator_routes)
        .merge(auth_protected_routes)
        .layer(axum::middleware::from_fn_with_state(
            rate_limiter.clone(),
            user_rate_limit,
        ))
        .layer(axum::middleware::from_fn(subscription_context_middleware))
        .layer(axum::middleware::from_fn(client_type_guard))
        .layer(axum::middleware::from_fn(move |req, next| {
            auth_middleware(req, next, jwt_secret_clone.clone())
        }));

    // ── Merge all route groups ──────────────────────────────────────────────
    let app = Router::new()
        .merge(health_routes)
        .merge(auth_routes)
        .merge(webhook_routes)
        .merge(ws_route)
        .merge(authenticated_routes);

    // ── Global middleware (applied outermost, runs first) ───────────────────
    // Order matters: layers are applied bottom-up, so the last .layer() runs first.
    // We want: trace_id → security_headers → cors → body_limit → panic_recovery → ip_rate_limit
    // So we apply them in reverse order.
    let origins_clone = allowed_origins.clone();
    let app = app
        .layer(axum::middleware::from_fn_with_state(
            rate_limiter.clone(),
            ip_rate_limit,
        ))
        .layer(axum::middleware::from_fn(panic_recovery_layer))
        .layer(body_limit_layer(DEFAULT_BODY_LIMIT))
        .layer(axum::middleware::from_fn(move |req, next| {
            cors_middleware(origins_clone.clone(), req, next)
        }))
        .layer(axum::middleware::from_fn(security_headers))
        .layer(axum::middleware::from_fn(trace_id_layer));

    app.with_state(rate_limiter)
}

// ─── Health & Readiness Handlers ────────────────────────────────────────────────

/// Health (liveness) check handler.
///
/// Verifies Postgres reachability by executing `SELECT 1`.
/// - On success: returns `{"status": "ok", "db": "ok"}` with HTTP 200.
/// - On failure: returns `{"status": "degraded", "db": "error"}` with HTTP 503.
async fn health_handler(Extension(pool): Extension<PgPool>) -> impl IntoResponse {
    let db_ok = sqlx::query_scalar::<_, i32>("SELECT 1")
        .fetch_one(&pool)
        .await
        .is_ok();

    if db_ok {
        (
            StatusCode::OK,
            Json(serde_json::json!({ "status": "ok", "db": "ok" })),
        )
    } else {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({ "status": "degraded", "db": "error" })),
        )
    }
}

/// Readiness check handler.
///
/// Checks:
/// - Database: pool can execute `SELECT 1`
/// - Scheduler: readiness flag has been set (covers scheduler + email init)
///
/// When ALL checks pass: returns HTTP 200 with `{"status": "ready"}`.
/// When ANY check fails: returns HTTP 503 with per-check detail.
async fn ready_handler(
    Extension(pool): Extension<PgPool>,
    Extension(ready_flag): Extension<Arc<AtomicBool>>,
) -> impl IntoResponse {
    let db_ok = sqlx::query_scalar::<_, i32>("SELECT 1")
        .fetch_one(&pool)
        .await
        .is_ok();

    let services_ready = ready_flag.load(Ordering::Relaxed);

    if db_ok && services_ready {
        (
            StatusCode::OK,
            Json(serde_json::json!({ "status": "ready" })),
        )
    } else {
        let db_status = if db_ok { "ok" } else { "pending" };
        let scheduler_status = if services_ready { "ok" } else { "pending" };
        let email_status = if services_ready { "ok" } else { "pending" };

        (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({
                "status": "not_ready",
                "checks": {
                    "db": db_status,
                    "scheduler": scheduler_status,
                    "email": email_status
                }
            })),
        )
    }
}

/// Placeholder handler for unimplemented endpoints.
/// Returns 501 Not Implemented.
async fn todo_handler() -> StatusCode {
    StatusCode::NOT_IMPLEMENTED
}

/// GET /plans
///
/// Returns the static list of available subscription plans.
/// Public endpoint — no auth required.
async fn plans_handler() -> impl IntoResponse {
    let plans = serde_json::json!([
        {
            "id": "free",
            "name": "Free",
            "price_monthly": 0,
            "price_yearly": 0,
            "currency": "USD",
            "features": {
                "max_snippets": 10,
                "max_folders": 3,
                "max_devices": 2,
                "max_workspaces": 1,
                "ai_requests_per_day": 50,
                "sync": true,
                "realtime_sync": false,
                "team_members": 1,
                "audit_logs": false
            }
        },
        {
            "id": "pro",
            "name": "Pro",
            "price_monthly": 9.99,
            "price_yearly": 99.99,
            "currency": "USD",
            "features": {
                "max_snippets": -1,
                "max_folders": -1,
                "max_devices": 5,
                "max_workspaces": 3,
                "ai_requests_per_day": 1000,
                "sync": true,
                "realtime_sync": true,
                "team_members": 1,
                "audit_logs": false
            }
        },
        {
            "id": "teams",
            "name": "Teams",
            "price_monthly": 29.00,
            "price_yearly": 290.00,
            "currency": "USD",
            "per_user": true,
            "features": {
                "max_snippets": -1,
                "max_folders": -1,
                "max_devices": -1,
                "max_workspaces": -1,
                "ai_requests_per_day": 1000,
                "sync": true,
                "realtime_sync": true,
                "team_members": -1,
                "rbac": true,
                "audit_logs": true,
                "sso": false
            }
        }
    ]);
    (StatusCode::OK, Json(plans))
}
