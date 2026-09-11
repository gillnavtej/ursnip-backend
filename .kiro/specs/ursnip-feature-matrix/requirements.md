# Requirements Document

## Introduction

The **UrSnip Feature Matrix** is a cross-stack initiative spanning the UrSnip backend (Rust API, `ursnip-backend`) and the UrSnip desktop/web frontend (React/Tauri, `ursnip-app`). A cross-stack gap analysis was performed prior to this document. Each requirement section below records the **existing state** and the **gap** so that scope is unambiguous: some sections close entirely missing features, others round out partial features, and some fix latent defects (for example, the missing `notifications` database table).

This document covers seven feature areas:

1. Snippet Sharing (in-app copy-to-recipient) — MISSING
2. Import & Export (JSON) — MISSING
3. Workspace CRUD — MOSTLY DONE (edge cases only)
4. Team Invite & Role Management — PARTIAL (frontend gaps)
5. Referral Pro/Team Gate — PARTIAL (backend gate not enforced)
6. Billing & Seat Management (per-seat duration) — LARGELY MISSING (most complex)
7. Notifications (system + promotional) — PARTIAL with a CRITICAL latent defect

Acceptance criteria use EARS patterns. Business rules with exact boundaries (the Billing seat-duration constraint and the Referral Pro/Team gate) are specified with explicitly testable criteria.

## Glossary

- **Backend**: The `ursnip-backend` Rust API service.
- **Frontend**: The `ursnip-app` React/Tauri client application.
- **User**: An authenticated account holder.
- **Workspace**: A container that owns snippets and enforces per-workspace rules.
- **Workspace_Owner**: The User who owns a given Workspace.
- **Snippet**: A stored expansion unit with a trigger and content.
- **Trigger**: The unique keyword string that activates a Snippet within a Workspace.
- **Recipient**: The User who receives a shared Snippet.
- **Sharing_Service**: The Backend component that creates recipient-owned copies of shared Snippets.
- **Import_Service**: The Backend component that ingests Snippets into a Workspace.
- **Export_Service**: The Frontend/Backend component that produces a JSON file of Snippets.
- **Batch_Sync_Endpoint**: The existing `POST /sync/snippets/batch` endpoint.
- **Tier**: A subscription level; one of `free`, `pro`, `teams`.
- **Paid_Tier**: A Tier of `pro` or `teams`.
- **Subscription**: A record of a User's Tier and its paid period boundaries.
- **Subscription_Period_End**: The timestamp at which a Subscription's current paid period ends.
- **Team**: A `teams`-tier group with an administrator and members.
- **Team_Admin**: The User who administers a Team.
- **Team_Member**: A User who belongs to a Team.
- **Invite_Code**: An existing code-based token used to join a Team.
- **Referrer**: A User who shares a referral code.
- **Referee**: A User who signs up using a Referrer's referral code.
- **Referral_Service**: The Backend component that records and converts referrals.
- **Referral_Reward**: A one-month Paid_Tier extension allotted to a party once a referral converts.
- **Reward_Claim**: The act by which a party redeems an allotted Referral_Reward.
- **Seat**: A single membership allocation within a Team subscription.
- **Seat_Expiry**: The timestamp at which an individual Seat's paid allocation ends.
- **Seat_Duration_Option**: A selectable seat allocation length; one of 1, 2, 3, or 6 months, or "until plan lasts".
- **Billing_Service**: The Backend component that manages Seats and their durations.
- **Notification**: A record delivered to a User's notification center.
- **System_Notification**: A Notification triggered by a system event (invite, subscription, expiry, device).
- **Promotional_Notification**: A Notification for sale offers, new features, or platform announcements.
- **Notification_Service**: The Backend component that writes Notifications.
- **Notification_Preferences**: A User's opt-in/opt-out settings for Notifications.
- **Expiry_Reminder_Scheduler**: The Backend scheduled job that emits plan-expiry Notifications.

## Requirements

---

## Feature 1: Snippet Sharing

**Existing state:** The Frontend "Share" button only copies a URL to the clipboard. There is no in-app share that delivers a Snippet to another User. User-lookup logic exists and can be reused for Recipient resolution.

**Gap:** Implement in-app sharing where a shared Snippet is saved as an independent **copy** in the Recipient's account (not a live/linked reference).

