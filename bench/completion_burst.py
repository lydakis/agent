"""Release held model replies together to measure durable completion tails.

Synthetic provider, no spend. Creation and admission happen before the timer;
the measured interval includes receiving replies, storing them, and publishing
terminal events. This isolates completion pressure, not sustained throughput.
"""
import argparse
from datetime import datetime, timezone
import json
import platform
import time
from pathlib import Path

import psutil

from .events import percentiles
from .runtime_client import Client
from .synthetic_model import start
from .targets import file_hash


def run(binary, out, bots):
    model, _ = start()
    client = None
    try:
        client = Client(binary, out / 'state.sqlite', f'http://127.0.0.1:{model.server_port}/v1')
        for n in range(bots):
            assert 'result' in client.request('create', bot=str(n), workspace=str(out.resolve()))
        turns = [client.request('submit', bot=str(n), request_id='first', prompt='hold:completion')['result']['turn']
                 for n in range(bots)]
        deadline = time.monotonic() + 10
        while model.requests != bots:
            if time.monotonic() >= deadline:
                raise TimeoutError('not all provider requests arrived at the gate')
            time.sleep(.005)
        before = client.request('stats')['result']['store']['operations']
        process = psutil.Process(client.process.pid)
        cpu = process.cpu_times()
        began = time.monotonic()
        model.release.set()
        latencies = []
        for turn in turns:
            event = client.finished(turn)
            assert event['data']['status'] == 'completed', event
            latencies.append((event['_received_at'] - began) * 1000)
        elapsed = time.monotonic() - began
        after_cpu = process.cpu_times()
        after = client.request('stats')['result']['store']['operations']
        return dict(completed=len(turns), release_to_terminal_ms=percentiles(latencies),
                    elapsed_s=elapsed, cpu_s=after_cpu.user + after_cpu.system - cpu.user - cpu.system,
                    operations={name: {key: value[key] - before.get(name, {}).get(key, 0)
                                       for key in ('count', 'ran_ms', 'queued_ms')}
                                for name, value in after.items()})
    finally:
        model.release.set()
        if client:
            client.close()
        model.shutdown()
        model.server_close()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--binary', type=Path, default=Path('.local/target/release/agent'))
    parser.add_argument('--out', type=Path, required=True)
    parser.add_argument('--bots', type=int, default=32)
    parser.add_argument('--repeat', type=int, default=3)
    args = parser.parse_args()
    if not 1 <= args.bots <= 64 or args.repeat < 1:
        parser.error('use 1–64 bots and at least one measured repetition')
    args.out.mkdir(parents=True, exist_ok=False)
    binary = args.binary.resolve()
    result = dict(schema='completion_burst_v1', binary_sha256=file_hash(binary),
                  observed_at=datetime.now(timezone.utc).isoformat(),
                  observer_sha256=file_hash(Path(__file__)),
                  fixture_sha256=file_hash(Path(__file__).with_name('synthetic_model.py')),
                  host=dict(system=platform.system(), architecture=platform.machine(),
                            python=platform.python_version()),
                  bots=args.bots, runs=[])
    for n in range(args.repeat + 1):
        directory = args.out / str(n)
        directory.mkdir()
        row = dict(warmup=n == 0, **run(binary, directory, args.bots))
        result['runs'].append(row)
        (args.out / 'result.json').write_text(json.dumps(result, indent=2) + '\n')
        print(json.dumps(row), flush=True)


if __name__ == '__main__':
    main()
