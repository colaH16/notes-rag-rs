#!/usr/bin/env python3
"""End-to-end differential test: pinned Python web/MCP vs the Rust binary.

    python -I tests/oracle/e2e.py --source <pinned-python-src> --binary target/debug/notes-rag-rs \
        --pg postgresql://postgres@127.0.0.1:PORT/postgres --tokenizer qwen-tokenizer.json [--report out.json]

1. Builds a synthetic SilverBullet corpus and indexes it with the *unchanged
   Python indexer* (real tokenizer, fake deterministic embeddings) into a
   throwaway database, so the stored generation signature is exactly what a
   production Python producer writes.
2. Starts a fake embedding endpoint (records every request), the pinned
   Python MCP server and the Rust MCP role on the same DB/corpus.
3. Compares raw HTTP/JSON-RPC exchanges, tool results and the internal graph
   between Python and Rust, and drives both with the official Python MCP SDK
   client in `legacy` and `auto` modes.
4. Starts the Python web app and the Rust web role, each on its own MCP and
   its own fresh SQLite state, and compares the public HTTP API.

No real notes, credentials, or external services are used.
"""
import argparse
import asyncio
import hashlib
import json
import os
import re
import secrets
import socket
import subprocess
import sys
import tempfile
import threading
import time
import urllib.error
import urllib.request
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

PARSER = argparse.ArgumentParser()
PARSER.add_argument('--source', required=True)
PARSER.add_argument('--binary', required=True)
PARSER.add_argument('--pg', required=True)
PARSER.add_argument('--tokenizer', required=True)
PARSER.add_argument('--report')
PARSER.add_argument('--playwright', help='playwright-core directory (enables the browser check)')
PARSER.add_argument('--node', default='node')
ARGS = PARSER.parse_args()
SOURCE = str(Path(ARGS.source).resolve())
TOKEN = 'e2e-public-token-' + secrets.token_hex(16)
OWNER_TOKEN = 'e2e-owner-token-' + secrets.token_hex(16)
DIMENSIONS = 1536
RESULTS = {'checks': [], 'mismatches': [], 'documented': [], 'embedding_requests': 0}
# Intentional, documented differences (README section 9).
DOCUMENTED = {
    # Python opened a never-ending idle SSE stream for a stateless GET; rmcp answers 405 (spec-allowed).
    'mcp:get-mcp-sse',
}


def check(name, ok, detail=None):
    RESULTS['checks'].append({'name': name, 'ok': bool(ok)})
    if not ok:
        target = 'documented' if name in DOCUMENTED else 'mismatches'
        RESULTS[target].append({'name': name, 'detail': detail})
        print('DOCUMENTED' if name in DOCUMENTED else 'MISMATCH', name, json.dumps(detail, ensure_ascii=False)[:2000], flush=True)


def free_port():
    with socket.socket() as s:
        s.bind(('127.0.0.1', 0))
        return s.getsockname()[1]


def embed(text):
    """Deterministic bag-of-words vector (synthetic, no model)."""
    if text.startswith('Instruct:') and '\nQuery:' in text:
        text = text.split('\nQuery:', 1)[1]  # the instruction is identical for every query
    vector = [0.0] * DIMENSIONS
    vector[0] = 0.01
    for word in re.findall(r'\w+', text.lower()):
        digest = int(hashlib.sha256(word.encode()).hexdigest(), 16)
        vector[1 + digest % (DIMENSIONS - 1)] += 1 + (digest >> 40) % 3 / 10
    return vector


# ----- synthetic corpus ---------------------------------------------------

def write_corpus(root):
    corpus = root / 'corpus'
    selection = root / 'selection' / 'markdown.json'
    policy = root / 'policy' / 'access.json'
    for directory in (corpus, selection.parent, policy.parent):
        directory.mkdir(parents=True)
    selection.write_text(json.dumps({'schemaVersion': 1, 'sourceArchiveRoot': 'archive', 'silverBulletRoot': 'OneNote/markdown',
        'notebooks': [{'id': 'nb-tech', 'name': 'tech'}, {'id': 'nb-study', 'name': '공부'}, {'id': 'nb-private', 'name': 'private'}]}))
    policy.write_text(json.dumps({'schemaVersion': 1, 'publicNotebookIds': ['nb-study', 'nb-tech']}))
    long_section = '\n\n'.join(f'## 단계 {i}\n\n' + ' '.join(f'zypper 패키지 업데이트 절차 설명 {i} {j} refresh repository' for j in range(40))
                               for i in range(1, 6))
    notes = {
        'tech/linux/zypper.md': '# zypper update 방법\n\nzypper refresh 후 zypper update 를 실행합니다.\n\n![화면](zypper.assets/screen.png)\n',
        'tech/linux/long procedure.md': '# 긴 절차\n\n' + long_section,
        'tech/linux/--old/legacy.md': '# 예전 zypper 방법\n\nzypper update 예전 방식 history\n',
        'tech/net/tailscale (vpn).md': '# Tailscale 설정\n\ntailscale up 명령과 ACL 설정\n\n[zypper](../linux/zypper.md)\n',
        'tech/100%_O\'Reilly/book.md': '# 책 메모\n\nliteral folder name with percent and underscore zypper\n',
        '공부/한글 폴더/노트 (1).md': '# 한글 노트\n\n데이터베이스 인덱스와 zypper 비교\n\n| a | b |\n|---|---|\n| 1 | 2 |\n',
        '공부/script.md': '# XSS\n\n<script>alert(1)</script><img src="https://evil.invalid/x.png" onerror="x()">\n',
        'private/secret.md': '# 비공개 일기\n\nzypper update private diary 비밀\n',
    }
    for path, body in notes.items():
        target = corpus / path
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_text(body, encoding='utf-8')
    assets = corpus / 'tech/linux/zypper.assets'
    assets.mkdir()
    (assets / 'screen.png').write_bytes(b'\x89PNG synthetic image bytes')
    return corpus, selection, policy, len(notes)


