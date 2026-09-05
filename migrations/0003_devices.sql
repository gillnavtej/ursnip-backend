-- Device management table
-- Tracks registered devices per user per workspace for sync management.

CREATE TABLE IF NOT EXISTS devices (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    workspace_id UUID NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    user_id UUID NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    device_name VARCHAR(100) NOT NULL,
    device_type VARCHAR(50) NOT NULL DEFAULT 'desktop',
    platform VARCHAR(50),
    last_sync_version BIGINT NOT NULL DEFAULT 0,
    last_sync_at TIMESTAMPTZ,
    is_active BOOLEAN NOT NULL DEFAULT true,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),

    CONSTRAINT uq_device_user_workspace_name UNIQUE (workspace_id, user_id, device_name)
);

CREATE INDEX idx_devices_workspace_user ON devices(workspace_id, user_id);
CREATE INDEX idx_devices_user ON devices(user_id);