### Requirement 1: Snippet Sharing — Share a Snippet to a Recipient

**User Story:** As a User, I want to share a snippet directly to another user's account, so that they receive their own editable copy.

#### Acceptance Criteria

1. WHEN a User submits a share request identifying a Snippet and a Recipient, THE Sharing_Service SHALL create a new Snippet owned by the Recipient that copies the source Snippet's trigger and content.
2. THE Sharing_Service SHALL create the shared Snippet as an independent copy such that subsequent edits to the source Snippet do not modify the Recipient's copy.
3. WHEN a share request is submitted, THE Sharing_Service SHALL resolve the Recipient using the existing user-lookup mechanism.
4. IF the Recipient identifier does not resolve to an existing User, THEN THE Sharing_Service SHALL reject the share request with a "recipient not found" error and SHALL NOT create any Snippet.
5. IF the requesting User does not have read access to the source Snippet, THEN THE Sharing_Service SHALL reject the share request with an authorization error.

### Requirement 2: Snippet Sharing — Recipient Workspace Rule Enforcement on Shared Copy

**User Story:** As a Recipient, I want received snippets to obey my workspace rules, so that my workspace stays valid.

#### Acceptance Criteria

1. WHEN the Sharing_Service creates a copy in the Recipient's target Workspace, THE Sharing_Service SHALL enforce that Workspace's trigger uniqueness rule.
2. IF the shared Snippet's Trigger conflicts with an existing Trigger in the Recipient's target Workspace, THEN THE Sharing_Service SHALL reject the copy and SHALL return a trigger-conflict error identifying the conflicting Trigger.
3. WHEN the Sharing_Service creates a copy, THE Sharing_Service SHALL enforce the Recipient's Tier content-size limit on the copied content.

### Requirement 3: Snippet Sharing — Frontend Share Action

**User Story:** As a User, I want a clear in-app share control, so that I can send a snippet without copying URLs.

#### Acceptance Criteria

1. THE Frontend SHALL provide a share control that lets a User select a Recipient and submit an in-app share request to the Sharing_Service.
2. WHEN the Sharing_Service confirms a successful share, THE Frontend SHALL display a success confirmation identifying the Recipient.
3. IF a share request fails for any cause, including a Sharing_Service error, a network timeout, or Frontend validation failure, THEN THE Frontend SHALL display an error message describing the failure to the User.

---

## Feature 2: Import & Export

**Existing state:** No import or export capability exists. The `Batch_Sync_Endpoint` (`POST /sync/snippets/batch`) exists and can be reused for import ingestion. Per-workspace rules (trigger uniqueness, trigger prefix-conflict, Tier content-size limits) are already enforced by existing sync logic.

**Gap:** Implement importing Snippets into an account and exporting Snippets to the local device as JSON.

### Requirement 4: Import & Export — Import Snippets from JSON

**User Story:** As a User, I want to import snippets from a JSON file, so that I can bring existing snippets into my account.

#### Acceptance Criteria

1. WHEN a User submits a JSON import payload for a target Workspace, THE Import_Service SHALL parse the payload into a list of candidate Snippets.
2. WHEN the import payload is parsed successfully, THE Import_Service SHALL ingest the candidate Snippets using the existing `Batch_Sync_Endpoint`.
3. WHEN ingesting candidate Snippets, THE Import_Service SHALL enforce the target Workspace's trigger uniqueness rule.
4. WHEN ingesting candidate Snippets, THE Import_Service SHALL enforce the target Workspace's trigger prefix-conflict rule.
5. WHEN ingesting candidate Snippets, THE Import_Service SHALL enforce the User's Tier content-size limit.
6. IF the import payload is not valid JSON or does not match the expected Snippet schema, THEN THE Import_Service SHALL reject the import with a descriptive parse error and SHALL NOT ingest any Snippet.
7. IF one or more candidate Snippets violate a Workspace rule, THEN THE Import_Service SHALL report each violating Snippet with the specific rule violated.

### Requirement 5: Import & Export — Export Snippets to JSON

**User Story:** As a User, I want to export my snippets to a JSON file on my device, so that I can back them up or move them.

#### Acceptance Criteria

