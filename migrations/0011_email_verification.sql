-- Migration: 0011_email_verification.sql
-- Description: Add email verification state for the email/password signup OTP flow.
-- Tables/columns: users.email_verified (backfilled TRUE for existing rows), otp_codes

-- 1. users.email_verified
-- Add the verification flag, then backfill all existing rows as verified so current
-- users and OAuth accounts are never locked out (grandfathering). New rows default FALSE.
ALTER TABLE users ADD COLUMN email_verified BOOLEAN NOT NULL DEFAULT FALSE;
UPDATE users SET email_verified = TRUE;                           -- existing rows are grandfathered
ALTER TABLE users ALTER COLUMN email_verified SET DEFAULT FALSE;  -- new rows default false

-- 2. otp_codes
-- One-time verification codes. Mirrors password_reset_tokens: store only a hash, never plaintext.
CREATE TABLE otp_codes (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    user_id UUID NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    code_hash TEXT NOT NULL,
    purpose TEXT NOT NULL DEFAULT 'signup_verification'
        CHECK (purpose IN ('signup_verification')),
    attempts INT NOT NULL DEFAULT 0,
    expires_at TIMESTAMPTZ NOT NULL,
    used BOOLEAN NOT NULL DEFAULT FALSE,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX idx_otp_codes_user ON otp_codes (user_id);
CREATE INDEX idx_otp_codes_active ON otp_codes (user_id, used, expires_at);
