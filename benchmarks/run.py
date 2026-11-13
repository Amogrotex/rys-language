#!/usr/bin/env python3
"""End-to-end timing: process startup, load/compile/verify and execution."""
import argparse
import os
from pathlib import Path
import statistics
import subprocess
import tempfile
import time

CASES = {'arithmetic': '499999500000', 'arrays': '1500000',
         'builtins': '3000000', 'string_index': 'x'}
parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument('binary', type=Path)
parser.add_argument('--runs', type=int, default=9)
parser.add_argument('--cache', choices=['off', 'warm'], default='off')
args = parser.parse_args()
if args.runs < 1:
    parser.error('--runs must be positive')
binary = args.binary.resolve()
root = Path(__file__).resolve().parent
with tempfile.TemporaryDirectory(prefix='rys-bench-') as cache:
    env = os.environ.copy()
    env['RYS_CACHE_DIR'] = cache
    if args.cache == 'off':
        env['RYS_NO_CACHE'] = '1'
    else:
        env.pop('RYS_NO_CACHE', None)
    for case, expected in CASES.items():
        command = [str(binary), str(root / (case + '.rys'))]
        def run():
            started = time.perf_counter()
            result = subprocess.run(command, env=env, capture_output=True, text=True, check=True)
            elapsed = time.perf_counter() - started
            if result.stdout.strip() != expected:
                raise RuntimeError(f'{case}: unexpected output {result.stdout!r}')
            return elapsed * 1000
        run()  # warm process-independent caches; populate RYS cache if enabled
        samples = [run() for _ in range(args.runs)]
        print(f'{case:14} median={statistics.median(samples):8.2f} ms  min={min(samples):8.2f} max={max(samples):8.2f} n={len(samples)} cache={args.cache}')
