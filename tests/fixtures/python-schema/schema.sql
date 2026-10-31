-- Dedicated schema; never modify another application's tables.
-- Applied only by the explicit init-db command, inside one transaction.
CREATE EXTENSION IF NOT EXISTS vector;
CREATE SCHEMA notes_rag;
CREATE TABLE notes_rag.schema_version (version integer PRIMARY KEY CHECK (version = 1));
INSERT INTO notes_rag.schema_version VALUES (1);
CREATE TABLE notes_rag.generation (
    singleton boolean PRIMARY KEY DEFAULT true CHECK (singleton),
    scope_signature text NOT NULL,
    config_signature text NOT NULL,
    updated_at timestamptz NOT NULL DEFAULT now()
);
CREATE TABLE notes_rag.pages (
    page_key text PRIMARY KEY,
    notebook_id text,
    source_sha256 text NOT NULL,
    relative_path text NOT NULL,
    title text NOT NULL,
    status text NOT NULL CHECK (status IN ('current', 'old')),
    needs_visual_review boolean NOT NULL,
    updated_at timestamptz NOT NULL DEFAULT now()
);
CREATE TABLE notes_rag.chunks (
    page_key text NOT NULL REFERENCES notes_rag.pages(page_key) ON DELETE CASCADE,
    ordinal integer NOT NULL CHECK (ordinal >= 0),
    rag_priority text NOT NULL CHECK (rag_priority IN ('normal', 'fallback')),
    content text NOT NULL,
    token_count integer NOT NULL CHECK (token_count > 0 AND token_count <= 32760),
    oversize_kind text,
    embedding public.vector(1536) NOT NULL,
    PRIMARY KEY (page_key, ordinal)
);
-- Additive capability; base schema version stays compatible with existing readers.
CREATE TABLE notes_rag.missing_pages (
    page_key text PRIMARY KEY REFERENCES notes_rag.pages(page_key) ON DELETE CASCADE,
    source_sha256 text NOT NULL,
    first_seen timestamptz NOT NULL
);
CREATE INDEX chunks_normal_hnsw ON notes_rag.chunks
    USING hnsw (embedding public.vector_cosine_ops) WHERE rag_priority = 'normal';
CREATE INDEX chunks_fallback_hnsw ON notes_rag.chunks
    USING hnsw (embedding public.vector_cosine_ops) WHERE rag_priority = 'fallback';
