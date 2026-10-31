#!/usr/bin/env python3
"""Generate a deterministic synthetic notes corpus, ACL files, DB rows and queries.

Shape mirrors the documented production scale (17 notebooks / ~2220 pages /
~3180 chunks, ~7% history chunks, 13 public notebooks) without any real note
content. Signatures are computed by the pinned Python oracle's own code.

Usage (bench venv): gen_fixtures.py --oracle DIR --tokenizer FILE --out DIR
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import random
import sys

sys.path.insert(0, str(Path(__file__).resolve().parent))
import synth  # noqa: E402

SEED = 20261031
NOTEBOOKS = 17
PUBLIC = 13
PAGES = 2220
ROOT = 'OneNote/markdown'

EN = ('zypper update snapshot rollback btrfs kernel module systemd timer network bridge vlan '
      'zfs pool scrub dataset tailscale exit node wireguard nginx proxy certificate renew '
      'postgres vacuum index replica backup restore cron rsync ssh key agent firewall nftables '
      'docker podman image registry kubernetes pod volume helm chart grafana alert prometheus '
      'python venv package rust cargo build linker cache memory leak profile flamegraph latency').split()
KO = ('설정 방법 오류 해결 정리 요약 명령어 확인 업데이트 복구 백업 네트워크 디스크 성능 '
      '메모리 로그 권한 사용자 그룹 서비스 재시작 인증서 갱신 공부 노트 시험 문제 풀이 개념 '
      '정의 예제 비교 차이 원인 분석 절차 주의 사항 참고 링크 버전 호환 이전 기록').split()


def words(rng, count):
    return ' '.join(rng.choice(EN if rng.random() < 0.55 else KO) for _ in range(count))


def paragraph_text(rng, characters):
    parts, size = [], 0
    while size < characters:
        line = words(rng, rng.randint(6, 16))
        if rng.random() < 0.15:
            line = '```\n$ ' + words(rng, rng.randint(3, 7)) + '\n```'
        elif rng.random() < 0.2:
            line = '- ' + line
        parts.append(line)
        size += len(line) + 1
    return '\n'.join(parts)[:characters]


def sha(value):
    return hashlib.sha256(value.encode('utf-8')).hexdigest()


def oracle_signatures(oracle, tokenizer, allowed):
    os.environ['RAG_TOKENIZER_PATH'] = str(tokenizer)
    sys.path.insert(0, str(oracle))
    from embedding_config import QwenTokenizer
    from index_plan import config_signature, sha256
    tok = QwenTokenizer()
    generation = config_signature(tokenizer_name=tok.name, tokenizer_version=tok.version,
                                  target_tokens=650, max_tokens=800, overlap_tokens=100)
    scope = sha256(json.dumps(sorted(allowed), ensure_ascii=False).encode('utf-8'))
    return scope, generation, tok.version


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--oracle', type=Path, required=True)
    parser.add_argument('--tokenizer', type=Path, required=True)
    parser.add_argument('--out', type=Path, required=True)
    parser.add_argument('--queries', type=int, default=9000)
    args = parser.parse_args()
    rng = random.Random(SEED)
    out = args.out
    for sub in ('selection', 'policy', 'corpus', 'db'):
        (out / sub).mkdir(parents=True, exist_ok=True)

    names = []
    for index in range(NOTEBOOKS):
        style = index % 3
        names.append((f'Notebook {index + 1:02d}', f'학습 노트 {index + 1:02d}', f'Ops-{index + 1:02d} 기록')[style])
    notebooks = [{'id': 'nb-' + sha(f'notebook:{i}')[:24], 'name': name} for i, name in enumerate(names)]
    public_ids = [nb['id'] for nb in notebooks[:PUBLIC]]
    selection = {'schemaVersion': 1, 'silverBulletRoot': ROOT, 'sourceArchiveRoot': 'synthetic-archive',
                 'notebooks': notebooks}
    (out / 'selection' / 'markdown.json').write_text(json.dumps(selection, ensure_ascii=False, indent=2))
    (out / 'policy' / 'access.json').write_text(json.dumps({'schemaVersion': 1, 'publicNotebookIds': public_ids}, indent=2))

    # Zipf-like topic popularity: some topics have many pages, some one or two.
    weights = [1.0 / (rank + 1) ** 0.6 for rank in range(synth.TOPICS)]
    # Owner-only notebooks are small, like production (93 owner-only pages).
    nb_weights = [12.0] * PUBLIC + [1.0] * (NOTEBOOKS - PUBLIC)
    pages, chunks, missing = [], [], []
    for p in range(PAGES):
        nb_index = rng.choices(range(NOTEBOOKS), nb_weights)[0]
        nb = notebooks[nb_index]
        old = rng.random() < 0.05
        section = f'섹션 {rng.randint(1, 9)}' if rng.random() < 0.5 else f'Section {rng.randint(1, 9)}'
        title = f'Page {p:04d} ' + words(rng, rng.randint(1, 3))
        relative = '/'.join([nb['name']] + (['old'] if old else []) + [section, title + '.md'])
        key = sha('page:' + relative)
        topic = rng.choices(range(synth.TOPICS), weights)[0]
        visual = rng.random() < 0.03
        pages.append({'page_key': key, 'notebook_id': nb['id'], 'source_sha256': sha('src:' + relative),
                      'relative_path': relative, 'title': title, 'status': 'old' if old else 'current',
                      'needs_visual_review': visual})
        roll = rng.random()
        count = 1 if roll < 0.72 else 2 if roll < 0.92 else rng.randint(3, 5)
        body = []
        for ordinal in range(count):
            fallback = old or rng.random() < 0.03
            length = rng.randint(4500, 7200) if rng.random() < 0.02 else rng.randint(700, 2600)
            content = paragraph_text(rng, length)
            chunk_topic = topic if rng.random() < 0.85 else rng.choices(range(synth.TOPICS), weights)[0]
            k = rng.uniform(0.45, 1.7)
            vector = synth.mix(chunk_topic, k, f'chunk:{key}:{ordinal}')
            chunks.append({'page_key': key, 'ordinal': ordinal,
                           'rag_priority': 'fallback' if fallback else 'normal', 'content': content,
                           'token_count': max(1, len(content) // 3), 'embedding': synth.format_vector(vector)})
            body.append(content)
        path = out / 'corpus' / relative
        # A few pages are indexed but their source file is gone (fresh ACL must hide them),
        # and a few are recorded as missing_pages (SQL must hide them).
        if p % 701 == 5:
            continue
        if p % 487 == 9:
            missing.append({'page_key': key, 'source_sha256': sha('src:' + relative)})
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text('\n\n'.join(body) + '\n', encoding='utf-8')

    scope, generation, tokenizer_version = oracle_signatures(args.oracle, args.tokenizer, [nb['id'] for nb in notebooks])
    (out / 'db' / 'pages.jsonl').write_text(''.join(json.dumps(r, ensure_ascii=False) + '\n' for r in pages))
    (out / 'db' / 'chunks.jsonl').write_text(''.join(json.dumps(r, ensure_ascii=False) + '\n' for r in chunks))
    (out / 'db' / 'missing.jsonl').write_text(''.join(json.dumps(r) + '\n' for r in missing))

    # Queries: unique strings (fresh per request), realistic lengths and mixed language.
    qrng = random.Random(SEED + 1)
    seen, queries = set(), []
    while len(queries) < args.queries:
        q = words(qrng, qrng.randint(2, 14))
        if qrng.random() < 0.3:
            q += ' 어떻게 하나요?'
        q = f'{q} #{len(queries)}'
        if q not in seen:
            seen.add(q)
            queries.append(q)
    (out / 'queries.json').write_text(json.dumps(queries, ensure_ascii=False))

    manifest = {
        'seed': SEED, 'notebooks': NOTEBOOKS, 'public_notebooks': PUBLIC,
        'pages': len(pages), 'chunks': len(chunks),
        'normal_chunks': sum(c['rag_priority'] == 'normal' for c in chunks),
        'fallback_chunks': sum(c['rag_priority'] == 'fallback' for c in chunks),
        'old_pages': sum(p['status'] == 'old' for p in pages),
        'public_pages': sum(p['notebook_id'] in public_ids for p in pages),
        'missing_pages_rows': len(missing),
        'indexed_without_source_file': sum(1 for i in range(PAGES) if i % 701 == 5),
        'queries': len(queries), 'topics': synth.TOPICS, 'dimensions': synth.DIMENSIONS,
        'silverbullet_root': ROOT,
        'scope_signature': scope, 'config_signature': generation, 'tokenizer_version': tokenizer_version,
        'access_revision': hashlib.sha256(json.dumps(tuple(sorted(public_ids))).encode()).hexdigest(),
    }
    digests = {}
    for path in sorted(out.rglob('*')):
        if path.is_file() and path.name != 'manifest.json' and 'corpus' not in path.parts:
            digests[str(path.relative_to(out))] = hashlib.sha256(path.read_bytes()).hexdigest()
    corpus = hashlib.sha256()
    for path in sorted((out / 'corpus').rglob('*.md')):
        corpus.update(str(path.relative_to(out)).encode() + b'\0' + path.read_bytes())
    digests['corpus/*'] = corpus.hexdigest()
    manifest['sha256'] = digests
    (out / 'manifest.json').write_text(json.dumps(manifest, ensure_ascii=False, indent=2))
    print(json.dumps({k: v for k, v in manifest.items() if k != 'sha256'}, ensure_ascii=False, indent=2))


if __name__ == '__main__':
    main()
