-- Migration: 0009_organizations.sql
-- Description: Add organizations layer for Teams tier.
-- An organization is the billing entity that owns multiple workspaces.
-- Seats are counted at the org level (unique users), not per workspace.

-- ─── Organizations Table ────────────────────────────────────────────────────────

CREATE TABLE IF NOT EXISTS organizations (
    id          UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    name        TEXT NOT NULL,
    owner_id    UUID NOT NULL REFERENCES users(id),
    max_workspaces INTEGER NOT NULL DEFAULT 5,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX IF NOT EXISTS idx_organizations_owner ON organizations (owner_id);

-- ─── Organization Members Table ─────────────────────────────────────────────────
-- Each unique user in an org counts as 1 seat regardless of how many workspaces
-- they are assigned to within that org.

CREATE TABLE IF NOT EXISTS org_members (
    id          UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    org_id      UUID NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    user_id     UUID NOT NULL REFERENCES users(id),
    role        TEXT NOT NULL DEFAULT 'member',  -- owner, admin, member
    joined_at   TIMESTAMPTZ NOT NULL DEFAULT now(),

    UNIQUE (org_id, user_id)
);

CREATE INDEX IF NOT EXISTS idx_org_members_org ON org_members (org_id);
CREATE INDEX IF NOT EXISTS idx_org_members_user ON org_members (user_id);

-- ─── Organization Invites ───────────────────────────────────────────────────────
-- Invites are to the org (not a specific workspace). After joining, owner assigns
-- the member to workspaces.

CREATE TABLE IF NOT EXISTS org_invites (
    id              UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    org_id          UUID NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    invite_code     TEXT NOT NULL UNIQUE,
    max_uses        INTEGER NOT NULL DEFAULT 1,
    times_used      INTEGER NOT NULL DEFAULT 0,
    expires_at      TIMESTAMPTZ NOT NULL,
    created_by      UUID NOT NULL REFERENCES users(id),
    invited_role    TEXT NOT NULL DEFAULT 'member',
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX IF NOT EXISTS idx_org_invites_code ON org_invites (invite_code);
CREATE INDEX IF NOT EXISTS idx_org_invites_org ON org_invites (org_id);

-- ─── Link Workspaces to Organizations ───────────────────────────────────────────
-- Individual workspaces have org_id = NULL. Team workspaces belong to an org.

ALTER TABLE workspaces ADD COLUMN IF NOT EXISTS org_id UUID REFERENCES organizations(id);
CREATE INDEX IF NOT EXISTS idx_workspaces_org ON workspaces (org_id) WHERE org_id IS NOT NULL;

-- ─── Workspace Assignments ──────────────────────────────────────────────────────
-- Which org members have access to which workspaces within the org.
-- Owner/admin of org have implicit access to all workspaces.

CREATE TABLE IF NOT EXISTS workspace_assignments (
    id              UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    org_id          UUID NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    workspace_id    UUID NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    user_id         UUID NOT NULL REFERENCES users(id),
    assigned_at     TIMESTAMPTZ NOT NULL DEFAULT now(),

    UNIQUE (workspace_id, user_id)
);

CREATE INDEX IF NOT EXISTS idx_workspace_assignments_workspace ON workspace_assignments (workspace_id);
CREATE INDEX IF NOT EXISTS idx_workspace_assignments_user ON workspace_assignments (user_id);

-- ─── Move Subscription to Org Level ─────────────────────────────────────────────
-- Add org_id to subscriptions. For teams tier, subscription is linked to org.
-- For free/pro, subscription stays linked to workspace (individual).

ALTER TABLE subscriptions ADD COLUMN IF NOT EXISTS org_id UUID REFERENCES organizations(id);
CREATE INDEX IF NOT EXISTS idx_subscriptions_org ON subscriptions (org_id) WHERE org_id IS NOT NULL;
