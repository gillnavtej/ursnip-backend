# Implementation Plan: UrSnip Feature Matrix

## Overview

This plan implements seven cross-stack feature areas across `ursnip-backend` (Rust / Axum / sqlx / Postgres) and `ursnip-app` (React / TanStack Router / Tauri). Each task notes which project it touches.

Sequencing is dependency-driven:

1. **Migrations first**, starting with the CRITICAL `0015_notifications` migration — notification writes fail today because the table was never created, and multiple features emit notifications.
2. **Foundational backend** (error variants, service scaffolding, `Notification` struct/classification, retry) before handlers/endpoints.
3. **Handlers/endpoints** before the **frontend** that calls them.
4. **Property tests** (proptest, ≥100 iterations, tagged per property) placed immediately after the code each validates.

Property tests follow the design's tagging convention:
`// Feature: ursnip-feature-matrix, Property {number}: {property_text}`

---

## Tasks

- [x] 1. Database migrations and shared backend foundation (`ursnip-backend`)
  - [x] 1.1 Create the notifications table migration (CRITICAL — fixes latent defect)
    - Add `ursnip-backend/migrations/0015_notifications.sql` per design: columns `id, user_id, workspace_id, event_type, title, description, metadata (jsonb), classification (default 'system', CHECK IN ('system','promotional')), is_read, created_at`
    - Add indexes `idx_notifications_user_created` and partial `idx_notifications_user_unread`
    - Column set must match `NotificationService`'s existing `SELECT *` / `FromRow` usage
    - _Requirements: 20.1, 25.2_

  - [x] 1.2 Create the notification_preferences table migration
    - Add `ursnip-backend/migrations/0016_notification_preferences.sql`: `user_id (PK), system_enabled, promotional_enabled, security_system_enabled` (all default TRUE), `created_at`, `updated_at`
    - _Requirements: 26.1, 26.2, 26.5_

  - [x] 1.3 Create the seats table migration
    - Add `ursnip-backend/migrations/0017_seats.sql`: `id, workspace_id, assigned_user_id (nullable, ON DELETE SET NULL), duration_option (CHECK IN '1m','2m','3m','6m','until_plan_lasts'), seat_start, seat_expiry, created_at`
    - Add indexes `idx_seats_workspace`, partial `idx_seats_assigned`, `idx_seats_expiry`
    - _Requirements: 15.1, 15.2, 15.3, 16.1_

  - [x] 1.4 Create the expiry reminders marker table migration
    - Add `ursnip-backend/migrations/0018_expiry_reminders_sent.sql`: `id, subscription_id, period_end, threshold_days (CHECK IN 30,15,7,3,1), sent_at`
    - Add unique index `idx_expiry_reminders_unique` on `(subscription_id, period_end, threshold_days)`
    - _Requirements: 23.3_

  - [x] 1.5 Add new AppError variants
    - In `ursnip-backend/src/errors.rs` add `RecipientNotFound` (404), `ImportParseError` (422), `ReferrerNotOnPaidPlan` (403), `SeatExceedsPaidPeriod` (422), `InvalidSeatDuration` (422), `SeatInUse` (409), `SeatNotFound` (404), and a trigger-conflict variant if not reusing the existing sync one
    - Wire each to its HTTP status in the existing `IntoResponse`/status mapping
    - _Requirements: 1.4, 4.6, 12.2, 12.3, 16.2, 17.4, 18.3, 18.1_

  - [x] 1.6 Smoke test: notifications table exists post-migration
    - Add a migration/smoke test (`ursnip-backend/tests/`) asserting the `notifications` table and columns exist after migrations run
    - _Requirements: 20.1_