1. WHEN a User requests an export for a selected set of Snippets, THE Export_Service SHALL produce a JSON document containing those Snippets.
2. THE Export_Service SHALL write the exported JSON document to a file on the User's local device.
3. WHERE the User selects an entire Workspace for export, THE Export_Service SHALL include every Snippet in that Workspace in the exported JSON document.

### Requirement 6: Import & Export — Round-Trip Fidelity

**User Story:** As a User, I want exported snippets to import back identically, so that no data is lost in transit.

#### Acceptance Criteria

1. FOR ALL Snippets exported by the Export_Service, after the exported JSON document is imported via the Import_Service into an empty Workspace, THE resulting Snippets SHALL have triggers and content equivalent to the exported Snippets (round-trip postcondition).

---

## Feature 3: Workspace

**Existing state:** Workspace CRUD (list, create, update, delete) exists on both Backend and Frontend and is functional.

**Gap:** No new capability; round out edge cases and confirm boundary behavior. Treat as mostly done.

### Requirement 7: Workspace — CRUD Confirmation

**User Story:** As a User, I want reliable workspace management, so that I can organize my snippets.

#### Acceptance Criteria

1. WHEN a User requests the list of Workspaces, THE Backend SHALL return a list response containing every Workspace the User owns or is a member of, returning an empty list when the User has none.
2. WHEN a User creates a Workspace with a valid name, THE Backend SHALL persist the Workspace and return its identifier.
3. WHEN a Workspace_Owner updates a Workspace's name, THE Backend SHALL persist the updated name.
4. WHEN a Workspace_Owner deletes a Workspace, THE Backend SHALL remove the Workspace and its owned Snippets.

### Requirement 8: Workspace — Edge Cases

**User Story:** As a User, I want workspace operations to fail safely on invalid input, so that data stays consistent.

#### Acceptance Criteria

1. IF a create or update request supplies an empty Workspace name, THEN THE Backend SHALL reject the request with a validation error.
2. IF a User attempts to update or delete a Workspace the User does not own, THEN THE Backend SHALL reject the request with an authorization error.
3. IF a User requests deletion of a Workspace that does not exist, THEN THE Backend SHALL return a "workspace not found" error.

---

## Feature 4: Team Invite & Role Management

**Existing state:** Backend supports Invite_Codes, join, member list/remove, and per-member permissions/roles. Frontend is PARTIAL: only code-based invites exist, there is no role-change UI, and the standalone teams page renders mock data.

**Gap:** Close Frontend gaps — email-based invites, role assignment/change UI, and wiring the teams page to the real Backend API. Backend endpoints are reused.

### Requirement 9: Team Invite — Email-Based Invites (Frontend)

**User Story:** As a Team_Admin, I want to invite members by email, so that I do not have to share raw invite codes manually.

#### Acceptance Criteria

1. WHEN a Team_Admin submits an email address to invite, THE Frontend SHALL send an email-based invitation request to the Backend team-invite API.
2. WHEN the Backend confirms the invitation, THE Frontend SHALL display a confirmation identifying the invited email address.
3. IF the submitted value is not a valid email address, THEN THE Frontend SHALL display a validation error and SHALL NOT send the request.
4. WHERE code-based invites are available, THE Frontend SHALL support generating and displaying an Invite_Code.
5. WHERE code-based invites are unavailable, THE Frontend SHALL disable the Invite_Code generation control.

### Requirement 10: Team Invite — Role Assignment and Change UI (Frontend)

**User Story:** As a Team_Admin, I want to assign and change member roles in the UI, so that I can manage permissions without the API directly.

#### Acceptance Criteria

1. THE Frontend SHALL display each Team_Member's current role in the team management view.
2. WHEN a Team_Admin selects a new role for a Team_Member and confirms the change, THE Frontend SHALL send a role-update request to the existing Backend permissions/roles API.
3. WHEN the Backend confirms a role change, THE Frontend SHALL display the Team_Member's updated role.
4. IF the Backend rejects a role change, THEN THE Frontend SHALL display the returned error and SHALL retain the previously displayed role.

### Requirement 11: Team Invite — Live Teams Page (Frontend)

**User Story:** As a Team_Admin, I want the teams page to show real data, so that I can trust what I see.

#### Acceptance Criteria

