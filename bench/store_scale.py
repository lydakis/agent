"""Grow one store past each target size and measure the daemon at every step.

Synthetic provider, no spend. Thousands of bots share the growth: most get a
few turns, a handful of heavy bots get a quarter of them, so both short and
long histories exist at every size. Half the turns are shell commands whose
1 MB of output overflows into an artifact, the other half are 4 KiB text
prompts, so nodes, events, and artifact blobs all grow. At each target the
screen measures, on the same store:

- submit-to-finish latency on light and heavy bots, text and shell;
- paging every bot, one heavy bot's turns, and one bot's events;
- a fork, deleting that fork, and deleting one heavy bot with its artifacts;
- a clean restart, a crash restart with 32 turns in flight, and the
  schema-21 migration replayed against the store;
- the storage worker's own per-operation histograms over the growth phase;
- daemon RSS and WAL size, sampled throughout.

    .local/venv/bin/python -m bench.store_scale --out DIR --sizes-gb 1 10

The store is removed at the end unless --keep is given.
"""
import argparse
import hashlib
import json
import shutil
import sqlite3
import sys
import threading
import time
from datetime import datetime, timezone
from pathlib import Path

import psutil

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from bench.runtime_client import Client  # noqa: E402
from bench.synthetic_model import start  # noqa: E402

SHELL = "shell:head -c 1000000 /dev/zero | tr '\\0' x"
TEXT = 'x' * 4096
CONCURRENCY = 32
HEAVY = 8


def percentiles(values):
    if not values:
        return {'p50': None, 'p95': None, 'max': None, 'n': 0}
    values = sorted(values)
    pick = lambda q: values[min(len(values) - 1, int(round(q * (len(values) - 1))))]
    return {'p50': round(pick(.5), 2), 'p95': round(pick(.95), 2), 'max': round(values[-1], 2), 'n': len(values)}


def store_bytes(store):
    return sum(p.stat().st_size for p in (store, store.with_name(store.name + '-wal')) if p.exists())


def wal_bytes(store):
    wal = store.with_name(store.name + '-wal')
    return wal.stat().st_size if wal.exists() else 0


class Sampler:
    """Peak daemon RSS and WAL size per phase, sampled twice a second."""

    def __init__(self, store):
        self.store, self.phase, self.peaks = store, 'idle', {}
        self.pid, self.stop = None, threading.Event()
        self.thread = threading.Thread(target=self.run, daemon=True)
        self.thread.start()

    def run(self):
        while not self.stop.wait(.5):
            rss = 0
            if self.pid:
                try:
                    rss = psutil.Process(self.pid).memory_info().rss
                except psutil.Error:
                    pass
            peak = self.peaks.setdefault(self.phase, {'rss_mib': 0, 'wal_mib': 0})
            peak['rss_mib'] = round(max(peak['rss_mib'], rss / 2**20), 2)
            peak['wal_mib'] = round(max(peak['wal_mib'], wal_bytes(self.store) / 2**20), 2)


