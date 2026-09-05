-- Migration: 0012_drop_organizations.sql
-- Description: Remove the organization feature. Drops the org tables and the
-- nullable org_id columns added to workspaces/subscriptions by 0009. The
-- organization layer has been removed from the application; Teams-tier
-- subscriptions are now workspace-scoped.
--
-- Drop order is FK-safe: first the ALTER-added columns (which reference
-- organizations), then the dependent org tables, then organizations itself.

-- 1. Drop org_id columns (and their partial indexes) from pre-existing tables.
DROP INDEX IF EXISTS idx_workspaces_org;
DROP INDEX IF EXISTS idx_subscriptions_org;
ALTER TABLE workspaces     DROP COLUMN IF EXISTS org_id;
ALTER TABLE subscriptions  DROP COLUMN IF EXISTS org_id;

-- 2. Drop dependent org tables (they CASCADE from organizations, but drop
--    explicitly for clarity and to remove their indexes).
DROP TABLE IF EXISTS workspace_assignments;
DROP TABLE IF EXISTS org_invites;
DROP TABLE IF EXISTS org_members;

-- 3. Drop the organizations table.
DROP TABLE IF EXISTS organizations;