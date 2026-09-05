-- Migration: 0008_seats.sql
-- Description: Add seat management columns to subscriptions for Teams tier.
-- Teams includes 3 seats by default; additional seats can be purchased.

-- Add seat columns to subscriptions
ALTER TABLE subscriptions ADD COLUMN IF NOT EXISTS max_seats INTEGER NOT NULL DEFAULT 1;
ALTER TABLE subscriptions ADD COLUMN IF NOT EXISTS extra_seats_purchased INTEGER NOT NULL DEFAULT 0;

-- Set existing teams subscriptions to 3 seats
UPDATE subscriptions SET max_seats = 3 WHERE tier = 'teams';

-- Set pro to 1 seat (single user)
UPDATE subscriptions SET max_seats = 1 WHERE tier = 'pro';

-- Free tier stays at 1 seat
UPDATE subscriptions SET max_seats = 1 WHERE tier = 'free';