1. WHEN the teams page loads, THE Frontend SHALL retrieve the Team_Member list from the Backend member-list API instead of using mock data.
2. WHEN a Team_Admin removes a Team_Member, THE Frontend SHALL call the Backend member-remove API and SHALL update the displayed member list on success.
3. IF the member-list retrieval fails, THEN THE Frontend SHALL display an error state rather than mock data.

---

## Feature 5: Referral

**Existing state:** Referral reward logic exists and correctly grants +1 month of Paid_Tier extension to BOTH the Referrer and the Referee when the Referee activates a paid plan (`convert_referral` / `apply_referral_reward`). Frontend referral UI exists. The reward mechanics are NOT changing.

**Gap:** The rule that a Referrer must currently be on a Paid_Tier (`pro` or `teams`) to send a referral is NOT enforced — `record_referral` accepts any User as Referrer. This gate must be added.

### Requirement 12: Referral — Enforce Pro/Team-Only Referrer Gate

**User Story:** As the business, I want only paid users to send referrals, so that referral rewards are reserved for paying customers.

#### Acceptance Criteria

1. WHEN the Referral_Service records a referral, THE Referral_Service SHALL verify that the Referrer has an active Subscription whose Tier is `pro` or `teams`.
2. IF the Referrer's active Subscription Tier is `free`, THEN THE Referral_Service SHALL reject the referral with a "referrer not on paid plan" error and SHALL NOT create a referral record.
3. IF the Referrer has no active Subscription, THEN THE Referral_Service SHALL reject the referral with a "referrer not on paid plan" error and SHALL NOT create a referral record.
4. WHERE the Referrer has an active `pro` or `teams` Subscription, THE Referral_Service SHALL record the referral with status `pending`.

### Requirement 13: Referral — Reward Allotment and Claim Flow

**User Story:** As a Referrer and Referee, we want the referral reward to be allotted on conversion and claimable, so that we can redeem the free month after upgrading.

#### Acceptance Criteria

1. WHEN a Referee activates a Paid_Tier Subscription for a pending referral, THE Referral_Service SHALL allot a one-month Paid_Tier Referral_Reward to both the Referrer and the Referee in the same conversion transaction.
2. WHEN a referral is converted, THE Referral_Service SHALL set the referral status to `converted`.
3. IF a referral is already `converted`, THEN THE Referral_Service SHALL make no additional Referral_Reward allotment for that referral (idempotence).
4. WHEN a party initiates a Reward_Claim for an allotted Referral_Reward, THE Referral_Service SHALL apply the one-month Paid_Tier extension to that party.
5. WHERE a party who initiates a Reward_Claim is not currently on a Paid_Tier, THE Frontend SHALL display an upgrade prompt and THE Referral_Service SHALL defer the Reward_Claim until the party has activated a Paid_Tier.
6. IF an allotted Referral_Reward has already been claimed, THEN THE Referral_Service SHALL reject a further Reward_Claim for that Referral_Reward (idempotence).

### Requirement 14: Referral — Frontend Referrer Eligibility Feedback

**User Story:** As a free-tier User, I want to understand why I cannot refer, so that I know upgrading unlocks referrals.

#### Acceptance Criteria

1. IF the Referral_Service rejects a referral because the Referrer is not on a Paid_Tier, THEN THE Frontend SHALL display a message explaining that referrals require a `pro` or `teams` plan.
2. WHILE the eligibility message cannot be displayed due to a technical error, THE Frontend SHALL keep the referral action in a rejected-pending state and SHALL NOT present the referral as failed silently until the message is shown.

---

## Feature 6: Billing & Seat Management

**Existing state:** The Backend tracks only integer seat COUNTS on subscriptions (`max_seats`, `extra_seats_purchased`). There is NO per-Seat duration or expiry concept, NO remove-seat operation, and the Frontend has NO add-seat UI. This is the most complex feature area.

**Gap:** Introduce a per-Seat duration/expiry data model, add-seat and remove-seat flows, the "a seat cannot extend beyond the Team subscription's current paid period" invariant, and the add-seat UI (duration stepper with 1/2/3/6-month options plus an "until plan lasts" radio option).

### Requirement 15: Billing — Per-Seat Duration/Expiry Data Model

**User Story:** As a Team_Admin, I want each purchased seat to have its own expiry, so that seats can have different durations.

#### Acceptance Criteria

