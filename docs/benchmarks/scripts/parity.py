#!/usr/bin/env python3
"""Output parity BEFORE timing: identical requests to Python oracle and Rust port.

Compares MCP initialize/tools/list/tools/call (search_notes, list_note_folders,
get_note_chunk incl. error and ACL edges, HTTP-level security responses) and the
public gateway (/api/search uncached/cached/errors, /api/folders, static and
health routes, security headers). Payloads are synthetic; the placeholder
citation origin is redacted in the report.

Usage: parity.py --rust-binary PATH --out FILE   (or --impls py,py for self-check)
"""
import argparse
import asyncio
import json
import re
from pathlib import Path
import signal
import sys
import time
from urllib.parse import quote

sys.path.insert(0, str(Path(__file__).resolve().parent))
import bench_env as env  # noqa: E402
import harness as h  # noqa: E402

VOLATILE_HEADERS = {'date', 'server', 'content-length', 'connection', 'keep-alive', 'transfer-encoding',
                    'mcp-session-id', 'vary', 'etag', 'last-modified', 'accept-ranges'}


def redact(value):
    if isinstance(value, str):
        return value.replace(*env.REDACT)
    if isinstance(value, list):
        return [redact(v) for v in value]
    if isinstance(value, dict):
        return {k: redact(v) for k, v in value.items()}
    return value


def decode(body):
    try:
        return json.loads(body)
    except ValueError:
        return {'_non_json_sha256': h.hashlib.sha256(body).hexdigest(), '_bytes': len(body)}


DOCUMENTED_VALIDATION_NORMALIZATIONS = []
PYDANTIC_DETAIL = re.compile(r', input_value=.*?, input_type=\w+\]|\n    For further information visit \S+')


def normalize_mcp(status, headers, body):
    data = decode(body)
    # Intentional port behavior: do not echo caller input or Python-specific
    # help URLs in tool argument errors. Keep field, constraint, error code,
    # HTTP status and isError intact; success responses are never normalized.
    result = data.get('result') if isinstance(data, dict) else None
    if isinstance(result, dict) and result.get('isError'):
        for item in result.get('content', []):
            text = item.get('text') if isinstance(item, dict) else None
            if isinstance(text, str) and text.startswith('Error executing tool ') and ' validation error for ' in text:
                cleaned = PYDANTIC_DETAIL.sub(lambda m: ']' if m.group(0).startswith(',') else '', text)
                if cleaned != text:
                    DOCUMENTED_VALIDATION_NORMALIZATIONS.append({'raw_sha256': h.hashlib.sha256(text.encode()).hexdigest(),
                        'compared_sha256': h.hashlib.sha256(cleaned.encode()).hexdigest()})
                    item['text'] = cleaned
    out = {'http': status, 'content_type': headers.get('content-type'), 'body': data}
    result = data.get('result') if isinstance(data, dict) else None
    if isinstance(result, dict) and isinstance(result.get('content'), list):
        texts = [c.get('text') for c in result['content'] if isinstance(c, dict)]
        parsed = []
        for text in texts:
            try:
                parsed.append(json.loads(text))
            except (TypeError, ValueError):
                parsed.append({'_text': text})
        out['content_text_parsed'] = parsed
        out['content_text_compared_sha256'] = h.hashlib.sha256('\0'.join(t or '' for t in texts).encode()).hexdigest()
    if isinstance(data, dict) and 'id' in data:
        data.pop('id')
    return out


def diff(a, b, path='$', limit=40, found=None):
    found = [] if found is None else found
    if len(found) >= limit:
        return found
    if type(a) is not type(b) and not (isinstance(a, (int, float)) and isinstance(b, (int, float))
                                        and not isinstance(a, bool) and not isinstance(b, bool)):
        found.append({'path': path, 'py': short(a), 'rs': short(b)})
    elif isinstance(a, dict):
        for key in sorted(set(a) | set(b)):
            if key not in a or key not in b:
                found.append({'path': f'{path}.{key}', 'py': short(a.get(key, '<missing>')),
                              'rs': short(b.get(key, '<missing>'))})
            else:
                diff(a[key], b[key], f'{path}.{key}', limit, found)
    elif isinstance(a, list):
        if len(a) != len(b):
            found.append({'path': path + '.length', 'py': len(a), 'rs': len(b)})
        for index, (x, y) in enumerate(zip(a, b)):
            diff(x, y, f'{path}[{index}]', limit, found)
    elif a != b or type(a) is not type(b):
        found.append({'path': path, 'py': short(a), 'rs': short(b)})
    return found


