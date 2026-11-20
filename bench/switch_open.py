"""What `/switch` costs to open on a store of 10,000 agents: the time to
its list, then the reads its question scan makes and the CPU the window
and the daemon spend until both are quiet, the first open and a second.

One window on `p0.lead` of the store `bench.projects_open --seed` keeps
(100 projects of 100), quiet before it is measured (a fresh store and home
run Sonny's one-time upgrade work first), its footer's follow of every
project running as it does every day.

    python -m bench.switch_open --agent A --sonny S --seed .local/projects-10k.sqlite --out s.json
"""
import argparse
import json
import os
import shutil
import subprocess
import sys
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from bench.ends import connect  # noqa: E402
from bench.lookout_scale import cpu_seconds  # noqa: E402
from bench.projects_open import quiet  # noqa: E402
from bench.sonny_screen import Screen, Tap  # noqa: E402
from bench.synthetic_model import start  # noqa: E402

ESC = b'\x1b'


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--agent', required=True)
    parser.add_argument('--sonny', required=True)
    parser.add_argument('--seed', required=True, help='a store bench.projects_open --seed kept (100 projects of 100)')
    parser.add_argument('--projects', type=int, default=100)
    parser.add_argument('--opens', type=int, default=2)
    parser.add_argument('--out', required=True)
    args = parser.parse_args()
    agent, sonny = (str(Path(p).resolve()) for p in (args.agent, args.sonny))
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
    window = None
    result = {'agent': agent, 'sonny': sonny}
    try:
        window = Screen([sonny, '--store', str(store), '--socket', str(tapped), '--agent', 'p0.lead',
                         '--no-sound', '--no-layout'], env, str(folders[0]), columns=150, rows=40)
        window.wait('composer', lambda line: 'Message' in line, 300)
        began = time.monotonic()
        while time.monotonic() - began < 600:
            before = sum(tap.counts()[0].values())
            time.sleep(2)
            if sum(tap.counts()[0].values()) - before < 5:
                break
        result['window_quiet_s'] = round(time.monotonic() - began, 1)
        time.sleep(3)
        opens = []
        for n in range(args.opens):
            ops, _, back = tap.counts()
            cpu = (cpu_seconds(window.pid), cpu_seconds(daemon.pid))
            at = time.monotonic()
            os.write(window.fd, b'/switch\r')
            shown = window.wait('the list', lambda line: 'p1.lead' in line, 300)
            settled = quiet([window.pid, daemon.pid])
            ops2, _, back2 = tap.counts()
            opens.append({
                'shown_s': shown,
                'settled_s': round(time.monotonic() - at, 2),
                'window_cpu_s': round(cpu_seconds(window.pid) - cpu[0], 2),
                'daemon_cpu_s': round(cpu_seconds(daemon.pid) - cpu[1], 2),
                'requests': {op: ops2[op] - ops.get(op, 0) for op in ops2 if ops2[op] - ops.get(op, 0)},
                'daemon_bytes_back_mib': round((back2 - back) / 2**20, 2),
            })
            print('open', n + 1, opens[-1], settled, file=sys.stderr)
            os.write(window.fd, ESC)
            window.wait('conversation', lambda line: 'p1.lead' not in line and 'Message' in line, 30)
            time.sleep(2)
        result['opens'] = opens
        Path(args.out).write_text(json.dumps(result, indent=1))
        print(json.dumps(result, indent=1))
    finally:
        if window:
            window.close()
        try:
            control.request('shutdown', grace_ms=0)
        except Exception:  # noqa: BLE001
            pass
        daemon.wait(30)
        server.shutdown()


if __name__ == '__main__':
    main()
