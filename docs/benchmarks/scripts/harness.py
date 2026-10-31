"""Benchmark plumbing shared by parity and timing runs.

- Minimal asyncio HTTP/1.1 keep-alive client (identical bytes for both servers).
- MCP Streamable HTTP (stateless, JSON response) and public /api/search calls.
- /proc accounting of SERVER processes only: CPU ticks, VmRSS/VmHWM, smaps_rollup PSS.
- Server lifecycle with CPU affinity via taskset (taskset execs: PID == server).
"""
import asyncio
import hashlib
import json
import os
from pathlib import Path
import signal
import subprocess
import time

CLK_TCK = os.sysconf('SC_CLK_TCK')
PAGE = os.sysconf('SC_PAGE_SIZE')
PROTOCOL = '2025-11-25'


# ---------------------------------------------------------------- HTTP client

class HTTPError(Exception):
    pass


class Connection:
    """One persistent HTTP/1.1 connection; reconnects only between requests."""

    def __init__(self, host, port, idle_reconnect=3.0):
        self.host, self.port, self.idle = host, port, idle_reconnect
        self.reader = self.writer = None
        self.last = 0.0
        self.reconnects = 0

    async def _open(self):
        if self.writer is not None:
            self.writer.close()
            self.reconnects += 1
        self.reader, self.writer = await asyncio.open_connection(self.host, self.port)

    async def close(self):
        if self.writer is not None:
            self.writer.close()
            try:
                await self.writer.wait_closed()
            except Exception:
                pass
            self.writer = None

    async def request(self, method, path, headers, body=b'', head_only=False, timeout=60):
        return await asyncio.wait_for(self._request(method, path, headers, body, head_only), timeout)

    async def _request(self, method, path, headers, body, head_only):
        if self.writer is None or self.reader.at_eof() or time.monotonic() - self.last > self.idle:
            await self._open()
        head = [f'{method} {path} HTTP/1.1', f'Host: {self.host}:{self.port}', f'Content-Length: {len(body)}']
        head += [f'{k}: {v}' for k, v in headers.items()]
        self.writer.write(('\r\n'.join(head) + '\r\n\r\n').encode() + body)
        await self.writer.drain()
        try:
            status_line = await self.reader.readuntil(b'\r\n')
            status = int(status_line.split(b' ', 2)[1])
            response_headers = {}
            while True:
                line = await self.reader.readuntil(b'\r\n')
                if line == b'\r\n':
                    break
                k, _, v = line.decode('latin-1').partition(':')
                response_headers[k.strip().lower()] = v.strip()
            if head_only:
                self.writer.close()
                self.writer = None
                return status, response_headers, b''
            if response_headers.get('transfer-encoding', '').lower() == 'chunked':
                parts = []
                while True:
                    size = int((await self.reader.readuntil(b'\r\n')).split(b';')[0], 16)
                    if size == 0:
                        await self.reader.readuntil(b'\r\n')
                        break
                    parts.append(await self.reader.readexactly(size))
                    await self.reader.readexactly(2)
                payload = b''.join(parts)
            elif 'content-length' in response_headers:
                payload = await self.reader.readexactly(int(response_headers['content-length']))
            elif status in (202, 204, 304):
                payload = b''
            else:
                payload = await self.reader.read()
                self.writer = None
        except (asyncio.IncompleteReadError, ConnectionError) as error:
            self.writer = None
            raise HTTPError(type(error).__name__) from None
        if response_headers.get('connection', '').lower() == 'close' and self.writer is not None:
            self.writer.close()
            self.writer = None
        self.last = time.monotonic()
        return status, response_headers, payload


def canonical(value):
    return json.dumps(value, sort_keys=True, ensure_ascii=False, separators=(',', ':'))


def digest(value):
    return hashlib.sha256(canonical(value).encode()).hexdigest()[:24]


class MCPClient:
    """Stateless Streamable HTTP MCP, JSON responses, like the official SDK's POST path."""

    def __init__(self, host, port, token):
        self.conn = Connection(host, port)
        self.headers = {'Authorization': 'Bearer ' + token, 'Accept': 'application/json, text/event-stream',
                        'Content-Type': 'application/json'}
        self.next_id = 0

    async def rpc(self, method, params=None, notify=False):
        message = {'jsonrpc': '2.0', 'method': method}
        if params is not None:
            message['params'] = params
        if not notify:
            self.next_id += 1
            message['id'] = self.next_id
        headers = dict(self.headers)
        if method != 'initialize':
            headers['MCP-Protocol-Version'] = PROTOCOL
        return await self.conn.request('POST', '/mcp', headers, json.dumps(message, ensure_ascii=False).encode())

    async def initialize(self):
        status, _, body = await self.rpc('initialize', {'protocolVersion': PROTOCOL, 'capabilities': {},
                                                       'clientInfo': {'name': 'rag-bench', 'version': '1'}})
        if status != 200:
            raise HTTPError(f'initialize HTTP {status}')
        await self.rpc('notifications/initialized', notify=True)
        return json.loads(body)

    async def call(self, name, arguments):
        return await self.rpc('tools/call', {'name': name, 'arguments': arguments})


