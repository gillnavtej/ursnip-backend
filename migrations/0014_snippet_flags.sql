-- Migration: 0014_snippet_flags.sql
-- Description: Add ai_prompt, is_enabled, and is_pinned columns to snippets.
--              The application code (sync service create/update/batch/select
--              queries and the SnippetRow / SnippetRowBasic row types) has long
--              expected these columns, but no migration ever defined them. This
--              backfills the schema so snippet creation and updates succeed.

ALTER TABLE snippets
    ADD COLUMN IF NOT EXISTS ai_prompt TEXT;

ALTER TABLE snippets
    ADD COLUMN IF NOT EXISTS is_enabled BOOLEAN NOT NULL DEFAULT true;

ALTER TABLE snippets
    ADD COLUMN IF NOT EXISTS is_pinned BOOLEAN NOT NULL DEFAULT false;
