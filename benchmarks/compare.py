#!/usr/bin/env python3
"""Compare two RYS binaries in alternating order, with the cache disabled."""
import argparse
import os
from pathlib import Path
import statistics
import subprocess
import time

CASES = {'arithmetic': '499999500000', 'arrays': '1500000',
         'builtins': '3000000', 'string_index': 'x'}
p = argparse.ArgumentParser(description=__doc__)
p.add_argument('baseline', type=Path)
p.add_argument('candidate', type=Path)
p.add_argument('--runs', type=int, default=16)
a = p.parse_args()
if a.runs < 1:
    p.error('--runs must be positive')
env = os.environ.copy()
env['RYS_NO_CACHE'] = '1'
root = Path(__file__).resolve().parent
for case, expected in CASES.items():
    commands = [[str(binary.resolve()), str(root / (case + '.rys'))] for binary in (a.baseline, a.candidate)]
    samples = [[], []]
    for iteration in range(a.runs + 1):
        for i in ((0, 1) if iteration % 2 == 0 else (1, 0)):
            start = time.perf_counter()
            result = subprocess.run(commands[i], env=env, capture_output=True, text=True, check=True)
            elapsed = (time.perf_counter() - start) * 1000
            if result.stdout.strip() != expected:
                raise RuntimeError(f'{case}: unexpected output {result.stdout!r}')
            if iteration:
                samples[i].append(elapsed)
    before, after = map(statistics.median, samples)
    print(f'{case:14} baseline={before:8.2f} ms candidate={after:8.2f} ms ratio={before/after:.2f}x '
          f'ranges={min(samples[0]):.2f}-{max(samples[0]):.2f}/{min(samples[1]):.2f}-{max(samples[1]):.2f} n={a.runs}')