def classify_mcp(status, body):
    """Return a record without payload text: ok flag, error class, digests, shape."""
    record = {'http': status}
    if status != 200:
        record.update(ok=False, error=f'http_{status}')
        return record
    try:
        data = json.loads(body)
    except ValueError:
        record.update(ok=False, error='invalid_json')
        return record
    if 'error' in data:
        record.update(ok=False, error='jsonrpc_' + str(data['error'].get('code')))
        return record
    result = data.get('result', {})
    structured = result.get('structuredContent')
    if result.get('isError') or not isinstance(structured, dict):
        text = ''.join(c.get('text', '') for c in result.get('content', []) if isinstance(c, dict))
        record.update(ok=False, error='tool_error:' + ('busy' if 'busy' in text else hashlib.sha256(text.encode()).hexdigest()[:12]))
        return record
    texts = [c.get('text', '') for c in result.get('content', []) if isinstance(c, dict)]
    try:
        parsed_text = [json.loads(t) for t in texts]
    except ValueError:
        parsed_text = None
    rows = structured.get('results')
    record.update(ok=True, structured=digest(structured),
                  content_text_raw=hashlib.sha256('\0'.join(texts).encode()).hexdigest()[:24],
                  content_text_parsed=digest(parsed_text) if parsed_text is not None else None,
                  result_keys=sorted(result), bytes=len(body))
    if isinstance(rows, list):
        record.update(results=len(rows), status=structured.get('retrieval_status'),
                      fallback=sum(1 for r in rows if r.get('rag_priority') == 'fallback'))
    return record


class PublicClient:
    def __init__(self, host, port):
        self.conn = Connection(host, port)

    async def search(self, query, **extra):
        body = json.dumps({'query': query, **extra}, ensure_ascii=False).encode()
        return await self.conn.request('POST', '/api/search', {'Content-Type': 'application/json'}, body)


def classify_public(status, headers, body):
    record = {'http': status}
    try:
        data = json.loads(body)
    except ValueError:
        record.update(ok=False, error='invalid_json')
        return record
    if status != 200 or 'error' in data:
        record.update(ok=False, error=f'http_{status}')
        return record
    stripped = {k: v for k, v in data.items() if k != 'cached'}
    record.update(ok=True, cached=data.get('cached'), results=len(data.get('results', [])),
                  structured=digest(stripped), bytes=len(body),
                  security_headers=digest({k: headers.get(k) for k in (
                      'cache-control', 'x-content-type-options', 'referrer-policy', 'x-robots-tag',
                      'content-security-policy', 'content-type')}))
    return record


# ---------------------------------------------------------------- /proc accounting

def proc_cpu(pid):
    """utime+stime of the whole process (all threads, incl. exited ones), seconds."""
    fields = Path(f'/proc/{pid}/stat').read_text().rsplit(')', 1)[1].split()
    return (int(fields[11]) + int(fields[12])) / CLK_TCK, (int(fields[13]) + int(fields[14])) / CLK_TCK


def proc_memory(pid):
    out = {}
    for line in Path(f'/proc/{pid}/status').read_text().splitlines():
        key, _, value = line.partition(':')
        if key in ('VmRSS', 'VmHWM', 'RssAnon', 'RssFile', 'RssShmem', 'VmSwap', 'VmSize'):
            out[key + '_kB'] = int(value.split()[0])
        elif key == 'Threads':
            out['Threads'] = int(value)
    try:
        for line in Path(f'/proc/{pid}/smaps_rollup').read_text().splitlines()[1:]:
            key, _, value = line.partition(':')
            if key in ('Rss', 'Pss', 'Pss_Anon', 'Pss_File', 'Pss_Shmem', 'Private_Dirty', 'Private_Clean',
                       'Shared_Clean', 'Swap', 'SwapPss'):
                out['smaps_' + key + '_kB'] = int(value.split()[0])
    except OSError:
        pass
    return out


def children(pid):
    found = []
    for entry in Path('/proc').iterdir():
        if entry.name.isdigit():
            try:
                if int((entry / 'stat').read_text().rsplit(')', 1)[1].split()[1]) == pid:
                    found.append(int(entry.name))
            except (OSError, IndexError, ValueError):
                pass
    return found


