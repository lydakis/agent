"""Store reads under one fixed trace: memory, CPU and latency at operating points.

Synthetic provider, no spend. One socket daemon per run. What the daemon is
asked depends only on the arguments: a fixed number of turns per bot, each
prompt a pure function of its bot and turn, so two binaries, or two reader
settings, replay the same trace and their numbers compare (a run of fixed
length would compare unequal work). Each run:

1. Seeds `bots` stored bots with one short turn each.
2. Opens a Sky: one `follow` of every bot from cursor 0, as Sonny's Sky
   reads the past, then kept live on the kinds it animates.
3. Burst: `live` bots each take `turns` turns back to back, `prompt_bytes`
   prompts in a context bounded at `context_bytes`, so every model call
   past the first window lists the turns it omits. Beside them, `observers`
   clients loop short reads (a live bot's newest turns, as /status and
   lookouts do) and `searchers` loop a history search of one bot. Halfway,
   a second Sky opens.
4. Idle for `idle_s` seconds: what the daemon keeps once the burst is over.
5. A second burst, the same as the first.
6. A slow reader: one client replays every event from cursor 0 while one
   short reader times its reads beside it; then the short reader alone.

Reported per run: daemon RSS (peak and after each phase), threads, CPU (whole
process, so threads that exited still count), turn and short-read latency,
the second Sky's open time, and store operations. Presets name three
operating points: `today` (100 bots, 10 live), `heavy` (50 live) and `team`
(2,000 bots, 200 live, more observers and searches).
"""
import argparse
from datetime import datetime, timezone
import hashlib
import json
import platform
import statistics
import threading
import time
from pathlib import Path

import psutil

from .socket_client import Connection, SocketClient
from .synthetic_model import start
from .targets import file_hash

PRESETS = {
    'today': dict(bots=100, live=10, observers=1, searchers=0),
    'heavy': dict(bots=100, live=50, observers=2, searchers=1),
    'team': dict(bots=2000, live=200, observers=8, searchers=2),
}
# What Sonny's Sky follows: the past it replays, then what it animates.
PAST = ['accepted', 'steered', 'turn_finished']
LIVE = ['created', 'forked', 'deleted', 'queued', 'accepted', 'steered', 'tool_started',
        'tool_completed', 'approval_requested', 'approval_asked', 'step_decided', 'turn_resumed',
        'turn_finished', 'configured']


