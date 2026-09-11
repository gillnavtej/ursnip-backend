//! Property-based & unit tests for Billing / seat management (Feature 6,
//! Properties 10–16, tasks 8.5–8.12).
//!
//! These tests validate the seat-expiry and authorization invariants of
//! `BillingService::add_seat` / `remove_seat` in `src/billing/service.rs`.
//! Like `import_property_tests.rs`, they run against a SIMULATED in-memory model
//! (no real database) that mirrors the production semantics:
//!
//!   * `SeatDuration::Months(n)` is valid only for n ∈ {1,2,3,6}; any other n (or a
//!     missing duration) is `InvalidSeatDuration` and creates no seat.
//!   * expiry: `Months(n)` → `seat_start + n months` (chrono month math);
//!     `UntilPlanLasts` → `period_end`.
//!   * inclusive boundary: reject with `SeatExceedsPaidPeriod` ONLY when
//!     `expiry > period_end` (strictly greater). `expiry == period_end` is allowed.
//!   * `remove_seat`: an assigned seat (`assigned_user_id.is_some()`) → `SeatInUse`;
//!     a seat whose workspace owner ≠ admin → `Forbidden`; a missing seat →
//!     `SeatNotFound`.
//!
//! Month arithmetic uses `chrono::Months` / `checked_add_months` so the simulation
//! matches production exactly.
//!
//! Run with: `cargo test --test billing_property_tests`

use chrono::{DateTime, Months, TimeZone, Utc};
use proptest::prelude::*;
use proptest::test_runner::Config as ProptestConfig;
use uuid::Uuid;

// ─── Simulated error type ────────────────────────────────────────────────────────

/// Mirrors the subset of `AppError` variants the billing seat logic can produce.
#[derive(Debug, Clone, PartialEq, Eq)]
enum BillingError {
    /// `AppError::SeatExceedsPaidPeriod` — expiry strictly beyond `period_end`.
    SeatExceedsPaidPeriod,
    /// `AppError::InvalidSeatDuration` — unsupported / missing duration.
    InvalidSeatDuration,
    /// `AppError::SeatInUse` — assigned seat cannot be removed until cleared.
    SeatInUse,
    /// `AppError::SeatNotFound` — no such seat.
    SeatNotFound,
    /// `AppError::Forbidden` — non-owner / cross-team authorization failure.
    Forbidden,
}

// ─── Simulated seat duration (mirrors `SeatDuration`) ──────────────────────────────

/// Mirror of `billing::service::SeatDuration`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SimSeatDuration {
    /// Fixed whole months. Valid only for {1,2,3,6}.
    Months(u8),
    /// Seat lasts until the paying plan's current period ends.
    UntilPlanLasts,
    /// Represents a request that arrives with NO usable duration (missing).
    Missing,
}

impl SimSeatDuration {
    /// Mirror of `SeatDuration::as_duration_option`: only {1,2,3,6} months and
    /// `UntilPlanLasts` are supported; anything else → `InvalidSeatDuration`.
    fn validate(&self) -> Result<(), BillingError> {
        match self {
            SimSeatDuration::Months(1)
            | SimSeatDuration::Months(2)
            | SimSeatDuration::Months(3)
            | SimSeatDuration::Months(6) => Ok(()),
            SimSeatDuration::Months(_) => Err(BillingError::InvalidSeatDuration),
            SimSeatDuration::UntilPlanLasts => Ok(()),
            SimSeatDuration::Missing => Err(BillingError::InvalidSeatDuration),
        }
    }
}

// ─── Simulated seat row (mirrors `Seat`) ───────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
struct SimSeat {
    id: Uuid,
    workspace_id: Uuid,
    assigned_user_id: Option<Uuid>,
    seat_start: DateTime<Utc>,
    seat_expiry: DateTime<Utc>,
}

// ─── Simulated billing store ───────────────────────────────────────────────────────
//
// Mirrors the production `add_seat` / `remove_seat` semantics against an in-memory
// seat list plus a per-workspace owner + subscription `period_end` map.

