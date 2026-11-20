"""A reviewer started over a backlog, behind a slow store.

G bots each make one gated shell call while no reviewer serves `auto`, as
they would while the reviewer restarts. Then `sonny reviewer` starts behind
a store whose every reply takes D ms ([`bench.slow_socket`]), and is timed
until it has judged every call:

- serving_s: from its start until it says it serves;
- first_verdict_s: until its first round's verdict;
- verdict_s_p50 / p99 / max: until each call's verdict;
- peak_rss_mib: the reviewer's own, sampled every quarter second;
- lost: whether it let its lease go, and the event it ended on.

One daemon on the synthetic model of `bench.lookout_scale`, its judge
answering after J ms. Run from the repo root after
`cargo build --release --workspace --bins`:
    python3 -m bench.reviewer_restart --gated 1000 --store-ms 250 --out /tmp/x
`--sonny` measures a sonny binary built from another revision.
"""
import argparse
import json
import subprocess
import threading
import time
from pathlib import Path

from bench.lookout_scale import Bench, Sampler
from bench.slow_socket import SlowSocket


def pct(values, q):
    values = sorted(values)
    return round(values[min(len(values) - 1, int(q * len(values)))], 2) if values else None


def restart(bench, gated, store_ms, limit_s):
    names = [f'g{i}' for i in range(gated)]
    bench.create(names, tools=('echo', 'shell'), approve=['shell'], approver='auto',
                 approve_expire_ms=600_000)
    bench.control.many([('submit', {'bot': name, 'request_id': 'gate', 'prompt': 'shell:true',
                                    'delivery': 'reject'}) for name in names])
    # Every call waits on the gate before the reviewer starts.
    deadline = time.monotonic() + 300
    while waiting(bench) < gated:
        if time.monotonic() > deadline:
            raise RuntimeError(f'{waiting(bench)} of {gated} calls waiting')
        time.sleep(.25)
    slow = SlowSocket(bench.socket, bench.tmp / 'slow.sock', store_ms / 1000)
    log = open(bench.out / 'reviewer.err', 'w')
    started = time.monotonic()
    process = subprocess.Popen(
        [str(bench.sonny), 'reviewer', '--store', str(bench.store), '--socket', str(slow.path),
         '--model', 'openai/synthetic-model'],
        stdin=subprocess.DEVNULL, stdout=subprocess.PIPE, stderr=log, env=bench.env, text=True,
        start_new_session=True)
    sampler = Sampler(process.pid)
    lines = []

    def read():
        for line in process.stdout:
            lines.append((time.monotonic() - started, json.loads(line)))
    reader = threading.Thread(target=read, daemon=True)
    reader.start()
    verdicts = lambda: [(at, c) for at, l in list(lines) if l.get('event') == 'reviewed' for c in l['calls']]
    ended = lambda: next((l for _, l in list(lines)
                          if l.get('event') not in ('serving', 'reviewed', 'review_failed')), None)
    while len(verdicts()) < gated and process.poll() is None and time.monotonic() - started < limit_s:
        time.sleep(.1)
    took = time.monotonic() - started
    stats = sampler.done()
    alive = process.poll() is None
    process.terminate()
    process.wait(10)
    reader.join(5)
    slow.close()
    (bench.out / 'reviewer.jsonl').write_text(''.join(json.dumps({'at_s': round(at, 3), **l}) + '\n'
                                                      for at, l in lines))
    at = [at for at, _ in verdicts()]
    serving = next((at for at, l in lines if l.get('event') == 'serving'), None)
    end = ended()
    return {'gated': gated, 'store_ms': store_ms, 'judge_ms_model': bench.args.judge_ms,
            'decided': len(at), 'allowed': sum(c['decision'] == 'allow' for _, c in verdicts()),
            'seconds': round(took, 2),
            'serving_s': round(serving, 2) if serving is not None else None,
            'first_verdict_s': round(min(at), 2) if at else None,
            'verdict_s_p50': pct(at, .5), 'verdict_s_p99': pct(at, .99),
            'verdict_s_max': round(max(at), 2) if at else None,
            'peak_rss_mib': stats['peak_rss_mib'], 'cpu_s': stats['cpu_s'],
            'renewals': slow.renewals, 'alive': alive,
            'lost': end is not None, 'ended_on': end.get('event') if end else None}


def waiting(bench):
    count, after = 0, 0
    while True:
        page = bench.control.request('approvals', tag='auto', after=after, limit=256)['result']
        count += len(page['approvals'])
        if page['next_after'] is None:
            return count
        after = page['next_after']


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--gated', type=int, default=1000)
    parser.add_argument('--store-ms', type=int, default=250, help='how long every store reply takes')
    parser.add_argument('--judge-ms', type=int, default=0)
    parser.add_argument('--limit-s', type=float, default=300, help='how long to wait for every verdict')
    parser.add_argument('--out', required=True)
    parser.add_argument('--sonny', help='the sonny binary to measure, as one built from another revision')
    parser.add_argument('--agent', help='the daemon binary')
    args = parser.parse_args()
    args.projects = args.tasks = 0
    bench = Bench(args)
    try:
        result = restart(bench, args.gated, args.store_ms, args.limit_s)
        print(json.dumps(result, indent=1))
        (Path(args.out) / 'results.json').write_text(json.dumps(result, indent=1))
    finally:
        bench.close()


if __name__ == '__main__':
    main()
