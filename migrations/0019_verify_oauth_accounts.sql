-- Migration: 0019_verify_oauth_accounts.sql
-- Description: Backfill email_verified = TRUE for accounts that have a linked
--              OAuth identity but were stored unverified.
--
--              Migration 0011 added users.email_verified DEFAULT FALSE and
--              grandfathered rows that existed at that time to TRUE. However,
--              the OAuth new-user INSERT (auth/oauth.rs) did not set the column,
--              so every account created via an OAuth provider AFTER 0011 took
--              the FALSE default. OAuth sign-in itself never checks the flag, so
--              this stayed hidden — until the user attempted password login
--              (e.g. after a "forgot password" reset), which returns
--              EMAIL_VERIFICATION_REQUIRED. These emails are genuinely verified
--              by the provider (we only accept provider-verified emails), so we
--              mark them verified. The code path is fixed alongside this migration.

UPDATE users
SET email_verified = TRUE,
    updated_at = now()
WHERE email_verified = FALSE
  AND id IN (SELECT user_id FROM oauth_accounts);