#[derive(Debug, Clone)]
struct SimBillingStore {
    /// (workspace_id, owner_id, period_end)
    workspaces: Vec<(Uuid, Uuid, DateTime<Utc>)>,
    /// All seat rows across all workspaces.
    seats: Vec<SimSeat>,
}

impl SimBillingStore {
    fn new() -> Self {
        Self {
            workspaces: Vec::new(),
            seats: Vec::new(),
        }
    }

    /// Register a workspace with an owner and its subscription period end.
    fn add_workspace(&mut self, workspace_id: Uuid, owner_id: Uuid, period_end: DateTime<Utc>) {
        self.workspaces.push((workspace_id, owner_id, period_end));
    }

    fn owner_of(&self, workspace_id: Uuid) -> Option<Uuid> {
        self.workspaces
            .iter()
            .find(|(wid, _, _)| *wid == workspace_id)
            .map(|(_, owner, _)| *owner)
    }

    fn period_end_of(&self, workspace_id: Uuid) -> Option<DateTime<Utc>> {
        self.workspaces
            .iter()
            .find(|(wid, _, _)| *wid == workspace_id)
            .map(|(_, _, pe)| *pe)
    }

    /// Number of (usable) seats currently allocated to a workspace.
    fn seat_count(&self, workspace_id: Uuid) -> usize {
        self.seats
            .iter()
            .filter(|s| s.workspace_id == workspace_id)
            .count()
    }

    /// Mirror of `BillingService::add_seat`.
    ///
    /// Order of checks matches production:
    ///   1. validate duration (`InvalidSeatDuration`)
    ///   2. owner authorization (`Forbidden`)
    ///   3. compute expiry
    ///   4. inclusive boundary (`SeatExceedsPaidPeriod` iff expiry > period_end)
    ///   5. insert exactly one seat
    fn add_seat(
        &mut self,
        admin_id: Uuid,
        workspace_id: Uuid,
        duration: SimSeatDuration,
        seat_start: DateTime<Utc>,
        assigned_user_id: Option<Uuid>,
    ) -> Result<SimSeat, BillingError> {
        // Step 1: validate the duration up front.
        duration.validate()?;

        // Step 2: owner authorization. Missing workspace fails authorization too.
        match self.owner_of(workspace_id) {
            Some(owner) if owner == admin_id => {}
            _ => return Err(BillingError::Forbidden),
        }

        // Step 3: period_end must exist (subscription present).
        let period_end = self
            .period_end_of(workspace_id)
            .expect("workspace registered above has a period_end");

        // Step 4: compute expiry with chrono month math.
        let expiry = match duration {
            SimSeatDuration::Months(n) => match seat_start.checked_add_months(Months::new(n as u32))
            {
                Some(e) => e,
                None => return Err(BillingError::SeatExceedsPaidPeriod),
            },
            SimSeatDuration::UntilPlanLasts => period_end,
            SimSeatDuration::Missing => unreachable!("rejected by validate()"),
        };

        // Step 5: inclusive-boundary enforcement — strictly greater is rejected.
        if expiry > period_end {
            return Err(BillingError::SeatExceedsPaidPeriod);
        }

        let seat = SimSeat {
            id: Uuid::new_v4(),
            workspace_id,
            assigned_user_id,
            seat_start,
            seat_expiry: expiry,
        };
        self.seats.push(seat.clone());
        Ok(seat)
    }

