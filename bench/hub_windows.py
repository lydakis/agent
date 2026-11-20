"""What sharing every agent's row across a store's windows costs and
saves, the whole machine counted (Astra's case, 11-20): WINDOWS Sonny
windows on a store of 10,000 agents, every one showing only its footer but
the first, which opens **Projects**, while BUSY tasks of other projects run
turns back to back.

Three spells of SECONDS each: the page closed, the page open, and closed
again (the extra work must go away). In each: the CPU seconds of every
Sonny window together and of the daemon, the requests the windows made of
the daemon by kind and its bytes back (through `bench.sonny_screen.Tap`),
the bytes every window read and wrote (`/proc/PID/io`: the daemon's
socket, the store's shared socket and the terminal), and the turns served.
Before the open spell, the page opens and closes OPENS times, each timed
from the key to its first row of projects (p50, p99). Then every window's
RSS.

    python -m bench.hub_windows --agent A --sonny S --seed .local/projects-10k.sqlite \\
        --windows 100 --out high.json
"""
import argparse
import json
import os
import shutil
import subprocess
import sys
import threading
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from bench.ends import connect, need  # noqa: E402
from bench.lookout_scale import cpu_seconds, rss_mib  # noqa: E402
from bench.sonny_screen import Screen, Tap  # noqa: E402
from bench.synthetic_model import start  # noqa: E402

LEFT, ESC = b'\x1b[D', b'\x1b'


def io(pid):
    """Bytes `pid` read and wrote, every file and socket."""
    try:
        fields = dict(line.split(': ') for line in Path(f'/proc/{pid}/io').read_text().splitlines())
        return int(fields['rchar']), int(fields['wchar'])
    except (FileNotFoundError, ProcessLookupError, PermissionError):
        return 0, 0


