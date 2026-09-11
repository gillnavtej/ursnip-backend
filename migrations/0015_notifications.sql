-- Migration: 0015_notifications.sql
-- Description: Create the notifications table. NotificationService already writes
--              user_id, workspace_id, event_type, title, description, metadata,
--              is_read, created_at (see src/notifications/service.rs) and reads with
--              SELECT * into a #[derive(FromRow)] Notification struct, but no migration
--              ever defined the table, so every notification write fails today. This
--              migration fixes that latent defect (Req 20.1).
--              Adds a `classification` column (default 'system') to split system vs
--              promotional notifications (Req 25.2). The column is additive with a
--              default so existing inserts keep working; the FromRow struct + insert
--              queries gain the field in a later task.

CREATE TABLE IF NOT EXISTS notifications (
    id             UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    user_id        UUID NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    workspace_id   UUID NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    event_type     TEXT NOT NULL,
    title          TEXT NOT NULL,
    description    TEXT,
    metadata       JSONB NOT NULL DEFAULT '{}'::jsonb,
    classification TEXT NOT NULL DEFAULT 'system'
                   CHECK (classification IN ('system', 'promotional')),
    is_read        BOOLEAN NOT NULL DEFAULT FALSE,
    created_at     TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- List queries in service.rs filter by user_id and order by created_at DESC.
CREATE INDEX IF NOT EXISTS idx_notifications_user_created
    ON notifications (user_id, created_at DESC);

-- Unread counts/queries filter by user_id where is_read = FALSE.
CREATE INDEX IF NOT EXISTS idx_notifications_user_unread
    ON notifications (user_id) WHERE is_read = FALSE;