    /// Mirror of `BillingService::remove_seat`.
    ///
    /// Order of checks matches production:
    ///   1. load seat (`SeatNotFound`)
    ///   2. owner authorization (`Forbidden`)
    ///   3. assigned → `SeatInUse`
    ///   4. delete
    fn remove_seat(&mut self, admin_id: Uuid, seat_id: Uuid) -> Result<(), BillingError> {
        let seat = self
            .seats
            .iter()
            .find(|s| s.id == seat_id)
            .cloned()
            .ok_or(BillingError::SeatNotFound)?;

        match self.owner_of(seat.workspace_id) {
            Some(owner) if owner == admin_id => {}
            _ => return Err(BillingError::Forbidden),
        }

        if seat.assigned_user_id.is_some() {
            return Err(BillingError::SeatInUse);
        }

        self.seats.retain(|s| s.id != seat_id);
        Ok(())
    }

    /// Clear a seat's assignment (mirrors the app clearing `assigned_user_id`).
    fn clear_assignment(&mut self, seat_id: Uuid) {
        if let Some(seat) = self.seats.iter_mut().find(|s| s.id == seat_id) {
            seat.assigned_user_id = None;
        }
    }
}

// ─── Strategies ───────────────────────────────────────────────────────────────────

/// A fixed reference "now" so month math is deterministic across cases.
fn base_start() -> DateTime<Utc> {
    // 2024-01-15T12:00:00Z — mid-month, mid-year, so month addition is unambiguous.
    Utc.with_ymd_and_hms(2024, 1, 15, 12, 0, 0).unwrap()
}

fn uuid_strategy() -> impl Strategy<Value = Uuid> {
    Just(()).prop_map(|_| Uuid::new_v4())
}

/// A supported fixed-month count (the only ones that create a seat).
fn supported_months_strategy() -> impl Strategy<Value = u8> {
    prop_oneof![Just(1u8), Just(2u8), Just(3u8), Just(6u8)]
}

/// An UNsupported month count (0, 4, 5, 7..=60) — always `InvalidSeatDuration`.
fn unsupported_months_strategy() -> impl Strategy<Value = u8> {
    prop_oneof![Just(0u8), Just(4u8), Just(5u8), 7u8..=60u8]
}