def short(value):
    text = json.dumps(redact(value), ensure_ascii=False)
    return text if len(text) <= 300 else text[:300] + f'...({len(text)} chars)'


async def mcp_cases(port, ctx):
    token, owner = env.secret('mcp.tok'), env.secret('owner.tok')
    pub = h.MCPClient('127.0.0.1', port, token)
    own = h.MCPClient('127.0.0.1', port, owner)
    results = {}

    async def record(name, coroutine):
        try:
            status, headers, body = await coroutine
        except (asyncio.TimeoutError, h.HTTPError, OSError) as error:
            results[name] = {'transport_error': type(error).__name__}
            return
        results[name] = normalize_mcp(status, headers, body)

    await record('initialize', pub.rpc('initialize', {'protocolVersion': h.PROTOCOL, 'capabilities': {},
                                                      'clientInfo': {'name': 'parity', 'version': '1'}}))
    await record('initialized_notification', pub.rpc('notifications/initialized', notify=True))
    await record('tools_list', pub.rpc('tools/list', {}))
    queries = ctx['queries']
    folders = ctx['folders']
    for i, q in enumerate(queries[:60]):
        await record(f'search_default_{i}', pub.call('search_notes', {'query': q}))
    variants = [{'limit': 1}, {'limit': 8}, {'limit': 10}, {'limit': 3, 'include_history': False},
                {'min_similarity': 0}, {'min_similarity': 0.2}, {'min_similarity': 0.55}, {'min_similarity': 1},
                {'min_similarity': 0.3, 'limit': 8}, {'include_history': True, 'limit': 8, 'min_similarity': 0.25}]
    for i, extra in enumerate(variants):
        for j in range(3):
            q = queries[60 + i * 3 + j]
            await record(f'search_variant_{i}_{j}', pub.call('search_notes', {'query': q, **extra}))
    for i, folder in enumerate(folders['public'][:6]):
        await record(f'search_folder_{i}', pub.call('search_notes', {'query': queries[100 + i], 'folder': folder}))
    await record('search_folder_owner_only_public_token',
                 pub.call('search_notes', {'query': queries[110], 'folder': folders['owner_only'][0]}))
    await record('search_folder_owner_only_owner_token',
                 own.call('search_notes', {'query': queries[110], 'folder': folders['owner_only'][0]}))
    await record('search_folder_unknown', pub.call('search_notes', {'query': queries[111], 'folder': 'No Such Folder'}))
    await record('search_folder_traversal', pub.call('search_notes', {'query': queries[112], 'folder': '../etc'}))
    await record('search_folder_trailing_slash', pub.call('search_notes', {'query': queries[112], 'folder': folders['public'][0] + '/'}))
    for i, q in enumerate(queries[120:130]):
        await record(f'search_owner_{i}', own.call('search_notes', {'query': q, 'limit': 8}))
    edge = {
        'empty_query': {'query': ''}, 'blank_query': {'query': '   '}, 'limit_zero': {'query': 'x', 'limit': 0},
        'limit_negative': {'query': 'x', 'limit': -1}, 'limit_float': {'query': 'x', 'limit': 2.0},
        'limit_string': {'query': 'x', 'limit': '3'}, 'min_sim_high': {'query': 'x', 'min_similarity': 1.1},
        'min_sim_negative': {'query': 'x', 'min_similarity': -0.1}, 'history_string': {'query': 'x', 'include_history': 'yes'},
        'query_4000': {'query': 'a' * 4000}, 'query_4001': {'query': 'a' * 4001},
        'query_tokens_over_2048': {'query': ' '.join(['zypper 설정'] * 1500)[:3990]},
        'unknown_argument': {'query': 'x', 'bogus': 1}, 'missing_query': {},
        'unicode_query': {'query': '🙂 한국어 질문​ 테스트'},
    }
    for name, arguments in edge.items():
        await record('edge_' + name, pub.call('search_notes', arguments))
    await record('folders_public', pub.call('list_note_folders', {}))
    await record('folders_owner', own.call('list_note_folders', {}))
    page, ordinal = ctx['chunk_ref']
    await record('chunk_valid', pub.call('get_note_chunk', {'page_key': page, 'ordinal': ordinal}))
    await record('chunk_missing_ordinal', pub.call('get_note_chunk', {'page_key': page, 'ordinal': 999}))
    await record('chunk_bad_key', pub.call('get_note_chunk', {'page_key': 'ABC', 'ordinal': 0}))
    await record('chunk_owner_only_public', pub.call('get_note_chunk', {'page_key': ctx['owner_page'], 'ordinal': 0}))
    await record('chunk_owner_only_owner', own.call('get_note_chunk', {'page_key': ctx['owner_page'], 'ordinal': 0}))
    await record('chunk_missing_page_row', pub.call('get_note_chunk', {'page_key': ctx['missing_page'], 'ordinal': 0}))
    await record('unknown_tool', pub.call('no_such_tool', {}))
    await record('unknown_method', pub.rpc('no/such/method', {}))
    # HTTP-level gate behaviour (fresh connections; bodies compared too).
    raw = h.Connection('127.0.0.1', port)
    body = json.dumps({'jsonrpc': '2.0', 'id': 1, 'method': 'tools/list', 'params': {}}).encode()
    base = {'Accept': 'application/json, text/event-stream', 'Content-Type': 'application/json'}
    http_cases = {
        'no_auth': ('POST', '/mcp', base, body),
        'wrong_auth': ('POST', '/mcp', {**base, 'Authorization': 'Bearer wrong'}, body),
        'evil_origin': ('POST', '/mcp', {**base, 'Authorization': 'Bearer ' + token, 'Origin': 'https://evil.invalid'}, body),
        'oversize_body': ('POST', '/mcp', {**base, 'Authorization': 'Bearer ' + token},
                          json.dumps({'jsonrpc': '2.0', 'id': 1, 'method': 'tools/call', 'params': {
                              'name': 'search_notes', 'arguments': {'query': 'x' * 40000}}}).encode()),
        'wrong_accept': ('POST', '/mcp', {'Accept': 'text/html', 'Content-Type': 'application/json',
                                          'Authorization': 'Bearer ' + token}, body),
        'get_mcp': ('GET', '/mcp', {'Authorization': 'Bearer ' + token, 'Accept': 'text/event-stream'}, b''),
        'healthz': ('GET', '/healthz', {}, b''),
        'unknown_path_auth': ('GET', '/nope', {'Authorization': 'Bearer ' + token}, b''),
        'invalid_json': ('POST', '/mcp', {**base, 'Authorization': 'Bearer ' + token}, b'{not json'),
    }
    for name, (method, path, headers, payload) in http_cases.items():
        raw = h.Connection('127.0.0.1', port)
        status, response_headers, response = await raw.request(method, path, headers, payload,
                                                                head_only=(name == 'get_mcp'), timeout=20)
        await raw.close()
        results['http_' + name] = {'http': status, 'www_authenticate': response_headers.get('www-authenticate'),
                                   'content_type': response_headers.get('content-type'), 'body': decode(response)}
    # Wrong Host header (DNS-rebinding guard) needs a custom Host line.
    reader, writer = await asyncio.open_connection('127.0.0.1', port)
    writer.write(b'POST /mcp HTTP/1.1\r\nHost: evil.invalid\r\nAuthorization: Bearer ' + token.encode() +
                 b'\r\nAccept: application/json, text/event-stream\r\nContent-Type: application/json\r\n'
                 b'Connection: close\r\nContent-Length: ' + str(len(body)).encode() + b'\r\n\r\n' + body)
    await writer.drain()
    response = await reader.read()
    writer.close()
    results['http_evil_host'] = {'http': int(response.split(b' ', 2)[1]), 'body': decode(response.split(b'\r\n\r\n', 1)[1])}
    await pub.conn.close()
    await own.conn.close()
    return results


