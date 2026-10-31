"""Deterministic synthetic vectors shared by the fixture generator and the mock embedder.

No real notes, no real model. Each text maps to one of TOPICS unit "centers";
vector = normalize(center + k * noise), with noise seeded by the text itself.
Unrelated vectors are ~orthogonal in 1536-D, so cosine similarity is driven by
the shared topic and k, producing a realistic spread around the 0.35 cutoff.
"""
import hashlib
import math
import random

DIMENSIONS = 1536
TOPICS = 360
SEED = 'notes-rag-bench-v1'

_centers = {}


def _unit(rng):
    values = [rng.gauss(0.0, 1.0) for _ in range(DIMENSIONS)]
    norm = math.sqrt(sum(v * v for v in values))
    return [v / norm for v in values]


def center(topic):
    if topic not in _centers:
        _centers[topic] = _unit(random.Random(f'{SEED}:center:{topic}'))
    return _centers[topic]


def mix(topic, k, noise_seed):
    base = center(topic)
    noise = _unit(random.Random(f'{SEED}:noise:{noise_seed}'))
    values = [b + k * n for b, n in zip(base, noise)]
    norm = math.sqrt(sum(v * v for v in values))
    return [v / norm for v in values]


def text_digest(text):
    return hashlib.sha256(text.encode('utf-8')).hexdigest()


def query_vector(text):
    """Mock embedding of an arbitrary input text (deterministic)."""
    digest = text_digest(text)
    topic = int(digest[:8], 16) % TOPICS
    k = 0.6 + (int(digest[8:12], 16) / 0xFFFF) * 0.6
    return mix(topic, k, 'q:' + digest)


def format_vector(vector):
    return '[' + ','.join('%.7g' % v for v in vector) + ']'