- [x] 2. Notification service fix, classification, and preferences (`ursnip-backend`)
  - [x] 2.1 Add classification to the Notification model and writes
    - In `ursnip-backend/src/notifications/service.rs`, add `classification` to the `Notification` struct and `CreateNotification` (default `system`), and include it in `create` / `notify_workspace_members` INSERT statements
    - _Requirements: 25.2_

  - [x] 2.2 Write property test for notification persistence round-trip
    - **Property 17: Notification persistence round-trip**
    - **Validates: Requirements 20.2**
    - proptest ≥100 iters; write via service, list, assert equal `user_id, workspace_id, event_type, title, description, metadata`

  - [x] 2.3 Write property test for notification classification
    - **Property 21: Every notification is classified**
    - **Validates: Requirements 25.2**
    - proptest ≥100 iters; assert every created notification's classification is exactly `system` or `promotional`

  - [x] 2.4 Add bounded retry to notification writes
    - Wrap the insert in a bounded retry loop (mirror scheduler 3-attempt policy) that retries transient DB errors, then surfaces/logs failure rather than discarding silently
    - _Requirements: 20.3_

  - [x] 2.5 Write unit test for retry-then-surface behavior
    - Simulate transient failure then success (retry succeeds) and persistent failure (surfaces after bound)
    - _Requirements: 20.3_

  - [x] 2.6 Implement NotificationPreferences model, service, and endpoints
    - Add `ursnip-backend/src/notifications/preferences.rs` with `NotificationPreferences { user_id, system_enabled, promotional_enabled, security_system_enabled }`
    - Add `GET /notifications/preferences` and `PUT /notifications/preferences` (upsert `ON CONFLICT (user_id) DO UPDATE`); absent row = defaults
    - Register routes in `router.rs`
    - _Requirements: 26.1, 26.2_

  - [x] 2.7 Write property test for preferences persistence round-trip
    - **Property 22: Preferences persistence round-trip**
    - **Validates: Requirements 26.1, 26.2**
    - proptest ≥100 iters; PUT then GET returns equal `system_enabled, promotional_enabled, security_system_enabled`

  - [x] 2.8 Enforce preference gating in notification creation
    - Apply promotional opt-out and system opt-out logic; system opt-out must still create expiry + security-relevant device notifications unless `security_system_enabled` is false
    - _Requirements: 26.4, 26.5_

  - [x] 2.9 Write property test for system opt-out preserving expiry/security notifications
    - **Property 23: System opt-out preserves expiry and security notifications**
    - **Validates: Requirements 26.5**
    - proptest ≥100 iters; user opted out of system but not security still receives plan-expiry and security-relevant device notifications

- [x] 3. Checkpoint - notifications foundation
  - Ensure all tests pass, ask the user if questions arise.

- [x] 4. Snippet sharing backend (`ursnip-backend`)
  - [x] 4.1 Scaffold the sharing module and service
    - Create `ursnip-backend/src/sharing/{mod.rs, service.rs, handlers.rs}` with `SharingService { pool, sync: Arc<SyncService> }`, `ShareRequest`, `SharedCopy`
    - _Requirements: 1.1, 1.3_

  - [x] 4.2 Implement share_snippet logic (transactional value copy)
    - Load source + read-access check (403 on failure); resolve recipient via existing user-lookup extended for email (404 `RecipientNotFound` if unresolved, no write)
    - Create a new recipient-owned `snippets` row copying `trigger, content, snippet_type, ai_prompt` with `created_by = recipient_id` (value copy, no FK)
    - Reuse `SyncService` trigger-uniqueness (409 identifying trigger) and tier content-size checks; emit a share notification to recipient
    - _Requirements: 1.1, 1.2, 1.4, 1.5, 2.1, 2.2, 2.3_

  - [x] 4.3 Register the sharing endpoint
    - Add `POST /sharing/snippets` in a `sharing_routes` group in `router.rs` with `Extension(Arc<SharingService>)`; source workspace guarded by `workspace_context_guard`
    - _Requirements: 1.1_

  - [x] 4.4 Write property test for shared copy preservation and independence
    - **Property 1: Shared copy preserves data and is independent of the source**
    - **Validates: Requirements 1.1, 1.2**
    - proptest ≥100 iters; copy equals source trigger/content; editing source does not change copy

  - [x] 4.5 Write property test for unresolvable recipient
    - **Property 2: Unresolvable recipient creates no snippet**
    - **Validates: Requirements 1.4**
    - proptest ≥100 iters; snippet counts unchanged across all workspaces

  - [x] 4.6 Write property test for trigger conflict
    - **Property 3: Trigger conflict rejects the copy**
    - **Validates: Requirements 2.1, 2.2**
    - proptest ≥100 iters; conflict rejected identifying T, no copy created

  - [x] 4.7 Write unit test for read-access authorization rejection
    - Requester without read access to source → authorization error
    - _Requirements: 1.5_

