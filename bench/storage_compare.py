"""Alternate unchanged and candidate daemons on exact-history lifecycle workloads.

This reuses the lifecycle observer and synthetic provider. All timed turns,
provider input/output bytes, replay, retries of prior submissions, and historical
forks must agree. It does not infer capacity from these bounded workloads.
"""
import argparse
import json
from pathlib import Path
from .lifecycle import run_once
from .targets import file_hash


def run(before, after, out, repeats=5):
    out.mkdir(parents=True, exist_ok=False)
    binaries = {'before': before.resolve(), 'after': after.resolve()}
    report = {'schema': 'storage_compare_v1', 'binaries': {k: file_hash(v) for k, v in binaries.items()},
              'runs': []}
    for size in (256, 65536):
        config = dict(version=1, concurrency=8, turns=16, chunks=4, chunk_bytes=256,
                      chunk_delay_ms=0, history_bytes=size)
        expected = None
        for pair in range(repeats + 1):
            for label in (('before', 'after') if pair % 2 == 0 else ('after', 'before')):
                directory = out / f'{size}-{pair}-{label}'
                directory.mkdir()
                result = run_once(binaries[label], directory, config, 'text', 'echo')
                assert result['status'] == 'ok', result
                provider = json.loads((directory / 'provider.json').read_text())
                contract = {k: provider[k] for k in ('requests', 'completed_requests', 'invalid_requests',
                                                    'request_body_bytes', 'response_body_bytes')}
                expected = expected or contract
                assert contract == expected, (contract, expected)
                result.update(label=label, warmup=pair == 0, pair=pair, history_bytes=size,
                              provider_contract=contract)
                report['runs'].append(result)
                (out / 'result.json').write_text(json.dumps(report, indent=2) + '\n')
                print(json.dumps({k: result[k] for k in ('label', 'pair', 'history_bytes', 'turn_ms',
                                 'target_observed_cpu_seconds', 'retained_store_bytes')}), flush=True)
    return report


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--before', type=Path, required=True)
    parser.add_argument('--after', type=Path, required=True)
    parser.add_argument('--out', type=Path, required=True)
    parser.add_argument('--repeats', type=int, default=5)
    args = parser.parse_args()
    if not 1 <= args.repeats <= 10:
        parser.error('--repeats must be 1..10')
    run(args.before, args.after, args.out.resolve(), args.repeats)


if __name__ == '__main__':
    main()
