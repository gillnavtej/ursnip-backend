-- Migration: 0010_user_date_format.sql
-- Description: Persist each user's preferred date-fns display pattern.

ALTER TABLE users
    ADD COLUMN date_format TEXT NOT NULL DEFAULT 'dd-MM-yyyy';

ALTER TABLE users
    ADD CONSTRAINT users_date_format_check
    CHECK (date_format IN (
        'dd-MM-yyyy',
        'MM-dd-yyyy',
        'yyyy-MM-dd',
        'dd/MM/yyyy',
        'MM/dd/yyyy',
        'yyyy/MM/dd'
    ));
