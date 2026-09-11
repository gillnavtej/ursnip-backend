//! HTTP handlers for subscription and billing webhook endpoints.
//!
//! Provides handlers for:
//! - POST /subscriptions/upgrade — initiate upgrade from free to pro
//! - POST /subscriptions/checkout — process checkout with invoice computation
//! - GET /subscriptions/current — get current subscription for a workspace
//! - POST /webhooks/billing — process billing provider webhook events

use std::sync::Arc;

use axum::{
    body::Bytes,
    extract::{Extension, Query},
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    Json,
};
use serde::Deserialize;
use sqlx::PgPool;
use uuid::Uuid;

use crate::config::AppConfig;
use crate::errors::AppError;
use crate::middleware::auth_extractor::AccessTokenClaims;
use crate::middleware::workspace_context::{
    check_role_owner, check_role_viewer, WorkspaceContext,
};
use crate::notifications::service::{persist_user_notification_async, NotificationService};
use crate::subscription::service::{CheckoutRequest, SubscriptionService};
use crate::subscription::webhook;

// ─── Internal Row Types ─────────────────────────────────────────────────────────

#[derive(Debug, sqlx::FromRow)]
struct TransactionRow {
    pub id: Uuid,
    pub external_transaction_id: String,
    pub tier: String,
    pub billing_cycle_months: i32,
    pub base_price: rust_decimal::Decimal,
    pub discount_amount: rust_decimal::Decimal,
    pub tax_amount: rust_decimal::Decimal,
    pub total_amount: rust_decimal::Decimal,
    pub currency: String,
    pub status: String,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub completed_at: Option<chrono::DateTime<chrono::Utc>>,
}

// ─── Request/Response DTOs ──────────────────────────────────────────────────────

/// Request body for POST /subscriptions/upgrade.
#[derive(Debug, Deserialize)]
pub struct UpgradeRequest {
    pub workspace_id: Uuid,
}

/// Request body for POST /subscriptions/activate.
#[derive(Debug, Deserialize)]
pub struct ActivateRequest {
    pub workspace_id: Uuid,
}

/// Request body for POST /subscriptions/add-seats.
#[derive(Debug, Deserialize)]
pub struct AddSeatsRequest {
    pub workspace_id: Uuid,
    pub seats: i32,
}

/// Request body for POST /subscriptions/checkout.
#[derive(Debug, Deserialize)]
pub struct CheckoutRequestBody {
    pub workspace_id: Uuid,
    pub tier: String,
    pub billing_cycle_months: i32,
    pub coupon_code: Option<String>,
    pub discount_id: Option<Uuid>,
    pub country_code: Option<String>,
    pub success_url: Option<String>,
    pub cancel_url: Option<String>,
}

/// Query parameters for GET /subscriptions/current.
#[derive(Debug, Deserialize)]
pub struct CurrentSubscriptionQuery {
    pub workspace_id: Uuid,
}

// ─── Handlers ───────────────────────────────────────────────────────────────────

/// POST /subscriptions/upgrade
///
/// Initiate an upgrade from free to pro tier for the given workspace.
/// Returns 200 OK on success with a status message.
pub async fn upgrade_handler(
    Extension(service): Extension<Arc<SubscriptionService>>,
    Extension(_claims): Extension<AccessTokenClaims>,
    Extension(ws_ctx): Extension<WorkspaceContext>,
    Json(body): Json<UpgradeRequest>,
) -> Result<impl IntoResponse, AppError> {
    check_role_owner(&ws_ctx)?;
    service.initiate_upgrade(body.workspace_id).await?;
    Ok((
        StatusCode::OK,
        Json(serde_json::json!({ "status": "pending_payment" })),
    ))
}

