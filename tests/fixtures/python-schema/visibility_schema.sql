-- Additive prerequisite before deploying readers that hide absent documents.
-- Run as the dedicated notes_rag database owner, in one transaction.
SELECT pg_advisory_xact_lock(726147013165);
GRANT SELECT ON notes_rag.missing_pages TO notes_rag_reader;