class Screen:
    def __init__(self, binary, store, url, server, bots, sampler):
        self.binary, self.store, self.url, self.server = binary, store, url, server
        self.bots, self.sampler = bots, sampler
        self.turns = {}  # bot -> submissions so far, for request ids
        self.shell_turns = {}
        self.next_light = 0
        self.client = None
        self.deleted = 0

    def connect(self):
        before = time.monotonic()
        self.client = Client(self.binary, self.store, self.url, tools='echo,shell',
                             extra=('--max-active', str(CONCURRENCY), '--max-processes', str(CONCURRENCY)))
        self.sampler.pid = self.client.process.pid
        return round((time.monotonic() - before) * 1000, 1)

    def name(self, n):
        return f'b{n}'

    def heavy(self, n):
        return f'h{n}'

    def request_id(self, bot):
        self.turns[bot] = self.turns.get(bot, 0) + 1
        return f'r{self.turns[bot]}'

    def pick(self, k):
        # A quarter of the growth lands on the heavy bots.
        if k % 4 == 0:
            return self.heavy(k // 4 % HEAVY)
        self.next_light = (self.next_light + 1) % self.bots
        return self.name(self.next_light)

    def create_all(self):
        before = time.monotonic()
        for n in range(self.bots):
            assert 'result' in self.client.request('create', bot=self.name(n), workspace=str(self.store.parent))
        for n in range(HEAVY):
            assert 'result' in self.client.request('create', bot=self.heavy(n), workspace=str(self.store.parent))
        return round((time.monotonic() - before) * 1000, 1)

    def growth_batch(self, start, count):
        # Flip shapes every full heavy-bot rotation. Bot choice and shape
        # must not share the same parity: each heavy bot gets both shapes.
        return [(self.pick(k), SHELL if (k // (4 * HEAVY) + k % 2) % 2 else TEXT)
                for k in range(start, start + count)]

    def run_batch(self, submissions):
        """Submit (bot, prompt) pairs with CONCURRENCY in flight, at most one per
        bot so no turn waits behind its own bot; return latencies in ms."""
        latencies, pending, queue = [], [], list(submissions)
        client = self.client
        # Unconsumed notifications would otherwise pile up in the client and
        # slow every later receive; the screen reads only what it waits for.
        client.saved.clear()
        while queue or pending:
            busy = {bot for bot, _, _ in pending}
            while len(pending) < CONCURRENCY:
                ready = next((i for i, (bot, _) in enumerate(queue) if bot not in busy), None)
                if ready is None:
                    break
                bot, prompt = queue.pop(ready)
                busy.add(bot)
                before = time.monotonic()
                response = client.request('submit', bot=bot, request_id=self.request_id(bot), prompt=prompt)
                assert 'result' in response, response
                if prompt == SHELL:
                    self.shell_turns[bot] = self.shell_turns.get(bot, 0) + 1
                pending.append((bot, response['result']['turn'], before))
            bot, turn, before = pending.pop(0)
            event = client.receive(lambda m, t=turn: m.get('event') == 'turn_finished' and m.get('turn') == t,
                                   timeout=120)
            assert event['data']['status'] == 'completed', event
            latencies.append((event['_received_at'] - before) * 1000)
        return latencies

    def grow(self, target_bytes):
        """Submit mixed turns until the store passes the target. Returns growth stats."""
        self.sampler.phase = f'grow-{target_bytes}'
        started = time.monotonic()
        turns, k = 0, 0
        latencies = []
        mix = {'heavy_text': 0, 'heavy_shell': 0, 'light_text': 0, 'light_shell': 0}
        while store_bytes(self.store) < target_bytes:
            batch = self.growth_batch(k, CONCURRENCY * 4)
            k += len(batch)
            for bot, prompt in batch:
                mix[('heavy_' if bot.startswith('h') else 'light_') +
                    ('shell' if prompt == SHELL else 'text')] += 1
            latencies.extend(self.run_batch(batch))
            turns += len(batch)
        elapsed = time.monotonic() - started
        return {'turns': turns, 'seconds': round(elapsed, 1), 'turns_per_second': round(turns / elapsed, 1),
                'turn_ms': percentiles(latencies), 'mix': mix}

    def call(self, op, timeout=600, **params):
        """A request that may take longer than the client's default deadline."""
        client = self.client
        client.next_id += 1
        client.process.stdin.write(json.dumps({'id': client.next_id, 'op': op, **params}) + '\n')
        client.process.stdin.flush()
        return client.receive(lambda m, i=client.next_id: m.get('id') == i, timeout=timeout)

    def timed(self, op, **params):
        before = time.monotonic()
        response = self.call(op, **params)
        assert 'result' in response, response
        return response['result'], round((time.monotonic() - before) * 1000, 2)

    def ops(self):
        stats = self.client.request('stats')['result']['store']['operations']
        return {name: (o['count'], o['ran_ms'], o['queued_ms'], o['slowest_ms']) for name, o in stats.items()}

    def measured(self, work):
        """Run `work` and report, per storage operation, what it cost during it."""
        before = self.ops()
        result = work()
        after = self.ops()
        delta = {}
        for name, (count, ran, queued, slowest) in after.items():
            c0, r0, q0, _ = before.get(name, (0, 0, 0, 0))
            if count > c0:
                delta[name] = {'count': count - c0, 'mean_ran_ms': round((ran - r0) / (count - c0), 3),
                               'queued_ms': queued - q0, 'slowest_ms_lifetime': slowest}
        return result, delta

    def page_bots(self):
        after, pages, seen = None, 0, 0
        while True:
            page = self.client.request('bots', after=after, limit=256)['result']
            pages, seen, after = pages + 1, seen + len(page['bots']), page['next_after']
            if after is None:
                return {'pages': pages, 'bots': seen}

    def page_turns(self, bot):
        cursor, pages, seen = 0, 0, 0
        while True:
            page = self.client.request('turns', bot=bot, after=cursor, limit=64)['result']
            if not page['turns']:
                return {'pages': pages, 'turns': seen}
            pages, seen, cursor = pages + 1, seen + len(page['turns']), page['turns'][-1]['turn']

    def twice(self, work):
        """Consecutive passes on the live store, with uncontrolled cache state."""
        out = []
        for _ in range(2):
            before = time.monotonic()
            result = work()
            out.append({'ms': round((time.monotonic() - before) * 1000, 1), **result})
        return {'first': out[0], 'repeat': out[1]}

    def crash_restart(self, timeout=30):
        client = self.client
        # All earlier work has finished. Only this new batch can advance the
        # counter, and hold: cannot finish while the release event is clear.
        before = self.server.requests
        held = []
        for i in range(CONCURRENCY):
            bot = self.name(i)
            response = client.request('submit', bot=bot, request_id=self.request_id(bot), prompt='hold:')
            assert 'result' in response, response
            held.append((bot, response['result']['turn']))
        deadline = time.monotonic() + timeout
        while self.server.requests - before < len(held):
            if time.monotonic() >= deadline:
                raise TimeoutError(f'held requests started: {self.server.requests - before}/{len(held)}')
            time.sleep(.02)
        started = self.server.requests - before
        client.close(kill=True)
        self.server.release.set()
        ready_ms = self.connect()
        self.server.release.clear()
        for bot, turn in held:
            row = self.client.request('turns', bot=bot, after=turn - 1, limit=1)['result']['turns'][0]
            assert row['turn'] == turn and row['status'] == 'interrupted', row
        return {'ready_ms': ready_ms, 'requests_started': started, 'recovered_turns': len(held)}

    def checkpoint(self, label):
        client = self.client
        client.saved.clear()
        self.sampler.phase = f'measure-{label}'
        out = {'store_mib': round(store_bytes(self.store) / 2**20, 1)}
        # Latency by history length and turn shape, 32 turns each, with the
        # storage worker's own accounting of that batch.
        light = [self.name(self.bots - 1 - i) for i in range(CONCURRENCY)]
        heavy = [self.heavy(i % HEAVY) for i in range(CONCURRENCY)]
        for name, bots, prompt in (('light_text', light, TEXT), ('heavy_text', heavy, TEXT),
                                   ('light_shell', light, SHELL), ('heavy_shell', heavy, SHELL)):
            requests, request_bytes = self.server.requests, self.server.request_bytes
            latencies, delta = self.measured(lambda: self.run_batch([(b, prompt) for b in bots]))
            out[f'submit_{name}_ms'] = percentiles(latencies)
            out[f'submit_{name}_store'] = delta
            out[f'submit_{name}_concurrency_bound'] = min(CONCURRENCY, len(set(bots)))
            calls = self.server.requests - requests
            out[f'submit_{name}_provider'] = {
                'requests': calls,
                'mean_request_body_bytes': (self.server.request_bytes - request_bytes) // max(calls, 1)}
        # Consecutive paging passes; neither establishes a cold cache.
        out['bots_paging'] = self.twice(self.page_bots)
        out['turns_paging'] = self.twice(lambda: self.page_turns(self.heavy(0)))
        out['events_page'] = self.twice(lambda: {'events': len(
            client.request('events', bot=self.heavy(0), after=0, limit=256)['result']['events'])})
        # Fork, delete the fork, delete one heavy bot with everything it owns.
        head = client.request('resume', bot=self.heavy(1))['result']['head']
        _, out['fork_ms'] = self.timed('fork', source=self.heavy(1), checkpoint=head, bot=f'fork-{label}',
                                       workspace=str(self.store.parent))
        _, out['delete_fork_ms'] = self.timed('delete', bot=f'fork-{label}')
        victim = self.heavy((HEAVY - 1 - self.deleted) % HEAVY)
        turns_owned = self.turns.get(victim, 0)
        self.deleted += 1
        freed, ms = self.timed('delete', bot=victim)
        out['delete_heavy'] = {'ms': ms, 'turns': turns_owned,
                               'shell_turns': self.shell_turns.get(victim, 0), 'freed': freed}
        assert 'result' in client.request('create', bot=victim, workspace=str(self.store.parent))
        self.turns[victim] = 0
        self.shell_turns[victim] = 0
        # Storage worker view of everything since the daemon started.
        stats = client.request('stats')['result']
        store = stats['store']
        out['store_ops'] = {name: {'count': o['count'], 'mean_ran_ms': round(o['ran_ms'] / max(o['count'], 1), 3),
                                   'slowest_ms': o['slowest_ms'], 'queued_ms': o['queued_ms']}
                            for name, o in sorted(store['operations'].items())
                            if name in ('begin', 'start', 'append', 'tool_finish', 'finish', 'events', 'turns',
                                        'list', 'fork', 'delete_bot', 'create', 'window', 'context', 'resume')}
        out['store_bytes_reported'] = store.get('bytes')
        # Clean restart: shutdown, reopen the big store.
        self.sampler.phase = f'restart-{label}'
        self.call('shutdown')
        client.close()
        out['clean_restart_ready_ms'] = self.connect()
        out['crash_restart'] = self.crash_restart()
        client = self.client
        # The schema-21 migration replayed against this store.
        self.call('shutdown')
        client.close()
        with sqlite3.connect(self.store) as db:
            db.executescript("DROP INDEX bots_id; ALTER TABLE bots DROP COLUMN id; DROP TABLE bot_sequence;"
                             " PRAGMA user_version=20;")
        out['migration_restart_ready_ms'] = self.connect()
        assert self.client.request('resume', bot=self.heavy(0))['result']['id'] > 0
        # Copy values so later phases cannot rewrite an earlier checkpoint.
        out['peaks'] = {k: dict(v) for k, v in list(self.sampler.peaks.items())}
        self.sampler.phase = 'idle'
        return out


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--binary', type=Path, default=Path('.local/target/release/agent'))
    parser.add_argument('--out', required=True, type=Path)
    parser.add_argument('--sizes-gb', type=float, nargs='+', default=[1, 10])
    parser.add_argument('--bots', type=int, default=4096)
    parser.add_argument('--keep', action='store_true')
    args = parser.parse_args()
    if args.bots < CONCURRENCY:
        parser.error(f'--bots must be at least {CONCURRENCY}')
    root = Path(__file__).resolve().parents[1]
    out = args.out.resolve()
    if Path.cwd() != root or not out.is_relative_to(root / '.local'):
        parser.error('run from repository root with an output under .local')
    out.mkdir(parents=True, exist_ok=False)
    store = out / 'state.sqlite'
    server, url = start()
    sampler = Sampler(store)
    screen = Screen(args.binary.resolve(), store, url, server, args.bots, sampler)
    record = {'created_at': datetime.now(timezone.utc).isoformat(), 'binary': str(args.binary), 'bots': args.bots,
              'concurrency': CONCURRENCY, 'heavy_bots': HEAVY, 'sqlite': sqlite3.sqlite_version,
              'disk_free_gib_before': round(shutil.disk_usage(out).free / 2**30, 1), 'checkpoints': []}
    with args.binary.open('rb') as binary:
        record['binary_sha256'] = hashlib.file_digest(binary, 'sha256').hexdigest()
    try:
        record['first_ready_ms'] = screen.connect()
        record['create_all_ms'] = screen.create_all()
        grown = 0
        for size in args.sizes_gb:
            target = int(size * 10**9)
            growth = screen.grow(target)
            grown += growth['turns']
            entry = {'target_gb': size, 'growth': growth, 'turns_total': grown}
            entry.update(screen.checkpoint(f'{size:g}gb'))
            record['checkpoints'].append(entry)
            print(json.dumps(entry), flush=True)
            (out / 'result.json').write_text(json.dumps(record, indent=1))
    finally:
        sampler.stop.set()
        sampler.thread.join()
        if screen.client:
            screen.client.close(kill=True)
        server.release.set()
        server.shutdown()
        server.server_close()
        record['disk_free_gib_after'] = round(shutil.disk_usage(out).free / 2**30, 1)
        (out / 'result.json').write_text(json.dumps(record, indent=1))
        if not args.keep:
            for p in out.glob('state.sqlite*'):
                p.unlink()


if __name__ == '__main__':
    main()
