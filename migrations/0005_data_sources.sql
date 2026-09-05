-- 0005_data_sources.sql
-- Per-user data source configurations for {{data:...}} snippet variables.
-- Stored as JSONB (array of configs) per user — not workspace-scoped.

CREATE TABLE IF NOT EXISTS user_data_sources (
    user_id    UUID PRIMARY KEY REFERENCES users(id) ON DELETE CASCADE,
    configs    JSONB NOT NULL DEFAULT '[]'::jsonb,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