1. THE Billing_Service SHALL represent each Seat with an individual Seat_Expiry timestamp.
2. WHEN a Seat is created with a fixed Seat_Duration_Option of N months, THE Billing_Service SHALL set the Seat_Expiry to the seat's start time plus N months.
3. WHERE a Seat is created with the "until plan lasts" option, THE Billing_Service SHALL set the Seat_Expiry equal to the Team Subscription_Period_End.

### Requirement 16: Billing — Seat Cannot Exceed Current Paid Period (Boundary Invariant)

**User Story:** As the business, I want seats to never outlast the team's paid period, so that we never grant unpaid access.

#### Acceptance Criteria

1. THE Billing_Service SHALL enforce that every Seat_Expiry is less than or equal to the Team Subscription_Period_End.
2. IF a requested fixed Seat_Duration_Option would set a Seat_Expiry after the Team Subscription_Period_End, THEN THE Billing_Service SHALL reject the add-seat request with a "seat exceeds paid period" error and SHALL NOT create the Seat.
3. WHEN a computed Seat_Expiry exactly equals the Team Subscription_Period_End, THE Billing_Service SHALL accept the add-seat request (the boundary is inclusive).
4. FOR ALL Seats created by the Billing_Service, the created Seat_Expiry SHALL satisfy Seat_Expiry ≤ Team Subscription_Period_End (invariant).

### Requirement 17: Billing — Add Seat Flow

**User Story:** As a Team_Admin, I want to add seats with a chosen duration, so that I can scale my team for a defined period.

#### Acceptance Criteria

1. WHEN a Team_Admin submits an add-seat request specifying a Seat_Duration_Option, THE Billing_Service SHALL create a Seat according to Requirement 6.1 and subject to Requirement 6.2.
2. WHEN a Seat is created, THE Billing_Service SHALL increase the Team's usable seat allocation by one for the duration of the Seat's validity.
3. IF a User who is not the Team_Admin submits an add-seat request, THEN THE Billing_Service SHALL reject the request with an authorization error.
4. IF an add-seat request omits or supplies an unsupported Seat_Duration_Option, THEN THE Billing_Service SHALL reject the request with a validation error.

### Requirement 18: Billing — Remove Seat Flow

**User Story:** As a Team_Admin, I want to remove a seat, so that I can reduce my team allocation.

#### Acceptance Criteria

1. WHEN a Team_Admin submits a remove-seat request for an existing Seat, THE Billing_Service SHALL remove the Seat and decrease the Team's usable seat allocation by one.
2. IF a remove-seat request targets a Seat that does not belong to the Team_Admin's Team, THEN THE Billing_Service SHALL reject the request with an authorization error.
3. IF a remove-seat request targets a Seat currently assigned to a Team_Member, THEN THE Billing_Service SHALL require the assignment to be cleared before removal and SHALL return a "seat in use" error when it is not.
4. WHEN a Team_Admin requests removal of a Seat that is not assigned to any Team_Member, THE Frontend SHALL require explicit confirmation before THE Billing_Service removes the Seat.

### Requirement 19: Billing — Add-Seat UI (Frontend)

**User Story:** As a Team_Admin, I want an add-seat dialog with duration options, so that I can pick how long a seat lasts.

#### Acceptance Criteria

1. WHEN a Team_Admin opens the add-seat control, THE Frontend SHALL present a duration picker offering the values 1 month, 2 months, 3 months, and 6 months as a value/stepper.
2. THE Frontend SHALL present a separate radio option labeled "until plan lasts" alongside the stepper.
3. WHEN a Team_Admin confirms the add-seat dialog, THE Frontend SHALL send an add-seat request carrying the selected Seat_Duration_Option to the Billing_Service.
4. IF the Billing_Service rejects the request because the seat would exceed the paid period, THEN THE Frontend SHALL display the "seat exceeds paid period" message.
5. THE Frontend SHALL provide a remove-seat control that sends a remove-seat request to the Billing_Service.

---

## Feature 7: Notifications

**Existing state:** A `NotificationService` and a notification-center UI exist. CRITICAL DEFECT: the `notifications` database table has no migration and does not exist, so notification writes silently fail today. Only two triggers currently emit notifications (snippet created and snippet deleted). Notification_Preferences are saved only to localStorage; there is no Backend preferences endpoint.