class FakeEmbeddingClient:
    def embed(self, texts):
        return [embed(text) for text in texts]


def index(corpus, selection, pages, database_url):
    os.environ['RAG_NATIVE_NOTES'] = '1'
    sys.path.insert(0, SOURCE)
    import psycopg
    from embedding_config import QwenTokenizer
    from index_store import IndexStore
    from indexer import run_index
    with psycopg.connect(database_url, autocommit=True) as connection:
        store = IndexStore(connection)
        store.initialize()
        summary = run_index(store, FakeEmbeddingClient(), QwenTokenizer(), corpus, selection, pages, apply=True)
        generation = connection.execute('SELECT config_signature FROM notes_rag.generation').fetchone()[0]
    return summary, generation


# ----- fake embedding endpoint -----------------------------------------------

class EmbeddingHandler(BaseHTTPRequestHandler):
    requests = []

    def do_POST(self):
        body = json.loads(self.rfile.read(int(self.headers['Content-Length'])))
        EmbeddingHandler.requests.append({'path': self.path, 'body': body, 'auth': self.headers.get('Authorization')})
        data = [{'index': i, 'embedding': embed(text), 'object': 'embedding'} for i, text in enumerate(body['input'])]
        payload = json.dumps({'model': 'rag-text-qwen3-embedding-8b', 'data': data}).encode()
        self.send_response(200)
        self.send_header('Content-Type', 'application/json')
        self.send_header('Content-Length', str(len(payload)))
        self.end_headers()
        self.wfile.write(payload)

    def log_message(self, *args):
        pass


# ----- processes -------------------------------------------------------------

def wait_http(url, timeout=60):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        try:
            with urllib.request.urlopen(url, timeout=2):
                return
        except urllib.error.HTTPError:
            return
        except OSError:
            time.sleep(0.2)
    raise RuntimeError('service did not start: ' + url)


PYTHON_MCP = '''
import os, sys, uvicorn
sys.path.insert(0, os.environ["ORACLE_SOURCE"])
from search import from_environment
from mcp_server import create_app
owner = os.environ["RAG_OWNER_MCP_TOKEN"]
app = create_app(from_environment(), os.environ["RAG_MCP_TOKEN"], os.environ["RAG_MCP_HOSTS"].split(","),
                 owner_service=from_environment(owner=True), owner_token=owner)
uvicorn.run(app, host="127.0.0.1", port=int(os.environ["ORACLE_PORT"]), access_log=False, log_level="critical")
'''

PYTHON_WEB = '''
import os, sys, uvicorn
sys.path.insert(0, os.environ["ORACLE_SOURCE"])
from public_search import State, MCPGateway, create_app
from web_auth import OwnerAuth
from document_reader import DocumentReader
state = State(os.environ["SEARCH_STATE_DB"])
mcp = os.environ["ORACLE_MCP_URL"]
auth = OwnerAuth(state, "synthetic-client", "c" * 48, "s" * 48, "owner-uuid")
reader = DocumentReader(os.environ["RAG_CORPUS_ROOT"], os.environ["ORACLE_SELECTION"], os.environ["ORACLE_POLICY"],
                        os.environ["RAG_PUBLIC_ACCESS_REVISION"])
app = create_app(state, MCPGateway(mcp, os.environ["RAG_MCP_TOKEN"]), os.environ["RAG_PUBLIC_ACCESS_REVISION"],
                 auth=auth, owner_gateway=MCPGateway(mcp, os.environ["RAG_OWNER_MCP_TOKEN"]), reader=reader)
uvicorn.run(app, host="127.0.0.1", port=int(os.environ["ORACLE_PORT"]), workers=1, access_log=False,
            log_level="critical", proxy_headers=False, limit_concurrency=32, timeout_keep_alive=5)
'''