def quantile(values, q):
    values = sorted(values)
    return values[min(len(values) - 1, int(round(q * (len(values) - 1))))]


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--agent', required=True)
    parser.add_argument('--sonny', required=True)
    parser.add_argument('--seed', required=True, help='a store bench.projects_open --seed kept (100 projects of 100)')
    parser.add_argument('--projects', type=int, default=100)
    parser.add_argument('--windows', type=int, default=100)
    parser.add_argument('--busy', type=int, default=30)
    parser.add_argument('--seconds', type=float, default=20)
    parser.add_argument('--opens', type=int, default=20)
    parser.add_argument('--out', required=True)
    args = parser.parse_args()
    agent, sonny = (str(Path(p).resolve()) for p in (args.agent, args.sonny))
    # Where the seed's agents work (`bench.projects_open` made them there).
    work = Path('/tmp/projects-open')
    shutil.rmtree(work, ignore_errors=True)
    work.mkdir()
    home, store, sock, tapped = work / 'home', work / 'state.sqlite', work / 'd.sock', work / 't.sock'
    (home / '.agent').mkdir(parents=True)
    (home / '.agent/config.toml').write_text('[sonny]\nchime = false\nsuggest = false\n')
    server, url = start()
    env = {'PATH': os.environ['PATH'], 'HOME': str(home), 'TERM': 'xterm-256color', 'LANG': 'en_US.UTF-8'}
    folders = [work / f'p{n}' for n in range(args.projects)]
    for n, folder in enumerate(folders):
        (folder / '.agents').mkdir(parents=True)
        (folder / '.agents/project.toml').write_text(f'name = "p{n}"\ntitle = "Proj {n:03}"\n')
        subprocess.run(['git', 'init', '-q', '-b', 'main', str(folder)], check=True)
    for suffix in ('', '-wal', '-shm'):
        if Path(args.seed + suffix).exists():
            shutil.copy(args.seed + suffix, str(store) + suffix)
    daemon = subprocess.Popen(
        [agent, 'serve', '--store', str(store), '--provider', f'openai=responses,{url}', '--socket', str(sock)],
        stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=open(work / 'daemon.err', 'a'),
        env=env, start_new_session=True)
    control = connect(sock, daemon)
    tap = Tap(sock, tapped)
    windows = []
    result = {'agent': agent, 'sonny': sonny, 'windows': args.windows, 'busy': args.busy}
    stop = threading.Event()
    turns = [0]
    failed = []
    try:
        began = time.monotonic()
        for n in range(args.windows):
            at = n % args.projects
            windows.append(Screen([sonny, '--store', str(store), '--socket', str(tapped), '--agent', f'p{at}.lead',
                                   '--no-sound', '--no-layout'], env, str(folders[at]), columns=150, rows=40))
        for window in windows:
            window.wait('composer', lambda line: 'Message' in line, 300)
        result['windows_started_s'] = round(time.monotonic() - began, 1)
        pids = [w.pid for w in windows]
        # A fresh store and home: every window starts Sonny's one-time
        # upgrade work (a whole listing, each agent looked at), which a
        # person's store did once long ago. Measured from when the windows
        # are quiet again, so what is left is what they cost every day.
        began = time.monotonic()
        while time.monotonic() - began < 600:
            before = sum(tap.counts()[0].values())
            time.sleep(2)
            if sum(tap.counts()[0].values()) - before < 20:
                break
        result['windows_quiet_s'] = round(time.monotonic() - began, 1)
        time.sleep(5)

        def busy(n):
            connection = connect(sock, daemon)
            # Tasks of the last ten projects, whose windows show only the
            # footer.
            project = args.projects - 1 - n % 10
            bot = f'p{project}.t{n // 10}'
            k = 0
            while not stop.is_set():
                k += 1
                sent = need(connection.request('submit', bot=bot, request_id=f'{bot}-h{k}', prompt='delay:500'),
                            'submit')
                handle = f'{bot}/{sent["message"]}'
                while True:
                    found = need(connection.request('wait', handles=[handle], timeout_ms=60_000), 'wait')
                    if not found['results'][handle].get('pending'):
                        break
                # A turn that failed did no model work: the measure stops.
                status = found['results'][handle].get('status')
                if status != 'completed':
                    failed.append(f'{bot}: {status}')
                    stop.set()
                    break
                turns[0] += 1
            connection.close()

        workers = [threading.Thread(target=busy, args=(n,), daemon=True) for n in range(args.busy)]
        for worker in workers:
            worker.start()
        time.sleep(5)

        def spell(name):
            cpu = (sum(cpu_seconds(p) for p in pids), cpu_seconds(daemon.pid))
            ops, sent, read = tap.counts()
            moved = [io(p) for p in pids]
            done, at = turns[0], time.monotonic()
            time.sleep(args.seconds)
            took = time.monotonic() - at
            ops2, sent2, read2 = tap.counts()
            moved2 = [io(p) for p in pids]
            result[name] = {
                'sonny_cpu_pct': round((sum(cpu_seconds(p) for p in pids) - cpu[0]) / took * 100, 1),
                'daemon_cpu_pct': round((cpu_seconds(daemon.pid) - cpu[1]) / took * 100, 1),
                'requests': {op: ops2[op] - ops.get(op, 0) for op in ops2 if ops2[op] - ops.get(op, 0)},
                'daemon_bytes_back_mib': round((read2 - read) / 2**20, 2),
                'windows_read_mib': round(sum(b[0] - a[0] for a, b in zip(moved, moved2)) / 2**20, 2),
                'windows_wrote_mib': round(sum(b[1] - a[1] for a, b in zip(moved, moved2)) / 2**20, 2),
                'turns': turns[0] - done,
            }
            print(name, result[name], file=sys.stderr)

        spell('closed')
        first = windows[0]
        shown = []
        for n in range(args.opens):
            began = time.monotonic()
            os.write(first.fd, LEFT)
            shown.append(first.wait('projects', lambda line: 'Proj 001' in line, 300))
            time.sleep(.5)
            os.write(first.fd, ESC)
            first.wait('conversation', lambda line: 'Message' in line, 30)
            time.sleep(.5)
        result['open_s'] = {'p50': quantile(shown, .5), 'p99': quantile(shown, .99), 'max': max(shown)}
        result['open_each_s'] = [round(s, 3) for s in shown]
        print('opens', result['open_s'], file=sys.stderr)
        os.write(first.fd, LEFT)
        first.wait('projects', lambda line: 'Proj 001' in line, 300)
        time.sleep(3)
        spell('open')
        os.write(first.fd, ESC)
        first.wait('conversation', lambda line: 'Message' in line, 30)
        time.sleep(3)
        spell('closed_again')
        rss = [rss_mib(p) for p in pids]
        result['rss_mib'] = {'first': round(rss[0], 1), 'others_mean': round(sum(rss[1:]) / max(1, len(rss) - 1), 1),
                             'total': round(sum(rss), 1)}
        stop.set()
        if failed:
            raise RuntimeError(f'busy turns failed, so nothing was measured: {failed[:3]}')
        Path(args.out).write_text(json.dumps(result, indent=1))
        print(json.dumps(result, indent=1))
    finally:
        stop.set()
        for window in windows:
            window.close()
        try:
            control.request('shutdown', grace_ms=0)
        except Exception:  # noqa: BLE001
            pass
        daemon.wait(30)
        server.shutdown()


if __name__ == '__main__':
    main()
