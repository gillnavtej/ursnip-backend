-- 0006_permissions.sql
-- Fine-grained RBAC permissions for Teams workspaces.
--
-- workspace_members.permissions: JSONB array of permission strings.
--   NULL = use role-based defaults (backward compatible).
--   Non-null = custom permission set assigned by owner/admin.
--
-- team_invites: when creating an invite, owner can specify what role
-- and permissions the joined user should receive.

-- Add permissions column to workspace_members
ALTER TABLE workspace_members
ADD COLUMN IF NOT EXISTS permissions JSONB DEFAULT NULL;

-- Add role + permissions to team_invites (what the invited user will get)
ALTER TABLE team_invites
ADD COLUMN IF NOT EXISTS invited_role TEXT NOT NULL DEFAULT 'editor';

ALTER TABLE team_invites
ADD COLUMN IF NOT EXISTS invited_permissions JSONB DEFAULT NULL;

-- Index for efficient permission lookups
CREATE INDEX IF NOT EXISTS idx_workspace_members_permissions
ON workspace_members (workspace_id, user_id)
WHERE permissions IS NOT NULL;
