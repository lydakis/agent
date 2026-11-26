"""What the person sees of a window while the store's disk is slow
(George, 11-25: typed letters came half a second late, then all at once,
and scrolling went choppy, while commits took 1 to 4 s).

One daemon on a copy of a `bench.ui_loads` store, every sync it makes held
by the preloaded shim (bench/held/slow_sync.c), as a busy disk holds a
commit: from once the window is open, so it opens as it did before the
disk slowed. The window's own changes to the file system wait too, each
WRITE_MS, as they do while another's sync holds the disk's journal. The
window's lead has a conversation tall enough to scroll.
While BUSY tasks run turns back to back (and the held tasks keep a turn
open), in a real pseudo-terminal:

  typing   key to character: each letter until the composer's line
           reads what was typed so far
  scroll   wheel notch to the conversation moving: each notch until the
           screen's rows differ from those before it, up a run of notches
           and back down

A key or notch that never shows counts as the five seconds waited. Builds
alternate on the same seed as in `bench.ui_loads`.

    cc -O2 -shared -fPIC -o .local/slow_sync.so bench/held/slow_sync.c -ldl
    python3 -m bench.held_window --build main=.local/main --build head=.local/head \\
        --shim .local/slow_sync.so --sync-ms 0,1000,3000 --layout --rounds 2
"""
import argparse
import json
import os
import signal
import statistics
import subprocess
import sys
import threading
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from bench.ends import connect, need  # noqa: E402
from bench.runtime_client import Client  # noqa: E402
from bench.socket_client import Connection  # noqa: E402
from bench.lookout_scale import cpu_seconds  # noqa: E402
from bench.ui_loads import Fleet, Term, bar, builds, composer_reads, evict, has  # noqa: E402

WORK = Path('/tmp/held-window')
# A request answered after a few held commits is still answered: the
# clients here wait for it as long as a person's window does.
Connection.receive = lambda self, predicate, timeout=120: Client.receive(self, predicate, timeout)
# A wheel notch up and down over the conversation, as a terminal sends it
# (SGR mouse reporting).
WHEEL_UP, WHEEL_DOWN = b'\x1b[<64;40;10M', b'\x1b[<65;40;10M'


class Held(Fleet):
    """A `bench.ui_loads` fleet whose daemon, once seeded, runs again with
    its syncs held as long as the run says, from the moment it says."""

    def slow(self, shim):
        """Serve again with every sync held as long as `self.sync_ms` says,
        read at each sync: none until `hold_syncs`."""
        self.control.request('shutdown', grace_ms=0)
        self.daemon.wait(120)
        self.sync_file = self.work / 'sync-ms'
        self.sync_file.write_text('0')
        self.env = {**self.env, 'LD_PRELOAD': str(Path(shim).resolve()),
                    'SLOW_SYNC_FILE': str(self.sync_file)}
        self.serve()

    def hold_syncs(self, ms, write_ms):
        self.sync_file.write_text(str(ms))
        self.write_file.write_text(str(write_ms))

    def held_window(self, shim, layout):
        """A window whose own changes to the file system (a file opened to
        write, a rename, a mkdir) wait as long as `write-ms` says: the
        disk is the person's, not only the daemon's."""
        self.write_file = self.work / 'write-ms'
        self.write_file.write_text('0')
        env = {**self.env, 'LD_PRELOAD': str(Path(shim).resolve()),
               'SLOW_SYNC_FILE': str(self.write_file), 'SLOW_WRITES': '1'}
        return Term([self.sonny, '--store', str(self.store), '--socket', str(self.sock),
                     '--agent', 'p0.lead', '--no-sound', *(() if layout else ('--no-layout',))],
                    env, str(self.folders[0]))


def summary(took):
    took = sorted(took)
    if not took:
        return {'count': 0}
    return {'count': len(took), 'p50_ms': round(statistics.median(took), 1),
            'p99_ms': took[min(len(took) - 1, int(len(took) * .99))], 'max_ms': took[-1],
            'over_100ms': sum(t > 100 for t in took), 'over_250ms': sum(t > 250 for t in took)}


