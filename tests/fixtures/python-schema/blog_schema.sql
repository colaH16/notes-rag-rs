-- Additive WordPress index. Existing note tables and generation stay untouched.
CREATE TABLE notes_rag.blog_generation (
 singleton boolean PRIMARY KEY DEFAULT true CHECK(singleton),
 config_signature text NOT NULL, updated_at timestamptz NOT NULL DEFAULT now()
);
CREATE TABLE notes_rag.blog_posts (
 id bigint PRIMARY KEY CHECK(id>0), page_key text UNIQUE NOT NULL,
 revision text NOT NULL CHECK(revision ~ '^[a-f0-9]{64}$'),
 title text NOT NULL, url text NOT NULL, published text NOT NULL, modified text NOT NULL,
 needs_visual_review boolean NOT NULL DEFAULT false
);
CREATE TABLE notes_rag.blog_chunks (
 post_id bigint NOT NULL REFERENCES notes_rag.blog_posts(id) ON DELETE CASCADE,
 ordinal integer NOT NULL CHECK(ordinal>=0), rag_priority text NOT NULL CHECK(rag_priority IN ('normal','fallback')),
 content text NOT NULL, token_count integer NOT NULL CHECK(token_count>0 AND token_count<=32760),
 embedding public.vector(1536) NOT NULL, PRIMARY KEY(post_id,ordinal)
);
GRANT SELECT ON notes_rag.blog_generation, notes_rag.blog_posts, notes_rag.blog_chunks TO notes_rag_reader;
GRANT SELECT, INSERT, UPDATE, DELETE ON notes_rag.blog_generation, notes_rag.blog_posts, notes_rag.blog_chunks TO notes_rag_indexer;
