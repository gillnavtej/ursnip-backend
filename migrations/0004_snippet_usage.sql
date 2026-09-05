-- Snippet usage tracking table.
-- Records each time a snippet is expanded (triggered) by a user on a device.

CREATE TABLE IF NOT EXISTS snippet_usages (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    snippet_id UUID NOT NULL REFERENCES snippets(id) ON DELETE CASCADE,
    workspace_id UUID NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    user_id UUID NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    device_id UUID REFERENCES devices(id) ON DELETE SET NULL,
    expanded_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX idx_snippet_usages_snippet ON snippet_usages(snippet_id, expanded_at DESC);
CREATE INDEX idx_snippet_usages_workspace ON snippet_usages(workspace_id, expanded_at DESC);
CREATE INDEX idx_snippet_usages_user ON snippet_usages(user_id, expanded_at DESC);
