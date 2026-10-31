"""Shared, identical configuration for the Python oracle and the Rust port.

Every server gets the same synthetic fixture, DB role, tokenizer file, mock
embedding endpoint, tokens and CPU set. Only role/bind variables differ by
implementation (Python launcher vs Rust ENV-only binary).
"""
import json
import os
from pathlib import Path

BENCH = Path(os.environ.get('BENCH', '/tmp/rag-bench'))
SCRIPTS = Path(__file__).resolve().parent
ORACLE = BENCH / 'oracle'
FIXTURE = BENCH / 'fixture'
RUN = BENCH / 'run'
VENV_PY = Path(os.environ.get('BENCH_VENV', '/tmp/rag-bench-venv')) / 'bin' / 'python'
TOKENIZER = BENCH / 'tok' / 'qwen-tokenizer.json'

PG_PORT = 55432
DB_NAME = 'notes_rag_bench'
MOCK_PORTS = {0: 18100, 30: 18130}          # mock embedding latency (ms) -> port
MCP_PORT = {'py': 18200, 'rs': 18300}
WEB_PORT = {'py': 18280, 'rs': 18380}

# 10 single-thread cores. 0-1: host/other agents, 2-3: PostgreSQL, 4: mock embedder,
# 5-6: MCP server under test, 7: public web gateway under test, 8-9: load client.
CPUS = {'pg': '2,3', 'mock': '4', 'mcp': '5,6', 'web': '7', 'client': '8,9'}

# Placeholder citation origin required by the original public gateway's URL allowlist;
# never contacted. Raw outputs redact it.
SILVERBULLET = 'https://notes.cola16.app'
REDACT = (SILVERBULLET, '<silverbullet-origin>')


def secret(name):
    return (RUN / name).read_text().strip()


def database_url():
    return f'postgresql://notes_rag_reader:{secret("reader.pw")}@127.0.0.1:{PG_PORT}/{DB_NAME}'


def manifest():
    return json.loads((FIXTURE / 'manifest.json').read_text())


def base_env():
    env = {k: v for k, v in os.environ.items() if k in ('PATH', 'HOME', 'LANG', 'LC_ALL', 'TZ')}
    env.update(PYTHONDONTWRITEBYTECODE='1', PYTHONUNBUFFERED='1')
    return env


def mcp_env(impl, latency):
    env = base_env()
    env.update({
        'RAG_NATIVE_NOTES': '1',
        'RAG_ALLOWLIST': str(FIXTURE / 'selection' / 'markdown.json'),
        'RAG_ACCESS_POLICY': str(FIXTURE / 'policy' / 'access.json'),
        'RAG_CORPUS_ROOT': str(FIXTURE / 'corpus'),
        'RAG_DATABASE_URL': database_url(),
        'RAG_LITELLM_BASE_URL': f'http://127.0.0.1:{MOCK_PORTS[latency]}',
        'RAG_LITELLM_API_KEY': secret('embed_key.tok'),
        'RAG_ALLOW_INTERNAL_HTTP': 'true',
        'RAG_SILVERBULLET_URL': SILVERBULLET,
        'RAG_TOKENIZER_PATH': str(TOKENIZER),
        'RAG_MCP_TOKEN': secret('mcp.tok'),
        'RAG_OWNER_MCP_TOKEN': secret('owner.tok'),
        'RAG_MCP_HOSTS': '127.0.0.1:*,localhost:*',
    })
    if impl == 'py':
        env.update(BENCH_HOST='127.0.0.1', BENCH_PORT=str(MCP_PORT['py']))
    else:
        env.update(RAG_ROLE='mcp', RAG_LISTEN_ADDR=f'127.0.0.1:{MCP_PORT["rs"]}')
    return env


def web_env(impl, state_db):
    env = base_env()
    mcp_url = f'http://127.0.0.1:{MCP_PORT[impl]}/mcp'
    env.update({
        'RAG_NATIVE_NOTES': '1',
        'SEARCH_STATE_DB': str(state_db),
        'RAG_MCP_TOKEN': secret('mcp.tok'),
        'RAG_OWNER_MCP_TOKEN': secret('owner.tok'),
        'RAG_OIDC_CLIENT_ID': 'bench-synthetic-client',
        'RAG_OIDC_CLIENT_SECRET': secret('web_secret.tok'),
        'RAG_SESSION_KEY': secret('session.tok'),
        'RAG_OWNER_SUBJECT': 'bench-synthetic-owner-subject',
        'RAG_CORPUS_ROOT': str(FIXTURE / 'corpus'),
        'RAG_PUBLIC_ACCESS_REVISION': manifest()['access_revision'],
        'RAG_TOKENIZER_PATH': str(TOKENIZER),
    })
    if impl == 'py':
        env.update(BENCH_HOST='127.0.0.1', BENCH_PORT=str(WEB_PORT['py']), BENCH_MCP_URL=mcp_url,
                   BENCH_SELECTION=str(FIXTURE / 'selection' / 'markdown.json'),
                   BENCH_POLICY=str(FIXTURE / 'policy' / 'access.json'))
    else:
        # Safety only: OIDC discovery is lazy (login only) in both implementations and the
        # benchmark never logs in; an unresolvable issuer guarantees no real endpoint contact.
        env.update(RAG_ROLE='web', RAG_LISTEN_ADDR=f'127.0.0.1:{WEB_PORT["rs"]}', RAG_MCP_URL=mcp_url,
                   RAG_OIDC_ISSUER='https://oidc.invalid/application/o/notes-rag/',
                   RAG_WEB_SELECTION_PATH=str(FIXTURE / 'selection' / 'markdown.json'),
                   RAG_WEB_POLICY_PATH=str(FIXTURE / 'policy' / 'access.json'))
    return env


def server_command(impl, role, rust_binary=None):
    if impl == 'py':
        return [str(VENV_PY), str(SCRIPTS / 'py_launch.py'), str(ORACLE), role]
    return [str(rust_binary)]