def start(cmd, env, log):
    return subprocess.Popen(cmd, env=env, stdout=log, stderr=subprocess.STDOUT)


# ----- HTTP helpers ------------------------------------------------------------

def http(method, url, headers=None, body=None):
    data = body if isinstance(body, (bytes, type(None))) else json.dumps(body).encode()
    request = urllib.request.Request(url, data=data, method=method, headers=headers or {})
    opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))
    try:
        with opener.open(request, timeout=20) as response:
            if response.headers.get('Content-Type', '').startswith('text/event-stream'):
                # An open SSE stream never ends by itself: record that it opened.
                return response.status, dict(response.headers), b'<event-stream opened>'
            return response.status, dict(response.headers), response.read()
    except urllib.error.HTTPError as error:
        return error.code, dict(error.headers), error.read()
    except OSError as error:  # timeout / reset: an open SSE stream, for example
        return 'no-complete-response', {}, type(error).__name__.encode()


def decoded(raw):
    try:
        return json.loads(raw)
    except ValueError:
        return raw.decode(errors='replace')


def strip_volatile(value):
    if isinstance(value, dict):
        return {k: strip_volatile(v) for k, v in value.items() if k not in ('generated_at',)}
    if isinstance(value, list):
        return [strip_volatile(v) for v in value]
    return value


MCP_CASES = [
    # (name, method, path, headers, body)
    ('health', 'GET', '/healthz', {}, None),
    ('health-head', 'HEAD', '/healthz', {}, None),
    ('no-auth', 'POST', '/mcp', {'Content-Type': 'application/json'}, {'jsonrpc': '2.0', 'id': 1, 'method': 'tools/list'}),
    ('lowercase-bearer', 'POST', '/mcp', {'Authorization': 'bearer ' + 'X'}, None),
    ('other-path', 'GET', '/other', 'AUTH', None),
    ('graph-public', 'GET', '/graph', 'AUTH', None),
    ('graph-owner', 'GET', '/graph', 'OWNER', None),
    ('get-mcp', 'GET', '/mcp', 'AUTH', None),
    ('get-mcp-sse', 'GET', '/mcp', {'Accept': 'text/event-stream', '_auth': 'AUTH'}, None),
    ('delete-mcp', 'DELETE', '/mcp', 'AUTH', None),
    ('bad-content-type', 'POST', '/mcp', {'Content-Type': 'text/plain', 'Accept': 'application/json, text/event-stream', '_auth': 'AUTH'}, b'{}'),
    ('bad-host', 'POST', '/mcp', {'Host': 'evil.invalid', '_auth': 'AUTH', '_json': True}, {'jsonrpc': '2.0', 'id': 1, 'method': 'tools/list'}),
    ('origin', 'POST', '/mcp', {'Origin': 'https://evil.invalid', '_auth': 'AUTH', '_json': True}, {'jsonrpc': '2.0', 'id': 1, 'method': 'tools/list'}),
    ('too-large', 'POST', '/mcp', {'_auth': 'AUTH', '_json': True}, {'jsonrpc': '2.0', 'id': 1, 'method': 'tools/call', 'params': {'name': 'search_notes', 'arguments': {'query': 'x' * 40000}}}),
    ('no-accept', 'POST', '/mcp', {'Content-Type': 'application/json', '_auth': 'AUTH'}, {'jsonrpc': '2.0', 'id': 1, 'method': 'tools/list'}),
    ('json-accept-only', 'POST', '/mcp', {'Accept': 'application/json', 'Content-Type': 'application/json', '_auth': 'AUTH'}, {'jsonrpc': '2.0', 'id': 1, 'method': 'tools/list'}),
    ('parse-error', 'POST', '/mcp', {'_auth': 'AUTH', '_json': True}, b'{not json'),
    ('batch', 'POST', '/mcp', {'_auth': 'AUTH', '_json': True}, [{'jsonrpc': '2.0', 'id': 1, 'method': 'tools/list'}]),
    ('notification', 'POST', '/mcp', {'_auth': 'AUTH', '_json': True}, {'jsonrpc': '2.0', 'method': 'notifications/initialized'}),
    ('unknown-method', 'POST', '/mcp', {'_auth': 'AUTH', '_json': True}, {'jsonrpc': '2.0', 'id': 7, 'method': 'nope/x'}),
    ('ping', 'POST', '/mcp', {'_auth': 'AUTH', '_json': True}, {'jsonrpc': '2.0', 'id': 8, 'method': 'ping'}),
    ('initialize', 'POST', '/mcp', {'_auth': 'AUTH', '_json': True}, {'jsonrpc': '2.0', 'id': 1, 'method': 'initialize', 'params': {
        'protocolVersion': '2025-11-25', 'capabilities': {}, 'clientInfo': {'name': 'e2e', 'version': '1'}}}),
    ('initialize-old', 'POST', '/mcp', {'_auth': 'AUTH', '_json': True}, {'jsonrpc': '2.0', 'id': 1, 'method': 'initialize', 'params': {
        'protocolVersion': '2025-03-26', 'capabilities': {}, 'clientInfo': {'name': 'e2e', 'version': '1'}}}),
    ('initialize-unknown', 'POST', '/mcp', {'_auth': 'AUTH', '_json': True}, {'jsonrpc': '2.0', 'id': 1, 'method': 'initialize', 'params': {
        'protocolVersion': '1999-01-01', 'capabilities': {}, 'clientInfo': {'name': 'e2e', 'version': '1'}}}),
    ('tools-list', 'POST', '/mcp', {'_auth': 'AUTH', '_json': True}, {'jsonrpc': '2.0', 'id': 2, 'method': 'tools/list', 'params': {}}),
    ('prompts-list', 'POST', '/mcp', {'_auth': 'AUTH', '_json': True}, {'jsonrpc': '2.0', 'id': 3, 'method': 'prompts/list', 'params': {}}),
    ('resources-list', 'POST', '/mcp', {'_auth': 'AUTH', '_json': True}, {'jsonrpc': '2.0', 'id': 4, 'method': 'resources/list', 'params': {}}),
]


