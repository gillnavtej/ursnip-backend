-- Migration: 0013_collaborators.sql
-- Description: Teams account-level collaborators with granular access control.
-- Adds three additive tables (no changes to existing tables):
--   collaborations       - links a Teams account holder to an invited collaborator user
--   collaborator_invites - pending email invites (hashed token, 7-day TTL)
--   access_grants        - per-resource view/edit grants (workspace|folder|snippet)
--
-- access_grants.resource_id is polymorphic (no DB FK); referential cleanup on
-- resource deletion is done in the application delete paths.

-- 1. collaborations
-- One row per (owner, collaborator) pair once an invite is accepted.
CREATE TABLE collaborations (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    owner_id UUID NOT NULL REFERENCES users(id) ON DELETE CASCADE,        -- Teams account holder
    collaborator_id UUID NOT NULL REFERENCES users(id) ON DELETE CASCADE, -- the invited user
    status TEXT NOT NULL DEFAULT 'accepted' CHECK (status IN ('accepted')),
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE UNIQUE INDEX idx_collaborations_pair ON collaborations (owner_id, collaborator_id);
CREATE INDEX idx_collaborations_collaborator ON collaborations (collaborator_id);

-- 2. collaborator_invites
-- Pending invites. Mirrors password_reset_tokens: store only a hash, never plaintext.
-- Accepted invites are marked used; a collaborations row is created on acceptance.
CREATE TABLE collaborator_invites (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    owner_id UUID NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    email TEXT NOT NULL,
    token_hash TEXT NOT NULL,
    expires_at TIMESTAMPTZ NOT NULL,
    used BOOLEAN NOT NULL DEFAULT FALSE,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX idx_collab_invites_hash ON collaborator_invites (token_hash);
CREATE INDEX idx_collab_invites_owner ON collaborator_invites (owner_id);

-- 3. access_grants
-- Granular per-resource access. resource_type in workspace|folder|snippet; level in view|edit.
-- No FK on resource_id (polymorphic); integrity enforced in the app and cleaned up in delete paths.
CREATE TABLE access_grants (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    owner_id UUID NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    collaborator_id UUID NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    resource_type TEXT NOT NULL CHECK (resource_type IN ('workspace', 'folder', 'snippet')),
    resource_id UUID NOT NULL,
    level TEXT NOT NULL CHECK (level IN ('view', 'edit')),
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE UNIQUE INDEX idx_access_grants_unique
    ON access_grants (collaborator_id, resource_type, resource_id);
CREATE INDEX idx_access_grants_collaborator ON access_grants (collaborator_id);
CREATE INDEX idx_access_grants_resource ON access_grants (resource_type, resource_id);
