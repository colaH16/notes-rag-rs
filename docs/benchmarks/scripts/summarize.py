#!/usr/bin/env python3
"""Summarize raw run_bench.py output: no trial is dropped silently.

Per group (surface, case, concurrency, impl): pooled latency percentiles over
all successful requests, per-trial (=per-round) ranges, successful-reply
throughput, SERVER CPU ms per successful request (from /proc), PostgreSQL CPU,
memory (RSS/PSS after ready, warm idle, during load, post load, VmHWM peak),
errors/rejects, and cross-implementation reply equivalence.

Usage: summarize.py RAW.json [--out SUMMARY.json] [--clean-only]
"""
import argparse
import json
from pathlib import Path
import statistics
import sys

sys.path.insert(0, str(Path(__file__).resolve().parent))
from harness import percentile  # noqa: E402


def rng(values):
    values = [v for v in values if v is not None]
    if not values:
        return None
    return {'median': round(statistics.median(values), 3), 'min': round(min(values), 3), 'max': round(max(values), 3)}


def group_key(trial, phase):
    if trial['kind'] == 'mcp':
        return f"mcp/lat{trial['latency_ms']}/c{phase['concurrency']}"
    return f"public/{trial['case']}/c{phase['concurrency']}"


def mem(trial, stage, role, field):
    value = trial.get(stage, {}).get(role, {}).get(field)
    return value / 1024 if value is not None else None


def summarize(raw, clean_only):
    groups = {}
    for trial in raw['trials']:
        if clean_only and trial.get('contaminated'):
            continue
        for phase in trial.get('phases', []):
            key = (group_key(trial, phase), trial['impl'])
            groups.setdefault(key, []).append((trial, phase))
    out = {}
    for (key, impl), items in sorted(groups.items()):
        latencies, per_trial = [], []
        errors, no_match, cache_mismatch, total = {}, 0, 0, 0
        roles = ['mcp'] + (['web'] if key.startswith('public') else [])
        for trial, phase in items:
            records = phase['records']
            ok = [r for r in records if r.get('ok')]
            total += len(records)
            for r in records:
                if not r.get('ok'):
                    errors[r.get('error')] = errors.get(r.get('error'), 0) + 1
                elif r.get('results') == 0:
                    no_match += 1
                if key.startswith('public/cached') and r.get('ok') and r.get('cached') is not True:
                    cache_mismatch += 1
                if key.startswith('public/uncached') and r.get('ok') and r.get('cached') is not False:
                    cache_mismatch += 1
            values = sorted(r['ms'] for r in ok)
            latencies.extend(values)
            acc = phase['accounting']
            row = {'round': trial['round'], 'order': trial['order'], 'n_ok': len(ok), 'n': len(records),
                   'p50_ms': percentile(values, 0.5), 'p95_ms': percentile(values, 0.95),
                   'mean_ms': statistics.fmean(values) if values else None,
                   'throughput_ok_per_s': len(ok) / acc['wall_s'] if acc['wall_s'] else None,
                   'pg_cpu_ms_per_ok': 1000 * acc['pg_cpu_s'] / len(ok) if ok else None,
                   'client_cpu_ms_per_ok': 1000 * acc['client_cpu_s'] / len(ok) if ok else None,
                   'contaminated': trial.get('contaminated', False)}
            for role in roles:
                row[f'{role}_cpu_ms_per_ok'] = 1000 * acc[f'{role}_cpu_s'] / len(ok) if ok else None
                row[f'{role}_cpu_util'] = acc[f'{role}_cpu_s'] / acc['wall_s']
                during = phase['memory_during'].get(role, {})
                row[f'{role}_rss_MiB_ready'] = mem(trial, 'memory_after_ready', role, 'VmRSS_kB')
                row[f'{role}_pss_MiB_ready'] = mem(trial, 'memory_after_ready', role, 'smaps_Pss_kB')
                row[f'{role}_rss_MiB_warm_idle'] = mem(trial, 'memory_warm_idle', role, 'VmRSS_kB')
                row[f'{role}_pss_MiB_warm_idle'] = mem(trial, 'memory_warm_idle', role, 'smaps_Pss_kB')
                row[f'{role}_rss_MiB_load_median'] = during.get('rss_kB_median', 0) / 1024 if during else None
                row[f'{role}_rss_MiB_load_max'] = during.get('rss_kB_max', 0) / 1024 if during else None
                row[f'{role}_pss_MiB_load_max'] = (during.get('pss_kB_max') or 0) / 1024 if during else None
                row[f'{role}_rss_MiB_post_idle'] = mem(trial, 'memory_post_load_idle', role, 'VmRSS_kB')
                row[f'{role}_pss_MiB_post_idle'] = mem(trial, 'memory_post_load_idle', role, 'smaps_Pss_kB')
                row[f'{role}_hwm_MiB'] = mem(trial, 'memory_post_load_idle', role, 'VmHWM_kB')
                row[f'{role}_threads_max'] = during.get('threads_max') if during else None
            row['host_cpu_util_others'] = {c: v for c, v in acc['host_cpu_util'].items() if c in ('cpu0', 'cpu1')}
            per_trial.append(row)
        latencies.sort()
        summary = {'trials': len(items), 'n_total': total, 'n_ok': len(latencies), 'errors': errors,
                   'ok_but_no_matches': no_match, 'cache_flag_mismatch': cache_mismatch,
                   'pooled_ms': {'p50': percentile(latencies, 0.5), 'p95': percentile(latencies, 0.95),
                                 'p99': percentile(latencies, 0.99) if len(latencies) >= 1000 else None,
                                 'mean': statistics.fmean(latencies) if latencies else None,
                                 'min': latencies[0] if latencies else None, 'max': latencies[-1] if latencies else None},
                   'per_round': {}, 'trials_detail': per_trial,
                   'contaminated_trials': sum(1 for r in per_trial if r['contaminated'])}
        for field in per_trial[0]:
            if field not in ('round', 'order', 'contaminated', 'host_cpu_util_others', 'n', 'n_ok'):
                summary['per_round'][field] = rng([r[field] for r in per_trial])
        out.setdefault(key, {})[impl] = summary
    return out