def tool_case(name, tool, arguments, auth='AUTH'):
    return (name, 'POST', '/mcp', {'_auth': auth, '_json': True},
            {'jsonrpc': '2.0', 'id': 9, 'method': 'tools/call', 'params': {'name': tool, 'arguments': arguments}})


TOOL_CASES = [
    tool_case('search-default', 'search_notes', {'query': 'zypper update 방법'}),
    tool_case('search-owner', 'search_notes', {'query': 'zypper update 방법'}, 'OWNER'),
    tool_case('search-limit10', 'search_notes', {'query': 'zypper update', 'limit': 10, 'min_similarity': 0.0}),
    tool_case('search-no-history', 'search_notes', {'query': 'zypper update', 'include_history': False, 'min_similarity': 0}),
    tool_case('search-history-str', 'search_notes', {'query': 'zypper update', 'include_history': 'false'}),
    tool_case('search-folder', 'search_notes', {'query': 'zypper', 'folder': 'tech/linux', 'min_similarity': 0.0}),
    tool_case('search-literal-folder', 'search_notes', {'query': 'zypper', 'folder': "tech/100%_O'Reilly", 'min_similarity': 0.0}),
    tool_case('search-korean-folder', 'search_notes', {'query': '데이터베이스 인덱스', 'folder': '공부/한글 폴더'}),
    tool_case('search-missing-folder', 'search_notes', {'query': 'zypper', 'folder': 'tech/none'}),
    tool_case('search-private-folder', 'search_notes', {'query': 'zypper', 'folder': 'private'}),
    tool_case('search-bad-folder', 'search_notes', {'query': 'zypper', 'folder': '/abs'}),
    tool_case('search-limit0', 'search_notes', {'query': 'x', 'limit': 0}),
    tool_case('search-limit-str', 'search_notes', {'query': 'x', 'limit': '5'}),
    tool_case('search-limit-bool', 'search_notes', {'query': 'x', 'limit': True}),
    tool_case('search-sim-high', 'search_notes', {'query': 'x', 'min_similarity': 1.1}),
    tool_case('search-sim-str', 'search_notes', {'query': 'zypper', 'min_similarity': '0.2'}),
    tool_case('search-empty', 'search_notes', {'query': ''}),
    tool_case('search-blank', 'search_notes', {'query': '   '}),
    tool_case('search-missing-query', 'search_notes', {}),
    tool_case('search-extra-arg', 'search_notes', {'query': 'tailscale', 'owner': True}),
    tool_case('search-long-tokens', 'search_notes', {'query': '가' * 3999}),
    tool_case('folders', 'list_note_folders', {}),
    tool_case('folders-owner', 'list_note_folders', {}, 'OWNER'),
    tool_case('chunk-bad', 'get_note_chunk', {'page_key': 'arbitrary/path', 'ordinal': 0}),
    tool_case('chunk-negative', 'get_note_chunk', {'page_key': 'a' * 64, 'ordinal': -1}),
    tool_case('chunk-float', 'get_note_chunk', {'page_key': 'a' * 64, 'ordinal': 1.5}),
    tool_case('chunk-str-ordinal', 'get_note_chunk', {'page_key': 'a' * 64, 'ordinal': '0'}),
    tool_case('chunk-missing', 'get_note_chunk', {'page_key': 'a' * 64}),
    tool_case('unknown-tool', 'nope', {}),
]


def mcp_request(base, case):
    name, method, path, headers, body = case
    if headers in ('AUTH', 'OWNER'):
        headers = {'_auth': headers}
    headers = dict(headers)
    auth = headers.pop('_auth', None)
    if headers.pop('_json', False):
        headers.setdefault('Content-Type', 'application/json')
        headers.setdefault('Accept', 'application/json, text/event-stream')
    if auth:
        headers['Authorization'] = 'Bearer ' + (OWNER_TOKEN if auth == 'OWNER' else TOKEN)
    return http(method, base + path, headers, body)


