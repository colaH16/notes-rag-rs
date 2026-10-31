#!/usr/bin/env python3
"""Rotated-order serving benchmark: Python oracle vs Rust port, same data/client/limits.

Cases
  mcp/<lat>     MCP tools/call search_notes, fresh server per trial, mock embedding
                latency 0 ms or fixed 30 ms; phases at concurrency 1 and 2 (the
                original 2-slot cap is never exceeded by the client).
  public/<kind> /api/search through the public gateway -> same-language MCP, fresh
                servers and fresh SQLite state per trial; concurrency 1 (gateway
                serialises upstream searches). kind = uncached0 | uncached30 | cached.
                <=55 requests per trial keeps every request under the unchanged
                60/min traffic and 60/h search budgets.

Every request uses a query string never used before in the run (except the
deliberate cache-hit phase), so no application/embedding cache can help.
Raw per-request records (no note text) are written to --out.

Usage: run_bench.py --rust-binary PATH --out FILE [--rounds 6] [--allow-without-marker --only py]
"""
import argparse
import asyncio
import json
import os
from pathlib import Path
import platform
import subprocess
import signal
import sys
import time

sys.path.insert(0, str(Path(__file__).resolve().parent))
import bench_env as env  # noqa: E402
import harness as h  # noqa: E402

MARKER = Path('/tmp/rag-rs-ready-for-bench')
MCP_WARM = (20, 20)          # warm-up requests at c=1 then c=2 (not measured)
MCP_PHASES = ((1, 200), (2, 300))
PUBLIC_WARM, PUBLIC_N = 5, 50
CACHED_DISTINCT, CACHED_N = 10, 45
HEAVY = ('rustc', 'cargo', 'cc1', 'cc1plus', 'ld', 'ld.lld', 'clang', 'gcc', 'make', 'go', 'node', 'chromium')
FOLDER_EVERY = 10


def foreign_heavy():
    """Build/compile processes anywhere on the host (contamination flag)."""
    seen = []
    for entry in Path('/proc').iterdir():
        if entry.name.isdigit():
            try:
                comm = (entry / 'comm').read_text().strip()
            except OSError:
                continue
            if comm in HEAVY:
                seen.append(comm)
    return sorted(set(seen))


def loadavg():
    return Path('/proc/loadavg').read_text().split()[:3]


class Queries:
    def __init__(self, values):
        self.values, self.position = values, 0

    def take(self, count):
        block = self.values[self.position:self.position + count]
        if len(block) != count:
            raise RuntimeError('query pool exhausted')
        self.position += count
        return block


