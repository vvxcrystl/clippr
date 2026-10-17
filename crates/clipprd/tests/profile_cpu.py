#!/usr/bin/env python3
"""Measure a steady live hardware capture, without attaching a debugger.

Usage: python3 crates/clipprd/tests/profile_cpu.py PID --seconds 45
Wait for the replay buffer to fill first; avoid saves/builds during measurement.
CPU percentages are fractions of one core, including kernel time. schedstat
provides nanosecond resolution; stat is a separate, tick-resolution cross-check.
This script only reads /proc and never controls or stops the process.
"""
import argparse
import json
import os
from pathlib import Path
import time


def sample(pid):
    root = Path(f'/proc/{pid}')
    stat = (root / 'stat').read_text().rsplit(')', 1)[1].split()
    threads = {}
    for task in (root / 'task').iterdir():
        status = dict(line.split(':', 1) for line in
                      (task / 'status').read_text().splitlines() if ':' in line)
        threads[task.name] = {
            'name': (task / 'comm').read_text().strip(),
            'runtime_ns': int((task / 'schedstat').read_text().split()[0]),
            'switches': int(status['voluntary_ctxt_switches']) +
                        int(status['nonvoluntary_ctxt_switches']),
        }
    return stat[19], int(stat[11]) + int(stat[12]), threads


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('pid', type=int)
    parser.add_argument('--seconds', type=float, default=45)
    args = parser.parse_args()
    if args.pid <= 0 or args.seconds <= 0:
        parser.error('PID and seconds must be positive')
    identity, ticks_before, before = sample(args.pid)
    start = time.monotonic()
    time.sleep(args.seconds)
    end_identity, ticks_after, after = sample(args.pid)
    elapsed = time.monotonic() - start
    if identity != end_identity or before.keys() != after.keys():
        raise SystemExit('Process/threads changed; repeat during steady recording')
    threads = []
    for tid, last in after.items():
        first = before[tid]
        threads.append({
            'tid': int(tid), 'name': last['name'],
            'cpu_percent': (last['runtime_ns'] - first['runtime_ns']) / elapsed / 1e7,
            'context_switches_per_second': (last['switches'] - first['switches']) / elapsed,
        })
    threads.sort(key=lambda row: -row['cpu_percent'])
    print(json.dumps({
        'elapsed_seconds': elapsed,
        'cpu_percent': sum(row['cpu_percent'] for row in threads),
        'stat_cpu_percent': (ticks_after - ticks_before) / os.sysconf('SC_CLK_TCK') / elapsed * 100,
        'threads': threads,
    }, indent=2))


if __name__ == '__main__':
    main()