def compare(groups):
    """Absolute deltas (rs - py) and ratios (rs / py) of headline metrics."""
    rows = {}
    for key, impls in groups.items():
        if 'py' not in impls or 'rs' not in impls:
            continue
        py, rs = impls['py'], impls['rs']
        entry = {}
        for metric in ('p50', 'p95', 'p99', 'mean'):
            a, b = py['pooled_ms'][metric], rs['pooled_ms'][metric]
            if a is not None and b is not None:
                entry[f'latency_{metric}_ms'] = {'py': round(a, 3), 'rs': round(b, 3), 'delta': round(b - a, 3),
                                                 'ratio': round(b / a, 4) if a else None}
        for field in py['per_round']:
            a, b = py['per_round'][field], rs['per_round'].get(field)
            if a and b and a['median'] is not None and b['median'] is not None:
                entry[field] = {'py': a['median'], 'rs': b['median'], 'delta': round(b['median'] - a['median'], 3),
                                'ratio': round(b['median'] / a['median'], 4) if a['median'] else None,
                                'py_range': [a['min'], a['max']], 'rs_range': [b['min'], b['max']]}
        rows[key] = entry
    return rows


def equivalence(raw):
    """Per identical request (round, case, phase, index): py vs rs reply digests."""
    replies = {}
    for trial in raw['trials']:
        for p, phase in enumerate(trial.get('phases', [])):
            case = trial['latency_ms'] if trial['kind'] == 'mcp' else trial['case']
            for r in phase['records']:
                key = (trial['kind'], trial['round'], str(case), p, r['i'])
                replies.setdefault(key, {})[trial['impl']] = r
    stats = {}
    for (kind, *_), pair in replies.items():
        s = stats.setdefault(kind, {'pairs': 0, 'structured_equal': 0, 'structured_different': 0,
                                    'content_text_parsed_equal': 0, 'content_text_raw_equal': 0,
                                    'one_side_failed': 0, 'unpaired': 0})
        if set(pair) != {'py', 'rs'}:
            s['unpaired'] += 1
            continue
        a, b = pair['py'], pair['rs']
        s['pairs'] += 1
        if not (a.get('ok') and b.get('ok')):
            s['one_side_failed'] += 1
            continue
        if a.get('structured') == b.get('structured'):
            s['structured_equal'] += 1
        else:
            s['structured_different'] += 1
        if kind == 'mcp':
            s['content_text_parsed_equal'] += a.get('content_text_parsed') == b.get('content_text_parsed')
            s['content_text_raw_equal'] += a.get('content_text_raw') == b.get('content_text_raw')
    return stats


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('raw', type=Path)
    parser.add_argument('--out', type=Path)
    parser.add_argument('--clean-only', action='store_true')
    args = parser.parse_args()
    raw = json.loads(args.raw.read_text())
    groups = summarize(raw, args.clean_only)
    result = {'source': args.raw.name, 'clean_only': args.clean_only,
              'trials': len(raw['trials']), 'contaminated_trials': sum(1 for t in raw['trials'] if t.get('contaminated')),
              'trial_max_duration_s': max((t['duration_s'] for t in raw['trials']), default=None),
              'exit_codes': sorted({json.dumps(t.get('exit_code')) for t in raw['trials']}),
              'mock_rejected': sum(t.get('mock_stats', {}).get('rejected', 0) for t in raw['trials']),
              'mock_computed_unexpected': sum(t.get('mock_stats', {}).get('computed', 0) for t in raw['trials']),
              'groups': groups, 'comparison': compare(groups), 'equivalence': equivalence(raw)}
    text = json.dumps(result, ensure_ascii=False, indent=1)
    if args.out:
        args.out.write_text(text)
    for key, entry in result['comparison'].items():
        print(key)
        for metric in ('latency_p50_ms', 'latency_p95_ms', 'latency_p99_ms', 'throughput_ok_per_s', 'mcp_cpu_ms_per_ok',
                       'web_cpu_ms_per_ok', 'pg_cpu_ms_per_ok', 'mcp_rss_MiB_warm_idle', 'mcp_pss_MiB_warm_idle',
                       'mcp_rss_MiB_load_max', 'mcp_hwm_MiB', 'web_rss_MiB_warm_idle', 'web_pss_MiB_warm_idle',
                       'web_hwm_MiB'):
            if metric in entry:
                e = entry[metric]
                print(f"  {metric:24s} py={e['py']:.2f} rs={e['rs']:.2f} delta={e['delta']:+.2f} ratio={e['ratio']}"
                      + (f"  py[{e['py_range'][0]:.2f},{e['py_range'][1]:.2f}] rs[{e['rs_range'][0]:.2f},{e['rs_range'][1]:.2f}]"
                         if 'py_range' in e else ''))
    print(json.dumps({k: result[k] for k in ('trials', 'contaminated_trials', 'trial_max_duration_s', 'exit_codes',
                                             'mock_rejected', 'mock_computed_unexpected', 'equivalence')}))


if __name__ == '__main__':
    main()
