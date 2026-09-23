"""Bounded, sequential engine screening matrix; no real provider calls."""

import argparse
import json
from pathlib import Path
import subprocess
import sys

from .report import compare

# Sampled-RSS guard per target tree, in MiB. opencode's one Bun server exceeds
# 512 MiB with a single agent (about 0.8 GiB at c1 and 1.0 GiB at c32-h65536,
# observed 2026-09-23). compare() requires identical limits, so every engine in
# a matrix runs under the largest guard among the selected engines.
RSS_GUARD_MIB = {'opencode': 2048}
DEFAULT_RSS_GUARD_MIB = 512
PROCESS_GUARD = 16


def rss_guard(engines):
    return max(RSS_GUARD_MIB.get(engine, DEFAULT_RSS_GUARD_MIB) for engine in engines)


# Engines whose normal deployment is one native process per agent session.
PROCESS_PER_AGENT = {'claude-code'}


def guards(engine, concurrency, rss_mib=DEFAULT_RSS_GUARD_MIB):
    """Sampled RSS (MiB) and process-count guards for one target tree.

    The 512 MiB / 16-process guard bounds a shared-process target. A
    process-per-agent engine gets the same guard per agent process tree,
    so the guard scales with concurrency instead of failing by construction.
    """
    scale = concurrency if engine in PROCESS_PER_AGENT else 1
    return rss_mib * scale, PROCESS_GUARD * scale


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--out", required=True, type=Path)
    parser.add_argument("--engines", nargs='+', choices=('pi', 'codex', 'rust', 'fx', 'opencode', 'claude-code'),
                        default=['pi', 'codex', 'rust'])
    args = parser.parse_args()
    print('Exploratory cross-engine screen: unequal feature footprints; no efficiency ranking.', flush=True)
    if len(set(args.engines)) != len(args.engines) or len(args.engines) < 2:
        parser.error('select at least two distinct engines')
    root = Path(__file__).resolve().parent.parent
    output = args.out.resolve()
    if Path.cwd() != root or not output.is_relative_to(root / '.local'):
        parser.error('run from the repository root with a new output under .local')
    output.mkdir(parents=True, exist_ok=False)
    guard = rss_guard(args.engines)
    if guard != DEFAULT_RSS_GUARD_MIB:
        print(f'RSS guard raised to {guard} MiB per tree for every engine in this matrix.', flush=True)
    results = []
    # Freeze the screening conditions before interpreting results. These are
    # exploration bounds, not a claim that 32 agents meets product capacity.
    for index, (concurrency, history) in enumerate(
            (c, h) for c in (1, 8, 32) for h in (4096, 65536)):
        name = f'c{concurrency}-h{history}'
        workload = dict(version=1, concurrency=concurrency, turns=3, chunks=20,
                        chunk_bytes=256, chunk_delay_ms=25, history_bytes=history)
        workload_path = output / f'{name}.json'
        workload_path.write_text(json.dumps(workload, indent=2) + '\n')
        # Alternate order across cases. Targets never run concurrently.
        for engine in (args.engines if index % 2 == 0 else list(reversed(args.engines))):
            print(f'{name}: {engine}', flush=True)
            rss_limit, process_limit = guards(engine, concurrency, guard)
            command = [sys.executable, '-m', 'bench', 'run', '--engine', engine,
                       '--out', str(output / f'{name}-{engine}'),
                       '--workload', str(workload_path), '--repeat', '3', '--warmup', '1',
                       '--timeout', '30', '--interval', '.1', '--discovery-interval', '.5',
                       '--rss-limit-mib', str(rss_limit), '--process-limit', str(process_limit)]
            code = subprocess.call(command)
            if code:
                print('Matrix stopped on a failed case; partial captures retained.', file=sys.stderr)
                return code
        saved = {engine: json.loads((output / f'{name}-{engine}' / 'result.json').read_text())
                 for engine in args.engines}
        results.append({'case': name, 'workload': workload,
                        'baseline': args.engines[0],
                        'comparisons': {engine: compare(saved[args.engines[0]], saved[engine], exploratory=True)
                                        for engine in args.engines[1:]}})
        (output / 'comparisons.json').write_text(json.dumps(results, indent=2) + '\n')
    return 0


if __name__ == '__main__':
    sys.exit(main())
