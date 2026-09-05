//! HTTP handlers for AI endpoints.
//!
//! Provides:
//! - POST /ai/expand — expand a trigger via the Gemini AI model
//! - POST /ai/generate — generate a new snippet from a description
//! - POST /ai/improve — improve/rewrite existing snippet content

use std::sync::Arc;

use axum::{extract::Extension, Json};

use crate::ai::service::{
    AiExpandRequest, AiExpandResponse, AiGenerateRequest, AiGenerateResponse,
    AiImproveRequest, AiImproveResponse, AiService,
};
use crate::errors::AppError;
use crate::middleware::auth_extractor::AccessTokenClaims;

// ─── Handlers ───────────────────────────────────────────────────────────────────

/// POST /ai/expand
///
/// Accepts a JSON body with `trigger` and `system_prompt`, expands the trigger
/// using the Gemini AI model, and returns `{ "expanded_text": "..." }`.
///
/// Requirements:
/// - Authenticated user (AccessTokenClaims injected by auth middleware)
pub async fn expand_handler(
    Extension(service): Extension<Arc<AiService>>,
    Extension(claims): Extension<AccessTokenClaims>,
    Json(body): Json<AiExpandRequest>,
) -> Result<Json<AiExpandResponse>, AppError> {
    let user_id = claims.sub;
    let response = service.expand(user_id, body).await?;
    Ok(Json(response))
}

/// POST /ai/generate
///
/// Generate a new snippet from a natural language description.
/// Returns the generated snippet content and a suggested trigger.
///
/// Request body:
/// - `description` (required): what the snippet should do
/// - `tone` (optional): "professional", "casual", "empathetic", etc.
/// - `category` (optional): "customer-support", "sales", "developer", etc.
/// - `include_variables` (optional): whether to include dynamic variables
///
/// Response:
/// - `content`: the generated snippet text
/// - `suggested_trigger`: a suggested /trigger shortcut
pub async fn generate_handler(
    Extension(service): Extension<Arc<AiService>>,
    Extension(claims): Extension<AccessTokenClaims>,
    Json(body): Json<AiGenerateRequest>,
) -> Result<Json<AiGenerateResponse>, AppError> {
    let user_id = claims.sub;
    let response = service.generate(user_id, body).await?;
    Ok(Json(response))
}

/// POST /ai/improve
///
/// Improve or rewrite an existing snippet based on a natural language instruction.
///
/// Request body:
/// - `content` (required): the existing snippet content
/// - `instruction` (required): how to improve it (e.g. "make it shorter", "add empathy")
///
/// Response:
/// - `content`: the improved snippet text
pub async fn improve_handler(
    Extension(service): Extension<Arc<AiService>>,
    Extension(claims): Extension<AccessTokenClaims>,
    Json(body): Json<AiImproveRequest>,
) -> Result<Json<AiImproveResponse>, AppError> {
    let user_id = claims.sub;
    let response = service.improve(user_id, body).await?;
    Ok(Json(response))
}
