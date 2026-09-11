//! Billing & seat-management service (Feature 6).
//!
//! Models individual Teams **seats** as rows in the `seats` table (migration
//! `0017_seats.sql`), each with its own duration and expiry, replacing the old
//! integer-count-only model. The core invariant, enforced in this service
//! rather than the schema, is that a seat's expiry may never exceed the paying
//! subscription's `period_end` — the boundary being *inclusive*
//! (`seat_expiry <= period_end`).
//!
//! NOTE: This module is scaffolding for task 8.1. The type definitions, the
//! `SeatDuration` ⇄ `duration_option` conversions, and the `BillingService`
//! struct/constructor are complete. The full logic of [`BillingService::add_seat`]
//! (expiry computation + inclusive-boundary enforcement + authorization) lands
//! in task 8.2, and [`BillingService::remove_seat`] / [`BillingService::list_seats`]
//! in task 8.3.

use chrono::{DateTime, Months, Utc};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use uuid::Uuid;

use crate::errors::AppError;

/// How long a seat is granted for.
///
/// The fixed-month variants are limited to 1, 2, 3 or 6 months; `UntilPlanLasts`
/// pins the seat's expiry to the subscription's `period_end`. These map 1:1 with
/// the `duration_option` TEXT column values (`'1m','2m','3m','6m','until_plan_lasts'`)
/// via [`SeatDuration::as_duration_option`] / [`SeatDuration::from_duration_option`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind", content = "months")]
pub enum SeatDuration {
    /// A fixed number of whole months (valid values: 1, 2, 3, 6).
    Months(u8),
    /// The seat lasts until the paying plan's current period ends.
    UntilPlanLasts,
}

impl SeatDuration {
    /// Convert this duration into its `seats.duration_option` string.
    ///
    /// Returns [`AppError::InvalidSeatDuration`] for an unsupported month count
    /// (anything other than 1, 2, 3 or 6), so callers never persist a value the
    /// `duration_option` CHECK constraint would reject.
    pub fn as_duration_option(&self) -> Result<&'static str, AppError> {
        match self {
            SeatDuration::Months(1) => Ok("1m"),
            SeatDuration::Months(2) => Ok("2m"),
            SeatDuration::Months(3) => Ok("3m"),
            SeatDuration::Months(6) => Ok("6m"),
            SeatDuration::Months(_) => Err(AppError::InvalidSeatDuration),
            SeatDuration::UntilPlanLasts => Ok("until_plan_lasts"),
        }
    }

    /// Parse a `seats.duration_option` string back into a [`SeatDuration`].
    ///
    /// Returns [`AppError::InvalidSeatDuration`] for an unrecognized value.
    pub fn from_duration_option(value: &str) -> Result<Self, AppError> {
        match value {
            "1m" => Ok(SeatDuration::Months(1)),
            "2m" => Ok(SeatDuration::Months(2)),
            "3m" => Ok(SeatDuration::Months(3)),
            "6m" => Ok(SeatDuration::Months(6)),
            "until_plan_lasts" => Ok(SeatDuration::UntilPlanLasts),
            _ => Err(AppError::InvalidSeatDuration),
        }
    }
}

/// Request to add a seat to a workspace.
#[derive(Debug, Clone, Deserialize)]
pub struct AddSeatRequest {
    /// The workspace the seat is added to.
    pub workspace_id: Uuid,
    /// The seat's duration (fixed months or until-plan-lasts).
    pub duration: SeatDuration,
    /// Optional user the seat is assigned to at creation time; `None` = unassigned.
    pub assigned_user_id: Option<Uuid>,
}

/// A single seat row from the `seats` table.
///
/// Maps 1:1 with `0017_seats.sql`. `duration_option` is kept as the raw column
/// string; convert to/from [`SeatDuration`] with the helpers on that type.
#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct Seat {
    pub id: Uuid,
    pub workspace_id: Uuid,
    pub assigned_user_id: Option<Uuid>,
    pub duration_option: String,
    pub seat_start: DateTime<Utc>,
    pub seat_expiry: DateTime<Utc>,
    pub created_at: DateTime<Utc>,
}

/// Service that manages Teams seats (add / remove / list) against the `seats`
/// table, enforcing the inclusive `seat_expiry <= period_end` boundary in the
/// application layer.
#[derive(Clone)]
pub struct BillingService {
    pool: PgPool,
}

