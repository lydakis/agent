"""Measure what the daemon holds: idle, per idle bot, and after a turn each.

Synthetic provider, no spend. One stdio daemon starts with no bots; RSS is
read a second after each phase settles. Bots are created in pipelined
batches of 64, then each takes one text turn; that phase also reports daemon
CPU and store work per turn. RSS after the turns is the allocator's
high-water mark from running them, not the live state of finished bots.
"""
import argparse
from datetime import datetime, timezone
import json
import platform
import time
from pathlib import Path

import psutil

from .admission_burst import cpu_ms
from .runtime_client import Client
from .synthetic_model import start
from .targets import file_hash

BATCH = 64


def pipelined(client, requests):
    """Write the requests together; return their replies in request order."""
    ids = []
    for op, params in requests:
        if op == 'create':
            params.setdefault('model', client.model)
            params.setdefault('instructions', client.instructions)
            params.setdefault('tools', client.tools)
        client.next_id += 1
        ids.append(client.next_id)
        client.process.stdin.write(json.dumps({'id': client.next_id, 'op': op, **params}) + '\n')
    client.process.stdin.flush()
    wanted, replies = set(ids), {}
    while wanted:
        reply = client.receive(lambda m: m.get('id') in wanted, timeout=60)
        wanted.discard(reply['id'])
        replies[reply['id']] = reply
    return [replies[i] for i in ids]


def run(binary, out, bots):
    model, url = start()
    client = None
    try:
        (out / 'workspace').mkdir()
        client = Client(binary, out / 'state.sqlite', url, 'echo,shell,wait', extra=['--max-active', '0'])
        process = psutil.Process(client.process.pid)

        def rss_mib():
            time.sleep(1)
            return process.memory_info().rss / 2**20

        idle = rss_mib()
        for first in range(0, bots, BATCH):
            for reply in pipelined(client, [('create', dict(bot=f'b{n}', workspace=str(out / 'workspace')))
                                            for n in range(first, min(bots, first + BATCH))]):
                assert 'result' in reply, reply
        created = rss_mib()
        before = client.request('stats')['result']['store']['operations']
        began, cpu = time.monotonic(), cpu_ms(process)
        turns = []
        for first in range(0, bots, BATCH):
            for reply in pipelined(client, [('submit', dict(bot=f'b{n}', request_id='one', prompt='hi'))
                                            for n in range(first, min(bots, first + BATCH))]):
                assert 'result' in reply, reply
                turns.append(reply['result']['turn'])
        for turn in turns:
            event = client.finished(turn, timeout=60)
            assert event['data']['status'] == 'completed', event
        elapsed, cpu = time.monotonic() - began, cpu_ms(process) - cpu
        after = client.request('stats')['result']['store']['operations']
        operations = {name: {key: value[key] - before.get(name, {}).get(key, 0) for key in ('count', 'ran_ms')}
                      for name, value in after.items()}
        turned = rss_mib()
        jobs = sum(op['count'] for name, op in operations.items() if name != 'commit')
        return dict(idle_rss_mib=idle, bots_rss_mib=created, per_idle_bot_kib=(created - idle) * 1024 / bots,
                    turns_rss_mib=turned, turns_s=elapsed, cpu_ms_per_turn=cpu / bots,
                    store_jobs_per_turn=jobs / bots,
                    commits_per_turn=operations.get('commit', {}).get('count', 0) / bots,
                    store_bytes=sum(f.stat().st_size for f in out.glob('state.sqlite*')),
                    operations=operations)
    finally:
        if client:
            client.close()
        model.shutdown()
        model.server_close()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--binary', type=Path, default=Path('.local/target/release/agent'))
    parser.add_argument('--out', type=Path, required=True)
    parser.add_argument('--bots', type=int, default=1000)
    parser.add_argument('--repeat', type=int, default=3)
    args = parser.parse_args()
    if not 1 <= args.bots <= 10000 or args.repeat < 1:
        parser.error('use 1–10,000 bots and at least one measured repetition')
    args.out.mkdir(parents=True, exist_ok=False)
    binary = args.binary.resolve()
    result = dict(schema='footprint_v1', binary_sha256=file_hash(binary),
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
        print(json.dumps({k: v for k, v in row.items() if k != 'operations'}), flush=True)


if __name__ == '__main__':
    main()