async def public_cases(port, ctx):
    client = h.PublicClient('127.0.0.1', port)
    results = {}

    async def record(name, method, path, headers=None, body=b''):
        conn = h.Connection('127.0.0.1', port)
        try:
            status, response_headers, payload = await conn.request(method, path, headers or {}, body, timeout=60)
        except (asyncio.TimeoutError, h.HTTPError, OSError) as error:
            results[name] = {'transport_error': type(error).__name__}
            return
        finally:
            await conn.close()
        kept = {k: v for k, v in response_headers.items() if k not in VOLATILE_HEADERS}
        data = decode(payload) if 'json' in response_headers.get('content-type', '') else {
            '_sha256': h.hashlib.sha256(payload).hexdigest(), '_bytes': len(payload)}
        results[name] = {'http': status, 'headers': kept, 'body': data}

    json_header = {'Content-Type': 'application/json'}
    queries = ctx['queries']
    for i, q in enumerate(queries[200:225]):
        await record(f'search_{i}', 'POST', '/api/search', json_header, json.dumps({'query': q}, ensure_ascii=False).encode())
    for i, q in enumerate(queries[200:205]):
        await record(f'search_cached_{i}', 'POST', '/api/search', json_header, json.dumps({'query': q}, ensure_ascii=False).encode())
    await record('search_no_history', 'POST', '/api/search', json_header,
                 json.dumps({'query': queries[230], 'include_history': False}, ensure_ascii=False).encode())
    await record('search_whitespace_normalized', 'POST', '/api/search', json_header,
                 json.dumps({'query': '  ' + queries[231].replace(' ', '   ') + ' '}, ensure_ascii=False).encode())
    await record('search_folder_valid', 'POST', '/api/search', json_header,
                 json.dumps({'query': queries[232], 'folder': ctx['folders']['public'][1]}, ensure_ascii=False).encode())
    await record('search_folder_owner_only', 'POST', '/api/search', json_header,
                 json.dumps({'query': queries[233], 'folder': ctx['folders']['owner_only'][0]}, ensure_ascii=False).encode())
    await record('search_too_long', 'POST', '/api/search', json_header, json.dumps({'query': 'a' * 301}).encode())
    await record('search_empty', 'POST', '/api/search', json_header, json.dumps({'query': ' '}).encode())
    await record('search_extra_key', 'POST', '/api/search', json_header, json.dumps({'query': 'x', 'limit': 3}).encode())
    await record('search_wrong_type', 'POST', '/api/search', {'Content-Type': 'text/plain'}, b'{"query":"x"}')
    await record('search_cross_site', 'POST', '/api/search', {**json_header, 'Sec-Fetch-Site': 'cross-site'}, b'{"query":"x"}')
    await record('search_body_8193', 'POST', '/api/search', json_header, b'{"query":"' + b'a' * 8200 + b'"}')
    await record('search_invalid_json', 'POST', '/api/search', json_header, b'{nope')
    await record('search_get', 'GET', '/api/search')
    await record('folders', 'GET', '/api/folders')
    for path in ('/', '/app.js', '/style.css', '/graph', '/graph.js', '/graph.css', '/graph-layout.js',
                 '/cola-theme.css', '/cola-theme-init.js', '/cola-theme.js', '/reader.css', '/reader.js',
                 '/healthz', '/robots.txt', '/api/session', '/nope'):
        await record('get' + path, 'GET', path)
    await record('read_public_doc', 'GET', '/read?path=' + quote(ctx['public_doc']))
    await record('read_owner_only_doc', 'GET', '/read?path=' + quote(ctx['owner_doc']))
    await record('read_traversal', 'GET', '/read?path=../../etc/passwd')
    await client.conn.close()
    return results


