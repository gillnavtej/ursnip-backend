//! HTTP handlers for the billing / seat-management REST API (Feature 6).
//!
//! NOTE: Route registration is intentionally deferred to task 8.4. These
//! handlers are scaffolding that mirror the sharing/notifications conventions
//! (`Extension`-injected `Arc<Service>` + `AccessTokenClaims`) so the endpoints
//! (`POST /subscriptions/seats`, `DELETE /subscriptions/seats/:seat_id`,
//! `GET /subscriptions/seats?workspace_id=`) can be wired into `router.rs`
//! without reshaping the service API.

use std::sync::Arc;

use axum::{
    extract::{Extension, Path, Query},
    http::StatusCode,
    response::IntoResponse,
    Json,
};
use serde::Deserialize;
use uuid::Uuid;

use crate::billing::service::{AddSeatRequest, BillingService};
use crate::errors::AppError;
use crate::middleware::auth_extractor::AccessTokenClaims;
use crate::middleware::workspace_context::{check_role_owner, WorkspaceContext};

/// POST /subscriptions/seats
///
/// Add a seat to a workspace. Owner-only (Req 17.1): the workspace-scoped
/// `workspace_context_guard` injects `WorkspaceContext`, and `check_role_owner`
/// enforces owner access as defense-in-depth alongside the service's own
/// authoritative owner check.
pub async fn add_seat_handler(
    Extension(service): Extension<Arc<BillingService>>,
    Extension(claims): Extension<AccessTokenClaims>,
    Extension(ws_ctx): Extension<WorkspaceContext>,
    Json(req): Json<AddSeatRequest>,
) -> Result<impl IntoResponse, AppError> {
    check_role_owner(&ws_ctx)?;
    let seat = service.add_seat(claims.sub, req).await?;
    Ok((StatusCode::CREATED, Json(seat)))
}

/// DELETE /subscriptions/seats/:seat_id
///
/// Remove a seat. Owner-only (Req 18.1): `check_role_owner` enforces owner
/// access on the workspace-scoped route as defense-in-depth alongside the
/// service's authoritative team-ownership check.
pub async fn remove_seat_handler(
    Extension(service): Extension<Arc<BillingService>>,
    Extension(claims): Extension<AccessTokenClaims>,
    Extension(ws_ctx): Extension<WorkspaceContext>,
    Path(seat_id): Path<Uuid>,
) -> Result<impl IntoResponse, AppError> {
    check_role_owner(&ws_ctx)?;
    service.remove_seat(claims.sub, seat_id).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Query parameters for the list-seats endpoint.
#[derive(Debug, Deserialize)]
pub struct ListSeatsQuery {
    pub workspace_id: Uuid,
}

/// GET /subscriptions/seats?workspace_id=
///
/// List the seats of a workspace. Owner-only (Req 17.1/18.1): the
/// `workspace_id` query param drives `workspace_context_guard`, and
/// `check_role_owner` restricts listing to the workspace owner.
pub async fn list_seats_handler(
    Extension(service): Extension<Arc<BillingService>>,
    Extension(ws_ctx): Extension<WorkspaceContext>,
    Query(query): Query<ListSeatsQuery>,
) -> Result<impl IntoResponse, AppError> {
    check_role_owner(&ws_ctx)?;
    let seats = service.list_seats(query.workspace_id).await?;
    Ok((StatusCode::OK, Json(seats)))
}
