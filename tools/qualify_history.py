#!/usr/bin/env python3
"""Read-only HTTP qualification against the synthetic 30-day fixture server."""
import argparse
from collections import Counter
from concurrent.futures import ThreadPoolExecutor
import json
import socket
import time
import urllib.error
import urllib.parse
import urllib.request


def get(url):
    with urllib.request.urlopen(url, timeout=12) as response:
        return json.load(response)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--url', required=True)
    parser.add_argument('--seconds', type=int, default=30)
    args = parser.parse_args()
    base = args.url.rstrip('/')
    state = get(base + '/api/v1/state')
    assert state['inventory']['synthetic'], 'Only run on an explicitly synthetic fixture'
    assert state['storage']['samples'] >= 250000, 'Seed the full 30-day fixture first'
    ids = [metric['id'] for metric in state['inventory']['metrics'][:64]]
    query = urllib.parse.urlencode({'from':state['now_ms']-30*86400000,'to':state['now_ms'],'metrics':','.join(ids),'points':800})
    url = base + '/api/v1/history?' + query
    start = time.monotonic()
    result = get(url)
    assert result['reduced']
    assert len(result['series']) == len(ids)
    assert all(len(series['points']) <= 800 for series in result['series'])
    gpu = next(series for series in result['series'] if series['id']=='gpu.demo.temperature')
    assert max(p[1] for p in gpu['points'] if p[1] is not None) == 78
    assert any(p[1] is None for p in gpu['points'])
    first_query_s = time.monotonic()-start
    counts = Counter()
    latencies = []
    latest = state['snapshot']['sequence']
    started = time.monotonic()
    def history_request():
        try:
            get(url)
            return 200
        except urllib.error.HTTPError as error:
            return error.code
    with ThreadPoolExecutor(max_workers=4) as pool:
        pending = [pool.submit(history_request) for _ in range(4)]
        while time.monotonic()-started < args.seconds:
            for future in list(pending):
                if future.done():
                    counts[future.result()] += 1
                    pending.remove(future)
                    pending.append(pool.submit(history_request))
            stamp = time.monotonic()
            current = get(base + '/api/v1/state')
            latencies.append(time.monotonic()-stamp)
            latest = max(latest, current['snapshot']['sequence'])
            time.sleep(.1)
        for future in pending:
            counts[future.result()] += 1
    elapsed = time.monotonic()-started
    collected = latest-state['snapshot']['sequence']
    assert collected > 0, 'Collection must continue under history contention'
    assert collected <= elapsed / state['settings']['sampling_seconds'] + 2, 'HTTP queries must not trigger extra collection'
    assert counts[200] > 0 and set(counts).issubset({200,429}), counts
    parsed = urllib.parse.urlsplit(base)
    assert parsed.scheme == 'http' and parsed.hostname in ('localhost','127.0.0.1'), 'Cancellation probe is loopback HTTP only'
    with socket.create_connection((parsed.hostname,parsed.port or 80)) as sock:
        sock.sendall(f'GET /api/v1/history?{query} HTTP/1.1\r\nHost: {parsed.netloc}\r\n\r\n'.encode())
    # An abandoned request must not leave the worker pinned indefinitely.
    deadline = time.monotonic()+11
    while True:
        try:
            get(url)
            break
        except urllib.error.HTTPError as error:
            if error.code != 429 or time.monotonic() >= deadline:
                raise
            time.sleep(.05)
    print(json.dumps({'metrics':len(ids),'stored_collections':state['storage']['samples'],'first_query_s':first_query_s,'contention_seconds':elapsed,'responses':dict(counts),'new_collections':collected,'latest_state_max_latency_s':max(latencies),'peak_and_gap_preserved':True,'abandoned_query_recovered':True},indent=2))


if __name__ == '__main__':
    main()