def context():
    manifest = env.manifest()
    selection = json.loads((env.FIXTURE / 'selection' / 'markdown.json').read_text())
    public_ids = set(json.loads((env.FIXTURE / 'policy' / 'access.json').read_text())['publicNotebookIds'])
    pages = [json.loads(line) for line in (env.FIXTURE / 'db' / 'pages.jsonl').open(encoding='utf-8')]
    missing = {json.loads(line)['page_key'] for line in (env.FIXTURE / 'db' / 'missing.jsonl').open()}
    public_page = next(p for p in pages if p['notebook_id'] in public_ids and p['status'] == 'current'
                       and p['page_key'] not in missing and (env.FIXTURE / 'corpus' / p['relative_path']).exists())
    owner_page = next(p for p in pages if p['notebook_id'] not in public_ids and p['page_key'] not in missing)
    return {
        'queries': json.loads((env.FIXTURE / 'queries.json').read_text())[:400],
        'folders': {'public': [nb['name'] for nb in selection['notebooks'] if nb['id'] in public_ids],
                    'owner_only': [nb['name'] for nb in selection['notebooks'] if nb['id'] not in public_ids]},
        'chunk_ref': (public_page['page_key'], 0), 'owner_page': owner_page['page_key'],
        'missing_page': sorted(missing)[0], 'public_doc': public_page['relative_path'],
        'owner_doc': owner_page['relative_path'], 'config_signature': manifest['config_signature'],
    }


