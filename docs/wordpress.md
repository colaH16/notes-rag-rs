# WordPress source integration

WordPress remains the source of truth. Opt-in `RAG_WORDPRESS_API` points at the
internal public WordPress service (`/wp-json/cola16-rag/v1`). No browser credentials,
WordPress editor access, filesystem mount or source writes are used by retrieval.
Only published, unpassworded posts with native Markdown are exported. The shared
API returns a fresh catalog revision and per-post Markdown. Preview reads that same
Markdown directly and links to **WordPress에서 전문 보기**, never to an editor.

The Python `blog_sync.py` producer incrementally fills three additive `notes_rag.blog_*`
tables. It uses the same chunker, tokenizer, model and dimensions as notes, with a
separate generation row and writer advisory lock. Existing note allowlists, folder
ACLs, generation, vectors and OCR jobs are untouched. New/changed posts alone need
embedding. Partial/API failures cannot authorize removals; a second complete catalog
check is required before commit. Large physical deletions are blocked for review.

Rust MCP reuses the **one query vector** for both sources, merges current before
fallback evidence and keeps the result limit. Selecting a note folder scopes to notes;
the default whole-library search includes both notes and blog. Blog results are gated
by the live public catalog and matching source revision/URL. The web role rechecks
that gate even for 3-hour cached responses. Changed, deleted, passworded or private
posts disappear immediately from served old excerpts, without waiting for index
cleanup. An unavailable WordPress source hides blog results and reports the degraded
scope while keeping note retrieval available. The graph remains a note graph.

`/read-blog?id=...` re-fetches the currently public Markdown. CommonMark, tables and
raw HTML pass through ammonia sanitization. Only image URLs on
`https://cola16.app/wp-content/uploads/` are kept, resolved relative to the original
post URL; foreign tracking images and script URLs are stripped. Only this preview
response adds WordPress to `img-src`. Other note/asset/CSP rules stay as before.
Images load lazily through WordPress's normal public media service/CDN. No uploads
volume is mounted in RAG. Video and unsupported external media can be viewed at the
original WordPress page.

Default deployment must leave the source environment unset until schema, producer
preview/canary/full index and real preview checks have passed. Rollback removes the
source env and restores the old reader image; the additive blog tables can remain.
Do not drop/reinitialize the note schema or re-run note generation migrations.

Tests use mock HTTP sources and isolated PostgreSQL databases. Source revocation,
cache hits, sanitizer/image policy, normal/fallback context and unchanged note
index data are covered without production queries or paid embedding in tests.