PYDANTIC_DETAIL = re.compile(r', input_value=.*?, input_type=\w+\]|\n    For further information visit \S+')


def documented(value):
    """Remove the intentionally omitted parts: pydantic's echoed input values /
    help URLs, and the CPython-3.13 docstring dedent of the oracle venv (the
    production image runs 3.11, which keeps the raw docstring)."""
    import inspect
    if isinstance(value, dict):
        out = {}
        for key, item in value.items():
            if key == 'description' and isinstance(item, str) and '\n' in item:
                item = inspect.cleandoc(item)
            if key == 'text' and isinstance(item, str) and 'validation error' in item:
                item = PYDANTIC_DETAIL.sub(lambda m: ']' if m.group(0).startswith(',') else '', item)
            out[key] = documented(item)
        return out
    if isinstance(value, list):
        return [documented(v) for v in value]
    return value


def normalize_mcp(status, headers, raw):
    body = documented(decoded(raw))
    content_type = (headers.get('Content-Type') or headers.get('content-type') or '').split(';')[0]
    if isinstance(body, dict) and isinstance(body.get('error'), dict) and str(body['error'].get('message', '')).startswith('Validation error:'):
        body['error']['message'] = 'Validation error: <pydantic wording not reproduced>'
    if isinstance(body, dict) and isinstance(body.get('result'), dict):
        body['result'] = strip_volatile(body['result'])
        sc = body['result'].get('structuredContent')
        if isinstance(sc, dict):
            body['result']['content'] = [{'type': 'text', 'text': '<json>' if json.loads(body['result']['content'][0]['text']) == sc else 'MISMATCHED-TEXT'}]
    return {'status': status, 'type': content_type, 'body': body}


def compare_mcp(python_base, rust_base, chunk_cases):
    for case in MCP_CASES + TOOL_CASES + chunk_cases:
        python = normalize_mcp(*mcp_request(python_base, case))
        rust = normalize_mcp(*mcp_request(rust_base, case))
        check('mcp:' + case[0], python == rust, {'python': python, 'rust': rust})


async def sdk_client(base, mode, token):
    import httpx2
    from mcp.client import Client
    from mcp.client.streamable_http import streamable_http_client
    async with httpx2.AsyncClient(timeout=40, trust_env=False, headers={'Authorization': 'Bearer ' + token}) as http_client:
        async with Client(streamable_http_client(base + '/mcp', http_client=http_client), mode=mode, read_timeout_seconds=40) as client:
            tools = await client.list_tools()
            search = await client.call_tool('search_notes', {'query': 'zypper update 방법'})
            folders = await client.call_tool('list_note_folders', {})
            first = search.structured_content['results'][0]
            chunk = await client.call_tool('get_note_chunk', {'page_key': first['page_key'], 'ordinal': first['ordinal']})
            failure = await client.call_tool('search_notes', {'query': 'x', 'limit': 0})
            return {
                'tools': sorted(t.name for t in tools.tools),
                'search': strip_volatile(search.structured_content),
                'search_error': search.is_error,
                'folders': folders.structured_content,
                'chunk': chunk.structured_content,
                'failure_is_error': failure.is_error,
            }


def compare_sdk(python_base, rust_base):
    for mode in ('legacy', 'auto'):
        for label, token in (('public', TOKEN), ('owner', OWNER_TOKEN)):
            def run(base):
                try:
                    return asyncio.run(sdk_client(base, mode, token))
                except BaseException as error:  # noqa: BLE001 - record, do not abort the run
                    return {'error': repr(error)[:500]}
            python, rust = run(python_base), run(rust_base)
            check(f'sdk:{mode}:{label}', python == rust, {'python': python, 'rust': rust})


