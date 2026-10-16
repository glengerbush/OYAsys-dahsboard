#!/usr/bin/env python3
"""Reproducible SQLite encoding comparison; development only, no OYAsys data used."""
import argparse
import json
import math
from pathlib import Path
import resource
import sqlite3
import struct
import subprocess
import sys
import tempfile
import time


def run(kind, rows, metrics, directory):
    path = Path(directory) / (kind + '.sqlite3')
    db = sqlite3.connect(path, cached_statements=8)
    db.executescript('PRAGMA auto_vacuum=INCREMENTAL; PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL; PRAGMA cache_size=-512; PRAGMA mmap_size=0; PRAGMA wal_autocheckpoint=256;')
    columns = 'values_blob BLOB NOT NULL' if kind == 'dense' else ','.join(f'v{i} REAL' for i in range(metrics))
    db.executescript(f'CREATE TABLE samples(sequence INTEGER PRIMARY KEY,timestamp_ms INTEGER,catalog INTEGER,cadence_ms INTEGER,{columns}); CREATE INDEX sample_time ON samples(timestamp_ms);')
    insert = f'INSERT INTO samples VALUES({",".join("?" for _ in range(5 if kind == "dense" else metrics+4))})'
    packed = struct.Struct('<' + 'd' * metrics)
    wall = time.monotonic()
    cpu = time.process_time()
    for index in range(rows):
        values = tuple(40 + j + math.sin(index / (20 + j)) for j in range(metrics))
        payload = (b'\1' + packed.pack(*values),) if kind == 'dense' else values
        db.execute(insert, (index + 1, index * 10000, 1, 10000, *payload))
        db.commit()
    write = {'wall_s': time.monotonic()-wall, 'cpu_s': time.process_time()-cpu}
    db.execute('PRAGMA wal_checkpoint(TRUNCATE)')
    wall = time.monotonic()
    cpu = time.process_time()
    checksum = 0
    query = 'SELECT values_blob FROM samples ORDER BY timestamp_ms' if kind == 'dense' else 'SELECT '+','.join(f'v{i}' for i in range(metrics))+' FROM samples ORDER BY timestamp_ms'
    for row in db.execute(query):
        values = packed.unpack_from(row[0], 1) if kind == 'dense' else row
        checksum += sum(values)
    read = {'wall_s': time.monotonic()-wall, 'cpu_s': time.process_time()-cpu}
    print(json.dumps({'encoding':kind,'rows':rows,'metrics':metrics,'bytes':path.stat().st_size,'write':write,'read_all':read,'peak_rss_kib':resource.getrusage(resource.RUSAGE_SELF).ru_maxrss,'checksum':checksum}))
    db.close()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--rows', type=int, default=259200)
    parser.add_argument('--metrics', type=int, default=50)
    parser.add_argument('--worker', choices=['dense', 'wide'])
    parser.add_argument('--directory')
    args = parser.parse_args()
    if not 1 <= args.rows <= 31536000 or not 1 <= args.metrics <= 128:
        parser.error('rows 1..31536000, metrics 1..128')
    if args.worker:
        run(args.worker, args.rows, args.metrics, args.directory)
        return
    with tempfile.TemporaryDirectory(prefix='oyasys-encoding-') as directory:
        results = []
        for kind in ('dense', 'wide'):
            output = subprocess.check_output([sys.executable, __file__, '--worker', kind, '--directory', directory, '--rows', str(args.rows), '--metrics', str(args.metrics)], text=True)
            results.append(json.loads(output))
        print(json.dumps({'sqlite':sqlite3.sqlite_version,'results':results,'note':'Development encoding comparison, not service/NVML footprint. Each format has an isolated Python process, one transaction per collection, identical metrics/indexes/cache/WAL settings. Full reads decode every gauge.'}, indent=2))


if __name__ == '__main__':
    main()