def prompt(bot, turn, size):
    """The trace's prompt for `bot`'s `turn`: its words, then filler that
    differs per turn, so no two prompts share a page."""
    head = f'{bot} turn {turn}: what changed since the last one?\n'
    seed = hashlib.sha256(head.encode()).hexdigest()
    return head + (seed * (size // len(seed) + 1))[:max(0, size - len(head))]


def percentile(values, q):
    if not values:
        return None
    values = sorted(values)
    return round(values[min(len(values) - 1, int(q * len(values)))] * 1000, 2)


class Sampler:
    """The daemon's RSS and threads every 50 ms, and their peaks."""

    def __init__(self, pid):
        self.process = psutil.Process(pid)
        self.peak_rss = self.peak_threads = 0
        self.stop = threading.Event()
        self.thread = threading.Thread(target=self.run, daemon=True)
        self.thread.start()

    def now(self):
        return dict(rss_mib=round(self.process.memory_info().rss / 2**20, 2),
                    threads=self.process.num_threads(),
                    cpu_s=round(sum(self.process.cpu_times()[:2]), 2))

    def run(self):
        while not self.stop.wait(.05):
            try:
                self.peak_rss = max(self.peak_rss, self.process.memory_info().rss)
                self.peak_threads = max(self.peak_threads, self.process.num_threads())
            except psutil.Error:
                return


class Sky:
    """Sonny's Sky as the daemon sees it: the past from cursor 0, then live."""

    def __init__(self, socket_path):
        began = time.monotonic()
        self.connection = Connection(socket_path, retain_durable=False)
        assert 'result' in self.connection.request('follow', bot='*', after=0, kinds=PAST)
        live = self.connection.receive(lambda e: e.get('event') == 'follow_live', timeout=600)
        assert 'result' in self.connection.request('follow', bot='*', after=live['cursor'], kinds=LIVE)
        self.connection.receive(lambda e: e.get('event') == 'follow_live', timeout=600)
        self.open_ms = round((time.monotonic() - began) * 1000, 1)
        # Events keep coming; nothing reads them here, so drop them as they come.
        self.connection.record = lambda event: None


def loop_reads(socket_path, ask, done, latencies):
    connection = Connection(socket_path, retain_durable=False)
    n = 0
    try:
        while not done.is_set():
            op, params = ask(n)
            began = time.monotonic()
            reply = connection.request(op, **params)
            latencies.append(time.monotonic() - began)
            assert 'result' in reply, reply
            n += 1
    finally:
        connection.close()


def burst(daemon, finishes, live, turns, size, observers, searchers, on_half):
    """Every live bot takes `turns` turns, each submitted when its last ends."""
    done = threading.Event()
    short, searches = [], []
    readers = [threading.Thread(target=loop_reads, daemon=True, args=(
        daemon.socket_path, lambda n, k=k: ('turns', dict(bot=f'b{(n * 7 + k) % live}', newest=True, limit=20)),
        done, short)) for k in range(observers)]
    readers += [threading.Thread(target=loop_reads, daemon=True, args=(
        daemon.socket_path, lambda n, k=k: ('history_search', dict(bot=f'b{(n * 13 + k) % live}', query='changed')),
        done, searches)) for k in range(searchers)]
    for reader in readers:
        reader.start()
    taken = {f'b{n}': 0 for n in range(live)}
    sent, latencies, total = {}, [], live * turns
    finished, halved = 0, False
    control = daemon.control

    def submit(bot):
        n = taken[bot]
        reply = control.request('submit', bot=bot, request_id=f'{bot}-{n}-{size}', prompt=prompt(bot, n, size))
        assert 'result' in reply, reply
        sent[reply['result']['turn']] = time.monotonic()
        taken[bot] = n + 1

    began = time.monotonic()
    for bot in taken:
        submit(bot)
    while finished < total:
        event = finishes.receive(lambda e: e.get('event') == 'turn_finished' and e.get('turn') in sent, timeout=300)
        assert event['data']['status'] == 'completed', event
        latencies.append(time.monotonic() - sent.pop(event['turn']))
        finished += 1
        if taken[event['bot']] < turns:
            submit(event['bot'])
        if not halved and finished * 2 >= total:
            halved = True
            on_half()
    elapsed = time.monotonic() - began
    done.set()
    for reader in readers:
        reader.join(timeout=60)
    return dict(seconds=round(elapsed, 2), turns=total, turn_p50_ms=percentile(latencies, .5),
                turn_p95_ms=percentile(latencies, .95), short_reads=len(short),
                short_p50_ms=percentile(short, .5), short_p95_ms=percentile(short, .95),
                searches=len(searches), search_p95_ms=percentile(searches, .95))


def slow_reader(daemon, live, rounds=3, alone=200):
    """Short reads beside replays of every event, then alone."""
    done, short = threading.Event(), []
    reader = threading.Thread(target=loop_reads, daemon=True, args=(
        daemon.socket_path, lambda n: ('turns', dict(bot=f'b{n % live}', newest=True, limit=20)), done, short))
    reader.start()
    replays = []
    for _ in range(rounds):
        began = time.monotonic()
        replay = Connection(daemon.socket_path, retain_durable=False)
        assert 'result' in replay.request('follow', bot='*', after=0)
        replay.receive(lambda e: e.get('event') == 'follow_live', timeout=600)
        replays.append(time.monotonic() - began)
        replay.close()
    done.set()
    reader.join(timeout=60)
    beside = list(short)
    connection = Connection(daemon.socket_path, retain_durable=False)
    lone = []
    for n in range(alone):
        began = time.monotonic()
        assert 'result' in connection.request('turns', bot=f'b{n % live}', newest=True, limit=20)
        lone.append(time.monotonic() - began)
    connection.close()
    return dict(replay_ms=[round(r * 1000, 1) for r in replays], short_reads_beside=len(beside),
                short_p50_beside_ms=percentile(beside, .5), short_p95_beside_ms=percentile(beside, .95),
                short_p50_alone_ms=percentile(lone, .5), short_p95_alone_ms=percentile(lone, .95))


def run(binary, out, *, bots, live, observers, searchers, turns, prompt_bytes, context_bytes, idle_s, readers):
    model, url = start()
    settings = dict(context_bytes=context_bytes, context_items=4096)
    extra = ['--max-readers', str(readers)] if readers else []
    daemon = SocketClient(binary, out / 'state.sqlite', url, 'echo', extra=extra, settings=settings)
    try:
        sampler = Sampler(daemon.process.pid)
        row = dict(start=sampler.now())
        for first in range(0, bots, 64):
            for n in range(first, min(bots, first + 64)):
                assert 'result' in daemon.control.request('create', bot=f'b{n}', workspace=str(out))
        finishes = Connection(daemon.socket_path, retain_durable=False)
        assert 'result' in finishes.request('follow', bot='*', after=0, kinds=['turn_finished'])
        finishes.receive(lambda e: e.get('event') == 'follow_live', timeout=60)
        seeded = {}
        for n in range(bots):
            reply = daemon.control.request('submit', bot=f'b{n}', request_id='seed', prompt=f'b{n} hello')
            seeded[reply['result']['turn']] = True
        for _ in range(bots):
            finishes.receive(lambda e: e.get('event') == 'turn_finished' and e.get('turn') in seeded, timeout=300)
        row['seeded'] = sampler.now()
        sky = Sky(daemon.socket_path)
        row['sky_open_ms'] = sky.open_ms
        second = {}

        def open_second():
            # On its own thread, as a person would open it: the burst goes on.
            thread = threading.Thread(target=lambda: second.setdefault('sky', Sky(daemon.socket_path)), daemon=True)
            thread.start()
            second['thread'] = thread

        row['burst'] = burst(daemon, finishes, live, turns, prompt_bytes, observers, searchers, open_second)
        second['thread'].join(timeout=600)
        row['second_sky_open_ms'] = second['sky'].open_ms
        row['after_burst'] = sampler.now()
        time.sleep(idle_s)
        row['idle'] = sampler.now()
        row['burst_again'] = burst(daemon, finishes, live, turns, prompt_bytes + 1, observers, searchers, lambda: None)
        row['after_burst_again'] = sampler.now()
        row['slow_reader'] = slow_reader(daemon, live)
        row['end'] = sampler.now()
        row['peak'] = dict(rss_mib=round(sampler.peak_rss / 2**20, 2), threads=sampler.peak_threads)
        operations = daemon.control.request('stats')['result']['store']['operations']
        row['store_ops'] = {name: dict(count=o['count'], ran_ms=o['ran_ms']) for name, o in operations.items()}
        row['store_bytes'] = sum(f.stat().st_size for f in out.glob('state.sqlite*'))
        sampler.stop.set()
        for connection in (sky.connection, second['sky'].connection, finishes):
            connection.close()
        return row
    finally:
        daemon.close()
        model.shutdown()
        model.server_close()


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument('--binary', type=Path, default=Path('.local/target/release/agent'))
    parser.add_argument('--out', type=Path, required=True)
    parser.add_argument('--point', choices=sorted(PRESETS), default='today')
    parser.add_argument('--bots', type=int, help="stored bots, in place of the point's")
    parser.add_argument('--live', type=int, help="live bots, in place of the point's")
    parser.add_argument('--turns', type=int, default=12, help='turns per live bot in each burst')
    parser.add_argument('--prompt-bytes', type=int, default=32 * 1024)
    parser.add_argument('--context-bytes', type=int, default=128 * 1024)
    parser.add_argument('--idle-s', type=float, default=15)
    parser.add_argument('--readers', type=int, default=0, help='--max-readers for the daemon; 0 leaves its default')
    parser.add_argument('--repeat', type=int, default=1)
    args = parser.parse_args()
    args.out.mkdir(parents=True, exist_ok=False)
    binary = args.binary.resolve()
    point = dict(PRESETS[args.point])
    point.update({k: v for k, v in (('bots', args.bots), ('live', args.live)) if v})
    if point['live'] > point['bots']:
        parser.error('live bots are some of the stored ones')
    shape = dict(point, turns=args.turns, prompt_bytes=args.prompt_bytes,
                 context_bytes=args.context_bytes, idle_s=args.idle_s, readers=args.readers)
    result = dict(schema='store_load_v1', point=args.point, shape=shape, binary_sha256=file_hash(binary),
                  observer_sha256=file_hash(Path(__file__)),
                  observed_at=datetime.now(timezone.utc).isoformat(),
                  host=dict(system=platform.system(), architecture=platform.machine(),
                            cpus=psutil.cpu_count(), python=platform.python_version()),
                  runs=[])
    for n in range(args.repeat):
        directory = args.out / str(n)
        directory.mkdir()
        row = run(binary, directory, **shape)
        result['runs'].append(row)
        (args.out / 'result.json').write_text(json.dumps(result, indent=2) + '\n')
        print(json.dumps({k: v for k, v in row.items() if k != 'store_ops'}), flush=True)
    if len(result['runs']) > 1:
        print(json.dumps(dict(median_peak_rss_mib=statistics.median(r['peak']['rss_mib'] for r in result['runs']))))


if __name__ == '__main__':
    main()
