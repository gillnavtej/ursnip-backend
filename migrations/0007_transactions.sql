-- Migration: 0007_transactions.sql
-- Description: Add transactions table for tracking checkout sessions and payment lifecycle.
-- This table bridges the gap between checkout initiation and webhook confirmation,
-- providing a full audit trail for billing operations.

-- ─── Transactions Table ─────────────────────────────────────────────────────────

CREATE TABLE IF NOT EXISTS transactions (
    id                      UUID PRIMARY KEY DEFAULT gen_random_uuid(),

    -- Linkage
    workspace_id            UUID NOT NULL REFERENCES workspaces(id),
    user_id                 UUID NOT NULL REFERENCES users(id),
    external_transaction_id TEXT NOT NULL,           -- Paddle transaction ID (txn_...)
    external_subscription_id TEXT,                   -- Paddle subscription ID (set after activation)

    -- What was purchased
    tier                    TEXT NOT NULL,           -- 'pro' or 'teams'
    billing_cycle_months    INTEGER NOT NULL,        -- 12 = annual

    -- Invoice snapshot (captured at creation time)
    base_price              NUMERIC(10,2) NOT NULL,
    discount_amount         NUMERIC(10,2) NOT NULL DEFAULT 0,
    tax_amount              NUMERIC(10,2) NOT NULL DEFAULT 0,
    total_amount            NUMERIC(10,2) NOT NULL,
    currency                TEXT NOT NULL DEFAULT 'USD',

    -- Lifecycle
    status                  TEXT NOT NULL DEFAULT 'created',
    -- Possible statuses: created, completed, failed, refunded, cancelled

    -- Timestamps
    created_at              TIMESTAMPTZ NOT NULL DEFAULT now(),
    completed_at            TIMESTAMPTZ,            -- When payment was confirmed
    updated_at              TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- ─── Indexes ────────────────────────────────────────────────────────────────────

-- Fast lookup by external Paddle transaction ID (webhook processing)
CREATE UNIQUE INDEX IF NOT EXISTS idx_transactions_external_txn ON transactions (external_transaction_id);

-- List transactions for a workspace (billing history)
CREATE INDEX IF NOT EXISTS idx_transactions_workspace ON transactions (workspace_id, created_at DESC);

-- Find transactions by status (for reconciliation/retry jobs)
CREATE INDEX IF NOT EXISTS idx_transactions_status ON transactions (status) WHERE status NOT IN ('completed', 'cancelled');

-- ─── Link subscriptions to external ID ──────────────────────────────────────────

-- Add external_subscription_id to subscriptions if not already present
DO $$
BEGIN
    IF NOT EXISTS (
        SELECT 1 FROM information_schema.columns
        WHERE table_name = 'subscriptions' AND column_name = 'external_subscription_id'
    ) THEN
        ALTER TABLE subscriptions ADD COLUMN external_subscription_id TEXT;
    END IF;
END $$;
