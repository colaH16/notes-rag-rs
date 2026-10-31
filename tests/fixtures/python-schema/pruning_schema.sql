-- App-owned, additive migration. Existing pages/chunks/readers are unchanged.
SELECT pg_advisory_xact_lock(726147013165);
CREATE TABLE IF NOT EXISTS notes_rag.missing_pages (
    page_key text PRIMARY KEY REFERENCES notes_rag.pages(page_key) ON DELETE CASCADE,
    source_sha256 text NOT NULL,
    first_seen timestamptz NOT NULL
);
GRANT SELECT, INSERT, UPDATE, DELETE ON notes_rag.missing_pages TO notes_rag_indexer;