async def collect(impl, slot, args, ctx):
    state = env.RUN / f'parity-state-{impl}-{slot}.db'
    for suffix in ('', '-wal', '-shm', '-journal'):
        Path(str(state) + suffix).unlink(missing_ok=True)
    mcp = h.Server(f'{impl}-mcp', env.server_command(impl, 'mcp', args.rust_binary), env.mcp_env(impl, 0),
                   env.CPUS['mcp'], env.MCP_PORT[impl], env.RUN / f'parity-{impl}-{slot}-mcp.log')
    web = h.Server(f'{impl}-web', env.server_command(impl, 'web', args.rust_binary), env.web_env(impl, state),
                   env.CPUS['web'], env.WEB_PORT[impl], env.RUN / f'parity-{impl}-{slot}-web.log')
    try:
        await mcp.start()
        await web.start()
        out = {'mcp': await mcp_cases(env.MCP_PORT[impl], ctx), 'public': await public_cases(env.WEB_PORT[impl], ctx)}
    finally:
        out_codes = {'web': web.stop(), 'mcp': mcp.stop()}
    out['exit_codes'] = out_codes
    return out


async def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--rust-binary', type=Path)
    parser.add_argument('--impls', default='py,rs')
    parser.add_argument('--out', type=Path, required=True)
    args = parser.parse_args()
    first, second = args.impls.split(',')
    ctx = context()
    mock = await __import__('run_bench').start_mocks(env.FIXTURE / 'queries.json')
    try:
        a = await collect(first, 'a', args, ctx)
        b = await collect(second, 'b', args, ctx)
    finally:
        for process in mock[0].values():
            process.terminate()
    report = {'compared': [first, second], 'at': time.strftime('%Y-%m-%dT%H:%M:%S%z'), 'cases': {}, 'summary': {}}
    for surface in ('mcp', 'public'):
        same = different = documented = 0
        for name in sorted(set(a[surface]) | set(b[surface])):
            x, y = a[surface].get(name), b[surface].get(name)
            found = diff(x, y) if x is not None and y is not None else [{'path': '$', 'py': short(x), 'rs': short(y)}]
            # rmcp stateless mode has no server-initiated SSE channel.
            # MCP explicitly permits 405 here; real Python SDK legacy+auto
            # calls are verified independently. This is the ONLY permitted
            # transport difference and does not affect search timing.
            expected_sse = (surface == 'mcp' and name == 'http_get_mcp'
                and x and y and x.get('http') == 200 and y.get('http') == 405
                and x.get('content_type') == 'text/event-stream'
                and y.get('content_type') is None
                and {d['path'] for d in found} == {'$.http', '$.content_type'})
            if expected_sse:
                documented += 1
            elif found:
                different += 1
            else:
                same += 1
            report['cases'][f'{surface}/{name}'] = {'equal': not found, 'documented_sse_difference': bool(expected_sse), 'differences': found,
                                                     'http': [x and x.get('http'), y and y.get('http')]}
        report['summary'][surface] = {'equal': same, 'different': different, 'documented': documented}
    report['exit_codes'] = {first: a['exit_codes'], second: b['exit_codes']}
    report['documented_validation_normalizations'] = DOCUMENTED_VALIDATION_NORMALIZATIONS
    report['samples'] = redact({'mcp/search_default_0': a['mcp'].get('search_default_0'),
                                'public/search_0': a['public'].get('search_0')})
    args.out.write_text(json.dumps(report, ensure_ascii=False, indent=1))
    print(json.dumps(report['summary']))
    for name, case in report['cases'].items():
        if not case['equal']:
            print(name, json.dumps(case['differences'][:3], ensure_ascii=False)[:600])
    # A report must gate timing: matching errors/timeouts are not healthy parity.
    if any(v['different'] for v in report['summary'].values()):
        raise SystemExit('Output parity failed; timing is forbidden')
    for surface in ('mcp', 'public'):
        if any(not isinstance(v.get('http'), int) for dataset in (a, b) for v in dataset[surface].values()):
            raise SystemExit('Transport error during parity; timing is forbidden')
    for dataset in (a, b):
        m = dataset['mcp']['search_default_0']
        result = m.get('body', {}).get('result', {})
        assert m['http'] == 200 and not result.get('isError') and result.get('structuredContent', {}).get('results'), 'MCP healthy search missing'
        w = dataset['public']['search_0']
        assert w['http'] == 200 and w.get('body', {}).get('results'), 'Public healthy search missing'


if __name__ == '__main__':
    signal.signal(signal.SIGTERM, lambda *_: sys.exit(143))
    asyncio.run(main())
