-- Migration: 0020_handoff_codes.sql
-- Description: Short-lived, single-use codes for handing an authenticated web
--              session off to another client (the browser extension) without
--              re-entering credentials.
--
--              An authenticated user mints a code (POST /auth/handoff/mint);
--              the extension exchanges it (POST /auth/handoff/exchange) for a
--              standard token pair. This lets OAuth-only users — who have no
--              password — sign into the extension. Codes are stored hashed
--              (SHA-256, like refresh/OTP tokens), expire quickly, and are
--              consumed on first use. Modeled on the otp_codes table.

CREATE TABLE IF NOT EXISTS handoff_codes (
    id          UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    user_id     UUID NOT NULL REFERENCES users(id) ON DELETE CASCADE,

    -- SHA-256 hex of the plaintext code (never stored in plaintext).
    code_hash   TEXT NOT NULL,

    -- Single-use + short TTL.
    used        BOOLEAN NOT NULL DEFAULT FALSE,
    expires_at  TIMESTAMPTZ NOT NULL,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- Fast lookup on exchange (by hash) and for invalidating a user's prior codes.
CREATE UNIQUE INDEX IF NOT EXISTS idx_handoff_codes_hash ON handoff_codes (code_hash);
CREATE INDEX IF NOT EXISTS idx_handoff_codes_user ON handoff_codes (user_id, used, expires_at);