/// Offset (in whole seconds) added to `base_start` so seat_start varies per case.
fn start_offset_strategy() -> impl Strategy<Value = i64> {
    0i64..=(60 * 60 * 24 * 400)
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    // ─── 8.5 ────────────────────────────────────────────────────────────────────
    // Feature: ursnip-feature-matrix, Property 10: Seat expiry never exceeds the paid period (inclusive boundary)
    //
    // Validates Req 16.1, 16.3, 16.4, 15.1. Generators include the exact
    // `start + N months == period_end` boundary. For any add-seat that RESULTS IN a
    // created seat, `seat_expiry <= period_end`.
    #[test]
    fn prop_seat_expiry_never_exceeds_paid_period(
        admin_id in uuid_strategy(),
        workspace_id in uuid_strategy(),
        months in supported_months_strategy(),
        start_offset in start_offset_strategy(),
        // How many EXTRA whole months of slack the period_end has beyond expiry.
        // 0 => exact-boundary case (start + N months == period_end).
        extra_period_months in 0u32..=6,
        use_until_plan_lasts in any::<bool>(),
    ) {
        let seat_start = base_start() + chrono::Duration::seconds(start_offset);

        // period_end sits `extra_period_months` beyond the fixed-month expiry, so
        // extra_period_months == 0 hits the inclusive boundary EXACTLY.
        let fixed_expiry = seat_start
            .checked_add_months(Months::new(months as u32))
            .expect("start + N months in range");
        let period_end = fixed_expiry
            .checked_add_months(Months::new(extra_period_months))
            .expect("period_end in range");

        let mut store = SimBillingStore::new();
        store.add_workspace(workspace_id, admin_id, period_end);

        let duration = if use_until_plan_lasts {
            SimSeatDuration::UntilPlanLasts
        } else {
            SimSeatDuration::Months(months)
        };

        let result = store.add_seat(admin_id, workspace_id, duration, seat_start, None);

        // Every duration here is supported, within period, owner-authorized → seat created.
        let seat = result.expect("seat must be created within the paid period");
        prop_assert!(
            seat.seat_expiry <= period_end,
            "seat_expiry ({}) must be <= period_end ({})",
            seat.seat_expiry, period_end
        );
    }

    // ─── 8.6 ────────────────────────────────────────────────────────────────────
    // Feature: ursnip-feature-matrix, Property 11: Over-period fixed durations create no seat
    //
    // Validates Req 16.2. For any fixed duration whose computed expiry
    // (start + N months) is STRICTLY greater than period_end → rejected with
    // SeatExceedsPaidPeriod AND no seat created.
    #[test]
    fn prop_over_period_fixed_duration_creates_no_seat(
        admin_id in uuid_strategy(),
        workspace_id in uuid_strategy(),
        months in supported_months_strategy(),
        start_offset in start_offset_strategy(),
        // Pull period_end strictly BEFORE the expiry by some positive seconds.
        gap_seconds in 1i64..=(60 * 60 * 24 * 30),
    ) {
        let seat_start = base_start() + chrono::Duration::seconds(start_offset);
        let expiry = seat_start
            .checked_add_months(Months::new(months as u32))
            .expect("start + N months in range");
        // period_end strictly before expiry → expiry > period_end.
        let period_end = expiry - chrono::Duration::seconds(gap_seconds);

        let mut store = SimBillingStore::new();
        store.add_workspace(workspace_id, admin_id, period_end);

        let result = store.add_seat(
            admin_id,
            workspace_id,
            SimSeatDuration::Months(months),
            seat_start,
            None,
        );

        prop_assert_eq!(result, Err(BillingError::SeatExceedsPaidPeriod),
            "expiry strictly beyond period_end must be rejected");
        prop_assert_eq!(store.seat_count(workspace_id), 0,
            "no seat may be created when the duration exceeds the paid period");
    }

    // ─── 8.7 ────────────────────────────────────────────────────────────────────
    // Feature: ursnip-feature-matrix, Property 12: Fixed-duration expiry equals start plus N months
    //
    // Validates Req 15.2. For N in {1,2,3,6} within the paid period →
    // seat_expiry == start + N months (chrono month math).
    #[test]
    fn prop_fixed_duration_expiry_equals_start_plus_n_months(
        admin_id in uuid_strategy(),
        workspace_id in uuid_strategy(),
        months in supported_months_strategy(),
        start_offset in start_offset_strategy(),
    ) {
        let seat_start = base_start() + chrono::Duration::seconds(start_offset);
        let expected_expiry = seat_start
            .checked_add_months(Months::new(months as u32))
            .expect("start + N months in range");
        // Give the plan plenty of room so the seat is created (well within period).
        let period_end = expected_expiry + chrono::Duration::days(365);

        let mut store = SimBillingStore::new();
        store.add_workspace(workspace_id, admin_id, period_end);

        let seat = store
            .add_seat(admin_id, workspace_id, SimSeatDuration::Months(months), seat_start, None)
            .expect("seat within period must be created");

        prop_assert_eq!(seat.seat_expiry, expected_expiry,
            "fixed-duration expiry must equal start + N months");
    }

    // ─── 8.8 ────────────────────────────────────────────────────────────────────
    // Feature: ursnip-feature-matrix, Property 13: "Until plan lasts" expiry equals the period end
    //
    // Validates Req 15.3. UntilPlanLasts → seat_expiry == subscription.period_end.
    #[test]
    fn prop_until_plan_lasts_expiry_equals_period_end(
        admin_id in uuid_strategy(),
        workspace_id in uuid_strategy(),
        start_offset in start_offset_strategy(),
        period_days in 1i64..=1000,
    ) {
        let seat_start = base_start() + chrono::Duration::seconds(start_offset);
        let period_end = seat_start + chrono::Duration::days(period_days);

        let mut store = SimBillingStore::new();
        store.add_workspace(workspace_id, admin_id, period_end);

        let seat = store
            .add_seat(admin_id, workspace_id, SimSeatDuration::UntilPlanLasts, seat_start, None)
            .expect("until-plan-lasts seat must be created");

        prop_assert_eq!(seat.seat_expiry, period_end,
            "until-plan-lasts expiry must equal the subscription period_end");
    }

    // ─── 8.9 ────────────────────────────────────────────────────────────────────
    // Feature: ursnip-feature-matrix, Property 14: Add and remove change usable allocation by one
    //
    // Validates Req 17.2, 18.1. A successful add-seat increases usable seat
    // allocation by exactly one; a successful remove-seat decreases it by one.
    #[test]
    fn prop_add_and_remove_change_allocation_by_one(
        admin_id in uuid_strategy(),
        workspace_id in uuid_strategy(),
        months in supported_months_strategy(),
        start_offset in start_offset_strategy(),
    ) {
        let seat_start = base_start() + chrono::Duration::seconds(start_offset);
        let expiry = seat_start
            .checked_add_months(Months::new(months as u32))
            .expect("start + N months in range");
        let period_end = expiry + chrono::Duration::days(365);

        let mut store = SimBillingStore::new();
        store.add_workspace(workspace_id, admin_id, period_end);

        let before = store.seat_count(workspace_id);

        // Successful add: unassigned so it can later be removed.
        let seat = store
            .add_seat(admin_id, workspace_id, SimSeatDuration::Months(months), seat_start, None)
            .expect("add must succeed");
        let after_add = store.seat_count(workspace_id);
        prop_assert_eq!(after_add, before + 1,
            "successful add-seat must increase allocation by exactly one");

        // Successful remove.
        store.remove_seat(admin_id, seat.id).expect("remove must succeed");
        let after_remove = store.seat_count(workspace_id);
        prop_assert_eq!(after_remove, before,
            "successful remove-seat must decrease allocation by exactly one");
    }

    // ─── 8.10 ───────────────────────────────────────────────────────────────────
    // Feature: ursnip-feature-matrix, Property 15: Unsupported durations create no seat
    //
    // Validates Req 17.4. Missing/unsupported duration (Months(n), n ∉ {1,2,3,6})
    // → validation error (InvalidSeatDuration), no seat.
    #[test]
    fn prop_unsupported_durations_create_no_seat(
        admin_id in uuid_strategy(),
        workspace_id in uuid_strategy(),
        start_offset in start_offset_strategy(),
        bad_months in unsupported_months_strategy(),
        use_missing in any::<bool>(),
    ) {
        let seat_start = base_start() + chrono::Duration::seconds(start_offset);
        // Generous period so the ONLY possible failure is the duration itself.
        let period_end = seat_start + chrono::Duration::days(3650);

        let mut store = SimBillingStore::new();
        store.add_workspace(workspace_id, admin_id, period_end);

        let duration = if use_missing {
            SimSeatDuration::Missing
        } else {
            SimSeatDuration::Months(bad_months)
        };

        let result = store.add_seat(admin_id, workspace_id, duration, seat_start, None);

        prop_assert_eq!(result, Err(BillingError::InvalidSeatDuration),
            "unsupported/missing duration must be rejected with InvalidSeatDuration");
        prop_assert_eq!(store.seat_count(workspace_id), 0,
            "no seat may be created for an unsupported/missing duration");
    }

    // ─── 8.11 ───────────────────────────────────────────────────────────────────
    // Feature: ursnip-feature-matrix, Property 16: Assigned seats cannot be removed until cleared
    //
    // Validates Req 18.3. Assigned seat → SeatInUse on remove; after clearing the
    // assignment (assigned_user_id = None) → removal succeeds.
    #[test]
    fn prop_assigned_seats_not_removable_until_cleared(
        admin_id in uuid_strategy(),
        workspace_id in uuid_strategy(),
        assignee_id in uuid_strategy(),
        months in supported_months_strategy(),
        start_offset in start_offset_strategy(),
    ) {
        let seat_start = base_start() + chrono::Duration::seconds(start_offset);
        let expiry = seat_start
            .checked_add_months(Months::new(months as u32))
            .expect("start + N months in range");
        let period_end = expiry + chrono::Duration::days(365);

        let mut store = SimBillingStore::new();
        store.add_workspace(workspace_id, admin_id, period_end);

        // Create a seat that IS assigned.
        let seat = store
            .add_seat(
                admin_id,
                workspace_id,
                SimSeatDuration::Months(months),
                seat_start,
                Some(assignee_id),
            )
            .expect("assigned seat must be created");

        // Removal while assigned must fail with SeatInUse, and the seat remains.
        let blocked = store.remove_seat(admin_id, seat.id);
        prop_assert_eq!(blocked, Err(BillingError::SeatInUse),
            "assigned seat must not be removable");
        prop_assert_eq!(store.seat_count(workspace_id), 1,
            "assigned seat must still exist after a blocked removal");

        // After clearing the assignment, removal succeeds.
        store.clear_assignment(seat.id);
        store.remove_seat(admin_id, seat.id).expect("cleared seat must be removable");
        prop_assert_eq!(store.seat_count(workspace_id), 0,
            "cleared seat must be removed successfully");
    }
}

