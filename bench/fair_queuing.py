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
import threading
import time
from pathlib import Path

from bench.lookout_scale import Bench, Conn


def percentile(values, p):
    ordered = sorted(values)
    return ordered[min(len(ordered) - 1, int(len(ordered) * p))] if ordered else None


class Probe:
    """Times one `inspect` at a time on its own session until stopped."""

    def __init__(self, path, bot, gap_s):
        self.conn, self.bot, self.gap = Conn(path), bot, gap_s
        self.times, self.stop = [], threading.Event()
        self.thread = threading.Thread(target=self.run, daemon=True)
        self.thread.start()

    def run(self):
        while not self.stop.is_set():
            started = time.monotonic()
            reply = self.conn.request('inspect', bot=self.bot)
            assert 'result' in reply, reply
            self.times.append((time.monotonic() - started) * 1000)
            time.sleep(self.gap)

    def done(self):
        self.stop.set()
        self.thread.join(60)
        ms = self.times
        return {'probes': len(ms), 'p50_ms': round(percentile(ms, .5), 1),
                'p95_ms': round(percentile(ms, .95), 1), 'max_ms': round(max(ms), 1)}


def floods(bench, count):
    bots = bench.tasks()
    return {
        'inspect': [('inspect', {'bot': bots[i % len(bots)]}) for i in range(count)],
        'events': [('events', {'bot': bots[i % len(bots)], 'after': 0, 'limit': 256})
                   for i in range(count)],
        'bots': [('bots', {'limit': 64}) for _ in range(count // 5)],
        'create_delete': [pair for i in range(count // 2) for pair in (
            ('create', {'bot': f'flood{i}', 'workspace': str(bench.out),
                        'model': 'openai/synthetic-model', 'instructions': 'x', 'tools': []}),
            ('delete', {'bot': f'flood{i}'}))],
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
    parser.add_argument('--out', required=True)
    args = parser.parse_args()
    if args.model:
        from bench.lookout_scale import model_main
        return model_main(args.model, args.judge_ms)
    bench = Bench(args)
    results = {'bot_count': args.projects * (args.tasks + 1), 'flood': args.flood}
    try:
        bench.setup()
        probe = Probe(bench.socket, bench.tasks()[0], args.gap_ms / 1000)
        time.sleep(args.quiet_s)
        results['alone'] = probe.done()
        flood = Conn(bench.socket)
        for kind, requests in floods(bench, args.flood).items():
            if kind not in args.kinds.split(','):
                continue
            probe = Probe(bench.socket, bench.tasks()[0], args.gap_ms / 1000)
            time.sleep(.2)
            started = time.monotonic()
            replies = flood.many(requests, timeout=600)
            elapsed = time.monotonic() - started
            bad = [r for r in replies if 'error' in r]
            results[kind] = {'requests': len(requests), 'flood_s': round(elapsed, 2),
                             'per_s': round(len(requests) / elapsed), 'errors': len(bad),
                             'probe': probe.done()}
            print(kind, json.dumps(results[kind]), flush=True)
        print(json.dumps(results, indent=1))
        Path(args.out, 'results.json').write_text(json.dumps(results, indent=1))
    finally:
        bench.close()


if __name__ == '__main__':
    main()