WEB_CASES = [
    ('index', 'GET', '/', {}, None),
    ('app-js', 'GET', '/app.js', {}, None),
    ('graph-page', 'GET', '/graph', {}, None),
    ('session', 'GET', '/api/session', {}, None),
    ('folders', 'GET', '/api/folders', {}, None),
    ('search', 'POST', '/api/search', {'Content-Type': 'application/json'}, {'query': 'zypper update 방법'}),
    ('search-cached', 'POST', '/api/search', {'Content-Type': 'application/json'}, {'query': ' zypper   update 방법 '}),
    ('search-folder', 'POST', '/api/search', {'Content-Type': 'application/json'}, {'query': 'zypper', 'folder': 'tech/linux'}),
    ('search-unknown-folder', 'POST', '/api/search', {'Content-Type': 'application/json'}, {'query': 'zypper', 'folder': 'private'}),
    ('search-invalid', 'POST', '/api/search', {'Content-Type': 'application/json'}, {'query': ''}),
    ('search-extra', 'POST', '/api/search', {'Content-Type': 'application/json'}, {'query': 'x', 'owner': True}),
    ('search-type', 'POST', '/api/search', {'Content-Type': 'text/plain'}, b'{}'),
    ('search-type-params', 'POST', '/api/search', {'Content-Type': 'application/json; charset=utf-8'}, {'query': 'tailscale'}),
    ('search-cross-site', 'POST', '/api/search', {'Content-Type': 'application/json', 'Sec-Fetch-Site': 'cross-site'}, {'query': 'x'}),
    ('search-get', 'GET', '/api/search', {}, None),
    ('search-bom', 'POST', '/api/search', {'Content-Type': 'application/json'}, b'\xef\xbb\xbf{"query": "acl"}'),
    ('graph', 'GET', '/api/graph', {}, None),
    ('graph-cross', 'GET', '/api/graph', {'Sec-Fetch-Site': 'cross-site'}, None),
    ('read', 'GET', '/read?path=tech%2Flinux%2Fzypper.md', {}, None),
    ('read-korean', 'GET', '/read?path=' + urllib.request.quote('공부/한글 폴더/노트 (1).md', safe=''), {}, None),
    ('read-xss', 'GET', '/read?path=%EA%B3%B5%EB%B6%80%2Fscript.md', {}, None),
    ('read-private', 'GET', '/read?path=private%2Fsecret.md', {}, None),
    ('read-traversal', 'GET', '/read?path=tech%2F..%2Fprivate%2Fsecret.md', {}, None),
    ('read-old', 'GET', '/read?path=tech%2Flinux%2F--old%2Flegacy.md', {}, None),
    ('read-asset', 'GET', '/read-asset?path=tech%2Flinux%2Fzypper.md&asset=tech%2Flinux%2Fzypper.assets%2Fscreen.png', {}, None),
    ('read-asset-unref', 'GET', '/read-asset?path=tech%2Flinux%2Fzypper.md&asset=tech%2Flinux%2Fother.png', {}, None),
    ('missing', 'GET', '/nope', {}, None),
    ('mcp-not-proxied', 'POST', '/mcp', {'Content-Type': 'application/json'}, {}),
    ('logout-wrong-origin', 'POST', '/auth/logout', {'Origin': 'https://evil.invalid'}, None),
    ('logout-get', 'GET', '/auth/logout', {}, None),
    ('health', 'GET', '/healthz', {}, None),
    ('robots', 'GET', '/robots.txt', {}, None),
]

SECURITY_HEADERS = ('Cache-Control', 'X-Content-Type-Options', 'Referrer-Policy', 'X-Robots-Tag', 'Content-Security-Policy')


# Approved UI divergence (README §9): the Rust pages use the shared cola16 header. The static
# page shells are pinned by compat/static-assets.sha256 and cargo tests instead; a rendered
# reader page is compared by everything the backend puts into it.
PROVIDER = 'https://apps.cola16.app'
UI_SHELLS = {'index', 'graph-page'}
READER_PARTS = (r'data-audience="[^"]*"', r'<h1 class="page-title">.*?</h1>', r'<p class="page-path">.*?</p>',
                r'<article>.*?</article>', r'<span>[^<]*</span><a href="[^"]*" target="_blank"')


def expected_rust_csp(python_csp):
    # Only the header provider's script and navigation.json are added.
    return python_csp and python_csp.replace("script-src 'self'", f"script-src 'self' {PROVIDER}") \
        .replace("connect-src 'self'", f"connect-src 'self' {PROVIDER}")


def normalize_web(name, status, headers, raw):
    lower = {k.lower(): v for k, v in headers.items()}
    text = raw.decode(errors='replace')
    if lower.get('content-type', '').startswith('application/json'):
        body = decoded(raw)
    elif name in UI_SHELLS:
        body = 'ui-shell'
    elif lower.get('content-type', '').startswith('text/html') and '<h1 class="page-title">' in text:
        body = [re.findall(part, text, re.S) for part in READER_PARTS]
    else:
        body = hashlib.sha256(raw).hexdigest()
    body = strip_volatile(body)
    return {'status': status, 'type': lower.get('content-type'), 'retry': lower.get('retry-after'),
            'disposition': lower.get('content-disposition'),
            'security': [lower.get(h.lower()) for h in SECURITY_HEADERS], 'body': body}


def compare_web(python_base, rust_base):
    for name, method, path, headers, body in WEB_CASES:
        python = normalize_web(name, *http(method, python_base + path, dict(headers), body))
        rust = normalize_web(name, *http(method, rust_base + path, dict(headers), body))
        python['security'][-1] = expected_rust_csp(python['security'][-1])
        check('web:' + name, python == rust, {'python': python, 'rust': rust})


