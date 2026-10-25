//! HTTP handlers for all auth endpoints.
//!
//! Each handler extracts the request body/params, delegates to the appropriate
//! service method, and formats the HTTP response.

use std::sync::Arc;

use axum::{
    extract::{Extension, Path, Query},
    http::StatusCode,
    response::{IntoResponse, Redirect},
    Json,
};
use serde::Deserialize;
use uuid::Uuid;

use crate::auth::oauth::{OAuthProvider, OAuthService};
use crate::auth::service::{
    AuthResponse, AuthService, ChangeEmailRequest, ChangePasswordRequest,
    CreateAdminInviteRequest, ForgotPasswordRequest, LoginRequest, LogoutRequest, ProfileResponse,
    RefreshRequest, RegisterRequest, RegisterResponse, RegisterViaInviteRequest,
    ResetPasswordRequest, SessionInfo, UpdateProfileRequest, VerifyEmailChangeRequest,
    VerifyOtpRequest,
};
use crate::config::AppConfig;
use crate::errors::AppError;
use crate::middleware::auth_extractor::AccessTokenClaims;
use crate::models::common::ClientType;

// ─── OAuth Query Parameter DTOs ─────────────────────────────────────────────────

/// Query parameters for the OAuth authorize endpoint.
#[derive(Debug, Deserialize)]
pub struct OAuthAuthorizeParams {
    /// Client type: "native" or "web".
    pub client: String,
}

/// Request body for the resend-OTP endpoint. Mirrors `ForgotPasswordRequest`
/// (a single `email` field) but is kept distinct for endpoint clarity.
#[derive(Debug, Deserialize)]
pub struct ResendOtpRequest {
    /// Email address to reissue a signup verification code to.
    pub email: String,
}

/// Query parameters for the OAuth callback endpoint.
#[derive(Debug, Deserialize)]
pub struct OAuthCallbackParams {
    /// Authorization code from the provider.
    pub code: Option<String>,
    /// Error returned by the provider (e.g., user denied access).
    pub error: Option<String>,
    /// State parameter (for CSRF protection / client type encoding).
    pub state: Option<String>,
}

// ─── Public Auth Handlers ───────────────────────────────────────────────────────

/// POST /auth/register
///
/// Register a new user account. Email/password signup no longer returns tokens;
/// it creates an unverified user, issues a signup OTP by email, and responds
/// `202 Accepted` with a verification-required body. The client must complete
/// email-OTP verification (`/auth/verify-otp`) to obtain tokens.
pub async fn register_handler(
    Extension(auth_service): Extension<Arc<AuthService>>,
    Json(body): Json<RegisterRequest>,
) -> Result<(StatusCode, Json<RegisterResponse>), AppError> {
    let response = auth_service.register(body).await?;
    Ok((StatusCode::ACCEPTED, Json(response)))
}

/// POST /auth/verify-otp
///
/// Verify a signup email-OTP code. On success, marks the user verified and
/// returns the standard token pair (`AuthResponse`, HTTP 200).
pub async fn verify_otp_handler(
    Extension(auth_service): Extension<Arc<AuthService>>,
    Json(body): Json<VerifyOtpRequest>,
) -> Result<Json<AuthResponse>, AppError> {
    Ok(Json(auth_service.verify_otp(body).await?))
}

