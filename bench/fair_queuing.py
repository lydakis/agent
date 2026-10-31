"""How long one client waits while another floods the daemon.

One socket daemon with B bots. A probe session sends one cheap request
(`inspect`) every few milliseconds and times each answer; a second
session pipelines thousands of requests of one kind at once. The probe's
latency while the flood runs, beside its latency alone, is how much one
client's burst costs every other client: Sonny's window, a lead, the
approver.

Run from the repo root after `cargo build --release --bin agent`:
    python3 -m bench.fair_queuing --out /tmp/fq
"""
import argparse
import json
import multiprocessing
import queue
import threading
import time
from pathlib import Path

from bench.lookout_scale import Bench, Conn


def percentile(values, p):
    ordered = sorted(values)
    return ordered[min(len(ordered) - 1, int(len(ordered) * p))] if ordered else None


def probing(path, ask, gap_s, stop, ready, times):
    conn = Conn(path)
    ready.set()
    while not stop.is_set():
        started = time.monotonic()
        reply = conn.request(ask[0], **ask[1])
        assert 'result' in reply, reply
        times.put((time.monotonic() - started) * 1000)
        time.sleep(gap_s)


class Probe:
    """Times one request at a time on its own session until stopped: an
    `inspect`, or with `listing` a page of `bots`. It runs in a process of
    its own, so parsing the flood's replies here never holds it up."""

    def __init__(self, path, bot, gap_s, listing=False):
        ask = ('bots', {'limit': 64}) if listing else ('inspect', {'bot': bot})
        spawn = multiprocessing.get_context('spawn')
        self.stop, self.times, ready = spawn.Event(), spawn.Queue(), spawn.Event()
        self.process = spawn.Process(target=probing, daemon=True,
                                     args=(str(path), ask, gap_s, self.stop, ready, self.times))
        self.process.start()
        ready.wait(30)

    def done(self):
        self.stop.set()
        ms = []
        while self.process.is_alive() or not self.times.empty():
            try:
                ms.append(self.times.get(timeout=.2))
            except queue.Empty:
                pass
        self.process.join(60)
        return {'probes': len(ms), 'p50_ms': round(percentile(ms, .5), 1),
                'p95_ms': round(percentile(ms, .95), 1), 'max_ms': round(max(ms), 1)}


def deepen(bench, turns, size):
    """Give every task `turns` finished turns, each prompt `size` bytes, so
    reads over histories have something to read."""
    words = ('alpha bravo charlie delta echo foxtrot golf hotel india juliet '
             'kilo lima mike november oscar papa quebec romeo sierra tango ')
    prompt = (words * (size // len(words) + 1))[:size]
    for turn in range(turns):
        replies = bench.control.many([('submit', {
            'bot': bot, 'request_id': f'deep{turn}', 'prompt': prompt, 'delivery': 'queue'})
            for bot in bench.tasks()], timeout=600)
        bad = [r for r in replies if 'error' in r]
        assert not bad, bad[:3]
        while bench.busy_tasks():
            time.sleep(.2)


def floods(bench, count, flooder=0):
    """Each flood's requests for one flooding connection; `flooder` keeps
    the bots one creates and deletes its own."""
    bots = bench.tasks()
    return {
        'inspect': [('inspect', {'bot': bots[i % len(bots)]}) for i in range(count)],
        'events': [('events', {'bot': bots[i % len(bots)], 'after': 0, 'limit': 256})
                   for i in range(count)],
        'bots': [('bots', {'limit': 64}) for _ in range(count // 5)],
        # Reads that decide nothing: whole histories, and a search across
        # every bot's.
        'history': [('history_nodes', {'bot': bots[i % len(bots)], 'limit': 400})
                    for i in range(count)],
        'search': [('history_search', {'bot': '*', 'query': 'Bench'})
                   for _ in range(count // 25)],
        'create_delete': [pair for i in range(count // 2) for pair in (
            ('create', {'bot': f'flood{flooder}-{i}', 'workspace': str(bench.out),
                        'model': 'openai/synthetic-model', 'instructions': 'x', 'tools': []}),
            ('delete', {'bot': f'flood{flooder}-{i}'}))],
    }


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--model')
    parser.add_argument('--judge-ms', type=int, default=0)
    parser.add_argument('--projects', type=int, default=10)
    parser.add_argument('--tasks', type=int, default=20)
    parser.add_argument('--flood', type=int, default=5000)
    parser.add_argument('--gap-ms', type=float, default=5)
    parser.add_argument('--quiet-s', type=float, default=3)
    parser.add_argument('--kinds', default='inspect,events,bots,create_delete')
    # Sessions flooding at once, each with the whole flood.
    parser.add_argument('--flooders', type=int, default=1)
    # The probe lists bots, a read itself, rather than inspecting one.
    parser.add_argument('--probe-bots', action='store_true')
    # The daemon's store reader connections; its default when left out.
    parser.add_argument('--readers', type=int)
    # Finished turns each task gets first, and each prompt's bytes.
    parser.add_argument('--history-turns', type=int, default=0)
    parser.add_argument('--history-bytes', type=int, default=16384)
    parser.add_argument('--out', required=True)
    args = parser.parse_args()
    if args.model:
        from bench.lookout_scale import model_main
        return model_main(args.model, args.judge_ms)
    bench = Bench(args)
    results = {'bot_count': args.projects * (args.tasks + 1), 'flood': args.flood}
    try:
        bench.setup()
        if args.history_turns:
            started = time.monotonic()
            deepen(bench, args.history_turns, args.history_bytes)
            results['deep'] = {'turns': args.history_turns, 'bytes': args.history_bytes,
                                  'setup_s': round(time.monotonic() - started, 1)}
        probe = Probe(bench.socket, bench.tasks()[0], args.gap_ms / 1000, args.probe_bots)
        time.sleep(args.quiet_s)
        results['alone'] = probe.done()
        flooders = [Conn(bench.socket) for _ in range(args.flooders)]
        plans = [floods(bench, args.flood, index) for index in range(len(flooders))]
        for kind in plans[0]:
            if kind not in args.kinds.split(','):
                continue
            probe = Probe(bench.socket, bench.tasks()[0], args.gap_ms / 1000, args.probe_bots)
            time.sleep(.2)
            replies = [None] * len(flooders)

            def flood(index):
                replies[index] = flooders[index].many(plans[index][kind], timeout=600)
            threads = [threading.Thread(target=flood, args=(i,)) for i in range(len(flooders))]
            started = time.monotonic()
            for thread in threads:
                thread.start()
            for thread in threads:
                thread.join()
            elapsed = time.monotonic() - started
            sent = sum(len(plan[kind]) for plan in plans)
            bad = [r for each in replies for r in each if 'error' in r]
            results[kind] = {'requests': sent, 'flood_s': round(elapsed, 2),
                             'per_s': round(sent / elapsed), 'errors': len(bad),
                             'probe': probe.done()}
            print(kind, json.dumps(results[kind]), flush=True)
        print(json.dumps(results, indent=1))
        Path(args.out, 'results.json').write_text(json.dumps(results, indent=1))
    finally:
        bench.close()


if __name__ == '__main__':
    main()