**Gap:** Create the `notifications` table (fixing the latent bug), add the four System_Notification triggers plus the expiry-reminder scheduler (30/15/7/3/1 days), add Promotional_Notifications, and persist Notification_Preferences to the Backend with a system-vs-promotional split.

### Requirement 20: Notifications — Create the Notifications Table (Fix Latent Defect)

**User Story:** As a User, I want my notifications to actually persist, so that the notification center shows real data.

#### Acceptance Criteria

1. THE Backend SHALL provide a database migration that creates a `notifications` table for persisting Notifications.
2. WHEN the Notification_Service writes a Notification after the migration is applied, THE Backend SHALL persist the Notification durably.
3. IF a Notification write fails, THEN THE Notification_Service SHALL retry the write up to a bounded number of attempts before surfacing the failure rather than discarding it silently.

### Requirement 21: Notifications — Workspace Invite Notifications

**User Story:** As a User, I want to be notified about workspace invites I send and receive, so that I can act on them.

#### Acceptance Criteria

1. WHEN a User sends a Workspace invite, THE Notification_Service SHALL create a System_Notification for the sending User recording the sent invite.
2. WHEN a User receives a Workspace invite, THE Notification_Service SHALL create a System_Notification for the receiving User recording the received invite.

### Requirement 22: Notifications — Subscription Notification

**User Story:** As a User, I want a notification when my subscription activates, so that I have a record of it.

#### Acceptance Criteria

1. WHEN a User's Paid_Tier Subscription is activated, THE Notification_Service SHALL create a System_Notification for that User confirming the subscription.
2. IF Notification creation fails during subscription activation, THEN THE Backend SHALL complete the subscription activation and SHALL NOT block activation on the Notification failure.

### Requirement 23: Notifications — Plan Expiry Reminder Scheduler

**User Story:** As a User, I want reminders before my plan expires, so that I can renew in time.

#### Acceptance Criteria

1. THE Expiry_Reminder_Scheduler SHALL evaluate active Paid_Tier Subscriptions for upcoming expiry on a recurring schedule.
2. WHEN a Subscription's Subscription_Period_End is exactly 30, 15, 7, 3, or 1 day(s) in the future at evaluation time, THE Expiry_Reminder_Scheduler SHALL create a plan-expiry System_Notification for the Subscription's User.
3. THE Expiry_Reminder_Scheduler SHALL create at most one plan-expiry Notification per Subscription per reminder day threshold (idempotence per threshold).

### Requirement 24: Notifications — Device Management Notification

**User Story:** As a User, I want to be notified about device management events, so that I stay aware of account access.

#### Acceptance Criteria

1. WHEN a device is registered to or removed from a User's account, THE Notification_Service SHALL create a System_Notification for that User recording the device event.
2. IF Notification creation fails during device registration, THEN THE Backend SHALL complete the device registration and SHALL NOT block registration on the Notification failure.

### Requirement 25: Notifications — Promotional Notifications

**User Story:** As a User, I want promotional notifications, so that I learn about offers, features, and platforms.

#### Acceptance Criteria

1. WHEN an administrator issues a promotional message for sale offers, new features, or platform announcements, THE Notification_Service SHALL create a Promotional_Notification for each targeted User who has not opted out of Promotional_Notifications.
2. THE Notification_Service SHALL classify each Notification as either a System_Notification or a Promotional_Notification.

### Requirement 26: Notifications — Notification Preferences Persistence

**User Story:** As a User, I want my notification preferences saved server-side, so that they follow me across devices.

#### Acceptance Criteria

1. THE Backend SHALL provide an endpoint to persist a User's Notification_Preferences.
2. THE Notification_Preferences SHALL include a separate opt-in/opt-out setting for System_Notifications and for Promotional_Notifications.
3. WHEN a User updates Notification_Preferences in the Frontend, THE Frontend SHALL persist the preferences to the Backend endpoint instead of only localStorage.
4. WHERE a User has opted out of Promotional_Notifications, THE Notification_Service SHALL NOT create Promotional_Notifications for that User.
5. WHERE a User has opted out of System_Notifications, THE Notification_Service SHALL still create expiry and security-relevant device System_Notifications for that User, unless the User has explicitly confirmed disabling security-relevant System_Notifications.
6. WHEN a User requests to disable security-relevant System_Notifications, THE Frontend SHALL require an explicit confirmation before persisting that preference to the Backend.
