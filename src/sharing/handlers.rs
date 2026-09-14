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
use crate::sharing::service::{ShareFolderRequest, ShareRequest, SharingService};

/// POST /sharing/snippets
///
/// Share a snippet to a recipient, creating an independent recipient-owned copy.
pub async fn share_snippet_handler(
    Extension(service): Extension<Arc<SharingService>>,
    Extension(claims): Extension<AccessTokenClaims>,
    Json(req): Json<ShareRequest>,
) -> Result<impl IntoResponse, AppError> {
    let shared = service.share_snippet(claims.sub, req).await?;
    Ok((StatusCode::OK, Json(shared)))
}

/// POST /sharing/folders
///
/// Share every non-deleted snippet in a folder to a recipient, each as an
/// independent recipient-owned copy. Returns a per-snippet breakdown
/// (`shared` / `skipped` / `failed`) so partial success (e.g. a trigger already
/// present in the target workspace) is surfaced rather than failing the batch.
pub async fn share_folder_handler(
    Extension(service): Extension<Arc<SharingService>>,
    Extension(claims): Extension<AccessTokenClaims>,
    Json(req): Json<ShareFolderRequest>,
) -> Result<impl IntoResponse, AppError> {
    let result = service.share_folder(claims.sub, req).await?;
    Ok((StatusCode::OK, Json(result)))
}
