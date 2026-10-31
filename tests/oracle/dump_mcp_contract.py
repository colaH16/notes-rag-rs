#!/usr/bin/env python3
"""Capture the MCP wire contract (initialize result + tools/list) from the
pinned *Python* notes-rag MCP server, using its official SDK and mocks only.

Usage (inside the oracle venv built from the pinned requirements.lock):
    python -I tests/oracle/dump_mcp_contract.py <pinned-python-source> > compat/mcp-contract.json

No database, corpus, credentials or network service is used: the search
service is a MagicMock, exactly like the original tests/test_mcp.py.
"""
import json
import sys
from unittest.mock import MagicMock

sys.path.insert(0, sys.argv[1])

from starlette.testclient import TestClient  # noqa: E402

from mcp_server import create_app  # noqa: E402

TOKEN = 'synthetic-oracle-token-' * 3


def main():
    service = MagicMock()
    with TestClient(create_app(service, TOKEN, ['testserver']),
                    headers={'Authorization': 'Bearer ' + TOKEN}) as client:
        def rpc(method, params):
            response = client.post('/mcp', json={'jsonrpc': '2.0', 'id': 1, 'method': method, 'params': params},
                                   headers={'Accept': 'application/json, text/event-stream'})
            response.raise_for_status()
            return response.json()['result']
        initialize = rpc('initialize', {'protocolVersion': '2025-11-25', 'capabilities': {},
                                        'clientInfo': {'name': 'oracle', 'version': '1'}})
        tools = rpc('tools/list', {})['tools']
    contract = {
        'source': 'captured from the pinned Python MCP server by tests/oracle/dump_mcp_contract.py',
        'python': sys.version.split()[0],
        'initialize': initialize,
        'tools': tools,
    }
    print(json.dumps(contract, indent=2, ensure_ascii=False))


if __name__ == '__main__':
    main()
