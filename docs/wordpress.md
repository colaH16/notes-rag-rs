# WordPress source integration

WordPress remains the source of truth. Opt-in `RAG_WORDPRESS_API` points at the
internal public WordPress service (`/wp-json/cola16-rag/v1`). Retrieval uses a
separate read token, with no browser credentials, editor access, uploads mount or
WordPress source writes. Only published, unpassworded posts with Markdown are
exported. Subscriber-only/paywall content and stale block-derived Markdown are
excluded. The API returns a fresh catalog revision and per-post Markdown. Preview
reads that same Markdown and links to **WordPress에서 전문 보기**, never to an editor.

## Source authentication

Both `/catalog` and `/posts/{id}` require `X-Cola-RAG-Token`. Missing, wrong or
unconfigured tokens fail closed. `notes-rag-wordpress-source/token` supplies
`RAG_WORDPRESS_TOKEN` to the producer, MCP and web. WordPress receives that same
value as `COLA16_RAG_READ_TOKEN`; tokens are never sent to the browser or logged.
The source API is private, no-store and noindex, even though its eligible posts are
public on WordPress.

PHP-FPM does not automatically inherit container environment variables. The
WordPress public/admin deployments therefore mount `wordpress-rag-php-env` at
`/config/php/zz-cola16-rag.conf`, with only this explicit pool setting:

```ini
[www]
env[COLA16_RAG_READ_TOKEN] = $COLA16_RAG_READ_TOKEN
```

Do not replace it with `clear_env = no`. The internal WordPress Service selects
`wordpress-public`, targets port 80, and requires the canonical Host and forwarded
HTTPS headers already supplied by both clients.

## Incremental index and visibility

The Python `blog_sync.py` producer incrementally fills three additive
`notes_rag.blog_*` tables. It uses the same chunker, tokenizer, model and dimensions
as notes, with a separate generation row and writer advisory lock. Before preparing
bodies or embedding, it verifies the active note generation matches its own
configuration. Existing note allowlists, folder ACLs, generation, vectors and OCR
jobs are preserved.

`notes-rag-blog-sync` runs every 15 minutes in Asia/Seoul, with overlapping runs
forbidden and a minimum complete catalog of 200 posts. Only new or changed posts
are embedded. An unchanged run makes no embedding request, writes no posts, and
leaves the blog generation epoch unchanged. Complete catalog checks before planning
and commit prevent partial API failures from authorizing removals. Large physical
deletions are blocked, reported as `deletion_blocked`, and do not advance the epoch
unless another post was actually written. Investigate that flag rather than
lowering the deletion guard during an unexplained source failure.

Rust MCP reuses the **one query vector** for both sources, merges current before
fallback evidence and keeps the result limit. Selecting a note folder scopes to
notes; the default whole-library search includes notes and blog. Blog results are
gated by the live public catalog and matching source revision/URL. The web role
rechecks that gate even for cached responses. Changed, deleted, passworded or
private posts disappear from served old excerpts without waiting for index cleanup.
An unavailable source hides blog results and reports the degraded scope while
keeping note retrieval available; degraded responses are not saved in the search
cache. The graph remains a note graph.

The blog generation epoch participates in search cache keys through the existing
60-second catalog cache. Newly indexed content therefore invalidates previous
whole-library search results after that catalog refresh, without resetting search
budgets. Unchanged synchronization preserves those cache keys.

Native author Markdown is preserved. The eight legacy block posts converted for
this rollout retained their original HTML/title/date/URL/status, with rollback
backups outside the repository. Later saves refresh derived Markdown only when
conversion succeeds; unsupported changes mark that projection stale. If a native
Markdown post is edited directly as classic HTML instead of through its Markdown
source, its stored Markdown may still describe the previous version. Keep edits in
the source-aware editor; this integration does not rewrite native Markdown from
arbitrary rendered HTML.

## Preview and media

`/read-blog?id=...` re-fetches currently public Markdown. CommonMark, tables and raw
HTML pass through ammonia sanitization. Only image URLs on
`https://cola16.app/wp-content/uploads/` are kept, resolved relative to the original
post URL; foreign tracking images and script URLs are stripped. Only this preview
response adds that uploads prefix to `img-src`. Existing note/asset rules remain as
before. Images load lazily through WordPress's normal public media service/CDN; no
uploads volume is mounted in RAG. Video and unsupported external media can be
viewed at the original WordPress page.

## Rollout and rollback

Keep the sync CronJob suspended while staging. Apply the additive schema and
protected source API first. Verify valid-token HTTP 200, anonymous/wrong-token 403,
internal routing, and equality of Python, Rust manifest and active note DB generation
signatures before any paid embedding.

1. Run `blog --apply --min-posts 200 --max-posts 3` using the staged producer image.
   Verify dimensions, source revisions and unchanged note/vector fingerprints.
2. Deploy the compatible web image and source settings after that committed canary
   generation, while keeping the old MCP note-only. Check the three live previews
   on desktop and mobile, including the original WordPress link.
3. Run the full initial sync, then an unchanged sync. Check the original three
   vectors are reused and unchanged work neither embeds nor advances the epoch.
4. Deploy the compatible MCP image and source settings after full-index validation.
   Check mixed search, note-only folder search, cached queries, media previews and
   the existing note graph. Then enable the 15-minute CronJob.

The 2026-11-01 initial rollout indexed 245 posts into 686 chunks, all with 1536
embedding dimensions. The initial three posts used 721 tokens; the remaining 242
used 395,379 tokens. Existing notes remained at 2,225 pages / 5,938 chunks with the
same generation and vector/content fingerprint. Operational evidence is retained
in `wp-rag-integration/reports/`, separate from temporary review inputs.

For rollback, suspend the sync CronJob in the original GitOps repository, remove
the WordPress source settings from web/MCP, and restore their previous image pins.
The additive blog tables and original-source conversion backups can remain. Do not
drop/reinitialize the note schema or re-run note generation migrations. To retry
normal synchronization, create a one-off Job from the existing CronJob template;
it uses the same reviewed limits and credentials.

## Validation and accepted scope

Mock HTTP sources and isolated PostgreSQL databases cover revocation, failed
embedding/source changes, sanitizer/image policy, generation mismatch, cache epoch
invalidation, blocked deletions, normal/fallback context and unchanged note data.
Live checks use public posts without unpublishing or editing production content.
Actual Safari hardware checks are separate from Chromium browser verification.

The user approved reusing the existing DB writer and embedding key references.
Those credentials remain shared; dedicated blog-only DB and embedding credentials
are optional later hardening. The dedicated source read token does not resolve
that separate least-privilege tradeoff.
