"""Bounded, sequential engine screening matrix; no real provider calls."""

import argparse
import json
from pathlib import Path
import subprocess
import sys

from .report import compare


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--out", required=True, type=Path)
    parser.add_argument("--engines", nargs='+', choices=('pi', 'codex', 'rust', 'fx'),
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
            command = [sys.executable, '-m', 'bench', 'run', '--engine', engine,
                       '--out', str(output / f'{name}-{engine}'),
                       '--workload', str(workload_path), '--repeat', '3', '--warmup', '1',
                       '--timeout', '30', '--interval', '.1', '--discovery-interval', '.5',
                       '--rss-limit-mib', '512', '--process-limit', '16']
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
