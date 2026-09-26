"""Send creations and submissions in bursts to measure durable admission.

Synthetic provider, no spend. A burst writes every request to the daemon at
once on one connection and times each reply from that write; submitted turns
hold at the model gate, so no completion competes with admission. A lone
phase first submits one request at a time: work that arrives alone must not
wait for company.
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

LABELS = ('create', 'begin', 'commit')


def cpu_ms(process):
    """The daemon's CPU time: per-thread scheduler nanoseconds on Linux,
    whose tick-based counters are too coarse for a burst of milliseconds."""
    try:
        return sum(int(Path(f'/proc/{process.pid}/task/{task}/schedstat').read_text().split()[0])
                   for task in (thread.id for thread in process.threads())) / 1e6
    except (OSError, psutil.Error):
        times = process.cpu_times()
        return (times.user + times.system) * 1000


def burst(client, requests):
    """Write the requests together; return each reply with its latency."""
    lines = []
    for op, params in requests:
        if op == 'create':
            params = dict(model=client.model, instructions=client.instructions, tools=client.tools, **params)
        client.next_id += 1
        lines.append(json.dumps({'id': client.next_id, 'op': op, **params}) + '\n')
    first = client.next_id - len(lines) + 1
    began = time.monotonic()
    client.process.stdin.write(''.join(lines))
    client.process.stdin.flush()
    replies = []
    for id in range(first, client.next_id + 1):
        reply = client.receive(lambda m, id=id: m.get('id') == id)
        replies.append((reply, (time.monotonic() - began) * 1000))
    return replies


def phase(client, process, send):
    """Run one timed phase and report replies, daemon CPU, and store work."""
    before = client.request('stats')['result']['store']
    cpu = cpu_ms(process)
    replies, elapsed = send()
    spent = cpu_ms(process) - cpu
    after = client.request('stats')['result']['store']
    for reply, _ in replies:
        assert 'result' in reply, reply
    operations = {label: {key: after['operations'].get(label, {}).get(key, 0)
                          - before['operations'].get(label, {}).get(key, 0)
                          for key in ('count', 'queued_ms', 'ran_ms', 'answered_ms')}
                  for label in LABELS}
    groups = {key: after['groups'][key] - before['groups'][key] for key in ('count', 'jobs')}
    return dict(requests=len(replies), elapsed_ms=elapsed,
                reply_ms=percentiles([latency for _, latency in replies]),
                cpu_ms=spent,
                operations=operations, groups=groups)


def run(binary, out, bots):
    model, _ = start()
    client = None
    try:
        client = Client(binary, out / 'state.sqlite', f'http://127.0.0.1:{model.server_port}/v1')
        process = psutil.Process(client.process.pid)
        workspace = str(out.resolve())
        for n in range(bots):
            assert 'result' in client.request('create', bot=f'lone{n}', workspace=workspace)

        def lone():
            replies, began = [], time.monotonic()
            for n in range(bots):
                sent = time.monotonic()
                reply = client.request('submit', bot=f'lone{n}', request_id='first', prompt='hold:admission')
                replies.append((reply, (time.monotonic() - sent) * 1000))
            return replies, (time.monotonic() - began) * 1000

        def together(requests):
            def send():
                replies = burst(client, requests)
                return replies, max(latency for _, latency in replies)
            return send

        row = dict(lone_submit=phase(client, process, lone))
        row['burst_create'] = phase(client, process, together(
            [('create', dict(bot=f'burst{n}', workspace=workspace)) for n in range(bots)]))
        row['burst_submit'] = phase(client, process, together(
            [('submit', dict(bot=f'burst{n}', request_id='first', prompt='hold:admission'))
             for n in range(bots)]))
        started = client.request('stats')['result']['active_turns']
        assert started == 2 * bots, started
        row['rss_mib'] = process.memory_info().rss / 2**20
        return row
    finally:
        model.release.set()
        if client:
            client.close(kill=True)
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
    result = dict(schema='admission_burst_v1', binary_sha256=file_hash(binary),
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
