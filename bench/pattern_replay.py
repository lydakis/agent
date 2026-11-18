"""What a pattern's follow costs to replay a backlog (#1161 review).

A follow of a GLOB pattern replays every bot's events from a store-wide
cursor and passes the other bots' by, so a sparse pattern reads most of a
backlog to deliver its share. This seeds PREFIXES projects of BOTS agents
each (`p0.a0`, `p0.a1`, ...) with a backlog of EVENTS durable events spread
evenly across them, then, on a fresh daemon each time, follows from cursor
0 with:

  exact      one follow of one bot by name (`p0.a0`)
  prefix     one follow of one project (`p0.*`)
  prefixes   PREFIXES follows at once, one project each, each on its own
             connection, as that many windows coming back at once
  kinds      the same PREFIXES follows narrowed to `turn_finished`

and reports, for each, the time from asking to `follow_live` (each
subscriber's), the rows the store passes over per event delivered (from the
store's own counts: a name's follow reads that bot's rows by index, a
pattern's every row past the cursor, or every row of its kinds when it names
few enough; a pattern's are matched in SQLite, so only its own are
decoded), the daemon's CPU, and the p99 of an unrelated context read
(`history_nodes` of another agent, back to back) while it runs. While the
replay pages, turns run on one agent of each project, so live events cross
the hand-off; every subscriber's durable cursors must then be exactly the
store's for its selector, in order, none missing and none twice.

    python -m bench.pattern_replay --agent target/release/agent --out replay.json
"""
import argparse
import json
import os
import shutil
import sqlite3
import statistics
import subprocess
import sys
import threading
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from bench.socket_client import Connection  # noqa: E402
from bench.synthetic_model import start  # noqa: E402

TICKS = os.sysconf('SC_CLK_TCK')


class Subscriber(Connection):
    """A follower that keeps only its durable cursors and when it went live."""

    def __init__(self, path):
        self.cursors, self.live_at = [], None
        super().__init__(path, retain_durable=False)

    def record(self, event):
        kind = event.get('event')
        if kind == 'follow_live':
            self.live_at = self.live_at or time.monotonic()
        elif 'cursor' in event and 'id' not in event:
            self.cursors.append(event['cursor'])
            return
        super().record(event)


def cpu(pid):
    with open(f'/proc/{pid}/stat') as stat:
        fields = stat.read().rsplit(')', 1)[1].split()
    return (int(fields[11]) + int(fields[12])) / TICKS


