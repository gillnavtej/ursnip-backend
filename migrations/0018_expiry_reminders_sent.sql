-- Migration: 0018_expiry_reminders_sent.sql
-- Description: Track which subscription-expiry reminder emails have been sent so
--              the reminder job stays idempotent. A row is inserted the first
--              time a reminder for a given (subscription, billing period,
--              threshold) is dispatched; the unique index prevents duplicate
--              sends for the same threshold within the same period.

-- ─── Expiry Reminders Sent Table ────────────────────────────────────────────────

CREATE TABLE IF NOT EXISTS expiry_reminders_sent (
    id              UUID PRIMARY KEY DEFAULT gen_random_uuid(),

    -- Linkage
    subscription_id UUID NOT NULL REFERENCES subscriptions(id) ON DELETE CASCADE,

    -- The subscription period this reminder applies to
    period_end      TIMESTAMPTZ NOT NULL,

    -- How many days before period_end this reminder fired
    threshold_days  INTEGER NOT NULL CHECK (threshold_days IN (30, 15, 7, 3, 1)),

    -- Timestamps
    sent_at         TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- ─── Indexes ────────────────────────────────────────────────────────────────────

-- Ensure a reminder for a given threshold is only sent once per subscription
-- period. Also serves as the fast lookup used by the reminder job.
CREATE UNIQUE INDEX IF NOT EXISTS idx_expiry_reminders_unique
    ON expiry_reminders_sent (subscription_id, period_end, threshold_days);
