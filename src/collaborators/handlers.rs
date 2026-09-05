//! HTTP handlers for account-level collaborator management.
//!
//! All handlers resolve the acting user from `AccessTokenClaims`. Owner
//! operations (invite, list, remove, grants) are Teams-gated inside the
//! service. `accept_invite_handler` is usable by any authenticated user; it
//! matches the invite by the caller's email.

use std::sync::Arc;

use axum::{
    extract::{Extension, Path},
    http::StatusCode,
    response::IntoResponse,
    Json,
};
use sqlx::PgPool;
use uuid::Uuid;

use crate::errors::AppError;
use crate::middleware::auth_extractor::AccessTokenClaims;

use super::service::CollaboratorService;
use super::{AcceptInviteRequest, InviteRequest, UpsertGrantRequest};

/// Fetch the authenticated user's email (not present in JWT claims).
async fn user_email(pool: &PgPool, user_id: Uuid) -> Result<String, AppError> {
    sqlx::query_scalar("SELECT email FROM users WHERE id = $1")
        .bind(user_id)
        .fetch_optional(pool)
        .await
        .map_err(|_| AppError::InternalError)?
        .ok_or(AppError::UserNotFound)
}

/// POST /collaborators/invite
pub async fn invite_handler(
    Extension(service): Extension<Arc<CollaboratorService>>,
    Extension(claims): Extension<AccessTokenClaims>,
    Json(body): Json<InviteRequest>,
) -> Result<impl IntoResponse, AppError> {
    let response = service.invite(claims.sub, &body.email).await?;
    Ok((StatusCode::CREATED, Json(response)))
}

/// GET /collaborators
pub async fn list_collaborators_handler(
    Extension(service): Extension<Arc<CollaboratorService>>,
    Extension(claims): Extension<AccessTokenClaims>,
) -> Result<impl IntoResponse, AppError> {
    let collaborators = service.list_collaborators(claims.sub).await?;
    Ok((StatusCode::OK, Json(collaborators)))
}

/// DELETE /collaborators/:user_id
pub async fn remove_collaborator_handler(
    Extension(service): Extension<Arc<CollaboratorService>>,
    Extension(claims): Extension<AccessTokenClaims>,
    Path(collaborator_id): Path<Uuid>,
) -> Result<impl IntoResponse, AppError> {
    service
        .remove_collaborator(claims.sub, collaborator_id)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

/// POST /collaborators/accept
pub async fn accept_invite_handler(
    Extension(service): Extension<Arc<CollaboratorService>>,
    Extension(pool): Extension<PgPool>,
    Extension(claims): Extension<AccessTokenClaims>,
    Json(body): Json<AcceptInviteRequest>,
) -> Result<impl IntoResponse, AppError> {
    let email = user_email(&pool, claims.sub).await?;
    service
        .accept_invite(claims.sub, &email, &body.token)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

/// GET /collaborators/:user_id/grants
pub async fn list_grants_handler(
    Extension(service): Extension<Arc<CollaboratorService>>,
    Extension(claims): Extension<AccessTokenClaims>,
    Path(collaborator_id): Path<Uuid>,
) -> Result<impl IntoResponse, AppError> {
    let grants = service.list_grants(claims.sub, collaborator_id).await?;
    Ok((StatusCode::OK, Json(grants)))
}

/// POST /collaborators/:user_id/grants
pub async fn upsert_grant_handler(
    Extension(service): Extension<Arc<CollaboratorService>>,
    Extension(claims): Extension<AccessTokenClaims>,
    Path(collaborator_id): Path<Uuid>,
    Json(body): Json<UpsertGrantRequest>,
) -> Result<impl IntoResponse, AppError> {
    let grant = service
        .upsert_grant(
            claims.sub,
            collaborator_id,
            &body.resource_type,
            body.resource_id,
            &body.level,
        )
        .await?;
    Ok((StatusCode::OK, Json(grant)))
}

/// DELETE /collaborators/grants/:grant_id
pub async fn delete_grant_handler(
    Extension(service): Extension<Arc<CollaboratorService>>,
    Extension(claims): Extension<AccessTokenClaims>,
    Path(grant_id): Path<Uuid>,
) -> Result<impl IntoResponse, AppError> {
    service.delete_grant(claims.sub, grant_id).await?;
    Ok(StatusCode::NO_CONTENT)
}
