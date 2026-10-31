#!/usr/bin/env python3
"""Derive the Python index-producer compatibility manifest from a pinned source tree.

Usage: python3 -I scripts/producer_manifest.py <python-notes-rag-checkout> <commit> [tokenizers-version]

Reads only the three interpreted files that the Python `index_plan.code_signature()`
hashes plus the tokenizer pins in `embedding_config.py`. It never imports the
producer, opens a database, reads notes, or contacts any network service.
The tokenizers version is the *Python wheel* version that the producer image
installs (requirements.lock), because it is part of the Python signature.
"""
import ast
import hashlib
import json
import sys
from pathlib import Path


def constants(path):
    """Top-level string/int assignments from embedding_config.py, without importing it."""
    values = {}
    for node in ast.parse(path.read_text(encoding='utf-8')).body:
        if isinstance(node, ast.Assign) and len(node.targets) == 1 and isinstance(node.targets[0], ast.Name):
            if isinstance(node.value, ast.Constant) and isinstance(node.value.value, (str, int)):
                values[node.targets[0].id] = node.value.value
    return values


def locked_version(lock, name):
    for line in lock.read_text(encoding='utf-8').splitlines():
        package, _, version = line.partition('==')
        if package.strip().lower() == name:
            return version.strip()
    raise SystemExit(f'{name} is not pinned in requirements.lock')


def main():
    root, commit = Path(sys.argv[1]), sys.argv[2]
    tokenizers = sys.argv[3] if len(sys.argv) > 3 else locked_version(root / 'requirements.lock', 'tokenizers')
    files = ('segmenter.py', 'chunker.py', 'embedding_config.py')
    data = [(root / name).read_bytes() for name in files]
    config = constants(root / 'embedding_config.py')
    manifest = {
        'schema': 1,
        'producer': {'repository': 'notes-rag (Python)', 'commit': commit},
        'code_signature_files': [
            {'path': name, 'sha256': hashlib.sha256(raw).hexdigest(), 'bytes': len(raw)}
            for name, raw in zip(files, data)],
        'code_signature': hashlib.sha256(b'\0'.join(data)).hexdigest(),
        'model': config['MODEL'],
        'model_alias': config['MODEL_ALIAS'],
        'dimensions': config['DIMENSIONS'],
        'max_input_tokens': config['MAX_INPUT_TOKENS'],
        'tokenizer_name': config['TOKENIZER_MODEL'],
        'tokenizer_revision': config['TOKENIZER_REVISION'],
        'tokenizer_sha256': config['TOKENIZER_SHA256'],
        'python_tokenizers_version': tokenizers,
        'query_instruction': config['QUERY_INSTRUCTION'],
        'target_tokens': 650, 'max_tokens': 800, 'overlap_tokens': 100,
    }
    print(json.dumps(manifest, indent=2, ensure_ascii=False))


if __name__ == '__main__':
    main()