/// POST /auth/resend-otp
///
/// Reissue a signup OTP for the given email. Enumeration-safe: always returns
/// `204 No Content`, whether the email is unknown, already verified, or
/// rate-limited by the resend cooldown.
pub async fn resend_otp_handler(
    Extension(auth_service): Extension<Arc<AuthService>>,
    Json(body): Json<ResendOtpRequest>,
) -> Result<StatusCode, AppError> {
    auth_service.resend_otp(&body.email).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// POST /auth/login
///
/// Authenticate a user and return a token pair.
pub async fn login_handler(
    Extension(auth_service): Extension<Arc<AuthService>>,
    Json(body): Json<LoginRequest>,
) -> Result<Json<AuthResponse>, AppError> {
    let response = auth_service.login(body).await?;
    Ok(Json(response))
}

/// POST /auth/refresh
///
/// Refresh an access/refresh token pair using token rotation.
pub async fn refresh_handler(
    Extension(auth_service): Extension<Arc<AuthService>>,
    Json(body): Json<RefreshRequest>,
) -> Result<Json<AuthResponse>, AppError> {
    let response = auth_service.refresh_token(body).await?;
    Ok(Json(response))
}

/// POST /auth/logout
///
/// Invalidate a refresh token (logout).
pub async fn logout_handler(
    Extension(auth_service): Extension<Arc<AuthService>>,
    Extension(claims): Extension<AccessTokenClaims>,
    Json(body): Json<LogoutRequest>,
) -> Result<StatusCode, AppError> {
    let _ = claims; // Ensures user is authenticated
    auth_service.logout(body).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// POST /auth/forgot-password
///
/// Initiate a password reset flow. Always returns 200 to prevent email enumeration.
pub async fn forgot_password_handler(
    Extension(auth_service): Extension<Arc<AuthService>>,
    Json(body): Json<ForgotPasswordRequest>,
) -> Result<StatusCode, AppError> {
    auth_service.forgot_password(body).await?;
    Ok(StatusCode::OK)
}

/// POST /auth/reset-password
///
/// Reset a user's password using a valid reset token.
pub async fn reset_password_handler(
    Extension(auth_service): Extension<Arc<AuthService>>,
    Json(body): Json<ResetPasswordRequest>,
) -> Result<StatusCode, AppError> {
    auth_service.reset_password(body).await?;
    Ok(StatusCode::OK)
}

// ─── Authenticated Auth Handlers ────────────────────────────────────────────────

/// PATCH /auth/profile
///
/// Update the authenticated user's profile fields.
pub async fn update_profile_handler(
    Extension(auth_service): Extension<Arc<AuthService>>,
    Extension(claims): Extension<AccessTokenClaims>,
    Json(body): Json<UpdateProfileRequest>,
) -> Result<Json<ProfileResponse>, AppError> {
    let response = auth_service.update_profile(claims.sub, body).await?;
    Ok(Json(response))
}

/// GET /auth/profile
///
/// Return the authenticated user's full profile.
pub async fn get_profile_handler(
    Extension(auth_service): Extension<Arc<AuthService>>,
    Extension(claims): Extension<AccessTokenClaims>,
) -> Result<Json<ProfileResponse>, AppError> {
    let response = auth_service.get_profile(claims.sub).await?;
    Ok(Json(response))
}

/// POST /auth/change-email
///
/// Initiate an email change by sending a verification link to the new address.
pub async fn change_email_handler(
    Extension(auth_service): Extension<Arc<AuthService>>,
    Extension(claims): Extension<AccessTokenClaims>,
    Json(body): Json<ChangeEmailRequest>,
) -> Result<StatusCode, AppError> {
    auth_service.initiate_email_change(claims.sub, body).await?;
    Ok(StatusCode::OK)
}

/// GET /auth/verify-email-change?token=...
///
/// Verify an email change token and update the user's email.
pub async fn verify_email_change_handler(
    Extension(auth_service): Extension<Arc<AuthService>>,
    Query(params): Query<VerifyEmailChangeRequest>,
) -> Result<StatusCode, AppError> {
    auth_service.verify_email_change(params).await?;
    Ok(StatusCode::OK)
}

/// POST /auth/change-password
///
/// Change the authenticated user's password.
pub async fn change_password_handler(
    Extension(auth_service): Extension<Arc<AuthService>>,
    Extension(claims): Extension<AccessTokenClaims>,
    Json(body): Json<ChangePasswordRequest>,
) -> Result<StatusCode, AppError> {
    auth_service.change_password(claims.sub, body).await?;
    Ok(StatusCode::OK)
}

/// DELETE /auth/account
///
/// Soft-delete the authenticated user's account.
pub async fn delete_account_handler(
    Extension(auth_service): Extension<Arc<AuthService>>,
    Extension(claims): Extension<AccessTokenClaims>,
) -> Result<StatusCode, AppError> {
    auth_service.delete_account(claims.sub).await?;
    Ok(StatusCode::NO_CONTENT)
}

// ─── Session Management Handlers ────────────────────────────────────────────────

/// GET /auth/sessions
///
/// List all active sessions for the authenticated user.
pub async fn list_sessions_handler(
    Extension(auth_service): Extension<Arc<AuthService>>,
    Extension(claims): Extension<AccessTokenClaims>,
) -> Result<Json<Vec<SessionInfo>>, AppError> {
    let sessions = auth_service.list_sessions(claims.sub).await?;
    Ok(Json(sessions))
}

/// DELETE /auth/sessions/:session_id
///
/// Revoke a specific session for the authenticated user.
pub async fn revoke_session_handler(
    Extension(auth_service): Extension<Arc<AuthService>>,
    Extension(claims): Extension<AccessTokenClaims>,
    Path(session_id): Path<Uuid>,
) -> Result<StatusCode, AppError> {
    auth_service.revoke_session(claims.sub, session_id).await?;
    Ok(StatusCode::NO_CONTENT)
}

// ─── OAuth Handlers ─────────────────────────────────────────────────────────────

/// GET /auth/oauth/:provider/authorize?client=native|web
///
/// Redirect the user to the OAuth provider's authorization page.
pub async fn oauth_authorize_handler(
    Extension(oauth_service): Extension<Arc<OAuthService>>,
    Path(provider): Path<String>,
    Query(params): Query<OAuthAuthorizeParams>,
) -> Result<Redirect, AppError> {
    let provider = OAuthProvider::from_str(&provider).ok_or(AppError::InvalidRequestBody)?;

    let client_type = parse_client_type(&params.client)?;

    let url = oauth_service.oauth_authorize(provider, &client_type);
    Ok(Redirect::temporary(&url))
}

/// GET /auth/oauth/:provider/callback?code=...&error=...&state=...
///
/// Handle the OAuth callback from the provider. Exchanges the authorization code
/// for tokens, upserts the user, and redirects to the frontend callback page
/// with tokens in URL params (for web clients) or returns JSON (for native clients).
pub async fn oauth_callback_handler(
    Extension(oauth_service): Extension<Arc<OAuthService>>,
    Extension(config): Extension<Arc<AppConfig>>,
    Path(provider): Path<String>,
    Query(params): Query<OAuthCallbackParams>,
) -> Result<impl IntoResponse, AppError> {
    tracing::info!(
        provider = %provider,
        has_code = params.code.is_some(),
        has_error = params.error.is_some(),
        "OAuth callback received"
    );

    let provider = OAuthProvider::from_str(&provider).ok_or(AppError::InvalidRequestBody)?;

    // Determine client type from state parameter or default to web
    let client_type = match params.state.as_deref() {
        Some("native") => ClientType::Native,
        _ => ClientType::Web,
    };

    let code = params.code.as_deref().unwrap_or("");
    let frontend_url = &config.frontend_url;

    // Build the client-appropriate callback URL carrying an `error` param, so a
    // cancelled/denied authorization lands the user back on a page that knows how
    // to recover (the frontend `/callback` route redirects to `/sign-in`), rather
    // than on a blank page. Web → the configured frontend origin; Native → the
    // app's deep link so the Tauri app can catch it.
    let error_redirect = |error: &str| -> String {
        match client_type {
            ClientType::Native => {
                format!("ursnip://oauth/callback?error={}", urlencoding::encode(error))
            }
            ClientType::Web => {
                format!("{}/callback?error={}", frontend_url, urlencoding::encode(error))
            }
        }
    };

    // Handle error from provider (e.g. the user cancelled) — redirect back with
    // the error param for BOTH client types.
    if let Some(ref error) = params.error {
        return Ok(Redirect::temporary(&error_redirect(error)).into_response());
    }

    let response = oauth_service
        .oauth_callback(
            provider,
            code,
            &client_type,
            params.error.as_deref(),
            None, // No authenticated user for public callback
        )
        .await;

    match (response, &client_type) {
        (Ok(auth_response), _) => {
            // Success: redirect back to the client's callback with tokens in the
            // query. Native OAuth runs in the system browser (so the provider can
            // reach the custom scheme), so BOTH clients redirect — Web to the
            // frontend `/callback`, Native to the `ursnip://oauth/callback` deep
            // link the app catches. Same param shape either way, so the frontend
            // `/callback` handler is reused for both.
            tracing::info!(
                email = %auth_response.user.email,
                ?client_type,
                "OAuth success, redirecting to callback"
            );
            let user_json = serde_json::to_string(&auth_response.user).unwrap_or_default();
            let base = match client_type {
                ClientType::Native => "ursnip://oauth/callback".to_string(),
                ClientType::Web => format!("{}/callback", frontend_url),
            };
            let redirect_url = format!(
                "{}?access_token={}&refresh_token={}&user={}",
                base,
                urlencoding::encode(&auth_response.access_token),
                urlencoding::encode(&auth_response.refresh_token),
                urlencoding::encode(&user_json),
            );
            Ok(Redirect::temporary(&redirect_url).into_response())
        }
        (Err(ref e), _) => {
            // Redirect back with an error code for BOTH client types so the user
            // never lands on a blank page (Web → frontend /callback, Native →
            // the app deep link). Previously only Web redirected and Native
            // returned a raw 401/JSON, which showed as a blank page in the app.
            tracing::error!(error = ?e, ?client_type, "OAuth callback failed, redirecting with error");
            let error_code = match &e {
                AppError::OAuthAuthorizationDenied => "OAUTH_AUTHORIZATION_DENIED",
                AppError::AccountLinkingConflict => "ACCOUNT_LINKING_CONFLICT",
                _ => "OAUTH_ERROR",
            };
            Ok(Redirect::temporary(&error_redirect(error_code)).into_response())
        }
    }
}

// ─── Admin Invite Handlers ──────────────────────────────────────────────────────

/// POST /admin/invites
///
/// Create an admin invite (admin-only).
pub async fn create_admin_invite_handler(
    Extension(auth_service): Extension<Arc<AuthService>>,
    Extension(claims): Extension<AccessTokenClaims>,
    Json(body): Json<CreateAdminInviteRequest>,
) -> Result<impl IntoResponse, AppError> {
    let response = auth_service
        .create_admin_invite(body.email, claims.sub)
        .await?;
    Ok((StatusCode::CREATED, Json(response)))
}

/// POST /auth/register-invite
///
/// Register a new admin user via a valid invite token.
pub async fn register_via_invite_handler(
    Extension(auth_service): Extension<Arc<AuthService>>,
    Json(body): Json<RegisterViaInviteRequest>,
) -> Result<impl IntoResponse, AppError> {
    let response = auth_service.register_via_invite(body).await?;
    Ok((StatusCode::CREATED, Json(response)))
}

// ─── Helpers ────────────────────────────────────────────────────────────────────

/// Parse a client type string ("native" or "web") into a `ClientType` enum.
fn parse_client_type(s: &str) -> Result<ClientType, AppError> {
    match s.to_lowercase().as_str() {
        "native" => Ok(ClientType::Native),
        "web" => Ok(ClientType::Web),
        _ => Err(AppError::InvalidRequestBody),
    }
}


// ─── Referral Stats Handler ─────────────────────────────────────────────────────

/// GET /auth/referrals
///
/// Get the authenticated user's referral information:
/// - Their referral code
/// - Number of people they've referred
/// - Credits earned (months)
/// - List of referral dates (no PII of referred users)
pub async fn get_my_referrals_handler(
    Extension(pool): Extension<sqlx::PgPool>,
    Extension(claims): Extension<AccessTokenClaims>,
) -> Result<Json<serde_json::Value>, AppError> {
    // Get referral code
    let referral_code: String = sqlx::query_scalar(
        "SELECT referral_code FROM users WHERE id = $1",
    )
    .bind(claims.sub)
    .fetch_one(&pool)
    .await
    .map_err(|_| AppError::InternalError)?;

    // Count referrals
    let total_referrals: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM referrals WHERE referrer_id = $1",
    )
    .bind(claims.sub)
    .fetch_one(&pool)
    .await
    .map_err(|_| AppError::InternalError)?;

    let converted_referrals: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM referrals WHERE referrer_id = $1 AND status = 'converted'",
    )
    .bind(claims.sub)
    .fetch_one(&pool)
    .await
    .map_err(|_| AppError::InternalError)?;

    // Total credits earned
    let total_credits: i64 = sqlx::query_scalar(
        "SELECT COALESCE(SUM(months), 0) FROM referral_credits WHERE user_id = $1",
    )
    .bind(claims.sub)
    .fetch_one(&pool)
    .await
    .map_err(|_| AppError::InternalError)?;

    Ok(Json(serde_json::json!({
        "referral_code": referral_code,
        "referral_url": format!("https://app.ursnip.com/register?ref={}", referral_code),
        "total_referrals": total_referrals,
        "converted_referrals": converted_referrals,
        "total_credits_months": total_credits,
        "reward_per_referral": "1 month Pro for both you and your friend",
    })))
}