def spread(values):
    values = sorted(values)
    if not values:
        return None
    at = lambda q: values[min(len(values) - 1, int(q * len(values)))]  # noqa: E731
    return {'p50': round(statistics.median(values), 2), 'p99': round(at(.99), 2),
            'max': round(values[-1], 2), 'n': len(values)}


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--agent', required=True)
    parser.add_argument('--out', required=True)
    parser.add_argument('--events', type=int, default=200_000)
    parser.add_argument('--prefixes', type=int, default=100)
    parser.add_argument('--bots', type=int, default=2, help='agents in each project')
    parser.add_argument('--live-turns', type=int, default=3, help='turns per project while replaying')
    args = parser.parse_args()
    agent = str(Path(args.agent).resolve())
    work = Path('/tmp/pattern-replay')
    shutil.rmtree(work, ignore_errors=True)
    work.mkdir()
    home, store, sock, seed = work / 'home', work / 'state.sqlite', work / 'd.sock', work / 'seed.sqlite'
    (home / '.agent').mkdir(parents=True)
    folder = work / 'w'
    folder.mkdir()
    server, url = start()
    env = {'PATH': os.environ['PATH'], 'HOME': str(home)}
    daemon = None

    def serve():
        process = subprocess.Popen(
            [agent, 'serve', '--store', str(store), '--provider', f'openai=responses,{url}',
             '--socket', str(sock)], stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL,
            stderr=open(work / 'daemon.err', 'a'), env=env, start_new_session=True)
        deadline = time.monotonic() + 60
        while True:
            try:
                return process, Connection(sock, retain_durable=False)
            except (FileNotFoundError, ConnectionRefusedError):
                if process.poll() is not None or time.monotonic() > deadline:
                    raise RuntimeError('daemon startup failed')
                time.sleep(.02)

    def stop(process, control):
        control.request('shutdown', grace_ms=0)
        process.wait(60)

    bots = [f'p{p}.a{b}' for p in range(args.prefixes) for b in range(args.bots)]
    try:
        # The agents, and one other with a history to read.
        daemon, control = serve()
        for bot in bots + ['other']:
            created = control.request('create', bot=bot, workspace=str(folder))
            assert 'error' not in created, created
        for n in range(20):
            submitted = control.request('submit', bot='other', request_id=f'o{n}', prompt='hello')
            control.request('wait', handles=[f'other/{submitted["result"]["message"]}'], timeout_ms=60_000)
        stop(daemon, control)
        # The backlog, written straight to the store: one bot after another,
        # mostly tool events with a turn's end now and then.
        db = sqlite3.connect(store)
        now = int(time.time() * 1000)
        rows = ((bots[n % len(bots)], 1, 'turn_finished' if n % 10 == 9 else 'tool_started',
                 json.dumps({'call_id': f'c{n}', 'name': 'echo', 'arguments': {'text': 'x' * 40}}), now)
                for n in range(args.events))
        db.executemany('INSERT INTO events(bot,turn,kind,data,written_ms) VALUES (?,?,?,?,?)', rows)
        db.commit()
        db.close()
        shutil.copy(store, seed)

        scenarios = {
            'exact': [dict(bot='p0.a0')],
            'prefix': [dict(bot='p0.*')],
            'prefixes': [dict(bot=f'p{p}.*') for p in range(args.prefixes)],
            'kinds': [dict(bot=f'p{p}.*', kinds=['turn_finished']) for p in range(args.prefixes)],
        }
        result = {'agent': agent, 'events': args.events, 'prefixes': args.prefixes,
                  'bots_each': args.bots, 'scenarios': {}}
        for name, follows in scenarios.items():
            for suffix in ('-wal', '-shm'):
                Path(str(store) + suffix).unlink(missing_ok=True)
            shutil.copy(seed, store)
            daemon, control = serve()
            subscribers = [Subscriber(sock) for _ in follows]
            reader = Connection(sock, retain_durable=False)
            reads, done = [], threading.Event()

            def probe():
                while not done.is_set():
                    began = time.monotonic()
                    reader.request('history_nodes', bot='other', limit=50)
                    reads.append((time.monotonic() - began) * 1000)

            def live():
                driver = Connection(sock, retain_durable=False)
                for turn in range(args.live_turns):
                    handles = []
                    for p in range(len(follows)):
                        bot = f'p{p}.a{args.bots - 1}' if name != 'exact' else 'p0.a0'
                        submitted = driver.request('submit', bot=bot, request_id=f'l{turn}', prompt='hi')
                        if 'result' in submitted:
                            handles.append(f'{bot}/{submitted["result"]["message"]}')
                    driver.request('wait', handles=handles, timeout_ms=120_000)
                driver.close()

            probing = threading.Thread(target=probe)
            living = threading.Thread(target=live)
            time.sleep(1)
            cpu_before = cpu(daemon.pid)
            began = time.monotonic()
            probing.start()
            for subscriber, follow in zip(subscribers, follows):
                subscriber.asked_at = time.monotonic()
                threading.Thread(target=subscriber.request, args=('follow',),
                                 kwargs=dict(follow, after=0), daemon=True).start()
            living.start()
            deadline = began + 600
            while any(s.live_at is None for s in subscribers) and time.monotonic() < deadline:
                time.sleep(.005)
            all_live = time.monotonic() - began
            living.join()
            time.sleep(.5)
            done.set()
            probing.join()
            daemon_cpu = cpu(daemon.pid) - cpu_before
            # What each should have, from the store.
            db = sqlite3.connect(store)
            ordered, exact, delivered, read = True, True, 0, 0
            total = db.execute('SELECT count(*) FROM events').fetchone()[0]
            for subscriber, follow in zip(subscribers, follows):
                kinds = follow.get('kinds')
                where, params = ('bot GLOB ?', [follow['bot']]) if '*' in follow['bot'] else ('bot = ?', [follow['bot']])
                if kinds:
                    where += f" AND kind IN ({','.join('?' * len(kinds))})"
                    params += kinds
                want = [r[0] for r in db.execute(f'SELECT id FROM events WHERE {where} ORDER BY id', params)]
                got = subscriber.cursors
                ordered &= got == sorted(set(got))
                exact &= got == want
                delivered += len(got)
                # A name's replay reads its rows by index; a pattern's every
                # row, or every row of the few kinds it names.
                if '*' not in follow['bot']:
                    read += len(want)
                elif kinds:
                    read += db.execute(f"SELECT count(*) FROM events WHERE kind IN ({','.join('?' * len(kinds))})",
                                       kinds).fetchone()[0]
                else:
                    read += total
            db.close()
            result['scenarios'][name] = {
                'follows': len(follows),
                'to_live_s': spread([s.live_at - s.asked_at for s in subscribers if s.live_at]),
                'all_live_s': round(all_live, 3),
                'delivered': delivered,
                'rows_read': read,
                'rows_read_per_delivered': round(read / max(delivered, 1), 1),
                'daemon_cpu_s': round(daemon_cpu, 2),
                'context_read_ms': spread(reads),
                'cursors_in_order': ordered,
                'cursors_exact': exact,
            }
            print(name, json.dumps(result['scenarios'][name]), flush=True)
            for subscriber in subscribers:
                subscriber.close()
            reader.close()
            stop(daemon, control)
            daemon = None
        Path(args.out).write_text(json.dumps(result, indent=1) + '\n')
    finally:
        if daemon and daemon.poll() is None:
            daemon.terminate()
            daemon.wait(30)
        server.shutdown()
        shutil.rmtree(work, ignore_errors=True)


if __name__ == '__main__':
    main()
