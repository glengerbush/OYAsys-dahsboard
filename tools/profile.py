#!/usr/bin/env python3
"""Temporary, standard-library-only observer. Never changes OYAsys settings."""
import argparse
import concurrent.futures
import json
import os
from pathlib import Path
import time
import urllib.parse
import urllib.request


def process_sample(pid):
    if not pid:
        return {}
    root = Path('/proc') / str(pid)
    # comm may contain spaces or parentheses; split after its final closing ')'.
    stat = (root / 'stat').read_text().rsplit(')', 1)[1].split()
    out = {'cpu_ticks': int(stat[11]) + int(stat[12]), 'start_ticks': int(stat[19])}
    for line in (root / 'status').read_text().splitlines():
        key, _, value = line.partition(':')
        if key in ('VmRSS', 'VmHWM', 'Threads', 'voluntary_ctxt_switches', 'nonvoluntary_ctxt_switches'):
            out[key] = int(value.split()[0])
    # Per-thread status counters capture collector/reader activity as well as HTTP.
    switches = {'voluntary_ctxt_switches': 0, 'nonvoluntary_ctxt_switches': 0}
    try:
        for task in (root / 'task').iterdir():
            for line in (task / 'status').read_text().splitlines():
                key, _, value = line.partition(':')
                if key in switches:
                    switches[key] += int(value)
        out.update({f'all_threads_{key}': value for key, value in switches.items()})
    except (FileNotFoundError, PermissionError):
        pass
    try:
        for line in (root / 'smaps_rollup').read_text().splitlines():
            if line.startswith('Pss:'):
                out['Pss'] = int(line.split()[1])
    except PermissionError:
        out['Pss'] = None
    try:
        for line in (root / 'io').read_text().splitlines():
            key, value = line.split(':')
            out[key] = int(value)
    except PermissionError:
        pass
    return out


def host_sample():
    memory = {}
    for line in Path('/proc/meminfo').read_text().splitlines():
        key, value = line.split(':', 1)
        if key in ('MemAvailable', 'Cached', 'SReclaimable', 'Dirty', 'Writeback'):
            memory[key] = int(value.split()[0])
    return {'memory_kib': memory, 'cpu_ticks': list(map(int, Path('/proc/stat').read_text().splitlines()[0].split()[1:]))}


def get_json(url):
    started = time.monotonic()
    try:
        with urllib.request.urlopen(url, timeout=30) as response:
            result = json.load(response)
        return result, {'elapsed_s': round(time.monotonic() - started, 3), 'ok': True}
    except Exception as error:
        return None, {'elapsed_s': round(time.monotonic() - started, 3), 'ok': False, 'error': str(error)}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--pid', type=int, default=0, help='0 records host baseline only')
    parser.add_argument('--seconds', type=int, default=120)
    parser.add_argument('--url', help='OYAsys origin; required for live/history scenarios')
    parser.add_argument('--scenario', choices=['idle', 'live', 'clients', 'history'], default='idle')
    parser.add_argument('--data-dir', type=Path, help='Optional: report sizes of top-level persistent files')
    args = parser.parse_args()
    if args.seconds < 1 or args.pid < 0:
        parser.error('seconds must be positive and pid nonnegative')
    if args.scenario != 'idle' and not args.url:
        parser.error('--url is required for an HTTP scenario')
    base = args.url.rstrip('/') if args.url else None
    state, initial_request = get_json(base + '/api/v1/state') if base else (None, None)
    if base and state is None:
        parser.error(initial_request['error'])
    interval = state['settings']['sampling_seconds'] if state else 10
    cursor = (state.get('snapshot') or {}).get('sequence', 0) if state else 0
    clock_ticks = os.sysconf('SC_CLK_TCK')
    before = process_sample(args.pid)
    host_before = host_sample()
    start = time.monotonic()
    readings = [before]
    requests = []
    next_request = start
    futures = []
    with concurrent.futures.ThreadPoolExecutor(max_workers=3) as pool:
        while time.monotonic() - start < args.seconds:
            now = time.monotonic()
            if base and args.scenario != 'idle' and now >= next_request:
                request_url = base + '/api/v1/updates?' + urllib.parse.urlencode({'after': cursor, 'limit': 1000})
                if args.scenario == 'history':
                    ids = ','.join(m['id'] for m in state['inventory']['metrics'][:64])
                    end = state['now_ms']
                    request_url = base + '/api/v1/history?' + urllib.parse.urlencode({
                        'from': end - 30 * 86400000, 'to': end, 'metrics': ids, 'points': 800})
                count = 3 if args.scenario == 'clients' else 1
                for _ in range(count):
                    if len(futures) < 3:
                        futures.append((now, pool.submit(get_json, request_url)))
                # A history scenario requests one cold scan and one repeated scan.
                next_request = now + (max(args.seconds / 2, 1) if args.scenario == 'history' else interval)
            for sent, future in list(futures):
                if future.done():
                    payload, timing = future.result()
                    requests.append(timing)
                    if args.scenario != 'history' and payload:
                        cursor = max(cursor, payload.get('next_after', cursor))
                    futures.remove((sent, future))
            time.sleep(min(1, max(0, args.seconds - (time.monotonic() - start))))
            current = process_sample(args.pid)
            if args.pid and current['start_ticks'] != before['start_ticks']:
                raise RuntimeError('Process changed during measurement')
            readings.append(current)
        for _, future in futures:
            requests.append(future.result()[1])
        readings.append(process_sample(args.pid))
    duration = time.monotonic() - start
    after = readings[-1]
    result = {'scenario': args.scenario, 'duration_s': round(duration, 3),
              'pid': args.pid, 'host_before': host_before, 'host_after': host_sample(),
              'requests': requests, 'observer_pid': os.getpid(),
              'notes': ['CPU percentage is relative to one core; observer overhead is separate.',
                        'Host memory differences include unrelated workloads and filesystem cache.',
                        'HTTP scenarios approximate client requests; use a browser for rendering measurements.']}
    if args.pid:
        cpu = (after['cpu_ticks'] - before['cpu_ticks']) / clock_ticks
        result.update(cpu_seconds=cpu, cpu_one_core_percent=round(cpu / duration * 100, 4),
                      peak_rss_kib=max(x.get('VmRSS', 0) for x in readings),
                      peak_pss_kib=max((x['Pss'] for x in readings if x.get('Pss') is not None), default=None),
                      threads=max(x.get('Threads', 0) for x in readings),
                      before=before, after=after)
        result['io_delta'] = {k: after[k] - before[k] for k in ('read_bytes', 'write_bytes', 'syscr', 'syscw') if k in after and k in before}
    if args.data_dir:
        result['persistent_bytes'] = {p.name: p.stat().st_size for p in args.data_dir.iterdir() if p.is_file()}
    print(json.dumps(result, indent=2))


if __name__ == '__main__':
    main()