def main():
    root = Path(tempfile.mkdtemp(prefix='rag-e2e-'))
    corpus, selection, policy, pages = write_corpus(root)
    import psycopg
    database = 'notes_rag_e2e_' + secrets.token_hex(6)
    with psycopg.connect(ARGS.pg, autocommit=True) as admin:
        admin.execute(f'CREATE DATABASE {database}')
    database_url = ARGS.pg.rsplit('/', 1)[0] + '/' + database
    processes = []
    try:
        os.environ['RAG_TOKENIZER_PATH'] = ARGS.tokenizer
        summary, generation = index(corpus, selection, pages, database_url)
        manifest = json.loads((Path(__file__).resolve().parents[2] / 'compat/producer-manifest.json').read_text())
        check('producer-generation-matches-manifest', generation == manifest['expected_config_signature'],
              {'db': generation, 'manifest': manifest['expected_config_signature']})
        RESULTS['index_summary'] = summary
        embedding = ThreadingHTTPServer(('127.0.0.1', 0), EmbeddingHandler)
        threading.Thread(target=embedding.serve_forever, daemon=True).start()
        sys.path.insert(0, SOURCE)
        from access_policy import load_public_notebooks
        revision = load_public_notebooks(policy, {'nb-tech', 'nb-study', 'nb-private'})[1]
        common = {
            'PATH': '/usr/bin:/bin', 'HOME': str(root), 'RAG_NATIVE_NOTES': '1', 'RAG_TOKENIZER_PATH': ARGS.tokenizer,
            'RAG_MCP_TOKEN': TOKEN, 'RAG_OWNER_MCP_TOKEN': OWNER_TOKEN, 'RAG_MCP_HOSTS': '127.0.0.1:*,localhost:*',
            'RAG_ALLOWLIST': str(selection), 'RAG_ACCESS_POLICY': str(policy), 'RAG_CORPUS_ROOT': str(corpus),
            'RAG_DATABASE_URL': database_url, 'RAG_LITELLM_BASE_URL': f'http://127.0.0.1:{embedding.server_port}/v1',
            'RAG_LITELLM_API_KEY': 'synthetic-key', 'RAG_ALLOW_INTERNAL_HTTP': 'true',
            'RAG_SILVERBULLET_URL': 'https://notes.cola16.app', 'ORACLE_SOURCE': SOURCE,
        }
        logs = root / 'logs'
        logs.mkdir()
        py_mcp_port, rs_mcp_port, py_web_port, rs_web_port = (free_port() for _ in range(4))
        processes.append(start([sys.executable, '-c', PYTHON_MCP], {**common, 'ORACLE_PORT': str(py_mcp_port)}, open(logs / 'py-mcp.log', 'w')))
        processes.append(start([ARGS.binary], {**common, 'RAG_ROLE': 'mcp', 'RAG_LISTEN_ADDR': f'127.0.0.1:{rs_mcp_port}'},
                               open(logs / 'rs-mcp.log', 'w')))
        python_mcp, rust_mcp = f'http://127.0.0.1:{py_mcp_port}', f'http://127.0.0.1:{rs_mcp_port}'
        wait_http(python_mcp + '/healthz')
        wait_http(rust_mcp + '/healthz')
        # Chunk references discovered through the public search on the Python side.
        status, _, raw = mcp_request(python_mcp, tool_case('probe', 'search_notes', {'query': 'zypper', 'limit': 8, 'min_similarity': 0}))
        hits = json.loads(raw)['result']['structuredContent']['results']
        chunk_cases = [tool_case(f'chunk-{i}', 'get_note_chunk', {'page_key': h['page_key'], 'ordinal': h['ordinal']}) for i, h in enumerate(hits[:3])]
        chunk_cases += [tool_case('chunk-owner-private', 'get_note_chunk', {'page_key': h['page_key'], 'ordinal': 0}, 'OWNER') for h in hits[:1]]
        before = len(EmbeddingHandler.requests)
        compare_mcp(python_mcp, rust_mcp, chunk_cases)
        compare_sdk(python_mcp, rust_mcp)
        rust_queries = [r for r in EmbeddingHandler.requests[before:] if r['body']['input'][0].startswith('Instruct:')]
        check('embedding-requests-only-question-and-zdr',
              all(r['body'].get('provider') == {'zdr': True, 'data_collection': 'deny'} and r['body']['dimensions'] == 1536
                  and r['body']['encoding_format'] == 'float' and r['body']['model'] == 'rag-text-qwen3-embedding-8b'
                  and len(r['body']['input']) == 1 and r['path'] == '/v1/embeddings' for r in rust_queries), rust_queries[:2])
        web_env = {**common, 'RAG_PUBLIC_ACCESS_REVISION': revision, 'RAG_OIDC_CLIENT_ID': 'synthetic-client',
                   'RAG_OIDC_CLIENT_SECRET': 'c' * 48, 'RAG_SESSION_KEY': 's' * 48, 'RAG_OWNER_SUBJECT': 'owner-uuid',
                   'RAG_OIDC_ISSUER': 'http://127.0.0.1:9/application/o/notes-rag/'}
        (root / 'py-state').mkdir()
        (root / 'rs-state').mkdir()
        processes.append(start([sys.executable, '-c', PYTHON_WEB], {**web_env, 'ORACLE_PORT': str(py_web_port), 'ORACLE_MCP_URL': python_mcp + '/mcp',
                               'SEARCH_STATE_DB': str(root / 'py-state/search.db'), 'ORACLE_SELECTION': str(selection),
                               'ORACLE_POLICY': str(policy)}, open(logs / 'py-web.log', 'w')))
        processes.append(start([ARGS.binary], {**web_env, 'RAG_ROLE': 'web', 'RAG_LISTEN_ADDR': f'127.0.0.1:{rs_web_port}',
                               'RAG_MCP_URL': rust_mcp + '/mcp', 'SEARCH_STATE_DB': str(root / 'rs-state/search.db'),
                               'RAG_WEB_SELECTION_PATH': str(selection), 'RAG_WEB_POLICY_PATH': str(policy)},
                               open(logs / 'rs-web.log', 'w')))
        python_web, rust_web = f'http://127.0.0.1:{py_web_port}', f'http://127.0.0.1:{rs_web_port}'
        wait_http(python_web + '/healthz')
        wait_http(rust_web + '/healthz')
        compare_web(python_web, rust_web)
        if ARGS.playwright:
            browser = {}
            for label, base in (('python', python_web), ('rust', rust_web)):
                out = root / f'browser-{label}.json'
                run = subprocess.run([ARGS.node, str(Path(__file__).with_name('browser.js')), ARGS.playwright, base, str(out)],
                                     env={**os.environ}, capture_output=True, text=True, timeout=300)
                browser[label] = json.loads(out.read_text()) if out.exists() else {'error': run.stderr[-2000:]}
            RESULTS['browser'] = browser
            rust_view = browser.get('rust', {})
            # Browsers request /favicon.ico on their own; neither implementation serves one.
            def unexpected(problems):
                return [p for p in problems if p != 'http 404: /favicon.ico'
                        and not p.startswith('error: Failed to load resource: the server responded with a status of 404')]
            check('browser:rust-no-console-errors', 'error' not in rust_view and
                  all(not unexpected(v.get('problems', [])) for v in rust_view.values()), rust_view)
            check('browser:same-as-python', browser.get('python') == browser.get('rust'), browser)
        # Mixed rollout: the Rust web role in front of the *Python* MCP (and the reverse).
        mixed_port = free_port()
        processes.append(start([ARGS.binary], {**web_env, 'RAG_ROLE': 'web', 'RAG_LISTEN_ADDR': f'127.0.0.1:{mixed_port}',
                               'RAG_MCP_URL': python_mcp + '/mcp', 'SEARCH_STATE_DB': str(root / 'rs-state/mixed.db'),
                               'RAG_WEB_SELECTION_PATH': str(selection), 'RAG_WEB_POLICY_PATH': str(policy)}, open(logs / 'rs-web-mixed.log', 'w')))
        mixed = f'http://127.0.0.1:{mixed_port}'
        wait_http(mixed + '/healthz')
        status, headers, raw = http('POST', mixed + '/api/search', {'Content-Type': 'application/json'}, {'query': 'zypper update 방법'})
        status_ref, _, raw_ref = http('POST', rust_web + '/api/search', {'Content-Type': 'application/json'}, {'query': 'zypper update 방법'})
        mixed_body = strip_volatile(json.loads(raw))
        reference = {k: v for k, v in json.loads(raw_ref).items() if k != 'cached'}
        check('mixed:rust-web-on-python-mcp', status == 200 and {k: v for k, v in mixed_body.items() if k != 'cached'} == reference,
              {'mixed': mixed_body, 'reference': reference})
        RESULTS['embedding_requests'] = len(EmbeddingHandler.requests)
        RESULTS['logs'] = {p.name: p.read_text()[-2000:] for p in logs.iterdir()}
    finally:
        for process in processes:
            process.terminate()
        for process in processes:
            try:
                process.wait(timeout=10)
            except subprocess.TimeoutExpired:
                process.kill()
        with psycopg.connect(ARGS.pg, autocommit=True) as admin:
            admin.execute(f'DROP DATABASE IF EXISTS {database} WITH (FORCE)')
    total, failed = len(RESULTS['checks']), len(RESULTS['mismatches'])
    print(json.dumps({'checks': total, 'mismatches': failed, 'documented': len(RESULTS['documented']),
                      'embedding_requests': RESULTS['embedding_requests']}))
    if ARGS.report:
        Path(ARGS.report).write_text(json.dumps(RESULTS, indent=1, ensure_ascii=False, default=str))
    return 1 if failed else 0


if __name__ == '__main__':
    raise SystemExit(main())
