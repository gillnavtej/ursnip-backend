-- Migration: 0016_notification_preferences.sql
-- Description: Per-user notification preferences, persisted server-side so they
--              follow the user across devices (Requirement 26).
--
-- One row per user (user_id is the primary key). An absent row is treated as
-- all-defaults (everything enabled) by the application; rows are created via
-- upsert (ON CONFLICT (user_id) DO UPDATE) when a user changes a preference.
--
--   system_enabled          - receive System_Notifications
--   promotional_enabled     - receive Promotional_Notifications
--   security_system_enabled - receive security-relevant System_Notifications
--                             (expiry + security device notifications); kept
--                             separate so opting out of system notifications
--                             does not silently drop security-relevant ones.

-- 1. notification_preferences
CREATE TABLE notification_preferences (
    user_id UUID PRIMARY KEY REFERENCES users(id) ON DELETE CASCADE,
    system_enabled BOOLEAN NOT NULL DEFAULT TRUE,
    promotional_enabled BOOLEAN NOT NULL DEFAULT TRUE,
    security_system_enabled BOOLEAN NOT NULL DEFAULT TRUE,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