- [x] 5. Import parsing backend (`ursnip-backend`)
  - [x] 5.1 Implement ImportService::parse
    - In `ursnip-backend/src/sync/`, add `ImportService::parse(payload) -> Result<Vec<CandidateSnippet>, ParseError>` producing `BatchOperation::CreateSnippet` values; canonical schema `{ version, workspace_name?, snippets:[{trigger, content, snippet_type, ai_prompt}] }`
    - Reject invalid JSON / wrong shape with `ImportParseError` and ingest nothing
    - _Requirements: 4.1, 4.6_

  - [x] 5.2 Wire parsed candidates to the batch endpoint and per-item reporting
    - Map candidates to `CreateSnippetPayload` and post to `POST /sync/snippets/batch`; surface per-item violations `{ index, trigger, rule }` from `BatchItemResult` (uniqueness, prefix-conflict, content-size)
    - _Requirements: 4.2, 4.3, 4.4, 4.5, 4.7_

  - [x] 5.3 Write property test for invalid import payload
    - **Property 5: Invalid import payload ingests nothing**
    - **Validates: Requirements 4.6**
    - proptest ≥100 iters; include empty strings, truncated JSON, valid-JSON-wrong-shape, non-UTF-8-ish; zero snippets ingested

  - [x] 5.4 Write integration test for import ingestion via batch endpoint
    - Import posts to `/sync/snippets/batch` and rows appear in the workspace
    - _Requirements: 4.2_

- [x] 6. Referral Pro/Team gate (`ursnip-backend`)
  - [x] 6.1 Add the paid-tier gate in record_referral
    - In `ursnip-backend/src/subscription/service.rs` `record_referral`, after resolving `referrer_id` and before the duplicate check/INSERT, query the referrer's active individual-workspace subscription tier; allow only `pro`/`teams`, else return `ReferrerNotOnPaidPlan` with no referral row
    - _Requirements: 12.1, 12.2, 12.3, 12.4_

  - [x] 6.2 Write property test for referral requiring an active paid referrer
    - **Property 8: Referral requires an active paid referrer**
    - **Validates: Requirements 12.2, 12.3**
    - proptest ≥100 iters; free/no-active-subscription rejected, referral count unchanged

  - [x] 6.3 Write property test for eligible referrer recording a pending referral
    - **Property 9: Eligible referrer records a pending referral**
    - **Validates: Requirements 12.1, 12.4**
    - proptest ≥100 iters; active pro/teams + valid non-self non-duplicate referee creates exactly one `pending` referral

- [x] 7. Checkpoint - sharing, import, referral gate
  - Ensure all tests pass, ask the user if questions arise.

