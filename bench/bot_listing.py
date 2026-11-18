"""Time listing every agent at thousands of stored agents with long instructions.

Synthetic provider, no spend. For each size one stdio daemon starts on a new
store, takes that many bots with instructions of `--instructions-bytes` each
(35 KB is the average a lead's store held on 2026-11-17) and Sonny's 1 KB
compaction instructions, and closes every third, as a lead closes the tasks it
is done with. It then reads the whole listing a window reads, every page of 256
of the open agents, `--repeat` times, and reports each full listing's time
seen at the client and the store's own time for its pages, warm and right
after a restart. Last, 32 of the bots take a text turn each, and the store
time those turns spent is reported per turn: every write of a bot's row
rewrites all of it.

    .local/venv/bin/python -m bench.bot_listing --binary target/release/agent \
        --bots 1000 5000 10000 --out .local/bench/bot-listing
"""
import argparse
import json
import platform
import shutil
import statistics
import time
from datetime import datetime, timezone
from pathlib import Path

from .footprint import pipelined
from .runtime_client import Client
from .synthetic_model import start

BATCH = 64
TURNS = 32
WORDS = ('agent', 'turn', 'tool', 'store', 'window', 'lead', 'task', 'review', 'person', 'model', 'context',
         'shell', 'branch', 'worktree', 'sandbox', 'summary', 'approval', 'margin', 'lookout', 'skill')


def instructions(n, size):
    """Instructions of `size` bytes, different for every bot, as each
    lead and task is composed with its own name, folder and profile."""
    words, i = [f'You are agent b{n}.'], n
    length = len(words[0])
    while length < size:
        i = (i * 1103515245 + 12345) % 2**31
        words.append(WORDS[i % len(WORDS)])
        length += len(words[-1]) + 1
    return ' '.join(words)[:size]


def operations(client):
    return client.request('stats')['result']['store']['operations']


def spent(before, after, names):
    """Count and store time of the named operations between two reads."""
    count = sum(after.get(n, {}).get('count', 0) - before.get(n, {}).get('count', 0) for n in names)
    ran = sum(after.get(n, {}).get('ran_ms', 0) - before.get(n, {}).get('ran_ms', 0) for n in names)
    return count, ran


def list_all(client):
    """Every page of the open agents; how many were listed."""
    after, listed = None, 0
    while True:
        params = dict(limit=256, open=True)
        if after is not None:
            params['after'] = after
        page = client.request('bots', **params)['result']
        listed += len(page['bots'])
        after = page['next_after']
        if after is None:
            return listed


def timed_listings(client, repeat):
    before = operations(client)
    times = []
    for _ in range(repeat):
        began = time.perf_counter()
        listed = list_all(client)
        times.append((time.perf_counter() - began) * 1000)
    count, ran = spent(before, operations(client), ('list',))
    return listed, times, ran / repeat


def summary(times):
    times = sorted(times)
    return {'p50_ms': round(statistics.median(times), 2),
            'p95_ms': round(times[min(len(times) - 1, int(round(.95 * (len(times) - 1))))], 2),
            'min_ms': round(times[0], 2)}


def run(binary, out, bots, size, repeat, url):
    store = out / f'state-{bots}.sqlite'
    workspace = (out / 'workspace').resolve()
    workspace.mkdir(exist_ok=True)
    compaction = 'Summarize the conversation so far. ' * 30
    client = Client(binary, store, url, 'echo,wait', extra=['--max-active', '0'])
    try:
        began = time.monotonic()
        for first in range(0, bots, BATCH):
            requests = [('create', dict(bot=f'b{n:05d}', workspace=str(workspace),
                                        instructions=instructions(n, size), compaction_instructions=compaction))
                        for n in range(first, min(bots, first + BATCH))]
            for reply in pipelined(client, requests):
                assert 'result' in reply, reply
        closing = [('close', dict(bot=f'b{n:05d}')) for n in range(0, bots, 3)]
        for first in range(0, len(closing), BATCH):
            for reply in pipelined(client, closing[first:first + BATCH]):
                assert 'result' in reply, reply
        created_s = time.monotonic() - began
        listed, warm, warm_store = timed_listings(client, repeat)
    finally:
        client.close()
    # A restart: the store's page cache starts empty, as a daemon's does.
    client = Client(binary, store, url, 'echo,wait', extra=['--max-active', '0'])
    try:
        _, cold, cold_store = timed_listings(client, 1)
        before = operations(client)
        turns = []
        for reply in pipelined(client, [('submit', dict(bot=f'b{n:05d}', request_id='one', prompt='hi',
                                                        delivery='reject')) for n in range(1, min(bots, 3 * TURNS), 3)]):
            assert 'result' in reply, reply
            turns.append(reply['result']['turn'])
        for turn in turns:
            event = client.finished(turn, timeout=60)
            assert event['data']['status'] == 'completed', event
        after = operations(client)
        jobs, ran = spent(before, after, list(after))
    finally:
        client.close()
    row = {'bots': bots, 'open': listed, 'instructions_bytes': size, 'created_s': round(created_s, 1),
           'store_mib': round(store.stat().st_size / 2**20, 1),
           'warm': {**summary(warm), 'store_ms': round(warm_store, 2)},
           'cold': {**summary(cold), 'store_ms': round(cold_store, 2)},
           'turn_store_ms': round(ran / len(turns), 3), 'turn_jobs': round(jobs / len(turns), 2)}
    store.unlink()
    for suffix in ('-wal', '-shm'):
        store.with_name(store.name + suffix).unlink(missing_ok=True)
    return row


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument('--binary', type=Path, required=True)
    parser.add_argument('--out', type=Path, required=True)
    parser.add_argument('--bots', type=int, nargs='+', default=[1000, 5000, 10000])
    parser.add_argument('--instructions-bytes', type=int, default=35_000)
    parser.add_argument('--repeat', type=int, default=20)
    args = parser.parse_args()
    if args.out.exists():
        shutil.rmtree(args.out)
    args.out.mkdir(parents=True)
    _, url = start()
    rows = [run(args.binary.resolve(), args.out, n, args.instructions_bytes, args.repeat, url) for n in args.bots]
    result = {'when': datetime.now(timezone.utc).isoformat(), 'machine': platform.platform(), 'rows': rows}
    (args.out / 'bot_listing.json').write_text(json.dumps(result, indent=2) + '\n')
    print(json.dumps(result, indent=2))


if __name__ == '__main__':
    main()