def mcp_arguments(index, query, folders):
    arguments = {'query': query, 'limit': 5}
    if index % FOLDER_EVERY == FOLDER_EVERY - 1:
        arguments['folder'] = folders[(index // FOLDER_EVERY) % len(folders)]
    return arguments


async def mcp_load(port, token, queries, concurrency, folders, keep):
    clients = [h.MCPClient('127.0.0.1', port, token) for _ in range(concurrency)]
    for client in clients:
        await client.initialize()
    pending = list(enumerate(queries))
    records = []
    origin = time.perf_counter_ns()

    async def worker(number, client):
        while pending:
            index, query = pending.pop(0)
            started = time.perf_counter_ns()
            try:
                status, _, body = await client.call('search_notes', mcp_arguments(index, query, folders))
                record = h.classify_mcp(status, body)
            except (h.HTTPError, OSError) as error:
                record = {'ok': False, 'error': 'transport:' + type(error).__name__}
            ended = time.perf_counter_ns()
            if keep:
                record.update(i=index, w=number, t0_ms=round((started - origin) / 1e6, 3),
                              ms=round((ended - started) / 1e6, 3))
                records.append(record)

    started = time.perf_counter()
    await asyncio.gather(*(worker(n, c) for n, c in enumerate(clients)))
    wall = time.perf_counter() - started
    reconnects = sum(c.conn.reconnects for c in clients)
    for client in clients:
        await client.conn.close()
    return records, wall, reconnects


async def public_load(port, queries, keep):
    client = h.PublicClient('127.0.0.1', port)
    records = []
    origin = time.perf_counter_ns()
    started_all = time.perf_counter()
    for index, query in enumerate(queries):
        started = time.perf_counter_ns()
        try:
            status, headers, body = await client.search(query)
            record = h.classify_public(status, headers, body)
        except (h.HTTPError, OSError) as error:
            record = {'ok': False, 'error': 'transport:' + type(error).__name__}
        ended = time.perf_counter_ns()
        if keep:
            record.update(i=index, t0_ms=round((started - origin) / 1e6, 3), ms=round((ended - started) / 1e6, 3))
            records.append(record)
    wall = time.perf_counter() - started_all
    reconnects = client.conn.reconnects
    await client.conn.close()
    return records, wall, reconnects


def accounting(servers, postmaster, mocks):
    snapshot = {'t': time.perf_counter(), 'host': h.host_cpu(), 'client': os.times()}
    for name, server in servers.items():
        own, _ = h.proc_cpu(server.pid)
        snapshot[name] = own
    snapshot['pg'] = h.postgres_account(postmaster)['cpu_s']
    snapshot['mock'] = sum(h.proc_cpu(pid)[0] for pid in mocks)
    return snapshot


def delta(before, after, servers):
    out = {'wall_s': round(after['t'] - before['t'], 4),
           'client_cpu_s': round((after['client'].user + after['client'].system)
                                 - (before['client'].user + before['client'].system), 3),
           'pg_cpu_s': round(after['pg'] - before['pg'], 3), 'mock_cpu_s': round(after['mock'] - before['mock'], 3),
           'host_cpu_util': h.host_cpu_delta(before['host'], after['host'])}
    for name in servers:
        out[f'{name}_cpu_s'] = round(after[name] - before[name], 3)
    return out


def memory(servers):
    return {name: h.proc_memory(server.pid) for name, server in servers.items()}


class HeavyWatch:
    """Scan for compile/build processes once per second for the whole trial."""

    def __init__(self):
        self.seen, self.hits, self.task = set(), 0, None

    async def _run(self):
        while True:
            found = foreign_heavy()
            if found:
                self.hits += 1
                self.seen.update(found)
            await asyncio.sleep(1.0)

    def __enter__(self):
        self.task = asyncio.get_running_loop().create_task(self._run())
        return self

    def __exit__(self, *_):
        self.task.cancel()

    def result(self):
        return {'processes': sorted(self.seen), 'seconds_seen': self.hits}


async def run_mcp_trial(impl, latency, block, args, ctx):
    servers = {}
    trial = {'kind': 'mcp', 'impl': impl, 'latency_ms': latency, 'heavy_before': foreign_heavy(),
             'loadavg_before': loadavg()}
    started = time.monotonic()
    await ctx['mock_reset'](latency)
    mcp = h.Server(f'{impl}-mcp', env.server_command(impl, 'mcp', args.rust_binary), env.mcp_env(impl, latency),
                   env.CPUS['mcp'], env.MCP_PORT[impl], env.RUN / f'{impl}-mcp.log')
    try:
        await mcp.start()
        servers['mcp'] = mcp
        trial['server'] = {'exe': mcp.exe, 'startup_s': round(mcp.startup_seconds, 3), 'cpus': env.CPUS['mcp']}
        await asyncio.sleep(1.0)
        trial['memory_after_ready'] = memory(servers)
        token = env.secret('mcp.tok')
        warm = block[:sum(MCP_WARM)]
        await mcp_load(env.MCP_PORT[impl], token, warm[:MCP_WARM[0]], 1, ctx['folders'], False)
        await mcp_load(env.MCP_PORT[impl], token, warm[MCP_WARM[0]:], 2, ctx['folders'], False)
        await asyncio.sleep(2.0)
        trial['memory_warm_idle'] = memory(servers)
        offset = sum(MCP_WARM)
        trial['phases'] = []
        for concurrency, count in MCP_PHASES:
            queries = block[offset:offset + count]
            offset += count
            sampler = h.Sampler({'mcp': mcp.pid})
            before = accounting(servers, ctx['postmaster'], ctx['mock_pids'])
            sampler.start()
            records, wall, reconnects = await mcp_load(env.MCP_PORT[impl], token, queries, concurrency,
                                                       ctx['folders'], True)
            memory_load = await sampler.stop()
            after = accounting(servers, ctx['postmaster'], ctx['mock_pids'])
            trial['phases'].append({'concurrency': concurrency, 'n': count, 'reconnects': reconnects,
                                    'accounting': delta(before, after, servers), 'memory_during': memory_load,
                                    'records': records})
        await asyncio.sleep(2.0)
        trial['memory_post_load_idle'] = memory(servers)
        trial['mock_stats'] = await ctx['mock_stats'](latency)
        trial['pg_after'] = h.postgres_account(ctx['postmaster'])
    finally:
        trial['exit_code'] = mcp.stop()
        trial['duration_s'] = round(time.monotonic() - started, 2)
        trial['heavy_after'] = foreign_heavy()
        trial['loadavg_after'] = loadavg()
    return trial


async def run_public_trial(impl, kind, block, args, ctx, round_index):
    latency = 30 if kind == 'uncached30' else 0
    servers = {}
    trial = {'kind': 'public', 'impl': impl, 'case': kind, 'latency_ms': latency, 'heavy_before': foreign_heavy(),
             'loadavg_before': loadavg()}
    started = time.monotonic()
    state = env.RUN / f'state-{impl}-{kind}-r{round_index}.db'
    for suffix in ('', '-wal', '-shm', '-journal'):
        Path(str(state) + suffix).unlink(missing_ok=True)
    await ctx['mock_reset'](latency)
    mcp = h.Server(f'{impl}-mcp', env.server_command(impl, 'mcp', args.rust_binary), env.mcp_env(impl, latency),
                   env.CPUS['mcp'], env.MCP_PORT[impl], env.RUN / f'{impl}-mcp.log')
    web = h.Server(f'{impl}-web', env.server_command(impl, 'web', args.rust_binary), env.web_env(impl, state),
                   env.CPUS['web'], env.WEB_PORT[impl], env.RUN / f'{impl}-web.log')
    try:
        await mcp.start()
        servers['mcp'] = mcp
        await web.start()
        servers['web'] = web
        trial['server'] = {'mcp_exe': mcp.exe, 'web_exe': web.exe, 'mcp_startup_s': round(mcp.startup_seconds, 3),
                           'web_startup_s': round(web.startup_seconds, 3)}
        await asyncio.sleep(1.0)
        trial['memory_after_ready'] = memory(servers)
        if kind == 'cached':
            distinct = block[:CACHED_DISTINCT]
            await public_load(env.WEB_PORT[impl], distinct, False)
            measured = [distinct[i % CACHED_DISTINCT] for i in range(CACHED_N)]
        else:
            await public_load(env.WEB_PORT[impl], block[:PUBLIC_WARM], False)
            measured = block[PUBLIC_WARM:PUBLIC_WARM + PUBLIC_N]
        await asyncio.sleep(1.0)
        trial['memory_warm_idle'] = memory(servers)
        sampler = h.Sampler({'mcp': mcp.pid, 'web': web.pid})
        before = accounting(servers, ctx['postmaster'], ctx['mock_pids'])
        sampler.start()
        records, wall, reconnects = await public_load(env.WEB_PORT[impl], measured, True)
        memory_load = await sampler.stop()
        after = accounting(servers, ctx['postmaster'], ctx['mock_pids'])
        trial['phases'] = [{'concurrency': 1, 'n': len(measured), 'reconnects': reconnects,
                            'accounting': delta(before, after, servers), 'memory_during': memory_load,
                            'records': records}]
        await asyncio.sleep(2.0)
        trial['memory_post_load_idle'] = memory(servers)
        trial['mock_stats'] = await ctx['mock_stats'](latency)
    finally:
        trial['exit_code'] = {'web': web.stop(), 'mcp': mcp.stop()}
        trial['duration_s'] = round(time.monotonic() - started, 2)
        trial['heavy_after'] = foreign_heavy()
        trial['loadavg_after'] = loadavg()
    return trial


async def start_mocks(queries_file):
    processes, pids = {}, []
    for latency, port in env.MOCK_PORTS.items():
        process = subprocess.Popen(['taskset', '-c', env.CPUS['mock'], str(env.VENV_PY), str(env.SCRIPTS / 'mock_embed.py'),
                                    '--port', str(port), '--delay-ms', str(latency), '--key-file',
                                    str(env.RUN / 'embed_key.tok'), '--queries', str(queries_file)],
                                   stdout=subprocess.PIPE, stderr=subprocess.STDOUT, start_new_session=True)
        processes[latency] = process
    for latency, process in processes.items():
        line = await asyncio.get_running_loop().run_in_executor(None, process.stdout.readline)
        if b'mock_embed_ready' not in line:
            raise RuntimeError('mock failed: ' + line.decode(errors='replace'))
        pids.append(process.pid)
    return processes, pids


async def mock_call(latency, method, path):
    conn = h.Connection('127.0.0.1', env.MOCK_PORTS[latency])
    status, _, body = await conn.request(method, path, {})
    await conn.close()
    return json.loads(body)


def sanitize(value):
    """Public-ready raw output: no absolute host paths."""
    if isinstance(value, dict):
        return {k: sanitize(v) for k, v in value.items()}
    if isinstance(value, list):
        return [sanitize(v) for v in value]
    if isinstance(value, str) and value.startswith('/'):
        parts = Path(value).parts
        if 'notes-rag-rs' in parts:
            return '/'.join(parts[parts.index('notes-rag-rs') + 1:]) or '.'
        return '<abs>/' + Path(value).name
    return value


def sanitized_marker(text):
    try:
        return sanitize(json.loads(text))
    except ValueError:
        return {'unparsed_marker_sha256': h.hashlib.sha256(text.encode()).hexdigest()}


def toolchain(rust_binary):
    def run(command):
        try:
            return subprocess.run(command, capture_output=True, text=True, timeout=20).stdout.strip()
        except (OSError, subprocess.TimeoutExpired):
            return None
    info = {'python': run([str(env.VENV_PY), '-c', 'import sys;print(sys.version)']),
            'kernel': platform.release(), 'cpu_model': next((l.split(':', 1)[1].strip() for l in
                Path('/proc/cpuinfo').read_text().splitlines() if l.startswith('model name')), None),
            'nproc': os.cpu_count(), 'mem_total_kB': int(Path('/proc/meminfo').read_text().split()[1]),
            'postgres': run([str(env.BENCH / 'pg' / 'bin' / 'postgres'), '--version']),
            'venv_freeze_sha256': h.hashlib.sha256((env.BENCH / 'venv-freeze.txt').read_bytes()).hexdigest()}
    rust_env = {**os.environ, 'CARGO_HOME': '/tmp/bridge-cargo', 'RUSTUP_HOME': '/tmp/bridge-rustup'}
    for tool in ('rustc', 'cargo'):
        try:
            info[tool] = subprocess.run(['/tmp/bridge-cargo/bin/' + tool, '--version'], capture_output=True,
                                        text=True, timeout=20, env=rust_env,
                                        cwd='/tmp').stdout.strip() or None
        except (OSError, subprocess.TimeoutExpired):
            info[tool] = None
    if rust_binary:
        info['rust_binary_sha256'] = h.hashlib.sha256(Path(rust_binary).read_bytes()).hexdigest()
        info['rust_binary_bytes'] = Path(rust_binary).stat().st_size
    return info


async def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--rust-binary', type=Path)
    parser.add_argument('--out', type=Path, required=True)
    parser.add_argument('--rounds', type=int, default=6)
    parser.add_argument('--only', choices=('py', 'rs'))
    parser.add_argument('--skip-public', action='store_true')
    parser.add_argument('--allow-without-marker', action='store_true', help='harness validation only')
    args = parser.parse_args()
    marker = None
    if MARKER.exists():
        marker = MARKER.read_text()
    elif not args.allow_without_marker:
        raise SystemExit('Rust readiness marker missing; final measurements are not allowed yet.')
    impls = [args.only] if args.only else ['py', 'rs']
    if 'rs' in impls and not (args.rust_binary and args.rust_binary.is_file()):
        raise SystemExit('--rust-binary is required')
    os.sched_setaffinity(0, {int(c) for c in env.CPUS['client'].split(',')})

    manifest = env.manifest()
    folders = [nb['name'] for nb in json.loads((env.FIXTURE / 'selection' / 'markdown.json').read_text())['notebooks'][:13]]
    pool = Queries(json.loads((env.FIXTURE / 'queries.json').read_text()))
    pool.take(400)  # reserved for parity.py
    postmaster = int((env.BENCH / 'pgdata' / 'postmaster.pid').read_text().split()[0])
    mocks, mock_pids = await start_mocks(env.FIXTURE / 'queries.json')

    async def mock_reset(latency):
        await mock_call(latency, 'POST', '/reset')

    async def mock_stats(latency):
        return await mock_call(latency, 'GET', '/stats')

    ctx = {'folders': folders, 'postmaster': postmaster, 'mock_pids': mock_pids,
           'mock_reset': mock_reset, 'mock_stats': mock_stats}
    output = {'started': time.strftime('%Y-%m-%dT%H:%M:%S%z'), 'marker': sanitized_marker(marker) if marker else None, 'toolchain': toolchain(args.rust_binary),
              'cpus': env.CPUS, 'fixture': {k: manifest[k] for k in ('pages', 'chunks', 'normal_chunks', 'fallback_chunks',
                  'public_pages', 'missing_pages_rows', 'config_signature', 'scope_signature', 'access_revision')},
              'design': {'mcp_warm': MCP_WARM, 'mcp_phases': MCP_PHASES, 'public_warm': PUBLIC_WARM,
                         'public_n': PUBLIC_N, 'cached_distinct': CACHED_DISTINCT, 'cached_n': CACHED_N,
                         'folder_every': FOLDER_EVERY, 'rounds': args.rounds},
              'pg_affinity': sorted(os.sched_getaffinity(postmaster)), 'trials': []}
    try:
        for r in range(args.rounds):
            mcp_cases = [(impl, latency) for latency in (0, 30) for impl in impls]
            public_cases = [(impl, kind) for kind in ('uncached0', 'uncached30', 'cached') for impl in impls]
            mcp_blocks = {latency: pool.take(sum(MCP_WARM) + sum(n for _, n in MCP_PHASES)) for latency in (0, 30)}
            public_blocks = {kind: pool.take(PUBLIC_WARM + PUBLIC_N) for kind in ('uncached0', 'uncached30', 'cached')}
            shift = r % len(mcp_cases)
            mcp_cases = mcp_cases[shift:] + mcp_cases[:shift]
            shift = r % len(public_cases)
            public_cases = public_cases[shift:] + public_cases[:shift]
            if r % 2:
                mcp_cases.reverse()
                public_cases.reverse()
            plan = [('mcp', c) for c in mcp_cases] + ([] if args.skip_public else [('public', c) for c in public_cases])
            if r % 2 and not args.skip_public:
                plan = plan[len(mcp_cases):] + plan[:len(mcp_cases)]
            for order, (kind, case) in enumerate(plan):
                with HeavyWatch() as watch:
                    if kind == 'mcp':
                        trial = await run_mcp_trial(case[0], case[1], mcp_blocks[case[1]], args, ctx)
                    else:
                        trial = await run_public_trial(case[0], case[1], public_blocks[case[1]], args, ctx, r)
                trial.update(round=r, order=order, heavy_during=watch.result())
                trial['contaminated'] = bool(trial['heavy_before'] or trial['heavy_after'] or watch.hits)
                output['trials'].append(trial)
                ok = sum(rec.get('ok', False) for p in trial.get('phases', []) for rec in p['records'])
                total = sum(len(p['records']) for p in trial.get('phases', []))
                print(json.dumps({'round': r, 'order': order, 'kind': kind, 'case': case, 'ok': ok, 'total': total,
                                  'duration_s': trial['duration_s'], 'contaminated': trial['contaminated'],
                                  'heavy': trial['heavy_during']}),
                      flush=True)
                args.out.write_text(json.dumps(output, ensure_ascii=False))
    finally:
        for process in mocks.values():
            process.terminate()
        output['finished'] = time.strftime('%Y-%m-%dT%H:%M:%S%z')
        args.out.write_text(json.dumps(output, ensure_ascii=False))


if __name__ == '__main__':
    signal.signal(signal.SIGTERM, lambda *_: sys.exit(143))
    asyncio.run(main())