- [x] 8. Billing & seat management backend (`ursnip-backend`)
  - [x] 8.1 Scaffold the billing module and service
    - Create `ursnip-backend/src/billing/{mod.rs, service.rs, handlers.rs}` with `SeatDuration`, `AddSeatRequest`, `Seat`, and `BillingService { pool }`
    - _Requirements: 15.1_

  - [x] 8.2 Implement add_seat with expiry computation and inclusive boundary
    - Compute expiry: `Months(n)` → `now + n months`; `UntilPlanLasts` → `period_end`; reject only when `expiry > period_end` (`SeatExceedsPaidPeriod`); reject unsupported/missing duration (`InvalidSeatDuration`); `check_role_owner` authorization; transactional insert
    - _Requirements: 15.2, 15.3, 16.1, 16.2, 16.3, 16.4, 17.1, 17.2, 17.3, 17.4_

  - [x] 8.3 Implement remove_seat and list_seats
    - `remove_seat`: authorization (seat belongs to admin's team), `SeatInUse` when assigned until cleared, `SeatNotFound` otherwise, decrease usable allocation by one; `list_seats(workspace_id)`
    - _Requirements: 18.1, 18.2, 18.3_

  - [x] 8.4 Register billing endpoints and switch frontend target
    - Add `POST /subscriptions/seats`, `DELETE /subscriptions/seats/:seat_id`, `GET /subscriptions/seats?workspace_id=` (workspace-scoped, `check_role_owner`) in `router.rs`; keep legacy `add-seats` for compatibility
    - _Requirements: 17.1, 18.1_

  - [x] 8.5 Write property test for seat expiry inclusive boundary invariant
    - **Property 10: Seat expiry never exceeds the paid period (inclusive boundary)**
    - **Validates: Requirements 16.1, 16.3, 16.4, 15.1**
    - proptest ≥100 iters; include `start + N months == period_end` exactly; created `seat_expiry <= period_end`

  - [x] 8.6 Write property test for over-period fixed durations creating no seat
    - **Property 11: Over-period fixed durations create no seat**
    - **Validates: Requirements 16.2**
    - proptest ≥100 iters; strictly-greater expiry rejected, no seat created

  - [x] 8.7 Write property test for fixed-duration expiry equals start plus N months
    - **Property 12: Fixed-duration expiry equals start plus N months**
    - **Validates: Requirements 15.2**
    - proptest ≥100 iters; N in {1,2,3,6} within period → expiry == start + N months

  - [x] 8.8 Write property test for "until plan lasts" expiry
    - **Property 13: "Until plan lasts" expiry equals the period end**
    - **Validates: Requirements 15.3**
    - proptest ≥100 iters; expiry == subscription.period_end

  - [x] 8.9 Write property test for allocation change on add/remove
    - **Property 14: Add and remove change usable allocation by one**
    - **Validates: Requirements 17.2, 18.1**
    - proptest ≥100 iters; add +1, remove -1 usable allocation

  - [x] 8.10 Write property test for unsupported durations
    - **Property 15: Unsupported durations create no seat**
    - **Validates: Requirements 17.4**
    - proptest ≥100 iters; missing/unsupported duration → validation error, no seat

  - [x] 8.11 Write property test for assigned seats not removable until cleared
    - **Property 16: Assigned seats cannot be removed until cleared**
    - **Validates: Requirements 18.3**
    - proptest ≥100 iters; assigned → `SeatInUse`; after clearing → removal succeeds

  - [x] 8.12 Write unit tests for billing authorization rejections
    - Non-admin add-seat and cross-team remove-seat → authorization error
    - _Requirements: 17.3, 18.2_

- [x] 9. Checkpoint - billing backend
  - Ensure all tests pass, ask the user if questions arise.

- [x] 10. Notification triggers and expiry-reminder scheduler (`ursnip-backend`)
  - [x] 10.1 Wire workspace invite notifications
    - Emit `create` System_Notifications for sender and receiver at the workspace-invite event site
    - _Requirements: 21.1, 21.2_

  - [x] 10.2 Wire subscription activation notification (non-blocking)
    - On paid-tier activation, emit via `persist_notification_async` so failure does not block activation
    - _Requirements: 22.1, 22.2_

  - [x] 10.3 Wire device register/remove notifications (non-blocking)
    - On device register/remove, emit non-blocking System_Notification; failure does not block the device operation
    - _Requirements: 24.1, 24.2_

  - [x] 10.4 Implement the expiry-reminder scheduler job
    - Add `spawn_task("expiry_reminder_check", ...)` (hourly) in `ursnip-backend/src/scheduler/`; create plan-expiry notification iff whole-day distance to `period_end` is in {30,15,7,3,1}; guard idempotence via `INSERT ... ON CONFLICT DO NOTHING` on `expiry_reminders_sent`
    - _Requirements: 23.1, 23.2, 23.3_

  - [x] 10.9 Implement promotional broadcast endpoint
    - Add an admin endpoint that fans out promotional notifications to targeted users who have not opted out
    - _Requirements: 25.1_

  - [x] 10.5 Write property test for expiry reminders firing at thresholds
    - **Property 18: Expiry reminders fire exactly at the day thresholds**
    - **Validates: Requirements 23.1, 23.2**
    - proptest ≥100 iters; include exact 30/15/7/3/1 and off-by-one (29,31,2,0) to confirm iff

  - [x] 10.6 Write property test for expiry reminder idempotence
    - **Property 19: Expiry reminders are idempotent per threshold**
    - **Validates: Requirements 23.3**
    - proptest ≥100 iters; repeated runs → at most one notification per (subscription, period_end, threshold)

  - [x] 10.7 Write property test for promotional opt-out
    - **Property 20: Promotional notifications respect opt-out**
    - **Validates: Requirements 25.1, 26.4**
    - proptest ≥100 iters; promotional notification created iff user has not opted out

  - [x] 10.8 Write unit tests for triggered/non-blocking notification behavior
    - Assert invite/subscription/device triggers create notifications; subscription and device operations succeed even when notification write fails
    - _Requirements: 21.1, 21.2, 22.1, 22.2, 24.1, 24.2_

- [x] 11. Workspace CRUD edge-case confirmation (`ursnip-backend`)
  - [x] 11.1 Add explicit empty-name validation on create/update
    - In `ursnip-backend/src/workspace/` create/update handlers, reject whitespace-only/empty names with `InvalidRequestBody` (replace silent no-op on update); confirm ownership + not-found errors via `workspace_context_guard` / `WorkspaceNotFound`
    - _Requirements: 8.1, 8.2, 8.3_

  - [x] 11.2 Write property test for empty workspace name rejection
    - **Property 7: Empty workspace names are rejected**
    - **Validates: Requirements 8.1**
    - proptest ≥100 iters; whitespace-only/empty names rejected, name not persisted

- [x] 12. Checkpoint - notification triggers, scheduler, workspace edge cases
  - Ensure all tests pass, ask the user if questions arise.

- [x] 13. Frontend API clients (`ursnip-app`)
  - [x] 13.1 Add sharing and import/export API clients
    - In `ursnip-app/src/lib/api/`, add `shareSnippet()` (POST /sharing/snippets); add import client posting to the batch endpoint and export helper serializing to the canonical JSON schema
    - _Requirements: 3.1, 4.2, 5.1_

  - [x] 13.2 Add billing/seats and notification-preferences API clients
    - Add seat clients (`addSeat`, `removeSeat`, `listSeats`) targeting the new `/subscriptions/seats` endpoints; add preferences client (GET/PUT `/notifications/preferences`)
    - _Requirements: 19.3, 19.5, 26.3_

  - [x] 13.3 Wire teams API client to real endpoints
    - In `ursnip-app/src/lib/api/workspaces.ts`, ensure `listMembers`, member-remove, email-invite, and permissions get/patch call the real backend endpoints
    - _Requirements: 9.1, 10.2, 11.1, 11.2_

- [x] 14. Frontend snippet sharing and import/export UI (`ursnip-app`)
  - [x] 14.1 Implement the share dialog
    - In `ursnip-app/src/features/snippets`, add a share control to select a recipient and submit; success confirmation naming recipient; error message on any failure (service/network/validation)
    - _Requirements: 3.1, 3.2, 3.3_

  - [x] 14.2 Implement import/export UI with Tauri file I/O
    - Import: file picker → parse/validate → call import client → show per-item violation report; Export: serialize selected snippets or whole-workspace snapshot to JSON and write to device via Tauri file APIs
    - _Requirements: 4.7, 5.1, 5.2, 5.3_

  - [x] 14.3 Write property test for export/import round-trip fidelity
    - **Property 4: Export/import round-trip fidelity**
    - **Validates: Requirements 6.1, 4.1, 5.1**
    - proptest/fast-check ≥100 iters over the shared codec; imported snippets equal exported `trigger, content, snippet_type, ai_prompt`

  - [x] 14.4 Write property test for whole-workspace export completeness
    - **Property 6: Whole-workspace export completeness**
    - **Validates: Requirements 5.1, 5.3**
    - proptest/fast-check ≥100 iters; whole-workspace export contains exactly every non-deleted snippet

  - [x] 14.5 Write e2e tests for share dialog and import/export
    - In `ursnip-app/e2e`, cover share flow and import/export UI
    - _Requirements: 3.1, 3.2, 3.3, 5.2_

- [x] 15. Frontend billing add/remove seat UI (`ursnip-app`)
  - [x] 15.1 Implement the add-seat dialog with duration picker and radio
    - In `ursnip-app/src/features/billing`, add a 1/2/3/6-month stepper plus a separate "until plan lasts" radio; submit selected `Seat_Duration_Option`; display "seat exceeds paid period" on rejection
    - _Requirements: 19.1, 19.2, 19.3, 19.4_

  - [x] 15.2 Implement the remove-seat control with confirmation
    - Add a remove-seat control that requires explicit confirmation before calling the remove endpoint
    - _Requirements: 18.4, 19.5_

  - [x] 15.3 Write e2e test for add-seat stepper/radio and remove-seat confirmation
    - In `ursnip-app/e2e`, verify duration options render, submission, and remove confirmation
    - _Requirements: 19.1, 19.2, 18.4_

- [x] 16. Frontend teams and referral UI (`ursnip-app`)
  - [x] 16.1 Implement email-invite form and code-invite controls
    - In `ursnip-app/src/features/teams`, add email-invite form with email validation (block send + show validation error on invalid); keep code-invite generation, disabling the control when unavailable
    - _Requirements: 9.1, 9.2, 9.3, 9.4, 9.5_

  - [x] 16.2 Implement role assignment/change UI
    - Show each member's current role; role dropdown wired to permissions PATCH API; show updated role on success; on rejection show error and retain previous role
    - _Requirements: 10.1, 10.2, 10.3, 10.4_

  - [x] 16.3 Wire live teams page and member removal
    - Replace `ursnip-app/src/features/teams/data/teams.ts` mock data with `listMembers()`; member-remove calls backend and updates list on success; show error state (not mock) on retrieval failure
    - _Requirements: 11.1, 11.2, 11.3_

  - [x] 16.4 Implement referral eligibility feedback
    - Surface `ReferrerNotOnPaidPlan` as an upgrade message (pro/teams required); on technical error keep the action in rejected-pending state rather than silently failed
    - _Requirements: 14.1, 14.2_

  - [x] 16.5 Write e2e tests for teams and referral UI
    - Cover email-invite validation, role-change UI, live teams page, and referral eligibility message
    - _Requirements: 9.3, 10.2, 11.1, 14.1_

- [x] 17. Frontend notification preferences UI (`ursnip-app`)
  - [x] 17.1 Implement server-backed preferences UI with security confirmation
    - In `ursnip-app/src/features/notifications`, persist system/promotional toggles to the backend endpoint (not just localStorage); require explicit confirmation before disabling security-relevant system notifications
    - _Requirements: 26.3, 26.6_

  - [x] 17.2 Write e2e test for preferences persistence and security-disable confirmation
    - _Requirements: 26.3, 26.6_

- [x] 18. Final checkpoint - full stack
  - Ensure all tests pass, ask the user if questions arise.

## Notes

- Tasks marked with `*` are optional test sub-tasks and can be skipped for a faster MVP.
- Backend property tests use `proptest` (extending `ursnip-backend/tests/subscription_property_tests.rs`), ≥100 iterations, each tagged `// Feature: ursnip-feature-matrix, Property {n}: ...`. Frontend round-trip/completeness properties (4, 6) use the frontend property library over the shared JSON codec.
- Generators must include the design's boundary edge cases (Property 10 exact-boundary, Property 18 off-by-one days, Property 5 malformed payloads, content-size at/just-below/just-above the tier limit).
- Feature 3 (Workspace CRUD) is intentionally light — only empty-name rejection plus its property test, since CRUD is already functional.
- The `0015_notifications` migration is sequenced first because notification writes currently fail and sharing/subscription/device/expiry features all emit notifications.

## Task Dependency Graph

```json
{
  "waves": [
    { "id": 0, "tasks": ["1.1", "1.2", "1.3", "1.4", "1.5"] },
    { "id": 1, "tasks": ["1.6", "2.1", "5.1", "6.1", "8.1", "11.1", "13.3"] },
    { "id": 2, "tasks": ["2.2", "2.3", "2.4", "2.6", "4.1", "5.2", "6.2", "6.3", "8.2", "11.2", "16.1", "16.2", "16.3", "16.4"] },
    { "id": 3, "tasks": ["2.5", "2.7", "2.8", "4.2", "5.3", "5.4", "8.3", "8.4", "13.1", "13.2", "16.5"] },
    { "id": 4, "tasks": ["2.9", "4.3", "4.7", "8.5", "8.6", "8.7", "8.8", "8.9", "8.10", "8.11", "8.12", "10.1", "10.2", "10.3", "10.4", "10.9", "14.1", "14.2", "15.1", "15.2", "17.1"] },
    { "id": 5, "tasks": ["4.4", "4.5", "4.6", "10.5", "10.6", "10.7", "10.8", "14.3", "14.4", "15.3", "17.2"] },
    { "id": 6, "tasks": ["14.5"] }
  ]
}
```
