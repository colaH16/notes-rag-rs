#!/usr/bin/env python3
"""Run the UNMODIFIED pinned Python oracle's MCP or web server for local benchmarking.

The oracle's main() functions hard-code cluster-only values (0.0.0.0:8000 for
MCP; port 8080, the in-cluster MCP URL and /selection,/policy mounts for web).
This launcher repeats each main() statement-for-statement, calling the same
factories with the same arguments and uvicorn options, and changes ONLY:
  - bind host/port      (BENCH_HOST, BENCH_PORT; loopback)
  - web: MCP gateway URL (BENCH_MCP_URL) and the selection/policy file paths
    (BENCH_SELECTION, BENCH_POLICY) that main() hard-codes as container mounts.
No oracle source file is edited. Usage: py_launch.py ORACLE_DIR mcp|web
"""
import os
import sys


def run_mcp():
    # == mcp_server.main()
    import logging
    import uvicorn
    from mcp_server import create_app
    from search import from_environment
    logging.disable(logging.CRITICAL)
    owner_token = os.environ.get('RAG_OWNER_MCP_TOKEN')
    app = create_app(from_environment(), os.environ['RAG_MCP_TOKEN'],
                     os.environ.get('RAG_MCP_HOSTS', '127.0.0.1:*,localhost:*').split(','),
                     owner_service=from_environment(owner=True) if owner_token else None,
                     owner_token=owner_token)
    print('Read-only notes MCP ready; HTTP bearer authentication required.', flush=True)
    uvicorn.run(app, host=os.environ['BENCH_HOST'], port=int(os.environ['BENCH_PORT']),
                access_log=False, log_level='critical')


def run_web():
    # == public_search.main()
    import uvicorn
    from public_search import MCPGateway, State, create_app
    from native_notes import require_folder_acl
    require_folder_acl()
    import logging
    logging.disable(logging.CRITICAL)
    state = State(os.environ.get('SEARCH_STATE_DB', '/state/search.db'),
                  int(os.environ.get('SEARCH_DAILY_LIMIT', '300')), int(os.environ.get('SEARCH_HOURLY_LIMIT', '60')))
    gateway = MCPGateway(os.environ['BENCH_MCP_URL'], os.environ['RAG_MCP_TOKEN'])
    from web_auth import OwnerAuth
    auth = OwnerAuth.from_environment(state)
    owner_gateway = MCPGateway(os.environ['BENCH_MCP_URL'], os.environ['RAG_OWNER_MCP_TOKEN'])
    from document_reader import DocumentReader
    reader = DocumentReader(os.environ['RAG_CORPUS_ROOT'], os.environ['BENCH_SELECTION'],
                            os.environ['BENCH_POLICY'], os.environ['RAG_PUBLIC_ACCESS_REVISION'])
    uvicorn.run(create_app(state, gateway, os.environ['RAG_PUBLIC_ACCESS_REVISION'], auth=auth,
                           owner_gateway=owner_gateway, reader=reader),
                host=os.environ['BENCH_HOST'], port=int(os.environ['BENCH_PORT']), workers=1,
                access_log=False, log_level='critical', proxy_headers=False,
                limit_concurrency=32, timeout_keep_alive=5)


if __name__ == '__main__':
    oracle, role = sys.argv[1], sys.argv[2]
    sys.path.insert(0, oracle)
    os.chdir(oracle)
    {'mcp': run_mcp, 'web': run_web}[role]()