def run(build, size, ms, args):
    work = WORK / f'{size}'
    fleet = Held(build['agent'], size, args.seeds, work)
    fleet.sonny = build['sonny']
    window = None
    try:
        # The lead's conversation, tall enough to scroll, on the fast disk:
        # each turn's message tells its rows from the others'.
        for k in range(args.tall):
            sent = need(fleet.control.request('submit', bot='p0.lead', request_id=f'tall-{k}',
                                              prompt=f'text:200 turn {k}'), 'submit')
            handle = f'p0.lead/{sent["message"]}'
            while need(fleet.control.request('wait', handles=[handle], timeout_ms=3_000),
                       'wait')['results'][handle].get('pending'):
                pass
        held = fleet.hold(args.own, args.spread)
        # Held turns are parked by the stop and go on at the start.
        fleet.slow(args.shim)
        own = sum(1 for bot in held if bot.startswith('p0.'))
        window = fleet.held_window(args.shim, args.layout)
        window.expect(composer=has('Message'), bar=bar(own)).wait(300)
        time.sleep(2)
        # The disk slows down once the window is open, as it did under one
        # in use.
        write_ms = args.write_ms if ms else 0
        fleet.hold_syncs(ms, write_ms)
        result = {'build': build['name'], 'size': size, 'sync_ms': ms, 'write_ms': write_ms,
                  **busy(fleet, window, held, args)}
        fleet.hold_syncs(0, 0)
        return result
    finally:
        if window:
            window.close()
        fleet.close()


def busy(fleet, window, held, args):
    """Typing and the wheel while BUSY open tasks no one holds run turns
    back to back."""
    stop, turns = threading.Event(), [0]
    free = [f'p{n}.t{t}' for n in range(fleet.projects - 1, 0, -1) for t in range(fleet.per - 1)
            if t % 3 and f'p{n}.t{t}' not in held]

    def work(bot):
        connection = connect(fleet.sock, fleet.daemon)
        k = 0
        while not stop.is_set():
            k += 1
            sent = connection.request('submit', bot=bot, request_id=f'{bot}-b{k}', prompt='delay:500')
            if 'result' not in sent:
                time.sleep(.1)
                continue
            handle = f'{bot}/{sent["result"]["message"]}'
            while not stop.is_set():
                found = need(connection.request('wait', handles=[handle], timeout_ms=2_000), 'wait')
                if not found['results'][handle].get('pending'):
                    break
            turns[0] += 1
        connection.close()
    workers = [threading.Thread(target=work, args=(bot,), daemon=True) for bot in free[:args.busy]]
    for worker in workers:
        worker.start()
    time.sleep(args.settle)
    cpu = (cpu_seconds(window.pid), cpu_seconds(fleet.daemon.pid))
    began, before = time.monotonic(), turns[0]
    typed_ms, scrolled_ms = [], []
    letters = 'abcdefghijklmnopqrst'
    for k in range(args.keys):
        at = k % len(letters)
        if at == 0 and k:
            os.write(window.fd, b'\x15')  # ctrl+u
            window.expect(empty=composer_reads('')).wait(10, need_all=False)
        typed = letters[:at + 1]
        got = window.press(typed[-1].encode(), shown=composer_reads(typed)).wait(5, need_all=False)['shown']
        typed_ms.append(got if got is not None else 5000)
        time.sleep(.05)
    os.write(window.fd, b'\x15')
    window.expect(empty=composer_reads('')).wait(10, need_all=False)
    for _ in range(args.scrolls):
        for notch in [WHEEL_UP] * args.notches + [WHEEL_DOWN] * args.notches:
            shown = window.shown()
            got = window.press(notch, moved=lambda lines, shown=shown: lines != shown).wait(
                5, need_all=False)['moved']
            scrolled_ms.append(got if got is not None else 5000)
            time.sleep(.05)
    spent = time.monotonic() - began
    done = turns[0] - before
    spent_cpu = (cpu_seconds(window.pid) - cpu[0], cpu_seconds(fleet.daemon.pid) - cpu[1])
    stop.set()
    for worker in workers:
        worker.join(30)
    return {'busy': len(workers), 'turns_per_s': round(done / spent, 1),
            'typing': summary(typed_ms), 'scroll': summary(scrolled_ms),
            'window_cpu_pct': round(spent_cpu[0] / spent * 100, 1),
            'daemon_cpu_pct': round(spent_cpu[1] / spent * 100, 1)}