def postgres_account(postmaster):
    """CPU of postmaster + live backends + reaped children; PSS sum of live processes."""
    own, reaped = proc_cpu(postmaster)
    cpu, pss, rss, count = own + reaped, 0, 0, 0
    for child in children(postmaster):
        try:
            c_own, c_reaped = proc_cpu(child)
            mem = proc_memory(child)
        except OSError:
            continue
        cpu += c_own + c_reaped
        pss += mem.get('smaps_Pss_kB', 0)
        rss += mem.get('VmRSS_kB', 0)
        count += 1
    pss += proc_memory(postmaster).get('smaps_Pss_kB', 0)
    return {'cpu_s': cpu, 'pss_kB': pss, 'rss_sum_kB': rss, 'processes': count + 1}


def host_cpu():
    """Per-CPU busy/total jiffies from /proc/stat."""
    cpus = {}
    for line in Path('/proc/stat').read_text().splitlines():
        if line.startswith('cpu') and line[3].isdigit():
            name, *values = line.split()
            values = list(map(int, values))
            idle = values[3] + values[4]
            cpus[name] = (sum(values) - idle, sum(values))
    return cpus


def host_cpu_delta(before, after):
    return {name: round((after[name][0] - before[name][0]) / max(1, after[name][1] - before[name][1]), 3)
            for name in after}


class Sampler:
    """Periodic memory samples of server PIDs while load runs (client CPUs)."""

    def __init__(self, pids, interval=0.2):
        self.pids, self.interval = pids, interval
        self.samples = {name: [] for name in pids}
        self.task = None

    async def _run(self):
        while True:
            for name, pid in self.pids.items():
                try:
                    m = proc_memory(pid)
                    self.samples[name].append((m.get('VmRSS_kB'), m.get('smaps_Pss_kB'), m.get('Threads')))
                except OSError:
                    pass
            await asyncio.sleep(self.interval)

    def start(self):
        self.task = asyncio.get_running_loop().create_task(self._run())

    async def stop(self):
        self.task.cancel()
        try:
            await self.task
        except asyncio.CancelledError:
            pass
        summary = {}
        for name, rows in self.samples.items():
            if rows:
                rss = sorted(r[0] for r in rows if r[0] is not None)
                pss = sorted(r[1] for r in rows if r[1] is not None)
                summary[name] = {'samples': len(rows), 'rss_kB_max': rss[-1], 'rss_kB_median': rss[len(rss) // 2],
                                 'pss_kB_max': pss[-1] if pss else None,
                                 'pss_kB_median': pss[len(pss) // 2] if pss else None,
                                 'threads_max': max(r[2] for r in rows if r[2] is not None)}
        return summary


# ---------------------------------------------------------------- server lifecycle

class Server:
    def __init__(self, name, command, env, cpus, port, log_path):
        self.name, self.command, self.env, self.cpus, self.port = name, command, env, cpus, port
        self.log_path = log_path
        self.process = None

    async def start(self, timeout=60):
        self.log = open(self.log_path, 'wb')
        started = time.monotonic()
        self.process = subprocess.Popen(['taskset', '-c', self.cpus] + self.command, env=self.env,
                                        stdout=self.log, stderr=subprocess.STDOUT, stdin=subprocess.DEVNULL,
                                        start_new_session=True)
        conn = Connection('127.0.0.1', self.port)
        while time.monotonic() - started < timeout:
            if self.process.poll() is not None:
                raise RuntimeError(f'{self.name} exited during startup ({self.process.returncode}); see {self.log_path}')
            try:
                status, _, _ = await conn.request('GET', '/healthz', {})
                if status == 200:
                    await conn.close()
                    self.startup_seconds = time.monotonic() - started
                    exe = os.readlink(f'/proc/{self.process.pid}/exe')
                    self.exe = Path(exe).name
                    return self
            except (OSError, HTTPError):
                pass
            await asyncio.sleep(0.05)
        raise RuntimeError(f'{self.name} not ready in {timeout}s')

    @property
    def pid(self):
        return self.process.pid

    def stop(self, timeout=25):
        if self.process is None or self.process.poll() is not None:
            return self.process.returncode if self.process else None
        self.process.send_signal(signal.SIGTERM)
        try:
            code = self.process.wait(timeout)
        except subprocess.TimeoutExpired:
            self.process.kill()
            code = self.process.wait()
        self.log.close()
        return code


def percentile(sorted_values, q):
    if not sorted_values:
        return None
    position = (len(sorted_values) - 1) * q
    low = int(position)
    high = min(low + 1, len(sorted_values) - 1)
    return sorted_values[low] + (sorted_values[high] - sorted_values[low]) * (position - low)
