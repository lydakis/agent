"""How long `/usage` takes to show every project's tasks, the first time
and each time after, and in a new window, at the low end and the high end
(George, 11-19: cache and update instead of loading everything).

PROJECTS projects, each a lead `pN.lead` in its own folder with TASKS
tasks `pN.tK` it made, every agent with one finished turn the person
started (as a task's brief is), on a synthetic model: the store
`bench.projects_open --seed` keeps. One Sonny window on `p0.lead` types
`/usage`, then `a` for every project; `esc` closes it and it opens again
OPENS times. Each read is timed from the key to the card read (no
`reading…` left), with the CPU seconds the window and the daemon spent
until both went quiet, and the daemon's requests by kind (through
`bench.sonny_screen.Tap`). Then a second window, as `/switch` makes one (a
new process), reads every project once, and the first window's RSS.

    python -m bench.usage_open --agent A --sonny S --seed .local/projects-10k.sqlite \\
        --projects 100 --out high.json
    python -m bench.usage_open --agent A --sonny S --projects 3 --tasks 3 --out low.json
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
from bench.ends import connect, need, spread  # noqa: E402
from bench.lookout_scale import cpu_seconds, rss_mib  # noqa: E402
from bench.sonny_screen import Screen, Tap  # noqa: E402
from bench.synthetic_model import start  # noqa: E402

ESC = b'\x1b'


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
    parser.add_argument('--opens', type=int, default=4)
    parser.add_argument('--seed', help='a store bench.projects_open --seed kept')
    parser.add_argument('--out', required=True)
    args = parser.parse_args()
    agent, sonny = (str(Path(p).resolve()) for p in (args.agent, args.sonny))
    work = Path('/tmp/usage-open')
    shutil.rmtree(work, ignore_errors=True)
    work.mkdir()
    home, store, sock, tapped = work / 'home', work / 'state.sqlite', work / 'd.sock', work / 'tap.sock'
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
    if args.seed:
        shutil.copy(args.seed, store)
    daemon = subprocess.Popen(
        [agent, 'serve', '--store', str(store), '--provider', f'openai=responses,{url}',
         '--socket', str(sock)], stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL,
        stderr=open(work / 'daemon.err', 'a'), env=env, start_new_session=True)
    control = connect(sock, daemon)
    window = None
    try:
        if not args.seed:
            handles = []
            for n, folder in enumerate(folders):
                lead = f'p{n}.lead'
                made = need(control.request('create', bot=lead, workspace=str(folder)), 'create')
                lead_id = made.get('bot_id') or made.get('bot', {}).get('bot_id')
                for name in [lead] + [f'p{n}.t{t}' for t in range(args.tasks)]:
                    if name != lead:
                        need(control.request('create', bot=name, workspace=str(folder),
                                             created_by=lead, created_by_id=lead_id), 'create')
                    sent = need(control.request('submit', bot=name, request_id=f'{name}-1',
                                                prompt='hi'), 'submit')
                    handles.append(f'{name}/{sent["message"]}')
            while handles:
                batch, handles = handles[:64], handles[64:]
                found = need(control.request('wait', handles=batch, timeout_ms=600_000), 'wait')
                handles += [h for h, r in found['results'].items() if r.get('pending')]
        tap = Tap(sock, tapped)

        def open_window(lead, folder):
            screen = Screen([sonny, '--store', str(store), '--socket', str(tapped), '--agent', lead,
                             '--no-sound', '--no-layout'], env, str(folder), columns=150, rows=60)
            screen.wait('composer', lambda line: 'Message' in line, 120)
            quiet([screen.pid, daemon.pid], limit=600)
            time.sleep(2)
            quiet([screen.pid, daemon.pid], limit=600)
            return screen

        def read(screen, keys, until):
            ops = dict(tap.counts()[0])
            cpu = (cpu_seconds(screen.pid), cpu_seconds(daemon.pid))
            began = time.monotonic()
            for key in keys:
                os.write(screen.fd, key)
                time.sleep(.02)
            screen.wait('usage', until, 300)
            while any('reading…' in line for line in screen.shown()):
                time.sleep(.02)
            shown = round(time.monotonic() - began, 2)
            quiet([screen.pid, daemon.pid])
            return {'shown_s': shown,
                    'window_cpu_s': round(cpu_seconds(screen.pid) - cpu[0], 3),
                    'daemon_cpu_s': round(cpu_seconds(daemon.pid) - cpu[1], 3),
                    'ops': {k: v - ops.get(k, 0) for k, v in tap.counts()[0].items() if v - ops.get(k, 0)}}

        every = lambda line: 'every project' in line  # noqa: E731
        result = {'agent': agent, 'sonny': sonny, 'projects': args.projects,
                  'agents': args.projects * (args.tasks + 1) if not args.seed else None}
        window = open_window('p0.lead', folders[0])
        result['rss_before_mib'] = round(rss_mib(window.pid), 1)
        opens = []
        for n in range(args.opens):
            read(window, [c.encode() for c in '/usage'] + [b'\r'], lambda line: 'last 24 hours' in line)
            opens.append(read(window, [b'a'], every))
            print(f'every project {n}: {opens[-1]}', file=sys.stderr)
            os.write(window.fd, ESC)
            window.wait('conversation', lambda line: 'Message' in line, 30)
            quiet([window.pid, daemon.pid])
        result['opens'] = opens
        result['first_s'] = opens[0]['shown_s']
        result['again_s'] = spread([o['shown_s'] * 1000 for o in opens[1:]])
        result['again_window_cpu_s'] = spread([o['window_cpu_s'] for o in opens[1:]])
        result['again_daemon_cpu_s'] = spread([o['daemon_cpu_s'] for o in opens[1:]])
        result['rss_after_mib'] = round(rss_mib(window.pid), 1)
        window.close()
        window = open_window('p1.lead' if args.projects > 1 else 'p0.lead', folders[min(1, args.projects - 1)])
        read(window, [c.encode() for c in '/usage'] + [b'\r'], lambda line: 'last 24 hours' in line)
        result['next_window'] = read(window, [b'a'], every)
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