/// POST /subscriptions/checkout
///
/// Process a checkout request: validate coupon/discount, compute invoice,
/// update subscription status, and return a checkout URL with invoice breakdown.
pub async fn checkout_handler(
    Extension(service): Extension<Arc<SubscriptionService>>,
    Extension(_claims): Extension<AccessTokenClaims>,
    Extension(ws_ctx): Extension<WorkspaceContext>,
    Json(body): Json<CheckoutRequestBody>,
) -> Result<impl IntoResponse, AppError> {
    check_role_owner(&ws_ctx)?;
    let request = CheckoutRequest {
        workspace_id: body.workspace_id,
        tier: body.tier,
        billing_cycle_months: body.billing_cycle_months,
        coupon_code: body.coupon_code,
        discount_id: body.discount_id,
        country_code: body.country_code,
        success_url: body.success_url,
        cancel_url: body.cancel_url,
    };

    let response = service.checkout(request).await?;
    Ok((StatusCode::OK, Json(response)))
}

/// GET /subscriptions/current
///
/// Returns the current subscription for a workspace, identified by query parameter.
pub async fn current_subscription_handler(
    Extension(service): Extension<Arc<SubscriptionService>>,
    Extension(_claims): Extension<AccessTokenClaims>,
    Extension(ws_ctx): Extension<WorkspaceContext>,
    Query(query): Query<CurrentSubscriptionQuery>,
) -> Result<impl IntoResponse, AppError> {
    check_role_viewer(&ws_ctx)?;
    let response = service.get_current_subscription(query.workspace_id).await?;
    Ok((StatusCode::OK, Json(response)))
}

/// GET /subscriptions/transactions?workspace_id=...
///
/// Returns the transaction history for a workspace (billing history).
pub async fn list_transactions_handler(
    Extension(pool): Extension<PgPool>,
    Extension(_claims): Extension<AccessTokenClaims>,
    Extension(ws_ctx): Extension<WorkspaceContext>,
    Query(query): Query<CurrentSubscriptionQuery>,
) -> Result<impl IntoResponse, AppError> {
    check_role_viewer(&ws_ctx)?;

    let transactions = sqlx::query_as::<_, TransactionRow>(
        r#"
        SELECT id, external_transaction_id, tier, billing_cycle_months,
               base_price, discount_amount, tax_amount, total_amount, currency,
               status, created_at, completed_at
        FROM transactions
        WHERE workspace_id = $1
        ORDER BY created_at DESC
        LIMIT 50
        "#,
    )
    .bind(query.workspace_id)
    .fetch_all(&pool)
    .await
    .map_err(|_| AppError::InternalError)?;

    let response: Vec<serde_json::Value> = transactions
        .into_iter()
        .map(|t| serde_json::json!({
            "id": t.id,
            "external_transaction_id": t.external_transaction_id,
            "tier": t.tier,
            "billing_cycle_months": t.billing_cycle_months,
            "base_price": t.base_price.to_string(),
            "discount_amount": t.discount_amount.to_string(),
            "tax_amount": t.tax_amount.to_string(),
            "total_amount": t.total_amount.to_string(),
            "currency": t.currency,
            "status": t.status,
            "created_at": t.created_at,
            "completed_at": t.completed_at,
        }))
        .collect();

    Ok((StatusCode::OK, Json(response)))
}

