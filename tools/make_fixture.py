#!/usr/bin/env python3
"""Create explicitly synthetic telemetry for browser and 30-day query qualification."""
import argparse
import json
import math
from pathlib import Path
import time


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--days', type=int, default=1)
    parser.add_argument('--step', type=int, default=10, help='Seconds per collection')
    parser.add_argument('--metrics', type=int, default=18, help='At least 18; extras are synthetic sensors')
    parser.add_argument('--output', required=True, type=Path, help='A new file, never overwritten')
    args = parser.parse_args()
    if not 1 <= args.days <= 365 or not 1 <= args.step <= 3600 or not 18 <= args.metrics <= 128:
        parser.error('days 1..365; step 1..3600; metrics 18..128')
    definitions = [
        ('gpu.demo.temperature', 'P100 temperature', '°C', 'gpu', 'Synthetic Tesla P100 GPU-demo'),
        ('gpu.demo.utilization', 'GPU utilization', '%', 'gpu', 'Synthetic Tesla P100 GPU-demo'),
        ('gpu.demo.memory_used', 'VRAM used', 'bytes', 'gpu', 'Synthetic Tesla P100 GPU-demo'),
        ('gpu.demo.memory_total', 'VRAM total', 'bytes', 'gpu', 'Synthetic Tesla P100 GPU-demo'),
        ('gpu.demo.power', 'GPU power', 'W', 'gpu', 'Synthetic Tesla P100 GPU-demo'),
        ('gpu.demo.clock', 'GPU clock', 'MHz', 'gpu', 'Synthetic Tesla P100 GPU-demo'),
        ('hwmon.demo.fan2', 'Chassis fan 2 (synthetic)', 'RPM', 'fan', 'Synthetic motherboard header'),
        ('cpu.utilization', 'CPU utilization', '%', 'cpu', 'Synthetic CPU'),
        ('memory.used', 'Memory used', 'bytes', 'memory', 'Synthetic RAM'),
        ('memory.total', 'Memory total', 'bytes', 'memory', 'Synthetic RAM'),
        ('memory.swap_used', 'Swap used', 'bytes', 'memory', 'Synthetic RAM'),
        ('cpu.temperature', 'CPU temperature', '°C', 'thermal', 'Synthetic CPU'),
        ('disk.demo.used', 'System disk used', 'bytes', 'storage', 'Synthetic NVMe'),
        ('disk.demo.total', 'System disk total', 'bytes', 'storage', 'Synthetic NVMe'),
        ('disk.demo.read', 'Disk reads', 'bytes/s', 'storage', 'Synthetic NVMe'),
        ('network.demo.receive', 'Ethernet receive', 'bytes/s', 'network', 'Synthetic Ethernet'),
        ('network.demo.transmit', 'Ethernet transmit', 'bytes/s', 'network', 'Synthetic Ethernet'),
        ('system.uptime', 'Uptime', 's', 'system', 'Synthetic host'),
    ]
    for index in range(18, args.metrics):
        definitions.append((f'thermal.demo.{index}', f'Synthetic sensor {index}', '°C', 'thermal', 'Synthetic qualification fixture'))
    inventory = {
        'hostname': 'minisforum-demo', 'synthetic': True, 'warnings': [],
        'system': [{'label': 'Fixture', 'value': 'Synthetic — not live hardware'},
                   {'label': 'Board', 'value': 'BD790i (example)'}, {'label': 'CPU', 'value': 'AMD Ryzen 9 (example)'},
                   {'label': 'Memory', 'value': '32 GiB (example)'}, {'label': 'OS', 'value': 'DietPi (example)'}],
        'metrics': [dict(zip(('id', 'label', 'unit', 'group', 'source'), row)) for row in definitions],
    }
    now = int(time.time()) * 1000
    count = args.days * 86400 // args.step
    with args.output.open('x') as output:
        output.write('{"inventory":')
        json.dump(inventory, output, separators=(',', ':'))
        output.write(',"now_ms":' + str(now) + ',"samples":[')
        for index in range(count):
            stamp = now - (count - 1 - index) * args.step * 1000
            wave = (math.sin(index * args.step / 900) + 1) / 2
            load = max(0, wave * 80 - 10)
            temperature = 36 + load * .35
            raw = [temperature, load, (6 + load / 14) * 2**30, 16 * 2**30,
                   25 + load * 1.6, 405 + load * 10, 1200 + load * 16,
                   8 + wave * 27, (11 + wave * 2) * 2**30, 32 * 2**30, 0,
                   44 + wave * 18, 180 * 2**30, 512 * 2**30, wave * 2e7,
                   wave * 6e6, wave * 1e6, 86400 + index * args.step]
            raw.extend(32 + wave * 15 + n % 8 for n in range(18, args.metrics))
            values = {row[0]: round(value, 3) for row, value in zip(definitions, raw)}
            # One explicit GPU availability gap and one sampled peak per day.
            if 2100 <= (index * args.step) % 86400 < 2220:
                values['gpu.demo.temperature'] = None
            if (index * args.step) % 86400 == 3600:
                values['gpu.demo.temperature'] = 78
            if index:
                output.write(',')
            json.dump({'timestamp_ms': stamp, 'values': values}, output, separators=(',', ':'))
        output.write(']}\n')
    print(f'{count} synthetic collections, {args.metrics} metrics: {args.output}')


if __name__ == '__main__':
    main()
