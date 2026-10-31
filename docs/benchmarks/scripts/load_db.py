#!/usr/bin/env python3
"""Create the isolated synthetic notes_rag database from the oracle's own schema files.

Applies schema.sql, access_schema.sql, pruning_schema.sql and visibility_schema.sql
verbatim from the pinned Python oracle, loads synthetic rows with COPY, and
grants the documented read-only role (SELECT on the RAG tables only).

Usage: load_db.py --oracle DIR --fixture DIR --admin-url URL --reader-password-file F
"""
import argparse
import json
from pathlib import Path

import psycopg
from psycopg import sql

DB = 'notes_rag_bench'
UPDATED_AT = '2026-10-01T00:00:00+00:00'


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--oracle', type=Path, required=True)
    parser.add_argument('--fixture', type=Path, required=True)
    parser.add_argument('--admin-url', required=True, help='postgresql://bench_admin:...@127.0.0.1:PORT/postgres')
    parser.add_argument('--reader-password-file', type=Path, required=True)
    args = parser.parse_args()
    manifest = json.loads((args.fixture / 'manifest.json').read_text())
    reader_password = args.reader_password_file.read_text().strip()

    with psycopg.connect(args.admin_url, autocommit=True) as admin:
        admin.execute(sql.SQL('DROP DATABASE IF EXISTS {}').format(sql.Identifier(DB)))
        admin.execute(sql.SQL('CREATE DATABASE {}').format(sql.Identifier(DB)))
        for role, login in (('notes_rag_reader', True), ('notes_rag_indexer', False)):
            if not admin.execute('SELECT 1 FROM pg_roles WHERE rolname=%s', (role,)).fetchone():
                admin.execute(sql.SQL('CREATE ROLE {} NOLOGIN').format(sql.Identifier(role)))
        admin.execute(sql.SQL('ALTER ROLE notes_rag_reader LOGIN PASSWORD {}').format(sql.Literal(reader_password)))

    url = args.admin_url.rsplit('/', 1)[0] + '/' + DB
    with psycopg.connect(url) as db:
        with db.transaction():
            for name in ('schema.sql', 'access_schema.sql', 'pruning_schema.sql', 'visibility_schema.sql'):
                db.execute((args.oracle / name).read_text())
            db.execute('INSERT INTO notes_rag.generation (scope_signature, config_signature, updated_at) VALUES (%s, %s, %s)',
                       (manifest['scope_signature'], manifest['config_signature'], UPDATED_AT))
            with db.cursor() as cur:
                with cur.copy('COPY notes_rag.pages (page_key, notebook_id, source_sha256, relative_path, title, status, '
                              'needs_visual_review, updated_at) FROM STDIN') as copy:
                    for line in (args.fixture / 'db' / 'pages.jsonl').open(encoding='utf-8'):
                        r = json.loads(line)
                        copy.write_row((r['page_key'], r['notebook_id'], r['source_sha256'], r['relative_path'],
                                        r['title'], r['status'], r['needs_visual_review'], UPDATED_AT))
                with cur.copy('COPY notes_rag.chunks (page_key, ordinal, rag_priority, content, token_count, embedding) '
                              'FROM STDIN') as copy:
                    for line in (args.fixture / 'db' / 'chunks.jsonl').open(encoding='utf-8'):
                        r = json.loads(line)
                        copy.write_row((r['page_key'], r['ordinal'], r['rag_priority'], r['content'],
                                        r['token_count'], r['embedding']))
                with cur.copy('COPY notes_rag.missing_pages (page_key, source_sha256, first_seen) FROM STDIN') as copy:
                    for line in (args.fixture / 'db' / 'missing.jsonl').open():
                        r = json.loads(line)
                        copy.write_row((r['page_key'], r['source_sha256'], UPDATED_AT))
            db.execute('GRANT USAGE ON SCHEMA notes_rag TO notes_rag_reader')
            db.execute('GRANT SELECT ON notes_rag.schema_version, notes_rag.generation, notes_rag.pages, '
                       'notes_rag.chunks TO notes_rag_reader')
        db.execute('ANALYZE')
        counts = {t: db.execute(f'SELECT count(*) FROM notes_rag.{t}').fetchone()[0]
                  for t in ('pages', 'chunks', 'missing_pages')}
        vector = db.execute("SELECT extversion FROM pg_extension WHERE extname='vector'").fetchone()[0]
        size = db.execute('SELECT pg_database_size(current_database())').fetchone()[0]
    assert counts['pages'] == manifest['pages'] and counts['chunks'] == manifest['chunks'], counts
    print(json.dumps({'database': DB, 'counts': counts, 'pgvector': vector, 'database_bytes': size}))


if __name__ == '__main__':
    main()