/// POST /subscriptions/activate
///
/// Activate a subscription that's in pending_payment status.
/// Called by the frontend after a successful Paddle checkout (since webhooks
/// may not arrive immediately or can't reach localhost in development).
pub async fn activate_subscription_handler(
    Extension(pool): Extension<PgPool>,
    Extension(_claims): Extension<AccessTokenClaims>,
    Extension(ws_ctx): Extension<WorkspaceContext>,
    Json(body): Json<ActivateRequest>,
) -> Result<impl IntoResponse, AppError> {
    check_role_owner(&ws_ctx)?;

    let now = chrono::Utc::now();
    let period_end = now + chrono::Duration::days(365);

    // Only activate if currently pending_payment
    let result = sqlx::query(
        r#"
        UPDATE subscriptions
        SET status = 'active', period_start = $1, period_end = $2, updated_at = now()
        WHERE workspace_id = $3 AND status = 'pending_payment'
        "#,
    )
    .bind(now)
    .bind(period_end)
    .bind(body.workspace_id)
    .execute(&pool)
    .await
    .map_err(|_| AppError::InternalError)?;

    if result.rows_affected() > 0 {
        // Also mark any pending transaction as completed
        let _ = sqlx::query(
            r#"
            UPDATE transactions
            SET status = 'completed', completed_at = now(), updated_at = now()
            WHERE workspace_id = $1 AND status = 'created'
            "#,
        )
        .bind(body.workspace_id)
        .execute(&pool)
        .await;

        // Convert pending referral (award +1 month to both users)
        let owner_id: Option<Uuid> = sqlx::query_scalar(
            "SELECT owner_id FROM workspaces WHERE id = $1",
        )
        .bind(body.workspace_id)
        .fetch_optional(&pool)
        .await
        .map_err(|_| AppError::InternalError)?;

        if let Some(user_id) = owner_id {
            let _ = crate::auth::service::AuthService::convert_referral(&pool, user_id).await;

            // Emit a System_Notification confirming the paid-tier activation
            // (Requirement 22.1). Non-blocking: the activation has already been
            // committed above and must not be failed or rolled back if the
            // notification write fails (Requirement 22.2). `create` applies the
            // owner's preferences and bounded retry internally.
            let tier: Option<String> = sqlx::query_scalar(
                "SELECT tier FROM subscriptions WHERE workspace_id = $1",
            )
            .bind(body.workspace_id)
            .fetch_optional(&pool)
            .await
            .ok()
            .flatten();

            let notif_service = Arc::new(NotificationService::new(pool.clone()));
            persist_user_notification_async(
                notif_service,
                user_id,
                body.workspace_id,
                "subscription_activated".to_string(),
                "Subscription activated".to_string(),
                Some("Your paid subscription is now active.".to_string()),
                Some(serde_json::json!({
                    "workspace_id": body.workspace_id.to_string(),
                    "tier": tier,
                })),
            );
        }
    }

    Ok((StatusCode::OK, Json(serde_json::json!({ "status": "activated" }))))
}

/// POST /subscriptions/cancel
///
/// Cancel an active subscription. Sets status to cancelled immediately.
/// In production, this would also call Paddle's API to cancel the subscription.
pub async fn cancel_subscription_handler(
    Extension(pool): Extension<PgPool>,
    Extension(_claims): Extension<AccessTokenClaims>,
    Extension(ws_ctx): Extension<WorkspaceContext>,
    Json(body): Json<ActivateRequest>,
) -> Result<impl IntoResponse, AppError> {
    check_role_owner(&ws_ctx)?;

    sqlx::query(
        r#"
        UPDATE subscriptions
        SET status = 'cancelled', cancelled_at = now(), updated_at = now()
        WHERE workspace_id = $1 AND status = 'active' AND tier != 'free'
        "#,
    )
    .bind(body.workspace_id)
    .execute(&pool)
    .await
    .map_err(|_| AppError::InternalError)?;

    Ok((StatusCode::OK, Json(serde_json::json!({ "status": "cancelled" }))))
}

