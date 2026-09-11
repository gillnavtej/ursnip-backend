-- Migration: 0017_seats.sql
-- Description: Per-seat duration/expiry model for Teams. The existing schema only
--              tracks integer counts (subscriptions.max_seats / extra_seats_purchased);
--              this adds individual seat rows so each seat has its own expiry and can
--              be assigned/removed. Enforces seat_expiry <= subscription.period_end in
--              the application (BillingService), the boundary being inclusive.

CREATE TABLE IF NOT EXISTS seats (
    id               UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    workspace_id     UUID NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    assigned_user_id UUID REFERENCES users(id) ON DELETE SET NULL,  -- nullable: unassigned seat
    duration_option  TEXT NOT NULL
                     CHECK (duration_option IN ('1m','2m','3m','6m','until_plan_lasts')),
    seat_start       TIMESTAMPTZ NOT NULL DEFAULT now(),
    seat_expiry      TIMESTAMPTZ NOT NULL,
    created_at       TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX IF NOT EXISTS idx_seats_workspace ON seats (workspace_id);
CREATE INDEX IF NOT EXISTS idx_seats_assigned ON seats (assigned_user_id) WHERE assigned_user_id IS NOT NULL;
CREATE INDEX IF NOT EXISTS idx_seats_expiry ON seats (seat_expiry);