def report(out):
    rows = {}
    for file in sorted(Path(out).glob('round-*.json')):
        got = json.loads(file.read_text())
        key = (got['size'], got['sync_ms'], got['build'])
        for part in ('typing', 'scroll'):
            for measure, value in got[part].items():
                rows.setdefault((part, measure), {}).setdefault(key, []).append(value)
        for measure in ('turns_per_s', 'window_cpu_pct', 'daemon_cpu_pct'):
            rows.setdefault(('load', measure), {}).setdefault(key, []).append(got[measure])
    keys = sorted({k for found in rows.values() for k in found})
    lines = ['| what | ' + ' | '.join(f'{size:,} at {ms} ms {build}' for size, ms, build in keys) + ' |',
             '|---|' + '---|' * len(keys)]
    for (part, measure), found in rows.items():
        cells = []
        for key in keys:
            values = sorted(found.get(key, []))
            cells.append(' / '.join(f'{v:g}' for v in values))
        lines.append(f'| {part} {measure} | ' + ' | '.join(cells) + ' |')
    table = '\n'.join(lines)
    (Path(out) / 'summary.md').write_text(table + '\n')
    return table


def main():
    parser = argparse.ArgumentParser(description=__doc__.split('\n\n')[0])
    parser.add_argument('--build', action='append', default=[], help='NAME=DIR holding agent and sonny')
    parser.add_argument('--shim', default='.local/slow_sync.so')
    parser.add_argument('--sync-ms', default='0,1000,3000', help="how long each of the daemon's syncs is held, per pass")
    parser.add_argument('--write-ms', type=int, default=150,
                        help="how long each of the window's file-system changes is held while the disk is slow")
    parser.add_argument('--sizes', default='50')
    parser.add_argument('--rounds', type=int, default=1)
    parser.add_argument('--own', type=int, default=2, help="tasks of the window's project that work throughout")
    parser.add_argument('--spread', type=int, default=6, help='tasks of other projects that work throughout')
    parser.add_argument('--busy', type=int, default=20, help='tasks that run turns back to back throughout')
    parser.add_argument('--tall', type=int, default=40, help="turns of the lead's conversation, to scroll")
    parser.add_argument('--keys', type=int, default=100)
    parser.add_argument('--settle', type=float, default=5, help='seconds of load before the first key')
    parser.add_argument('--scrolls', type=int, default=3, help='runs of notches up and back down')
    parser.add_argument('--notches', type=int, default=15)
    parser.add_argument('--layout', action='store_true', help="the window follows the fleet, as by default")
    parser.add_argument('--seeds', default='.local/ui-seeds')
    parser.add_argument('--report', help='only print the summary of the rounds under this folder')
    parser.add_argument('--out', default='.local/held-window')
    args = parser.parse_args()
    if args.report:
        print(report(args.report))
        return
    if not args.build:
        parser.error('--build NAME=DIR, at least one')
    out = Path(args.out)
    out.mkdir(parents=True, exist_ok=True)
    found = builds(args.build, out)
    signal.signal(signal.SIGTERM, lambda *_: sys.exit(1))
    for size in (int(s) for s in args.sizes.split(',')):
        for ms in (int(m) for m in args.sync_ms.split(',')):
            for round_ in range(args.rounds):
                order = found if round_ % 2 == 0 else found[::-1]
                for build in order:
                    for binary in (build['agent'], build['sonny']):
                        evict(binary)
                    got = run(build, size, ms, args)
                    print(json.dumps(got), file=sys.stderr)
                    name = f'round-{size}-{ms}-{build["name"]}-{round_}.json'
                    (out / name).write_text(json.dumps(got, indent=1))
    print(report(out))


if __name__ == '__main__':
    main()
