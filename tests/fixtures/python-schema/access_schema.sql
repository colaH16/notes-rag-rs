-- Additive, fail-closed: unknown notebook IDs are never public.
SELECT pg_advisory_xact_lock(726147013165);
ALTER TABLE notes_rag.pages ADD COLUMN IF NOT EXISTS notebook_id text;