// ─── 8.12 — Unit tests for billing authorization rejections ────────────────────────
//
// Validates Req 17.3, 18.2. These are regular `#[test]` functions (NOT proptest)
// exercising the simulated owner-check authorization that mirrors the service's
// `owner_id == admin_id` guard.

/// Req 17.3: a non-admin (non-owner) add-seat must be rejected with an authorization
/// error (`Forbidden`) and create no seat.
#[test]
fn unit_non_owner_add_seat_is_forbidden() {
    let owner_id = Uuid::new_v4();
    let non_owner_id = Uuid::new_v4();
    let workspace_id = Uuid::new_v4();
    let seat_start = base_start();
    let period_end = seat_start + chrono::Duration::days(365);

    let mut store = SimBillingStore::new();
    store.add_workspace(workspace_id, owner_id, period_end);

    // A valid, within-period, supported duration — so the ONLY reason to reject is
    // that the caller is not the workspace owner.
    let result = store.add_seat(
        non_owner_id,
        workspace_id,
        SimSeatDuration::Months(1),
        seat_start,
        None,
    );

    assert_eq!(
        result,
        Err(BillingError::Forbidden),
        "a non-owner add-seat must be rejected with Forbidden"
    );
    assert_eq!(
        store.seat_count(workspace_id),
        0,
        "a forbidden add-seat must create no seat"
    );
}