/// POST /subscriptions/add-seats
///
/// Purchase additional seats for a Teams workspace.
/// Creates a Paddle transaction for the extra seats, then on activation
/// increases max_seats accordingly.
pub async fn add_seats_handler(
    Extension(pool): Extension<PgPool>,
    Extension(config): Extension<Arc<AppConfig>>,
    Extension(_claims): Extension<AccessTokenClaims>,
    Extension(ws_ctx): Extension<WorkspaceContext>,
    Json(body): Json<AddSeatsRequest>,
) -> Result<impl IntoResponse, AppError> {
    check_role_owner(&ws_ctx)?;

    if body.seats < 1 {
        return Err(AppError::InvalidRequestBody);
    }

    // Verify workspace is on teams tier with active subscription
    let (tier, status): (String, String) = sqlx::query_as(
        "SELECT tier, status FROM subscriptions WHERE workspace_id = $1",
    )
    .bind(body.workspace_id)
    .fetch_one(&pool)
    .await
    .map_err(|_| AppError::InternalError)?;

    if tier != "teams" {
        return Err(AppError::InvalidTier);
    }
    if status != "active" {
        return Err(AppError::InvalidRequestBody);
    }

    // Get the seat price ID
    let price_id = std::env::var("PADDLE_PRICE_ID_TEAMS_SEAT").unwrap_or_default();
    if price_id.is_empty() {
        // No Paddle seat price configured — just add seats directly (dev mode)
        sqlx::query(
            r#"
            UPDATE subscriptions
            SET max_seats = max_seats + $1, extra_seats_purchased = extra_seats_purchased + $1, updated_at = now()
            WHERE workspace_id = $2
            "#,
        )
        .bind(body.seats)
        .bind(body.workspace_id)
        .execute(&pool)
        .await
        .map_err(|_| AppError::InternalError)?;

        return Ok((StatusCode::OK, Json(serde_json::json!({
            "status": "seats_added",
            "seats_added": body.seats,
        }))));
    }

    // Create Paddle transaction for extra seats
    let api_key = config.paddle_api_key.as_deref().unwrap_or_default();
    let base_url = match config.paddle_environment.as_str() {
        "production" | "live" => "https://api.paddle.com",
        _ => "https://sandbox-api.paddle.com",
    };

    let request_body = serde_json::json!({
        "items": [{
            "price_id": price_id,
            "quantity": body.seats
        }],
        "custom_data": {
            "workspace_id": body.workspace_id.to_string(),
            "type": "add_seats",
            "seats": body.seats
        }
    });

    let client = reqwest::Client::new();
    let response = client
        .post(format!("{}/transactions", base_url))
        .header("Authorization", format!("Bearer {}", api_key))
        .header("Content-Type", "application/json")
        .json(&request_body)
        .send()
        .await
        .map_err(|_| AppError::InternalError)?;

    if !response.status().is_success() {
        let error_body = response.text().await.unwrap_or_default();
        tracing::error!("Paddle add-seats failed: {}", error_body);
        return Err(AppError::InternalError);
    }

    let paddle_resp: serde_json::Value = response.json().await.map_err(|_| AppError::InternalError)?;
    let txn_id = paddle_resp["data"]["id"].as_str().unwrap_or("unknown").to_string();
    let checkout_url = paddle_resp["data"]["checkout"]["url"]
        .as_str()
        .unwrap_or("")
        .to_string();

    // Record the transaction
    let owner_id: Uuid = sqlx::query_scalar(
        "SELECT owner_id FROM workspaces WHERE id = $1",
    )
    .bind(body.workspace_id)
    .fetch_one(&pool)
    .await
    .map_err(|_| AppError::InternalError)?;

    let seat_cost = rust_decimal::Decimal::from(body.seats) * rust_decimal::Decimal::from(36); // $36/seat/year
    let _ = sqlx::query(
        r#"
        INSERT INTO transactions (
            workspace_id, user_id, external_transaction_id,
            tier, billing_cycle_months,
            base_price, discount_amount, tax_amount, total_amount, currency,
            status
        )
        VALUES ($1, $2, $3, 'teams_seat', 12, $4, 0, 0, $4, 'USD', 'created')
        ON CONFLICT (external_transaction_id) DO NOTHING
        "#,
    )
    .bind(body.workspace_id)
    .bind(owner_id)
    .bind(&txn_id)
    .bind(seat_cost)
    .execute(&pool)
    .await;

    Ok((StatusCode::OK, Json(serde_json::json!({
        "checkout_url": checkout_url,
        "transaction_id": txn_id,
        "seats": body.seats,
    }))))
}

/// POST /webhooks/billing
///
/// Process a billing webhook event from the payment provider.
/// Verifies the signature from the X-Webhook-Signature header, then parses
/// and processes the webhook payload (Requirements 5.55–5.58).
pub async fn billing_webhook_handler(
    Extension(config): Extension<Arc<AppConfig>>,
    Extension(pool): Extension<PgPool>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<impl IntoResponse, AppError> {
    // Extract signature from header
    let signature = headers
        .get("X-Webhook-Signature")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");

    // Verify the webhook signature (Requirement 5.56)
    webhook::verify_signature(&body, signature, &config.billing_webhook_secret)?;

    // Parse the JSON payload
    let payload: webhook::WebhookPayload =
        serde_json::from_slice(&body).map_err(|_| AppError::MalformedRequestBody)?;

    // Process the webhook event (Requirements 5.55, 5.57, 5.58)
    let result = webhook::process_webhook(&pool, payload).await?;

    Ok((StatusCode::OK, Json(result)))
}
