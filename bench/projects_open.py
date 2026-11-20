"""How long **Projects** takes to show, the first time a window opens it
and each time after, at the low end and the high end (George, 11-19: "slow
as fuck, and it reloads everything every time you reopen it").

PROJECTS projects, each a lead `pN.lead` in its own folder with TASKS
tasks `pN.tK` it made, every agent with one finished turn behind it on a
synthetic model. One Sonny window on `p0.lead`; `←` on its empty composer
opens **Projects**, `esc` closes it, OPENS times over. Each open is timed
from the key to the page's first row of projects (not `reading…`), with
the CPU seconds the window and the daemon spent until both went quiet.
Then the window's RSS with the page closed, and its idle CPU closed.

    python -m bench.projects_open --agent A --sonny S --projects 3 --tasks 3 --out low.json
    python -m bench.projects_open --agent A --sonny S --projects 100 --tasks 99 \\
        --seed .local/projects-10k.sqlite --out high.json
`--seed` keeps the stored agents and their turns from a first run.
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
from bench.ends import connect, need, spread  # noqa: E402
from bench.lookout_scale import cpu_seconds, rss_mib  # noqa: E402
from bench.sonny_screen import Screen  # noqa: E402
from bench.synthetic_model import start  # noqa: E402

LEFT, ESC = b'\x1b[D', b'\x1b'


def quiet(pids, limit=120, still=.03):
    """Seconds until half a second passes in which `pids` together spend
    under `still` CPU seconds."""
    began = time.monotonic()
    last = sum(cpu_seconds(p) for p in pids)
    while time.monotonic() - began < limit:
        time.sleep(.5)
        now = sum(cpu_seconds(p) for p in pids)
        if now - last < still:
            break
        last = now
    return round(time.monotonic() - began - .5, 2)


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--agent', required=True)
    parser.add_argument('--sonny', required=True)
    parser.add_argument('--projects', type=int, default=3)
    parser.add_argument('--tasks', type=int, default=3)
    parser.add_argument('--opens', type=int, default=5)
    parser.add_argument('--busy', type=int, default=0,
                        help='then, with the page open, this many tasks of other projects run turns back to back')
    parser.add_argument('--seconds', type=float, default=20)
    parser.add_argument('--seed')
    parser.add_argument('--out', required=True)
    args = parser.parse_args()
    agent, sonny = (str(Path(p).resolve()) for p in (args.agent, args.sonny))
    work = Path('/tmp/projects-open')
    shutil.rmtree(work, ignore_errors=True)
    work.mkdir()
    home, store, sock = work / 'home', work / 'state.sqlite', work / 'd.sock'
    (home / '.agent').mkdir(parents=True)
    (home / '.agent/config.toml').write_text('[sonny]\nchime = false\nsuggest = false\n')
    server, url = start()
    env = {'PATH': os.environ['PATH'], 'HOME': str(home), 'TERM': 'xterm-256color',
           'LANG': 'en_US.UTF-8'}
    folders = [work / f'p{n}' for n in range(args.projects)]
    for n, folder in enumerate(folders):
        (folder / '.agents').mkdir(parents=True)
        (folder / '.agents/project.toml').write_text(f'name = "p{n}"\ntitle = "Proj {n:03}"\n')
        subprocess.run(['git', 'init', '-q', '-b', 'main', str(folder)], check=True)
    seeded = args.seed and Path(args.seed).exists()
    if seeded:
        for suffix in ('', '-wal', '-shm'):
            if Path(args.seed + suffix).exists():
                shutil.copy(args.seed + suffix, str(store) + suffix)

    def serve():
        process = subprocess.Popen(
            [agent, 'serve', '--store', str(store), '--provider', f'openai=responses,{url}',
             '--socket', str(sock)], stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL,
            stderr=open(work / 'daemon.err', 'a'), env=env, start_new_session=True)
        return process, connect(sock, process)

    daemon, control = serve()
    window = None
    try:
        if not seeded:
            made = time.monotonic()
            handles = []
            for n, folder in enumerate(folders):
                lead = f'p{n}.lead'
                made_lead = need(control.request('create', bot=lead, workspace=str(folder)), 'create')
                lead_id = made_lead.get('bot_id') or made_lead.get('bot', {}).get('bot_id')
                names = [lead]
                for t in range(args.tasks):
                    name = f'p{n}.t{t}'
                    need(control.request('create', bot=name, workspace=str(folder), created_by=lead, created_by_id=lead_id),
                         'create')
                    names.append(name)
                for name in names:
                    sent = need(control.request('submit', bot=name, request_id=f'{name}-1',
                                                prompt='hi'), 'submit')
                    handles.append(f'{name}/{sent["message"]}')
            waiting = list(handles)
            while waiting:
                batch, waiting = waiting[:64], waiting[64:]
                found = need(control.request('wait', handles=batch, timeout_ms=600_000), 'wait')
                for handle, result in found['results'].items():
                    if result.get('pending'):
                        waiting.append(handle)
                    elif result.get('status') != 'completed':
                        raise RuntimeError(f'turn {handle}: {result}')
            print(f'seeded {len(handles)} agents in {time.monotonic() - made:.0f} s', file=sys.stderr)
            if args.seed:
                control.request('shutdown', grace_ms=0)
                daemon.wait(60)
                shutil.copy(store, args.seed)
                daemon, control = serve()
        result = {'agent': agent, 'sonny': sonny, 'projects': args.projects,
                  'agents': args.projects * (args.tasks + 1)}
        window = Screen([sonny, '--store', str(store), '--socket', str(sock), '--agent', 'p0.lead',
                         '--no-sound', '--no-layout'], env, str(folders[0]), columns=150, rows=40)
        result['window_start_s'] = window.wait('composer', lambda line: 'Message' in line, 60)
        quiet([window.pid, daemon.pid])
        time.sleep(3)
        quiet([window.pid, daemon.pid])
        result['rss_before_mib'] = round(rss_mib(window.pid), 1)
        last = f'Proj {min(args.projects - 1, 1):03}'
        opens = []
        for n in range(args.opens):
            cpu = (cpu_seconds(window.pid), cpu_seconds(daemon.pid))
            began = time.monotonic()
            os.write(window.fd, LEFT)
            shown = window.wait('projects', lambda line: last in line, 300)
            quiet([window.pid, daemon.pid])
            opens.append({'shown_s': shown, 'settled_s': round(time.monotonic() - began, 2),
                          'window_cpu_s': round(cpu_seconds(window.pid) - cpu[0], 3),
                          'daemon_cpu_s': round(cpu_seconds(daemon.pid) - cpu[1], 3)})
            print(f'open {n}: {opens[-1]}', file=sys.stderr)
            os.write(window.fd, ESC)
            window.wait('conversation', lambda line: last not in line and 'Message' in line, 30)
            time.sleep(1)
            quiet([window.pid, daemon.pid])
        result['opens'] = opens
        result['first_shown_s'] = opens[0]['shown_s']
        result['again_shown_s'] = spread([o['shown_s'] * 1000 for o in opens[1:]])
        result['again_window_cpu_s'] = spread([o['window_cpu_s'] for o in opens[1:]])
        result['again_daemon_cpu_s'] = spread([o['daemon_cpu_s'] for o in opens[1:]])
        result['rss_after_mib'] = round(rss_mib(window.pid), 1)
        cpu = cpu_seconds(window.pid)
        time.sleep(10)
        result['closed_idle_cpu_pct'] = round((cpu_seconds(window.pid) - cpu) * 10, 2)
        result['rss_idle_mib'] = round(rss_mib(window.pid), 1)
        # Another window, as `/switch` or `enter` on a project makes one (a
        # new process): its first open.
        window.close()
        window = Screen([sonny, '--store', str(store), '--socket', str(sock), '--agent', 'p1.lead',
                         '--no-sound', '--no-layout'], env, str(folders[min(1, args.projects - 1)]),
                        columns=150, rows=40)
        window.wait('composer', lambda line: 'Message' in line, 60)
        quiet([window.pid, daemon.pid])
        time.sleep(3)
        quiet([window.pid, daemon.pid])
        cpu = (cpu_seconds(window.pid), cpu_seconds(daemon.pid))
        os.write(window.fd, LEFT)
        first = f'Proj {0:03}'
        result['next_window_shown_s'] = window.wait('projects', lambda line: first in line, 300)
        quiet([window.pid, daemon.pid])
        result['next_window_cpu_s'] = round(cpu_seconds(window.pid) - cpu[0], 3)
        result['next_window_daemon_cpu_s'] = round(cpu_seconds(daemon.pid) - cpu[1], 3)
        if args.busy:
            # The page stays open while tasks of other projects work.
            from bench.ends import connect as reach
            stop, turns = threading.Event(), [0]

            def busy(n):
                connection = reach(sock, daemon)
                bot = f'p{2 + n % (args.projects - 2)}.t{n // (args.projects - 2)}'
                k = 0
                while not stop.is_set():
                    k += 1
                    sent = need(connection.request('submit', bot=bot, request_id=f'{bot}-b{k}',
                                                   prompt='delay:500'), 'submit')
                    handle = f'{bot}/{sent["message"]}'
                    while True:
                        found = need(connection.request('wait', handles=[handle], timeout_ms=60_000), 'wait')
                        if not found['results'][handle].get('pending'):
                            break
                    turns[0] += 1
                connection.close()
            workers = [threading.Thread(target=busy, args=(n,), daemon=True) for n in range(args.busy)]
            for worker in workers:
                worker.start()
            time.sleep(3)
            cpu = (cpu_seconds(window.pid), cpu_seconds(daemon.pid))
            began, done = time.monotonic(), turns[0]
            time.sleep(args.seconds)
            took = time.monotonic() - began
            result['busy_open'] = {
                'busy': args.busy, 'turns': turns[0] - done,
                'window_cpu_pct': round((cpu_seconds(window.pid) - cpu[0]) / took * 100, 1),
                'daemon_cpu_pct': round((cpu_seconds(daemon.pid) - cpu[1]) / took * 100, 1)}
            stop.set()
            for worker in workers:
                worker.join(30)
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