/// Req 18.2: a cross-team remove-seat — where the seat belongs to a workspace owned
/// by someone other than the requesting admin — must be rejected with `Forbidden`
/// and leave the seat intact.
#[test]
fn unit_cross_team_remove_seat_is_forbidden() {
    let owner_a = Uuid::new_v4(); // owns workspace A (holds the seat)
    let admin_b = Uuid::new_v4(); // admin of a DIFFERENT team
    let workspace_a = Uuid::new_v4();
    let seat_start = base_start();
    let period_end = seat_start + chrono::Duration::days(365);

    let mut store = SimBillingStore::new();
    store.add_workspace(workspace_a, owner_a, period_end);

    // owner_a legitimately adds an unassigned seat to their own workspace.
    let seat = store
        .add_seat(
            owner_a,
            workspace_a,
            SimSeatDuration::Months(1),
            seat_start,
            None,
        )
        .expect("owner may add a seat to their own workspace");

    // admin_b (a different team's admin) tries to remove workspace A's seat.
    let result = store.remove_seat(admin_b, seat.id);

    assert_eq!(
        result,
        Err(BillingError::Forbidden),
        "a cross-team remove-seat must be rejected with Forbidden"
    );
    assert_eq!(
        store.seat_count(workspace_a),
        1,
        "a forbidden remove-seat must leave the seat intact"
    );
}