impl BillingService {
    /// Create a new `BillingService`.
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Borrow the underlying connection pool.
    ///
    /// Retained for the add/remove/list logic implemented in tasks 8.2 and 8.3.
    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    /// Add a seat to a workspace.
    ///
    /// Full behavior — expiry computation (`Months(n)` → `now + n months`,
    /// `UntilPlanLasts` → `period_end`), inclusive-boundary rejection
    /// (`SeatExceedsPaidPeriod` only when `expiry > period_end`), unsupported /
    /// missing duration rejection (`InvalidSeatDuration`), owner authorization,
    /// and the transactional insert — is implemented in task 8.2.
    pub async fn add_seat(&self, admin_id: Uuid, req: AddSeatRequest) -> Result<Seat, AppError> {
        // Step 1 (Req 17.4): validate the duration up front. Unsupported / missing
        // month counts map to InvalidSeatDuration before any DB work happens, so a
        // rejected request never touches a transaction.
        let duration_option = req.duration.as_duration_option()?;

        // Step 2 (Req 17.3, 17.1): authoritative server-side owner authorization.
        // The seat may only be added by the workspace OWNER; anyone else is Forbidden.
        // Missing workspace also fails authorization (no owner match).
        let owner_id: Option<Uuid> =
            sqlx::query_scalar("SELECT owner_id FROM workspaces WHERE id = $1")
                .bind(req.workspace_id)
                .fetch_optional(&self.pool)
                .await
                .map_err(|_| AppError::InternalError)?;

        match owner_id {
            Some(owner) if owner == admin_id => {}
            _ => return Err(AppError::Forbidden),
        }

        // Step 3: load the workspace-scoped subscription to get the paid period_end.
        // Seats are a Teams concept; the boundary is the subscription's current period.
        let period_end: Option<DateTime<Utc>> = sqlx::query_scalar(
            "SELECT period_end FROM subscriptions WHERE workspace_id = $1",
        )
        .bind(req.workspace_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(|_| AppError::InternalError)?
        .flatten();

        let period_end = period_end.ok_or(AppError::InvalidRequestBody)?;

        // Step 4: compute expiry.
        //   Months(n)       → now + n months
        //   UntilPlanLasts  → period_end
        let now = Utc::now();
        let expiry = match req.duration {
            SeatDuration::Months(n) => match now.checked_add_months(Months::new(n as u32)) {
                Some(e) => e,
                // Overflow when adding months: treat as over-period rather than panicking.
                None => return Err(AppError::SeatExceedsPaidPeriod),
            },
            SeatDuration::UntilPlanLasts => period_end,
        };

        // Step 5 (Req 16.1, 16.2, 16.3): inclusive-boundary enforcement. Reject only
        // when the expiry is STRICTLY greater than period_end (`expiry == period_end`
        // is allowed).
        if expiry > period_end {
            return Err(AppError::SeatExceedsPaidPeriod);
        }

        // Step 6 (Req 16.4, 17.2): transactional insert. A rejected request above has
        // already returned, so reaching here means we persist exactly one seat row and
        // commit. Any failure rolls back, leaving no seat.
        let mut tx = self.pool.begin().await.map_err(|_| AppError::InternalError)?;

        let seat = sqlx::query_as::<_, Seat>(
            r#"
            INSERT INTO seats (workspace_id, assigned_user_id, duration_option, seat_start, seat_expiry)
            VALUES ($1, $2, $3, $4, $5)
            RETURNING id, workspace_id, assigned_user_id, duration_option, seat_start, seat_expiry, created_at
            "#,
        )
        .bind(req.workspace_id)
        .bind(req.assigned_user_id)
        .bind(duration_option)
        .bind(now)
        .bind(expiry)
        .fetch_one(&mut *tx)
        .await
        .map_err(|_| AppError::InternalError)?;

        tx.commit().await.map_err(|_| AppError::InternalError)?;

        Ok(seat)
    }

    /// Remove a seat from a workspace.
    ///
    /// Full behavior — authorization (seat belongs to the admin's team),
    /// `SeatInUse` while the seat is assigned, `SeatNotFound` otherwise, and the
    /// usable-allocation decrement — is implemented in task 8.3.
    pub async fn remove_seat(&self, admin_id: Uuid, seat_id: Uuid) -> Result<(), AppError> {
        // Do the whole check-then-delete inside one transaction so that an
        // assigned / nonexistent seat leaves no change (Req 18.1).
        let mut tx = self.pool.begin().await.map_err(|_| AppError::InternalError)?;

        // Step 1 (Req 18 / 404): load the seat. Missing → SeatNotFound.
        let seat = sqlx::query_as::<_, Seat>(
            r#"
            SELECT id, workspace_id, assigned_user_id, duration_option, seat_start, seat_expiry, created_at
            FROM seats
            WHERE id = $1
            "#,
        )
        .bind(seat_id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(|_| AppError::InternalError)?
        .ok_or(AppError::SeatNotFound)?;

        // Step 2 (Req 18.2): authorization — the seat must belong to the admin's
        // team, i.e. its workspace's owner must be the admin. Cross-team removal
        // (and a workspace with no owner match) is Forbidden. Mirrors add_seat.
        let owner_id: Option<Uuid> =
            sqlx::query_scalar("SELECT owner_id FROM workspaces WHERE id = $1")
                .bind(seat.workspace_id)
                .fetch_optional(&mut *tx)
                .await
                .map_err(|_| AppError::InternalError)?;

        match owner_id {
            Some(owner) if owner == admin_id => {}
            _ => return Err(AppError::Forbidden),
        }

        // Step 3 (Req 18.3 / 409): a seat assigned to a team member cannot be
        // removed until the assignment is cleared.
        if seat.assigned_user_id.is_some() {
            return Err(AppError::SeatInUse);
        }

        // Step 4 (Req 18.1): delete the seat row, decreasing the usable
        // allocation by one, and commit.
        sqlx::query("DELETE FROM seats WHERE id = $1")
            .bind(seat_id)
            .execute(&mut *tx)
            .await
            .map_err(|_| AppError::InternalError)?;

        tx.commit().await.map_err(|_| AppError::InternalError)?;

        Ok(())
    }

    /// List all seats belonging to a workspace.
    ///
    /// Returns every seat row for `workspace_id`, ordered by creation time.
    pub async fn list_seats(&self, workspace_id: Uuid) -> Result<Vec<Seat>, AppError> {
        let seats = sqlx::query_as::<_, Seat>(
            r#"
            SELECT id, workspace_id, assigned_user_id, duration_option, seat_start, seat_expiry, created_at
            FROM seats
            WHERE workspace_id = $1
            ORDER BY created_at
            "#,
        )
        .bind(workspace_id)
        .fetch_all(&self.pool)
        .await
        .map_err(|_| AppError::InternalError)?;

        Ok(seats)
    }
}
