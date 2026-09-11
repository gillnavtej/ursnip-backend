//! HTTP handlers for the snippet-sharing REST API (Feature 1).
//!
//! NOTE: Route registration is intentionally deferred to task 4.3. This handler
//! is scaffolding that mirrors the notifications handler conventions
//! (`Extension`-injected `Arc<Service>` + `AccessTokenClaims`) so the endpoint
//! can be wired into a `sharing_routes` group without reshaping the service API.

use std::sync::Arc;

use axum::{extract::Extension, http::StatusCode, response::IntoResponse, Json};

use crate::errors::AppError;
use crate::middleware::auth_extractor::AccessTokenClaims;
use crate::sharing::service::{ShareRequest, SharingService};

/// POST /sharing/snippets
///
/// Share a snippet to a recipient, creating an independent recipient-owned copy.
///
/// Full behavior (success `{ snippet_id, recipient_id, trigger }`, 404 recipient
/// not found, 409 trigger conflict, 403 unauthorized-read) lands with the
/// service logic in task 4.2 and route registration in task 4.3.
pub async fn share_snippet_handler(
    Extension(service): Extension<Arc<SharingService>>,
    Extension(claims): Extension<AccessTokenClaims>,
    Json(req): Json<ShareRequest>,
) -> Result<impl IntoResponse, AppError> {
    let shared = service.share_snippet(claims.sub, req).await?;
    Ok((StatusCode::OK, Json(shared)))
}
