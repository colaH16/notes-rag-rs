#!/usr/bin/env python3
"""Mock OpenAI/LiteLLM-compatible /embeddings server. No real model, no network egress.

Validates the request contract used by notes-rag (model alias, dimensions,
encoding_format, ZDR provider policy, bearer key) and answers with
deterministic synthetic vectors (scripts/synth.py). Optional fixed delay
models upstream wait. Responses for known queries are precomputed so the mock's
own CPU cost is near-constant and identical for every server under test.

GET /stats returns counters; POST /reset clears them. Loopback only.
"""
import argparse
import asyncio
import hashlib
import json
from pathlib import Path
import sys

sys.path.insert(0, str(Path(__file__).resolve().parent))
import synth  # noqa: E402

MODEL_ALIAS = 'rag-text-qwen3-embedding-8b'
INSTRUCTION = 'Given a question, retrieve relevant personal technical and study notes that answer the question.'


def query_text(question):
    return f'Instruct: {INSTRUCTION}\nQuery:{question}'


class Mock:
    def __init__(self, key, delay):
        self.key, self.delay = key, delay
        self.vectors = {}
        self.reset()

    def reset(self):
        self.stats = {'requests': 0, 'inputs': 0, 'precomputed_hits': 0, 'computed': 0,
                      'rejected': 0, 'reject_reasons': {}, 'connections': 0}
        self.input_digests = {}

    def precompute(self, questions):
        for q in questions:
            for text in {query_text(q.strip()), query_text(' '.join(q.split()))}:
                self.vectors[text] = synth.format_vector(synth.query_vector(text)).encode()

    def reject(self, reason):
        self.stats['rejected'] += 1
        self.stats['reject_reasons'][reason] = self.stats['reject_reasons'].get(reason, 0) + 1
        return 400, json.dumps({'error': {'message': 'mock rejected: ' + reason}}).encode()

    def embeddings(self, headers, body):
        if headers.get('authorization') != 'Bearer ' + self.key:
            return self.reject('auth')
        try:
            data = json.loads(body)
        except ValueError:
            return self.reject('json')
        if (data.get('model') != MODEL_ALIAS or data.get('dimensions') != synth.DIMENSIONS
                or data.get('encoding_format') != 'float'
                or data.get('provider') != {'zdr': True, 'data_collection': 'deny'}):
            return self.reject('contract')
        inputs = data.get('input')
        if not isinstance(inputs, list) or not inputs or not all(isinstance(t, str) for t in inputs):
            return self.reject('input')
        parts = []
        for index, text in enumerate(inputs):
            vector = self.vectors.get(text)
            if vector is None:
                self.stats['computed'] += 1
                vector = synth.format_vector(synth.query_vector(text)).encode()
            else:
                self.stats['precomputed_hits'] += 1
            digest = hashlib.sha256(text.encode()).hexdigest()
            self.input_digests[digest] = self.input_digests.get(digest, 0) + 1
            parts.append(b'{"object":"embedding","index":%d,"embedding":%s}' % (index, vector))
        self.stats['inputs'] += len(inputs)
        return 200, (b'{"object":"list","model":"' + MODEL_ALIAS.encode() + b'","data":[' + b','.join(parts)
                     + b'],"usage":{"prompt_tokens":0,"total_tokens":0}}')

    async def handle(self, reader, writer):
        self.stats['connections'] += 1
        try:
            while True:
                head = await reader.readuntil(b'\r\n\r\n')
                lines = head.decode('latin-1').split('\r\n')
                method, path, _ = lines[0].split(' ', 2)
                headers = {}
                for line in lines[1:]:
                    if ':' in line:
                        k, v = line.split(':', 1)
                        headers[k.strip().lower()] = v.strip()
                length = int(headers.get('content-length', '0'))
                body = await reader.readexactly(length) if length else b''
                if method == 'POST' and path.rstrip('/').endswith('/embeddings'):
                    self.stats['requests'] += 1
                    status, payload = self.embeddings(headers, body)
                    if self.delay:
                        await asyncio.sleep(self.delay)
                elif method == 'GET' and path == '/stats':
                    status, payload = 200, json.dumps({**self.stats, 'distinct_inputs': len(self.input_digests),
                        'inputs_sha256': hashlib.sha256('\n'.join(sorted(self.input_digests)).encode()).hexdigest(),
                        'delay_seconds': self.delay}).encode()
                elif method == 'POST' and path == '/reset':
                    self.reset()
                    status, payload = 200, b'{}'
                elif method == 'POST' and path == '/precompute':
                    self.precompute(json.loads(body))
                    status, payload = 200, json.dumps({'precomputed': len(self.vectors)}).encode()
                else:
                    status, payload = 404, b'{}'
                close = headers.get('connection', '').lower() == 'close'
                writer.write(b'HTTP/1.1 %d %s\r\nContent-Type: application/json\r\nContent-Length: %d\r\n%s\r\n'
                             % (status, b'OK' if status == 200 else b'ERR', len(payload),
                                b'Connection: close\r\n' if close else b'') + payload)
                await writer.drain()
                if close:
                    break
        except (asyncio.IncompleteReadError, ConnectionError, ValueError):
            pass
        finally:
            writer.close()


async def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--port', type=int, required=True)
    parser.add_argument('--delay-ms', type=float, default=0)
    parser.add_argument('--key-file', type=Path, required=True)
    parser.add_argument('--queries', type=Path, help='JSON list of questions to precompute')
    args = parser.parse_args()
    mock = Mock(args.key_file.read_text().strip(), args.delay_ms / 1000)
    if args.queries:
        mock.precompute(json.loads(args.queries.read_text()))
    server = await asyncio.start_server(mock.handle, '127.0.0.1', args.port, backlog=256)
    print(json.dumps({'mock_embed_ready': args.port, 'precomputed': len(mock.vectors)}), flush=True)
    async with server:
        await server.serve_forever()


if __name__ == '__main__':
    asyncio.run(main())
